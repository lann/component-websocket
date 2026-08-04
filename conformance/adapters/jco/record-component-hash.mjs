// Record the sha256 of the component a transpile consumed, so the runners
// can stamp their reports with the build they actually executed (the
// runner rejects a matrix assembled from mixed guest builds).
//
// Usage: node record-component-hash.mjs <component.wasm> <out-file>
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

const [component, out] = process.argv.slice(2);
if (!component || !out) {
  console.error("usage: record-component-hash.mjs <component.wasm> <out-file>");
  process.exit(1);
}
const digest = createHash("sha256")
  .update(await readFile(component))
  .digest("hex");
await writeFile(out, `${digest}\n`);
console.error(`${out}: ${digest}`);
