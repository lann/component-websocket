//! `ct-driver`: the wasmtime leg of the component-test conformance
//! harness.
//!
//! The component-test analog of `conformance/adapters/wasmtime`: it
//! embeds the suite echo server in-process, provisions the
//! `lann:websocket` host ([`wasmtime_websocket`]) with the suite bounds
//! (see `conformance-adapter-common` for why each bound exists), hands
//! the suite its configuration through the store environment
//! (`WS_CONFORMANCE_*`), and drives the suite with the component-test
//! runner — which owns scheduling, isolation (fresh instance per case),
//! the per-case budgets, and the results wire format.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context as _, Result};
use component_test_runner::{
    CtCtx, OutputMode, Runner, RunnerView, DEFAULT_CASE_EXECUTION_BUDGET_SECS,
};
use conformance_adapter_common::{
    CONFORMANCE_CLOSE_TIMEOUT, CONFORMANCE_CONNECT_TIMEOUT, CONFORMANCE_MAX_INBOUND_BUFFER_BYTES,
    TEST_TIMEOUT,
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_websocket::{WasiWebsocketCtx, WasiWebsocketCtxView, WasiWebsocketView};

/// Per-store host state: WASI (the suite reads its config from the
/// environment), the runner's diagnostic sink, and the websocket host.
struct Data {
    wasi: WasiCtx,
    table: ResourceTable,
    ct: CtCtx,
    websocket: WasiWebsocketCtx,
}

impl WasiView for Data {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl RunnerView for Data {
    fn ct(&mut self) -> &mut CtCtx {
        &mut self.ct
    }
}

impl WasiWebsocketView for Data {
    fn websocket(&mut self) -> WasiWebsocketCtxView<'_> {
        WasiWebsocketCtxView {
            ctx: &mut self.websocket,
            table: &mut self.table,
        }
    }
}

const USAGE: &str = "usage: ct-driver <suite.wasm> [--jsonl] [--jobs N] [--target key] \
                     [--only substring] [--case-timeout secs]";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let mut suite: Option<PathBuf> = None;
    let mut mode = OutputMode::Human;
    let mut jobs: usize = 1;
    let mut target: String = "wasmtime".into();
    let mut only: Option<String> = None;
    // The incumbent harness's per-test hang guard, kept as the wall
    // bound; the execution budget stays at the runner default (these
    // cases wait on loopback I/O, they don't compute).
    let mut case_timeout: u64 = TEST_TIMEOUT.as_secs();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--jsonl" => mode = OutputMode::Jsonl,
            "--jobs" => jobs = value("--jobs")?.parse().context("--jobs")?,
            "--target" => target = value("--target")?,
            "--only" => only = Some(value("--only")?),
            "--case-timeout" => {
                case_timeout = value("--case-timeout")?.parse().context("--case-timeout")?
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            _ if suite.is_none() => suite = Some(PathBuf::from(arg)),
            _ => bail!("unexpected argument `{arg}`\n{USAGE}"),
        }
    }
    let Some(suite) = suite else {
        bail!("{USAGE}");
    };
    let suite_name = suite
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "suite".into());

    // The echo server (and the unreachable-URL mint) live on their own
    // multi-thread runtime for the lifetime of the process; the runner's
    // per-worker current-thread runtimes drive the websocket host's own
    // I/O. The `RunningServer` moves into the parked future so its
    // drop-shutdown never fires.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(String, String)>>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let setup = async {
                let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap()).await?;
                let unreachable = conformance_adapter_common::unreachable_url().await?;
                Ok::<_, anyhow::Error>((server, unreachable))
            };
            match setup.await {
                Ok((server, unreachable)) => {
                    let _ = tx.send(Ok((server.base_url(), unreachable)));
                    // Keep the server alive until process exit.
                    let _server = server;
                    std::future::pending::<()>().await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
    });
    let (base_url, unreachable) = rx.recv().context("echo server setup")??;
    eprintln!("echo server: {base_url}");

    let buffer_bytes = CONFORMANCE_MAX_INBOUND_BUFFER_BYTES.to_string();
    let make_data = move || {
        let mut websocket = WasiWebsocketCtx::new();
        websocket.set_connect_timeout(CONFORMANCE_CONNECT_TIMEOUT);
        websocket.set_close_timeout(CONFORMANCE_CLOSE_TIMEOUT);
        websocket.set_max_inbound_buffer_bytes(CONFORMANCE_MAX_INBOUND_BUFFER_BYTES as usize);
        Data {
            wasi: WasiCtxBuilder::new()
                .env("WS_CONFORMANCE_SERVER_URL", &base_url)
                .env("WS_CONFORMANCE_UNREACHABLE_URL", &unreachable)
                .env("WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES", &buffer_bytes)
                .build(),
            table: ResourceTable::new(),
            ct: CtCtx::default(),
            websocket,
        }
    };

    let runner = Runner::with_data(&suite, make_data, |linker| {
        wasmtime_websocket::add_to_linker(linker)
    })?;
    let summary = wasmtime_wasi::runtime::in_tokio(runner.run_suite_opts(
        &suite_name,
        &target,
        mode,
        &[],
        1, // fresh instance per case, matching the incumbent isolation
        jobs,
        only.as_deref(),
        DEFAULT_CASE_EXECUTION_BUDGET_SECS,
        case_timeout,
    ))?;

    Ok(if summary.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
