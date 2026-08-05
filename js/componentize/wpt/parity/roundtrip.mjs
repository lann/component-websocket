// The round-trip leg's Node driver: spawn the suite echo server, run the
// shared leg body (legs.mjs) through the Node-profile jco transpile
// (`npm run transpile` — see generated/), and emit the records as JSON on
// stdout, matching baseline.mjs. Needs --experimental-wasm-jspi (see
// run-legs.mjs).

import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnEchod } from "../../../../conformance/server/echod.mjs";
import { runRoundtrip } from "./legs.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");

const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
let records;
try {
  records = await runRoundtrip(echod.base, "generated");
} finally {
  await echod.shutdown();
}
process.stdout.write(JSON.stringify(records));
