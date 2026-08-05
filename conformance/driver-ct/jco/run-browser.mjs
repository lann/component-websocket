// The jco browser leg: the same transpiled suite and the same host
// module (js/jco/websocket.js) inside a real headless Chromium — the
// environment the browser-first host actually targets — emitting
// component-test results JSONL. Browser counterpart of run-node.mjs;
// the import wiring and suite loop live in harness.mjs, shared, and
// the page resolves its bare @polymorph/component-test-js specifiers
// through an import map onto the served facade files.
//
// jco's async ABI needs JSPI; Chrome ships it enabled from 137 onward.
// The page is served from http://127.0.0.1:<port> and opens ws:
// connections to the echo server directly (WebSocket is CORS-exempt).
import http from "node:http";
import { access, mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { chromium } from "playwright-core";

import { findChrome } from "../../../scripts/chrome.mjs";
import { spawnEchod, unreachableUrl } from "../../server/echod.mjs";

const JCO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(JCO_DIR, "..", "..", "..");
const SHIM_BROWSER_DIR = join(
  JCO_DIR,
  "node_modules",
  "@bytecodealliance",
  "preview2-shim",
  "lib",
  "browser",
);
// The upstream runner core's files, wherever the install put them (the
// package exports resolve to js/viewer/ inside the installed tree).
const CT_JS_DIR = dirname(
  fileURLToPath(import.meta.resolve("@polymorph/component-test-js/harness")),
);

const IMPORT_MAP = JSON.stringify({
  imports: {
    "@polymorph/component-test-js/harness": "/ct/harness.mjs",
    "@polymorph/component-test-js/context": "/ct/context.js",
  },
});

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

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
};

/** Serve the transpiled suite, the harness modules (local glue + the
 * upstream runner core under /ct/, resolved by the page's import map),
 * the host module, and the preview2-shim browser build — strict
 * allowlist, no dot segments. */
function startServer(wasmNames) {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);
    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end(
        "<!doctype html><meta charset=utf-8><title>conformance-ct jco browser leg</title>" +
          `<script type="importmap">${IMPORT_MAP}</script><body>`,
      );
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }
    if (pathname === "/generated-manifest") {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(wasmNames));
      return;
    }
    const match =
      /^\/(generated|shim|ct)\/([A-Za-z0-9._-]+)$|^\/(websocket\.js|harness\.mjs)$/.exec(
        pathname,
      );
    if (!match || pathname.includes("..")) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    const file = match[3]
      ? match[3] === "websocket.js"
        ? join(REPO_ROOT, "js", "jco", "websocket.js")
        : join(JCO_DIR, match[3])
      : match[1] === "shim"
        ? join(SHIM_BROWSER_DIR, match[2])
        : match[1] === "ct"
          ? join(CT_JS_DIR, match[2])
          : join(values.generated, match[2]);
    try {
      const body = await readFile(file);
      res.setHeader("content-type", MIME[extname(file)] ?? "application/octet-stream");
      res.end(body);
    } catch {
      res.statusCode = 404;
      res.end("not found");
    }
  });
  return new Promise((res) => server.listen(0, "127.0.0.1", () => res(server)));
}

/** The corpus run performed inside the page (via page.evaluate). */
async function runInPage({ base, env, target }) {
  const [{ bindImports, runSuite }, connections, { instantiate }, cli, clocks, io] =
    await Promise.all([
      import(`${base}/harness.mjs`),
      import(`${base}/websocket.js`),
      import(`${base}/generated/conformance-guest-ct.js`),
      import(`${base}/shim/cli.js`),
      import(`${base}/shim/clocks.js`),
      import(`${base}/shim/io.js`),
    ]);

  // The suite bounds, matching the other legs; connections capture them
  // at connect.
  connections.setMaxInboundBufferBytes(256 * 1024);
  connections.setConnectTimeoutMs(5000);
  connections.setCloseTimeoutMs(3000);

  const listing = await (await fetch(`${base}/generated-manifest`)).json();
  const modules = new Map();
  const coreBytes = [];
  for (const name of listing) {
    const bytes = new Uint8Array(
      await (await fetch(`${base}/generated/${name}`)).arrayBuffer(),
    );
    coreBytes.push(bytes);
    modules.set(name, await WebAssembly.compile(bytes));
  }

  const imports = bindImports({ connections, env, cli, clocks, io });
  const newInstance = () => instantiate((name) => modules.get(name), imports);

  const lines = [];
  const summary = await runSuite({
    newInstance,
    coreBytes,
    target,
    suiteName: "conformance_guest_ct",
    emit: (line) => lines.push(line),
    log: (msg) => console.log(msg.trimEnd()),
  });
  return { lines, summary };
}

async function main() {
  try {
    await access(join(values.generated, "conformance-guest-ct.js"));
  } catch {
    throw new Error(`missing transpiled suite in ${values.generated}; run "npm run transpile" first`);
  }

  const executablePath = await findChrome();
  if (!executablePath) {
    throw new Error("no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable");
  }

  const owned = values.server ? null : await spawnEchod(values["echod-bin"]);
  const serverUrl = values.server ?? owned.base;
  const tlsServerUrl = values["tls-server"] ?? owned?.tlsBase;
  if (!tlsServerUrl) {
    throw new Error("--server requires --tls-server (the suite echo server's wss: base URL)");
  }
  const wasmNames = (await readdir(values.generated)).filter((n) => n.endsWith(".wasm"));
  const server = await startServer(wasmNames);
  const base = `http://127.0.0.1:${server.address().port}`;
  process.stderr.write(`echo server at ${serverUrl}; page served from ${base}\n`);

  const browser = await chromium.launch({
    executablePath,
    headless: true,
    // --ignore-certificate-errors provisions trust for the committed
    // test PKI (loopback-only browser instance).
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--ignore-certificate-errors"],
  });

  let outcome;
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.on("console", (msg) => process.stderr.write(`[browser] ${msg.text()}\n`));
    page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
    await page.goto(`${base}/`);
    outcome = await page.evaluate(runInPage, {
      base,
      target: values.target,
      env: [
        ["WS_CONFORMANCE_SERVER_URL", serverUrl],
        ["WS_CONFORMANCE_TLS_SERVER_URL", tlsServerUrl],
        ["WS_CONFORMANCE_UNREACHABLE_URL", await unreachableUrl()],
        ["WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES", String(256 * 1024)],
      ],
    });
  } finally {
    await browser.close();
    server.close();
    if (owned) await owned.shutdown();
  }

  await mkdir(values.out, { recursive: true });
  const outPath = join(values.out, `${values.target}.jsonl`);
  await writeFile(outPath, `${outcome.lines.join("\n")}\n`);
  process.stderr.write(
    `wrote ${outPath} (${outcome.summary.total} cases, ${outcome.summary.failed} failed)\n`,
  );
  process.exit(outcome.summary.failed === 0 && outcome.summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
