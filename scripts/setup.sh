#!/usr/bin/env bash
# Idempotent dependency setup for this repository.
#
# Installs (skipping anything already on PATH):
#   - the Rust toolchain pinned by rust-toolchain.toml (via rustup)
#   - wasm-tools, wac, just (via cargo-binstall, versions pinned below;
#     cargo-binstall itself arrives as a release asset pinned by version
#     and digest — scripts/cargo-binstall.sha256 — never via a floating
#     bootstrap script)
#   - pnpm (via npm, version pinned below)
#   - JS dependencies for the package trees (skipped with SKIP_NODE=1);
#     the jco toolchain arrives as a prebuilt release-asset tarball
#     (@bytecodealliance/jco-transpile), so installs run no cargo build
#
# Prerequisites it does NOT install: rustup itself, Node 24+ and npm.
#
# Environment overrides:
#   WASM_TOOLS_VERSION, WAC_VERSION, JUST_VERSION, PNPM_VERSION
#                 - tool version pins
#   SKIP_NODE=1   - skip all JS installs
#
# CI runs this same script rather than duplicating install steps.
set -euo pipefail

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"
WAC_VERSION="${WAC_VERSION:-0.10.1}"
JUST_VERSION="${JUST_VERSION:-1.54.0}"
PNPM_VERSION="${PNPM_VERSION:-10.34.5}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

have() { command -v "$1" >/dev/null 2>&1; }

# Fail on the missing prerequisites this script deliberately does not
# install, before spending minutes on the ones it does.
if ! have rustup; then
    echo "setup: rustup is required but not on PATH (https://rustup.rs); see the README prerequisites" >&2
    exit 1
fi

if [ "${SKIP_NODE:-}" != "1" ] && ! have npm; then
    echo "setup: npm is required but not on PATH (Node 24+; see the README prerequisites), or set SKIP_NODE=1" >&2
    exit 1
fi

# Rust toolchain: rust-toolchain.toml drives what rustup installs.
(cd "$REPO_ROOT" && (rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install))

# cargo-binstall bootstraps the pinned cargo tools without compiling them.
# It is itself pinned: the release asset for this platform is downloaded
# directly and verified against scripts/cargo-binstall.sha256 before it
# runs. Bumping the version means re-recording those digests deliberately.
BINSTALL_VERSION="1.21.1"

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

install_binstall() {
    local asset
    case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset="cargo-binstall-x86_64-unknown-linux-musl.tgz" ;;
    Linux-aarch64) asset="cargo-binstall-aarch64-unknown-linux-musl.tgz" ;;
    Darwin-*) asset="cargo-binstall-universal-apple-darwin.zip" ;;
    *) asset="" ;;
    esac
    if [ -z "$asset" ]; then
        echo "setup: no pinned cargo-binstall asset for $(uname -s)/$(uname -m); building from crates.io (registry checksums)" >&2
        cargo install cargo-binstall --locked --version "$BINSTALL_VERSION"
        return
    fi

    local want
    want="$(grep -v '^#' "$REPO_ROOT/scripts/cargo-binstall.sha256" | awk -v a="$asset" '$2 == a { print $1 }')"
    if [ -z "$want" ]; then
        echo "setup: scripts/cargo-binstall.sha256 pins no digest for ${asset}; record it deliberately" >&2
        exit 1
    fi

    local tmp
    tmp="$(mktemp -d)"
    curl -fsSL --proto '=https' --tlsv1.2 -o "${tmp}/${asset}" \
        "https://github.com/cargo-bins/cargo-binstall/releases/download/v${BINSTALL_VERSION}/${asset}"

    local got
    got="$(sha256_of "${tmp}/${asset}")"
    if [ "$got" != "$want" ]; then
        rm -rf "$tmp"
        cat >&2 <<EOF
setup: ${asset} does not match the digest pinned for cargo-binstall ${BINSTALL_VERSION}.
  expected ${want}
  actual   ${got}

The download has been removed. Either the published asset was replaced,
the pin is stale, or the download was tampered with. Re-record the
digests deliberately after establishing why they changed.
EOF
        exit 1
    fi

    mkdir -p "$HOME/.cargo/bin"
    case "$asset" in
    *.tgz) tar -xzf "${tmp}/${asset}" -C "$HOME/.cargo/bin" cargo-binstall ;;
    *.zip) unzip -q -o "${tmp}/${asset}" cargo-binstall -d "$HOME/.cargo/bin" ;;
    esac
    rm -rf "$tmp"
}

if ! have cargo-binstall; then
    install_binstall
fi

# --force: a restored CI cache can contain cargo's install metadata without
# the binary itself, which would otherwise make binstall a no-op.
if ! have wasm-tools; then
    cargo binstall -y --locked --force "wasm-tools@${WASM_TOOLS_VERSION}"
fi
if ! have wac; then
    cargo binstall -y --locked --force "wac-cli@${WAC_VERSION}"
fi
if ! have just; then
    cargo binstall -y --locked --force "just@${JUST_VERSION}"
fi

if [ "${SKIP_NODE:-}" != "1" ]; then
    if ! have pnpm; then
        npm install -g "pnpm@${PNPM_VERSION}"
    fi
    for dir in conformance/driver-ct/jco examples/jco-demo js/componentize/wpt/parity; do
        if [ -f "$REPO_ROOT/$dir/package.json" ]; then
            (cd "$REPO_ROOT/$dir" && pnpm install)
        fi
    done
fi

# Make the installed tools visible to later GitHub Actions steps.
if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$HOME/.cargo/bin" >>"$GITHUB_PATH"
    echo "$HOME/.local/bin" >>"$GITHUB_PATH"
fi

echo "setup complete"
