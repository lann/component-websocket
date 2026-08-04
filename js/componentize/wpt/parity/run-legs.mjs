// Run the two parity legs and the comparator as one bounded flow (the
// `just wpt::parity` / `just wpt::update-losses` body): baseline, round
// trip, compare. A leg's failure prints its stderr and stops the run.
import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const update = process.argv.includes("--update");

/** Run `argv` in the parity directory, capturing stdout. */
function leg(name, argv) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, argv, { cwd: HERE, stdio: ["ignore", "pipe", "inherit"] });
    const chunks = [];
    child.stdout.on("data", (chunk) => chunks.push(chunk));
    child.on("exit", (code) => {
      if (code === 0) {
        resolve(Buffer.concat(chunks).toString());
      } else {
        reject(new Error(`${name} leg exited ${code}`));
      }
    });
    child.on("error", reject);
  });
}

await mkdir(join(HERE, "build"), { recursive: true });
const baseline = await leg("baseline", ["baseline.mjs"]);
await writeFile(join(HERE, "build", "parity-baseline.json"), baseline);
const roundtrip = await leg("roundtrip", ["--experimental-wasm-jspi", "roundtrip.mjs"]);
await writeFile(join(HERE, "build", "parity-roundtrip.json"), roundtrip);

const compareArgs = ["compare.mjs", "build/parity-baseline.json", "build/parity-roundtrip.json"];
if (update) compareArgs.push("--update");
await new Promise((resolve, reject) => {
  const child = spawn(process.execPath, compareArgs, { cwd: HERE, stdio: "inherit" });
  child.on("exit", (code) => (code === 0 ? resolve() : reject(new Error(`compare exited ${code}`))));
  child.on("error", reject);
});
