# The `lann:websocket` package

A WIT interface for WebSocket client connections. This document holds the
package-wide contracts; item doc comments state what is specific to their
item and reference these sections by name.

## How the package is organized

- **`types`** — every structural (non-resource) type: the `error` variant,
  the message types, `close-info`, and the lifecycle enum. Structural types
  carry no host-side identity, so a single composition can freely share
  them across components.
- **`connections`** — the stateful `websocket` resource. A resource is
  owned by the one component that implements it, so the live-object surface
  is grouped in its own interface.

The surface is client-only: it opens outbound connections and has no
listener. It is message-oriented: WebSocket message boundaries are always
preserved, never flattened into a byte stream.

## Streaming contract

Messages normally travel as whole `message` values. To bound in-memory
buffering for large messages, both directions also have stream-backed
forms (`send-via-stream`, `receive-via-stream`) that carry each message as
a `stream-message`: a `kind`, a total `length` in bytes, and a byte
`stream` payload.

- One `stream-message` is exactly one WebSocket message. Streaming never
  changes message boundaries.
- The bytes carried by `stream-message.data` must match `kind` (valid
  UTF-8 when `kind` is `string`) and must total exactly `length` bytes. A
  producer that violates this is in error; the connection may be closed.
- `receive-via-stream` may be called **once** per connection. After that
  call, `receive` and further `receive-via-stream` calls fail
  `receiving-via-stream`. Pending `receive` calls fail with the same
  error: a pending receive is never handed a message once the stream is
  claimed.
- A stream returned by `receive-via-stream` or `state-changes` ends when
  the connection closes, whatever the cause. The end of a stream carries
  no error value: consult `wait-closed` for close details.
- Streaming bounds the *guest's* memory. It does not promise that the
  implementation never materializes a message: a browser-backed
  implementation receives each message fully materialized by the platform
  before it can stream it onward. The inbound-buffering bound (below) is
  the only cap on implementation-side buffering.

## Inbound buffering

There is **no wire-level inbound backpressure**: a guest that receives
slowly does not slow the remote sender down. The W3C `WebSocket` API
offers no read-side flow control, so a browser-backed implementation
cannot provide it; other implementations deliberately match that floor so
the same guest behaves compatibly everywhere.

Every implementation buffers inbound messages up to an
implementation-defined bound (8 MiB of payload bytes by convention). If
the buffer overflows:

- the connection is closed toward the peer;
- messages buffered before the overflow remain receivable;
- once the backlog is drained, `receive` fails
  `error.receive-buffer-overflow`, and a `receive-via-stream` stream ends;
- `wait-closed` reports whatever the peer's teardown produced, exactly as
  for any other close.

Guests that need flow control must implement it at the application layer
(for example acknowledgment or credit messages).

Outbound sends resolve when the message is handed to the transport.
Implementations bound their outbound buffering; the async ABI carries the
backpressure to the guest (a `send` future that has not resolved is the
signal to stop producing).

## Close contract

WebSocket close has three distinguishable shapes on this surface:

1. **Local close.** `close(code, reason)` validates its arguments eagerly
   (`invalid-argument` on violation; see the method docs for the bounds),
   then initiates the closing handshake and returns. The close is observed
   locally at once: in-flight and subsequent `send`/`receive` calls fail
   `error.closed`, the resource's streams end, and unread buffered
   messages are discarded, as are messages the peer sends during the
   handshake. The closing procedure is bounded end to end: the connection
   reaches `closed` when the handshake completes, or after an
   implementation-defined bound when pending sends cannot flush or the
   peer never completes it. `close` is idempotent; only the first call's
   frame is sent.
2. **Remote clean close** (a close frame arrives). Messages the peer sent
   before its close frame remain receivable: `receive` drains the backlog,
   then fails `error.closed`. `wait-closed` resolves `some(close-info)`
   with the frame's contents (code 1005 and an empty reason when the frame
   carried none).
3. **Abnormal close** (the connection drops without a close frame: TCP
   reset or EOF, a TLS failure mid-connection, a handshake that never
   completes). The backlog remains receivable, then `receive` fails
   `error.closed`. `wait-closed` resolves `none`.

`wait-closed` is the one authority for close details. It is latched:
awaiting it any number of times, before or after the close, yields the
same value — the peer's close frame if one was ever received (including
the peer's acknowledgement of a local close), otherwise `none`.
`error.closed` deliberately carries no payload. Implementations never
invent a `close-info`: the browser's synthesized 1006 ("abnormal
closure") is represented as `none`, not as a frame.

`error.closed` reports a *state*, not an event: any operation that cannot
proceed because the connection is closed or closing fails with it,
regardless of who initiated the close or why. After a
`receive-buffer-overflow` close, `receive` fails with
`receive-buffer-overflow` rather than `closed` once the backlog drains,
so the guest can tell the overflow apart.

Dropping the resource without calling `close` implies `close(none, "")`.

## Error contract

Which case an operation produces is part of each item's contract; the
package-wide rules are:

- The `string` payloads are human-readable diagnostics for logging. Never
  match on their contents. They may be empty: a browser-backed
  implementation cannot observe most failure detail (see "Portability
  contract").
- Eager cases (`invalid-url`, `invalid-argument`) are produced before any
  network activity, and the operation has no effect.
- `other` is reserved for implementation-specific failures that fit no
  named case. A conforming implementation produces a named case whenever
  one applies.

## Portability contract

The browser `WebSocket` API is the least capable implementation, and it
bounds this surface. Capabilities it cannot serve do not appear here:
request headers, cookies, client certificates, proxy control, TLS trust
decisions, ping/pong access, and read-side flow control. Close codes a
client may *send* are restricted to 1000 and 3000-4999, and close reasons
to 123 bytes, because the browser enforces exactly that; every
implementation applies the same bounds so guests behave identically
everywhere.

Latitude — points where implementations may differ, recorded here so
guests do not rely on either behavior:

- **Failure diagnostics.** The `string` payload of `connect-failed` (and
  the diagnostic detail of abnormal closes generally) is
  implementation-defined and may be empty. Browsers deliberately hide
  connection-failure detail; native stacks can report more. Guests get a
  stable *shape* (`connect-failed`, or `wait-closed` returning `none`),
  not stable detail.
- **TCP teardown cleanliness.** Whether the transport under a completed
  closing handshake tore down cleanly (the W3C `wasClean` flag beyond the
  frame exchange) is not exposed: `close-info` presence means "the peer's
  close frame was received", nothing more.
- **Concurrency ordering.** Concurrently pending `send`s and `receive`s
  are served in an implementation-defined order.
- **State granularity.** `state-changes` may coalesce `closing` away.
- **Buffer bounds and timeouts.** The inbound-buffer bound, the connect
  timeout, and the closing-handshake bound are implementation-defined;
  embedders may configure them through implementation-specific channels.

## Design notes

Decisions that shape the surface, recorded so the doc comments can stay
short:

- **`connect` returns an open connection.** There is no constructor plus
  wait-for-open: the resource exists only once the handshake succeeded, so
  the W3C `CONNECTING` state is unrepresentable and `websocket-state` has
  no `connecting` member.
- **No URL accessor.** The guest supplied the URL; echoing it back would
  only expose implementation-specific normalization differences.
- **`close` is fallible only about its arguments.** The result reports
  eager validation; initiation itself cannot fail and completion is
  awaited through `wait-closed`. Rejecting (rather than truncating or
  clamping) keeps every implementation's behavior identical to the
  browser's, which throws for the same inputs.
- **Close details ride `wait-closed`, not `error.closed`.** A single
  authority avoids implementations disagreeing about which errors carry
  the frame, and keeps this package's `error.closed` shaped like its
  sibling transports' for consumers that abstract over message transports.
- **`connect` takes plain arguments, not an options builder.** The
  portable connect surface is exactly a URL and a subprotocol offer.
  Capabilities behind a future gate (for example request headers on
  non-browser hosts) would arrive as a builder resource alongside, not by
  reshaping `connect`.
- **Text messages are `string`.** The component model already guarantees
  valid UTF-8 for `string`, which matches the WebSocket text frame
  contract; a text message can never carry invalid UTF-8 on this surface.

## Terminology

- **Message**: one WebSocket protocol message (RFC 6455 section 5.6),
  binary or text. Frames and fragmentation are below this surface.
- **Closing handshake**: the close-frame exchange of RFC 6455 section 7.
- **Close code / reason**: the status code and UTF-8 reason of a close
  frame (RFC 6455 section 5.5.1). Code 1005 means "no code was present";
  it is never sent on the wire.
- **Subprotocol**: an application-level protocol negotiated during the
  handshake via `Sec-WebSocket-Protocol` (RFC 6455 section 1.9).
- **Abnormal closure**: a connection that ended without a received close
  frame (RFC 6455 section 7.1.7).
