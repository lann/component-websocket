// The round-trip legs' import module for `lann:websocket/connections`:
// `js/jco/websocket.js` with every async operation deferred by one
// macrotask after it settles.
//
// The deferral works around an incompatibility between componentize-js's
// async lowering and jco's JSPI import wrapper: an import whose promise
// settles without ever yielding to the macrotask queue (a receive served
// from the host's already-filled buffer, a send acked from memory) leaves
// the suspended guest unresumed — the second such call hangs forever.
// One `setTimeout(0)` hop after settlement restores the wake. The quirk
// is isolated here so the production host module stays undeferred; see
// the repository issue tracking the upstream report.
import { Websocket as Inner } from "../../../../js/jco/websocket.js";

const defer = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Await `promise`, then yield one macrotask before settling either way. */
async function deferred(promise) {
  try {
    const value = await promise;
    await defer();
    return value;
  } catch (err) {
    await defer();
    throw err;
  }
}

class Websocket {
  #inner;
  /** @param {InstanceType<typeof Inner>} inner */
  constructor(inner) {
    this.#inner = inner;
  }
  static async connect(url, protocols) {
    return new Websocket(await deferred(Inner.connect(url, protocols)));
  }
  protocol() {
    return this.#inner.protocol();
  }
  state() {
    return this.#inner.state();
  }
  send(message) {
    return deferred(this.#inner.send(message));
  }
  receive() {
    return deferred(this.#inner.receive());
  }
  sendViaStream(messages) {
    return deferred(this.#inner.sendViaStream(messages));
  }
  receiveViaStream() {
    return this.#inner.receiveViaStream();
  }
  waitClosed() {
    return deferred(this.#inner.waitClosed());
  }
  close(code, reason) {
    return this.#inner.close(code, reason);
  }
  [Symbol.dispose]() {
    this.#inner[Symbol.dispose]?.();
  }
}

export const connections = { Websocket };
