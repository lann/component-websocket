// The jco Node leg of the component-test conformance harness: runs the
// transpiled suite against the browser-first host (js/jco/websocket.js)
// backed by Node's built-in WebSocket, and writes component-test
// results JSONL for the aggregate.
//
// jco's async ABI needs JSPI: Node 24+ with --experimental-wasm-jspi
// (the npm `run:node` script supplies it).
import { access, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { cli, clocks, io } from "@bytecodealliance/preview2-shim";

import { bindImports, runSuite } from "./harness.mjs";
import { requireNode24, spawnEchod, unreachableUrl } from "../../server/echod.mjs";

requireNode24();
import * as connections from "../../../js/jco/websocket.js";

// The suite bounds, matching the wasmtime leg (the bound rationale
// lives on the consts in driver-ct/src/main.rs). Connections capture
// them at connect, so configuring the module once covers every case.
const MAX_INBOUND_BUFFER_BYTES = 256 * 1024;
connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);
connections.setConnectTimeoutMs(5000);
connections.setCloseTimeoutMs(3000);

const JCO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(JCO_DIR, "..", "..", "..");

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(JCO_DIR, "generated") },
    out: {
      type: "string",
      default: join(REPO_ROOT, "conformance", "driver-ct", "results"),
    },
    target: { type: "string", default: "jco-node" },
    server: { type: "string" },
    "echod-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-echod"),
    },
  },
});

async function loadCoreModules(generatedDir) {
  const modules = new Map();
  const coreBytes = [];
  for (const name of await readdir(generatedDir)) {
    if (name.endsWith(".wasm")) {
      const bytes = new Uint8Array(await readFile(join(generatedDir, name)));
      coreBytes.push(bytes);
      modules.set(name, await WebAssembly.compile(bytes));
    }
  }
  return { modules, coreBytes };
}


async function main() {
  const generatedDir = values.generated;
  try {
    await access(join(generatedDir, "conformance-guest-ct.js"));
  } catch {
    throw new Error(`missing transpiled suite in ${generatedDir}; run "npm run transpile" first`);
  }
  const { instantiate } = await import(join(generatedDir, "conformance-guest-ct.js"));
  const { modules, coreBytes } = await loadCoreModules(generatedDir);

  const owned = values.server ? null : await spawnEchod(values["echod-bin"]);
  const serverUrl = values.server ?? owned.base;
  process.stderr.write(`echo server ready at ${serverUrl}\n`);
  const env = [
    ["WS_CONFORMANCE_SERVER_URL", serverUrl],
    ["WS_CONFORMANCE_UNREACHABLE_URL", await unreachableUrl()],
    ["WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES", String(MAX_INBOUND_BUFFER_BYTES)],
  ];

  const imports = bindImports({ connections, env, cli, clocks, io });

  const newInstance = () => instantiate((name) => modules.get(name), imports);

  const lines = [];
  let summary;
  try {
    summary = await runSuite({
      newInstance,
      coreBytes,
      target: values.target,
      suiteName: "conformance_guest_ct",
      emit: (line) => lines.push(line),
      log: (msg) => process.stderr.write(`${msg}\n`),
    });
  } finally {
    if (owned) await owned.shutdown();
  }

  await mkdir(values.out, { recursive: true });
  const outPath = join(values.out, `${values.target}.jsonl`);
  await writeFile(outPath, `${lines.join("\n")}\n`);
  process.stderr.write(
    `wrote ${outPath} (${summary.total} cases, ${summary.failed} failed)\n`,
  );
  process.exit(summary.failed === 0 && summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
