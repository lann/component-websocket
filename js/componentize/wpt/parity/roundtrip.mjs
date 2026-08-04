// The round-trip leg of the WPT parity gate: the same vendored WPT
// suites, but through the full carrier stack — shim, WIT, component ABI,
// jco, websocket-jco — terminating in the same platform `WebSocket` the
// baseline leg measured, against the same suite echo server. Any test the
// baseline passes and this leg does not is a loss introduced by that
// stack.
//
// Imports the jco transpile of parity-runner.component.wasm (see
// `npm run transpile`), collects the records the runner streams through
// its `wpt:parity/reporter` import, cross-checks the count `run` resolves
// to, and emits the records as JSON on stdout, matching baseline.mjs.

import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnEchod } from "../../../../conformance/adapters/jco/echod.mjs";
import { setSink } from "../reporter.js";
import { runner } from "./generated/parity-runner.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");

const records = [];
setSink((record) => records.push(JSON.parse(record)));

const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
let output;
try {
  output = await runner.run(echod.base);
} finally {
  await echod.shutdown();
}

const MARKER = "WPT-PARITY-STREAMED ";
if (typeof output !== "string" || !output.startsWith(MARKER)) {
  throw new Error(`parity runner returned an unexpected shape: ${String(output).slice(0, 200)}`);
}
const count = Number(output.slice(MARKER.length));
if (count !== records.length) {
  // A record lost between the runner and the sink must fail the run.
  throw new Error(`parity runner reported ${count} records; received ${records.length}`);
}
process.stdout.write(JSON.stringify(records));
