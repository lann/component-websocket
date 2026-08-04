//! The native host runner for the echo-demo component: provisions the
//! `lann:websocket` imports with [`wasmtime_websocket`], stands up the
//! suite echo server in-process, and drives the component's exported `run`.

use anyhow::{Context as _, Result};
use wasmtime::component::{Accessor, Component, HasData, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_websocket::{WasiWebsocketCtx, WasiWebsocketCtxView, WasiWebsocketView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../echo-demo/wit",
        world: "websocket-echo-demo",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "lann:websocket/connections.websocket": wasmtime_websocket::Websocket,
        },
    });
}

struct Ctx {
    websocket: WasiWebsocketCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl WasiWebsocketView for Ctx {
    fn websocket(&mut self) -> WasiWebsocketCtxView<'_> {
        WasiWebsocketCtxView {
            ctx: &mut self.websocket,
            table: &mut self.table,
        }
    }
}

/// Run `component_path`'s exported demo against an in-process echo server.
pub async fn run_demo(component_path: &str, count: u32) -> Result<u32> {
    let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap()).await?;
    let url = format!("{}/echo", server.base_url());

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, component_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("load component {component_path}"))?;
    let mut linker: Linker<Ctx> = Linker::new(&engine);
    wasmtime_websocket::add_to_linker(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            websocket: WasiWebsocketCtx::new(),
            table: ResourceTable::new(),
        },
    );
    let instance =
        bindings::WebsocketEchoDemo::instantiate_async(&mut store, &component, &linker).await?;
    let received = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            instance
                .demo_websocket_echo_demo()
                .call_run(accessor, url, count)
                .await
        })
        .await??
        .map_err(|detail| anyhow::anyhow!("demo failed: {detail}"))?;
    Ok(received)
}
