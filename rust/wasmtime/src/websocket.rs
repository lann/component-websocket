//! The tokio-tungstenite-backed [`Websocket`] host resource.
//!
//! Each connection is driven by a single **pump task** that owns the
//! `tokio_tungstenite::WebSocketStream`: outbound work arrives as [`Cmd`]s
//! over an unbounded channel (so the synchronous `close` can enqueue without
//! awaiting), inbound messages flow into a budget-bounded queue readers
//! drain. Keeping reads and writes in one task lets tungstenite's automatic
//! ping/pong and close-frame replies flush deterministically.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::channel::{mpsc as fmpsc, oneshot};
use futures::future::Shared;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};

use crate::error::{WebsocketError, WebsocketResult};

/// The default bound on inbound payload bytes buffered per connection while
/// waiting for the guest to `receive` them.
///
/// There is no wire-level inbound backpressure (the WIT contract deliberately
/// matches the W3C `WebSocket` floor, where none is possible), so this bound
/// is what protects host memory from a slow guest reader: when it would be
/// exceeded the connection is closed and, once the buffered backlog drains,
/// `receive` fails with `error.receive-buffer-overflow`. The value is the
/// 8 MiB convention the WIT inbound-buffering contract documents. Embedders
/// override it per context through
/// [`WasiWebsocketCtx::set_max_inbound_buffer_bytes`](crate::WasiWebsocketCtx::set_max_inbound_buffer_bytes).
pub const DEFAULT_MAX_INBOUND_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// The default bound on how long `connect` waits for the handshake.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The default bound on how long a locally initiated close waits for the
/// peer to complete the closing handshake before the transport is torn down
/// anyway (and on any single stalled transport write once closing).
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// The contents of a received close frame, mirroring the WIT `close-info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseInfo {
    /// The close code, or 1005 if the frame carried none.
    pub code: u16,
    /// The close reason; empty if none.
    pub reason: String,
}

/// The connection lifecycle as observed by the host, backing
/// `websocket.state` (mapped onto the WIT `websocket-state` at the binding
/// layer). `Closed` is terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WsState {
    Open,
    Closing,
    Closed,
}

/// The lifecycle cell: state only ever advances (`Open` -> `Closing` ->
/// `Closed`), and `Closed` latches.
pub(crate) struct StateCell(Mutex<WsState>);

impl StateCell {
    fn new() -> Self {
        Self(Mutex::new(WsState::Open))
    }

    /// Advance to `next` if that is forward progress; regressions and
    /// post-terminal transitions are ignored.
    pub(crate) fn advance(&self, next: WsState) {
        let mut state = self.0.lock().unwrap();
        let allowed = matches!(
            (*state, next),
            (WsState::Open, WsState::Closing)
                | (WsState::Open, WsState::Closed)
                | (WsState::Closing, WsState::Closed)
        );
        if allowed {
            *state = next;
        }
    }

    pub(crate) fn get(&self) -> WsState {
        *self.0.lock().unwrap()
    }
}

/// A single WebSocket data message payload, direction-neutral: the pump
/// sends them outbound and the inbound queue delivers them.
pub(crate) enum Payload {
    /// A text message (valid UTF-8, validated by tungstenite).
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
}

impl Payload {
    pub(crate) fn payload_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(data) => data.len(),
        }
    }
}

/// The buffered-byte accounting shared between the pump (which reserves
/// capacity for each inbound message) and readers (which release it as
/// messages are consumed).
pub(crate) struct InboundBudget {
    limit: usize,
    buffered: AtomicUsize,
    /// Latched once an inbound message would have exceeded the bound.
    overflowed: AtomicBool,
}

impl InboundBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            buffered: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
        }
    }

    /// Reserve `len` buffered bytes. Returns `false` — latching the overflow
    /// — if the reservation would exceed the bound or an overflow was
    /// already latched.
    fn reserve(&self, len: usize) -> bool {
        if self.overflowed.load(Ordering::SeqCst) {
            return false;
        }
        if self.buffered.load(Ordering::SeqCst).saturating_add(len) > self.limit {
            self.overflowed.store(true, Ordering::SeqCst);
            return false;
        }
        self.buffered.fetch_add(len, Ordering::SeqCst);
        true
    }

    fn release(&self, len: usize) {
        self.buffered.fetch_sub(len, Ordering::SeqCst);
    }

    fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::SeqCst)
    }

    /// Latch the overflow directly (the transport rejected a message past
    /// its cap before the budget could account it).
    fn latch_overflow(&self) {
        self.overflowed.store(true, Ordering::SeqCst);
    }
}

/// A connection's inbound-message queue: the receiving half of the pump's
/// message stream plus the shared [`InboundBudget`] its consumption releases.
pub(crate) struct InboundQueue {
    rx: fmpsc::UnboundedReceiver<Payload>,
    budget: Arc<InboundBudget>,
}

impl InboundQueue {
    /// The next buffered message, or `None` once the pump has stopped (the
    /// connection closed or its inbound buffer overflowed) and the backlog
    /// is drained. Releases the message's bytes from the budget.
    pub(crate) async fn next(&mut self) -> Option<Payload> {
        let message = self.rx.next().await?;
        self.budget.release(message.payload_len());
        Some(message)
    }

    /// Whether the connection's inbound buffer overflowed. When `true`, the
    /// queue ends after the pre-overflow backlog and readers should surface
    /// `error.receive-buffer-overflow` rather than `closed`.
    pub(crate) fn overflowed(&self) -> bool {
        self.budget.overflowed()
    }
}

/// The receiving half of an idempotent one-shot signal.
#[derive(Clone)]
pub(crate) struct Signal {
    flag: Arc<AtomicBool>,
    fired: Shared<oneshot::Receiver<()>>,
}

impl Signal {
    pub(crate) fn is_fired(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub(crate) fn fired(&self) -> Shared<oneshot::Receiver<()>> {
        self.fired.clone()
    }
}

/// The firing half of a [`Signal`]. Cloneable; idempotent.
#[derive(Clone)]
pub(crate) struct SignalTrigger {
    flag: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl SignalTrigger {
    pub(crate) fn fire(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

pub(crate) fn signal() -> (SignalTrigger, Signal) {
    let (tx, rx) = oneshot::channel();
    let flag = Arc::new(AtomicBool::new(false));
    (
        SignalTrigger {
            flag: flag.clone(),
            tx: Arc::new(Mutex::new(Some(tx))),
        },
        Signal {
            flag,
            fired: rx.shared(),
        },
    )
}

/// Outbound work handed to the pump task.
enum Cmd {
    Send {
        message: Payload,
        ack: oneshot::Sender<WebsocketResult<()>>,
    },
    Close {
        code: Option<u16>,
        reason: String,
    },
}

/// State shared between the pump task and the resource's methods.
struct ConnShared {
    /// The lifecycle cell backing `websocket.state`.
    state: StateCell,
    /// The peer's close frame, if one was received. Set exactly once, before
    /// `closed` fires.
    close_info: OnceLock<Option<CloseInfo>>,
    /// Fired once the transport is fully torn down (`wait-closed` resolves).
    closed: Signal,
}

/// Host state behind a `websocket` resource: an open WebSocket client
/// connection.
pub struct Websocket {
    /// The negotiated subprotocol, or empty.
    protocol: String,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    shared: Arc<ConnShared>,
    /// Inbound messages, delivered one per `receive` call. Behind an async
    /// mutex so concurrent receivers serialize and each takes the next
    /// message.
    incoming: Arc<AsyncMutex<InboundQueue>>,
    /// Fired by a local `close()` (or the resource dropping): operations
    /// fail `error.closed` at once and the unread backlog is discarded. A
    /// *remote* or abnormal close deliberately does not fire this — its
    /// backlog stays deliverable and readers observe the end through the
    /// inbound queue draining.
    local_closed: Signal,
    local_close_trigger: SignalTrigger,
    /// Set once `receive-via-stream` has claimed the inbound messages.
    stream_receiving: Arc<AtomicBool>,
    /// Sender fired when `receive-via-stream` is first called: the first
    /// caller takes it (claiming the stream), later callers observe `None`.
    stream_started_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Resolves once `receive-via-stream` is called, so pending `receive`
    /// calls can be woken and fail with `receiving-via-stream`.
    stream_started: Shared<oneshot::Receiver<()>>,
}

/// Per-connection configuration snapshotted from the context at `connect`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnectConfig {
    pub(crate) connect_timeout: Duration,
    pub(crate) close_timeout: Duration,
    pub(crate) max_inbound_buffer_bytes: usize,
}

/// Validate a connect URL per the WIT contract: absolute `ws:`/`wss:`, no
/// fragment, no userinfo.
fn validate_url(url: &str) -> Result<(), String> {
    if url.contains('#') {
        return Err("URL must not have a fragment".to_string());
    }
    let uri: tungstenite::http::Uri = url
        .parse()
        .map_err(|err| format!("URL does not parse: {err}"))?;
    match uri.scheme_str() {
        Some("ws") | Some("wss") => {}
        Some(other) => return Err(format!("URL scheme must be ws or wss, not {other:?}")),
        None => return Err("URL must be absolute (ws: or wss:)".to_string()),
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err("URL must have a host".to_string());
    }
    // The WHATWG WebSocket constructor rejects credentials in the URL; the
    // eager taxonomy matches that floor uniformly.
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err("URL must not have userinfo".to_string());
    }
    Ok(())
}

/// Whether `token` is a valid RFC 6455 subprotocol token (an RFC 2616
/// `token`: 1+ US-ASCII characters, no separators or control characters).
fn is_valid_protocol_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b"\"(),/:;<=>?@[\\]{}".contains(&b))
}

/// Validate a subprotocol offer per the WIT contract: every entry a valid
/// token, no duplicates.
fn validate_protocols(protocols: &[String]) -> Result<(), String> {
    for (index, protocol) in protocols.iter().enumerate() {
        if !is_valid_protocol_token(protocol) {
            return Err(format!("subprotocol {protocol:?} is not a valid token"));
        }
        if protocols[..index].contains(protocol) {
            return Err(format!("subprotocol {protocol:?} is offered twice"));
        }
    }
    Ok(())
}

/// Validate close arguments per the WIT contract: `code` 1000 or 3000-4999,
/// `reason` at most 123 UTF-8 bytes and only alongside a code.
pub(crate) fn validate_close_args(code: Option<u16>, reason: &str) -> WebsocketResult<()> {
    if let Some(code) = code {
        if code != 1000 && !(3000..=4999).contains(&code) {
            return Err(WebsocketError::InvalidArgument(format!(
                "close code must be 1000 or in 3000-4999, not {code}"
            )));
        }
    } else if !reason.is_empty() {
        return Err(WebsocketError::InvalidArgument(
            "a close reason requires a close code".to_string(),
        ));
    }
    if reason.len() > 123 {
        return Err(WebsocketError::InvalidArgument(format!(
            "close reason must be at most 123 bytes, got {}",
            reason.len()
        )));
    }
    Ok(())
}

fn close_info_from(frame: Option<CloseFrame>) -> CloseInfo {
    match frame {
        Some(frame) => CloseInfo {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().to_owned(),
        },
        // A close frame with no body has no code; the WIT contract maps it
        // to 1005 ("no status received"), matching the browser.
        None => CloseInfo {
            code: 1005,
            reason: String::new(),
        },
    }
}

impl Websocket {
    /// Open a connection, run the handshake, and spawn the pump task.
    ///
    /// Must be called within a tokio runtime context (the pump task is
    /// spawned on it).
    pub(crate) async fn connect(
        url: String,
        protocols: Vec<String>,
        config: ConnectConfig,
    ) -> WebsocketResult<Websocket> {
        validate_url(&url).map_err(WebsocketError::InvalidUrl)?;
        validate_protocols(&protocols).map_err(WebsocketError::InvalidArgument)?;

        let mut request = url
            .clone()
            .into_client_request()
            .map_err(|err| WebsocketError::InvalidUrl(err.to_string()))?;
        if !protocols.is_empty() {
            let offer = protocols.join(", ");
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                offer.parse().map_err(|_| {
                    WebsocketError::InvalidArgument("malformed subprotocol offer".to_string())
                })?,
            );
        }

        // The transport's own message/frame caps scale with the configured
        // buffer bound instead of tungstenite's fixed defaults, so an
        // embedder raising the bound cannot make the transport reject
        // messages a browser-backed host would deliver (and the budget
        // would overflow-close). Messages in (bound, cap] take the normal
        // budget-overflow path; past the cap, the capacity error is mapped
        // onto the same overflow taxonomy in the pump.
        let transport_cap = config
            .max_inbound_buffer_bytes
            .saturating_mul(2)
            .max(64 * 1024 * 1024);
        let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(transport_cap))
            .max_frame_size(Some(transport_cap));

        let (ws, response) = match tokio::time::timeout(
            config.connect_timeout,
            tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => return Err(WebsocketError::ConnectFailed(err.to_string())),
            Err(_) => {
                return Err(WebsocketError::ConnectFailed(format!(
                    "handshake timed out after {:?}",
                    config.connect_timeout
                )))
            }
        };

        let negotiated = response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        // A non-empty offer binds the server, per the WIT contract (matching
        // the browser: offered-but-unselected fails the connection).
        if !protocols.is_empty() && !protocols.contains(&negotiated) {
            return Err(WebsocketError::ConnectFailed(if negotiated.is_empty() {
                "server selected no subprotocol although one was offered".to_string()
            } else {
                format!("server selected subprotocol {negotiated:?} which was not offered")
            }));
        }
        if protocols.is_empty() && !negotiated.is_empty() {
            return Err(WebsocketError::ConnectFailed(format!(
                "server selected subprotocol {negotiated:?} although none was offered"
            )));
        }

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = fmpsc::unbounded();
        let budget = Arc::new(InboundBudget::new(config.max_inbound_buffer_bytes));
        let (closed_trigger, closed) = signal();
        let shared = Arc::new(ConnShared {
            state: StateCell::new(),
            close_info: OnceLock::new(),
            closed,
        });
        let (local_close_trigger, local_closed) = signal();
        let (started_tx, started_rx) = oneshot::channel();

        let pump = Pump {
            in_tx,
            budget: budget.clone(),
            shared: shared.clone(),
            local_closed: local_closed.clone(),
            close_timeout: config.close_timeout,
            wire_closing: false,
            write_dead: false,
            peer_frame: None,
            deadline: None,
        };
        tokio::spawn(pump.run(ws, cmd_rx, closed_trigger));

        Ok(Websocket {
            protocol: negotiated,
            cmd_tx,
            shared,
            incoming: Arc::new(AsyncMutex::new(InboundQueue { rx: in_rx, budget })),
            local_closed,
            local_close_trigger,
            stream_receiving: Arc::new(AtomicBool::new(false)),
            stream_started_tx: Arc::new(Mutex::new(Some(started_tx))),
            stream_started: started_rx.shared(),
        })
    }

    /// The negotiated subprotocol, or empty.
    pub fn protocol(&self) -> String {
        self.protocol.clone()
    }

    /// Initiate the closing handshake (see the WIT `close` contract).
    pub(crate) fn close(&self, code: Option<u16>, reason: String) -> WebsocketResult<()> {
        validate_close_args(code, &reason)?;
        if self.local_closed.is_fired() {
            return Ok(());
        }
        self.shared.state.advance(WsState::Closing);
        self.local_close_trigger.fire();
        let _ = self.cmd_tx.send(Cmd::Close { code, reason });
        Ok(())
    }

    /// A cloneable handle for sending, usable without holding the resource
    /// borrow across an await.
    pub(crate) fn send_handle(&self) -> SendHandle {
        SendHandle {
            cmd_tx: self.cmd_tx.clone(),
            local_closed: self.local_closed.clone(),
        }
    }

    /// The shared inbound queue.
    pub(crate) fn incoming(&self) -> Arc<AsyncMutex<InboundQueue>> {
        self.incoming.clone()
    }

    /// The local-close signal (see the field docs).
    pub(crate) fn local_closed(&self) -> Signal {
        self.local_closed.clone()
    }

    /// Resolve once the transport is torn down, with the peer's close frame
    /// if any (the `wait-closed` contract).
    pub(crate) fn closed_handle(&self) -> ClosedHandle {
        ClosedHandle(self.shared.clone())
    }

    /// The connection's current lifecycle state (`websocket.state`).
    pub(crate) fn state(&self) -> WsState {
        self.shared.state.get()
    }

    /// Claim the inbound messages for `receive-via-stream`: `true` for the
    /// first caller, which also wakes pending `receive` calls.
    pub(crate) fn begin_stream_receiving(&self) -> bool {
        let mut guard = self.stream_started_tx.lock().unwrap();
        match guard.take() {
            Some(tx) => {
                self.stream_receiving.store(true, Ordering::SeqCst);
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// Whether `receive-via-stream` has claimed the inbound messages.
    pub(crate) fn is_stream_receiving(&self) -> bool {
        self.stream_receiving.load(Ordering::SeqCst)
    }

    /// A future resolving once `receive-via-stream` is called.
    pub(crate) fn stream_started(&self) -> Shared<oneshot::Receiver<()>> {
        self.stream_started.clone()
    }
}

/// A cloneable sending handle: the pump's command channel plus the
/// local-close signal for the fast-fail path.
#[derive(Clone)]
pub(crate) struct SendHandle {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    local_closed: Signal,
}

impl SendHandle {
    /// Hand one message to the pump, resolving once it reaches the
    /// transport.
    pub(crate) async fn send(&self, message: Payload) -> WebsocketResult<()> {
        if self.local_closed.is_fired() {
            return Err(WebsocketError::Closed);
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Cmd::Send {
                message,
                ack: ack_tx,
            })
            .is_err()
        {
            return Err(WebsocketError::Closed);
        }
        // A dropped ack means the pump exited before the send was written.
        match ack_rx.await {
            Ok(result) => result,
            Err(_) => Err(WebsocketError::Closed),
        }
    }
}

/// A cloneable handle resolving `wait-closed`.
#[derive(Clone)]
pub(crate) struct ClosedHandle(Arc<ConnShared>);

impl ClosedHandle {
    /// Resolve once the transport is torn down, with the peer's close frame
    /// if any.
    pub(crate) async fn wait(self) -> Option<CloseInfo> {
        let _ = self.0.closed.fired().await;
        self.0.close_info.get().cloned().flatten()
    }
}

impl Drop for Websocket {
    fn drop(&mut self) {
        // Dropping the resource without calling `close` implies
        // `close(none, "")`, per the WIT contract.
        let _ = self.close(None, String::new());
    }
}

/// Await the next inbound message from the shared queue, mapping the queue's
/// end onto the WIT error taxonomy.
pub(crate) async fn next_inbound(
    incoming: Arc<AsyncMutex<InboundQueue>>,
) -> WebsocketResult<Payload> {
    let mut queue = incoming.lock().await;
    match queue.next().await {
        Some(message) => Ok(message),
        None if queue.overflowed() => Err(WebsocketError::ReceiveBufferOverflow),
        None => Err(WebsocketError::Closed),
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The pump task's per-connection state, minus the socket itself (which
/// [`Pump::run`] keeps as a local so the select's read future and the arm
/// bodies borrow disjoint values).
///
/// Every transport write is bounded: while the connection is open a write
/// races the local-close signal (so a stalled transport can never block the
/// closing procedure), and once closing begins every write races the close
/// deadline. A failed or stalled write marks the write half dead — the pump
/// then keeps *reading* until the deadline, so a close frame the peer
/// already sent is still captured rather than misreported as an abnormal
/// closure.
struct Pump {
    in_tx: fmpsc::UnboundedSender<Payload>,
    budget: Arc<InboundBudget>,
    shared: Arc<ConnShared>,
    /// Fired by a local `close()` (or drop); bounds in-flight writes the
    /// moment a close is requested.
    local_closed: Signal,
    close_timeout: Duration,
    /// A close frame has been sent or received: sends fail `closed` and
    /// inbound data messages are discarded from this point on.
    wire_closing: bool,
    /// The write half failed or stalled out: nothing further is written,
    /// and the pump keeps reading until the deadline tears it down.
    write_dead: bool,
    /// The peer's close frame, if one was received.
    peer_frame: Option<CloseInfo>,
    /// Once closing, bounds how long teardown may take.
    deadline: Option<tokio::time::Instant>,
}

impl Pump {
    /// Own the socket, serialize outbound work, and feed the inbound
    /// queue until the connection is torn down. See the module docs.
    async fn run(
        mut self,
        mut ws: Ws,
        mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
        closed_trigger: SignalTrigger,
    ) {
        // Set once `cmd_rx` has returned `None` (the resource dropped), so
        // the exhausted channel is not polled again.
        let mut cmds_done = false;

        loop {
            // A local copy so the deadline arm does not borrow `self` while
            // the arm bodies mutate it.
            let deadline = self.deadline;
            tokio::select! {
                biased;
                cmd = cmd_rx.recv(), if !cmds_done => match cmd {
                    Some(Cmd::Send { message, ack }) => {
                        if self.wire_closing || self.write_dead {
                            let _ = ack.send(Err(WebsocketError::Closed));
                            continue;
                        }
                        let msg = match message {
                            Payload::Text(text) => WsMessage::text(text),
                            Payload::Binary(data) => WsMessage::binary(data),
                        };
                        let result = self.bounded_write(ws.send(msg)).await;
                        let _ = ack.send(result);
                    }
                    Some(Cmd::Close { code, reason }) => {
                        let frame = code.map(|code| CloseFrame {
                            code: code.into(),
                            reason: reason.into(),
                        });
                        self.begin_close(&mut ws, frame).await;
                    }
                    None => {
                        // The resource dropped: drop-implies-close.
                        cmds_done = true;
                        self.begin_close(&mut ws, None).await;
                    }
                },
                msg = ws.next() => match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        self.deliver(&mut ws, Payload::Text(text.as_str().to_owned())).await;
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        self.deliver(&mut ws, Payload::Binary(data.to_vec())).await;
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        if self.peer_frame.is_none() {
                            self.peer_frame = Some(close_info_from(frame));
                        }
                        self.shared.state.advance(WsState::Closing);
                        self.arm_deadline();
                        if !self.wire_closing {
                            self.wire_closing = true;
                            // tungstenite queues the close reply
                            // automatically; flush it (bounded) so the
                            // handshake completes.
                            let _ = self.bounded_write(ws.flush()).await;
                        }
                    }
                    Some(Ok(WsMessage::Ping(_))) => {
                        // tungstenite queues the pong automatically; flush it.
                        let _ = self.bounded_write(ws.flush()).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(tungstenite::Error::Capacity(_))) => {
                        // The transport rejected a message past its cap
                        // (which scales above the buffer bound), so the
                        // guest-observable outcome is the overflow
                        // contract, same as a message the budget rejected.
                        // The read stream is compromised mid-frame; close
                        // toward the peer and tear down rather than keep
                        // reading.
                        self.budget.latch_overflow();
                        self.begin_close(&mut ws, None).await;
                        break;
                    }
                    // A read error or EOF is the transport's verdict either
                    // way; `peer_frame` already records whether a close
                    // frame arrived first.
                    Some(Err(_)) | None => break,
                },
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    // The closing procedure did not complete within the
                    // bound; tear the transport down.
                    break;
                }
            }
        }

        // Finalize: publish the close outcome, end the inbound queue
        // (readers drain the backlog first), and fail any queued sends.
        let _ = self.shared.close_info.set(self.peer_frame);
        drop(self.in_tx);
        cmd_rx.close();
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let Cmd::Send { ack, .. } = cmd {
                let _ = ack.send(Err(WebsocketError::Closed));
            }
        }
        self.shared.state.advance(WsState::Closing);
        self.shared.state.advance(WsState::Closed);
        closed_trigger.fire();
    }

    /// Arm the closing deadline (idempotent).
    fn arm_deadline(&mut self) {
        if self.deadline.is_none() {
            self.deadline = Some(tokio::time::Instant::now() + self.close_timeout);
        }
    }

    /// Drive one transport write to completion, bounded: by the close
    /// deadline once closing has begun, and otherwise by the local-close
    /// signal arming that deadline mid-write. A failure or stall marks the
    /// write half dead and arms the deadline, so the connection still
    /// reaches `closed` within the bound.
    async fn bounded_write(
        &mut self,
        io: impl std::future::Future<Output = Result<(), tungstenite::Error>>,
    ) -> WebsocketResult<()> {
        if self.write_dead {
            return Err(WebsocketError::Closed);
        }
        let mut io = std::pin::pin!(io);
        let result = match self.deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, &mut io).await {
                Ok(result) => result.map_err(map_write_err),
                Err(_) => Err(WebsocketError::Closed),
            },
            None => {
                let fired = self.local_closed.fired();
                let raced = tokio::select! {
                    result = &mut io => Some(result),
                    _ = fired => None,
                };
                match raced {
                    Some(result) => result.map_err(map_write_err),
                    None => {
                        // A close was requested mid-write: finish the flush
                        // within the closing bound.
                        self.arm_deadline();
                        match tokio::time::timeout_at(self.deadline.unwrap(), &mut io).await {
                            Ok(result) => result.map_err(map_write_err),
                            Err(_) => Err(WebsocketError::Closed),
                        }
                    }
                }
            }
        };
        if result.is_err() {
            self.write_dead = true;
            self.arm_deadline();
        }
        result
    }

    /// Initiate the closing handshake toward the peer (idempotent): arm the
    /// deadline first, then send the close frame within it.
    async fn begin_close(&mut self, ws: &mut Ws, frame: Option<CloseFrame>) {
        if self.wire_closing {
            return;
        }
        self.wire_closing = true;
        self.shared.state.advance(WsState::Closing);
        self.arm_deadline();
        let _ = self.bounded_write(ws.send(WsMessage::Close(frame))).await;
    }

    /// Queue one inbound data message, enforcing the buffer budget: on
    /// overflow, latch it, initiate a close toward the peer, and discard
    /// the message. Readers drain the pre-overflow backlog and then surface
    /// `error.receive-buffer-overflow`.
    async fn deliver(&mut self, ws: &mut Ws, message: Payload) {
        // Data arriving during a closing handshake is discarded, per the
        // WIT close contract.
        if self.wire_closing {
            return;
        }
        if self.budget.reserve(message.payload_len()) {
            let _ = self.in_tx.unbounded_send(message);
            return;
        }
        self.begin_close(ws, None).await;
    }
}

/// Classify a transport write failure into the WIT error taxonomy.
fn map_write_err(err: tungstenite::Error) -> WebsocketError {
    match err {
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Protocol(tungstenite::error::ProtocolError::SendAfterClosing) => {
            WebsocketError::Closed
        }
        other => WebsocketError::other(other),
    }
}
