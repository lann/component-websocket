//! `ct-driver`: the wasmtime leg of the component-test conformance
//! harness.
//!
//! Embeds the suite echo server in-process, provisions the
//! `lann:websocket` host ([`wasmtime_websocket`]) with the suite bounds
//! (documented on the consts below), hands
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
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_websocket::{WasiWebsocketCtx, WasiWebsocketCtxView, WasiWebsocketView};

/// The suite bounds, applied identically by every leg (the jco legs
/// hardcode the same values through the host module's hooks):
///
/// - connect timeout 5s: `connect/timeout` holds a `/stall` handshake
///   and must fail within the bound, not hang;
/// - close timeout 3s: `close/handshake-timeout` and
///   `close/under-send-backpressure` terminate only because the host
///   gives up on an unanswered close;
/// - inbound buffer 256 KiB: the overflow rows flood `4 x bound`, so
///   the guest's stimulus is derived from exactly this value (it rides
///   the store environment as WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES);
/// - 60s wall bound per case (the runner's `--case-timeout`): the
///   single-attempt hang guard — no retries, a wedged case is reported,
///   never masked.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_INBOUND_BUFFER_BYTES: u32 = 256 * 1024;
const CASE_TIMEOUT_SECS: u64 = 60;

/// A loopback `ws:` URL whose connect attempt should be refused: a port
/// that was just bound and released. The window between release and use
/// is small but real; a collision surfaces as a `connect/refused`
/// failure, not a silent pass.
fn unreachable_url() -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(format!("ws://127.0.0.1:{port}/echo"))
}

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
                     [--only substring] [--case-timeout secs] [--composed]";

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
    // --composed: the suite wasm embeds the in-guest provider (wac
    // composition); link full WASI p2+p3 instead of the websocket host,
    // and provision the provider through its environment channel.
    let mut composed = false;
    // The incumbent harness's per-test hang guard, kept as the wall
    // bound; the execution budget stays at the runner default (these
    // cases wait on loopback I/O, they don't compute).
    let mut case_timeout: u64 = CASE_TIMEOUT_SECS;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--jsonl" => mode = OutputMode::Jsonl,
            "--composed" => composed = true,
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

    // The echo server lives on its own multi-thread runtime for the
    // lifetime of the process; the runner's
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
            match conformance_echod::spawn("127.0.0.1:0".parse().unwrap()).await {
                Ok(server) => {
                    let _ = tx.send(Ok((server.base_url(), server.tls_base_url())));
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
    let (base_url, tls_base_url) = rx.recv().context("echo server setup")??;
    let unreachable = unreachable_url()?;
    eprintln!("echo server: {base_url} (tls: {tls_base_url})");

    let buffer_bytes = MAX_INBOUND_BUFFER_BYTES.to_string();
    let make_data = move || {
        let mut websocket = WasiWebsocketCtx::new();
        websocket.set_connect_timeout(CONNECT_TIMEOUT);
        websocket.set_close_timeout(CLOSE_TIMEOUT);
        websocket.set_max_inbound_buffer_bytes(MAX_INBOUND_BUFFER_BYTES as usize);
        // The suite's TLS listener terminates with the committed test PKI.
        websocket.set_extra_tls_roots_pem(conformance_echod::TEST_CA_PEM);
        let mut wasi = WasiCtxBuilder::new();
        wasi.env("WS_CONFORMANCE_SERVER_URL", &base_url)
            .env("WS_CONFORMANCE_TLS_SERVER_URL", &tls_base_url)
            .env("WS_CONFORMANCE_UNREACHABLE_URL", &unreachable)
            .env("WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES", &buffer_bytes);
        if composed {
            // The in-guest provider's knob channel is its environment
            // (see rust/guest-provider/README.md): the same conformance
            // bounds the hosted legs configure, plus the test CA, plus
            // real network access for its wasi:sockets imports.
            wasi.env(
                "LANN_WEBSOCKET_CONNECT_TIMEOUT_MS",
                CONNECT_TIMEOUT.as_millis().to_string(),
            )
            .env(
                "LANN_WEBSOCKET_CLOSE_TIMEOUT_MS",
                CLOSE_TIMEOUT.as_millis().to_string(),
            )
            .env("LANN_WEBSOCKET_MAX_INBOUND_BUFFER_BYTES", &buffer_bytes)
            .env(
                "LANN_WEBSOCKET_TLS_ROOTS_PEM",
                conformance_echod::TEST_CA_PEM,
            )
            .inherit_network()
            .allow_ip_name_lookup(true);
        }
        Data {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            ct: CtCtx::default(),
            websocket,
        }
    };

    let runner = Runner::with_data(&suite, make_data, move |linker| {
        if composed {
            // The composition serves lann:websocket internally over
            // wasi:sockets; the leg links WASI p3 for those imports.
            wasmtime_wasi::p3::add_to_linker(linker)
        } else {
            wasmtime_websocket::add_to_linker(linker)
        }
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
