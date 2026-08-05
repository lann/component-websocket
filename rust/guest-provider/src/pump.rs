//! The per-connection pump: shared state between the resource methods and
//! two cooperating tasks (a reader owning the inbound half, a command
//! task owning sends and local close), plus a deadline watchdog.
//!
//! Semantics are a port of the reference host pump
//! (`rust/wasmtime/src/websocket.rs`); the conformance suite gates the
//! match. Notable mirrored behaviors: overflow latches and discards the
//! offending message, closes toward the peer with a code-less frame, and
//! keeps reading until the handshake completes or the close deadline
//! fires; a stalled write marks the write side dead and arms the
//! deadline; `wait-closed` reports the peer's close frame or `none`,
//! never an invented frame.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};

use crate::bindings::exports::lann::websocket::connections::{CloseInfo, Error, Message};
use crate::bindings::wasi::clocks::monotonic_clock as clock;
use crate::frame::{
    build_frame, close_payload, parse_close_payload, Parsed, Parser, OP_BINARY, OP_CLOSE,
    OP_CONTINUATION, OP_PING, OP_PONG, OP_TEXT,
};
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
    /// Fired at reader finalization: the command task drains and exits.
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
    pub(crate) reader: wit_bindgen::StreamReader<u8>,
    pub(crate) writer: wit_bindgen::StreamWriter<u8>,
    /// Bytes past the handshake response: the start of frame data.
    pub(crate) leftover: Vec<u8>,
    pub(crate) close_timeout_ns: u64,
    /// The transport cap: a single message past it takes the
    /// immediate-teardown overflow path.
    pub(crate) max_frame_bytes: usize,
    pub(crate) transport: Transport,
}

/// Spawn the pump: reader task, command task, deadline watchdog.
pub(crate) fn spawn_pump(args: PumpArgs) {
    let PumpArgs {
        shared,
        in_tx,
        cmd_rx,
        reader,
        writer,
        leftover,
        close_timeout_ns,
        max_frame_bytes,
        transport,
    } = args;

    let flags = Rc::new(PumpFlags {
        wire_closing: Cell::new(false),
        write_dead: Cell::new(false),
        deadline: Cell::new(None),
        deadline_armed: Flag::new(),
        pump_done: Flag::new(),
    });
    let writer = Rc::new(WriteHalf {
        writer: futures::lock::Mutex::new(writer),
        flags: Rc::clone(&flags),
        local_closed_view: Rc::clone(&shared),
        close_timeout_ns,
    });
    let transport = Rc::new(RefCell::new(Some(transport)));

    // Command task: sends and local close, in order.
    {
        let flags = Rc::clone(&flags);
        let writer = Rc::clone(&writer);
        wit_bindgen::spawn_local(cmd_task(flags, writer, cmd_rx, close_timeout_ns));
    }

    // Reader task: frames in, close semantics, finalization.
    wit_bindgen::spawn_local(reader_task(
        shared,
        flags,
        writer,
        in_tx,
        reader,
        leftover,
        max_frame_bytes,
        close_timeout_ns,
        transport,
    ));
}

async fn wait_until(mark: u64) {
    clock::wait_until(mark).await;
}

/// The serialized write half. Every write is bounded: it races the close
/// deadline (arming it when a local close lands mid-write), and a stalled
/// or failed write marks the side dead — the pump then only reads.
struct WriteHalf {
    writer: futures::lock::Mutex<wit_bindgen::StreamWriter<u8>>,
    flags: Rc<PumpFlags>,
    local_closed_view: Rc<Shared>,
    close_timeout_ns: u64,
}

impl WriteHalf {
    /// Write a frame, bounded. `Ok(())` means handed to the transport.
    async fn write(&self, bytes: Vec<u8>) -> Result<(), Error> {
        let mut writer = self.writer.lock().await;
        if self.flags.write_dead.get() {
            return Err(Error::Closed);
        }
        let bound = async {
            if self.flags.deadline.get().is_none() {
                // Not closing yet: a local close arriving mid-write arms
                // the deadline; the write then gets until the deadline.
                self.local_closed_view.local_closed.wait().await;
                self.flags.arm_deadline(self.close_timeout_ns);
            }
            let Some(deadline) = self.flags.deadline.get() else {
                std::future::pending::<()>().await;
                return;
            };
            wait_until(deadline).await;
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
}

async fn cmd_task(
    flags: Rc<PumpFlags>,
    writer: Rc<WriteHalf>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    close_timeout_ns: u64,
) {
    // Phase 1: the connection is live — sends write, close closes.
    loop {
        let cmd = futures::select_biased! {
            () = flags.pump_done.wait() => break,
            cmd = cmd_rx.next() => cmd,
        };
        let Some(cmd) = cmd else {
            // All resource handles dropped: drop implies close(none, "").
            if !flags.wire_closing.replace(true) {
                flags.arm_deadline(close_timeout_ns);
                let _ = writer
                    .write(build_frame(OP_CLOSE, &close_payload(None, "")))
                    .await;
            }
            return;
        };
        match cmd {
            Cmd::Send { message, ack } => {
                if flags.wire_closing.get() || flags.write_dead.get() {
                    let _ = ack.send(Err(Error::Closed));
                    continue;
                }
                let (opcode, payload) = match &message {
                    Message::Binary(bytes) => (OP_BINARY, bytes.as_slice()),
                    Message::String(text) => (OP_TEXT, text.as_bytes()),
                };
                let result = writer.write(build_frame(opcode, payload)).await;
                let _ = ack.send(result);
            }
            Cmd::Close { code, reason } => {
                if !flags.wire_closing.replace(true) {
                    flags.arm_deadline(close_timeout_ns);
                    let _ = writer
                        .write(build_frame(OP_CLOSE, &close_payload(code, &reason)))
                        .await;
                }
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

#[allow(clippy::too_many_arguments)]
async fn reader_task(
    shared: Rc<Shared>,
    flags: Rc<PumpFlags>,
    writer: Rc<WriteHalf>,
    in_tx: mpsc::UnboundedSender<Message>,
    mut reader: wit_bindgen::StreamReader<u8>,
    leftover: Vec<u8>,
    max_frame_bytes: usize,
    close_timeout_ns: u64,
    transport: Rc<RefCell<Option<Transport>>>,
) {
    let mut parser = Parser::new(max_frame_bytes);
    parser.extend(&leftover);
    // In-progress fragmented message: (is_text, assembled payload).
    let mut fragments: Option<(bool, Vec<u8>)> = None;
    let mut peer_frame: Option<CloseInfo> = None;

    'outer: loop {
        // Drain every complete frame already buffered before reading more.
        loop {
            match parser.next_frame() {
                Parsed::Incomplete => break,
                Parsed::Violation(_) => break 'outer,
                Parsed::TooLarge => {
                    // Mirror the reference's transport-cap path: latch
                    // overflow, close toward the peer, tear down at once.
                    shared.overflowed.set(true);
                    begin_close(&flags, &writer, close_timeout_ns).await;
                    break 'outer;
                }
                Parsed::Frame(frame) => {
                    match frame.opcode {
                        OP_TEXT | OP_BINARY => {
                            if fragments.is_some() {
                                break 'outer; // new data frame mid-fragmentation
                            }
                            let is_text = frame.opcode == OP_TEXT;
                            if frame.fin {
                                if !deliver(
                                    &shared,
                                    &flags,
                                    &writer,
                                    &in_tx,
                                    is_text,
                                    frame.payload,
                                    close_timeout_ns,
                                )
                                .await
                                {
                                    break 'outer;
                                }
                            } else {
                                fragments = Some((is_text, frame.payload));
                            }
                        }
                        OP_CONTINUATION => {
                            let Some((is_text, mut assembled)) = fragments.take() else {
                                break 'outer; // continuation with no start
                            };
                            if assembled.len() + frame.payload.len() > max_frame_bytes {
                                shared.overflowed.set(true);
                                begin_close(&flags, &writer, close_timeout_ns).await;
                                break 'outer;
                            }
                            assembled.extend_from_slice(&frame.payload);
                            if frame.fin {
                                if !deliver(
                                    &shared,
                                    &flags,
                                    &writer,
                                    &in_tx,
                                    is_text,
                                    assembled,
                                    close_timeout_ns,
                                )
                                .await
                                {
                                    break 'outer;
                                }
                            } else {
                                fragments = Some((is_text, assembled));
                            }
                        }
                        OP_PING => {
                            if !flags.write_dead.get() {
                                let _ = writer.write(build_frame(OP_PONG, &frame.payload)).await;
                            }
                        }
                        OP_PONG => {}
                        OP_CLOSE => {
                            let Ok((code, reason)) = parse_close_payload(&frame.payload) else {
                                break 'outer;
                            };
                            if peer_frame.is_none() {
                                peer_frame = Some(CloseInfo { code, reason });
                            }
                            shared.state.advance(WsState::Closing);
                            flags.arm_deadline(close_timeout_ns);
                            if !flags.wire_closing.replace(true) {
                                // Echo the close frame back, completing the
                                // handshake from our side.
                                let _ = writer.write(build_frame(OP_CLOSE, &frame.payload)).await;
                            }
                            // Keep reading until the peer closes the
                            // transport (or the deadline fires).
                        }
                        _ => break 'outer, // unknown opcode
                    }
                }
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
            wait_until(deadline).await;
        };
        futures::pin_mut!(deadline_expired);
        let read = reader.read(Vec::with_capacity(16 * 1024));
        futures::pin_mut!(read);
        let (status, chunk) = futures::select_biased! {
            () = deadline_expired.fuse() => break,
            result = read.fuse() => result,
        };
        parser.extend(&chunk);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) && chunk.is_empty()
        {
            break;
        }
    }

    // Finalization: latch close details, end the queue after the backlog,
    // stop the command task, tear the transport down, wake wait-closed.
    *shared.close_info.borrow_mut() = Some(peer_frame);
    shared.state.advance(WsState::Closed);
    drop(in_tx);
    flags.pump_done.fire();
    drop(transport.borrow_mut().take());
    shared.closed.fire();
}

/// Deliver one complete inbound message, or begin the overflow close.
/// Returns `false` when the connection must tear down immediately
/// (delivery only fails that way for text that is not valid UTF-8).
async fn deliver(
    shared: &Rc<Shared>,
    flags: &Rc<PumpFlags>,
    writer: &Rc<WriteHalf>,
    in_tx: &mpsc::UnboundedSender<Message>,
    is_text: bool,
    payload: Vec<u8>,
    close_timeout_ns: u64,
) -> bool {
    if flags.wire_closing.get() {
        // Messages arriving during the closing handshake are discarded.
        return true;
    }
    let message = if is_text {
        match String::from_utf8(payload) {
            Ok(text) => Message::String(text),
            Err(_) => return false,
        }
    } else {
        Message::Binary(payload)
    };
    let len = match &message {
        Message::Binary(bytes) => bytes.len(),
        Message::String(text) => text.len(),
    };
    if shared.reserve(len) {
        let _ = in_tx.unbounded_send(message);
    } else {
        // Overflow: the offending message is discarded, the connection
        // closes toward the peer, reading continues until the handshake
        // completes or the deadline fires.
        begin_close(flags, writer, close_timeout_ns).await;
    }
    true
}

/// Close toward the peer with a code-less frame (the peer observes 1005),
/// arming the deadline first.
async fn begin_close(flags: &Rc<PumpFlags>, writer: &Rc<WriteHalf>, close_timeout_ns: u64) {
    if !flags.wire_closing.replace(true) {
        flags.arm_deadline(close_timeout_ns);
        let _ = writer
            .write(build_frame(OP_CLOSE, &close_payload(None, "")))
            .await;
    }
}
