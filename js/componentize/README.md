# `js/componentize` — the browser-API shim

`websocket.js` is the inverse of `js/jco/websocket.js`: where that module
implements the `polymorph:websocket` **imports** over the standard browser
`WebSocket` API, this one implements the standard browser API **over the
WIT imports**, for JS guests componentized with
[componentize-js](https://github.com/dicej/componentize-js) (the wit-dylib
reboot). Application code written against `WebSocket` runs unchanged
inside a component, against any host that serves
`polymorph:websocket/connections`.

The shim's documented deviations from the WHATWG interface live in the
registry at the top of `websocket.js`, each classified *unserved*,
*WIT-forced*, or *environment* — the same discipline as the webcrypto
sibling's shim.

## The toolchain

componentize-js embeds a SpiderMonkey build (~20 minutes to compile), so
nobody here builds it: `wpt/component.sh toolchain` downloads a published
build for the revision pinned in `componentize-js.rev` and verifies it
against the byte digests in `componentize-js.sha256` — on download and on
every cached use. The published builds are currently the webcrypto
sibling's (both repositories pin the same revision); `component.sh`
documents the sourcing and the escape hatches (`COMPONENTIZE_JS`,
`COMPONENTIZE_JS_RELEASE`).

## The WPT parity gate

`wpt/` runs vendored web-platform-tests WebSocket suites through this shim
**round trip** — shim → WIT → component ABI → jco → `js/jco/websocket.js`
→ the platform's own WebSocket — against the same suite echo server a
plain-Node baseline leg uses, and holds the round trip to the baseline's
pass set. See [`wpt/README.md`](wpt/README.md).

```sh
just wpt::parity     # both legs + comparator
just wpt::smoke      # fast pipeline bisector
```
