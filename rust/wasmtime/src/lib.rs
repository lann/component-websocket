//! Wasmtime host implementation of the `polymorph:websocket` interfaces, backed
//! by [`tokio-tungstenite`](https://github.com/snapview/tokio-tungstenite).
//!
//! This crate factors the host-agnostic part of a Wasmtime WebSocket host
//! out of the embedding binaries so any host can satisfy the
//! `polymorph:websocket` imports with one call to [`add_to_linker`]. It is a
//! wasip3 (component-model async) implementation modeled after
//! [`wasmtime_wasi_http::p3`]: a host embeds a [`WasiWebsocketCtx`] in its
//! store state, implements [`WasiWebsocketView`] to expose it alongside the
//! store's [`ResourceTable`], and calls [`add_to_linker`] to satisfy the
//! `types` and `connections` imports with a real WebSocket client.
//!
//! The embedding must run inside a tokio runtime context: each connection
//! spawns a pump task on it.
//!
//! A host generating its own bindings with `wasmtime::component::bindgen!`
//! must map the `connections` resource onto this crate's host type:
//!
//! ```text
//! with: {
//!     "polymorph:websocket/connections.websocket":
//!         wasmtime_websocket::Websocket,
//! },
//! ```
//!
//! The crate has no behavioral tests of its own: its behavior is asserted
//! end to end by the conformance suite (`conformance/`).
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod bindings;
mod error;
mod host;
mod websocket;

pub use error::{WebsocketError, WebsocketResult};
pub use websocket::{
    CloseInfo, Websocket, DEFAULT_CLOSE_TIMEOUT, DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_MAX_INBOUND_BUFFER_BYTES,
};

use wasmtime::component::{HasData, Linker, ResourceTable};

/// Configuration and per-store state for the WebSocket host.
///
/// This is intentionally minimal (mirroring `wasmtime_wasi_http`'s
/// `WasiHttpCtx`); it exists so hosts have a stable place to grow
/// configuration without changing the [`WasiWebsocketView`] shape.
///
/// The knobs so far are the bounds the WIT leaves implementation-defined:
/// the connect timeout, the closing-handshake bound, and the per-connection
/// inbound buffer bound. The crate reads no ambient environment: every knob
/// is set through this context by the embedding host, which owns any
/// env-driven configuration.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WasiWebsocketCtx {
    connect_timeout: std::time::Duration,
    close_timeout: std::time::Duration,
    max_inbound_buffer_bytes: usize,
    extra_tls_roots_pem: Option<std::sync::Arc<str>>,
}

impl Default for WasiWebsocketCtx {
    fn default() -> Self {
        Self {
            connect_timeout: websocket::DEFAULT_CONNECT_TIMEOUT,
            close_timeout: websocket::DEFAULT_CLOSE_TIMEOUT,
            max_inbound_buffer_bytes: DEFAULT_MAX_INBOUND_BUFFER_BYTES,
            extra_tls_roots_pem: None,
        }
    }
}

impl WasiWebsocketCtx {
    /// Create a new, default context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set how long `websocket.connect` waits for the handshake before
    /// failing with `error.connect-failed` (the WIT leaves the bound
    /// implementation-defined). Default: [`DEFAULT_CONNECT_TIMEOUT`].
    pub fn set_connect_timeout(&mut self, timeout: std::time::Duration) {
        self.connect_timeout = timeout;
    }

    /// The configured connect timeout.
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.connect_timeout
    }

    /// Set how long the closing procedure may stay incomplete before the
    /// transport is torn down anyway (the WIT leaves the bound
    /// implementation-defined). Also bounds any single stalled transport
    /// write once closing. Default: [`DEFAULT_CLOSE_TIMEOUT`].
    pub fn set_close_timeout(&mut self, timeout: std::time::Duration) {
        self.close_timeout = timeout;
    }

    /// The configured closing-handshake bound.
    pub fn close_timeout(&self) -> std::time::Duration {
        self.close_timeout
    }

    /// Set the per-connection inbound buffer bound, in payload bytes (see
    /// the `websocket` WIT docs for the overflow contract). Default:
    /// [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`]. The crate itself never reads
    /// the environment; a host offering the bound as an env knob reads and
    /// validates the value itself and applies it here.
    pub fn set_max_inbound_buffer_bytes(&mut self, bytes: usize) {
        self.max_inbound_buffer_bytes = bytes;
    }

    /// The configured per-connection inbound buffer bound.
    pub fn max_inbound_buffer_bytes(&self) -> usize {
        self.max_inbound_buffer_bytes
    }

    /// Add trust anchors for `wss:` connections: a PEM bundle of CA
    /// certificates trusted *in addition to* the platform's native roots.
    /// Certificates that do not parse are rejected at the next `connect`
    /// (`error.connect-failed`), not here. Trust configuration is
    /// deliberately a host-side knob: the WIT surface carries no trust
    /// decisions (see the package README's "Portability contract").
    pub fn set_extra_tls_roots_pem(&mut self, pem: impl Into<std::sync::Arc<str>>) {
        self.extra_tls_roots_pem = Some(pem.into());
    }

    /// The configured additional TLS trust anchors, if any.
    pub fn extra_tls_roots_pem(&self) -> Option<&str> {
        self.extra_tls_roots_pem.as_deref()
    }
}

/// A borrowed view into a host's [`WasiWebsocketCtx`] and its
/// [`ResourceTable`].
///
/// Returned by [`WasiWebsocketView::websocket`], this is the
/// [`HasData::Data`] the generated host bindings operate on.
pub struct WasiWebsocketCtxView<'a> {
    /// Mutable reference to the WebSocket host context.
    pub ctx: &'a mut WasiWebsocketCtx,
    /// Mutable reference to the table used to manage host resources.
    pub table: &'a mut ResourceTable,
}

/// A trait that provides access to the [`WasiWebsocketCtx`] host state.
///
/// Implement this for your store's data type so [`add_to_linker`] can wire
/// the `polymorph:websocket` imports onto your linker.
pub trait WasiWebsocketView: Send {
    /// Return a [`WasiWebsocketCtxView`] from a mutable reference to `self`.
    fn websocket(&mut self) -> WasiWebsocketCtxView<'_>;
}

/// The type for which this crate implements the `polymorph:websocket`
/// interfaces. Used as the [`HasData`] marker for the generated bindings.
pub struct WasiWebsocket;

impl HasData for WasiWebsocket {
    type Data<'a> = WasiWebsocketCtxView<'a>;
}

/// Add the `polymorph:websocket` interfaces implemented by this crate (`types`
/// and `connections`) to the provided [`Linker`].
///
/// The store's data type `T` must implement [`WasiWebsocketView`]. The
/// engine's [`Config`](wasmtime::Config) must have
/// `wasm_component_model_async` enabled, since the resource's methods use
/// the component-model async ABI.
///
/// # Example
///
/// ```no_run
/// use wasmtime::component::{Linker, ResourceTable};
/// use wasmtime::Result;
/// use wasmtime_websocket::{
///     add_to_linker, WasiWebsocketCtx, WasiWebsocketCtxView, WasiWebsocketView,
/// };
///
/// struct MyState {
///     websocket: WasiWebsocketCtx,
///     table: ResourceTable,
/// }
///
/// impl WasiWebsocketView for MyState {
///     fn websocket(&mut self) -> WasiWebsocketCtxView<'_> {
///         WasiWebsocketCtxView {
///             ctx: &mut self.websocket,
///             table: &mut self.table,
///         }
///     }
/// }
///
/// fn wire(linker: &mut Linker<MyState>) -> Result<()> {
///     add_to_linker(linker)
/// }
/// ```
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WasiWebsocketView + 'static,
{
    bindings::websocket::types::add_to_linker::<_, WasiWebsocket>(linker, T::websocket)?;
    bindings::websocket::connections::add_to_linker::<_, WasiWebsocket>(linker, T::websocket)?;
    Ok(())
}
