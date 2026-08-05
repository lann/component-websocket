//! The provider's configuration channel: environment variables, read at
//! each `connect` (the component reads nothing at instantiation, so an
//! embedder can adjust between connections).
//!
//! Defaults mirror the hosted implementations' documented defaults. See
//! the crate README for the variables and their formats.

use std::time::Duration;

/// Per-connection configuration snapshotted at `connect`.
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) connect_timeout: Duration,
    pub(crate) close_timeout: Duration,
    pub(crate) max_inbound_buffer_bytes: usize,
    /// PEM bundle of `wss:` trust anchors; `None` means `wss:` fails
    /// closed (there is no ambient root store in-guest).
    pub(crate) tls_roots_pem: Option<String>,
}

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_INBOUND_BUFFER_BYTES: usize = 8 * 1024 * 1024;

impl Config {
    pub(crate) fn from_env() -> Config {
        Config {
            connect_timeout: millis_var("LANN_WEBSOCKET_CONNECT_TIMEOUT_MS")
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            close_timeout: millis_var("LANN_WEBSOCKET_CLOSE_TIMEOUT_MS")
                .unwrap_or(DEFAULT_CLOSE_TIMEOUT),
            max_inbound_buffer_bytes: usize_var("LANN_WEBSOCKET_MAX_INBOUND_BUFFER_BYTES")
                .unwrap_or(DEFAULT_MAX_INBOUND_BUFFER_BYTES),
            tls_roots_pem: std::env::var("LANN_WEBSOCKET_TLS_ROOTS_PEM").ok(),
        }
    }
}

/// A malformed value is a deployment bug; fall back to the default rather
/// than guessing at intent.
fn millis_var(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

fn usize_var(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse::<usize>().ok()
}
