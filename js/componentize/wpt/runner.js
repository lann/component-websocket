// The WPT parity runner guest: the measuring half of the parity gate. It
// runs the vendored WPT WebSocket groups against the
// `js/componentize/websocket.js` shim, asserting nothing in-guest: every
// result is reported, and the judgment — which losses relative to the
// platform baseline are known, which are new — is the host-side
// comparator's.
//
// Records stream out through the world's `wpt:parity/reporter` import as
// each test settles — one `report` call per record, the JSON encoding of
// `{ group, name, status, message? }` — so a live embedder can show
// progress mid-run and a batch one (the Node round-trip leg) collects.
// `run` resolves after the last record, to `WPT-PARITY-STREAMED <count>`
// for the embedder to cross-check against what it received.
//
// Module specifiers resolve against componentize-js's base directory, the
// repository root.

import { report } from "wpt:parity/reporter@0.1.0";
import { Blob, CloseEvent, MessageEvent, WebSocket } from "./js/componentize/websocket.js";
import { GROUPS } from "./js/componentize/wpt/build/groups.js";
import { drain, setOnResult, takeResults } from "./js/componentize/wpt/harness.js";

// The vendored suites reach the shim through the standard globals.
globalThis.WebSocket = WebSocket;
if (globalThis.CloseEvent === undefined) {
  globalThis.CloseEvent = CloseEvent;
}
if (globalThis.MessageEvent === undefined) {
  globalThis.MessageEvent = MessageEvent;
}
if (globalThis.Blob === undefined) {
  globalThis.Blob = Blob;
}

export const wptParityRunner010 = {
  /** @param {string} serverUrl */
  run: async function (serverUrl) {
    globalThis.__WPT_SERVER_BASE = serverUrl;
    let currentGroup = "";
    let count = 0;
    setOnResult(({ name, status, message }) => {
      count += 1;
      report(
        JSON.stringify(
          message === undefined
            ? { group: currentGroup, name, status }
            : { group: currentGroup, name, status, message },
        ),
      );
    });
    for (const { name, start } of GROUPS) {
      currentGroup = name;
      start();
      await drain();
      takeResults();
    }
    return `WPT-PARITY-STREAMED ${count}`;
  },
};
