# Conformance suite

Cross-implementation conformance tests for `lann:websocket`: one shared
guest component runs the same corpus against every implementation, and the
runner renders the results matrix. The suite is the behavioral gate for the
package — a change to the WIT surface is co-dependent across every
implementation *by construction*, because this suite holds them to one
behavior.

```
just conformance            # clean, build, run every target, classify
just conformance::wasmtime  # one target, end to end
just conformance::classify  # re-classify existing results
```

## How it works

| Piece | Role |
| --- | --- |
| [`guest/`](guest) | The shared conformance guest: exports `conformance:suite/runner` (`list-tests` + `run-test`), imports `lann:websocket/connections`, and owns **every assertion**. One wasm binary, run unchanged against every target. |
| [`server/`](server) | The suite-owned echo/reference server (`conformance-echod`): echo plus the fault modes the close-semantics rows need. Wire contract in [`server/PROTOCOL.md`](server/PROTOCOL.md). |
| [`tests.toml`](tests.toml) | The test registry: ids, tags, one-line descriptions. The guest's corpus and the adapters' registries mirror it; `verify_corpus` gates the mirrors against the guest's `list-tests` before anything runs. |
| [`targets.toml`](targets.toml) | Per-target capability facts: `unsupported` tags (with reasons) and `expected-fail` entries (with tracking issues). An undeclared divergence is a defect, not a fact to record. |
| [`adapters/`](adapters) | Per-target adapters: provision a host, drive the guest, emit one JSON result document (`results/<target>.json`). `common/` is the shared Rust vocabulary; `jco/` holds the Node and headless-Chromium runners plus their shared `driver.js`. |
| [`runner/`](runner) | Classifies result documents against the registry and target facts, renders `matrix.md`, and exits nonzero on any `FAIL`, `UNEXPECTED-PASS`, or undeclared skip. |

### The result document

An adapter's whole contract with the runner:

```json
{
  "target": "wasmtime",
  "environment": "loopback",
  "results": [{ "test_id": "echo-binary", "status": "pass", "detail": null }]
}
```

Raw statuses are `pass`/`fail`/`skip`; the runner reclassifies them against
`targets.toml`. Adapters exit nonzero only on harness errors — failing
cases are the runner's business. Adding a target = a new adapter that
emits a result document plus a new `[target.<id>]` table; the runner and
registry do not change.

### Timing and buffer bounds

Adapters configure the implementation under test with the suite's bounds
(see `adapters/common`): a 256 KiB inbound buffer (so
`receive-buffer-overflow` triggers with a bounded flood), a 5 s connect
bound (`/stall`), and a 3 s closing-handshake bound (`/ignore-close`). The
per-test hang guard is 60 s, single attempt, **no retries**: a
nondeterministic failure is a real signal and must surface, not be masked
by a second attempt.

Browser-specific scheduling: Chromium serializes in-flight WebSocket
handshakes per endpoint, so the jco driver runs the `/stall`-holding test
after the concurrent phase (see `adapters/jco/driver.js`).

## Evolution rules

- Never assert implementation-identical behavior where the WIT records
  latitude; assert the contract.
- A target that cannot serve a capability gets an `unsupported` declaration
  with a reason — never a weakened test.
- A known failure gets an `expected-fail` declaration with a tracking
  issue — never a deleted test. A passing expected-fail fails the run,
  forcing the declaration's cleanup.
- The corpus mirrors (guest `CORPUS`, `adapters/common` `TESTS`,
  `adapters/jco/driver.js` `TESTS`, `tests.toml`) must stay in sync;
  `verify_corpus` and the runner enforce it.
- Never copy the root WIT: the suite consumes it through the
  `wit/deps/lann-websocket` symlink.
- Conformance work must not change production host behavior except where a
  test deliberately drives a fix.
