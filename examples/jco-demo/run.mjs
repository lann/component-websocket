// Node runner for the echo-demo component: instantiates the transpiled
// guest with the browser-first host module and drives it against a
// spawned suite echo server. Requires Node 24+ (JSPI; the npm `start`
// script supplies the flag).
import { readdir, readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// The demo consumes the suite's echo server and Node helpers (the
// suite-facing stable surface: the binary's LISTENING line contract).
import { requireNode24, spawnEchod } from "../../conformance/adapters/jco/echod.mjs";
import * as connections from "../../js/jco/websocket.js";

requireNode24();

const DEMO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(DEMO_DIR, "..", "..");
const COUNT = Number(process.argv[2] ?? 100);

const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
const base = echod.base;

try {
  const generated = join(DEMO_DIR, "generated");
  const { instantiate } = await import(join(generated, "echo-demo.js"));
  const modules = new Map();
  for (const name of await readdir(generated)) {
    if (name.endsWith(".wasm")) {
      modules.set(name, await WebAssembly.compile(await readFile(join(generated, name))));
    }
  }
  const instance = await instantiate((name) => modules.get(name), {
    "lann:websocket/connections": connections,
  });
  // jco lifts the export's `result<u32, string>` into return-or-throw.
  try {
    const received = await instance.demo.run(`${base}/echo`, COUNT);
    console.log(`round-tripped ${received}/${COUNT} messages`);
  } catch (err) {
    console.error(`demo failed: ${err?.payload ?? err}`);
    process.exitCode = 1;
  }
} finally {
  await echod.shutdown();
}
