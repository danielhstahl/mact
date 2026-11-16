//! Which glyphs and which colours this surface is allowed to use.
//!
//! [`Palette`] is the single answer to three questions the ticket asks in one breath — *no-color,
//! `NO_COLOR`, `TERM=dumb` fallback, and a `--plain` escape hatch* — and it is resolved once per process
//! rather than per row for a reason that is not cosmetic: `Model::embedded()` *parses*
//! `schema/metric-model.json`, and a renderer that asked for the glyph table per cell would spend a
//! frame's worth of JSON parsing on punctuation.
//!
//! ```text
//!                        TERM=dumb / unset   NO_COLOR set   --plain   --no-color
//!                                  └────────────┴──────────────┴──────────┘
//!                                        ▼
//!                     Palette { glyphs: ascii | unicode, colour, band edges }
//! ```
//!
//! The rules it encodes are ADR-0008 §6.1's: the ascii column exists so a log file or a `TERM=dumb`
//! terminal still reads correctly, and no signal may be carried by colour alone — which is why `colour:
//! false` changes the *embellishment* and never the information.

use crate::metrics::model::Glyphs;
use crate::metrics::Model;
use crate::mi::definition::Definition;
use crate::mi::score::{band_of, Band};

/// The glyph table and colour policy for one run.
#[derive(Debug, Clone)]
pub struct Palette {
    /// The glyph column in force (ADR-0008 §6.1's `unicode` or `ascii` column).
    pub glyphs: Glyphs,
    /// Whether to emit colour at all.
    pub colour: bool,
    /// Whether the selection may use reverse video. On unless the caller asked for the plainest
    /// possible surface; the `▸` marker is drawn either way, so selection never depends on this.
    pub highlight: bool,
    /// The metric model, parsed once: caveat-column membership and the glyph table both come from it.
    model: Model,
    /// The MI definition, parsed once, for the *declared* band edges (ADR-0007 §5.1).
    definition: Definition,
}

impl Palette {
    /// Resolve from the environment and the flags.
    ///
    /// `term` is `$TERM` (`None` when unset), `no_color` is `$NO_COLOR` set to anything non-empty (the
    /// convention at <https://no-color.org>), `plain` asks for the ASCII table, `no_colour` asks for
    /// colour off while keeping the unicode glyphs.
    #[must_use]
    pub fn resolve(term: Option<&str>, no_color: bool, plain: bool, no_colour: bool) -> Self {
        let model = Model::embedded();
        // A `dumb` terminal, or no `TERM` at all, is the case the ascii column was written for. An unset
        // `TERM` is treated as dumb rather than as capable: guessing that a terminal with no declared
        // type can draw `▲` is how you print mojibake into someone's scrollback.
        let dumb = term.is_none_or(|value| value.is_empty() || value == "dumb");
        Self {
            glyphs: if plain || dumb {
                model.display.ascii.clone()
            } else {
                model.display.unicode.clone()
            },
            colour: !(dumb || no_color || no_colour),
            highlight: !plain,
            model,
            definition: Definition::embedded(),
        }
    }

    /// The usual terminal: unicode glyphs, colour on.
    #[must_use]
    pub fn interactive() -> Self {
        let model = Model::embedded();
        Self {
            glyphs: model.display.unicode.clone(),
            colour: true,
            highlight: true,
            model,
            definition: Definition::embedded(),
        }
    }

    /// The plain surface: ascii glyphs, no colour, no reverse video.
    #[must_use]
    pub fn plain() -> Self {
        let model = Model::embedded();
        Self {
            glyphs: model.display.ascii.clone(),
            colour: false,
            highlight: false,
            model,
            definition: Definition::embedded(),
        }
    }

    /// The metric model in force.
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The MI band, from ADR-0007 §5.1's declared edges.
    ///
    /// The view is allowed to *use* these edges and not to invent any (§4.4), which is why this is a
    /// method on the thing that loaded the definition rather than a threshold in the drawing code.
    #[must_use]
    pub fn band(&self, mi: f64) -> Band {
        band_of(&self.definition, mi)
    }

    /// The declared amber edge, for the help overlay's legend.
    #[must_use]
    pub fn amber_edge(&self) -> f64 {
        self.definition.bands.amber_below
    }

    /// The declared red edge, ditto.
    #[must_use]
    pub fn red_edge(&self) -> f64 {
        self.definition.bands.red_below
    }

    /// Whether the unicode glyph table is in force (the marker column picks `▸` or `>` from this).
    #[must_use]
    pub fn unicode(&self) -> bool {
        self.glyphs.unknown == "—"
    }

    /// The character that says "this was cut", in this surface's own alphabet.
    ///
    /// A truncated message with no marker is a truncated message read as a whole one, so the cut is
    /// marked — and the marker is `…` where unicode is on and `~` where it is not, because the plain
    /// surface exists for terminals that cannot draw `…`.
    #[must_use]
    pub fn ellipsis(&self) -> char {
        if self.unicode() {
            '…'
        } else {
            '~'
        }
    }
}
