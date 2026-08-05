//! The per-connection pump: shared state between the resource methods and
//! two cooperating tasks (a reader owning the inbound half, a command
//! task owning sends and local close).
//!
//! The WebSocket protocol itself — framing, masking, fragmentation,
//! UTF-8 validation, ping/pong, the close-frame exchange — is
//! tungstenite's sans-IO core over [`VirtualIo`](crate::io), the same
//! protocol crate the reference host implementation uses, so wire
//! behavior matches by construction. This file owns what tungstenite
//! does not: the inbound budget and its overflow contract, the close
//! deadline, write-side death, and the latched `wait-closed` value —
//! ported from the reference pump (`rust/wasmtime/src/websocket.rs`) and
//! gated by the conformance suite.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::WebSocket;

use crate::bindings::lann::websocket::types::{CloseInfo, Error, Message};
use crate::bindings::wasi::clocks::monotonic_clock as clock;
use crate::io::{IoHandle, VirtualIo};
use crate::Flag;

/// Connection state, forward-only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WsState {
    Open,
    Closing,
    Closed,
}

/// A forward-only state cell: regressions are ignored, `Closed` latches.
pub(crate) struct StateCell(Cell<WsState>);

impl StateCell {
    fn new() -> StateCell {
        StateCell(Cell::new(WsState::Open))
    }

    pub(crate) fn get(&self) -> WsState {
        self.0.get()
    }

    pub(crate) fn advance(&self, next: WsState) {
        let rank = |s: WsState| match s {
            WsState::Open => 0,
            WsState::Closing => 1,
            WsState::Closed => 2,
        };
        if rank(next) > rank(self.0.get()) {
            self.0.set(next);
        }
    }
}

/// A command from the resource surface into the pump.
pub(crate) enum Cmd {
    Send {
        message: Message,
        ack: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        code: Option<u16>,
        reason: String,
    },
    /// `receive-via-stream` claimed the inbound stream: spawn its feeder
    /// from the command task's async context (the claiming export is
    /// synchronous and has none).
    ClaimStream {
        feeder: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
    },
}

/// State shared between the resource methods and the pump tasks.
pub(crate) struct Shared {
    pub(crate) state: StateCell,
    /// Fired by `close()`/drop: local close is observed immediately.
    pub(crate) local_closed: Flag,
    /// Fired when `receive-via-stream` claims the inbound stream.
    pub(crate) stream_started: Flag,
    pub(crate) stream_claimed: Cell<bool>,
    /// Fired at pump finalization, after `close_info` is latched.
    pub(crate) closed: Flag,
    /// Latched at finalization: `Some(peer frame)` or `Some(None)` for an
    /// abnormal close; outer `None` means not terminal yet.
    pub(crate) close_info: RefCell<Option<Option<CloseInfo>>>,
    pub(crate) cmd_tx: mpsc::UnboundedSender<Cmd>,
    /// The inbound queue: receivers serialize on the lock and take
    /// messages in arrival order.
    pub(crate) queue: futures::lock::Mutex<mpsc::UnboundedReceiver<Message>>,
    budget_used: Cell<usize>,
    overflowed: Cell<bool>,
    budget_limit: usize,
}

impl Shared {
    pub(crate) fn new(
        cmd_tx: mpsc::UnboundedSender<Cmd>,
        in_rx: mpsc::UnboundedReceiver<Message>,
        budget_limit: usize,
    ) -> Shared {
        Shared {
            state: StateCell::new(),
            local_closed: Flag::new(),
            stream_started: Flag::new(),
            stream_claimed: Cell::new(false),
            closed: Flag::new(),
            close_info: RefCell::new(None),
            cmd_tx,
            queue: futures::lock::Mutex::new(in_rx),
            budget_used: Cell::new(0),
            overflowed: Cell::new(false),
            budget_limit,
        }
    }

    /// Reserve budget for an inbound message: `false` latches overflow.
    /// A message exactly filling the budget fits.
    fn reserve(&self, len: usize) -> bool {
        if self.overflowed.get() {
            return false;
        }
        if self.budget_used.get() + len > self.budget_limit {
            self.overflowed.set(true);
            return false;
        }
        self.budget_used.set(self.budget_used.get() + len);
        true
    }

    pub(crate) fn release_budget(&self, message: &Message) {
        let len = match message {
            Message::Binary(bytes) => bytes.len(),
            Message::String(text) => text.len(),
        };
        self.budget_used
            .set(self.budget_used.get().saturating_sub(len));
    }

    /// Map the queue's next item: backlog first; a drained queue reports
    /// overflow (if latched) over closed.
    pub(crate) fn map_queue_end(&self, item: Option<Message>) -> Result<Message, Error> {
        match item {
            Some(message) => {
                self.release_budget(&message);
                Ok(message)
            }
            None if self.overflowed.get() => Err(Error::ReceiveBufferOverflow),
            None => Err(Error::Closed),
        }
    }
}

/// Mutable pump-side flags shared by the reader and command tasks.
struct PumpFlags {
    wire_closing: Cell<bool>,
    write_dead: Cell<bool>,
    /// The close deadline (a monotonic mark), armed at most once.
    deadline: Cell<Option<u64>>,
    deadline_armed: Flag,
    /// Fired at reader finalization: the command task fails late sends.
    pump_done: Flag,
}

impl PumpFlags {
    fn arm_deadline(&self, close_timeout_ns: u64) {
        if self.deadline.get().is_none() {
            self.deadline.set(Some(clock::now() + close_timeout_ns));
            self.deadline_armed.fire();
        }
    }
}

/// The transport handles whose drop tears the connection down. The
/// fields exist for ownership alone: dropping the struct closes the
/// socket and releases the TLS resources.
#[allow(dead_code)]
pub(crate) struct Transport {
    pub(crate) socket: Option<crate::bindings::wasi::sockets::types::TcpSocket>,
    pub(crate) tls: Option<TlsKeepalive>,
}

/// The TLS-side handles for a `wss:` connection (ownership keepalive).
#[allow(dead_code)]
pub(crate) struct TlsKeepalive {
    pub(crate) connector: crate::bindings::lann::tls::client::Connector,
}

/// Everything the pump needs at spawn time.
pub(crate) struct PumpArgs {
    pub(crate) shared: Rc<Shared>,
    pub(crate) in_tx: mpsc::UnboundedSender<Message>,
    pub(crate) cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    /// The post-handshake protocol state machine.
    pub(crate) websocket: WebSocket<VirtualIo>,
    /// The feed/drain handle backing `websocket`'s virtual transport.
    pub(crate) handle: IoHandle,
    pub(crate) reader: wit_bindgen::StreamReader<u8>,
    pub(crate) writer: wit_bindgen::StreamWriter<u8>,
    pub(crate) close_timeout_ns: u64,
    pub(crate) transport: Transport,
}

/// Spawn the pump: the reader task and the command task.
pub(crate) fn spawn_pump(args: PumpArgs) {
    let PumpArgs {
        shared,
        in_tx,
        cmd_rx,
        websocket,
        handle,
        reader,
        writer,
        close_timeout_ns,
        transport,
    } = args;

    let flags = Rc::new(PumpFlags {
        wire_closing: Cell::new(false),
        write_dead: Cell::new(false),
        deadline: Cell::new(None),
        deadline_armed: Flag::new(),
        pump_done: Flag::new(),
    });
    let proto = Rc::new(Proto {
        websocket: RefCell::new(websocket),
        handle,
        write_lock: futures::lock::Mutex::new(writer),
        flags: Rc::clone(&flags),
        shared: Rc::clone(&shared),
        close_timeout_ns,
    });

    {
        let flags = Rc::clone(&flags);
        let proto = Rc::clone(&proto);
        wit_bindgen::spawn_local(cmd_task(flags, proto, cmd_rx));
    }
    let transport = Rc::new(RefCell::new(Some(transport)));
    wit_bindgen::spawn_local(reader_task(
        shared,
        flags,
        proto,
        in_tx,
        reader,
        close_timeout_ns,
        transport,
    ));
}

/// The protocol state machine plus the bounded path from it to the wit
/// outbound stream. Borrows of `websocket` are never held across an
/// await: tungstenite writes into the virtual transport synchronously,
/// and the buffered bytes are flushed to the stream afterwards.
struct Proto {
    websocket: RefCell<WebSocket<VirtualIo>>,
    handle: IoHandle,
    write_lock: futures::lock::Mutex<wit_bindgen::StreamWriter<u8>>,
    flags: Rc<PumpFlags>,
    shared: Rc<Shared>,
    close_timeout_ns: u64,
}

impl Proto {
    /// Flush everything tungstenite has buffered to the wit stream,
    /// bounded: the write races the close deadline (arming it when a
    /// local close lands mid-write); a stalled or failed write marks the
    /// write side dead — the pump then only reads.
    async fn flush_outbound(&self) -> Result<(), Error> {
        let mut writer = self.write_lock.lock().await;
        let bytes = self.handle.drain_outbound();
        if bytes.is_empty() {
            return if self.flags.write_dead.get() {
                Err(Error::Closed)
            } else {
                Ok(())
            };
        }
        if self.flags.write_dead.get() {
            return Err(Error::Closed);
        }
        let bound = async {
            if self.flags.deadline.get().is_none() {
                self.shared.local_closed.wait().await;
                self.flags.arm_deadline(self.close_timeout_ns);
            }
            let Some(deadline) = self.flags.deadline.get() else {
                std::future::pending::<()>().await;
                return;
            };
            clock::wait_until(deadline).await;
        };
        futures::pin_mut!(bound);
        let write = writer.write_all(bytes);
        futures::pin_mut!(write);
        futures::select_biased! {
            leftover = write.fuse() => {
                if leftover.is_empty() {
                    Ok(())
                } else {
                    self.flags.write_dead.set(true);
                    self.flags.arm_deadline(self.close_timeout_ns);
                    Err(Error::Closed)
                }
            }
            () = bound.fuse() => {
                self.flags.write_dead.set(true);
                self.flags.arm_deadline(self.close_timeout_ns);
                Err(Error::Closed)
            }
        }
    }

    /// Send one message: hand it to tungstenite, then flush. Error
    /// mapping mirrors the reference host (`map_write_err`).
    async fn send(&self, message: Message) -> Result<(), Error> {
        let ws_message = match message {
            Message::Binary(bytes) => tungstenite::Message::Binary(bytes.into()),
            Message::String(text) => tungstenite::Message::Text(text.into()),
        };
        let sent = self.websocket.borrow_mut().send(ws_message);
        match sent {
            Ok(()) => self.flush_outbound().await,
            Err(err) => Err(map_write_err(err)),
        }
    }

    /// Begin the closing handshake toward the peer (idempotent): arm the
    /// deadline, queue the close frame, flush.
    async fn begin_close(&self, frame: Option<CloseFrame>) {
        if self.flags.wire_closing.replace(true) {
            return;
        }
        self.flags.arm_deadline(self.close_timeout_ns);
        let closed = self.websocket.borrow_mut().close(frame);
        if closed.is_ok() {
            let _ = self.flush_outbound().await;
        }
    }
}

/// The reference host's write-error taxonomy.
fn map_write_err(err: tungstenite::Error) -> Error {
    use tungstenite::error::ProtocolError;
    match err {
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Protocol(ProtocolError::SendAfterClosing) => Error::Closed,
        other => Error::Other(other.to_string()),
    }
}

async fn cmd_task(
    flags: Rc<PumpFlags>,
    proto: Rc<Proto>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    // Phase 1: the connection is live — sends write, close closes.
    loop {
        let cmd = futures::select_biased! {
            () = flags.pump_done.wait() => break,
            cmd = cmd_rx.next() => cmd,
        };
        let Some(cmd) = cmd else {
            // All resource handles dropped: drop implies close(none, "").
            proto.begin_close(None).await;
            return;
        };
        match cmd {
            Cmd::Send { message, ack } => {
                if flags.wire_closing.get() || flags.write_dead.get() {
                    let _ = ack.send(Err(Error::Closed));
                    continue;
                }
                let _ = ack.send(proto.send(message).await);
            }
            Cmd::Close { code, reason } => {
                let frame = code.map(|code| CloseFrame {
                    code: CloseCode::from(code),
                    reason: reason.into(),
                });
                proto.begin_close(frame).await;
            }
            Cmd::ClaimStream { feeder } => {
                wit_bindgen::spawn_local(feeder);
            }
        }
    }
    // Phase 2: the pump has finalized. Sends fail, closes are no-ops, but
    // a late stream claim still gets its feeder spawned (the stream must
    // drain the backlog after a remote close). Exits when every resource
    // handle is gone.
    while let Some(cmd) = cmd_rx.next().await {
        match cmd {
            Cmd::Send { ack, .. } => {
                let _ = ack.send(Err(Error::Closed));
            }
            Cmd::Close { .. } => {}
            Cmd::ClaimStream { feeder } => {
                wit_bindgen::spawn_local(feeder);
            }
        }
    }
}

async fn reader_task(
    shared: Rc<Shared>,
    flags: Rc<PumpFlags>,
    proto: Rc<Proto>,
    in_tx: mpsc::UnboundedSender<Message>,
    mut reader: wit_bindgen::StreamReader<u8>,
    close_timeout_ns: u64,
    transport: Rc<RefCell<Option<Transport>>>,
) {
    let mut peer_frame: Option<CloseInfo> = None;

    'outer: loop {
        // Drain every message tungstenite can produce from the bytes fed
        // so far. Borrows of the state machine end before any await.
        loop {
            let step = proto.websocket.borrow_mut().read();
            match step {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    deliver(&shared, &flags, &in_tx, Message::Binary(bytes.into())).await;
                    if shared.overflowed.get() && !flags.wire_closing.get() {
                        proto.begin_close(None).await;
                    }
                }
                Ok(tungstenite::Message::Text(text)) => {
                    deliver(
                        &shared,
                        &flags,
                        &in_tx,
                        Message::String(text.as_str().to_string()),
                    )
                    .await;
                    if shared.overflowed.get() && !flags.wire_closing.get() {
                        proto.begin_close(None).await;
                    }
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    if peer_frame.is_none() {
                        peer_frame = Some(match &frame {
                            Some(frame) => CloseInfo {
                                code: frame.code.into(),
                                reason: frame.reason.as_str().to_string(),
                            },
                            // A close frame with no body reads as 1005
                            // with an empty reason.
                            None => CloseInfo {
                                code: 1005,
                                reason: String::new(),
                            },
                        });
                    }
                    shared.state.advance(WsState::Closing);
                    flags.arm_deadline(close_timeout_ns);
                    // tungstenite queued the reply (completing the
                    // handshake from our side) in the same read.
                    flags.wire_closing.set(true);
                    let _ = proto.flush_outbound().await;
                }
                // Ping/pong are handled inside tungstenite (the pong is
                // queued on read); surface nothing, flush the reply.
                Ok(tungstenite::Message::Ping(_)) | Ok(tungstenite::Message::Pong(_)) => {
                    let _ = proto.flush_outbound().await;
                }
                Ok(tungstenite::Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(err))
                    if err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                // A message past the transport cap: latch overflow and
                // tear down at once (the read stream is compromised).
                Err(tungstenite::Error::Capacity(_)) => {
                    shared.overflowed.set(true);
                    proto.begin_close(None).await;
                    break 'outer;
                }
                // The close handshake completed.
                Err(tungstenite::Error::ConnectionClosed) => break 'outer,
                // Everything else (protocol violation, invalid UTF-8, a
                // real transport error) is an abnormal end.
                Err(_) => break 'outer,
            }
        }

        // Read more, racing the close deadline: an armed deadline that
        // expires tears the connection down whatever the peer is doing
        // (the closing procedure is bounded end to end).
        let deadline_expired = async {
            flags.deadline_armed.wait().await;
            let Some(deadline) = flags.deadline.get() else {
                std::future::pending::<()>().await;
                return;
            };
            clock::wait_until(deadline).await;
        };
        futures::pin_mut!(deadline_expired);
        let read = reader.read(Vec::with_capacity(16 * 1024));
        futures::pin_mut!(read);
        let (status, chunk) = futures::select_biased! {
            () = deadline_expired.fuse() => break,
            result = read.fuse() => result,
        };
        proto.handle.feed(&chunk);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) && chunk.is_empty()
        {
            proto.handle.set_eof();
            // One final drain so a close frame arriving with EOF still
            // registers before finalization.
            loop {
                let step = proto.websocket.borrow_mut().read();
                match step {
                    Ok(tungstenite::Message::Close(frame)) => {
                        if peer_frame.is_none() {
                            peer_frame = Some(match &frame {
                                Some(frame) => CloseInfo {
                                    code: frame.code.into(),
                                    reason: frame.reason.as_str().to_string(),
                                },
                                None => CloseInfo {
                                    code: 1005,
                                    reason: String::new(),
                                },
                            });
                        }
                        break;
                    }
                    Ok(tungstenite::Message::Binary(bytes)) => {
                        deliver(&shared, &flags, &in_tx, Message::Binary(bytes.into())).await;
                    }
                    Ok(tungstenite::Message::Text(text)) => {
                        deliver(
                            &shared,
                            &flags,
                            &in_tx,
                            Message::String(text.as_str().to_string()),
                        )
                        .await;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            break;
        }
    }

    // Finalization: latch close details, end the queue after the backlog,
    // stop accepting sends, tear the transport down, wake wait-closed.
    *shared.close_info.borrow_mut() = Some(peer_frame);
    shared.state.advance(WsState::Closed);
    drop(in_tx);
    flags.pump_done.fire();
    drop(transport.borrow_mut().take());
    shared.closed.fire();
}

/// Deliver one complete inbound message under the budget. On overflow the
/// message is discarded and the latch is set; the caller closes toward
/// the peer.
async fn deliver(
    shared: &Rc<Shared>,
    flags: &Rc<PumpFlags>,
    in_tx: &mpsc::UnboundedSender<Message>,
    message: Message,
) {
    if flags.wire_closing.get() {
        // Messages arriving during the closing handshake are discarded.
        return;
    }
    let len = match &message {
        Message::Binary(bytes) => bytes.len(),
        Message::String(text) => text.len(),
    };
    if shared.reserve(len) {
        let _ = in_tx.unbounded_send(message);
    }
}
