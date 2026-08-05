//! The suite-owned WebSocket echo/reference server.
//!
//! Deliberately separate from any production code so it can evolve with the
//! tests and be discarded without API cost. The wire contract — every path,
//! query parameter, and fault mode — is documented in `PROTOCOL.md`; the
//! conformance guest builds URLs against it and this file implements them.
//!
//! The server is embeddable ([`spawn`]) for in-process adapters and runnable
//! as a binary (`conformance-echod`) for out-of-process ones.
//!
//! The demo runners consume it too; their stable surface is deliberately
//! small: [`spawn`]/[`RunningServer`], the `/echo` mode, and the binary's
//! `LISTENING` line. Everything else may change with the tests.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONNECTION, SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, Role};
use tokio_tungstenite::WebSocketStream;

/// The committed test PKI (see `tls/` beside this crate and
/// `scripts/gen-test-tls.sh`). TEST MATERIAL ONLY: the leaf key is public
/// by design, and nothing outside loopback conformance runs must ever
/// trust `TEST_CA_PEM`.
pub const TEST_CA_PEM: &str = include_str!("../tls/ca.pem");
const TEST_LEAF_PEM: &str = include_str!("../tls/leaf.pem");
const TEST_LEAF_KEY_PEM: &str = include_str!("../tls/leaf.key.pem");

/// How long `/ignore-close` holds the raw connection open after the client's
/// close frame goes unanswered, so a misbehaving test cannot leak the socket
/// forever.
const IGNORE_CLOSE_HOLD: Duration = Duration::from_secs(120);

/// How long `/stall` leaves the handshake unanswered.
const STALL_HOLD: Duration = Duration::from_secs(120);

/// A running echo server; dropping it (or calling [`shutdown`]) stops the
/// accept loops.
///
/// [`shutdown`]: RunningServer::shutdown
pub struct RunningServer {
    addr: SocketAddr,
    tls_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    tls_shutdown: Option<oneshot::Sender<()>>,
}

impl RunningServer {
    /// The bound plain-TCP address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The bound TLS address.
    pub fn tls_addr(&self) -> SocketAddr {
        self.tls_addr
    }

    /// The `ws:` base URL tests build their connect URLs against.
    pub fn base_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// The `wss:` base URL, terminated with the committed test PKI
    /// ([`TEST_CA_PEM`] is the trust anchor).
    pub fn tls_base_url(&self) -> String {
        format!("wss://{}", self.tls_addr)
    }

    /// Stop accepting connections. In-flight connections finish on their
    /// own.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.tls_shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.tls_shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Bind `addr` (use port 0 for an ephemeral port) plus a TLS listener on
/// the same host, and serve the protocol on both in the background until
/// the returned handle is dropped or shut down. Both listeners serve the
/// identical endpoint tree; the TLS one terminates with the committed
/// test PKI.
pub async fn spawn(addr: SocketAddr) -> anyhow::Result<RunningServer> {
    let listener = TcpListener::bind(addr).await.context("bind echo server")?;
    let addr = listener.local_addr()?;
    let tls_listener = TcpListener::bind((addr.ip(), 0))
        .await
        .context("bind TLS echo server")?;
    let tls_addr = tls_listener.local_addr()?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_server_config()?));

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { continue };
                    tokio::spawn(serve_stream(stream));
                }
            }
        }
    });

    let (tls_shutdown_tx, mut tls_shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut tls_shutdown_rx => break,
                accepted = tls_listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { continue };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        // A failed TLS handshake is the client's business
                        // to observe (some tests provoke exactly that).
                        let Ok(stream) = acceptor.accept(stream).await else { return };
                        serve_stream(stream).await;
                    });
                }
            }
        }
    });

    Ok(RunningServer {
        addr,
        tls_addr,
        shutdown: Some(shutdown_tx),
        tls_shutdown: Some(tls_shutdown_tx),
    })
}

/// Serve one accepted connection (plain or TLS) with the shared handler.
async fn serve_stream<S>(stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service_fn(handle_request))
        .with_upgrades();
    // Connection errors are the client's business to observe; the server
    // stays up.
    let _ = conn.await;
}

/// The TLS server configuration from the committed test PKI.
fn tls_server_config() -> anyhow::Result<rustls::ServerConfig> {
    let certs = rustls_pemfile::certs(&mut TEST_LEAF_PEM.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse test leaf certificate")?;
    let key = rustls_pemfile::private_key(&mut TEST_LEAF_KEY_PEM.as_bytes())
        .context("parse test leaf key")?
        .context("no private key in test leaf key PEM")?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS server config")
}

/// A parsed fault/echo mode. See `PROTOCOL.md`.
#[derive(Clone, Debug)]
enum Mode {
    Echo,
    CloseAfter {
        count: u32,
        code: Option<u16>,
        reason: String,
    },
    BurstThenClose {
        count: u32,
        size: u32,
        code: Option<u16>,
        reason: String,
    },
    Burst {
        count: u32,
        size: u32,
    },
    BurstOnMessage {
        count: u32,
        size: u32,
    },
    BurstThenIgnore {
        count: u32,
        size: u32,
    },
    AbruptClose {
        after: u32,
    },
    IgnoreClose,
    Blackhole,
}

/// Query parameters, parsed without percent-decoding (the protocol keeps
/// values token-safe; see `PROTOCOL.md`).
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn parse_mode(path: &str, query: &str) -> Option<Mode> {
    let int = |key: &str, default: u32| -> u32 {
        query_param(query, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let code = query_param(query, "code").and_then(|v| v.parse::<u16>().ok());
    let reason = query_param(query, "reason").unwrap_or("").to_owned();
    match path {
        "/echo" => Some(Mode::Echo),
        "/close-after" => Some(Mode::CloseAfter {
            count: int("count", 0),
            code,
            reason,
        }),
        "/burst-then-close" => Some(Mode::BurstThenClose {
            count: int("count", 1),
            size: int("size", 16),
            code,
            reason,
        }),
        "/burst" => Some(Mode::Burst {
            count: int("count", 1),
            size: int("size", 16),
        }),
        "/burst-on-message" => Some(Mode::BurstOnMessage {
            count: int("count", 1),
            size: int("size", 16),
        }),
        "/burst-then-ignore" => Some(Mode::BurstThenIgnore {
            count: int("count", 1),
            size: int("size", 16),
        }),
        "/abrupt-close" => Some(Mode::AbruptClose {
            after: int("after", 0),
        }),
        "/ignore-close" => Some(Mode::IgnoreClose),
        "/blackhole" => Some(Mode::Blackhole),
        _ => None,
    }
}

fn status_only(status: StatusCode) -> Response<Empty<Bytes>> {
    let mut response = Response::new(Empty::new());
    *response.status_mut() = status;
    response
}

async fn handle_request(mut req: Request<Incoming>) -> anyhow::Result<Response<Empty<Bytes>>> {
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    if path == "/healthz" {
        return Ok(status_only(StatusCode::OK));
    }
    if path == "/reject" {
        // A well-formed upgrade request answered with a plain HTTP error:
        // the client observes a failed handshake.
        return Ok(status_only(StatusCode::FORBIDDEN));
    }
    if path == "/redirect" {
        // Answer the upgrade with a redirect toward /echo. WebSocket
        // clients must not follow it (RFC 6455 leaves redirects to the
        // client, and browsers fail the connection), so the target is
        // deliberately valid: a client that connected anyway would pass
        // the echo behavior and fail the row.
        let mut response = status_only(StatusCode::FOUND);
        response
            .headers_mut()
            .insert(hyper::header::LOCATION, HeaderValue::from_static("/echo"));
        return Ok(response);
    }
    if path == "/stall" {
        // Never answer the handshake (bounded so a stuck test cannot leak
        // the socket forever); the client's connect bound must fire first.
        tokio::time::sleep(STALL_HOLD).await;
        return Ok(status_only(StatusCode::INTERNAL_SERVER_ERROR));
    }

    let Some(mode) = parse_mode(&path, &query) else {
        return Ok(status_only(StatusCode::NOT_FOUND));
    };

    // Minimal upgrade validation: enough to serve conforming clients and
    // reject plain GETs.
    let is_upgrade = req
        .headers()
        .get(UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let key = req
        .headers()
        .get("Sec-WebSocket-Key")
        .map(|v| v.as_bytes().to_owned());
    let (Some(key), true) = (key, is_upgrade) else {
        return Ok(status_only(StatusCode::BAD_REQUEST));
    };

    // Subprotocol selection: `protocol=NAME` selects NAME if the client
    // offered it; `force-protocol=NAME` selects NAME unconditionally (to
    // probe client-side enforcement).
    let offered: Vec<String> = req
        .headers()
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|token| token.trim().to_owned())
        .collect();
    let selected: Option<String> = if let Some(force) = query_param(&query, "force-protocol") {
        Some(force.to_owned())
    } else if let Some(want) = query_param(&query, "protocol") {
        offered
            .iter()
            .any(|offer| offer == want)
            .then(|| want.to_owned())
    } else if offered.iter().any(|offer| offer == "echo") {
        // The WPT echo handler's contract: an offer containing exactly
        // "echo" is accepted as "echo" without any query parameter (the
        // vendored WPT suites rely on it).
        Some("echo".to_owned())
    } else {
        None
    };

    let mut response = Response::new(Empty::new());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert(UPGRADE, HeaderValue::from_static("websocket"));
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    response.headers_mut().insert(
        "Sec-WebSocket-Accept",
        HeaderValue::from_str(&derive_accept_key(&key))?,
    );
    if let Some(protocol) = &selected {
        response
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_str(protocol)?);
    }

    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(upgraded) = upgrade.await else { return };
        let io = TokioIo::new(upgraded);
        if matches!(mode, Mode::IgnoreClose) {
            // Raw mode: never speak WebSocket back, so the client's close
            // frame goes deliberately unanswered.
            run_ignore_close(io).await;
            return;
        }
        if matches!(mode, Mode::Blackhole) {
            // Raw mode: neither read nor write, so the peer's send buffers
            // fill and its writes stall. Bounded so a stuck test cannot
            // leak the socket forever.
            tokio::time::sleep(IGNORE_CLOSE_HOLD).await;
            return;
        }
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        run_mode(ws, mode).await;
    });

    Ok(response)
}

type ServerWs = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

/// The deterministic burst payload: `size` bytes, each `(index + offset) %
/// 256`, so clients can verify content without carrying it in the test.
fn burst_payload(index: u32, size: u32) -> Vec<u8> {
    (0..size).map(|i| ((index + i) % 256) as u8).collect()
}

fn close_frame(code: Option<u16>, reason: &str) -> Option<CloseFrame> {
    code.map(|code| CloseFrame {
        code: code.into(),
        reason: reason.to_owned().into(),
    })
}

/// Read until the connection ends, echoing nothing. Returns when the client
/// closes (tungstenite completes the handshake) or the transport drops.
async fn drain(ws: &mut ServerWs) {
    loop {
        match ws.next().await {
            Some(Ok(Message::Ping(_))) => {
                let _ = ws.flush().await;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => break,
        }
    }
}

async fn run_mode(mut ws: ServerWs, mode: Mode) {
    match mode {
        Mode::Echo => {
            loop {
                match ws.next().await {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                        // The WPT echo handler's contract: "Goodbye" is
                        // echoed and then the server initiates a normal
                        // close (the vendored WPT suites rely on it).
                        let goodbye =
                            matches!(&message, Message::Text(text) if text.as_str() == "Goodbye");
                        if ws.send(message).await.is_err() {
                            break;
                        }
                        if goodbye {
                            let _ = ws.send(Message::Close(close_frame(Some(1000), ""))).await;
                            drain(&mut ws).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // tungstenite queues the pong automatically.
                        let _ = ws.flush().await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
        Mode::CloseAfter {
            count,
            code,
            reason,
        } => {
            let mut echoed = 0;
            while echoed < count {
                match ws.next().await {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                        if ws.send(message).await.is_err() {
                            return;
                        }
                        echoed += 1;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                }
            }
            let _ = ws.send(Message::Close(close_frame(code, &reason))).await;
            drain(&mut ws).await;
        }
        Mode::BurstThenClose {
            count,
            size,
            code,
            reason,
        } => {
            for index in 0..count {
                if ws
                    .send(Message::binary(burst_payload(index, size)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = ws.send(Message::Close(close_frame(code, &reason))).await;
            drain(&mut ws).await;
        }
        Mode::Burst { count, size } => {
            for index in 0..count {
                if ws
                    .send(Message::binary(burst_payload(index, size)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            drain(&mut ws).await;
        }
        Mode::BurstOnMessage { count, size } => {
            // Wait for the client's trigger message, so the client can have
            // a receive pending before the burst arrives.
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                }
            }
            for index in 0..count {
                if ws
                    .send(Message::binary(burst_payload(index, size)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            drain(&mut ws).await;
        }
        Mode::BurstThenIgnore { count, size } => {
            for index in 0..count {
                if ws
                    .send(Message::binary(burst_payload(index, size)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Never read again: the client's close frame goes unanswered
            // (reading it would trigger tungstenite's automatic close
            // reply). Bounded so a stuck test cannot leak the socket.
            tokio::time::sleep(IGNORE_CLOSE_HOLD).await;
        }
        Mode::AbruptClose { after } => {
            let mut echoed = 0;
            while echoed < after {
                match ws.next().await {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                        if ws.send(message).await.is_err() {
                            return;
                        }
                        echoed += 1;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                }
            }
            // Drop without a close frame: the client observes an abnormal
            // closure (TCP FIN with no closing handshake).
        }
        Mode::IgnoreClose | Mode::Blackhole => unreachable!("handled at the raw layer"),
    }
}

/// Hold the raw connection open, reading and discarding bytes, so the
/// client's close frame is never answered. Bounded so sockets cannot leak
/// past a stuck test.
async fn run_ignore_close(io: TokioIo<hyper::upgrade::Upgraded>) {
    let mut io = io;
    let mut buf = [0u8; 4096];
    let _ = tokio::time::timeout(IGNORE_CLOSE_HOLD, async {
        loop {
            match io.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
}
