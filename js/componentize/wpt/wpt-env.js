// The stand-in for WPT's `constants.sub.js`: the same constants and
// `CreateWebSocket*` helpers the vendored suites call, built against the
// suite echo server instead of wptserve's substituted host/ports. The
// server's `ws:` base URL is injected at run time through
// `globalThis.__WPT_SERVER_BASE` (the runner sets it from its `run`
// argument; the baseline leg sets it after spawning the server), so
// everything here reads it lazily.
//
// Where the WPT handlers select the "echo" subprotocol when offered, the
// suite server does the same (see conformance/server/PROTOCOL.md), so the
// helpers need no query parameters.

function __wptBase() {
  const base = globalThis.__WPT_SERVER_BASE;
  if (!base) {
    throw new Error("wpt-env: __WPT_SERVER_BASE is not set (the runner sets it before starting groups)");
  }
  return base;
}

var __PATH = "echo";

// Lazily-resolved constants, since group modules evaluate before the
// server base is known; only the wrapped test bodies read them.
Object.defineProperty(globalThis, "SCHEME_DOMAIN_PORT", {
  configurable: true,
  get: () => __wptBase(),
});

// A minimal `location` for suites that build negative-case URLs from the
// page origin. The values are inert (nothing serves them); identical in
// both parity legs so the comparison stays fair.
if (globalThis.location === undefined) {
  globalThis.location = {
    protocol: "http:",
    host: "web-platform.test:8001",
    hostname: "web-platform.test",
    origin: "http://web-platform.test:8001",
    search: "",
  };
}

function IsWebSocket() {
  if (!self.WebSocket) {
    assert_true(false, "environment does not provide WebSocket");
  }
}

var wsocket;

function CreateWebSocketNonAsciiProtocol(nonAsciiProtocol) {
  IsWebSocket();
  return new WebSocket(__wptBase() + "/" + __PATH, nonAsciiProtocol);
}

function CreateWebSocketWithAsciiSep(asciiWithSep) {
  IsWebSocket();
  return new WebSocket(__wptBase() + "/" + __PATH, asciiWithSep);
}

function CreateWebSocketWithSpaceInUrl(urlWithSpace) {
  IsWebSocket();
  const url = `ws://${urlWithSpace}:80/${__PATH}`;
  return new WebSocket(url);
}

function CreateWebSocketWithSpaceInProtocol(protocolWithSpace) {
  IsWebSocket();
  return new WebSocket(__wptBase() + "/" + __PATH, protocolWithSpace);
}

function CreateWebSocketWithRepeatedProtocols() {
  IsWebSocket();
  return new WebSocket(__wptBase() + "/" + __PATH, ["echo", "echo"]);
}

function CreateWebSocketWithRepeatedProtocolsCaseInsensitive() {
  IsWebSocket();
  wsocket = new WebSocket(__wptBase() + "/" + __PATH, ["echo", "eCho"]);
}

function CreateWebSocket(isProtocol, isProtocols) {
  IsWebSocket();
  const url = __wptBase() + "/" + __PATH;
  if (isProtocol) {
    return new WebSocket(url, "echo");
  }
  if (isProtocols) {
    return new WebSocket(url, ["echo", "chat"]);
  }
  return new WebSocket(url);
}
