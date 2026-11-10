#!/usr/bin/env bash
#
# scripts/smoke_binary.sh — prove a built act-mcp binary is a runnable binary.
#
# `cargo build --release` proves the linker was happy. It does not prove the artefact
# answers --help, that its subcommands are wired, that an unknown flag is refused, or that
# the schemas embedded with `include_str!` really made it into the image. Those are the
# five things a release would otherwise discover on someone's machine, so they are checked
# here, by CI's release job and by `scripts/ci.sh release` — the same script, so local and
# CI agree (repo convention: logic in scripts, not in the workflow YAML).
#
# Usage:
#   scripts/smoke_binary.sh [OPTIONS] <path-to-binary>
#
# Options:
#   -h, --help            this text
#   -q, --quiet           only the verdict
#   --report DIR          write DIR/version.txt (the binary's own --version output) and
#                         DIR/sha256.txt next to the binary, for CI to attach/upload.
#                         Also appended to $GITHUB_STEP_SUMMARY when that is set.
#   --fixture DIR         additionally run `<binary> validate-evidence DIR` and require it
#                         to pass: the embedded schemas and the reader have to work in the
#                         shipped image, not only in `cargo run`.
#   --expect-version VER  require --version to report VER (default: the version in
#                         Cargo.toml, so a binary built from a different tree is caught).
#
# Exit status: 0 everything checked out, 1 a check failed, 2 usage/environment error.

set -o errexit -o nounset -o pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/smoke_binary.sh [OPTIONS] <path-to-binary>

Prove a built act-mcp binary is a runnable binary: it exists, answers --version with the
version this tree declares, answers --help and names both subcommands, refuses an unknown
flag, and (with --fixture) validates a known-good .evidence tree with the schemas embedded
in the image.

Options:
  -h, --help            this text
  -q, --quiet           only the verdict
  --report DIR          write DIR/version.txt and DIR/sha256.txt, for CI to attach;
                        also appends a row to $GITHUB_STEP_SUMMARY when it is set
  --fixture DIR         also run `<binary> validate-evidence DIR` and require it to pass
  --expect-version VER  require --version to report VER (default: Cargo.toml's version)

Exit status: 0 everything checked out, 1 a check failed, 2 usage/environment error.
USAGE
}

report_dir=""
fixture=""
expect_version=""
binary=""
quiet=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -q | --quiet) quiet=1 ;;
        --report)
            report_dir=${2:?"--report needs a directory"}
            shift
            ;;
        --fixture)
            fixture=${2:?"--fixture needs a directory"}
            shift
            ;;
        --expect-version)
            expect_version=${2:?"--expect-version needs a version"}
            shift
            ;;
        -*)
            echo "smoke_binary: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
        *)
            if [[ -n "$binary" ]]; then
                echo "smoke_binary: more than one binary given ('$binary' and '$1')" >&2
                exit 2
            fi
            binary=$1
            ;;
    esac
    shift
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)

if [[ -z "$binary" ]]; then
    usage >&2
    exit 2
fi

say() { [[ "$quiet" == 1 ]] || printf '  %s\n' "$*"; }
fail() { printf 'smoke_binary: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 0. there is a binary, and it is executable
# ---------------------------------------------------------------------------
[[ -f "$binary" ]] || fail "no file at $binary (build it first: cargo build --release)"
[[ -x "$binary" ]] || fail "$binary is not executable"
size=$(wc -c < "$binary" | tr -d ' ')
if [[ "$size" -lt 100000 ]]; then
    fail "$binary is $size bytes: too small to be a linked act-mcp"
fi
say "binary   $binary ($size bytes)"

# ---------------------------------------------------------------------------
# 1. --version, and it is the version this tree says it is
# ---------------------------------------------------------------------------
if [[ -z "$expect_version" ]]; then
    expect_version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$repo_root/Cargo.toml" | head -1)
fi
version_output=$("$binary" --version 2>&1 | head -1) || fail "--version exited non-zero: $version_output"
if [[ "$version_output" != "act-mcp $expect_version" ]]; then
    fail "--version reported '$version_output', expected 'act-mcp $expect_version'"
fi
say "--version  $version_output"

# `--version` on a subcommand is how clap propagates it; a build that lost
# `propagate_version` shows up here rather than as a confused `--help` reader.
if ! "$binary" serve --version >/dev/null 2>&1; then
    fail "serve --version did not work (is propagate_version still set?)"
fi

# ---------------------------------------------------------------------------
# 2. --help works and names the surface a client depends on
# ---------------------------------------------------------------------------
help_output=$("$binary" --help 2>&1) || fail "--help exited non-zero: $help_output"
for needle in serve validate-evidence; do
    if ! grep -q -- "$needle" <<<"$help_output"; then
        fail "--help does not mention the '$needle' subcommand"
    fi
done
say "--help     $(wc -l <<<"$help_output" | tr -d ' ') lines, names serve and validate-evidence"
"$binary" serve --help >/dev/null 2>&1 || fail "serve --help exited non-zero"
"$binary" validate-evidence --help >/dev/null 2>&1 || fail "validate-evidence --help exited non-zero"

# ---------------------------------------------------------------------------
# 3. a bad flag is refused, not ignored
# ---------------------------------------------------------------------------
if "$binary" --definitely-not-a-flag >/dev/null 2>&1; then
    fail "an unknown flag was accepted: argument parsing is not validating anything"
fi
say "bad flag   refused as it must be"

# ---------------------------------------------------------------------------
# 4. the embedded schemas work in this image
# ---------------------------------------------------------------------------
if [[ -n "$fixture" ]]; then
    [[ -d "$fixture" ]] || fail "--fixture $fixture is not a directory"
    evidence_output=$("$binary" validate-evidence "$fixture" 2>&1) ||
        fail "validate-evidence $fixture failed: $evidence_output"
    say "evidence   $evidence_output"
fi

# ---------------------------------------------------------------------------
# 5. report for CI (version + digest), attached where CI can see it
# ---------------------------------------------------------------------------
digest=$(sha256sum "$binary" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$binary" | cut -d' ' -f1)
if [[ -n "$report_dir" ]]; then
    mkdir -p "$report_dir"
    printf '%s\n' "$version_output" > "$report_dir/version.txt"
    printf '%s  %s\n' "$digest" "$(basename "$binary")" > "$report_dir/sha256.txt"
    say "sha256     $digest"
    say "report     $report_dir/{version.txt,sha256.txt}"
fi
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        printf '| act-mcp | `%s` | %s bytes | `%s` |\n' \
            "$version_output" "$size" "${digest:0:16}…"
    } >> "$GITHUB_STEP_SUMMARY"
fi

exit 0
