#!/usr/bin/env bash
#
# scripts/check_dependency_firewalls.sh — the dependency-graph promises `deny.toml` cannot make.
#
# cargo-deny answers "which crates are in the resolved graph, and may they be here". It cannot
# answer the two questions this epic's tree turns on, because both are about *shape*:
#
#   firewalls   ADR-0005 §6's allowed/forbidden table is written against each crate's *manifest*,
#               and `scripts/validate_crate_layout.py` checks exactly that (C5/C6). Neither one can
#               see a transitive edge: `fathom` gaining `schemars` through `act-evidence`'s
#               `json-schema` feature is invisible in `crates/fathom/Cargo.toml` — the manifest just
#               says `act-evidence = { workspace = true }` — and it puts the server's schema engine
#               into the reader's release build. ADR-0005 §11 assigns that check to this ticket:
#               `cargo build -p fathom --release && cargo tree -p fathom -i schemars`, must be empty.
#               The same argument in the other direction keeps the TUI out of the server's tree.
#
#   no-C        ADR-0003 (libgit2), ADR-0006 §5 (bundled sqlite) and ADR-0007 §6 (tree-sitter) each
#               refused a C build dependency for the same reason: `cd.yml` builds five targets, two
#               of them musl and one macOS cross, so a C toolchain is not a developer's problem but
#               a release blocker with a week's lead time. deny.toml bans the known carriers; this
#               checks the graph for the *toolchain* instead, which catches a carrier nobody thought
#               to name. Nothing here needs `cc`, `cmake` or `pkg-config` today, and that is a
#               promise, not an accident — mact-by1.16 confirmed it against `rust-toolchain.toml`
#               (a minimal-profile toolchain ships no C toolchain of its own) and against CI's runner
#               image (`ubuntu-24.04` has gcc but not `musl-gcc`, which is why the musl rows of the
#               `check` matrix are the belt-and-braces version of this check).
#
# Run it as part of `scripts/ci.sh supply-chain`, or alone:
#
#   scripts/check_dependency_firewalls.sh              every check, on this workspace
#   scripts/check_dependency_firewalls.sh --list       the checks, one per line
#   scripts/check_dependency_firewalls.sh --check no-C only the C-toolchain one
#
# Options:
#   -h, --help          this text
#   -l, --list          list the check names and exit
#   --check NAME        firewall | no-C | all (default: all)
#   -q, --quiet         only the verdict
#
# Exit status: 0 every promise held, 1 a firewall was crossed, 2 usage/environment error.

set -o errexit -o nounset -o pipefail

# The targets this project ships: the same list `deny.toml`'s `[graph] targets` names, and for the
# same reason — a promise that is only checked on the host is not checked on half the releases.
TARGETS=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin)

#: What may never appear in `fathom`'s tree (ADR-0005 §6's forbidden side, plus the §6 budget fact
#: that a standalone fathom build carries no schema engine, and the C carriers ADR-0003/0006/0007
#: refused). Transitive as well as direct: that is the whole point.
FATHOM_BANNED=(
    act-mcp
    rmcp
    axum
    tower
    tower-http
    tokio
    tokio-util
    reqwest
    hyper
    tracing
    tracing-subscriber
    schemars
    rusqlite
    libsqlite3-sys
    tree-sitter
    git2
    libgit2-sys
)
#: What may never appear in `act-mcp`'s tree: the reader's UI, and the reader itself (ADR-0005 §6,
#: §16 — the server's compile time and advisory surface must not grow with a TUI's).
ACT_MCP_BANNED=(
    fathom
    ratatui
    ratatui-core
    ratatui-widgets
    crossterm
)
#: Crates whose only purpose is to invoke a C/C++ toolchain (or to vendor C for it). Their presence
#: anywhere in the graph means `cargo build` needs more than rustc, which five cross targets cannot
#: assume. `autocfg` and `rustc_version` are deliberately *not* listed: they ask rustc questions, in
#: Rust, and several of this graph's build scripts are exactly that.
C_TOOLCHAIN_CRATES=(
    cc
    cmake
    pkg-config
    bindgen
    cexpr
    openssl-src
    ring
    aws-lc-sys
    libz-sys
    bzip2-sys
    lzma-sys
    sqlite-wasm-rs
)

usage() {
    cat <<'USAGE'
Usage: scripts/check_dependency_firewalls.sh [OPTIONS]

Checks the dependency-graph promises cargo-deny cannot: that no crate from one side of ADR-0005's
graph appears on the other side (transitively included), that a standalone `fathom` build carries no
`schemars`, and that no crate in the resolved graph needs a C toolchain to build.

Options:
  -h, --help       this text
  -l, --list       list the check names and exit
  --check NAME     firewall | no-C | all (default: all)
  -q, --quiet      only the verdict

Exit status: 0 every promise held, 1 a promise was broken, 2 usage/environment error.
USAGE
}

selected=all
quiet=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -l | --list)
            printf 'firewall\nno-C\n'
            exit 0
            ;;
        --check)
            selected=${2:?"--check needs a name (firewall, no-C or all)"}
            case "$selected" in
                firewall | no-C | all) ;;
                *)
                    echo "check_dependency_firewalls: unknown --check '$selected' (try --list)" >&2
                    exit 2
                    ;;
            esac
            shift
            ;;
        -q | --quiet) quiet=1 ;;
        *)
            echo "check_dependency_firewalls: unknown argument '$1' (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if repo_root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null); then
    :
else
    repo_root=$(cd -- "$script_dir/.." && pwd)
fi
cd "$repo_root"

say() { [[ "$quiet" == 1 ]] || printf '  %s\n' "$*"; }
fix() { printf '  fix: %s\n' "$*"; }

command -v cargo >/dev/null 2>&1 || {
    echo "check_dependency_firewalls: cargo is not on PATH" >&2
    exit 2
}

# The resolved graph for one package, one crate name per line, direct or transitive, over the edges
# that a build can link: normal, build and dev (dev because `cargo test` — which CI runs for every
# member — builds them). `--prefix none` keeps column 1 the crate name and nothing else.
#
# `cargo tree` refreshes Cargo.lock if a manifest changed; this script never passes --locked on
# purpose, because the check has to answer the question about the tree the manifests *describe*, and
# `ci.sh`'s build steps have already resolved it by the time this runs.
graph() {
    local package=$1
    local target=${2:-}
    if [[ -n "$target" ]]; then
        cargo tree -p "$package" -e normal,build,dev --target "$target" --prefix none 2>/dev/null
    else
        cargo tree -p "$package" -e normal,build,dev --prefix none 2>/dev/null
    fi | awk 'NF { print $1 }' | sort -u
}

# Report every name from $2..$n that the graph in $1 names. Prints nothing and returns 0 when the
# firewall holds.
crossed() {
    local graph=$1
    shift
    local hit=()
    local name
    for name in "$@"; do
        if grep -qx -- "$name" <<<"$graph"; then
            hit+=("$name")
        fi
    done
    if [[ ${#hit[@]} -gt 0 ]]; then
        printf '%s' "${hit[*]}"
        return 1
    fi
    return 0
}

failed=0

# ---------------------------------------------------------------------------
# firewall: the edges ADR-0005 forbids, transitively
# ---------------------------------------------------------------------------
check_firewall() {
    local fathom_graph act_mcp_graph hit
    fathom_graph=$(graph fathom)
    act_mcp_graph=$(graph act-mcp)
    if [[ -z "$fathom_graph" || -z "$act_mcp_graph" ]]; then
        echo "check_dependency_firewalls: cargo tree returned nothing; is this the workspace root?" >&2
        return 1
    fi
    say "fathom's tree has $(wc -l <<<"$fathom_graph" | tr -d ' ') crates, act-mcp's has $(wc -l <<<"$act_mcp_graph" | tr -d ' ')"

    if hit=$(crossed "$fathom_graph" "${FATHOM_BANNED[@]}"); then
        say "firewall   fathom carries none of: ${FATHOM_BANNED[*]}"
    else
        failed=1
        printf 'firewall   BROKEN: fathom'"'"'s tree carries %s\n' "$hit"
        fix "ADR-0005 §6 forbids it in fathom; 'cargo tree -p fathom -i ${hit%% *}' names who brought it"
        fix "if it arrives through act-evidence's json-schema feature, that feature is the server's, not the reader's"
    fi

    if hit=$(crossed "$act_mcp_graph" "${ACT_MCP_BANNED[@]}"); then
        say "firewall   act-mcp carries none of: ${ACT_MCP_BANNED[*]}"
    else
        failed=1
        printf 'firewall   BROKEN: act-mcp'"'"'s tree carries %s\n' "$hit"
        fix "ADR-0005 §6/§16 keep the TUI out of the server; 'cargo tree -p act-mcp -i ${hit%% *}' names who brought it"
    fi
}

# ---------------------------------------------------------------------------
# no-C: nothing in the graph may need a C toolchain
# ---------------------------------------------------------------------------
check_no_c() {
    local target
    for target in "${TARGETS[@]}"; do
        local graph hit
        graph=$(graph fathom "$target")$'\n'$(graph act-mcp "$target")$'\n'$(graph act-evidence "$target")
        graph=$(sort -u <<<"$graph" | sed '/^$/d')
        if hit=$(crossed "$graph" "${C_TOOLCHAIN_CRATES[@]}"); then
            say "no-C       $target: $(wc -l <<<"$graph" | tr -d ' ') crates, none of them a C toolchain driver"
        else
            failed=1
            printf 'no-C       BROKEN: %s needs a C toolchain (%s)\n' "$target" "$hit"
            fix "cd.yml builds $target with no C toolchain guaranteed; ADR-0003/0006 §5/0007 §6 refused C carriers for this reason"
            fix "'cargo tree --target $target -i ${hit%% *}' names who brought it; deny.toml's [[bans.deny]] is where the refusal is written down"
        fi
    done
}

if [[ "$selected" == firewall || "$selected" == all ]]; then
    check_firewall
fi
if [[ "$selected" == no-C || "$selected" == all ]]; then
    check_no_c
fi

if [[ $failed -ne 0 ]]; then
    echo "check_dependency_firewalls: FAILED — a dependency-graph promise broke"
    exit 1
fi
exit 0
