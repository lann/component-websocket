//! The in-guest provider: a WebSocket client stack over `wasi:sockets`
//! TCP, exporting `polymorph:websocket/connections` — the same interface the
//! hosted implementations serve, so the shared conformance guest composes
//! against it unchanged.
//!
//! `wss:` is served by the composed `polymorph:tls` component (see
//! `tls-component.rev`); trust anchors are explicit configuration and
//! `wss:` fails closed without them. See `README.md` (beside this crate)
//! for the TLS posture and the configuration channel.
//!
//! Behavioral reference: `rust/wasmtime` (the host implementation) and
//! the package contracts in `wit/README.md`. The conformance suite is the
//! gate that keeps the two aligned; where this file makes a choice, it
//! mirrors the reference pump's.

#[allow(missing_docs)]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
    });
}

mod config;
mod connect;
mod io;
mod pump;

use std::cell::RefCell;
use std::rc::Rc;

use bindings::exports::polymorph::websocket::connections::{
    Guest, GuestWebsocket, Websocket as WebsocketResource,
};
use bindings::polymorph::websocket::types::{
    CloseInfo, Error, Message, MessageKind, SendViaStreamError, StreamMessage, WebsocketState,
};
use futures::{FutureExt as _, StreamExt as _};
use pump::{Cmd, Shared, WsState};

struct Component;

impl Guest for Component {
    type Websocket = Websocket;
}

/// One WebSocket connection: the shared state plus the command channel
/// into its pump task.
pub struct Websocket {
    shared: Rc<Shared>,
    protocol: String,
}

impl GuestWebsocket for Websocket {
    async fn connect(url: String, protocols: Vec<String>) -> Result<WebsocketResource, Error> {
        let (shared, protocol) = connect::connect(&url, &protocols).await?;
        Ok(WebsocketResource::new(Websocket { shared, protocol }))
    }

    fn protocol(&self) -> String {
        self.protocol.clone()
    }

    async fn send(&self, message: Message) -> Result<(), Error> {
        if self.shared.local_closed.is_fired() {
            return Err(Error::Closed);
        }
        let (ack_tx, ack_rx) = futures::channel::oneshot::channel();
        if self
            .shared
            .cmd_tx
            .unbounded_send(Cmd::Send {
                message,
                ack: ack_tx,
            })
            .is_err()
        {
            return Err(Error::Closed);
        }
        match ack_rx.await {
            Ok(result) => result,
            Err(_) => Err(Error::Closed),
        }
    }

    async fn receive(&self) -> Result<Message, Error> {
        let shared = &self.shared;
        if shared.local_closed.is_fired() {
            return Err(Error::Closed);
        }
        if shared.stream_claimed.get() {
            return Err(Error::ReceivingViaStream);
        }
        // Concurrent receives serialize on the queue lock, each taking the
        // next message in arrival order. Signals win over a ready message
        // (biased): a pending receive is never handed a message once the
        // stream is claimed or the connection is locally closed.
        let mut queue = shared.queue.lock().await;
        futures::select_biased! {
            () = shared.stream_started.wait() => {
                Err(Error::ReceivingViaStream)
            }
            () = shared.local_closed.wait() => Err(Error::Closed),
            message = queue.next() => shared.map_queue_end(message),
        }
    }

    async fn send_via_stream(
        &self,
        mut messages: wit_bindgen::StreamReader<StreamMessage>,
    ) -> Result<(), SendViaStreamError> {
        let fail = |error: Error, sent: u64| SendViaStreamError { error, sent };
        if self.shared.local_closed.is_fired() {
            return Err(fail(Error::Closed, 0));
        }
        let mut sent: u64 = 0;
        loop {
            // One stream-message is exactly one WebSocket message,
            // processed in stream order; a local close fails the call at
            // the next await point with the count so far.
            let next = futures::select_biased! {
                () = self.shared.local_closed.wait() => return Err(fail(Error::Closed, sent)),
                batch = read_one(&mut messages).fuse() => batch,
            };
            let Some(message) = next else {
                return Ok(());
            };
            let declared = message.length as usize;
            let (data, excess) = futures::select_biased! {
                () = self.shared.local_closed.wait() => return Err(fail(Error::Closed, sent)),
                collected = collect_payload(message.data, declared).fuse() => collected,
            };
            if excess > 0 || data.len() != declared {
                let actual = data.len() + excess;
                return Err(fail(
                    Error::Other(format!(
                        "stream-message payload was {actual} bytes but length declared {declared}"
                    )),
                    sent,
                ));
            }
            let payload = match message.kind {
                MessageKind::Binary => Message::Binary(data),
                MessageKind::String => match String::from_utf8(data) {
                    Ok(text) => Message::String(text),
                    Err(err) => {
                        return Err(fail(
                            Error::Other(format!(
                                "string stream-message payload is not valid UTF-8: {err}"
                            )),
                            sent,
                        ))
                    }
                },
            };
            self.send(payload)
                .await
                .map_err(|error| fail(error, sent))?;
            sent += 1;
        }
    }

    fn receive_via_stream(&self) -> Result<wit_bindgen::StreamReader<StreamMessage>, Error> {
        let shared = Rc::clone(&self.shared);
        if shared.local_closed.is_fired() {
            return Err(Error::Closed);
        }
        if shared.stream_claimed.replace(true) {
            return Err(Error::ReceivingViaStream);
        }
        shared.stream_started.fire();
        let (mut tx, rx) = bindings::wit_stream::new();
        let feeder_shared = Rc::clone(&shared);
        let feeder = Box::pin(async move {
            let shared = feeder_shared;
            let mut queue = shared.queue.lock().await;
            loop {
                let message = futures::select_biased! {
                    // A local close ends the stream immediately, backlog
                    // discarded; any other close ends it after the
                    // backlog. The end carries no error value.
                    () = shared.local_closed.wait() => break,
                    message = queue.next() => message,
                };
                let Some(message) = message else { break };
                shared.release_budget(&message);
                let (kind, payload) = match message {
                    Message::Binary(bytes) => (MessageKind::Binary, bytes),
                    Message::String(text) => (MessageKind::String, text.into_bytes()),
                };
                let length = payload.len() as u32;
                let (mut data_tx, data_rx) = bindings::wit_stream::new();
                let element = StreamMessage {
                    kind,
                    length,
                    data: data_rx,
                };
                if !tx.write_all(vec![element]).await.is_empty() {
                    break;
                }
                if !data_tx.write_all(payload).await.is_empty() {
                    break;
                }
                drop(data_tx);
            }
        });
        if self
            .shared
            .cmd_tx
            .unbounded_send(Cmd::ClaimStream { feeder })
            .is_err()
        {
            // The pump is gone: the connection is terminal; the stream
            // ends immediately (the writer drops unfed), which is the
            // contract's shape for a post-close claim... but a local
            // close was already checked above, so report closed.
            return Err(Error::Closed);
        }
        Ok(rx)
    }

    fn state(&self) -> WebsocketState {
        match self.shared.state.get() {
            WsState::Open => WebsocketState::Open,
            WsState::Closing => WebsocketState::Closing,
            WsState::Closed => WebsocketState::Closed,
        }
    }

    async fn wait_closed(&self) -> Option<CloseInfo> {
        self.shared.closed.wait().await;
        self.shared.close_info.borrow().clone().flatten()
    }

    fn close(&self, code: Option<u16>, reason: String) -> Result<(), Error> {
        validate_close_args(code, &reason)?;
        let shared = &self.shared;
        if shared.local_closed.is_fired() {
            // Idempotent: only the first call's frame is sent.
            return Ok(());
        }
        shared.state.advance(WsState::Closing);
        shared.local_closed.fire();
        let _ = shared.cmd_tx.unbounded_send(Cmd::Close { code, reason });
        Ok(())
    }
}

impl Drop for Websocket {
    fn drop(&mut self) {
        // Dropping the resource implies close(none, "").
        let _ = self.close(None, String::new());
    }
}

/// Read one element from a stream of stream-messages.
async fn read_one(reader: &mut wit_bindgen::StreamReader<StreamMessage>) -> Option<StreamMessage> {
    let (status, mut batch) = reader.read(Vec::with_capacity(1)).await;
    match batch.pop() {
        Some(element) => Some(element),
        None => match status {
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled => None,
            // A zero-length completion: keep reading.
            wit_bindgen::StreamResult::Complete(_) => Box::pin(read_one(reader)).await,
        },
    }
}

/// Drain a stream-message payload: collect up to `declared` bytes and
/// count (but do not keep) any excess, so a mis-declared length cannot
/// grow memory.
async fn collect_payload(
    mut data: wit_bindgen::StreamReader<u8>,
    declared: usize,
) -> (Vec<u8>, usize) {
    let mut collected = Vec::with_capacity(declared.min(64 * 1024));
    let mut excess = 0usize;
    loop {
        let (status, chunk) = data.read(Vec::with_capacity(16 * 1024)).await;
        for byte in chunk {
            if collected.len() < declared {
                collected.push(byte);
            } else {
                excess += 1;
            }
        }
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            return (collected, excess);
        }
    }
}

/// Validate close arguments per the close contract: code 1000 or
/// 3000-4999, a reason requires a code, reasons cap at 123 UTF-8 bytes.
fn validate_close_args(code: Option<u16>, reason: &str) -> Result<(), Error> {
    if let Some(code) = code {
        if code != 1000 && !(3000..=4999).contains(&code) {
            return Err(Error::InvalidArgument(format!(
                "close code must be 1000 or in 3000-4999, not {code}"
            )));
        }
    } else if !reason.is_empty() {
        return Err(Error::InvalidArgument(
            "a close reason requires a close code".to_string(),
        ));
    }
    if reason.len() > 123 {
        return Err(Error::InvalidArgument(format!(
            "close reason must be at most 123 bytes, got {}",
            reason.len()
        )));
    }
    Ok(())
}

/// A latched, single-threaded signal: fire once, await any number of
/// times (before or after the fire).
pub(crate) struct Flag {
    fired: std::cell::Cell<bool>,
    wakers: RefCell<Vec<std::task::Waker>>,
}

impl Flag {
    pub(crate) fn new() -> Self {
        Flag {
            fired: std::cell::Cell::new(false),
            wakers: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn fire(&self) {
        if !self.fired.replace(true) {
            for waker in self.wakers.borrow_mut().drain(..) {
                waker.wake();
            }
        }
    }

    pub(crate) fn is_fired(&self) -> bool {
        self.fired.get()
    }

    /// A future resolving when (or immediately if) the flag has fired.
    pub(crate) fn wait(&self) -> FlagWait<'_> {
        FlagWait { flag: self }
    }
}

pub(crate) struct FlagWait<'a> {
    flag: &'a Flag,
}

impl futures::future::FusedFuture for FlagWait<'_> {
    fn is_terminated(&self) -> bool {
        false
    }
}

impl std::future::Future for FlagWait<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.flag.fired.get() {
            std::task::Poll::Ready(())
        } else {
            let mut wakers = self.flag.wakers.borrow_mut();
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            std::task::Poll::Pending
        }
    }
}

bindings::export!(Component with_types_in bindings);
