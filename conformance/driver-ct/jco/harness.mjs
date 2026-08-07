// Leg-shared glue for the jco conformance legs: the SUT import wiring
// (bindImports) and the thin suite loop over the upstream runner core
// (@polymorph/component-test-js — one harness for every runner, per
// polymorph-components/polymorph-test#5; the case loop, verdict mapping, and tag
// inventory live there, not here). Runs in Node and inside the browser
// page unchanged (the browser page maps the bare specifiers through an
// import map; see run-browser.mjs).
//
// Scheduling: the upstream loop mark-schedules against the tag
// inventory read from the transpiled core wasm. The suite is untagged
// and targets.toml declares no features, so `missing` is always empty
// and every case runs — but the inventory lookup still gates drift:
// a case the tags section does not cover fails the run as unsound
// (the jco analog of the wasmtime runner's cross-check).
//
// Cases run sequentially: the corpus is loopback-I/O-bound (~tens of
// seconds total), and sequential execution sidesteps Chromium's
// per-endpoint handshake serialization (the incumbent driver's
// HANDSHAKE_BLOCKING workaround) entirely.

import { envelope, inventoryLookup, runCases } from "@polymorph/component-test-js/harness";
import { Context } from "@polymorph/component-test-js/context";
import {
  bindImports as bindCoreImports,
  envInterface,
} from "@polymorph/component-test-js/imports";

// The single-attempt wall bound per case, matching the wasmtime leg's
// --case-timeout: a wedged case is reported (limit-exceeded provenance),
// never retried, and never allowed to hang the leg. JSPI attempts
// cannot be cancelled — the abandoned attempt's promise keeps running
// until the leg exits, which is why every case gets a fresh instance
// (freshCases below): a timed-out instance may be wedged mid-suspension.
const CASE_TIMEOUT_MS = 60_000;

/**
 * The suite's import object over the upstream builder: the SUT host,
 * the test-context provider, the config environment, and the wasi
 * shims.
 */
export function bindImports({ connections, env, cli, clocks, io }) {
  return bindCoreImports({
    wasi: { cli, clocks, io },
    env,
    sut: { "polymorph:websocket/connections": connections },
  });
}

export { envInterface };

/**
 * Run the whole suite through the upstream case loop. `newInstance()`
 * must return a fresh instantiated suite (exports object) — census and
 * every case each get their own; `coreBytes` are the transpiled core
 * wasm bytes carrying the tag inventory; `emit(line)` receives each
 * JSONL line. Returns `{ total, failed }`.
 */
export async function runSuite({ newInstance, coreBytes, target, suiteName, emit, log }) {
  emit(JSON.stringify(envelope(target, suiteName)));
  const tagsOf = inventoryLookup(coreBytes);
  const census = await (await newInstance()).tests.all();
  const counts = await runCases({
    cases: census,
    Context,
    tagsOf,
    missing: [],
    emit: (event) => {
      emit(JSON.stringify(event));
      log?.(`${event.case} … ${event.status}`);
    },
    caseTimeoutMs: CASE_TIMEOUT_MS,
    freshCases: async () => (await newInstance()).tests.all(),
  });
  if (counts.total === 0) {
    throw new Error("suite enumerated zero cases (empty selection is a run error)");
  }
  emit(JSON.stringify({ "segment-end": true }));
  return { total: counts.total, failed: counts.failed };
}
