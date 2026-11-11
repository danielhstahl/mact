#!/usr/bin/env bash
#
# scripts/install_ci_tools.sh — install the tools scripts/ci.sh needs that rustup does not
# give you, so that the workflow YAML stays a list of script calls and this file stays the
# one place that knows how a tool is obtained.
#
# Everything here is idempotent: run it on a fresh runner or on your own box, it installs
# what is missing and says so.
#
#   cargo-deny        the supply-chain step (deny.toml). Fetched as upstream's own
#                     prebuilt static-musl tarball for the host arch, verified against a
#                     sha256 pinned *in this file* — reviewed here, not whatever the
#                     release page happens to carry — and installed into $CARGO_HOME/bin.
#                     Any other platform falls back to `cargo install --locked`, which is
#                     slower but builds from the same locked dependency set.
#   jsonschema (py)   the schema step's second-opinion checks (meta-schema validation and
#                     scripts/validate_evidence.py). Best effort on purpose: the hard
#                     schema checks run in the Rust engine the binary embeds, so a failed
#                     pip install is a warning here, not a red build.
#
# Usage:
#   scripts/install_ci_tools.sh [--force] [--no-python] [-h|--help]

set -o errexit -o nounset -o pipefail

# Bump together with a re-run of `scripts/ci.sh supply-chain`.
CARGO_DENY_VERSION=${CARGO_DENY_VERSION:-0.20.2}
# Pinned digests for cargo-deny's own release tarballs, keyed by the target triple of the
# tarball. A case function rather than `declare -A`: macOS /bin/bash is 3.2 (no assoc arrays).
# An empty answer means "no pinned tarball", which falls back to `cargo install --locked`.
cargo_deny_sha256() {
    case "$1" in
        aarch64-unknown-linux-musl) echo "995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d" ;;
        x86_64-unknown-linux-musl)  echo "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f" ;;
        aarch64-apple-darwin)       echo "fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3" ;; # TODO: pin from the release page
        *)                          echo "" ;;
    esac
}

# Portable sha256 of a file: GNU coreutils has sha256sum, macOS has shasum.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

usage() {
    cat <<'USAGE'
Usage: scripts/install_ci_tools.sh [OPTIONS]

Installs: cargo-deny (pinned version, pinned sha256, into $CARGO_HOME/bin) and the python
'jsonschema' package used by the schema step's second-opinion checks.

Options:
  -h, --help     this text
  -f, --force    reinstall cargo-deny even if a working one is already on PATH
      --no-python  skip the python package (use it when you manage python yourself)
      --deny-only  only cargo-deny
      --python-only  only the python package

Environment: CARGO_DENY_VERSION, CARGO_HOME (default ~/.cargo), PYTHON (default python3).
USAGE
}

force=0
want_python=1
want_deny=1
py=${PYTHON:-python3}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -f | --force) force=1 ;;
        --no-python) want_python=0 ;;
        --deny-only) want_python=0 ;;
        --python-only) want_deny=0 ;;
        *)
            echo "install_ci_tools: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

ok() { printf '  ok      %s\n' "$*"; }
did() { printf '  done    %s\n' "$*"; }
warn() { printf '  warn    %s\n' "$*"; }
die() {
    printf 'install_ci_tools: %s\n' "$*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# cargo-deny
# ---------------------------------------------------------------------------
install_cargo_deny() {
    local triple arch target tarball url want
    triple=$(rustc -vV | sed -n 's/^host: //p') || die "rustc did not answer -vV; is a Rust toolchain installed?"
    arch=${triple%%-*}
    case "$triple" in
        *-apple-darwin) target="$arch-apple-darwin" ;;
        *-linux-*)      target="$arch-unknown-linux-musl" ;; # static musl build runs on any linux libc
        *)              target="" ;;
    esac
    tarball="cargo-deny-$CARGO_DENY_VERSION-$target.tar.gz"
    url="https://github.com/EmbarkStudios/cargo-deny/releases/download/$CARGO_DENY_VERSION/$tarball"
    want=$(cargo_deny_sha256 "$target")

    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    mkdir -p "$cargo_bin"

    if [[ $force -eq 0 ]] && command -v cargo-deny >/dev/null 2>&1; then
        local now
        now=$(cargo deny --version 2>/dev/null | awk '{print $2}')
        if [[ "$now" == "$CARGO_DENY_VERSION" ]]; then
            ok "cargo-deny $now already installed"
            return 0
        fi
    fi

    if [[ -z "$want" ]]; then
        # No pinned tarball for this platform (macOS/Windows, or an unusual linux): build
        # it from the locked dependency set instead. Slow, but never silently skipped.
        warn "no pinned $arch tarball for $triple; falling back to 'cargo install --locked'"
        cargo install cargo-deny --locked --version "$CARGO_DENY_VERSION"
        ok "cargo-deny installed by cargo install"
        return 0
    fi

    local tmp
    tmp=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" RETURN

    did "downloading $url"
    curl --fail --silent --show-error --location --output "$tmp/$tarball" "$url" ||
        die "could not download $url"
    # The release page publishes a .sha256 next to the tarball. Compare it with the digest
    # pinned above: pinned value is the one that is reviewed, the published one is the
    # cross-check that this script and upstream agree.
    local published
    published=$(curl --fail --silent --show-error --location "$url.sha256" | tr -d ' \n') ||
        die "could not download $url.sha256"
    if [[ "$published" != "$want" ]]; then
        die "upstream's published sha256 ($published) is not the one pinned in this script ($want); stop and find out why before bumping"
    fi
    local actual
    actual=$(sha256_of "$tmp/$tarball")
    [[ "$actual" == "$want" ]] ||
        die "the downloaded tarball does not match its sha256 (got $actual, pinned $want)"
    tar -xzf "$tmp/$tarball" -C "$tmp"
    local extracted
    # `find -quit` is GNU-only; sed -n 1p works everywhere and (unlike head) can't SIGPIPE find.
    extracted=$(find "$tmp" -maxdepth 2 -type f -name cargo-deny -print | sed -n 1p)
    [[ -n "$extracted" ]] || die "the tarball had no cargo-deny binary in it"
    install -m 0755 "$extracted" "$cargo_bin/cargo-deny"
    ok "cargo-deny $(cargo deny --version | awk '{print $2}') installed in $cargo_bin"
}

# ---------------------------------------------------------------------------
# python: jsonschema (second-opinion schema checks)
# ---------------------------------------------------------------------------
install_python_jsonschema() {
    if command -v "$py" >/dev/null 2>&1 && "$py" -c 'import jsonschema' >/dev/null 2>&1; then
        ok "$py already has jsonschema"
        return 0
    fi
    command -v "$py" >/dev/null 2>&1 || {
        warn "$py is not installed; the schema step will skip its python second-opinion checks"
        return 0
    }
    # A user install first, and the pepsanction-for-the-venvless escape hatch second: distro
    # pythons mark their environment externally managed (PEP 668) and refuse the first one.
    if "$py" -m pip install --user --quiet jsonschema 2>/dev/null ||
        "$py" -m pip install --user --quiet --break-system-packages jsonschema 2>/dev/null; then
        did "jsonschema installed for $py"
    else
        warn "could not pip-install jsonschema for $py; the schema step will skip its python"
        warn "second-opinion checks (the Rust engine checks stay hard). PYTHON=... or a venv works too."
    fi
}

if [[ "$want_deny" == 1 ]]; then
    install_cargo_deny
fi
if [[ "$want_python" == 1 ]]; then
    install_python_jsonschema
fi
