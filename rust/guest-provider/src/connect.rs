//! `connect`: URL and subprotocol validation, name resolution, TCP (and
//! for `wss:` the composed `lann:tls` client), the opening handshake, and
//! pump spawn — all bounded by the configured connect timeout.
//!
//! Validation rules mirror the reference host implementation exactly; the
//! conformance rows `connect-invalid-url`, `connect-invalid-protocols`,
//! and the subprotocol quartet gate the match.

use std::rc::Rc;

use futures::channel::mpsc;
use futures::FutureExt as _;

use crate::bindings::exports::lann::websocket::connections::Error;
use crate::bindings::lann::tls::client::Connector;
use crate::bindings::wasi::clocks::monotonic_clock as clock;
use crate::bindings::wasi::sockets::ip_name_lookup::resolve_addresses;
use crate::bindings::wasi::sockets::types::{
    IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, TcpSocket,
};
use crate::config::Config;
use crate::handshake::{build_request, parse_response, MAX_RESPONSE_HEADER_BYTES};
use crate::pump::{spawn_pump, PumpArgs, Shared, TlsKeepalive, Transport};

/// Connect, negotiate, spawn the pump. Returns the shared state and the
/// negotiated subprotocol.
pub(crate) async fn connect(
    url: &str,
    protocols: &[String],
) -> Result<(Rc<Shared>, String), Error> {
    validate_url(url).map_err(Error::InvalidUrl)?;
    validate_protocols(protocols).map_err(Error::InvalidArgument)?;
    let parsed = url::Url::parse(url).map_err(|err| Error::InvalidUrl(err.to_string()))?;
    let config = Config::from_env();

    let deadline = clock::now() + config.connect_timeout.as_nanos() as u64;
    let attempt = establish(&parsed, protocols, &config);
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
        reader,
        writer,
        leftover,
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
    // The transport cap, mirroring the reference: inbound messages between
    // the buffer bound and the cap take the normal budget-overflow path;
    // past the cap, teardown is immediate.
    let max_frame_bytes = config
        .max_inbound_buffer_bytes
        .saturating_mul(2)
        .max(64 * 1024 * 1024);
    spawn_pump(PumpArgs {
        shared: Rc::clone(&shared),
        in_tx,
        cmd_rx,
        reader,
        writer,
        leftover,
        close_timeout_ns: config.close_timeout.as_nanos() as u64,
        max_frame_bytes,
        transport,
    });
    Ok((shared, negotiated))
}

/// An established, upgraded connection, pre-pump.
struct Established {
    reader: wit_bindgen::StreamReader<u8>,
    writer: wit_bindgen::StreamWriter<u8>,
    leftover: Vec<u8>,
    negotiated: String,
    transport: Transport,
}

/// Resolve, connect, (for `wss:`) run TLS, and complete the upgrade.
/// Every failure renders as a `connect-failed` diagnostic.
async fn establish(
    parsed: &url::Url,
    protocols: &[String],
    config: &Config,
) -> Result<Established, String> {
    let secure = parsed.scheme() == "wss";
    let host = parsed.host_str().unwrap_or_default().to_string();
    let default_port = if secure { 443 } else { 80 };
    let port = parsed.port().unwrap_or(default_port);

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

    let addresses = resolve(parsed, port).await?;
    let (socket, _local) = connect_first(&addresses).await?;

    // Transport streams: the socket transmit side is fed by a stream we
    // write into; the receive side hands us a stream to read.
    let (transport_tx, transport_tx_rx) = crate::bindings::wit_stream::new();
    let _tx_done = socket.send(transport_tx_rx);
    let (transport_rx, _rx_done) = socket.receive();

    let (reader, writer, tls) = if secure {
        let connector = Connector::new(&roots.unwrap_or_default());
        // Wire the transform pair straight into the socket: cleartext in
        // and out of the connector, ciphertext to and from the transport.
        let (app_tx, app_tx_rx) = crate::bindings::wit_stream::new();
        let (ciphertext_out, _send_done) = connector.send(app_tx_rx);
        // The ciphertext output feeds the transport transmit stream via a
        // pump task (stream ends cannot be spliced directly).
        wit_bindgen::spawn_local(pipe(ciphertext_out, transport_tx));
        let (app_rx, _recv_done) = connector.receive(transport_rx);
        let server_name = host.clone();
        connector
            .connect(server_name, Vec::new())
            .await
            .map_err(|err| format!("TLS handshake: {}", err.to_debug_string()))?;
        (app_rx, app_tx, Some(TlsKeepalive { connector }))
    } else {
        (transport_rx, transport_tx, None)
    };

    let mut reader = reader;
    let mut writer = writer;

    // The opening handshake.
    let host_header = host_header(&host, port, default_port);
    let path_and_query = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    };
    let request = build_request(&host_header, &path_and_query, protocols);
    let leftover = writer.write_all(request.bytes).await;
    if !leftover.is_empty() {
        return Err("connection closed while sending the upgrade request".to_string());
    }

    let (header, leftover) = read_response_header(&mut reader).await?;
    let response = parse_response(&header, &request.expected_accept)?;

    Ok(Established {
        reader,
        writer,
        leftover,
        negotiated: response.negotiated,
        transport: Transport {
            socket: Some(socket),
            tls,
        },
    })
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

/// Read until the end of the response header block (CRLFCRLF), returning
/// the header bytes and any leftover (the start of frame data).
async fn read_response_header(
    reader: &mut wit_bindgen::StreamReader<u8>,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        if let Some(end) = find_header_end(&buffer) {
            let leftover = buffer.split_off(end);
            return Ok((buffer, leftover));
        }
        if buffer.len() > MAX_RESPONSE_HEADER_BYTES {
            return Err("response header block is unreasonably large".to_string());
        }
        let (status, chunk) = reader.read(Vec::with_capacity(8 * 1024)).await;
        buffer.extend_from_slice(&chunk);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) && chunk.is_empty()
        {
            return Err("connection closed during the handshake".to_string());
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn host_header(host: &str, port: u16, default_port: u16) -> String {
    if port == default_port {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// Resolve the URL host to socket addresses (literal IPs skip lookup).
async fn resolve(parsed: &url::Url, port: u16) -> Result<Vec<IpSocketAddress>, String> {
    match parsed.host() {
        Some(url::Host::Ipv4(v4)) => Ok(vec![v4_addr(v4.octets(), port)]),
        Some(url::Host::Ipv6(v6)) => Ok(vec![v6_addr(v6.segments(), port)]),
        Some(url::Host::Domain(name)) => {
            let addresses = resolve_addresses(name.to_string())
                .await
                .map_err(|err| format!("name resolution failed: {err:?}"))?;
            if addresses.is_empty() {
                return Err(format!("{name:?} resolved to no addresses"));
            }
            Ok(addresses
                .into_iter()
                .map(|address| match address {
                    IpAddress::Ipv4(octets) => v4_addr(octets.into(), port),
                    IpAddress::Ipv6(segments) => v6_addr(segments.into(), port),
                })
                .collect())
        }
        None => Err("URL has no host".to_string()),
    }
}

fn v4_addr(octets: [u8; 4], port: u16) -> IpSocketAddress {
    let [a, b, c, d] = octets;
    IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port,
        address: (a, b, c, d),
    })
}

fn v6_addr(segments: [u16; 8], port: u16) -> IpSocketAddress {
    let [a, b, c, d, e, f, g, h] = segments;
    IpSocketAddress::Ipv6(Ipv6SocketAddress {
        port,
        flow_info: 0,
        scope_id: 0,
        address: (a, b, c, d, e, f, g, h),
    })
}

/// Try each address in order; the first successful connect wins.
async fn connect_first(addresses: &[IpSocketAddress]) -> Result<(TcpSocket, ()), String> {
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
            Ok(()) => return Ok((socket, ())),
            Err(err) => last_error = format!("connect: {err:?}"),
        }
    }
    Err(last_error)
}

/// URL validation per the WIT contract: absolute `ws:`/`wss:`, no
/// fragment, no userinfo. Mirrors the reference implementation's rules
/// (including rejecting a `#` anywhere in the raw string).
fn validate_url(url: &str) -> Result<(), String> {
    if url.contains('#') {
        return Err("URL must not have a fragment".to_string());
    }
    let parsed = url::Url::parse(url).map_err(|err| format!("URL does not parse: {err}"))?;
    match parsed.scheme() {
        "ws" | "wss" => {}
        other => return Err(format!("URL scheme must be ws or wss, not {other:?}")),
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("URL must have a host".to_string());
    }
    // The WHATWG WebSocket constructor rejects credentials in the URL.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL must not have userinfo".to_string());
    }
    Ok(())
}

/// Subprotocol offer validation: RFC 2616 tokens, no duplicates.
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
    let mut certificates = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            in_block = true;
            b64.clear();
        } else if line == "-----END CERTIFICATE-----" {
            if !in_block {
                return Err("TLS roots PEM: END without BEGIN".to_string());
            }
            in_block = false;
            certificates.push(base64_decode(&b64)?);
        } else if in_block {
            b64.push_str(line);
        }
    }
    if in_block {
        return Err("TLS roots PEM: unterminated certificate block".to_string());
    }
    if certificates.is_empty() {
        return Err("TLS roots PEM contains no certificates".to_string());
    }
    Ok(certificates)
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Result<u32, String> {
        match byte {
            b'A'..=b'Z' => Ok((byte - b'A') as u32),
            b'a'..=b'z' => Ok((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Ok((byte - b'0') as u32 + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            other => Err(format!("invalid base64 byte {other:#x}")),
        }
    }
    let stripped: Vec<u8> = input.bytes().filter(|b| *b != b'=').collect();
    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    for chunk in stripped.chunks(4) {
        let mut acc: u32 = 0;
        for &byte in chunk {
            acc = (acc << 6) | value(byte)?;
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]),
            3 => {
                acc <<= 6;
                out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8]);
            }
            2 => {
                acc <<= 12;
                out.push((acc >> 16) as u8);
            }
            _ => return Err("truncated base64".to_string()),
        }
    }
    Ok(out)
}
