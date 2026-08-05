# Conformance suite

Cross-implementation conformance tests for `polymorph:websocket`, on the shared
[`polymorph:test`](https://github.com/polymorph-components/polymorph-test)
infrastructure: one suite component runs the same corpus against every
implementation, and the aggregate joins the per-target result streams into
the committed matrix. The suite is the behavioral gate for the package — a
change to the WIT surface is co-dependent across every implementation *by
construction*, because this suite holds them to one behavior.

```
just conformance-ct                       # build, lock-check, run every leg, aggregate, matrix-check
just conformance-ct::run-wasmtime         # one leg (aggregate separately)
just conformance-ct::lock-update          # regenerate the lockfile after suite changes
just conformance-ct::matrix-update        # accept an intentionally changed matrix
```

The matrix (`driver-ct/matrix.md`) is **committed** — the cross-target
review surface, diffed by `matrix-check`; CI also uploads the generated
copy as an artifact. Failure details appear in the per-target
`driver-ct/results/<target>.jsonl` streams and in the matrix's Failures
section.

## How it works

| Piece | Role |
| --- | --- |
| [`guest-ct/`](guest-ct) | The suite component: 52 `#[case]`s importing `polymorph:websocket/connections`, owning **every assertion**. One wasm binary, run unchanged against every target. The committed `tests.lock` is its inventory (drift fails `lock-check` and the runner's own cross-check). |
| [`server/`](server) | The suite-owned echo/reference server (`conformance-echod`): echo plus the fault modes the close-semantics rows need. Wire contract in [`server/PROTOCOL.md`](server/PROTOCOL.md); `server/echod.mjs` holds the Node-side spawn helpers every JS leg shares. |
| [`driver-ct/`](driver-ct) | The legs and the aggregate. `ct-driver` (Rust) embeds the wasmtime host (and, with `--composed`, runs the wac-composed in-guest provider under WASI p2+p3 instead); `jco/` runs the transpiled suite in Node and headless Chromium through one shared `harness.mjs` — thin glue (SUT import wiring, config) over the upstream runner core (`@polymorph/component-test-js`, the git dep), which owns the case loop, verdict mapping, and tag-inventory drift check. `targets.toml` declares targets, features, and expected-fail entries; `component-test aggregate` validates and renders the matrix. The composed leg executes a different artifact by construction (the suite with the provider plugged in), so it binds its results envelope to the uncomposed suite's bytes (`--suite-artifact`, the runner's `bind_suite_artifact` attestation) and the aggregate's cross-target artifact agreement covers all four legs. |
| [`wit/`](wit) | The suite's world (`sut-imports`): only the surface under test — the export surface comes from the component-test SDK. The `polymorph:websocket` package arrives through the `deps/polymorph-websocket` symlink, never a copy. |

### The result stream

Each leg emits the component-test results wire format (JSONL: envelope,
one event per case, terminator) — see the component-test README for the
schema. The JS legs declare `"scheduling":"none"` in the envelope: they
execute everything, and the aggregate applies feature applicability from
the lockfile + manifest, so no scheduling logic lives in JavaScript.
Adding a target = a new leg that emits a result stream plus a
`[targets.<id>]` table in `driver-ct/targets.toml`; the suite and the
aggregate do not change.

### Timing and buffer bounds

Every leg configures the implementation under test with the same bounds
(declared in `driver-ct/src/main.rs`, mirrored by the jco runners): an
inbound-buffer bound small enough that the overflow rows trigger with a
bounded flood (it rides the `WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES`
store environment, so the guest floods against exactly the configured
value), and connect/close bounds short enough that the `/stall` and
`/ignore-close` probes resolve well inside the per-case wall bound. That
bound (60s, the runner's `--case-timeout`; mirrored in `harness.mjs`)
runs a single attempt, **no retries**: a nondeterministic failure is a
real signal and must surface, not be masked by a second attempt. Message
counts and sizes are the guest's own (`params` in `guest-ct/src/body.rs`):
every target runs the identical workload by construction.

Browser-specific scheduling: Chromium serializes in-flight WebSocket
handshakes per endpoint. The jco legs run cases sequentially, so nothing
is ever concurrent with the `/stall` hold; if a concurrent scheduler is
ever introduced, it needs an equivalent of the old harness's
handshake-blocking workaround.

## Adding a test

1. `guest-ct/src/body.rs`: add the case body (a `Result<(), String>`
   dispatch arm, and a `params` entry if count-parameterized).
2. `guest-ct/src/lib.rs`: add the `#[case]` delegator in the right
   category module.
3. If the test needs new server behavior, add the mode to `server/` and
   document it in `server/PROTOCOL.md`.
4. `just conformance-ct::lock-update` — commit the lockfile diff (it is
   the review surface for corpus changes).
5. `just conformance-ct` — all targets must pass (or carry a
   `targets.toml` declaration with a reason), then
   `matrix-update` if row counts changed.

## Evolution rules

- Never assert implementation-identical behavior where the WIT records
  latitude; assert the contract.
- A target that cannot serve a capability gets a **gated feature**: a
  `[features.<x>]` entry in `driver-ct/targets.toml`, tags on the
  affected cases, a `!x` decline probe, and `missing-features` on the
  target — never a weakened test.
- A known failure gets an `[[targets.<t>.expected-fail]]` declaration
  with a reason and a tracking issue — never a deleted test. A passing
  expected-fail fails the aggregate, forcing the declaration's cleanup.
- The committed lockfile is the corpus inventory; regenerate it with
  `lock-update` and review the diff. Drift fails `lock-check` and the
  runner's cross-check.
- Never copy the root WIT: the suite consumes it through the
  `wit/deps/polymorph-websocket` symlink.
- Conformance work must not change production host behavior except where a
  test deliberately drives a fix.

## Provisioning

The suite and driver consume component-test as git-sourced cargo
dependencies, rev-pinned in the root `Cargo.toml`'s
`[workspace.dependencies]` (both entries move together; Cargo.lock
records the resolution). The `component-test` CLI used by the lockfile
and aggregate recipes is cargo-installed into `target/ct-tools` at the
rev read back out of Cargo.lock (`conformance-ct::_ct-tools`), so the
libraries and the CLI cannot drift apart. Registry dependencies replace
the git pins when component-test publishes. The jco legs consume the
JS runner core the same way: `@polymorph/component-test-js` in
`driver-ct/jco/package.json`, a `github:polymorph-components/polymorph-test#<rev>` git
dep pinned to the same rev as the cargo entries (one rev everywhere —
the root `Cargo.toml` comment is the bump checklist).
