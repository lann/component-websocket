//! Conformance adapter for the wasmtime (native tokio-tungstenite) target.
//!
//! It runs the shared conformance guest component against the wasmtime host
//! ([`wasmtime_websocket`]) and emits an adapter result document the
//! conformance runner consumes. For each registered test it provisions a
//! fresh store (with the conformance timing/buffer bounds applied), drives
//! the guest's exported `run-test` against an in-process suite echo server,
//! and records the WIT-observable outcome.
//!
//! The guest owns every assertion; the adapter only provisions,
//! orchestrates, and records.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use wasmtime::component::{Accessor, Component, HasData, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_websocket::{WasiWebsocketCtx, WasiWebsocketCtxView, WasiWebsocketView};

use conformance_adapter_common::{
    default_jobs, file_sha256, run_corpus, unreachable_url, verify_corpus, verify_registry,
    write_report, AdapterReport, RawResult, TestOutcome, CONFORMANCE_CLOSE_TIMEOUT,
    CONFORMANCE_CONNECT_TIMEOUT, CONFORMANCE_MAX_INBOUND_BUFFER_BYTES, TESTS, TEST_TIMEOUT,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "conformance",
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

use bindings::exports::conformance::suite::runner::{TestConfig, TestResult};
use bindings::Conformance;

/// Per-store host state.
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

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Ok(Engine::new(&config)?)
}

/// A fresh store with the conformance bounds applied (see
/// `conformance-adapter-common` for why each bound exists).
fn new_store(engine: &Engine) -> Store<Ctx> {
    let mut websocket = WasiWebsocketCtx::new();
    websocket.set_connect_timeout(CONFORMANCE_CONNECT_TIMEOUT);
    websocket.set_close_timeout(CONFORMANCE_CLOSE_TIMEOUT);
    websocket.set_max_inbound_buffer_bytes(CONFORMANCE_MAX_INBOUND_BUFFER_BYTES as usize);
    Store::new(
        engine,
        Ctx {
            websocket,
            table: ResourceTable::new(),
        },
    )
}

fn add_to_linker(engine: &Engine) -> Result<Linker<Ctx>> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    wasmtime_websocket::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Instantiate the guest and drive one `run-test` call to its outcome.
async fn run_instance(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestOutcome> {
    let mut store = new_store(engine);
    let linker = add_to_linker(engine)?;
    let instance = Conformance::instantiate_async(&mut store, component, &linker).await?;
    let test_id = test_id.to_string();
    let result = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            instance
                .conformance_suite_runner()
                .call_run_test(accessor, test_id, config)
                .await
        })
        .await??;
    Ok(match result {
        TestResult::Pass => TestOutcome::Pass,
        TestResult::Fail(detail) => TestOutcome::Fail(detail),
        TestResult::Skipped(detail) => TestOutcome::Skipped(detail),
    })
}

/// Fetch the guest's `list-tests` for the corpus-drift gates.
async fn list_tests(engine: &Engine, component: &Component) -> Result<Vec<(String, Vec<String>)>> {
    let mut store = new_store(engine);
    let linker = add_to_linker(engine)?;
    let instance = Conformance::instantiate_async(&mut store, component, &linker).await?;
    let descriptors = instance
        .conformance_suite_runner()
        .call_list_tests(&mut store)
        .await?;
    Ok(descriptors.into_iter().map(|d| (d.id, d.tags)).collect())
}

struct Cli {
    guest: PathBuf,
    tests: PathBuf,
    out: PathBuf,
    only: Vec<String>,
    jobs: usize,
}

fn parse_cli() -> Result<Cli> {
    let mut guest = None;
    let mut tests = PathBuf::from("conformance/tests.toml");
    let mut out = PathBuf::from("conformance/results");
    let mut only = Vec::new();
    let mut jobs = default_jobs();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--guest" => guest = Some(PathBuf::from(value("--guest")?)),
            "--tests" => tests = PathBuf::from(value("--tests")?),
            "--out" => out = PathBuf::from(value("--out")?),
            "--only" => only.push(value("--only")?),
            "--jobs" => jobs = value("--jobs")?.parse().context("--jobs")?,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Cli {
        guest: guest.ok_or_else(|| anyhow::anyhow!("--guest <component.wasm> is required"))?,
        tests,
        out,
        only,
        jobs,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli()?;
    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("load guest component {}", cli.guest.display()))?;

    // The corpus-drift gates: the guest's corpus, the adapters' registries,
    // and the authoritative tests.toml (ids and tags) must all agree before
    // anything runs.
    let descriptors = list_tests(&engine, &component).await?;
    let ids: Vec<String> = descriptors.iter().map(|(id, _)| id.clone()).collect();
    verify_corpus(&ids, TESTS)?;
    let registry = std::fs::read_to_string(&cli.tests)
        .with_context(|| format!("read {}", cli.tests.display()))?;
    verify_registry(&descriptors, &registry)?;

    let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap()).await?;
    let base_url = server.base_url();
    let unreachable = unreachable_url().await?;
    eprintln!("echo server: {base_url}");

    let results: Vec<RawResult> = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        let engine = &engine;
        let component = &component;
        let base_url = base_url.clone();
        let unreachable = unreachable.clone();
        async move {
            conformance_adapter_common::run_test(test_id, TEST_TIMEOUT, async || {
                run_instance(
                    engine,
                    component,
                    test_id,
                    TestConfig {
                        server_url: base_url.clone(),
                        unreachable_url: unreachable.clone(),
                        max_inbound_buffer_bytes: CONFORMANCE_MAX_INBOUND_BUFFER_BYTES,
                    },
                )
                .await
            })
            .await
        }
    })
    .await?;

    let failed = results
        .iter()
        .filter(|r| matches!(r.status, conformance_adapter_common::RawStatus::Fail))
        .count();
    let report = AdapterReport {
        target: "wasmtime".to_string(),
        environment: "loopback".to_string(),
        guest: file_sha256(&cli.guest)?,
        results,
    };
    let path = write_report(&cli.out, "wasmtime", &report)?;
    eprintln!(
        "wrote {} ({} tests, {failed} failed)",
        path.display(),
        report.results.len()
    );
    Ok(())
}
