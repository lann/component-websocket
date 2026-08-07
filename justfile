# The orchestration surface: repo-wide recipes plus one module per software
# component, each module's justfile colocated with its code.

import 'justfile.shared.just'

mod gha '.github'
mod conformance-ct 'conformance/driver-ct'
mod demo 'examples'
mod wpt 'js/componentize/wpt'

default:
    @just --list

# The exact set of checks CI runs: each CI job runs exactly one gha:: job
# recipe.
ci: (gha::rust-checks) (gha::conformance-checks)

# Fast pre-commit checks.
check: fmt-check clippy validate-wit check-js jco-pin-check test

fmt-check:
    cargo fmt --all -- --check

# Native crates (the workspace default-members) on the host target, then
# each wasm-only crate on its wasm target.
clippy:
    cargo clippy -- -D warnings
    cargo clippy --target wasm32-wasip2 -p websocket-guest-provider -- -D warnings
    cargo clippy --target wasm32-unknown-unknown -p echo-demo -- -D warnings
    cargo clippy --target wasm32-wasip2 -p conformance-guest-ct -- -D warnings

# Validate every WIT tree: the shared package and each consumer's world
# (which pulls the package in through its deps symlink).
validate-wit:
    wasm-tools component wit wit >/dev/null
    wasm-tools component wit rust/wasmtime/wit >/dev/null
    wasm-tools component wit conformance/wit >/dev/null
    wasm-tools component wit rust/guest-provider/wit >/dev/null
    wasm-tools component wit examples/echo-demo/wit >/dev/null
    @echo "wit: ok"

# Syntax-check the JavaScript trees; nothing else compiles them before a
# full conformance run would.
check-js:
    node --check js/jco/websocket.js
    node --check js/componentize/websocket.js
    node --check js/componentize/wpt/harness.js
    node --check js/componentize/wpt/wpt-env.js
    node --check js/componentize/wpt/runner.js
    node --check js/componentize/wpt/smoke.js
    node --check js/componentize/wpt/reporter.js
    node --check js/componentize/wpt/parity/sockets-stub.mjs
    node --check js/componentize/wpt/parity/legs.mjs
    node --check js/componentize/wpt/parity/baseline.mjs
    node --check js/componentize/wpt/parity/roundtrip.mjs
    node --check js/componentize/wpt/parity/run-browser.mjs
    node --check js/componentize/wpt/parity/compare.mjs
    node --check conformance/driver-ct/jco/harness.mjs
    node --check conformance/driver-ct/jco/run-node.mjs
    node --check conformance/driver-ct/jco/run-browser.mjs
    node --check conformance/driver-ct/jco/browser-imports.mjs
    node --check conformance/server/echod.mjs
    node --check examples/jco-demo/run.mjs
    @echo "js: ok"

# Native tests (the workspace default-members).
test:
    cargo test

# The jco toolchain pin names one release-asset URL across all three JS
# package trees' package.json dependency specs. Catches a partial bump,
# which installs cleanly everywhere and drifts silently.
jco-pin-check:
    #!/usr/bin/env bash
    set -euo pipefail
    files=(
        conformance/driver-ct/jco/package.json
        examples/jco-demo/package.json
        js/componentize/wpt/parity/package.json
    )
    specs=()
    for f in "${files[@]}"; do
        spec="$(grep -oE '"@bytecodealliance/jco-transpile": "[^"]+"' "$f" | sort -u)"
        if [ "$(grep -c . <<<"$spec")" -ne 1 ]; then
            echo "jco-pin-check: expected exactly one jco-transpile spec in $f, found:" >&2
            printf '%s\n' "$spec" >&2
            exit 1
        fi
        specs+=("$spec")
    done
    if [ "$(printf '%s\n' "${specs[@]}" | sort -u | grep -c .)" -ne 1 ]; then
        echo "jco-pin-check: jco-transpile spec differs across pin sites:" >&2
        paste <(printf '%s\n' "${files[@]}") <(printf '%s\n' "${specs[@]}") >&2
        exit 1
    fi
    echo "jco pin: ${specs[0]#*: } (3 sites)"
