// The smoke entry's embedder (`just wpt::smoke`): instantiate the
// transpiled smoke component against the deferred host module and the
// suite echo server, and require the streamed marker back.
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnEchod } from "../../../../conformance/adapters/jco/echod.mjs";
import { setSink } from "../reporter.js";
import { runner } from "./generated-smoke/smoke.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");

setSink((record) => console.error("[smoke]", JSON.parse(record).name));
const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
let output;
try {
  output = await runner.run(echod.base);
} finally {
  await echod.shutdown();
}
if (typeof output !== "string" || !output.startsWith("WPT-PARITY-STREAMED ")) {
  console.error(`smoke returned an unexpected shape: ${String(output)}`);
  process.exit(1);
}
console.error(`smoke ok: ${output}`);
