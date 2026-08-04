// The baseline leg of the WPT parity gate: run the vendored WPT suites
// directly against this platform's own `WebSocket` (Node's built-in), with
// no shim, no WIT, and no wasm in the path — but against the same suite
// echo server. The comparator holds the round trip to this leg's pass
// set, so whatever this platform does not implement falls out of scope
// without an exclusion list.
//
// Emits the same `{ group, name, status, message? }` records as the
// runner, as JSON on stdout.

import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnEchod } from "../../../../conformance/adapters/jco/echod.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");

const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
globalThis.__WPT_SERVER_BASE = echod.base;

const records = [];
try {
  const [{ GROUP_MODULES }, { drain, takeResults }] = await Promise.all([
    import("../build/groups-manifest.js"),
    import("../harness.js"),
  ]);
  for (const { name: group, module } of GROUP_MODULES) {
    const { start } = await import(new URL(`../build/${module}`, import.meta.url).href);
    start();
    await drain();
    for (const { name, status, message } of takeResults()) {
      records.push(message === undefined ? { group, name, status } : { group, name, status, message });
    }
  }
} finally {
  await echod.shutdown();
}
process.stdout.write(JSON.stringify(records));
