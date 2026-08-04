# The orchestration surface: repo-wide recipes plus one module per software
# component, each module's justfile colocated with its code.

import 'justfile.shared.just'

mod gha '.github'
mod conformance
mod demo 'examples'

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
    cargo clippy --target wasm32-unknown-unknown -p conformance-guest -p echo-demo -- -D warnings

# Validate every WIT tree: the shared package and each consumer's world
# (which pulls the package in through its deps symlink).
validate-wit:
    wasm-tools component wit wit >/dev/null
    wasm-tools component wit rust/wasmtime/wit >/dev/null
    wasm-tools component wit conformance/wit >/dev/null
    wasm-tools component wit examples/echo-demo/wit >/dev/null
    @echo "wit: ok"

# Syntax-check the JavaScript trees; nothing else compiles them before a
# full conformance run would.
check-js:
    node --check js/jco/websocket.js
    node --check conformance/adapters/jco/driver.js
    node --check conformance/adapters/jco/echod.mjs
    node --check conformance/adapters/jco/run-node.mjs
    node --check conformance/adapters/jco/run-browser.mjs
    node --check conformance/adapters/jco/record-component-hash.mjs
    node --check examples/jco-demo/run.mjs
    node --check scripts/patch-jco-string-lowering.mjs
    @echo "js: ok"

# Native tests (the workspace default-members).
test:
    cargo test
