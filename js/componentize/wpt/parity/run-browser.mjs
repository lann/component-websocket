// The browser parity legs: the same two legs as run-legs.mjs — the shared
// bodies in legs.mjs — run inside a real, headless Chromium, so the
// baseline measures Chromium's own `WebSocket` and the round trip runs
// the browser-profile transpile (`pnpm run transpile:web`) against
// websocket-jco in the environment it actually targets. The comparator is
// the same compare.mjs; the ratchet is per-engine (losses-chromium.js),
// because a loss set is a fact about one engine's baseline.
//
// The static server mirrors the repository layout under an allowlist, so
// the served modules' relative imports (legs.mjs -> ../harness.js, the
// generated tree -> js/jco/websocket.js, the wasi maps -> the
// preview2-shim browser build) resolve with no bundling and the same URL
// identity legs.mjs relies on. The page opens `ws:` connections to the
// echo server directly: WebSocket is not subject to CORS, and a localhost
// `http:` page may open `ws:` connections.
//
// jco's async ABI needs JSPI; Chrome ships it enabled from 137 onward.
//
// Usage: node run-browser.mjs [--update]

import http from "node:http";
import { spawn } from "node:child_process";
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

import { findChrome } from "../../../../scripts/chrome.mjs";
import { spawnEchod } from "../../../../conformance/server/echod.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..", "..");
const update = process.argv.includes("--update");

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
  ".json": "application/json",
};

// The subtrees a page may read: the WPT gate's own tree (harness, groups,
// reporter, parity modules, the generated-web transpile and its
// node_modules maps) and the jco host module the round trip terminates
// in.
const SERVED_PREFIXES = ["/js/componentize/wpt/", "/js/jco/"];

/** Serve the repository layout, read-only, under SERVED_PREFIXES. */
function startServer() {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);
    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end("<!doctype html><meta charset=utf-8><title>wpt parity browser legs</title><body>");
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }
    const file = join(REPO_ROOT, normalize(pathname));
    const allowed =
      SERVED_PREFIXES.some((prefix) => pathname.startsWith(prefix)) &&
      !pathname.includes("..") &&
      file.startsWith(REPO_ROOT + "/");
    if (!allowed) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
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

/**
 * One leg, inside the browser page: import the shared leg bodies over
 * HTTP and run one of them. Serialized via `page.evaluate`.
 */
async function runLegInPage({ base, wsBase, leg }) {
  const legs = await import(`${base}/js/componentize/wpt/parity/legs.mjs`);
  return leg === "baseline" ? legs.runBaseline(wsBase) : legs.runRoundtrip(wsBase, "generated-web");
}

async function main() {
  try {
    await access(join(HERE, "generated-web", "parity-runner.js"));
  } catch {
    throw new Error(`missing browser-profile transpile in ${join(HERE, "generated-web")}; run "npm run transpile:web" first`);
  }

  const executablePath = await findChrome();
  if (!executablePath) {
    throw new Error("no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable");
  }

  const echod = await spawnEchod(join(REPO_ROOT, "target", "debug", "conformance-echod"));
  const server = await startServer();
  const base = `http://127.0.0.1:${server.address().port}`;
  process.stderr.write(`echo server at ${echod.base}; page served from ${base}\n`);

  const browser = await chromium.launch({
    executablePath,
    headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const records = {};
  try {
    // A fresh page per leg: the baseline installs the harness and its
    // globals, and the legs must not share that state.
    const context = await browser.newContext();
    for (const leg of ["baseline", "roundtrip"]) {
      const page = await context.newPage();
      page.on("console", (msg) => process.stderr.write(`[browser] ${msg.text()}\n`));
      page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
      await page.goto(`${base}/`);
      records[leg] = await page.evaluate(runLegInPage, { base, wsBase: echod.base, leg });
      await page.close();
    }
  } finally {
    await browser.close();
    server.close();
    await echod.shutdown();
  }

  await mkdir(join(HERE, "build"), { recursive: true });
  await writeFile(join(HERE, "build", "parity-baseline-chromium.json"), JSON.stringify(records.baseline));
  await writeFile(join(HERE, "build", "parity-roundtrip-chromium.json"), JSON.stringify(records.roundtrip));

  const compareArgs = [
    "compare.mjs",
    "build/parity-baseline-chromium.json",
    "build/parity-roundtrip-chromium.json",
    "--losses",
    "losses-chromium.js",
  ];
  if (update) compareArgs.push("--update");
  await new Promise((resolvePromise, reject) => {
    const child = spawn(process.execPath, compareArgs, { cwd: HERE, stdio: "inherit" });
    child.on("exit", (code) => (code === 0 ? resolvePromise() : reject(new Error(`compare exited ${code}`))));
    child.on("error", reject);
  });
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(1);
  },
);
