# `lann:websocket`

A WIT interface for WebSocket client connections, plus multiple
implementations that run the *same* guest component against real WebSocket
stacks — planned as a sibling of
[`lann:webcrypto`](https://github.com/lann/component-webcrypto) and
[`lann:webrtc-datachannels`](https://github.com/lann/component-webrtc-datachannels),
deliberately mirroring their architecture.

Status: **proposal**. This README is the design seed; open questions and
findings are tracked in the [issues](../../issues).

## Why this exists

WebAssembly components have no standard WebSocket path today: `wasi:http`
carries request/response and has no upgrade mechanism, and `wasi:sockets`
gives raw TCP with no TLS, HTTP handshake, or framing. Meanwhile the
browser — the platform this family of packages treats as a first-class
deployment target — offers *only* WebSocket (and WebRTC) for long-lived
duplex connections.

The immediate consumer is
[`component-iroh`](https://github.com/lann/component-iroh): an iroh
endpoint keeps a persistent secure WebSocket open to its home relay, and
publishes signed discovery records over HTTPS. The relay leg is exactly
the capability gap this package fills. But the interface is
general-purpose: message-oriented duplex connectivity that behaves
identically in a browser, on a native host, and (where feasible) fully
in-guest.

## Planned shape

Following the sibling repositories:

| Piece | Deliverable |
| --- | --- |
| `wit/` | The `lann:websocket` package: a `types` interface for structural types (errors, message variants, close info) and a stateful interface owning the connection resource. One copy at the root; consumers pull it in via `wit/deps` symlinks. |
| jco host | Browser-first host library over the standard [`WebSocket` API](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket) only — no `node:` modules, no runtime dependencies; the same file must load in a browser unchanged. Node (24+, for JSPI) is just the current runner. |
| Wasmtime host | Native host crate (e.g. `tokio-tungstenite`), modeled after `wasmtime_wasi_http::p3`: `add_to_linker` + a view trait. |
| In-guest provider | A wasm component running a WebSocket stack over `wasi:sockets` TCP, exporting the package surface; composable via `wac plug`. TLS is the open problem here — see "Design questions" below. |
| `conformance/` | Cross-implementation conformance tests asserting the targets behave compatibly, run against a real WebSocket echo/reference server. |

## Design notes

These record the intended rulings; each becomes binding only when the WIT
lands.

- **Message-oriented, not byte-oriented.** WebSocket is a message protocol;
  the interface preserves message boundaries. The surface should rhyme with
  the sibling's `data-channel` resource: a `message` variant
  (`binary(list<u8>)` / text), one message per `send`/`receive` call,
  concurrency for pipelining, and stream-backed variants
  (`stream<u8>` per message) to bound in-memory buffering for large
  messages. Backpressure rides the component-model async ABI. A shared
  shape here is deliberate: `component-iroh` wants to treat a relay
  WebSocket and a WebRTC data channel as interchangeable message
  transports.
- **The browser API is the least capable implementation, and it constrains
  the surface.** Browser `WebSocket` exposes no request headers, no client
  certificates, no proxy control, no manual TLS trust decisions, and no
  ping/pong access. The portable surface is therefore: URL (`ws:`/`wss:`),
  subprotocol list, messages, and close (code + reason). Capabilities the
  browser cannot serve are either designed out or isolated behind gates —
  divergence between implementations is resolved, never accumulated (the
  webcrypto sibling's portability ladder applies: design it out → enhance
  the deficient implementation → narrow uniformly → record latitude →
  gate).
- **Client-only first.** Accepting WebSocket connections is a server
  capability the browser lacks entirely and `component-iroh` does not need
  from this package. A `listener` surface, if ever wanted, is additive.
- **TLS in the in-guest provider.** `wss:` in-guest means a TLS client
  stack (rustls) over `wasi:sockets`, which raises trust-anchor
  provisioning and puts TLS secrets in guest linear memory. Options, to be
  settled by issue: ship it with the limitation documented, scope the
  in-guest provider to `ws:` (relay deployments terminating TLS elsewhere),
  or adopt [`wasi-tls`](https://github.com/WebAssembly/wasi-tls) when it is
  servable. The in-guest provider is explicitly *optional* in the
  conformance target matrix, like structurally absent capabilities in the
  siblings.
- **Close semantics are part of the contract.** Close code/reason, the
  half-closed states, and what `receive` returns after a clean vs. abnormal
  close must be pinned by conformance cases from the start; this is where
  WebSocket stacks disagree most.

## Relationship to standards efforts

If a standard `wasi:websocket` (or an upgrade path in a future `wasi:http`)
materializes, this package's job becomes migration, not competition — the
same posture the siblings take toward their domains. The interface should
stay small enough that mapping onto a standard surface is mechanical.
