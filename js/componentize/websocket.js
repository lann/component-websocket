// @ts-check
// A WHATWG `WebSocket` shim for JS guests componentized with
// componentize-js (https://github.com/lann/componentize-js, the wit-dylib
// reboot of ComponentizeJS), backed by the `polymorph:websocket` imports.
//
// This is the inverse of `js/jco/websocket.js`: that module implements the
// WIT *imports* over the standard browser API, while this one implements
// the standard browser API *over* the WIT imports, so JS application code
// written against `WebSocket` can be componentized and run against any
// host that serves `polymorph:websocket/connections` — including, round-trip,
// the jco host itself (the WPT parity gate in `wpt/` measures exactly what
// that carrier stack loses).
//
// The surface mirrors the WHATWG WebSocket interface: the constructor
// (URL and subprotocol validation with the spec's SyntaxError semantics),
// `send` for strings, ArrayBuffers, views, and Blobs (where the runtime
// provides Blob), `close` with the spec's InvalidAccessError/SyntaxError
// validation, `readyState`/`bufferedAmount`/`url`/`protocol`/`extensions`/
// `binaryType`, `open`/`message`/`error`/`close` events through
// EventTarget semantics, and `onopen`/`onmessage`/`onerror`/`onclose`
// handler properties.
//
// The component's world must import `polymorph:websocket/connections@0.1.0`
// (its `types` dependency is pulled in by WIT elaboration). Module
// specifiers here name the import directly, so this file needs no
// bundler: componentize-js resolves them against the world at
// componentize time.
//
// Documented deviations from the WHATWG WebSocket interface (all fail
// closed or are observable, never silently differ). Each is classified —
// *unserved* (the WIT carries the semantics; this shim does not serve
// them yet), *WIT-forced* (no shim could express the behavior through the
// interface shape; a recorded design ruling), or *environment* (the
// componentize-js runtime lacks a platform capability the spec assumes):
//
//   - `extensions` is always the empty string (WIT-forced: the package
//     rules extensions off the surface permanently — none can be
//     offered, configured, or observed; see the package README's
//     "Portability contract" latitude on transparent negotiation).
//   - Where the runtime provides no `Blob`, the shim supplies a minimal
//     one (constructor over string/buffer/view/Blob parts, `size`,
//     `type`, `arrayBuffer`, `bytes`, `text`, `slice`) so `binaryType`
//     defaults to "blob" per spec; it is exported for the embedder to
//     install as a global (environment).
//   - `close(undefined, reason)` sends close code 1000 alongside the
//     reason (the WIT close contract requires a code to carry a reason;
//     browsers drop the reason from the wire in this case) (WIT-forced).
//   - A `close()` during CONNECTING fails the connection once the
//     handshake resolves; the browser can abort the handshake in flight
//     (WIT-forced: `connect` resolves only at open).
//   - `url` reflects the constructor's input verbatim (no normalization;
//     the WIT deliberately has no URL accessor, and normalizing here
//     would only imitate one platform's parser) (WIT-forced).

import * as connections from "polymorph:websocket/connections@0.1.0";

// ----- platform pieces the componentize-js runtime may lack -----------------

/** The runtime's DOMException, or a spec-shaped stand-in. */
const DOMExceptionImpl =
  globalThis.DOMException ??
  class DOMException extends Error {
    /** @param {string} [message] @param {string} [name] */
    constructor(message = "", name = "Error") {
      super(message);
      this.name = name;
    }
  };

/** The runtime's Event, or a minimal stand-in with the fields WPT reads. */
const EventImpl =
  globalThis.Event ??
  class Event {
    /** @param {string} type @param {{ [k: string]: unknown }} [init] */
    constructor(type, init = {}) {
      this.type = type;
      this.bubbles = Boolean(init.bubbles);
      this.cancelable = Boolean(init.cancelable);
      /** @type {unknown} */ this.target = null;
      /** @type {unknown} */ this.currentTarget = null;
      this.defaultPrevented = false;
      this.isTrusted = false;
      this.timeStamp = 0;
    }
    preventDefault() {
      if (this.cancelable) this.defaultPrevented = true;
    }
    stopPropagation() {}
    stopImmediatePropagation() {}
  };

/** The runtime's EventTarget, or a minimal stand-in. */
const EventTargetImpl =
  globalThis.EventTarget ??
  class EventTarget {
    /** @type {Map<string, Set<Function>>} */
    #listeners = new Map();
    /** @param {string} type @param {Function | null} listener */
    addEventListener(type, listener) {
      if (typeof listener !== "function") return;
      let set = this.#listeners.get(type);
      if (!set) {
        set = new Set();
        this.#listeners.set(type, set);
      }
      set.add(listener);
    }
    /** @param {string} type @param {Function | null} listener */
    removeEventListener(type, listener) {
      this.#listeners.get(type)?.delete(/** @type {Function} */ (listener));
    }
    /** @param {{ type: string, target: unknown, currentTarget: unknown }} event */
    dispatchEvent(event) {
      try {
        event.target = this;
        event.currentTarget = this;
      } catch {
        // A native Event's fields are read-only; targets stay null.
      }
      for (const listener of [...(this.#listeners.get(event.type) ?? [])]) {
        try {
          listener.call(this, event);
        } catch {
          // Listener exceptions do not break dispatch, per spec.
        }
      }
      return true;
    }
  };

/** `MessageEvent`, from the runtime or spec-shaped over EventImpl. */
const MessageEventImpl =
  globalThis.MessageEvent ??
  class MessageEvent extends EventImpl {
    /** @param {string} type @param {{ data?: unknown, origin?: string }} [init] */
    constructor(type, init = {}) {
      super(type, init);
      this.data = init.data ?? null;
      this.origin = init.origin ?? "";
      this.lastEventId = "";
      this.source = null;
      this.ports = [];
    }
  };

/** `CloseEvent`, from the runtime or spec-shaped over EventImpl. */
const CloseEventImpl =
  globalThis.CloseEvent ??
  class CloseEvent extends EventImpl {
    /** @param {string} type @param {{ wasClean?: boolean, code?: number, reason?: string }} [init] */
    constructor(type, init = {}) {
      super(type, init);
      this.wasClean = Boolean(init.wasClean);
      this.code = init.code ?? 0;
      this.reason = init.reason ?? "";
    }
  };

const utf8Encoder = new TextEncoder();
const utf8Decoder = new TextDecoder();

/** The runtime's Blob, or a minimal stand-in over owned bytes. */
const BlobImpl =
  globalThis.Blob ??
  class Blob {
    /** @type {Uint8Array} */
    #bytes;
    #type;
    /** @param {unknown[]} [parts] @param {{ type?: string }} [options] */
    constructor(parts = [], options = {}) {
      const chunks = [];
      let total = 0;
      for (const part of parts) {
        let chunk;
        if (typeof part === "string") {
          chunk = utf8Encoder.encode(part);
        } else if (part instanceof ArrayBuffer) {
          chunk = new Uint8Array(part.slice(0));
        } else if (ArrayBuffer.isView(part)) {
          chunk = new Uint8Array(
            part.buffer.slice(part.byteOffset, part.byteOffset + part.byteLength),
          );
        } else if (part instanceof Blob) {
          chunk = part.#bytes;
        } else {
          chunk = utf8Encoder.encode(String(part));
        }
        chunks.push(chunk);
        total += chunk.byteLength;
      }
      const bytes = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.byteLength;
      }
      this.#bytes = bytes;
      this.#type = String(options.type ?? "").toLowerCase();
    }
    get size() {
      return this.#bytes.byteLength;
    }
    get type() {
      return this.#type;
    }
    async arrayBuffer() {
      return this.#bytes.buffer.slice(
        this.#bytes.byteOffset,
        this.#bytes.byteOffset + this.#bytes.byteLength,
      );
    }
    async bytes() {
      return new Uint8Array(await this.arrayBuffer());
    }
    async text() {
      return utf8Decoder.decode(this.#bytes);
    }
    /** @param {number} [start] @param {number} [end] @param {string} [type] */
    slice(start, end, type) {
      return new Blob([this.#bytes.subarray(start ?? 0, end)], { type: type ?? "" });
    }
  };

/** Whether `value` is a Blob of either provenance. */
function isBlob(value) {
  return (
    value instanceof BlobImpl ||
    (typeof globalThis.Blob === "function" && value instanceof globalThis.Blob)
  );
}

// ----- WHATWG argument validation -------------------------------------------

/**
 * Parse and validate a WebSocket URL per the WHATWG constructor steps this
 * shim can honor: absolute `ws:`/`wss:`, no fragment, no userinfo. Throws
 * the spec's SyntaxError DOMException.
 * @param {string} url
 */
function validateUrl(url) {
  const fail = (/** @type {string} */ message) => {
    throw new DOMExceptionImpl(`WebSocket constructor: ${message}`, "SyntaxError");
  };
  if (url.includes("#")) {
    fail(`the URL ${JSON.stringify(url)} must not have a fragment`);
  }
  const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):\/\//.exec(url);
  if (!scheme) {
    fail(`the URL ${JSON.stringify(url)} is not an absolute URL`);
  }
  const lower = /** @type {RegExpExecArray} */ (scheme)[1].toLowerCase();
  if (lower !== "ws" && lower !== "wss") {
    fail(`the URL scheme ${JSON.stringify(lower)} must be ws or wss`);
  }
  const rest = url.slice(/** @type {RegExpExecArray} */ (scheme)[0].length);
  const authority = rest.split(/[/?]/, 1)[0];
  if (authority.length === 0) {
    fail(`the URL ${JSON.stringify(url)} has no host`);
  }
  if (authority.includes("@")) {
    fail(`the URL ${JSON.stringify(url)} must not have userinfo`);
  }
  if (/[\s]/.test(authority)) {
    fail(`the URL host ${JSON.stringify(authority)} is invalid`);
  }
}

/** @param {string} token */
function isValidProtocolToken(token) {
  if (!token.length) return false;
  for (let i = 0; i < token.length; i += 1) {
    const c = token.charCodeAt(i);
    if (c <= 0x20 || c >= 0x7f) return false;
    if ('"(),/:;<=>?@[\\]{}'.includes(token[i])) return false;
  }
  return true;
}

/**
 * Normalize and validate the subprotocol argument per the WHATWG steps.
 * Throws the spec's SyntaxError DOMException.
 * @param {string | string[] | undefined} protocols
 * @returns {string[]}
 */
function validateProtocols(protocols) {
  const list =
    protocols === undefined ? [] : typeof protocols === "string" ? [protocols] : [...protocols].map(String);
  const seen = new Set();
  for (const protocol of list) {
    if (!isValidProtocolToken(protocol)) {
      throw new DOMExceptionImpl(
        `WebSocket constructor: invalid subprotocol ${JSON.stringify(protocol)}`,
        "SyntaxError",
      );
    }
    const lower = protocol.toLowerCase();
    if (seen.has(lower)) {
      throw new DOMExceptionImpl(
        `WebSocket constructor: duplicate subprotocol ${JSON.stringify(protocol)}`,
        "SyntaxError",
      );
    }
    seen.add(lower);
  }
  return list;
}

// ----- the shim --------------------------------------------------------------

export class WebSocket extends EventTargetImpl {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  CONNECTING = 0;
  OPEN = 1;
  CLOSING = 2;
  CLOSED = 3;

  /** @type {string} */
  #url;
  #readyState = 0;
  #bufferedAmount = 0;
  #protocol = "";
  #binaryType = "blob";
  /** @type {import("polymorph:websocket/connections@0.1.0").Websocket | null} */
  #ws = null;
  /** FIFO chain serializing WIT sends so message order is preserved. */
  #sendQueue = Promise.resolve();
  /** A close requested while CONNECTING: applied once the handshake settles. */
  /** @type {{ code: number | undefined, reason: string } | null} */
  #pendingClose = null;
  #closeDispatched = false;
  /** @type {Function | null} */
  #onopen = null;
  /** @type {Function | null} */
  #onmessage = null;
  /** @type {Function | null} */
  #onerror = null;
  /** @type {Function | null} */
  #onclose = null;

  /**
   * @param {string} url
   * @param {string | string[]} [protocols]
   */
  constructor(url, protocols) {
    super();
    const urlString = String(url);
    validateUrl(urlString);
    const offer = validateProtocols(protocols);
    this.#url = urlString;

    connections.Websocket.connect(urlString, offer).then(
      (ws) => this.#established(ws),
      () => this.#failed(),
    );
  }

  get url() {
    return this.#url;
  }
  get readyState() {
    return this.#readyState;
  }
  get bufferedAmount() {
    return this.#bufferedAmount;
  }
  get protocol() {
    return this.#protocol;
  }
  get extensions() {
    // The WIT package has no extensions surface (see the deviations
    // registry in the header).
    return "";
  }
  get binaryType() {
    return this.#binaryType;
  }
  set binaryType(value) {
    // Per spec, unrecognized values are ignored.
    if (value === "arraybuffer" || value === "blob") {
      this.#binaryType = value;
    }
  }

  get onopen() {
    return this.#onopen;
  }
  set onopen(handler) {
    if (this.#onopen) this.removeEventListener("open", this.#onopen);
    this.#onopen = typeof handler === "function" ? handler : null;
    if (this.#onopen) this.addEventListener("open", this.#onopen);
  }
  get onmessage() {
    return this.#onmessage;
  }
  set onmessage(handler) {
    if (this.#onmessage) this.removeEventListener("message", this.#onmessage);
    this.#onmessage = typeof handler === "function" ? handler : null;
    if (this.#onmessage) this.addEventListener("message", this.#onmessage);
  }
  get onerror() {
    return this.#onerror;
  }
  set onerror(handler) {
    if (this.#onerror) this.removeEventListener("error", this.#onerror);
    this.#onerror = typeof handler === "function" ? handler : null;
    if (this.#onerror) this.addEventListener("error", this.#onerror);
  }
  get onclose() {
    return this.#onclose;
  }
  set onclose(handler) {
    if (this.#onclose) this.removeEventListener("close", this.#onclose);
    this.#onclose = typeof handler === "function" ? handler : null;
    if (this.#onclose) this.addEventListener("close", this.#onclose);
  }

  /**
   * @param {string | ArrayBuffer | ArrayBufferView | { arrayBuffer(): Promise<ArrayBuffer>, size: number }} data
   */
  send(data) {
    if (this.#readyState === WebSocket.CONNECTING) {
      throw new DOMExceptionImpl("WebSocket send: still in CONNECTING state", "InvalidStateError");
    }
    if (typeof data === "string") {
      this.#sendPayload({ tag: "string", val: data }, utf8Encoder.encode(data).byteLength);
      return;
    }
    if (data instanceof ArrayBuffer) {
      this.#sendPayload({ tag: "binary", val: new Uint8Array(data.slice(0)) }, data.byteLength);
      return;
    }
    if (ArrayBuffer.isView(data)) {
      const bytes = new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
      this.#sendPayload({ tag: "binary", val: bytes }, bytes.byteLength);
      return;
    }
    if (isBlob(data)) {
      // The Blob's bytes are read asynchronously; bufferedAmount is
      // accounted immediately at the Blob's size, per spec.
      const size = data.size;
      this.#bufferedAmount += size;
      if (this.#readyState !== WebSocket.OPEN) return;
      const ws = this.#ws;
      this.#sendQueue = this.#sendQueue
        .then(async () => {
          const bytes = new Uint8Array(await data.arrayBuffer());
          await /** @type {NonNullable<typeof ws>} */ (ws).send({ tag: "binary", val: bytes });
        })
        .then(
          () => {
            this.#bufferedAmount -= size;
          },
          () => {
            this.#bufferedAmount -= size;
          },
        );
      return;
    }
    // Anything else stringifies, per WebIDL's USVString coercion of the
    // string branch.
    const text = String(data);
    this.#sendPayload({ tag: "string", val: text }, utf8Encoder.encode(text).byteLength);
  }

  /**
   * @param {{ tag: "string", val: string } | { tag: "binary", val: Uint8Array }} message
   * @param {number} size
   */
  #sendPayload(message, size) {
    // Per spec, bufferedAmount grows even in CLOSING/CLOSED, where the
    // data is silently discarded.
    this.#bufferedAmount += size;
    if (this.#readyState !== WebSocket.OPEN) return;
    const ws = /** @type {NonNullable<typeof this.__ws>} */ (this.#ws);
    this.#sendQueue = this.#sendQueue
      .then(() => ws.send(message))
      .then(
        () => {
          this.#bufferedAmount -= size;
        },
        () => {
          // A failed send means the connection is closing; the close
          // event carries the outcome. The bytes are no longer buffered.
          this.#bufferedAmount -= size;
        },
      );
  }

  /**
   * @param {number} [code]
   * @param {string} [reason]
   */
  close(code, reason) {
    if (code !== undefined && code !== 1000 && !(code >= 3000 && code <= 4999)) {
      throw new DOMExceptionImpl(
        `WebSocket close: invalid code ${code}`,
        "InvalidAccessError",
      );
    }
    const reasonString = reason === undefined ? "" : String(reason);
    if (utf8Encoder.encode(reasonString).byteLength > 123) {
      throw new DOMExceptionImpl("WebSocket close: reason is longer than 123 bytes", "SyntaxError");
    }
    if (this.#readyState === WebSocket.CLOSING || this.#readyState === WebSocket.CLOSED) {
      return;
    }
    if (this.#readyState === WebSocket.CONNECTING) {
      // The WIT connect resolves only at open, so the close applies the
      // moment the handshake settles (see the deviations registry).
      this.#pendingClose = { code, reason: reasonString };
      this.#readyState = WebSocket.CLOSING;
      return;
    }
    this.#readyState = WebSocket.CLOSING;
    this.#closeUnderlying(code, reasonString);
  }

  /**
   * @param {number | undefined} code
   * @param {string} reason
   */
  #closeUnderlying(code, reason) {
    const ws = /** @type {NonNullable<typeof this.__ws>} */ (this.#ws);
    try {
      if (reason.length > 0 && code === undefined) {
        // The WIT close contract requires a code to carry a reason (see
        // the deviations registry).
        ws.close(1000, reason);
      } else {
        ws.close(code, reason);
      }
    } catch {
      // Arguments were validated above; a residual failure means the
      // connection is already closing, which close() treats as a no-op.
    }
  }

  /** @param {import("polymorph:websocket/connections@0.1.0").Websocket} ws */
  #established(ws) {
    this.#ws = ws;
    if (this.#pendingClose !== null) {
      // close() during CONNECTING: fail the connection now that the
      // handshake settled.
      this.#closeUnderlying(this.#pendingClose.code, this.#pendingClose.reason);
      this.#settle(true);
      return;
    }
    this.#readyState = WebSocket.OPEN;
    this.#protocol = ws.protocol();
    this.dispatchEvent(new EventImpl("open"));
    this.#receiveLoop();
    this.#settle(false);
  }

  #failed() {
    this.#readyState = WebSocket.CLOSED;
    if (this.#closeDispatched) return;
    this.#closeDispatched = true;
    this.dispatchEvent(new EventImpl("error"));
    this.dispatchEvent(
      new CloseEventImpl("close", { wasClean: false, code: 1006, reason: "" }),
    );
  }

  async #receiveLoop() {
    const ws = /** @type {NonNullable<typeof this.__ws>} */ (this.#ws);
    for (;;) {
      let message;
      try {
        message = await ws.receive();
      } catch {
        // The connection is closing; #settle owns the close event.
        return;
      }
      if (message.tag === "string") {
        this.dispatchEvent(new MessageEventImpl("message", { data: message.val, origin: this.#originOf() }));
      } else {
        const bytes = /** @type {Uint8Array} */ (message.val);
        const data =
          this.#binaryType === "blob"
            ? new BlobImpl([bytes])
            : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
        this.dispatchEvent(new MessageEventImpl("message", { data, origin: this.#originOf() }));
      }
    }
  }

  /** The message event's origin: the connection URL's origin, per spec. */
  #originOf() {
    const match = /^(ws{1,2}):\/\/([^/?]+)/i.exec(this.#url);
    if (!match) return "";
    return `${match[1].toLowerCase()}://${match[2]}`;
  }

  /** @param {boolean} failedDuringConnecting */
  async #settle(failedDuringConnecting) {
    const ws = /** @type {NonNullable<typeof this.__ws>} */ (this.#ws);
    const info = await ws.waitClosed();
    this.#readyState = WebSocket.CLOSED;
    if (this.#closeDispatched) return;
    this.#closeDispatched = true;
    const wasClean = info !== undefined && info !== null && !failedDuringConnecting;
    const code = info?.code ?? 1006;
    const reason = info?.reason ?? "";
    if (!wasClean) {
      this.dispatchEvent(new EventImpl("error"));
    }
    this.dispatchEvent(new CloseEventImpl("close", { wasClean, code, reason }));
  }
}

export { BlobImpl as Blob, CloseEventImpl as CloseEvent, MessageEventImpl as MessageEvent };
