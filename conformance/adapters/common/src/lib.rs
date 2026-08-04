//! Shared building blocks for the conformance adapters.
//!
//! Everything an adapter needs beyond its own host provisioning lives here:
//! the [`TestOutcome`] vocabulary, the JSON result document
//! ([`AdapterReport`]), the corpus registry ([`TESTS`], kept in sync with
//! the guest by [`verify_corpus`]), the per-test hang guard ([`run_test`]),
//! and the bounded-concurrency corpus runner ([`run_corpus`]).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

/// The WIT-observable outcome of one test run, mirroring the guest's
/// `test-result` variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    /// Every assertion held.
    Pass,
    /// An assertion failed; the string says which.
    Fail(String),
    /// The test does not apply; the string says why.
    Skipped(String),
}

/// The raw status vocabulary of the result document. The runner reclassifies
/// these against `targets.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawStatus {
    Pass,
    Fail,
    Skip,
}

/// One test's row in the result document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawResult {
    pub test_id: String,
    pub status: RawStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

/// The adapter result document: the whole contract between an adapter and
/// the runner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdapterReport {
    /// The target id, matching a `[target.<id>]` table in `targets.toml`.
    pub target: String,
    /// The network environment the run used (currently always `loopback`).
    pub environment: String,
    /// The sha256 of the guest component this run executed (hex), so the
    /// runner can reject a matrix assembled from mixed builds.
    #[serde(default)]
    pub guest: String,
    pub results: Vec<RawResult>,
}

/// Serialize `report` to `<out_dir>/<file_stem>.json`.
pub fn write_report(out_dir: &Path, file_stem: &str, report: &AdapterReport) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("{file_stem}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(report)?)?;
    Ok(path)
}

/// The corpus registry, mirroring the guest's `CORPUS` and
/// `conformance/tests.toml`. [`verify_corpus`] holds the mirrors together.
pub const TESTS: &[&str] = &[
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

/// Message count/size for count-parameterized tests are owned by the
/// guest itself (see its `params`), so every target runs the identical
/// workload by construction; adapters carry no copy to drift.
///
/// The inbound-buffer bound every adapter must configure while running the
/// corpus. It rides `test-config`, so the guest floods against exactly the
/// bound the adapter configured — the two cannot drift.
pub const CONFORMANCE_MAX_INBOUND_BUFFER_BYTES: u32 = 256 * 1024;

/// The connect bound adapters must configure, so `connect-timeout` (the
/// `/stall` probe) resolves well inside the hang guard.
pub const CONFORMANCE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The closing-handshake bound adapters must configure, so
/// `close-handshake-timeout` (the `/ignore-close` probe) resolves well
/// inside the hang guard.
pub const CONFORMANCE_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// The hang guard for one test attempt: single attempt, no retries — a
/// nondeterministic failure is a real signal and must surface, not be
/// masked by a second attempt. Must comfortably exceed the configured
/// connect/close bounds so a genuine timeout surfaces as a WIT outcome
/// rather than tripping this guard.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Run one attempt under the hang guard, folding adapter errors and
/// timeouts into failures.
pub async fn run_test(
    test_id: &str,
    timeout: Duration,
    attempt: impl AsyncFnOnce() -> Result<TestOutcome>,
) -> RawResult {
    let outcome = match tokio::time::timeout(timeout, attempt()).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => TestOutcome::Fail(format!("adapter error: {err:#}")),
        Err(_) => TestOutcome::Fail("attempt timed-out".to_string()),
    };
    let (status, detail) = match outcome {
        TestOutcome::Pass => (RawStatus::Pass, None),
        TestOutcome::Fail(detail) => (RawStatus::Fail, Some(detail)),
        TestOutcome::Skipped(detail) => (RawStatus::Skip, Some(detail)),
    };
    RawResult {
        test_id: test_id.to_string(),
        status,
        detail,
    }
}

/// The default `--jobs` for in-process adapters: 3 x the cores available to
/// this process, clamped to [2, 12] (the corpus is I/O-bound, so the
/// optimum exceeds the core count).
pub fn default_jobs() -> usize {
    scaled_jobs(3, 12)
}

/// The default `--jobs` for adapters that boot a heavyweight runtime per
/// test (Node, Chromium): 2 x the available cores, clamped to [2, 8].
pub fn default_jobs_process_heavy() -> usize {
    scaled_jobs(2, 8)
}

fn scaled_jobs(multiplier: usize, max: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores * multiplier).clamp(2, max)
}

/// Run `tests` (each via `run`, filtered by `only` when non-empty)
/// concurrently, bounded by `jobs`, logging each result as it lands.
///
/// Tests are independent — a fresh instance and connection per test — so
/// they can safely overlap; `buffered` preserves the registry order of the
/// results. An `only` filter that selects nothing is an error: silently
/// running zero tests would let a typo'd id produce an empty (green)
/// report.
pub async fn run_corpus<F, Fut>(
    tests: &[&'static str],
    only: &[String],
    jobs: usize,
    run: F,
) -> Result<Vec<RawResult>>
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = RawResult>,
{
    let selected: Vec<&'static str> = tests
        .iter()
        .copied()
        .filter(|test_id| only.is_empty() || only.iter().any(|t| t == test_id))
        .collect();
    anyhow::ensure!(
        !selected.is_empty(),
        "--only selected no tests (registered: {})",
        tests.join(", ")
    );
    Ok(futures::stream::iter(selected)
        .map(|test_id| {
            let fut = run(test_id);
            async move {
                let result = fut.await;
                eprintln!("{test_id} … {:?}", result.status);
                result
            }
        })
        .buffered(jobs.max(1))
        .collect()
        .await)
}

/// Verify the guest's `list-tests` ids match this adapter's registered
/// corpus exactly, so the hand-mirrored lists cannot silently drift from
/// the corpus the guest actually implements.
pub fn verify_corpus(guest_ids: &[String], tests: &[&'static str]) -> Result<()> {
    let guest: std::collections::BTreeSet<&str> = guest_ids.iter().map(|s| s.as_str()).collect();
    let local: std::collections::BTreeSet<&str> = tests.iter().copied().collect();
    let missing_here: Vec<&&str> = guest.difference(&local).collect();
    let missing_in_guest: Vec<&&str> = local.difference(&guest).collect();
    anyhow::ensure!(
        missing_here.is_empty() && missing_in_guest.is_empty(),
        "adapter test list diverges from the guest's list-tests export: \
         in guest but not adapter: [{}]; in adapter but not guest: [{}]",
        missing_here
            .iter()
            .map(|s| **s)
            .collect::<Vec<_>>()
            .join(", "),
        missing_in_guest
            .iter()
            .map(|s| **s)
            .collect::<Vec<_>>()
            .join(", "),
    );
    Ok(())
}

/// Verify the guest's `list-tests` descriptors match the authoritative
/// registry (`tests.toml`) exactly — ids and tags both — so the registry
/// cannot claim coverage the guest does not implement, and guest tags
/// (which drive skip classification) cannot drift from the declared ones.
pub fn verify_registry(guest: &[(String, Vec<String>)], tests_toml: &str) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Registry {
        #[serde(rename = "test")]
        tests: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
        tags: Vec<String>,
    }
    let registry: Registry = toml::from_str(tests_toml).context("parse tests.toml")?;
    let declared: std::collections::BTreeMap<&str, &[String]> = registry
        .tests
        .iter()
        .map(|t| (t.id.as_str(), t.tags.as_slice()))
        .collect();
    let listed: std::collections::BTreeMap<&str, &[String]> = guest
        .iter()
        .map(|(id, tags)| (id.as_str(), tags.as_slice()))
        .collect();
    for (id, tags) in &listed {
        match declared.get(id) {
            None => anyhow::bail!("guest test {id:?} is not registered in tests.toml"),
            Some(declared_tags) if declared_tags != tags => anyhow::bail!(
                "test {id:?} tags diverge: guest {tags:?}, tests.toml {declared_tags:?}"
            ),
            Some(_) => {}
        }
    }
    for id in declared.keys() {
        anyhow::ensure!(
            listed.contains_key(id),
            "tests.toml registers {id:?} but the guest does not implement it"
        );
    }
    Ok(())
}

/// The sha256 of a file, hex-encoded: the provenance stamp adapters put in
/// their reports so the runner can reject a matrix assembled from mixed
/// guest builds.
pub fn file_sha256(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let digest = sha2::Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// A loopback `ws:` URL whose connect attempt should be refused: a port that
/// was just bound and released. The window between release and use is small
/// but real; a collision surfaces as a `connect-refused` failure, not a
/// silent pass.
pub async fn unreachable_url() -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(format!("ws://127.0.0.1:{port}/echo"))
}
