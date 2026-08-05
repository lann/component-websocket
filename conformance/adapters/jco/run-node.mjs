// The jco Node conformance adapter: runs the shared conformance guest
// against the browser-first host (`js/jco/websocket.js`) under Node, backed
// by Node's built-in browser-compatible `WebSocket` global, and emits the
// adapter result document the conformance runner consumes
// (`conformance/results/jco-node.json`).
//
// jco's async ABI needs JavaScript Promise Integration (JSPI), so this must
// run under a JSPI-capable runtime: Node 24+ with
// `--experimental-wasm-jspi` (the npm `run:node` script supplies it).
import { access, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { availableParallelism } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  runCorpus,
  CLOSE_TIMEOUT_MS,
  CONNECT_TIMEOUT_MS,
  MAX_INBOUND_BUFFER_BYTES,
} from "./driver.js";
import { requireNode24, spawnEchod, unreachableUrl } from "./echod.mjs";

requireNode24();
// The single host module, shared with the demo runners and the browser
// adapter.
import * as connections from "../../../js/jco/websocket.js";

// Apply the conformance bounds through the module's exported hooks (see
// driver.js for why each bound exists). Connections capture them at
// `connect`, so configuring the module here covers every instance.
connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);
connections.setConnectTimeoutMs(CONNECT_TIMEOUT_MS);
connections.setCloseTimeoutMs(CLOSE_TIMEOUT_MS);

const ADAPTER_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(ADAPTER_DIR, "..", "..", "..");

function defaultJobs() {
  return Math.min(8, Math.max(2, 2 * availableParallelism()));
}

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(ADAPTER_DIR, "generated") },
    out: { type: "string", default: join(REPO_ROOT, "conformance", "results") },
    target: { type: "string", default: "jco-node" },
    environment: { type: "string", default: "loopback" },
    // Base URL of an already-running echo server. When omitted, this
    // adapter spawns its own `conformance-echod`.
    server: { type: "string" },
    "tls-server": { type: "string" },
    "echod-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-echod"),
    },
    only: { type: "string", multiple: true, default: [] },
    jobs: { type: "string" },
  },
});

/** Compile the guest's core wasm modules so `instantiate` can resolve them synchronously. */
async function loadCoreModules(generatedDir) {
  const modules = new Map();
  for (const name of await readdir(generatedDir)) {
    if (name.endsWith(".wasm")) {
      modules.set(name, await WebAssembly.compile(await readFile(join(generatedDir, name))));
    }
  }
  return modules;
}


async function main() {
  const generatedDir = values.generated;
  try {
    await access(join(generatedDir, "conformance-guest.js"));
  } catch {
    throw new Error(
      `missing transpiled guest in ${generatedDir}; run "npm run transpile" first`,
    );
  }
  const { instantiate } = await import(join(generatedDir, "conformance-guest.js"));
  const modules = await loadCoreModules(generatedDir);

  const newInstance = () =>
    instantiate((name) => modules.get(name), {
      "lann:websocket/connections": connections,
    });

  const owned = values.server ? null : await spawnEchod(values["echod-bin"]);
  const serverUrl = values.server ?? owned.base;
  const tlsServerUrl = values["tls-server"] ?? owned?.tlsBase;
  if (!tlsServerUrl) {
    throw new Error("--server requires --tls-server (the suite echo server's wss: base URL)");
  }
  process.stderr.write(`echo server ready at ${serverUrl} (tls: ${tlsServerUrl})\n`);

  let results;
  try {
    results = await runCorpus({
      newInstance,
      serverUrl,
      tlsServerUrl,
      unreachableUrl: await unreachableUrl(),
      only: values.only,
      jobs: values.jobs ? Number(values.jobs) : defaultJobs(),
      log: (msg) => process.stderr.write(`${msg}\n`),
    });
  } finally {
    if (owned) await owned.shutdown();
  }

  const guest = (
    await readFile(join(values.generated, "component-hash.txt"), "utf8").catch(() => "")
  ).trim();
  const report = { target: values.target, environment: values.environment, guest, results };
  await mkdir(values.out, { recursive: true });
  const outPath = join(values.out, `${values.target}.json`);
  await writeFile(outPath, `${JSON.stringify(report, null, 2)}\n`);
  const failed = results.filter((r) => r.status === "fail").length;
  // Failing cases are the runner's business; this adapter exits nonzero
  // only on harness errors.
  process.stderr.write(`wrote ${outPath} (${results.length} tests, ${failed} failed)\n`);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(1);
  },
);
