#!/usr/bin/env bash
# Drill for scripts/validate_crate_layout.py.
#
# scripts/validate_crate_layout.py is written before the workspace split it governs (ADR-0005,
# mact-by1.1), so on this tree today it checks a single crate and the interesting rules wait. This
# script rehearses the other half: it assembles a throwaway three-crate workspace (act-evidence,
# act-mcp, fathom) in a scratch directory under /tmp, checks the checker accepts it, then breaks one
# ADR-0005 rule at a time and checks the right check fires. When mact-by1.5 does the real split, run
# this first: a rule that cannot be broken is a rule that is not being checked.
#
# Everything happens in $DRILL_DIR (default /tmp/crate-layout-drill) and the script refuses to run
# anywhere else. Nothing in the repository is read except by copy.
#
# Usage: scripts/crate_layout_drill.sh [DRILL_DIR]     # exit 0 when every case behaved
set -uo pipefail

SRC=$(cd "$(dirname "$0")/.." && pwd)
D=${1:-${DRILL_DIR:-/tmp/crate-layout-drill}}
case "$D" in
    /tmp/*) ;;
    *) echo "refusing to build a scratch workspace outside /tmp (got: $D)" >&2; exit 2 ;;
esac

build() {
    rm -rf "$D"
    mkdir -p "$D"/{scripts,docs/adr,schema,crates}
    cp "$SRC"/scripts/validate_crate_layout.py "$SRC"/scripts/ci.sh "$D"/scripts/
    cp "$SRC"/docs/adr/0005-*.md "$D"/docs/adr/
    cp "$SRC"/docs/evidence-format.md "$D"/docs/
    cp "$SRC"/README.md "$SRC"/deny.toml "$SRC"/rust-toolchain.toml "$SRC"/Cargo.lock "$D"/
    cp "$SRC"/schema/*.json "$D"/schema/
    # the golden tree, already moved into the crate that owns it (§9: one copy, moved, never copied)
    mkdir -p "$D"/crates/act-mcp/tests/e2e/testdata/expected
    : > "$D"/crates/act-mcp/tests/e2e/testdata/expected/benchmark.jsonl

    cat > "$D"/Cargo.toml <<'TOML'
[workspace]
resolver = "2"
members = ["crates/act-evidence", "crates/act-mcp", "crates/fathom"]

[workspace.package]
version = "0.0.1"
edition = "2021"
rust-version = "1.88"
publish = false

[workspace.dependencies]
act-evidence = { path = "crates/act-evidence" }
serde = "1"
clap = "4"
ratatui = "0.29"
schemars = "1"
tempfile = "3"

[workspace.lints.rust]
unsafe_code = "deny"
TOML

    mkdir -p "$D"/crates/act-evidence/src
    cat > "$D"/crates/act-evidence/Cargo.toml <<'TOML'
[package]
name = "act-evidence"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true

[lib]
name = "act_evidence"
path = "src/lib.rs"

[dependencies]
serde.workspace = true
schemars = { workspace = true, optional = true }
tempfile = { workspace = true, optional = true }

[features]
json-schema = ["dep:schemars"]
test-support = ["dep:tempfile"]

[lints]
workspace = true
TOML
    printf '#![warn(missing_docs)]\n//! docs.\npub const S: &str = include_str!("../../../schema/evidence.schema.json");\n' \
        > "$D"/crates/act-evidence/src/lib.rs

    mkdir -p "$D"/crates/act-mcp/src
    cat > "$D"/crates/act-mcp/Cargo.toml <<'TOML'
[package]
name = "act-mcp"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true

[lib]
name = "act_mcp"
path = "src/lib.rs"

[[bin]]
name = "act-mcp"
path = "src/main.rs"

[dependencies]
act-evidence = { workspace = true, features = ["json-schema"] }
clap.workspace = true

[lints]
workspace = true
TOML
    printf '#![warn(missing_docs)]\n//! docs.\n' > "$D"/crates/act-mcp/src/lib.rs
    printf 'fn main() {}\n' > "$D"/crates/act-mcp/src/main.rs

    mkdir -p "$D"/crates/fathom/src
    cat > "$D"/crates/fathom/Cargo.toml <<'TOML'
[package]
name = "fathom"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true

[lib]
name = "fathom"
path = "src/lib.rs"

[[bin]]
name = "fathom"
path = "src/main.rs"

[dependencies]
act-evidence.workspace = true
ratatui.workspace = true

[lints]
workspace = true
TOML
    printf '#![warn(missing_docs)]\n//! docs.\n' > "$D"/crates/fathom/src/lib.rs
    printf 'fn main() {}\n' > "$D"/crates/fathom/src/main.rs

    cd "$D" || exit 2
}

failures=0
expect() { # expect <label> <expected-exit> <command that breaks a rule...>
    local label=$1 want=$2 got
    shift 2
    ( "$@" ) >/dev/null 2>&1
    python3 scripts/validate_crate_layout.py --quiet >/tmp/crate-layout-drill.out 2>&1
    got=$?
    if [[ "$got" == "$want" ]]; then
        echo "PASS  $label (exit $got)"
    else
        echo "FAIL  $label: wanted exit $want, got $got"
        sed 's/^/        /' /tmp/crate-layout-drill.out
        failures=$((failures + 1))
    fi
    build
}

build
expect "a clean post-split workspace passes" 0 true
expect "fathom may not depend on an async runtime" 1 \
    sed -i 's|ratatui.workspace = true|ratatui.workspace = true\ntokio = "1"|' crates/fathom/Cargo.toml
expect "fathom may not depend on act-mcp, dev-dependencies included" 1 \
    sed -i 's|ratatui.workspace = true|ratatui.workspace = true\n[dev-dependencies]\nact-mcp.workspace = true|' crates/fathom/Cargo.toml
expect "act-evidence's dependency budget is closed" 1 \
    sed -i 's|serde.workspace = true|serde.workspace = true\nregex = "1"|' crates/act-evidence/Cargo.toml
expect "schemars stays optional so a reader's build can skip it" 1 \
    sed -i 's|schemars = { workspace = true, optional = true }|schemars.workspace = true|' crates/act-evidence/Cargo.toml
expect "nothing may be publishable" 1 \
    sed -i 's|publish.workspace = true|publish = true|' crates/fathom/Cargo.toml
expect "every member opts into the workspace lints" 1 \
    sed -i 's|^workspace = true|workspace = false|' crates/act-mcp/Cargo.toml
expect "missing_docs stays out of the workspace lints" 1 \
    sed -i 's|unsafe_code = "deny"|unsafe_code = "deny"\nmissing_docs = "warn"|' Cargo.toml
expect "the shared rust-version floor never goes down" 1 \
    sed -i 's|rust-version = "1.88"|rust-version = "1.85"|' Cargo.toml
expect "a member may not set its own version" 1 \
    sed -i '0,/^version.workspace = true$/s//version = "9.9.9"/' crates/fathom/Cargo.toml
expect "one copy of each schema file" 1 \
    cp schema/evidence.schema.json crates/act-evidence/evidence.schema.json
expect "one copy of the golden evidence tree" 1 \
    sh -c 'mkdir -p tests/e2e/testdata/expected && touch tests/e2e/testdata/expected/benchmark.jsonl'
expect "no orphan crate under crates/" 1 \
    sh -c 'mkdir -p crates/orphan/src && printf "[package]\nname = \"orphan\"\nversion.workspace = true\nedition.workspace = true\nrust-version.workspace = true\npublish.workspace = true\n" > crates/orphan/Cargo.toml && touch crates/orphan/src/lib.rs'
expect "a dropped member is noticed" 1 \
    sed -i 's|, "crates/fathom"||' Cargo.toml
expect "the root manifest stays virtual" 1 \
    sh -c 'printf "\n[package]\nname = \"root-thing\"\nversion = \"0.0.1\"\nedition = \"2021\"\n" >> Cargo.toml'
expect "default-members must not narrow what CI builds" 1 \
    sed -i 's|resolver = "2"|resolver = "2"\ndefault-members = ["crates/act-mcp"]|' Cargo.toml
expect "a path dependency may not escape the member set" 1 \
    sed -i 's|clap.workspace = true|clap.workspace = true\noutside = { path = "../../somewhere" }|' crates/act-mcp/Cargo.toml

echo
if [[ $failures == 0 ]]; then
    echo "ok: every ADR-0005 rule the checker claims can actually be broken, in $D"
else
    echo "$failures drill case(s) did not behave"
fi
exit $((failures > 0))
