#!/usr/bin/env bash
# Idempotent dependency setup for this repository.
#
# Installs (skipping anything already on PATH):
#   - the Rust toolchain pinned by rust-toolchain.toml (via rustup)
#   - wasm-tools, wac, just (via cargo-binstall, versions pinned below)
#   - npm dependencies for the JS trees (skipped with SKIP_NODE=1)
#
# Prerequisites it does NOT install: rustup itself, Node 24+ and npm.
#
# Environment overrides:
#   WASM_TOOLS_VERSION, WAC_VERSION, JUST_VERSION  - tool version pins
#   SKIP_NODE=1                       - skip all npm installs
#
# CI runs this same script rather than duplicating install steps.
set -euo pipefail

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"
WAC_VERSION="${WAC_VERSION:-0.10.1}"
JUST_VERSION="${JUST_VERSION:-1.54.0}"

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
if ! have cargo-binstall; then
    curl -L --proto '=https' --tlsv1.2 -sSf \
        https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
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
    for dir in conformance/driver-ct/jco examples/jco-demo js/componentize/wpt/parity; do
        if [ -f "$REPO_ROOT/$dir/package.json" ]; then
            (cd "$REPO_ROOT/$dir" && npm install)
        fi
    done
fi

# Make the installed tools visible to later GitHub Actions steps.
if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$HOME/.cargo/bin" >>"$GITHUB_PATH"
    echo "$HOME/.local/bin" >>"$GITHUB_PATH"
fi

echo "setup complete"
