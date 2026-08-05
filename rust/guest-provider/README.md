# `websocket-guest-provider`

The in-guest provider: a WebSocket client stack over `wasi:sockets` TCP,
built as a wasm component (`wasm32-wasip2`) that **exports**
`polymorph:websocket/connections`. Compositions plug it under any guest that
imports the interface:

```sh
# wss: capability comes from the polymorph:tls component (see below).
wac plug --plug polymorph-tls-component.wasm websocket-guest-provider.wasm \
    -o provider-tls.wasm
wac plug --plug provider-tls.wasm your-guest.wasm -o composed.wasm

wasmtime run -W component-model-async=y -S p3 -S inherit-network composed.wasm
```

It is one of this repository's conforming implementations: the shared
conformance suite runs the composed shape as the `composed` target, and
every behavioral contract in `wit/README.md` (close semantics, inbound
buffering, streaming, the portability bounds) is gated there against the
hosted implementations.

## TLS posture (`wss:`)

`wss:` is served by composing
[`polymorph:tls`](https://github.com/polymorph-components/polymorph-tls) — a pure-wasm TLS 1.3
client — pinned by `tls-component.rev` beside this crate. The posture,
resolving the trust-anchor and secret-placement questions the in-guest
setting raises:

- **Trust anchors are explicit deployment configuration.** There is no
  system root store in-guest, and the TLS component verifies against
  exactly the roots it is handed (no bypass exists). With no roots
  configured, `wss:` connects **fail closed** with `connect-failed`.
- **Session secrets live in the TLS component's linear memory**,
  unreachable from this provider and from application guests — the
  composition boundary is the isolation story (a memory bug in the
  application cannot exfiltrate key material). Timing is the remaining
  channel; `component-tls`'s README carries the algorithm profile and the
  class-based timing analysis, including why a client (which never signs)
  avoids the one class-D operation entirely.
- The provider itself never touches TLS internals: it wires the
  component's cleartext/ciphertext stream transforms between its protocol
  layer and the socket, and treats a TLS failure mid-connection as an
  abnormal closure (`wait-closed` reports `none`), per the close
  contract.

`ws:`-only deployments still compose the TLS component (the import must
be satisfied); it stays inert and costs nothing at run time.

## Configuration

The provider reads its knobs from the environment at each `connect` —
the standard configuration channel for wasip2-shaped components; the
embedder controls the environment through its WASI context:

| Variable | Meaning | Default |
| --- | --- | --- |
| `LANN_WEBSOCKET_CONNECT_TIMEOUT_MS` | the connect/handshake bound | `30000` |
| `LANN_WEBSOCKET_CLOSE_TIMEOUT_MS` | the closing-procedure bound | `10000` |
| `LANN_WEBSOCKET_MAX_INBOUND_BUFFER_BYTES` | the inbound-buffer bound | `8388608` (8 MiB) |
| `LANN_WEBSOCKET_TLS_ROOTS_PEM` | `wss:` trust anchors, as a PEM bundle | unset (`wss:` fails closed) |

These are the same implementation-defined bounds every implementation
exposes through its own channel (see `wit/README.md`, "Portability
contract"); malformed values fall back to the defaults.

## Building

```sh
cargo build --release -p websocket-guest-provider --target wasm32-wasip2
```

The wasip2 target emits a component directly. The conformance
composition is `just conformance::compose-guest`; the composed target
runs with `just conformance::composed`.
