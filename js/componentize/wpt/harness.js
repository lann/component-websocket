// A minimal testharness.js stand-in for running vendored WPT WebSocket
// tests inside a componentize-js guest (which has no browser globals) and
// under plain Node (the parity baseline leg).
//
// It implements what those tests use — `test`, `promise_test`, and the
// event-driven `async_test` with its `step_func`/`step_timeout`/`done`/
// `unreached_func` API, the `assert_*` functions they call (including
// `assert_throws_dom` with the legacy constant names the older suites
// pass), and the `self` global. `async_test`s run concurrently within a
// group (each owns its own socket) and are bounded by a per-test timeout,
// since a lost event would otherwise hang the run.
//
// Results collect in module state; `drain()` awaits quiescence (tests may
// register more tests from callbacks) and `takeResults()` returns
// `{ name, status, message? }` records for the runner to classify.

const results = [];
let registered = 0;
let settled = 0;
let onResult = null;
/** @type {Promise<unknown>[]} */
let pending = [];

/** Per-async_test bound: a lost event must settle as a failure, not a hang. */
const ASYNC_TEST_TIMEOUT_MS = 10_000;

// The componentize-js runtime has no timers; where they are missing the
// watchdog is not armed (the embedder's own run bound is the backstop)
// and `step_timeout` degrades to an immediate task. No vendored test
// relies on a real delay (the vendoring policy excludes those).
const setTimer = globalThis.setTimeout ?? ((fn) => {
  Promise.resolve().then(fn);
  return 0;
});
const clearTimer = globalThis.clearTimeout ?? (() => {});
const HAS_REAL_TIMERS = typeof globalThis.setTimeout === "function";

/**
 * Subscribe to results as they settle: `cb` receives each
 * `{ name, status, message? }` record immediately after it lands. One
 * subscriber; pass null to unsubscribe. The parity runner streams its
 * records through this.
 */
export function setOnResult(cb) {
  onResult = cb;
}

class AssertionError extends Error {}
globalThis.AssertionError = AssertionError;

function fail(message) {
  throw new AssertionError(message);
}

function settle(name, result) {
  settled += 1;
  results.push(result);
  onResult?.(result);
}

function record(name, run) {
  registered += 1;
  const promise = (async () => {
    try {
      await run();
      settle(name, { name, status: "PASS" });
    } catch (e) {
      settle(name, { name, status: "FAIL", message: String((e && e.message) || e) });
    }
  })();
  pending.push(promise);
}

if (globalThis.self === undefined) {
  globalThis.self = globalThis;
}

globalThis.setup = function () {};
globalThis.done = function () {};

globalThis.promise_test = function (fn, name) {
  record(name, () => fn({}));
};

globalThis.test = function (fn, name) {
  record(name, () => {
    fn({
      step(stepFn, ...args) {
        return stepFn(...args);
      },
    });
  });
};

/**
 * The event-driven WPT test type: the returned object's `step_func`
 * wrappers route callback exceptions into the test's outcome, and the
 * test settles when `done()` runs (PASS), a step throws (FAIL), or the
 * timeout fires (FAIL).
 */
globalThis.async_test = function (nameOrFn, maybeName) {
  const name = typeof nameOrFn === "string" ? nameOrFn : maybeName;
  let finish;
  const outcome = new Promise((resolve) => {
    finish = resolve;
  });
  let completed = false;
  const t = {
    step(fn, ...args) {
      if (completed) return undefined;
      try {
        return fn.call(t, ...args);
      } catch (e) {
        completed = true;
        finish({ ok: false, message: String((e && e.message) || e) });
        return undefined;
      }
    },
    step_func(fn) {
      return (...args) => t.step(fn, ...args);
    },
    step_func_done(fn) {
      return (...args) => {
        t.step(fn ?? (() => {}), ...args);
        t.done();
      };
    },
    step_timeout(fn, ms, ...args) {
      return setTimer(() => t.step(fn, ...args), ms);
    },
    unreached_func(description) {
      return () => {
        t.step(() => {
          fail(`unreached: ${description ?? "code was not meant to run"}`);
        });
      };
    },
    done() {
      if (completed) return;
      completed = true;
      finish({ ok: true });
    },
  };
  record(name, async () => {
    let timer;
    if (HAS_REAL_TIMERS) {
      timer = setTimer(() => {
        if (!completed) {
          completed = true;
          finish({ ok: false, message: `async_test timed out after ${ASYNC_TEST_TIMEOUT_MS}ms` });
        }
      }, ASYNC_TEST_TIMEOUT_MS);
    }
    const result = await outcome;
    if (timer !== undefined) {
      clearTimer(timer);
    }
    if (!result.ok) {
      fail(result.message);
    }
  });
  if (typeof nameOrFn === "function") {
    t.step(nameOrFn, t);
  }
  return t;
};

// WPT's /common/subset-tests.js sharding helper: run everything.
globalThis.subsetTest = function (testFunc, ...args) {
  return testFunc(...args);
};

globalThis.assert_true = function (value, message) {
  if (value !== true) {
    fail(`assert_true: ${message ?? ""} (got ${value})`);
  }
};

globalThis.assert_false = function (value, message) {
  if (value !== false) {
    fail(`assert_false: ${message ?? ""} (got ${value})`);
  }
};

globalThis.assert_equals = function (actual, expected, message) {
  if (actual !== expected) {
    fail(`assert_equals: ${message ?? ""} (got ${String(actual)}, expected ${String(expected)})`);
  }
};

globalThis.assert_not_equals = function (actual, expected, message) {
  if (actual === expected) {
    fail(`assert_not_equals: ${message ?? ""} (got ${String(actual)})`);
  }
};

globalThis.assert_in_array = function (actual, expected, message) {
  if (!expected.includes(actual)) {
    fail(`assert_in_array: ${message ?? ""} (got ${String(actual)})`);
  }
};

globalThis.assert_array_equals = function (actual, expected, message) {
  if (actual.length !== expected.length) {
    fail(
      `assert_array_equals: ${message ?? ""} (lengths differ: got ${actual.length}, expected ${expected.length})`,
    );
  }
  for (let i = 0; i < actual.length; i += 1) {
    if (actual[i] !== expected[i]) {
      fail(
        `assert_array_equals: ${message ?? ""} (index ${i}: got ${String(actual[i])}, expected ${String(expected[i])})`,
      );
    }
  }
};

globalThis.assert_greater_than = function (actual, expected, message) {
  if (!(actual > expected)) {
    fail(`assert_greater_than: ${message ?? ""} (got ${actual}, expected > ${expected})`);
  }
};

globalThis.assert_less_than_equal = function (actual, expected, message) {
  if (!(actual <= expected)) {
    fail(`assert_less_than_equal: ${message ?? ""} (got ${actual}, expected <= ${expected})`);
  }
};

// The legacy DOMException constant names some vendored suites still pass
// to assert_throws_dom, mapped to the modern names thrown errors carry.
const LEGACY_DOM_NAMES = {
  INDEX_SIZE_ERR: "IndexSizeError",
  HIERARCHY_REQUEST_ERR: "HierarchyRequestError",
  WRONG_DOCUMENT_ERR: "WrongDocumentError",
  INVALID_CHARACTER_ERR: "InvalidCharacterError",
  NO_MODIFICATION_ALLOWED_ERR: "NoModificationAllowedError",
  NOT_FOUND_ERR: "NotFoundError",
  NOT_SUPPORTED_ERR: "NotSupportedError",
  INUSE_ATTRIBUTE_ERR: "InUseAttributeError",
  INVALID_STATE_ERR: "InvalidStateError",
  SYNTAX_ERR: "SyntaxError",
  INVALID_MODIFICATION_ERR: "InvalidModificationError",
  NAMESPACE_ERR: "NamespaceError",
  INVALID_ACCESS_ERR: "InvalidAccessError",
  SECURITY_ERR: "SecurityError",
  NETWORK_ERR: "NetworkError",
  ABORT_ERR: "AbortError",
  QUOTA_EXCEEDED_ERR: "QuotaExceededError",
  TIMEOUT_ERR: "TimeoutError",
  DATA_CLONE_ERR: "DataCloneError",
};

function expectedDomName(name) {
  return LEGACY_DOM_NAMES[name] ?? name;
}

globalThis.assert_throws_dom = function (name, fn, description) {
  const want = expectedDomName(name);
  try {
    fn();
  } catch (e) {
    if (e?.name === want) {
      return;
    }
    fail(`${description ?? "assert_throws_dom"}: expected ${want}, got ${e}`);
  }
  fail(`${description ?? "assert_throws_dom"}: expected ${want}, nothing thrown`);
};

globalThis.assert_throws_js = function (constructor, fn, description) {
  try {
    fn();
  } catch (e) {
    if (e instanceof constructor) {
      return;
    }
    fail(`${description ?? "assert_throws_js"}: expected ${constructor.name}, got ${e}`);
  }
  fail(`${description ?? "assert_throws_js"}: expected ${constructor.name}, nothing thrown`);
};

globalThis.promise_rejects_dom = function (test, name, promise, description) {
  const want = expectedDomName(name);
  return promise.then(
    () => {
      fail(`${description ?? "promise_rejects_dom"}: expected ${want}, promise resolved`);
    },
    (e) => {
      if (e?.name !== want) {
        fail(`${description ?? "promise_rejects_dom"}: expected ${want}, got ${e}`);
      }
    },
  );
};

globalThis.assert_unreached = function (message) {
  fail(`assert_unreached: ${message ?? ""}`);
};

/**
 * Await test quiescence: keeps waiting while settled tests trail
 * registered ones (test callbacks may register further tests).
 */
export async function drain() {
  for (;;) {
    const wave = pending;
    pending = [];
    await Promise.all(wave);
    if (pending.length === 0 && settled === registered) {
      return;
    }
  }
}

export function takeResults() {
  return results.splice(0);
}
