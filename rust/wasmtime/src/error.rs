//! The host-side error taxonomy, mirroring the WIT `types.error` variant.

/// A failure surfaced to the guest as the WIT `types.error` variant.
///
/// The `String` payloads are human-readable diagnostics; per the WIT error
/// contract guests never match on their contents.
#[derive(Clone, Debug)]
pub enum WebsocketError {
    /// The supplied URL is not an absolute `ws:`/`wss:` URL without a
    /// fragment.
    InvalidUrl(String),
    /// The connection attempt failed (resolution, TCP, TLS, or the upgrade
    /// handshake).
    ConnectFailed(String),
    /// The connection was closed before the operation completed.
    Closed,
    /// The inbound messages are claimed by `receive-via-stream`.
    ReceivingViaStream,
    /// The bounded inbound buffer overflowed.
    ReceiveBufferOverflow,
    /// A supplied argument is invalid; the operation had no effect.
    InvalidArgument(String),
    /// An implementation-specific failure.
    Other(String),
}

impl WebsocketError {
    /// An `Other` error from a displayable cause.
    pub(crate) fn other(err: impl std::fmt::Display) -> Self {
        Self::Other(err.to_string())
    }
}

impl std::fmt::Display for WebsocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(msg) => write!(f, "invalid url: {msg}"),
            Self::ConnectFailed(msg) => write!(f, "connect failed: {msg}"),
            Self::Closed => write!(f, "connection closed"),
            Self::ReceivingViaStream => write!(f, "inbound messages are claimed by a stream"),
            Self::ReceiveBufferOverflow => write!(f, "inbound buffer overflowed"),
            Self::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WebsocketError {}

/// Result alias for host-side WebSocket operations.
pub type WebsocketResult<T> = Result<T, WebsocketError>;
