// The baseline leg's Node driver: spawn the suite echo server, run the
// shared leg body (legs.mjs) against Node's built-in `WebSocket`, and
// emit the records as JSON on stdout. The comparator holds the round
// trip to this leg's pass set, so whatever this platform does not
// implement falls out of scope without an exclusion list.

import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnEchod } from "../../../../conformance/adapters/jco/echod.mjs";
import { runBaseline } from "./legs.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");

const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
let records;
try {
  records = await runBaseline(echod.base);
} finally {
  await echod.shutdown();
}
process.stdout.write(JSON.stringify(records));
