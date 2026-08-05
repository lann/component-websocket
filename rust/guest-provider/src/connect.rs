//! `connect`: URL and subprotocol validation, name resolution, TCP (and
//! for `wss:` the composed `lann:tls` client), the opening handshake, and
//! pump spawn — all bounded by the configured connect timeout.
//!
//! The handshake itself is tungstenite's client handshake (key
//! generation, accept-key verification, 101 validation) driven over the
//! in-memory transport; validation rules and post-handshake subprotocol
//! checks mirror the reference host implementation line for line. The
//! conformance rows `connect-invalid-url`, `connect-invalid-protocols`,
//! and the subprotocol quartet gate the match.

use std::rc::Rc;

use futures::channel::mpsc;
use futures::FutureExt as _;
use tungstenite::client::IntoClientRequest as _;
use tungstenite::handshake::client::ClientHandshake;
use tungstenite::handshake::{HandshakeError, MidHandshake};
use tungstenite::http;

use crate::bindings::lann::tls::client::Connector;
use crate::bindings::lann::websocket::types::Error;
use crate::bindings::wasi::clocks::monotonic_clock as clock;
use crate::bindings::wasi::sockets::ip_name_lookup::resolve_addresses;
use crate::bindings::wasi::sockets::types::{
    IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, TcpSocket,
};
use crate::config::Config;
use crate::io::{IoHandle, VirtualIo};
use crate::pump::{spawn_pump, PumpArgs, Shared, TlsKeepalive, Transport};

/// Connect, negotiate, spawn the pump. Returns the shared state and the
/// negotiated subprotocol.
pub(crate) async fn connect(
    url: &str,
    protocols: &[String],
) -> Result<(Rc<Shared>, String), Error> {
    validate_url(url).map_err(Error::InvalidUrl)?;
    validate_protocols(protocols).map_err(Error::InvalidArgument)?;
    let config = Config::from_env();

    let deadline = clock::now() + config.connect_timeout.as_nanos() as u64;
    let attempt = establish(url, protocols, &config);
    futures::pin_mut!(attempt);
    let established = futures::select_biased! {
        result = attempt.fuse() => result.map_err(Error::ConnectFailed)?,
        () = clock::wait_until(deadline).fuse() => {
            return Err(Error::ConnectFailed(format!(
                "handshake timed out after {:?}",
                config.connect_timeout
            )));
        }
    };
    let Established {
        websocket,
        handle,
        reader,
        writer,
        negotiated,
        transport,
    } = established;

    // Post-handshake subprotocol validation, mirroring the reference: a
    // failed check discards the connection and fails the connect.
    if !protocols.is_empty() && !protocols.contains(&negotiated) {
        return Err(Error::ConnectFailed(if negotiated.is_empty() {
            "server selected no subprotocol although one was offered".to_string()
        } else {
            format!("server selected subprotocol {negotiated:?} which was not offered")
        }));
    }
    if protocols.is_empty() && !negotiated.is_empty() {
        return Err(Error::ConnectFailed(format!(
            "server selected subprotocol {negotiated:?} although none was offered"
        )));
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded();
    let (in_tx, in_rx) = mpsc::unbounded();
    let shared = Rc::new(Shared::new(cmd_tx, in_rx, config.max_inbound_buffer_bytes));
    spawn_pump(PumpArgs {
        shared: Rc::clone(&shared),
        in_tx,
        cmd_rx,
        websocket,
        handle,
        reader,
        writer,
        close_timeout_ns: config.close_timeout.as_nanos() as u64,
        transport,
    });
    Ok((shared, negotiated))
}

/// An established, upgraded connection, pre-pump.
struct Established {
    websocket: tungstenite::WebSocket<VirtualIo>,
    handle: IoHandle,
    reader: wit_bindgen::StreamReader<u8>,
    writer: wit_bindgen::StreamWriter<u8>,
    negotiated: String,
    transport: Transport,
}

/// Resolve, connect, (for `wss:`) run TLS, and complete the upgrade.
/// Every failure renders as a `connect-failed` diagnostic.
async fn establish(
    url: &str,
    protocols: &[String],
    config: &Config,
) -> Result<Established, String> {
    let uri: http::Uri = url.parse().map_err(|err| format!("URL: {err}"))?;
    let secure = uri.scheme_str() == Some("wss");
    let host = uri.host().unwrap_or_default().to_string();
    let default_port = if secure { 443 } else { 80 };
    let port = uri.port_u16().unwrap_or(default_port);

    // For wss:, fail closed before any network activity when no trust
    // anchors are configured (there is no ambient root store in-guest).
    let roots = if secure {
        match &config.tls_roots_pem {
            Some(pem) => Some(parse_pem_certificates(pem)?),
            None => {
                return Err("no TLS trust anchors configured; wss: fails closed (set \
                     LANN_WEBSOCKET_TLS_ROOTS_PEM; see the provider README)"
                    .to_string())
            }
        }
    } else {
        None
    };

    let addresses = resolve(&host, port).await?;
    let socket = connect_first(&addresses).await?;

    // Transport streams: the socket transmit side is fed by a stream we
    // write into; the receive side hands us a stream to read.
    let (transport_tx, transport_tx_rx) = crate::bindings::wit_stream::new();
    let _tx_done = socket.send(transport_tx_rx);
    let (transport_rx, _rx_done) = socket.receive();

    let (mut reader, writer, tls) = if secure {
        let connector = Connector::new(&roots.unwrap_or_default());
        // Wire the transform pair straight into the socket: cleartext in
        // and out of the connector, ciphertext to and from the transport.
        let (app_tx, app_tx_rx) = crate::bindings::wit_stream::new();
        let (ciphertext_out, _send_done) = connector.send(app_tx_rx);
        // The ciphertext output feeds the transport transmit stream via a
        // pump task (stream ends cannot be spliced directly).
        wit_bindgen::spawn_local(pipe(ciphertext_out, transport_tx));
        let (app_rx, _recv_done) = connector.receive(transport_rx);
        connector
            .connect(host.clone(), Vec::new())
            .await
            .map_err(|err| format!("TLS handshake: {}", err.to_debug_string()))?;
        (app_rx, app_tx, Some(TlsKeepalive { connector }))
    } else {
        (transport_rx, transport_tx, None)
    };
    let mut writer = writer;

    // The upgrade request, built exactly as the reference host builds it:
    // tungstenite's request derivation plus one joined
    // Sec-WebSocket-Protocol header for a non-empty offer.
    let mut request = url
        .into_client_request()
        .map_err(|err| format!("request: {err}"))?;
    if !protocols.is_empty() {
        let joined = protocols.join(", ");
        request.headers_mut().insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            joined
                .parse()
                .map_err(|_| "subprotocol offer is not a valid header value".to_string())?,
        );
    }
    let ws_config = tungstenite_config(config);

    // Drive tungstenite's sans-IO client handshake over the virtual
    // transport: flush what it wrote, feed what the peer sent, retry.
    let handle = IoHandle::new();
    let mut pending: MidHandshake<ClientHandshake<VirtualIo>> =
        ClientHandshake::start(handle.io(), request, Some(ws_config))
            .map_err(|err| format!("handshake: {err}"))?;
    let (websocket, response) = loop {
        match pending.handshake() {
            Ok(done) => break done,
            Err(HandshakeError::Interrupted(mid)) => {
                pending = mid;
                let outbound = handle.drain_outbound();
                if !outbound.is_empty() {
                    if !writer.write_all(outbound).await.is_empty() {
                        return Err(
                            "connection closed while sending the upgrade request".to_string()
                        );
                    }
                    continue;
                }
                let (status, chunk) = reader.read(Vec::with_capacity(8 * 1024)).await;
                handle.feed(&chunk);
                if matches!(
                    status,
                    wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
                ) && chunk.is_empty()
                {
                    return Err("connection closed during the handshake".to_string());
                }
            }
            Err(HandshakeError::Failure(err)) => {
                return Err(format!("handshake: {err}"));
            }
        }
    };

    let negotiated = response
        .headers()
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    Ok(Established {
        websocket,
        handle,
        reader,
        writer,
        negotiated,
        transport: Transport {
            socket: Some(socket),
            tls,
        },
    })
}

/// The transport caps, mirroring the reference: inbound messages between
/// the buffer bound and the cap take the normal budget-overflow path;
/// past the cap, teardown is immediate.
fn tungstenite_config(config: &Config) -> tungstenite::protocol::WebSocketConfig {
    let transport_cap = config
        .max_inbound_buffer_bytes
        .saturating_mul(2)
        .max(64 * 1024 * 1024);
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(transport_cap))
        .max_frame_size(Some(transport_cap))
}

/// Pipe one byte stream into another (the TLS ciphertext output into the
/// socket transmit stream).
async fn pipe(mut from: wit_bindgen::StreamReader<u8>, mut into: wit_bindgen::StreamWriter<u8>) {
    loop {
        let (status, chunk) = from.read(Vec::with_capacity(16 * 1024)).await;
        if !chunk.is_empty() && !into.write_all(chunk).await.is_empty() {
            return;
        }
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            return;
        }
    }
}

/// Resolve the URL host to socket addresses (literal IPs skip lookup).
/// `http::Uri` keeps IPv6 literals bracketed; strip for parsing.
async fn resolve(host: &str, port: u16) -> Result<Vec<IpSocketAddress>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(vec![ip_addr(ip, port)]);
    }
    let addresses = resolve_addresses(host.to_string())
        .await
        .map_err(|err| format!("name resolution failed: {err:?}"))?;
    if addresses.is_empty() {
        return Err(format!("{host:?} resolved to no addresses"));
    }
    Ok(addresses
        .into_iter()
        .map(|address| match address {
            IpAddress::Ipv4((a, b, c, d)) => ip_addr(std::net::IpAddr::from([a, b, c, d]), port),
            IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
                ip_addr(std::net::IpAddr::from([a, b, c, d, e, f, g, h]), port)
            }
        })
        .collect())
}

fn ip_addr(ip: std::net::IpAddr, port: u16) -> IpSocketAddress {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port,
                address: (a, b, c, d),
            })
        }
        std::net::IpAddr::V6(v6) => {
            let [a, b, c, d, e, f, g, h] = v6.segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port,
                flow_info: 0,
                scope_id: 0,
                address: (a, b, c, d, e, f, g, h),
            })
        }
    }
}

/// Try each address in order; the first successful connect wins.
async fn connect_first(addresses: &[IpSocketAddress]) -> Result<TcpSocket, String> {
    let mut last_error = "no addresses to connect to".to_string();
    for address in addresses {
        let family = match address {
            IpSocketAddress::Ipv4(_) => IpAddressFamily::Ipv4,
            IpSocketAddress::Ipv6(_) => IpAddressFamily::Ipv6,
        };
        let socket = match TcpSocket::create(family) {
            Ok(socket) => socket,
            Err(err) => {
                last_error = format!("create socket: {err:?}");
                continue;
            }
        };
        match socket.connect(*address).await {
            Ok(()) => return Ok(socket),
            Err(err) => last_error = format!("connect: {err:?}"),
        }
    }
    Err(last_error)
}

/// URL validation per the WIT contract: absolute `ws:`/`wss:`, no
/// fragment, no userinfo. A line-for-line mirror of the reference host's
/// `validate_url` (same parser, same messages).
fn validate_url(url: &str) -> Result<(), String> {
    if url.contains('#') {
        return Err("URL must not have a fragment".to_string());
    }
    let uri: http::Uri = url
        .parse()
        .map_err(|err| format!("URL does not parse: {err}"))?;
    match uri.scheme_str() {
        Some("ws") | Some("wss") => {}
        Some(other) => return Err(format!("URL scheme must be ws or wss, not {other:?}")),
        None => return Err("URL must be absolute (ws: or wss:)".to_string()),
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err("URL must have a host".to_string());
    }
    // The WHATWG WebSocket constructor rejects credentials in the URL.
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err("URL must not have userinfo".to_string());
    }
    Ok(())
}

/// Subprotocol offer validation: RFC 2616 tokens, no duplicates. A
/// mirror of the reference host's `validate_protocols`.
fn validate_protocols(protocols: &[String]) -> Result<(), String> {
    for (index, protocol) in protocols.iter().enumerate() {
        if !is_valid_protocol_token(protocol) {
            return Err(format!("invalid subprotocol {protocol:?}"));
        }
        if protocols[..index].contains(protocol) {
            return Err(format!("subprotocol {protocol:?} offered twice"));
        }
    }
    Ok(())
}

fn is_valid_protocol_token(protocol: &str) -> bool {
    !protocol.is_empty()
        && protocol
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && !br#"()<>@,;:\"/[]?={} "#.contains(&byte))
}

/// Parse a PEM bundle into DER certificates (for the TLS connector's
/// trust anchors).
fn parse_pem_certificates(pem: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    let mut certificates = Vec::new();
    for cert in rustls_pemfile::certs(&mut cursor) {
        let cert = cert.map_err(|err| format!("TLS roots PEM: {err}"))?;
        certificates.push(cert.as_ref().to_vec());
    }
    if certificates.is_empty() {
        return Err("TLS roots PEM contains no certificates".to_string());
    }
    Ok(certificates)
}
