//! Host trait implementations for the `lann:websocket` imports.
//!
//! Following the split the generated bindings produce (and mirroring
//! `wasmtime_wasi_http::p3`), the store-free traits are implemented for the
//! [`WasiWebsocketCtxView`] "data" type, while the traits whose methods need
//! the async `Accessor` are implemented for the [`WasiWebsocket`] `HasData`
//! marker.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt as _, StreamExt as _};
use wasmtime::component::{
    Access, Accessor, Destination, Resource, Source, StreamConsumer, StreamProducer, StreamReader,
    StreamResult,
};
use wasmtime::{AsContextMut, Result, StoreContextMut};

use crate::bindings::websocket::connections::{self, HostWebsocket, HostWebsocketWithStore};
use crate::bindings::websocket::types::{
    self, CloseInfo, Error, Message, MessageKind, SendViaStreamError, StreamMessage, WebsocketState,
};
use crate::error::WebsocketError;
use crate::state_watch::StateWatch;
use crate::websocket::{
    next_inbound, ConnectConfig, InboundMessage, InboundQueue, Signal, WsState,
};
use crate::{WasiWebsocket, WasiWebsocketCtxView, Websocket};

impl From<WebsocketError> for Error {
    fn from(err: WebsocketError) -> Self {
        match err {
            WebsocketError::InvalidUrl(msg) => Error::InvalidUrl(msg),
            WebsocketError::ConnectFailed(msg) => Error::ConnectFailed(msg),
            WebsocketError::Closed => Error::Closed,
            WebsocketError::ReceivingViaStream => Error::ReceivingViaStream,
            WebsocketError::ReceiveBufferOverflow => Error::ReceiveBufferOverflow,
            WebsocketError::InvalidArgument(msg) => Error::InvalidArgument(msg),
            WebsocketError::Other(msg) => Error::Other(msg),
        }
    }
}

impl From<crate::websocket::CloseInfo> for CloseInfo {
    fn from(info: crate::websocket::CloseInfo) -> Self {
        CloseInfo {
            code: info.code,
            reason: info.reason,
        }
    }
}

fn to_wit_message(inbound: InboundMessage) -> Message {
    match inbound {
        InboundMessage::Text(text) => Message::String(text),
        InboundMessage::Binary(data) => Message::Binary(data),
    }
}

fn to_inbound(message: Message) -> InboundMessage {
    match message {
        Message::String(text) => InboundMessage::Text(text),
        Message::Binary(data) => InboundMessage::Binary(data),
    }
}

// --- types -----------------------------------------------------------------

impl types::Host for WasiWebsocketCtxView<'_> {}

// --- connections -----------------------------------------------------------

impl connections::Host for WasiWebsocketCtxView<'_> {}

impl HostWebsocket for WasiWebsocketCtxView<'_> {
    fn protocol(&mut self, self_: Resource<Websocket>) -> Result<String> {
        Ok(self.table.get(&self_)?.protocol())
    }

    fn close(
        &mut self,
        self_: Resource<Websocket>,
        code: Option<u16>,
        reason: String,
    ) -> Result<std::result::Result<(), Error>> {
        Ok(self
            .table
            .get(&self_)?
            .close(code, reason)
            .map_err(Error::from))
    }
}

/// A drained `stream-message` payload: the bytes stored up to the declared
/// length, plus a count of any bytes the stream carried past it (consumed but
/// not buffered, so a mis-declared length cannot grow host memory unbounded).
#[derive(Default)]
struct CollectedPayload {
    data: Vec<u8>,
    excess: u64,
}

/// A [`StreamConsumer`] that drains every byte of a `stream<u8>` into a
/// buffer bounded by the message's declared `length` (bytes past it are
/// counted, not stored), handing the result back through `done_tx` when the
/// stream ends.
struct ByteCollector {
    buf: Vec<u8>,
    /// The message's declared `length`: the buffering bound.
    limit: usize,
    /// Bytes received past `limit`, consumed and discarded.
    excess: u64,
    done_tx: Option<oneshot::Sender<CollectedPayload>>,
}

impl ByteCollector {
    fn finish(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(CollectedPayload {
                data: std::mem::take(&mut self.buf),
                excess: self.excess,
            });
        }
    }
}

impl<D: Send + 'static> StreamConsumer<D> for ByteCollector {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: ByteCollector is Unpin

        let available = source.remaining(&mut store);
        if available > 0 {
            let mut chunk = Vec::with_capacity(available);
            source.read(&mut store, &mut chunk)?;
            let room = this.limit.saturating_sub(this.buf.len());
            if chunk.len() > room {
                this.excess += (chunk.len() - room) as u64;
                chunk.truncate(room);
            }
            this.buf.extend_from_slice(&chunk);
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        // No bytes available. When `finish` is set the stream is ending, so
        // hand the collected buffer back; `Drop` covers a normal
        // end-of-stream.
        if finish {
            this.finish();
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for ByteCollector {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A [`StreamProducer`] that yields one `stream-message` per inbound
/// WebSocket message, wrapping each message's bytes in a fresh `stream<u8>`.
struct InboundStreamMessages {
    incoming: Arc<AsyncMutex<InboundQueue>>,
    /// Ends the stream once the connection is locally closed (the unread
    /// backlog is discarded, per the close contract).
    local_closed: Signal,
    /// A future resolving to the next inbound message (or `None` once the
    /// connection is closed), retained across polls so the shared receiver
    /// lock is only held while awaiting the next message.
    pending: Option<Pin<Box<dyn Future<Output = Option<InboundMessage>> + Send>>>,
}

impl<D: Send + 'static> StreamProducer<D> for InboundStreamMessages {
    type Item = StreamMessage;
    type Buffer = Option<StreamMessage>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: InboundStreamMessages is Unpin

        let incoming = this.incoming.clone();
        let local_closed = this.local_closed.clone();
        let fut = this.pending.get_or_insert_with(|| {
            Box::pin(async move {
                let mut next = std::pin::pin!(async move {
                    let mut queue = incoming.lock().await;
                    queue.next().await
                }
                .fuse());
                let mut closed = std::pin::pin!(local_closed.fired().fuse());
                futures::select_biased! {
                    message = next => message,
                    _ = closed => None,
                }
            })
        });

        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                this.pending = None;
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Some(inbound)) => {
                this.pending = None;
                let (kind, data) = match inbound {
                    InboundMessage::Text(text) => (MessageKind::String, text.into_bytes()),
                    InboundMessage::Binary(data) => (MessageKind::Binary, data),
                };
                let length = data.len() as u32;
                let data = StreamReader::new(store, data)?;
                destination.set_buffer(Some(StreamMessage { kind, length, data }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

/// One outbound message parsed from a `stream<stream-message>` element: its
/// kind, declared length, and a receiver for its fully-drained payload.
struct PendingSend {
    is_string: bool,
    length: usize,
    done_rx: oneshot::Receiver<CollectedPayload>,
}

/// A [`StreamConsumer`] that reads each `stream-message` from a
/// `stream<stream-message>`, starts draining its `data` payload, and forwards
/// the resulting [`PendingSend`] to the `send-via-stream` driver.
struct OutboundStreamMessages {
    tx: mpsc::UnboundedSender<PendingSend>,
}

impl<D: Send + 'static> StreamConsumer<D> for OutboundStreamMessages {
    type Item = StreamMessage;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, StreamMessage>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: OutboundStreamMessages is Unpin

        let available = source.remaining(&mut store);
        if available == 0 {
            // No items are ready. When `finish` is set the writer is done,
            // so report cancellation; otherwise wait to be re-polled once
            // the writer provides more items.
            return if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            };
        }

        // Drain every message the writer has made available, so a writer
        // that queues several messages before closing does not have the
        // trailing ones silently discarded when it finishes. Each message's
        // payload is drained concurrently by a `ByteCollector`; the
        // `send-via-stream` driver then sends the fully-buffered messages
        // one at a time, in order.
        let mut messages: Vec<StreamMessage> = Vec::with_capacity(available);
        source.read(&mut store, &mut messages)?;
        for message in messages {
            let is_string = matches!(message.kind, MessageKind::String);
            let length = message.length as usize;
            let (done_tx, done_rx) = oneshot::channel();
            message.data.pipe(
                store.as_context_mut(),
                // Grow the buffer as bytes arrive (bounded by the declared
                // length) rather than pre-allocating the guest-declared
                // size.
                ByteCollector {
                    buf: Vec::new(),
                    limit: length,
                    excess: 0,
                    done_tx: Some(done_tx),
                },
            )?;
            let _ = this.tx.unbounded_send(PendingSend {
                is_string,
                length,
                done_rx,
            });
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// A [`StreamProducer`] backing the `state-changes` stream: a coalescing
/// watch over the connection's [`StateWatch`], converting each internal
/// state into its WIT enum on demand. The first element reflects the state
/// at the first read; consecutive elements are distinct; the stream ends
/// after the terminal state.
struct StateChanges {
    /// The watch, or `None` if `state-changes` was already claimed (in which
    /// case the stream is empty).
    watch: Option<Arc<StateWatch<WsState>>>,
    /// The version of the last-delivered state (`None` before the first).
    delivered: Option<u64>,
}

impl<D: Send + 'static> StreamProducer<D> for StateChanges {
    type Item = WebsocketState;
    type Buffer = Option<WebsocketState>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: StateChanges is Unpin
        let Some(watch) = this.watch.as_ref() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        let (value, version) = watch.current();
        match this.delivered {
            // Already delivered this version: the stream ends after the
            // terminal state, otherwise wait for the next change.
            Some(seen) if seen == version => {
                if watch.is_terminal_now() {
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                match watch.poll_changed(seen, cx) {
                    Poll::Ready(_) => {
                        // Changed between `current` and here; re-poll.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Poll::Pending => {
                        if finish {
                            Poll::Ready(Ok(StreamResult::Cancelled))
                        } else {
                            Poll::Pending
                        }
                    }
                }
            }
            // A new (or first) state: deliver it.
            _ => {
                this.delivered = Some(version);
                destination.set_buffer(Some(match value {
                    WsState::Open => WebsocketState::Open,
                    WsState::Closing => WebsocketState::Closing,
                    WsState::Closed => WebsocketState::Closed,
                }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

impl<T: Send> HostWebsocketWithStore<T> for WasiWebsocket {
    async fn connect(
        accessor: &Accessor<T, Self>,
        url: String,
        protocols: Vec<String>,
    ) -> Result<std::result::Result<Resource<Websocket>, Error>> {
        let config = accessor.with(|mut access| {
            let ctx = access.get().ctx;
            ConnectConfig {
                connect_timeout: ctx.connect_timeout(),
                close_timeout: ctx.close_timeout(),
                max_inbound_buffer_bytes: ctx.max_inbound_buffer_bytes(),
            }
        });
        match Websocket::connect(url, protocols, config).await {
            Ok(websocket) => Ok(Ok(
                accessor.with(|mut access| access.get().table.push(websocket))?
            )),
            Err(err) => Ok(Err(err.into())),
        }
    }

    async fn send(
        accessor: &Accessor<T, Self>,
        self_: Resource<Websocket>,
        message: Message,
    ) -> Result<std::result::Result<(), Error>> {
        let handle = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.send_handle())
        })?;
        Ok(handle.send(to_inbound(message)).await.map_err(Error::from))
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<Websocket>,
    ) -> Result<std::result::Result<Message, Error>> {
        let (incoming, stream_started, stream_receiving, local_closed) =
            accessor.with(|mut access| {
                let websocket = access.get().table.get(&self_)?;
                Ok::<_, wasmtime::Error>((
                    websocket.incoming(),
                    websocket.stream_started(),
                    websocket.is_stream_receiving(),
                    websocket.local_closed(),
                ))
            })?;

        // The connection was deliberately closed: calls made after fail
        // `closed` and the unread backlog is discarded. (A remote or
        // abnormal close is different: its backlog stays deliverable and
        // readers observe the end through the queue draining.)
        if local_closed.is_fired() {
            return Ok(Err(Error::Closed));
        }
        // `receive-via-stream` has already taken over the inbound messages.
        if stream_receiving {
            return Ok(Err(Error::ReceivingViaStream));
        }

        // Race receiving the next inbound message against
        // `receive-via-stream` being called and against a local close: a
        // pending receiver is woken and fails with `receiving-via-stream`
        // the moment the stream is claimed, or with `closed` on a local
        // close. Biased order: an already-available message wins over the
        // signals.
        let mut receive = std::pin::pin!(next_inbound(incoming).fuse());
        let mut started = std::pin::pin!(stream_started.fuse());
        let mut local = std::pin::pin!(local_closed.fired().fuse());
        Ok(futures::select_biased! {
            result = receive => match result {
                Ok(inbound) => Ok(to_wit_message(inbound)),
                Err(err) => Err(err.into()),
            },
            // The stream-started signal fired; when it was actually sent
            // (rather than cancelled by the resource dropping) report the
            // takeover.
            signal = started => {
                if signal.is_ok() {
                    Err(Error::ReceivingViaStream)
                } else {
                    Err(Error::Closed)
                }
            }
            _ = local => Err(Error::Closed),
        })
    }

    async fn send_via_stream(
        accessor: &Accessor<T, Self>,
        self_: Resource<Websocket>,
        messages: StreamReader<StreamMessage>,
    ) -> Result<std::result::Result<(), SendViaStreamError>> {
        let (handle, local_closed) = accessor.with(|mut access| {
            let websocket = access.get().table.get(&self_)?;
            Ok::<_, wasmtime::Error>((websocket.send_handle(), websocket.local_closed()))
        })?;
        let closed_err = |sent| {
            Ok(Err(SendViaStreamError {
                error: Error::Closed,
                sent,
            }))
        };

        if local_closed.is_fired() {
            return closed_err(0);
        }

        // Drain each element's payload concurrently (via
        // `OutboundStreamMessages`) while this driver sends the
        // fully-buffered messages one at a time, in stream order.
        let (tx, mut rx) = mpsc::unbounded::<PendingSend>();
        accessor.with(|access| messages.pipe(access, OutboundStreamMessages { tx }))?;

        let mut closed = std::pin::pin!(local_closed.fired().fuse());
        let mut sent: u64 = 0;
        loop {
            let pending = futures::select_biased! {
                pending = rx.next() => match pending {
                    Some(pending) => pending,
                    None => break,
                },
                _ = closed => return closed_err(sent),
            };
            let mut done_rx = std::pin::pin!(pending.done_rx.fuse());
            let payload = futures::select_biased! {
                payload = done_rx => payload.unwrap_or_default(),
                _ = closed => return closed_err(sent),
            };
            if payload.excess > 0 || payload.data.len() != pending.length {
                return Ok(Err(SendViaStreamError {
                    error: WebsocketError::Other(format!(
                        "stream-message payload was {} bytes but length declared {}",
                        payload.data.len() as u64 + payload.excess,
                        pending.length
                    ))
                    .into(),
                    sent,
                }));
            }
            let message = if pending.is_string {
                match String::from_utf8(payload.data) {
                    Ok(text) => InboundMessage::Text(text),
                    Err(err) => {
                        return Ok(Err(SendViaStreamError {
                            error: WebsocketError::Other(format!(
                                "string stream-message payload is not valid UTF-8: {err}"
                            ))
                            .into(),
                            sent,
                        }))
                    }
                }
            } else {
                InboundMessage::Binary(payload.data)
            };
            let mut send = std::pin::pin!(handle.send(message).fuse());
            futures::select_biased! {
                result = send => {
                    if let Err(error) = result {
                        return Ok(Err(SendViaStreamError {
                            error: error.into(),
                            sent,
                        }));
                    }
                }
                _ = closed => return closed_err(sent),
            }
            sent += 1;
        }
        Ok(Ok(()))
    }

    fn receive_via_stream(
        mut access: Access<'_, T, Self>,
        self_: Resource<Websocket>,
    ) -> Result<std::result::Result<StreamReader<StreamMessage>, Error>> {
        let websocket = access.get().table.get(&self_)?;
        // Calls made after a local close fail `closed`; a remote or abnormal
        // close instead ends the returned stream after the backlog drains.
        if websocket.local_closed().is_fired() {
            return Ok(Err(Error::Closed));
        }
        let (claimed, incoming, local_closed) = (
            websocket.begin_stream_receiving(),
            websocket.incoming(),
            websocket.local_closed(),
        );
        if !claimed {
            return Ok(Err(Error::ReceivingViaStream));
        }
        let reader = StreamReader::new(
            access,
            InboundStreamMessages {
                incoming,
                local_closed,
                pending: None,
            },
        )?;
        Ok(Ok(reader))
    }

    fn state_changes(
        mut access: Access<'_, T, Self>,
        self_: Resource<Websocket>,
    ) -> Result<StreamReader<WebsocketState>> {
        let websocket = access.get().table.get(&self_)?;
        // Take-once per the WIT contract: a later call returns a stream that
        // ends immediately.
        let watch = websocket
            .take_state_stream()
            .then(|| websocket.state_watch());
        StreamReader::new(
            access,
            StateChanges {
                watch,
                delivered: None,
            },
        )
    }

    async fn wait_closed(
        accessor: &Accessor<T, Self>,
        self_: Resource<Websocket>,
    ) -> Result<Option<CloseInfo>> {
        let handle = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.closed_handle())
        })?;
        Ok(handle.wait().await.map(CloseInfo::from))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<Websocket>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}
