//! The in-band stdout marker: `ACTMCP_BENCHMARK {json}` (ADR-0002 §5.2).
//!
//! The grammar is pinned by the ADR and re-stated nowhere else, so it is written out here
//! as code and tested as one unit:
//!
//! ```text
//! ^[ \t]*ACTMCP_BENCHMARK (\{.*\})[ \t]*$
//! ```
//!
//! Three things that grammar is deliberately doing, because each one is a way a marker
//! scanner could get this wrong:
//!
//! * **Anchored at line start** (after spaces or tabs): a README, a `cat`-ed source file,
//!   or a build log quoting the marker must not be read as a metric. So the marker must be
//!   the first thing on the line.
//! * **Greedy to the last `}`**: `{"labels":{"step":"x"}}` ends with a nested object's
//!   brace, and a non-greedy match would cut the JSON short and call the metric malformed.
//! * **One line, no continuation**: the rest of the line is one compact JSON object. A
//!   pretty-printed marker is not a marker, and there is no state to carry between lines.

/// The literal that begins a marker line, minus its trailing space.
pub(crate) const MARKER: &str = "ACTMCP_BENCHMARK";

/// The JSON object a line carries, when the line is a marker line.
///
/// `None` for a line that is not a marker at all; a `Some` whose JSON does not parse is a
/// counted `malformed_metric`, which is the distinction the caller needs.
pub(crate) fn marker_json(line: &str) -> Option<&str> {
    let body = line.trim_end_matches([' ', '\t']);
    let body = body.trim_start_matches([' ', '\t']);
    let rest = body.strip_prefix(MARKER)?.strip_prefix(' ')?;
    let rest = rest.trim_start_matches([' ', '\t']);
    // Greedy: the capture runs to the *last* `}` on the line (§5.2), so a nested object in
    // `labels` or `extra` stays inside the capture.
    let end = rest.rfind('}')?;
    let json = &rest[..=end];
    json.starts_with('{').then_some(json)
}

/// Whether a text is even worth handing to [`marker_json`] — a cheap pre-filter for the
/// caller, which runs it over every line of a run's log.
pub(crate) fn mentions_marker(text: &str) -> bool {
    text.contains(MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extracted(line: &str) -> Option<String> {
        marker_json(line).map(str::to_owned)
    }

    #[test]
    fn a_plain_marker_is_extracted() {
        assert_eq!(
            extracted(r#"ACTMCP_BENCHMARK {"name":"cold_start_ms","value":412.5,"unit":"ms"}"#),
            Some(r#"{"name":"cold_start_ms","value":412.5,"unit":"ms"}"#.to_owned())
        );
    }

    #[test]
    fn leading_and_trailing_tabs_and_spaces_are_allowed() {
        // act's own step output is indented, and a job that echoes the marker from a
        // Makefile recipe starts with a tab; both are the same marker.
        assert!(extracted("\t  ACTMCP_BENCHMARK {\"name\":\"x\",\"value\":1}  \t").is_some());
    }

    #[test]
    fn the_capture_runs_to_the_last_brace() {
        let line = r#"ACTMCP_BENCHMARK {"name":"x","value":1,"labels":{"step":"Run benchmark"}}"#;
        let json = extracted(line).expect("marker");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("whole object");
        assert_eq!(parsed["labels"]["step"], "Run benchmark");
    }

    #[test]
    fn text_after_the_last_brace_is_not_part_of_the_json() {
        let json =
            extracted(r#"ACTMCP_BENCHMARK {"name":"x","value":1} (took 1s)"#).expect("marker");
        assert_eq!(json, r#"{"name":"x","value":1}"#);
    }

    #[test]
    fn a_line_that_only_mentions_the_marker_is_not_a_marker() {
        // The §5.2 anchoring rule, in both directions: prose about the marker and a log
        // line that printed something before it.
        assert_eq!(
            extracted(r#"see ACTMCP_BENCHMARK {"name":"x","value":1}"#),
            None
        );
        assert_eq!(
            extracted("# ACTMCP_BENCHMARK {\"name\":\"x\",\"value\":1}"),
            None
        );
        assert_eq!(
            extracted("ACTMCP_BENCHMARK"),
            None,
            "no payload is not a marker"
        );
        assert_eq!(
            extracted("ACTMCP_BENCHMARK{}"),
            None,
            "the grammar wants a space"
        );
        assert_eq!(
            extracted("ACTMCP_BENCHMARK [1,2]"),
            None,
            "an array is not an entry"
        );
        assert_eq!(
            extracted("ACTMCP_BENCHMARK {oops"),
            None,
            "no closing brace"
        );
    }

    #[test]
    fn a_marker_whose_json_is_broken_still_extracts_and_fails_later() {
        // §5.2 counts these as `malformed_metric`, so the split between "this was a marker
        // line" and "this marker is garbage" has to be visible to the caller.
        let json = extracted(r#"ACTMCP_BENCHMARK {"name":"x",}"#).expect("it is a marker line");
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_err());
    }

    #[test]
    fn the_pre_filter_never_hides_a_marker() {
        for line in [
            r#"ACTMCP_BENCHMARK {"name":"x","value":1}"#,
            "\tACTMCP_BENCHMARK {\"name\":\"x\",\"value\":1}",
        ] {
            assert!(mentions_marker(line));
            assert!(marker_json(line).is_some());
        }
    }
}
