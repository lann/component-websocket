// Environment-agnostic corpus driver for the component-test suite:
// instantiates the transpiled suite fresh per case, drives the tests
// contract, and emits component-test results JSONL (the #26 edge
// encoding). Runs in Node and inside the browser page unchanged.
//
// Scheduling: this harness executes everything and says so —
// `"scheduling":"none"` in the envelope — so the aggregate applies
// feature applicability from the lockfile + manifest (the
// component-test #36 mechanism). No tags-section parsing in JS, no
// duplicated scheduling semantics to keep in sync.
//
// Cases run sequentially: the corpus is loopback-I/O-bound (~tens of
// seconds total), and sequential execution sidesteps Chromium's
// per-endpoint handshake serialization (the incumbent driver's
// HANDSHAKE_BLOCKING workaround) entirely.

import { Context } from "./context.js";

// The single-attempt wall bound per case, matching the wasmtime leg's
// --case-timeout: a wedged case is reported (limit-exceeded provenance,
// the component-test vocabulary), never retried, and never allowed to
// hang the leg. The abandoned attempt's promise keeps running until the
// leg exits; each case gets a fresh instance anyway.
const CASE_TIMEOUT_MS = 60_000;

/**
 * The suite's import object, both key spellings (the generated code
 * mixes versioned and unversioned): the SUT host, the test-context
 * shim, the config environment, and the wasi shims.
 */
export function bindImports({ connections, Context, env, cli, clocks, io }) {
  const imports = {};
  const bind = (name, impl) => {
    imports[name] = impl;
    const versioned = name.startsWith("wasi:") ? `${name}@0.2.0` : `${name}@0.1.0`;
    imports[versioned] = impl;
  };
  bind("lann:websocket/connections", connections);
  bind("lann:component-test/test-context", { Context });
  bind("wasi:cli/environment", envInterface(env));
  bind("wasi:cli/exit", cli.exit);
  bind("wasi:cli/stdin", cli.stdin);
  bind("wasi:cli/stdout", cli.stdout);
  bind("wasi:cli/stderr", cli.stderr);
  bind("wasi:cli/terminal-input", cli.terminalInput);
  bind("wasi:cli/terminal-output", cli.terminalOutput);
  bind("wasi:cli/terminal-stdin", cli.terminalStdin);
  bind("wasi:cli/terminal-stdout", cli.terminalStdout);
  bind("wasi:cli/terminal-stderr", cli.terminalStderr);
  bind("wasi:clocks/monotonic-clock", clocks.monotonicClock);
  bind("wasi:clocks/wall-clock", clocks.wallClock);
  bind("wasi:io/error", io.error);
  bind("wasi:io/poll", io.poll);
  bind("wasi:io/streams", io.streams);
  return imports;
}

/** One environment interface for both legs (explicit > shim-internal). */
export function envInterface(vars) {
  return {
    getEnvironment: () => vars,
    getArguments: () => [],
    initialCwd: () => undefined,
  };
}

/**
 * Run the whole suite. `newInstance()` must return a fresh instantiated
 * suite (exports object); `emit(line)` receives each JSONL line.
 * Returns `{ total, failed }`.
 */
export async function runSuite({ newInstance, target, suiteName, emit, log }) {
  emit(
    JSON.stringify({
      "component-test-results": "0.1",
      target,
      suite: { name: suiteName },
      run: { segment: 0, scheduling: "none" },
    }),
  );

  // Census from a fresh instance; each case then runs in its own.
  const census = await (await newInstance()).tests.all();
  const names = [];
  for (const testCase of census) {
    names.push(testCase.name());
  }
  if (names.length === 0) {
    throw new Error("suite enumerated zero cases (empty selection is a run error)");
  }

  let failed = 0;
  for (const name of names) {
    const diagnostics = [];
    const instance = await newInstance();
    const cases = await instance.tests.all();
    const testCase = cases.find((c) => c.name() === name);
    if (!testCase) {
      throw new Error(`case ${name} vanished on re-enumeration`);
    }
    let event;
    let timer;
    const outcome = await Promise.race([
      testCase.run(new Context(diagnostics)).then(
        () => ({ kind: "pass" }),
        (e) => ({ kind: "thrown", e }),
      ),
      new Promise((r) => {
        timer = setTimeout(() => r({ kind: "timeout" }), CASE_TIMEOUT_MS);
      }),
    ]);
    clearTimeout(timer);
    if (outcome.kind === "pass") {
      event = { case: name, status: "pass", provenance: "returned" };
    } else if (outcome.kind === "timeout") {
      failed += 1;
      event = {
        case: name,
        status: "fail",
        provenance: { "limit-exceeded": "case-timeout" },
        detail: `case timeout exceeded (${CASE_TIMEOUT_MS / 1000}s)`,
      };
    } else {
      // jco maps result::err to a thrown ComponentError with .payload;
      // anything else is a trap (or shim bug), attributed as such.
      const payload = outcome.e?.payload ?? outcome.e;
      if (payload?.tag === "failed") {
        failed += 1;
        event = { case: name, status: "fail", provenance: "returned", detail: payload.val };
      } else if (payload?.tag === "skipped") {
        event = { case: name, status: "skipped", provenance: "returned", detail: payload.val };
      } else {
        failed += 1;
        event = {
          case: name,
          status: "fail",
          provenance: "trap",
          detail: String(outcome.e?.message ?? outcome.e).split("\n")[0],
        };
      }
    }
    event.diagnostics = diagnostics;
    event["diagnostics-complete"] = event.status !== "fail" || event.provenance === "returned";
    emit(JSON.stringify(event));
    log?.(`${name} … ${event.status}`);
  }

  emit(JSON.stringify({ "segment-end": true }));
  return { total: names.length, failed };
}
