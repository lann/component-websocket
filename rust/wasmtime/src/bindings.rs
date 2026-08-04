//! Raw `bindgen!` output for the `lann:websocket` package.
//!
//! The crate implements the `types` interface and the `connections`
//! interface's `websocket` resource. See [`crate`] for the public API built
//! on top of these bindings.

#[allow(missing_docs, reason = "generated code")]
mod generated {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "imports",
        imports: {
            // `connect`/`send`/`receive`/`send-via-stream`/`wait-closed`/
            // `drop` need all three: `async` for the component-model async
            // ABI, `store` for `Accessor` access to the `ResourceTable` (and
            // the `…WithStore` traits that host the async methods), and
            // `trappable` so the host functions can return `wasmtime::Result`
            // and surface host errors as traps.
            default: async | store | trappable,
            // `websocket.protocol`, `websocket.state`, and
            // `websocket.close` are synchronous functions in the WIT and
            // are imported as such by guests, so they are bound
            // synchronously (still `trappable`, but not `async`); they
            // need no store access.
            "lann:websocket/connections@0.1.0.[method]websocket.protocol": trappable,
            "lann:websocket/connections@0.1.0.[method]websocket.state": trappable,
            "lann:websocket/connections@0.1.0.[method]websocket.close": trappable,
            // `receive-via-stream` is synchronous in the WIT: it hands back
            // a stream without awaiting. It still needs `store` to allocate
            // the returned stream on the guest's behalf.
            "lann:websocket/connections@0.1.0.[method]websocket.receive-via-stream": store | trappable,
        },
        with: {
            "lann:websocket/connections.websocket": crate::Websocket,
        },
    });
}

pub use self::generated::lann::*;
