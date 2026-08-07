// The jco browser leg: the same transpiled suite and the same host
// module (js/jco/websocket.js) inside a real headless Chromium — the
// environment the browser-first host actually targets — emitting
// component-test results JSONL. The page, worker, stall watchdog, and
// Chrome ladder live in the upstream browser driver; this file is the
// frame: the echo server, the environment, target configuration, and
// results writing.
//
// jco's async ABI needs JSPI; Chrome ships it enabled from 137 onward.
// The page is served from http://127.0.0.1:<port> and opens ws:
// connections to the echo server directly (WebSocket is CORS-exempt).
import { access, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  buildHarnessPage,
  findChrome,
  runPageHarness,
} from "@polymorph/component-test-js/browser-driver";
import { writeResultsFile } from "@polymorph/component-test-js/node-runner";

import { spawnEchod, unreachableUrl } from "../../server/echod.mjs";

const JCO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(JCO_DIR, "..", "..", "..");
const BASE = "/conformance/driver-ct/jco";
const MAX_INBOUND_BUFFER_BYTES = 256 * 1024;
const CASE_TIMEOUT_MS = 60_000;
// The pool heartbeats per suite and per 25 rows; quiet time is bounded
// by a batch of the slowest handshake cases.
const STALL_TIMEOUT_MS = 90_000;

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(JCO_DIR, "generated") },
    out: {
      type: "string",
      default: join(REPO_ROOT, "conformance", "driver-ct", "results"),
    },
    target: { type: "string", default: "jco-browser" },
    server: { type: "string" },
    "tls-server": { type: "string" },
    "echod-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-echod"),
    },
  },
});

async function main() {
  try {
    await access(join(values.generated, "conformance-guest-ct.js"));
  } catch {
    throw new Error(`missing transpiled suite in ${values.generated}; run "npm run transpile" first`);
  }

  const owned = values.server ? null : await spawnEchod(values["echod-bin"]);
  const serverUrl = values.server ?? owned.base;
  const tlsServerUrl = values["tls-server"] ?? owned?.tlsBase;
  if (!tlsServerUrl) {
    throw new Error("--server requires --tls-server (the suite echo server's wss: base URL)");
  }

  const coreUrls = (await readdir(values.generated))
    .filter((n) => n.endsWith(".wasm"))
    .sort()
    .map((n) => `${BASE}/generated/${n}`);

  const config = {
    // Sequential: the corpus is loopback-I/O-bound, and one worker
    // sidesteps Chromium's per-endpoint handshake serialization.
    jobs: 1,
    suites: [
      {
        suite: "conformance-guest-ct",
        target: values.target,
        moduleUrl: `${BASE}/generated/conformance-guest-ct.js`,
        coreUrls,
        importsUrl: `${BASE}/browser-imports.mjs`,
        missing: [],
        caseTimeoutMs: CASE_TIMEOUT_MS,
        env: [
          ["WS_CONFORMANCE_SERVER_URL", serverUrl],
          ["WS_CONFORMANCE_TLS_SERVER_URL", tlsServerUrl],
          ["WS_CONFORMANCE_UNREACHABLE_URL", await unreachableUrl()],
          ["WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES", String(MAX_INBOUND_BUFFER_BYTES)],
        ],
      },
    ],
  };

  const playwright = await import("playwright-core");
  let outcome;
  try {
    outcome = await runPageHarness({
      playwright,
      engine: "chromium",
      executablePath: await findChrome(),
      repoRoot: REPO_ROOT,
      html: buildHarnessPage({
        title: "polymorph:websocket conformance (jco-browser)",
        config,
      }),
      stallTimeoutMs: STALL_TIMEOUT_MS,
      // --ignore-certificate-errors provisions trust for the committed
      // test PKI (loopback-only browser instance).
      launchArgs: ["--no-sandbox", "--disable-dev-shm-usage", "--ignore-certificate-errors"],
    });
  } finally {
    if (owned) await owned.shutdown();
  }

  const run = outcome[values.target];
  if (!run) throw new Error(`the page reported no run for target ${values.target}`);
  const outPath = await writeResultsFile({
    dir: values.out,
    target: values.target,
    lines: run.lines,
  });
  const c = run.counts;
  process.stderr.write(`wrote ${outPath} (${c.total} cases, ${c.failed} failed)\n`);
  process.exit(c.failed === 0 && c.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
