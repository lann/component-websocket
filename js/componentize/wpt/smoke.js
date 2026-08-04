// A pipeline smoke entry: proves the componentize-js toolchain, the shim,
// and the jco round-trip host stack end to end before (and beside) the
// full WPT runner. Componentized against the same `wpt-parity-runner`
// world; the round-trip leg's transpile serves the same imports.
import { report } from "wpt:parity/reporter@0.1.0";
import { WebSocket } from "./js/componentize/websocket.js";

export const wptParityRunner010 = {
  /** @param {string} serverUrl */
  run: async function (serverUrl) {
    const stage = (name) => report(JSON.stringify({ group: "smoke", name, status: "INFO" }));
    stage("constructing");
    const ws = new WebSocket(`${serverUrl}/echo`);
    stage("constructed");
    const received = [];
    let count = 0;
    const outcome = await new Promise((resolve) => {
      ws.onopen = () => {
        stage("open");
        ws.binaryType = "arraybuffer";
        ws.send(Uint8Array.of(1, 2, 3));
        ws.send("hello");
        stage("sent");
      };
      ws.onmessage = (event) => {
        stage(`message ${received.length}`);
        received.push(event.data);
        if (received.length === 2) {
          ws.close(1000, "smoke done");
        }
      };
      ws.onerror = () => resolve("error event");
      ws.onclose = (event) =>
        resolve(
          `closed code=${event.code} reason=${JSON.stringify(event.reason)} clean=${event.wasClean}`,
        );
    });
    const textOk = received[1] === "hello";
    const binary = received[0];
    const binaryOk =
      binary instanceof ArrayBuffer && new Uint8Array(binary).join(",") === "1,2,3";
    for (const line of [
      `text ${textOk ? "PASS" : "FAIL"}`,
      `binary ${binaryOk ? "PASS" : "FAIL"}`,
      `close ${outcome}`,
    ]) {
      count += 1;
      report(JSON.stringify({ group: "smoke", name: line, status: "INFO" }));
    }
    if (!textOk || !binaryOk || !outcome.startsWith("closed code=1000")) {
      // componentize-js lowers a result-returning export from the plain ok
      // value or a thrown error.
      throw new Error(`smoke failed: ${outcome}; received=${received.length}`);
    }
    return `WPT-PARITY-STREAMED ${count}`;
  },
};
