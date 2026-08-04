# The WPT parity gate

The vendored [web-platform-tests](https://github.com/web-platform-tests/wpt)
WebSocket suites run twice against the same suite echo server:

- **baseline** (`parity/baseline.mjs`): directly on this platform's own
  `WebSocket` (Node's built-in) — no shim, no WIT, no wasm;
- **round trip** (`parity/roundtrip.mjs`): through the full carrier stack —
  `js/componentize/websocket.js` (the shim), the `lann:websocket` WIT
  surface, the component ABI, jco's transpile, and `js/jco/websocket.js` —
  terminating in the same platform WebSocket.

The comparator (`parity/compare.mjs`) holds the round trip to the
baseline's pass set: every baseline pass the round trip loses must be
pinned in `parity/losses.js` (with its classification in the shim header's
deviations registry), a pinned loss no longer observed fails the run until
re-recorded, and whatever the platform itself fails falls out of scope
without an exclusion list. `just wpt::parity` runs the gate;
`just wpt::update-losses` re-records the ratchet as a reviewable diff.

## Vendoring policy

`vendor/` holds unmodified files from the WPT `websockets/` directory,
pinned at commit `123fbf3003bb1277efbc0612ff55e689ba755db9` (see WPT's
`LICENSE.md`, vendored alongside). The subset is chosen by what the
carrier stack and the suite server can exercise:

- `.any.js` tests only (no `.html`, workers, or idlharness);
- the `?default` variant semantics only (no `wss`/`h2` — TLS coverage is
  tracked in the repository's wss issue);
- tests whose server behavior maps onto the suite echo server's contract
  (`conformance/server/PROTOCOL.md`): the `/echo` handler including its
  WPT-compat rules (the `echo` subprotocol offer, the `Goodbye` close);
- no dependence on real timer delays (the componentize-js runtime has no
  timers; the harness's `step_timeout` degrades to an immediate task).

`wpt-env.js` replaces WPT's `constants.sub.js` — the same constants and
`CreateWebSocket*` helpers, built against the injected server base URL —
and `harness.js` is the minimal `testharness.js` stand-in (including the
event-driven `async_test` API). `component.sh suites` wraps each vendored
file into an importable group module under `build/`; the manifest in
`component.sh` is the one list both legs run.

## Adding a test

1. Fetch the file from the pinned WPT commit into `vendor/`, unmodified.
2. Add its id to `GROUP_IDS` in `component.sh`.
3. If it needs new server behavior, extend `conformance/server` and
   `PROTOCOL.md` (WPT-compat rules are part of the `/echo` contract).
4. `just wpt::parity`; classify any new loss in the shim's deviations
   registry and record it with `just wpt::update-losses`.

Bumping the WPT pin is a deliberate act: re-fetch every vendored file at
the new commit and update the pin recorded above in one change.
