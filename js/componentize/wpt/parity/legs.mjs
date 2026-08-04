// The two parity legs' bodies, shared by every engine driver: the Node
// drivers (baseline.mjs, roundtrip.mjs) and the browser driver
// (run-browser.mjs) all run these same functions, so the legs cannot
// drift between engines — an engine driver only supplies the environment
// (an echo server URL, a generated tree) and serialization.
//
// Environment-neutral by construction: no Node imports, and every module
// reference is resolved against this module's own URL, so the same file
// works from `file:` (Node) and `http:` (a browser page). Module identity
// matters for the round trip: this module and the generated tree must
// reach the *same* reporter.js instance for `setSink` to observe the
// runner's records, which URL-relative resolution from the repository
// layout guarantees.

/**
 * The baseline leg: run the vendored WPT suites directly against this
 * platform's own `WebSocket` — no shim, no WIT, no wasm — against the
 * suite echo server at `wsBase`. Returns the runner-shaped records.
 * @param {string} wsBase
 * @returns {Promise<{ group: string, name: string, status: string, message?: string }[]>}
 */
export async function runBaseline(wsBase) {
  globalThis.__WPT_SERVER_BASE = wsBase;
  const [{ GROUP_MODULES }, { drain, takeResults }] = await Promise.all([
    import(new URL("../build/groups-manifest.js", import.meta.url).href),
    import(new URL("../harness.js", import.meta.url).href),
  ]);
  const records = [];
  for (const { name: group, module } of GROUP_MODULES) {
    const { start } = await import(new URL(`../build/${module}`, import.meta.url).href);
    start();
    await drain();
    for (const { name, status, message } of takeResults()) {
      records.push(message === undefined ? { group, name, status } : { group, name, status, message });
    }
  }
  return records;
}

/**
 * The round-trip leg: the same suites through the full carrier stack —
 * shim, WIT, component ABI, jco, websocket-jco — terminating in the same
 * platform `WebSocket` the baseline measured, against the same echo
 * server. `generated` names the jco transpile tree to load, relative to
 * this module: "generated" (the Node profile) or "generated-web" (the
 * browser profile). Collects the records the runner streams through its
 * `wpt:parity/reporter` import and cross-checks the count `run` resolves
 * to; a record lost between the runner and the sink fails the leg.
 * @param {string} wsBase
 * @param {string} generated
 * @returns {Promise<{ group: string, name: string, status: string, message?: string }[]>}
 */
export async function runRoundtrip(wsBase, generated) {
  const [{ setSink }, { runner }] = await Promise.all([
    import(new URL("../reporter.js", import.meta.url).href),
    import(new URL(`./${generated}/parity-runner.js`, import.meta.url).href),
  ]);
  const records = [];
  setSink((record) => records.push(JSON.parse(record)));
  const output = await runner.run(wsBase);
  const MARKER = "WPT-PARITY-STREAMED ";
  if (typeof output !== "string" || !output.startsWith(MARKER)) {
    throw new Error(`parity runner returned an unexpected shape: ${String(output).slice(0, 200)}`);
  }
  const count = Number(output.slice(MARKER.length));
  if (count !== records.length) {
    throw new Error(`parity runner reported ${count} records; received ${records.length}`);
  }
  return records;
}
