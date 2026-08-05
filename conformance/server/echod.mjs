// Node-only helpers for everything that drives the suite echo server
// (the conformance jco legs, the WPT parity legs, the jco demo):
// spawning the `conformance-echod` binary and deriving the unreachable
// URL. Colocated with the server it spawns.
import { spawn } from "node:child_process";
import net from "node:net";

/**
 * Start `conformance-echod`, returning its `ws:` and `wss:` base URLs and
 * a shutdown handle. Rejects if the binary is missing (spawn error) or
 * exits before reporting its URLs.
 */
export async function spawnEchod(bin) {
  const child = spawn(bin, [], { stdio: ["ignore", "pipe", "inherit"] });
  const { base, tlsBase } = await new Promise((resolveUrl, rejectUrl) => {
    let buffer = "";
    const onData = (chunk) => {
      buffer += chunk;
      const match = /LISTENING (ws:\/\/\S+) (wss:\/\/\S+)/.exec(buffer);
      if (match) {
        child.stdout.off("data", onData);
        resolveUrl({ base: match[1].trim(), tlsBase: match[2].trim() });
      }
    };
    child.stdout.on("data", onData);
    child.on("error", (err) =>
      rejectUrl(
        new Error(
          `could not start the echo server at ${bin} (build it with \`just conformance-ct::build-echod\`): ${err.message}`,
        ),
      ),
    );
    child.on("exit", (code) =>
      rejectUrl(new Error(`echo server exited before reporting a URL (exit code ${code})`)),
    );
    setTimeout(() => rejectUrl(new Error("echo server did not report a URL in time")), 10_000);
  });
  return {
    base,
    tlsBase,
    async shutdown() {
      child.kill("SIGTERM");
    },
  };
}

/**
 * A loopback `ws:` URL whose connect attempt should be refused: a port
 * that was just bound and released. The window between release and use is
 * small but real; a collision surfaces as a `connect-refused` failure, not
 * a silent pass.
 */
export function unreachableUrl() {
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
 * Fail fast, with the reason, on a Node too old for the jco async ABI
 * (JSPI needs Node 24+; the npm scripts supply the flag).
 */
export function requireNode24() {
  const major = Number(process.versions.node.split(".")[0]);
  if (major < 24) {
    console.error(
      `Node ${process.versions.node} is too old: the jco async ABI needs JSPI ` +
        "(Node 24+ with --experimental-wasm-jspi). See the README prerequisites.",
    );
    process.exit(1);
  }
}
