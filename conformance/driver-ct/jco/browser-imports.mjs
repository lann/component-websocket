// The browser worker's import-object module: loaded by the upstream
// browser-worker via URL (module workers cannot see import maps), so
// every specifier here is a server path — the harness core through the
// driver's self-mount, the SUT host and the wasi shims through the
// repository-root server. Binds the same surface as harness.mjs's
// bindImports, which cannot load here: its bare specifiers need the
// page's import map.
import { bindImports } from "/__component-test/js/viewer/imports.mjs";
import * as connections from "/js/jco/websocket.js";
import * as cli from "./node_modules/@bytecodealliance/preview2-shim/lib/browser/cli.js";
import * as clocks from "./node_modules/@bytecodealliance/preview2-shim/lib/browser/clocks.js";
import * as io from "./node_modules/@bytecodealliance/preview2-shim/lib/browser/io.js";

/** The suite bounds, matching the other legs; connections capture them
 *  at connect, so configuring the module once covers every case. The
 *  buffer bound rides the env so the driver configures it once. */
export async function suiteImports(env) {
  const bytes = env.find(([name]) => name === "WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES")?.[1];
  connections.setMaxInboundBufferBytes(Number(bytes ?? 256 * 1024));
  connections.setConnectTimeoutMs(5000);
  connections.setCloseTimeoutMs(3000);
  return bindImports({
    wasi: { cli, clocks, io },
    env,
    sut: { "polymorph:websocket/connections": connections },
  });
}
