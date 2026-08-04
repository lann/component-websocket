// The jco browser conformance adapter: runs the same shared conformance
// guest and the same host module (`js/jco/websocket.js`) inside a real,
// headless Chromium — the environment the "browser-first" host actually
// targets — and emits the adapter result document the conformance runner
// consumes (`conformance/results/jco-browser.json`). It is the browser
// counterpart of the Node adapter and shares the corpus orchestration in
// `driver.js`.
//
// jco's async ABI needs JavaScript Promise Integration (JSPI); Chrome ships
// it enabled from 137 onward, so a recent Chrome works with no flags. The
// page is served from `http://127.0.0.1:<port>` and opens `ws:` connections
// to the echo server directly: WebSocket is not subject to CORS, and a
// localhost `http:` page may open `ws:` connections, so no proxying is
// needed.
import { spawn } from "node:child_process";
import http from "node:http";
import net from "node:net";
import { availableParallelism } from "node:os";
import { access, mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { chromium } from "playwright-core";

const ADAPTER_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(ADAPTER_DIR, "..", "..", "..");

// The whole corpus runs inside one page, so every test shares the page's
// main thread. Wall-clock bounds (the 5s connect bound in particular) run
// against that thread: too many concurrent instantiations starve the event
// loop and time out connects spuriously, so the multiplier stays low and
// hard-capped.
function defaultJobs() {
  return Math.min(4, Math.max(2, availableParallelism()));
}

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(ADAPTER_DIR, "generated") },
    out: { type: "string", default: join(REPO_ROOT, "conformance", "results") },
    target: { type: "string", default: "jco-browser" },
    environment: { type: "string", default: "loopback" },
    server: { type: "string" },
    "echod-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-echod"),
    },
    only: { type: "string", multiple: true, default: [] },
    jobs: { type: "string" },
  },
});

// Candidate locations for a Chrome/Chromium binary (137+ for JSPI). CI can
// override with CHROME_PATH; a playwright-installed Chromium is also
// discovered.
async function findChrome() {
  const explicit = [
    process.env.CHROME_PATH,
    process.env.CHROME_BIN,
    process.env.PUPPETEER_EXECUTABLE_PATH,
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ];
  for (const p of explicit) {
    if (!p) continue;
    try {
      await access(p);
      return p;
    } catch {
      // keep looking
    }
  }
  // A playwright-managed Chromium (newest revision wins).
  try {
    const cache = join(
      process.env.PLAYWRIGHT_BROWSERS_PATH ?? join(process.env.HOME ?? "", ".cache", "ms-playwright"),
    );
    const revisions = (await readdir(cache))
      .filter((name) => /^chromium-\d+$/.test(name))
      .sort((a, b) => Number(b.split("-")[1]) - Number(a.split("-")[1]));
    for (const revision of revisions) {
      const candidate = join(cache, revision, "chrome-linux", "chrome");
      try {
        await access(candidate);
        return candidate;
      } catch {
        // keep looking
      }
    }
  } catch {
    // no cache
  }
  return undefined;
}

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
};

/** Serve the transpiled guest, the driver, and the host module. */
function startServer(wasmNames) {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);
    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end(
        "<!doctype html><meta charset=utf-8><title>conformance jco browser adapter</title><body>",
      );
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }
    // The page fetches the module list rather than hardcoding core module
    // names (their count depends on the transpilation).
    if (pathname === "/generated-manifest") {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(wasmNames));
      return;
    }
    // Strict allowlist: the transpiled bundle under /generated/ and the two
    // modules. Each path is a single, dot-segment-free file name, which
    // scopes the server and rules out path traversal.
    const match = /^\/(generated)\/([A-Za-z0-9._-]+)$|^\/(websocket\.js|driver\.js)$/.exec(
      pathname,
    );
    if (!match || pathname.includes("..")) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    const file =
      match[3] === "websocket.js"
        ? join(REPO_ROOT, "js", "jco", "websocket.js")
        : match[3]
          ? join(ADAPTER_DIR, match[3])
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

/** Start `conformance-echod`, returning its `ws:` base URL and a shutdown handle. */
async function spawnEchod(bin) {
  const child = spawn(bin, [], { stdio: ["ignore", "pipe", "inherit"] });
  const base = await new Promise((resolveUrl, rejectUrl) => {
    let buffer = "";
    const onData = (chunk) => {
      buffer += chunk;
      const match = /LISTENING (ws:\/\/\S+)/.exec(buffer);
      if (match) {
        child.stdout.off("data", onData);
        resolveUrl(match[1].trim());
      }
    };
    child.stdout.on("data", onData);
    child.on("exit", (code) =>
      rejectUrl(new Error(`echo server exited before reporting a URL (exit code ${code})`)),
    );
    setTimeout(() => rejectUrl(new Error("echo server did not report a URL in time")), 10_000);
  });
  return {
    base,
    async shutdown() {
      child.kill("SIGTERM");
    },
  };
}

/** A loopback `ws:` URL whose connect attempt should be refused. */
function unreachableUrl() {
  return new Promise((resolvePort, rejectPort) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolvePort(`ws://127.0.0.1:${port}/echo`));
    });
    server.on("error", rejectPort);
  });
}

/**
 * The corpus run performed inside the browser page. Serialized and
 * evaluated via `page.evaluate`; `base` is this adapter's static server.
 */
async function runInPage({ base, serverUrl, unreachableUrl, only, jobs }) {
  const [
    { runCorpus, MAX_INBOUND_BUFFER_BYTES, CONNECT_TIMEOUT_MS, CLOSE_TIMEOUT_MS },
    connections,
    { instantiate },
  ] = await Promise.all([
    import(`${base}/driver.js`),
    import(`${base}/websocket.js`),
    import(`${base}/generated/conformance-guest.js`),
  ]);

  // Apply the conformance bounds through the module's exported hooks (see
  // driver.js); connections capture them at `connect`.
  connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);
  connections.setConnectTimeoutMs(CONNECT_TIMEOUT_MS);
  connections.setCloseTimeoutMs(CLOSE_TIMEOUT_MS);

  const listing = await (await fetch(`${base}/generated-manifest`)).json();
  const modules = new Map();
  for (const name of listing) {
    modules.set(name, await WebAssembly.compileStreaming(fetch(`${base}/generated/${name}`)));
  }

  const newInstance = () =>
    instantiate((name) => modules.get(name), {
      "lann:websocket/connections": connections,
    });

  return runCorpus({
    newInstance,
    serverUrl,
    unreachableUrl,
    only,
    jobs,
    log: (msg) => console.log(msg.trimEnd()),
  });
}

async function main() {
  try {
    await access(join(values.generated, "conformance-guest.js"));
  } catch {
    throw new Error(
      `missing transpiled guest in ${values.generated}; run "npm run transpile" first`,
    );
  }

  const executablePath = await findChrome();
  if (!executablePath) {
    throw new Error("no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable");
  }

  const owned = values.server ? null : await spawnEchod(values["echod-bin"]);
  const serverUrl = values.server ?? owned.base;
  const wasmNames = (await readdir(values.generated)).filter((n) => n.endsWith(".wasm"));
  const server = await startServer(wasmNames);
  const base = `http://127.0.0.1:${server.address().port}`;
  process.stderr.write(`echo server at ${serverUrl}; page served from ${base}\n`);

  const browser = await chromium.launch({
    executablePath,
    headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  let results;
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.on("console", (msg) => process.stderr.write(`[browser] ${msg.text()}\n`));
    page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
    await page.goto(`${base}/`);
    results = await page.evaluate(runInPage, {
      base,
      serverUrl,
      unreachableUrl: await unreachableUrl(),
      only: values.only,
      jobs: values.jobs ? Number(values.jobs) : defaultJobs(),
    });
  } finally {
    await browser.close();
    server.close();
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
  process.stderr.write(`wrote ${outPath} (${results.length} tests, ${failed} failed)\n`);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(1);
  },
);
