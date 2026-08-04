# The suite echo server protocol

`conformance-echod` is the suite-owned WebSocket server every conformance
target runs against. It is deliberately separate from any production code
so it can evolve with the tests and be discarded without API cost. The
conformance guest builds URLs against this contract; changing it means
changing the guest and this file together.

## Startup contract

- Library: `conformance_echod::spawn(addr) -> RunningServer` binds and
  serves in the background; `base_url()` is the `ws://host:port` base.
- Binary: `conformance-echod [--addr HOST:PORT]` (default `127.0.0.1:0`)
  prints exactly one `LISTENING ws://HOST:PORT` line to stdout once bound,
  then serves until killed. Adapters that cannot embed the library scrape
  that line.

## Paths

Query values are parsed literally (no percent-decoding): keep them
token-safe (no spaces, `&`, `=`, or `#`). Unknown query parameters are
ignored; unknown paths answer HTTP 404.

| Path | Behavior |
| --- | --- |
| `GET /healthz` | Plain HTTP 200. Readiness probe; not a WebSocket endpoint. |
| `/echo` | Echo every text/binary message verbatim, preserving kind and boundaries. The closing handshake is echoed too: a client close frame is acknowledged with the same code and reason. |
| `/reject` | Answer the upgrade with HTTP 403: the client observes a failed handshake. |
| `/stall` | Never answer the handshake (held up to 120 s): the client's connect bound must fire. **Holds a pending handshake**: browsers serialize in-flight WebSocket handshakes per endpoint, so concurrent connects to the same host:port queue behind it. |
| `/close-after?count=N&code=C&reason=R` | Echo `N` messages (default 0), then the server initiates the close with code `C` and reason `R`; `code` omitted sends a code-less close frame (observed as 1005). Drains until the handshake completes. |
| `/burst-then-close?count=N&size=S&code=C&reason=R` | Immediately send `N` binary messages (default 1) of `S` bytes (default 16), then a close frame as in `/close-after`. |
| `/burst?count=N&size=S` | Immediately send `N` binary messages of `S` bytes, then keep the connection open, reading and discarding, until the client closes. |
| `/abrupt-close?after=N` | Echo `N` messages, then drop the TCP connection without a close frame: the client observes an abnormal closure. |
| `/ignore-close` | Never speak WebSocket back: every client frame (including its close frame) is read and discarded, and the connection is held open (up to 120 s). The client's closing-handshake bound must fire. |

## Subprotocol selection

On any WebSocket path:

- `?protocol=NAME` — select `NAME` if the client offered it; otherwise
  select nothing.
- `?force-protocol=NAME` — select `NAME` unconditionally, offered or not
  (probes client-side enforcement of the offer contract).
- Neither — select nothing, even when protocols were offered (probes the
  client-side offered-but-unselected rule).

## Burst payloads

The payload of message `index` in `/burst` and `/burst-then-close` is
`size` bytes where byte `i` is `(index + i) % 256`, so clients verify
content without carrying it in the test.
