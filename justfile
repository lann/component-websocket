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
check: fmt-check clippy validate-wit check-js test

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
    node --check js/componentize/wpt/parity/deferred-connections.mjs
    node --check js/componentize/wpt/parity/sockets-stub.mjs
    node --check js/componentize/wpt/parity/legs.mjs
    node --check js/componentize/wpt/parity/baseline.mjs
    node --check js/componentize/wpt/parity/roundtrip.mjs
    node --check js/componentize/wpt/parity/run-browser.mjs
    node --check js/componentize/wpt/parity/compare.mjs
    node --check conformance/driver-ct/jco/harness.mjs
    node --check conformance/driver-ct/jco/run-node.mjs
    node --check conformance/driver-ct/jco/run-browser.mjs
    node --check conformance/server/echod.mjs
    node --check scripts/chrome.mjs
    node --check scripts/patch-jco-string-lowering.mjs
    node --check examples/jco-demo/run.mjs
    @echo "js: ok"

# Native tests (the workspace default-members).
test:
    cargo test
