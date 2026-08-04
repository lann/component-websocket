// Environment-agnostic conformance corpus driver shared by the Node and
// browser jco runners. Given a factory that produces fresh guest instances
// and the suite echo server's base URL, it runs each registered test to a
// raw `pass`/`fail`/`skip` result row and returns the adapter result rows.
//
// Tests run in a single attempt (no retries): a nondeterministic failure is
// a real signal and must surface, not be masked by a second attempt. The
// guest owns every assertion; the driver only orchestrates and records.

/**
 * The registry of test ids, mirroring `conformance/tests.toml` (verified
 * against the guest's `list-tests` export before each run).
 */
export const TESTS = [
  "connect-basic",
  "connect-invalid-url",
  "connect-invalid-protocols",
  "connect-refused",
  "connect-rejected",
  "connect-timeout",
  "subprotocol-negotiated",
  "subprotocol-none-offered",
  "subprotocol-unoffered-selected",
  "subprotocol-none-selected",
  "echo-binary",
  "echo-text",
  "echo-text-unicode",
  "echo-empty",
  "echo-large",
  "message-boundaries",
  "binary-text-interleave",
  "concurrent-send-receive",
  "concurrent-receives",
  "close-local",
  "close-local-default",
  "close-local-idempotent",
  "close-boundary-codes",
  "close-reason-unicode",
  "close-invalid-code",
  "close-reason-too-long",
  "close-reason-without-code",
  "send-after-close",
  "receive-after-close",
  "close-remote",
  "close-remote-no-code",
  "close-abnormal",
  "receive-backlog-before-close",
  "close-handshake-timeout",
  "close-under-send-backpressure",
  "state-lifecycle",
  "wait-closed-latched",
  "wait-closed-pending",
  "send-via-stream",
  "receive-via-stream",
  "stream-text-round-trip",
  "send-via-stream-invalid-utf8",
  "send-via-stream-length-mismatch",
  "send-via-stream-sent-count",
  "receive-via-stream-once",
  "receive-via-stream-end-on-close",
  "receive-via-stream-overflow",
  "receive-buffer-overflow",
  "receive-buffer-overflow-unacknowledged",
  "overflow-oversized-message",
  "overflow-oversized-message-pending",
];

// Message count/size for count-parameterized tests are owned by the guest
// itself, so every target runs the identical workload by construction; the
// drivers carry no copy to drift.
//
// The bounds the jco runners configure through the host module's exported
// hooks, mirroring `conformance-common` (the Rust adapters' constants):
// the buffer small enough that `receive-buffer-overflow` triggers with a
// bounded flood (it rides the test config, so the guest floods against
// exactly this value), the timeouts short enough that the `/stall` and
// `/ignore-close` probes resolve well inside the hang guard.
export const MAX_INBOUND_BUFFER_BYTES = 256 * 1024;
export const CONNECT_TIMEOUT_MS = 5_000;
export const CLOSE_TIMEOUT_MS = 3_000;

// The hang guard for one test: single attempt, no retries. Must
// comfortably exceed the configured connect/close bounds so a genuine
// timeout surfaces as a WIT outcome rather than tripping this guard.
export const TEST_TIMEOUT_MS = 60_000;

// Tests that hold a *pending* WebSocket handshake to the echo server for
// their whole duration. Chromium serializes in-flight WebSocket handshakes
// per endpoint (the WebSocket endpoint lock), so a pending `/stall`
// handshake head-of-line-blocks every other connect to the same host:port
// until it resolves; running these after the concurrent phase keeps them
// from starving unrelated connects past their bound.
const HANDSHAKE_BLOCKING = new Set(["connect-timeout"]);

/**
 * Verify the guest's `list-tests` ids match this driver's registry exactly,
 * so the hand-mirrored lists cannot silently drift.
 */
export function verifyCorpus(guestIds) {
  const guest = new Set(guestIds);
  const local = new Set(TESTS);
  const missingHere = [...guest].filter((id) => !local.has(id));
  const missingInGuest = [...local].filter((id) => !guest.has(id));
  if (missingHere.length || missingInGuest.length) {
    throw new Error(
      `driver test list diverges from the guest's list-tests export: ` +
        `in guest but not driver: [${missingHere.join(", ")}]; ` +
        `in driver but not guest: [${missingInGuest.join(", ")}]`,
    );
  }
}

/**
 * Run the corpus. `newInstance` yields a fresh guest instance per test;
 * `serverUrl`/`unreachableUrl` flow into each test's config.
 * Returns result rows in registry order.
 */
export async function runCorpus({
  newInstance,
  serverUrl,
  unreachableUrl,
  only = [],
  jobs = 4,
  log = () => {},
}) {
  const selected = TESTS.filter((id) => !only.length || only.includes(id));
  if (!selected.length) {
    throw new Error(`--only selected no tests (registered: ${TESTS.join(", ")})`);
  }

  {
    // The corpus-drift gate, against a throwaway instance.
    const probe = await newInstance();
    verifyCorpus(probe.runner.listTests().map((d) => d.id));
  }

  // Instantiation is main-thread-heavy (in a browser page it competes with
  // every running test's wall-clock bounds), so instances are created one
  // at a time; the tests themselves still overlap on their network waits.
  let instantiateQueue = Promise.resolve();
  const serialInstance = () => {
    const next = instantiateQueue.then(() => newInstance());
    instantiateQueue = next.then(
      () => {},
      () => {},
    );
    return next;
  };

  const results = new Array(selected.length);
  const concurrent = [];
  const serial = [];
  selected.forEach((testId, index) => {
    (HANDSHAKE_BLOCKING.has(testId) ? serial : concurrent).push([testId, index]);
  });

  let next = 0;
  const worker = async () => {
    for (;;) {
      const slot = next;
      next += 1;
      if (slot >= concurrent.length) return;
      const [testId, index] = concurrent[slot];
      results[index] = await runOne(serialInstance, testId, serverUrl, unreachableUrl);
      log(`${testId} … ${results[index].status}`);
    }
  };
  await Promise.all(
    Array.from({ length: Math.max(1, Math.min(jobs, concurrent.length || 1)) }, worker),
  );
  for (const [testId, index] of serial) {
    results[index] = await runOne(serialInstance, testId, serverUrl, unreachableUrl);
    log(`${testId} … ${results[index].status}`);
  }
  return results;
}

async function runOne(newInstance, testId, serverUrl, unreachableUrl) {
  const config = {
    serverUrl,
    unreachableUrl,
    maxInboundBufferBytes: MAX_INBOUND_BUFFER_BYTES,
  };
  let outcome;
  try {
    const instance = await newInstance();
    outcome = await withTimeout(
      instance.runner.runTest(testId, config),
      TEST_TIMEOUT_MS,
      "attempt timed-out",
    );
  } catch (err) {
    outcome = { tag: "fail", val: `adapter error: ${err?.stack ?? err}` };
  }
  switch (outcome.tag) {
    case "pass":
      return { test_id: testId, status: "pass", detail: null };
    case "fail":
      return { test_id: testId, status: "fail", detail: outcome.val ?? "" };
    case "skipped":
      return { test_id: testId, status: "skip", detail: outcome.val ?? "" };
    default:
      return {
        test_id: testId,
        status: "fail",
        detail: `unrecognized outcome ${JSON.stringify(outcome)}`,
      };
  }
}

function withTimeout(promise, ms, message) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => resolve({ tag: "fail", val: message }), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}
