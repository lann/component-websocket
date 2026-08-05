//! Conformance adapter for the composed in-guest provider target.
//!
//! It runs the *composed* component — the shared conformance guest with
//! its `lann:websocket/connections` import plugged by the in-guest
//! provider (which carries the `lann:tls` component for `wss:`) — under
//! plain wasmtime with WASI p2 + p3, and emits the adapter result
//! document the conformance runner consumes.
//!
//! The provider's knob channel is its environment (see the provider
//! README): the adapter sets the conformance bounds and the suite test
//! CA there, mirroring what the other adapters configure through their
//! implementations' own channels.
//!
//! The guest stamp is the *inner* shared guest component's hash — the
//! same build every other adapter stamps — so the runner's one-build
//! invariant keeps meaning across targets; the composition wrapper is
//! target machinery, like a transpile.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use wasmtime::component::{Accessor, Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
    });
}

use bindings::exports::conformance::suite::runner::{TestConfig, TestResult};
use bindings::Conformance;

/// Per-store host state: plain WASI only — the websocket implementation
/// lives inside the composed component.
struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
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

/// A fresh store with the conformance bounds and the suite trust anchor
/// provisioned through the provider's environment channel.
fn new_store(engine: &Engine) -> Store<Ctx> {
    let wasi = WasiCtxBuilder::new()
        .env(
            "LANN_WEBSOCKET_CONNECT_TIMEOUT_MS",
            CONFORMANCE_CONNECT_TIMEOUT.as_millis().to_string(),
        )
        .env(
            "LANN_WEBSOCKET_CLOSE_TIMEOUT_MS",
            CONFORMANCE_CLOSE_TIMEOUT.as_millis().to_string(),
        )
        .env(
            "LANN_WEBSOCKET_MAX_INBOUND_BUFFER_BYTES",
            CONFORMANCE_MAX_INBOUND_BUFFER_BYTES.to_string(),
        )
        .env(
            "LANN_WEBSOCKET_TLS_ROOTS_PEM",
            conformance_echod::TEST_CA_PEM,
        )
        // The provider's panics (deployment bugs) render on stderr.
        .inherit_stderr()
        .inherit_network()
        .allow_ip_name_lookup(true)
        .build();
    Store::new(
        engine,
        Ctx {
            wasi,
            table: ResourceTable::new(),
        },
    )
}

fn add_to_linker(engine: &Engine) -> Result<Linker<Ctx>> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Instantiate the composed component and drive one `run-test` call.
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

/// Fetch the composed component's `list-tests` for the corpus-drift
/// gates.
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
    /// The composed component to run.
    composed: PathBuf,
    /// The inner shared guest, for the report's guest stamp.
    guest: PathBuf,
    tests: PathBuf,
    out: PathBuf,
    only: Vec<String>,
    jobs: usize,
}

fn parse_cli() -> Result<Cli> {
    let mut composed = None;
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
            "--composed" => composed = Some(PathBuf::from(value("--composed")?)),
            "--guest" => guest = Some(PathBuf::from(value("--guest")?)),
            "--tests" => tests = PathBuf::from(value("--tests")?),
            "--out" => out = PathBuf::from(value("--out")?),
            "--only" => only.push(value("--only")?),
            "--jobs" => jobs = value("--jobs")?.parse().context("--jobs")?,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Cli {
        composed: composed.context("--composed is required")?,
        guest: guest.context("--guest is required")?,
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
    let component = Component::from_file(&engine, &cli.composed)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("load composed component {}", cli.composed.display()))?;

    let descriptors = list_tests(&engine, &component).await?;
    let ids: Vec<String> = descriptors.iter().map(|(id, _)| id.clone()).collect();
    verify_corpus(&ids, TESTS)?;
    let registry = std::fs::read_to_string(&cli.tests)
        .with_context(|| format!("read {}", cli.tests.display()))?;
    verify_registry(&descriptors, &registry)?;

    let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap()).await?;
    let base_url = server.base_url();
    let tls_base_url = server.tls_base_url();
    let unreachable = unreachable_url().await?;
    eprintln!("echo server: {base_url} (tls: {tls_base_url})");

    let results: Vec<RawResult> = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        let engine = &engine;
        let component = &component;
        let base_url = base_url.clone();
        let tls_base_url = tls_base_url.clone();
        let unreachable = unreachable.clone();
        async move {
            conformance_adapter_common::run_test(test_id, TEST_TIMEOUT, async || {
                run_instance(
                    engine,
                    component,
                    test_id,
                    TestConfig {
                        server_url: base_url.clone(),
                        tls_server_url: tls_base_url.clone(),
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

    let report = AdapterReport {
        target: "composed".to_string(),
        environment: "loopback".to_string(),
        guest: file_sha256(&cli.guest)?,
        results,
    };
    write_report(&cli.out, "composed", &report)?;
    Ok(())
}
