//! The case bodies: ported verbatim from the incumbent conformance
//! guest (same assertions, same stimulus, same one-line failure
//! details), re-plumbed for the component-test suite:
//!
//! - `TestConfig` is a local struct populated once from the store
//!   environment (`WS_CONFORMANCE_*`, set by the driver) instead of a
//!   WIT-record argument — the tests contract has no per-case config
//!   channel, and the environment is the wasip2-native equivalent.
//! - The `conformance:suite/runner` export, `CORPUS` mirror, and
//!   `list-tests` are gone: the component-test SDK owns the export
//!   surface and the inventory (lockfile + runner drift cross-check
//!   replace the four-way corpus mirror).
//!
//! Dispatch is keyed by the incumbent's flat case ids (`connect-basic`);
//! the `#[case]` delegators in [`crate`] map them onto the
//! component-test case-name hierarchy.

use crate::bindings;
use crate::bindings::lann::websocket::connections::Websocket;
use crate::bindings::lann::websocket::types::{Error, Message, MessageKind, WebsocketState};

/// The per-run configuration (the incumbent harness passed this as a
/// WIT record), read once from the store environment.
pub struct TestConfig {
    pub server_url: String,
    pub unreachable_url: String,
    pub max_inbound_buffer_bytes: u32,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("harness bug: {name} is not set — the driver must configure the store environment")
    })
}

pub fn config() -> &'static TestConfig {
    static CONFIG: std::sync::LazyLock<TestConfig> = std::sync::LazyLock::new(|| TestConfig {
        server_url: env("WS_CONFORMANCE_SERVER_URL"),
        unreachable_url: env("WS_CONFORMANCE_UNREACHABLE_URL"),
        max_inbound_buffer_bytes: env("WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES")
            .parse()
            .expect("harness bug: WS_CONFORMANCE_MAX_INBOUND_BUFFER_BYTES is not a u32"),
    });
    &CONFIG
}

/// Adapt one incumbent case body to a component-test verdict: the old
/// ids stay as the dispatch keys, so every delegating `#[case]` names
/// the incumbent row it ports.
pub async fn case(test_id: &str) -> component_test_sdk::Verdict {
    run(test_id, config())
        .await
        .map_err(component_test_sdk::Failure::Failed)
}

/// Message count and payload size for a count-parameterized test. The
/// guest owns its stimulus (drivers cannot scale it), so every target
/// runs the identical workload by construction.
fn params(test_id: &str) -> (u32, u32) {
    match test_id {
        // Pipelining throughput probe: enough messages to overlap.
        "concurrent-send-receive" => (200, 1024),
        _ => (50, 1024),
    }
}

/// A short, stable rendering of an error variant for failure details.
/// Payload strings are included for humans; assertions never match them.
fn describe(err: &Error) -> String {
    match err {
        Error::InvalidUrl(msg) => format!("invalid-url({msg})"),
        Error::ConnectFailed(msg) => format!("connect-failed({msg})"),
        Error::Closed => "closed".to_string(),
        Error::ReceivingViaStream => "receiving-via-stream".to_string(),
        Error::ReceiveBufferOverflow => "receive-buffer-overflow".to_string(),
        Error::InvalidArgument(msg) => format!("invalid-argument({msg})"),
        Error::Other(msg) => format!("other({msg})"),
    }
}

/// Connect to a mode path on the suite server, mapping failure into a test
/// detail.
async fn connect(
    config: &TestConfig,
    path_query: &str,
    protocols: &[&str],
) -> Result<Websocket, String> {
    let url = format!("{}{}", config.server_url, path_query);
    let protocols: Vec<String> = protocols.iter().map(|p| p.to_string()).collect();
    Websocket::connect(url, protocols)
        .await
        .map_err(|err| format!("connect {path_query}: {}", describe(&err)))
}

/// A deterministic, index-tagged payload of `size` bytes (minimum 4).
fn make_payload(index: u32, size: u32) -> Vec<u8> {
    let size = size.max(4) as usize;
    let mut payload = vec![0u8; size];
    payload[..4].copy_from_slice(&index.to_le_bytes());
    for (offset, byte) in payload.iter_mut().enumerate().skip(4) {
        *byte = ((index as usize + offset) % 256) as u8;
    }
    payload
}

/// The payload `conformance/server`'s burst modes send for message `index`.
fn burst_payload(index: u32, size: u32) -> Vec<u8> {
    (0..size).map(|i| ((index + i) % 256) as u8).collect()
}

async fn send(ws: &Websocket, message: Message) -> Result<(), String> {
    ws.send(message)
        .await
        .map_err(|err| format!("send: {}", describe(&err)))
}

async fn receive(ws: &Websocket) -> Result<Message, String> {
    ws.receive()
        .await
        .map_err(|err| format!("receive: {}", describe(&err)))
}

async fn receive_binary(ws: &Websocket) -> Result<Vec<u8>, String> {
    match receive(ws).await? {
        Message::Binary(bytes) => Ok(bytes),
        Message::String(_) => Err("expected a binary message, got text".to_string()),
    }
}

async fn send_sequence(ws: &Websocket, count: u32, size: u32) -> Result<(), String> {
    for index in 0..count {
        send(ws, Message::Binary(make_payload(index, size))).await?;
    }
    Ok(())
}

async fn recv_sequence(ws: &Websocket, count: u32) -> Result<Vec<Vec<u8>>, String> {
    let mut received = Vec::with_capacity(count as usize);
    for _ in 0..count {
        received.push(receive_binary(ws).await?);
    }
    Ok(received)
}

/// Verify an echoed sequence arrived intact and in order.
fn verify_sequence(received: &[Vec<u8>], count: u32, size: u32) -> Result<(), String> {
    if received.len() != count as usize {
        return Err(format!("expected {count} messages, got {}", received.len()));
    }
    for (index, bytes) in received.iter().enumerate() {
        if bytes != &make_payload(index as u32, size) {
            return Err(format!("message {index} corrupted or out of order"));
        }
    }
    Ok(())
}

/// Read every byte of a `stream-message` payload stream until it ends.
async fn drain_byte_stream(reader: wit_bindgen::StreamReader<u8>) -> Vec<u8> {
    let mut reader = reader;
    let mut out = Vec::new();
    loop {
        let (status, chunk) = reader.read(Vec::with_capacity(8192)).await;
        out.extend_from_slice(&chunk);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            break;
        }
    }
    out
}

async fn run(test_id: &str, config: &TestConfig) -> Result<(), String> {
    let (count, size) = params(test_id);
    match test_id {
        "connect-basic" => {
            let ws = connect(config, "/echo", &[]).await?;
            let protocol = ws.protocol();
            if !protocol.is_empty() {
                return Err(format!(
                    "expected no negotiated subprotocol, got {protocol:?}"
                ));
            }
            ws.close(Some(1000), "").map_err(|e| describe(&e))?;
            Ok(())
        }
        "connect-invalid-url" => {
            // server_url is `ws://host:port`; splice userinfo in after the
            // scheme for the credentials case.
            let with_userinfo = format!("ws://user:secret@{}/echo", &config.server_url[5..]);
            let cases: &[String] = &[
                format!("http{}", &config.server_url[2..]), // http:// scheme
                format!("{}/echo#fragment", config.server_url),
                with_userinfo,
                "not a url".to_string(),
                "/echo".to_string(),
            ];
            for url in cases {
                match Websocket::connect(url.clone(), Vec::new()).await {
                    Err(Error::InvalidUrl(_)) => {}
                    Ok(_) => return Err(format!("connect {url:?} unexpectedly succeeded")),
                    Err(other) => {
                        return Err(format!(
                            "connect {url:?}: expected invalid-url, got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            Ok(())
        }
        "connect-invalid-protocols" => {
            let url = format!("{}/echo", config.server_url);
            let cases: &[&[&str]] = &[&["dup", "dup"], &["has space"], &[""], &["bad,comma"]];
            for protocols in cases {
                let protocols: Vec<String> = protocols.iter().map(|p| p.to_string()).collect();
                match Websocket::connect(url.clone(), protocols.clone()).await {
                    Err(Error::InvalidArgument(_)) => {}
                    Ok(_) => {
                        return Err(format!(
                            "connect with protocols {protocols:?} unexpectedly succeeded"
                        ))
                    }
                    Err(other) => {
                        return Err(format!(
                            "connect with protocols {protocols:?}: expected invalid-argument, \
                             got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            Ok(())
        }
        "connect-refused" => {
            match Websocket::connect(config.unreachable_url.clone(), Vec::new()).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err("connect to unreachable url unexpectedly succeeded".to_string()),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "connect-rejected" => {
            let url = format!("{}/reject", config.server_url);
            match Websocket::connect(url.clone(), Vec::new()).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err("connect to /reject unexpectedly succeeded".to_string()),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "connect-redirect" => {
            // A redirect instead of the upgrade must fail the connect;
            // clients never follow (the redirect target is a working echo
            // endpoint, so a client that followed would connect and fail
            // here).
            let url = format!("{}/redirect", config.server_url);
            match Websocket::connect(url.clone(), Vec::new()).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err("connect followed a redirect".to_string()),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "connect-timeout" => {
            // The adapter configures a short connect bound; /stall never
            // answers the handshake.
            let url = format!("{}/stall", config.server_url);
            match Websocket::connect(url.clone(), Vec::new()).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err("connect to /stall unexpectedly succeeded".to_string()),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "subprotocol-negotiated" => {
            let ws = connect(config, "/echo?protocol=beta", &["alpha", "beta"]).await?;
            let protocol = ws.protocol();
            if protocol != "beta" {
                return Err(format!("expected subprotocol \"beta\", got {protocol:?}"));
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "subprotocol-none-offered" => {
            let ws = connect(config, "/echo", &[]).await?;
            let protocol = ws.protocol();
            if !protocol.is_empty() {
                return Err(format!("expected no subprotocol, got {protocol:?}"));
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "subprotocol-unoffered-selected" => {
            let url = format!("{}/echo?force-protocol=zeta", config.server_url);
            match Websocket::connect(url.clone(), vec!["alpha".to_string()]).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err(
                    "connect succeeded although the server selected an unoffered subprotocol"
                        .to_string(),
                ),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "subprotocol-none-selected" => {
            let url = format!("{}/echo", config.server_url);
            match Websocket::connect(url.clone(), vec!["alpha".to_string()]).await {
                Err(Error::ConnectFailed(_)) => Ok(()),
                Ok(_) => Err(
                    "connect succeeded although the server selected no offered subprotocol"
                        .to_string(),
                ),
                Err(other) => Err(format!("expected connect-failed, got {}", describe(&other))),
            }
        }
        "echo-binary" => {
            let ws = connect(config, "/echo", &[]).await?;
            let payload = make_payload(0, size);
            send(&ws, Message::Binary(payload.clone())).await?;
            match receive(&ws).await? {
                Message::Binary(bytes) if bytes == payload => {}
                Message::Binary(_) => return Err("binary payload mismatch".to_string()),
                Message::String(_) => return Err("binary message arrived as text".to_string()),
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "echo-text" => {
            let ws = connect(config, "/echo", &[]).await?;
            let text = "conformance text message";
            send(&ws, Message::String(text.to_string())).await?;
            match receive(&ws).await? {
                Message::String(got) if got == text => {}
                Message::String(_) => return Err("text payload mismatch".to_string()),
                Message::Binary(_) => return Err("text message arrived as binary".to_string()),
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "echo-text-unicode" => {
            let ws = connect(config, "/echo", &[]).await?;
            let text = "héllo wörld — 你好 🦀\u{200d}🕸";
            send(&ws, Message::String(text.to_string())).await?;
            match receive(&ws).await? {
                Message::String(got) if got == text => {}
                other => {
                    return Err(format!(
                        "unicode text did not round-trip: got {}",
                        match other {
                            Message::String(_) => "different text",
                            Message::Binary(_) => "binary",
                        }
                    ))
                }
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "echo-empty" => {
            let ws = connect(config, "/echo", &[]).await?;
            send(&ws, Message::Binary(Vec::new())).await?;
            send(&ws, Message::String(String::new())).await?;
            match receive(&ws).await? {
                Message::Binary(bytes) if bytes.is_empty() => {}
                _ => return Err("expected empty binary message".to_string()),
            }
            match receive(&ws).await? {
                Message::String(text) if text.is_empty() => {}
                _ => return Err("expected empty text message".to_string()),
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "echo-large" => {
            let ws = connect(config, "/echo", &[]).await?;
            let payload = make_payload(0, size.max(128 * 1024));
            send(&ws, Message::Binary(payload.clone())).await?;
            match receive(&ws).await? {
                Message::Binary(bytes) if bytes == payload => {}
                _ => return Err("large payload mismatch".to_string()),
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "message-boundaries" => {
            // Back-to-back messages of varying sizes: each arrives as its
            // own message, intact, in order.
            let ws = connect(config, "/echo", &[]).await?;
            let sizes = [1u32, 2, 3, 5, 8, 13, 21, 1024, 4, 64];
            for (index, message_size) in sizes.iter().enumerate() {
                send(
                    &ws,
                    Message::Binary(make_payload(index as u32, *message_size)),
                )
                .await?;
            }
            for (index, message_size) in sizes.iter().enumerate() {
                let bytes = receive_binary(&ws).await?;
                if bytes != make_payload(index as u32, *message_size) {
                    return Err(format!("message {index} merged, split, or reordered"));
                }
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "binary-text-interleave" => {
            let ws = connect(config, "/echo", &[]).await?;
            for index in 0..8u32 {
                if index % 2 == 0 {
                    send(&ws, Message::Binary(make_payload(index, 32))).await?;
                } else {
                    send(&ws, Message::String(format!("text-{index}"))).await?;
                }
            }
            for index in 0..8u32 {
                match (index % 2, receive(&ws).await?) {
                    (0, Message::Binary(bytes)) if bytes == make_payload(index, 32) => {}
                    (1, Message::String(text)) if text == format!("text-{index}") => {}
                    (_, got) => {
                        return Err(format!(
                            "message {index}: kind or payload wrong (got {})",
                            match got {
                                Message::Binary(_) => "binary",
                                Message::String(_) => "text",
                            }
                        ))
                    }
                }
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "concurrent-send-receive" => {
            let ws = connect(config, "/echo", &[]).await?;
            let sender = send_sequence(&ws, count, size);
            let receiver = recv_sequence(&ws, count);
            let (sent, received) = futures::join!(sender, receiver);
            sent?;
            verify_sequence(&received?, count, size)?;
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "concurrent-receives" => {
            let ws = connect(config, "/echo", &[]).await?;
            // Two receives pending before anything is sent; each must get
            // exactly one of the two echoed messages.
            let first = ws.receive();
            let second = ws.receive();
            let feed = async {
                send(&ws, Message::Binary(make_payload(0, 32))).await?;
                send(&ws, Message::Binary(make_payload(1, 32))).await
            };
            let (first, second, fed) = futures::join!(first, second, feed);
            fed?;
            let a = first.map_err(|e| format!("first receive: {}", describe(&e)))?;
            let b = second.map_err(|e| format!("second receive: {}", describe(&e)))?;
            let mut got = vec![
                match a {
                    Message::Binary(bytes) => bytes,
                    Message::String(_) => return Err("unexpected text message".to_string()),
                },
                match b {
                    Message::Binary(bytes) => bytes,
                    Message::String(_) => return Err("unexpected text message".to_string()),
                },
            ];
            got.sort();
            let mut want = vec![make_payload(0, 32), make_payload(1, 32)];
            want.sort();
            if got != want {
                return Err("concurrent receives lost or duplicated a message".to_string());
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "close-local" => {
            let ws = connect(config, "/echo", &[]).await?;
            ws.close(Some(1000), "bye")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            // The suite server echoes the close frame verbatim, so the
            // acknowledgement carries the same code and reason.
            match ws.wait_closed().await {
                Some(info) if info.code == 1000 && info.reason == "bye" => Ok(()),
                Some(info) => Err(format!(
                    "close acknowledgement carried code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-local-default" => {
            let ws = connect(config, "/echo", &[]).await?;
            ws.close(None, "")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            // A code-less close frame is observed by the peer as 1005; the
            // echoed acknowledgement carries the same.
            match ws.wait_closed().await {
                Some(info) if info.code == 1005 && info.reason.is_empty() => Ok(()),
                Some(info) => Err(format!(
                    "expected code 1005 with empty reason, got code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-local-idempotent" => {
            let ws = connect(config, "/echo", &[]).await?;
            ws.close(Some(1000), "first")
                .map_err(|e| format!("first close: {}", describe(&e)))?;
            ws.close(Some(4000), "second")
                .map_err(|e| format!("second close: {}", describe(&e)))?;
            match ws.wait_closed().await {
                // Only the first close's frame was sent.
                Some(info) if info.code == 1000 => Ok(()),
                Some(info) => Err(format!(
                    "second close overrode the first: code={}",
                    info.code
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-boundary-codes" => {
            // The sendable range's edges: 3000 and 4999 are accepted and
            // travel intact (rejections just inside the bounds are
            // `close-invalid-code`'s rows).
            for code in [3000u16, 4999] {
                let ws = connect(config, "/echo", &[]).await?;
                ws.close(Some(code), "")
                    .map_err(|e| format!("close({code}): {}", describe(&e)))?;
                match ws.wait_closed().await {
                    Some(info) if info.code == code => {}
                    Some(info) => {
                        return Err(format!(
                            "close({code}) acknowledged with code={}",
                            info.code
                        ))
                    }
                    None => return Err(format!("close({code}) observed as an abnormal closure")),
                }
            }
            Ok(())
        }
        "close-reason-unicode" => {
            // The 123-byte reason bound counts UTF-8 bytes, not code
            // units: 41 three-byte characters fit exactly and round-trip;
            // 42 do not.
            let ws = connect(config, "/echo", &[]).await?;
            let too_long = "€".repeat(42);
            match ws.close(Some(4000), &too_long) {
                Err(Error::InvalidArgument(_)) => {}
                Ok(()) => return Err("close accepted a 126-byte reason".to_string()),
                Err(other) => {
                    return Err(format!(
                        "expected invalid-argument, got {}",
                        describe(&other)
                    ))
                }
            }
            let max = "€".repeat(41);
            ws.close(Some(4000), &max)
                .map_err(|e| format!("close with 123-byte unicode reason: {}", describe(&e)))?;
            match ws.wait_closed().await {
                Some(info) if info.code == 4000 && info.reason == max => Ok(()),
                Some(info) => Err(format!(
                    "unicode reason did not round-trip: code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-invalid-code" => {
            let ws = connect(config, "/echo", &[]).await?;
            for code in [0u16, 999, 1001, 1005, 1006, 1015, 2999, 5000, u16::MAX] {
                match ws.close(Some(code), "") {
                    Err(Error::InvalidArgument(_)) => {}
                    Ok(()) => return Err(format!("close accepted invalid code {code}")),
                    Err(other) => {
                        return Err(format!(
                            "close({code}): expected invalid-argument, got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            // A rejected close leaves the connection usable.
            let payload = make_payload(7, 32);
            send(&ws, Message::Binary(payload.clone())).await?;
            if receive_binary(&ws).await? != payload {
                return Err("connection unusable after rejected close".to_string());
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "close-reason-too-long" => {
            let ws = connect(config, "/echo", &[]).await?;
            let long = "r".repeat(124);
            match ws.close(Some(1000), &long) {
                Err(Error::InvalidArgument(_)) => {}
                Ok(()) => return Err("close accepted a 124-byte reason".to_string()),
                Err(other) => {
                    return Err(format!(
                        "expected invalid-argument, got {}",
                        describe(&other)
                    ))
                }
            }
            // Exactly 123 bytes is accepted.
            let max = "r".repeat(123);
            ws.close(Some(1000), &max)
                .map_err(|e| format!("close with 123-byte reason: {}", describe(&e)))?;
            Ok(())
        }
        "close-reason-without-code" => {
            let ws = connect(config, "/echo", &[]).await?;
            match ws.close(None, "reason") {
                Err(Error::InvalidArgument(_)) => {}
                Ok(()) => return Err("close accepted a code-less reason".to_string()),
                Err(other) => {
                    return Err(format!(
                        "expected invalid-argument, got {}",
                        describe(&other)
                    ))
                }
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "send-after-close" => {
            let ws = connect(config, "/echo", &[]).await?;
            ws.close(Some(1000), "")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            match ws.send(Message::Binary(vec![1, 2, 3])).await {
                Err(Error::Closed) => Ok(()),
                Ok(()) => Err("send succeeded after close".to_string()),
                Err(other) => Err(format!("expected closed, got {}", describe(&other))),
            }
        }
        "receive-after-close" => {
            let ws = connect(config, "/echo", &[]).await?;
            // Unread backlog is discarded by a local close.
            send(&ws, Message::Binary(make_payload(0, 32))).await?;
            ws.close(Some(1000), "")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            match ws.receive().await {
                Err(Error::Closed) => Ok(()),
                Ok(_) => Err("receive yielded a message after local close".to_string()),
                Err(other) => Err(format!("expected closed, got {}", describe(&other))),
            }
        }
        "close-remote" => {
            let ws = connect(
                config,
                "/close-after?count=1&code=4001&reason=going-away",
                &[],
            )
            .await?;
            let payload = make_payload(0, 32);
            send(&ws, Message::Binary(payload.clone())).await?;
            if receive_binary(&ws).await? != payload {
                return Err("echo before close corrupted".to_string());
            }
            match ws.receive().await {
                Err(Error::Closed) => {}
                Ok(_) => return Err("receive yielded a message after the close frame".to_string()),
                Err(other) => return Err(format!("expected closed, got {}", describe(&other))),
            }
            match ws.wait_closed().await {
                Some(info) if info.code == 4001 && info.reason == "going-away" => Ok(()),
                Some(info) => Err(format!(
                    "close frame carried code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-remote-no-code" => {
            let ws = connect(config, "/close-after?count=0", &[]).await?;
            match ws.receive().await {
                Err(Error::Closed) => {}
                Ok(_) => return Err("receive yielded an unexpected message".to_string()),
                Err(other) => return Err(format!("expected closed, got {}", describe(&other))),
            }
            match ws.wait_closed().await {
                Some(info) if info.code == 1005 && info.reason.is_empty() => Ok(()),
                Some(info) => Err(format!(
                    "expected code 1005 with empty reason, got code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-abnormal" => {
            let ws = connect(config, "/abrupt-close?after=1", &[]).await?;
            let payload = make_payload(0, 32);
            send(&ws, Message::Binary(payload.clone())).await?;
            if receive_binary(&ws).await? != payload {
                return Err("echo before abrupt close corrupted".to_string());
            }
            match ws.receive().await {
                Err(Error::Closed) => {}
                Ok(_) => return Err("receive yielded a message after the drop".to_string()),
                Err(other) => return Err(format!("expected closed, got {}", describe(&other))),
            }
            match ws.wait_closed().await {
                None => Ok(()),
                Some(info) => Err(format!(
                    "abnormal closure produced close-info code={} (implementations must not invent one)",
                    info.code
                )),
            }
        }
        "receive-backlog-before-close" => {
            let burst = 5u32;
            let ws = connect(
                config,
                "/burst-then-close?count=5&size=64&code=1000&reason=done",
                &[],
            )
            .await?;
            for index in 0..burst {
                let bytes = receive_binary(&ws).await?;
                if bytes != burst_payload(index, 64) {
                    return Err(format!("backlog message {index} corrupted or reordered"));
                }
            }
            match ws.receive().await {
                Err(Error::Closed) => {}
                Ok(_) => return Err("receive yielded more than the backlog".to_string()),
                Err(other) => return Err(format!("expected closed, got {}", describe(&other))),
            }
            match ws.wait_closed().await {
                Some(info) if info.code == 1000 => Ok(()),
                Some(info) => Err(format!("close frame carried code={}", info.code)),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "close-handshake-timeout" => {
            // The server never answers the close frame; the connection must
            // still reach closed within the host's (adapter-shortened)
            // bound, with no peer close frame.
            let ws = connect(config, "/ignore-close", &[]).await?;
            ws.close(Some(1000), "")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            match ws.wait_closed().await {
                None => Ok(()),
                Some(info) => Err(format!(
                    "unanswered close produced close-info code={}",
                    info.code
                )),
            }
        }
        "close-under-send-backpressure" => {
            // The server neither reads nor writes, so large sends stall in
            // the transport. The closing procedure must still complete
            // within the host's bound: in-flight and queued sends fail
            // `closed` (or complete), and `wait-closed` reports an
            // abnormal closure.
            let ws = connect(config, "/blackhole", &[]).await?;
            let payload = vec![0xa5u8; 256 * 1024];
            let sends = futures::future::join_all(
                (0..64).map(|_| ws.send(Message::Binary(payload.clone()))),
            );
            let close_side = async {
                ws.close(Some(1000), "")
                    .map_err(|e| format!("close: {}", describe(&e)))?;
                Ok::<_, String>(ws.wait_closed().await)
            };
            let (send_results, closed) = futures::join!(sends, close_side);
            if let Some(info) = closed? {
                return Err(format!(
                    "close against a silent peer produced close-info code={}",
                    info.code
                ));
            }
            for result in send_results {
                match result {
                    Ok(()) | Err(Error::Closed) => {}
                    Err(other) => {
                        return Err(format!(
                            "send under backpressure: expected ok or closed, got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            Ok(())
        }
        "state-lifecycle" => {
            // The getter tracks the lifecycle forward-only and latches on
            // closed. Whether closing is observable after close() returns
            // is implementation-defined; open would mean the close was not
            // observed locally at once.
            let ws = connect(config, "/echo", &[]).await?;
            match ws.state() {
                WebsocketState::Open => {}
                other => return Err(format!("state after connect: {other:?}")),
            }
            ws.close(Some(1000), "")
                .map_err(|e| format!("close: {}", describe(&e)))?;
            if ws.state() == WebsocketState::Open {
                return Err("state still open after close returned".to_string());
            }
            let _ = ws.wait_closed().await;
            match ws.state() {
                WebsocketState::Closed => {}
                other => return Err(format!("state after wait-closed: {other:?}")),
            }
            // Latched: it never leaves closed.
            match ws.state() {
                WebsocketState::Closed => Ok(()),
                other => Err(format!("closed did not latch: {other:?}")),
            }
        }
        "wait-closed-latched" => {
            let ws = connect(config, "/close-after?count=0&code=4009&reason=latch", &[]).await?;
            let first = ws.wait_closed().await;
            let second = ws.wait_closed().await;
            match (&first, &second) {
                (Some(a), Some(b)) if a.code == 4009 && b.code == 4009 && a.reason == b.reason => {
                    Ok(())
                }
                _ => Err(format!(
                    "wait-closed not latched: first={first:?} second={second:?}"
                )),
            }
        }
        "wait-closed-pending" => {
            // `wait-closed` may be awaited before any close exists; the
            // pending waiter resolves with the eventual acknowledgement.
            let ws = connect(config, "/echo", &[]).await?;
            // `join!` polls in order: the wait is pending before the
            // round-trip and close run.
            let waiter = ws.wait_closed();
            let driver = async {
                let payload = make_payload(0, 32);
                send(&ws, Message::Binary(payload.clone())).await?;
                if receive_binary(&ws).await? != payload {
                    return Err("echo corrupted".to_string());
                }
                ws.close(Some(1000), "bye")
                    .map_err(|e| format!("close: {}", describe(&e)))
            };
            let (info, driven) = futures::join!(waiter, driver);
            driven?;
            match info {
                Some(info) if info.code == 1000 && info.reason == "bye" => Ok(()),
                Some(info) => Err(format!(
                    "pending wait-closed resolved with code={} reason={:?}",
                    info.code, info.reason
                )),
                None => Err("pending wait-closed reported an abnormal closure".to_string()),
            }
        }
        "send-via-stream" => {
            let ws = connect(config, "/echo", &[]).await?;
            let count = count.min(16);
            let send_side = async {
                let (mut tx, rx) = bindings::wit_stream::new();
                let send = ws.send_via_stream(rx);
                let feed = async {
                    for index in 0..count {
                        let payload = make_payload(index, size);
                        let length = payload.len() as u32;
                        let (mut data_tx, data_rx) = bindings::wit_stream::new();
                        let message = bindings::lann::websocket::types::StreamMessage {
                            kind: MessageKind::Binary,
                            length,
                            data: data_rx,
                        };
                        if !tx.write_all(vec![message]).await.is_empty() {
                            return Err("stream-message writer closed early".to_string());
                        }
                        if !data_tx.write_all(payload).await.is_empty() {
                            return Err("payload writer closed early".to_string());
                        }
                        drop(data_tx);
                    }
                    drop(tx);
                    Ok(())
                };
                let (sent, fed) = futures::join!(send, feed);
                fed?;
                sent.map_err(|e| {
                    format!(
                        "send-via-stream: {} after {} message(s)",
                        describe(&e.error),
                        e.sent
                    )
                })
            };
            send_side.await?;
            let received = recv_sequence(&ws, count).await?;
            verify_sequence(&received, count, size)?;
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "receive-via-stream" => {
            let ws = connect(config, "/echo", &[]).await?;
            let count = count.min(16);
            send_sequence(&ws, count, size).await?;
            let mut stream = ws
                .receive_via_stream()
                .map_err(|e| format!("receive-via-stream: {}", describe(&e)))?;
            let mut received: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
            while received.len() < count as usize {
                let (status, batch) = stream.read(Vec::with_capacity(1)).await;
                for message in batch {
                    let declared = message.length as usize;
                    let is_text = matches!(message.kind, MessageKind::String);
                    let bytes = drain_byte_stream(message.data).await;
                    if bytes.len() != declared {
                        return Err(format!(
                            "stream-message declared {declared} bytes but carried {}",
                            bytes.len()
                        ));
                    }
                    if is_text {
                        return Err("binary message delivered as text".to_string());
                    }
                    received.push(bytes);
                }
                if matches!(
                    status,
                    wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
                ) && received.len() < count as usize
                {
                    return Err(format!(
                        "stream ended after {} of {count} message(s)",
                        received.len()
                    ));
                }
            }
            verify_sequence(&received, count, size)?;
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "receive-via-stream-once" => {
            let ws = connect(config, "/echo", &[]).await?;
            // A receive pending when the stream claims the connection must
            // resolve with `receiving-via-stream`. `join!` polls in order:
            // the receive starts first, then the claim is made.
            let pending = ws.receive();
            let claim = async {
                ws.receive_via_stream()
                    .map_err(|e| format!("first receive-via-stream: {}", describe(&e)))
            };
            let (pending, stream) = futures::join!(pending, claim);
            let _stream = stream?;
            match pending {
                Err(Error::ReceivingViaStream) => {}
                Ok(_) => return Err("pending receive yielded a message".to_string()),
                Err(other) => {
                    return Err(format!(
                        "pending receive: expected receiving-via-stream, got {}",
                        describe(&other)
                    ))
                }
            }
            match ws.receive_via_stream() {
                Err(Error::ReceivingViaStream) => {}
                Ok(_) => return Err("second receive-via-stream succeeded".to_string()),
                Err(other) => {
                    return Err(format!(
                        "second receive-via-stream: expected receiving-via-stream, got {}",
                        describe(&other)
                    ))
                }
            }
            match ws.receive().await {
                Err(Error::ReceivingViaStream) => {}
                Ok(_) => return Err("receive succeeded during stream receiving".to_string()),
                Err(other) => {
                    return Err(format!(
                        "receive during stream receiving: expected receiving-via-stream, got {}",
                        describe(&other)
                    ))
                }
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "receive-via-stream-end-on-close" => {
            let ws = connect(config, "/burst-then-close?count=3&size=32&code=1000", &[]).await?;
            let mut stream = ws
                .receive_via_stream()
                .map_err(|e| format!("receive-via-stream: {}", describe(&e)))?;
            let mut received = 0u32;
            loop {
                let (status, batch) = stream.read(Vec::with_capacity(1)).await;
                for message in batch {
                    let bytes = drain_byte_stream(message.data).await;
                    if bytes != burst_payload(received, 32) {
                        return Err(format!("stream message {received} corrupted"));
                    }
                    received += 1;
                }
                if matches!(
                    status,
                    wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
                ) {
                    break;
                }
            }
            if received != 3 {
                return Err(format!(
                    "expected 3 messages before the close, got {received}"
                ));
            }
            match ws.wait_closed().await {
                Some(info) if info.code == 1000 => Ok(()),
                Some(info) => Err(format!("close frame carried code={}", info.code)),
                None => Err("wait-closed reported an abnormal closure".to_string()),
            }
        }
        "stream-text-round-trip" => {
            let ws = connect(config, "/echo", &[]).await?;
            let text = "streamed téxt — 流 ✓";
            let bytes = text.as_bytes().to_vec();
            // Outbound: one text message through send-via-stream keeps its
            // kind through the echo.
            let send_side = async {
                let (mut tx, rx) = bindings::wit_stream::new();
                let send = ws.send_via_stream(rx);
                let feed = async {
                    let (mut data_tx, data_rx) = bindings::wit_stream::new();
                    let message = bindings::lann::websocket::types::StreamMessage {
                        kind: MessageKind::String,
                        length: bytes.len() as u32,
                        data: data_rx,
                    };
                    if !tx.write_all(vec![message]).await.is_empty() {
                        return Err("stream-message writer closed early".to_string());
                    }
                    if !data_tx.write_all(bytes.clone()).await.is_empty() {
                        return Err("payload writer closed early".to_string());
                    }
                    drop(data_tx);
                    drop(tx);
                    Ok(())
                };
                let (sent, fed) = futures::join!(send, feed);
                fed?;
                sent.map_err(|e| format!("send-via-stream: {}", describe(&e.error)))
            };
            send_side.await?;
            match receive(&ws).await? {
                Message::String(got) if got == text => {}
                Message::String(_) => return Err("streamed text corrupted".to_string()),
                Message::Binary(_) => return Err("streamed text arrived as binary".to_string()),
            }
            // Inbound: a text message through the claimed stream is
            // delivered with kind string and intact UTF-8.
            send(&ws, Message::String(text.to_string())).await?;
            let mut stream = ws
                .receive_via_stream()
                .map_err(|e| format!("receive-via-stream: {}", describe(&e)))?;
            let mut got: Option<(MessageKind, Vec<u8>)> = None;
            while got.is_none() {
                let (status, batch) = stream.read(Vec::with_capacity(1)).await;
                if let Some(message) = batch.into_iter().next() {
                    let kind = message.kind;
                    let data = drain_byte_stream(message.data).await;
                    got = Some((kind, data));
                } else if matches!(
                    status,
                    wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
                ) {
                    return Err("stream ended before the text message".to_string());
                }
            }
            match got {
                Some((MessageKind::String, data)) if data == bytes => {}
                Some((MessageKind::String, _)) => {
                    return Err("inbound streamed text corrupted".to_string())
                }
                Some((MessageKind::Binary, _)) => {
                    return Err("text message delivered as binary stream-message".to_string())
                }
                None => unreachable!("loop exits only with a message"),
            }
            let _ = ws.close(Some(1000), "");
            Ok(())
        }
        "send-via-stream-invalid-utf8" => {
            // A string-kind payload must be valid UTF-8; a violating
            // producer gets an error, never silently mangled text.
            let ws = connect(config, "/echo", &[]).await?;
            let result = {
                let (mut tx, rx) = bindings::wit_stream::new();
                let send = ws.send_via_stream(rx);
                let feed = async {
                    let (mut data_tx, data_rx) = bindings::wit_stream::new();
                    let message = bindings::lann::websocket::types::StreamMessage {
                        kind: MessageKind::String,
                        length: 4,
                        data: data_rx,
                    };
                    let _ = tx.write_all(vec![message]).await;
                    let _ = data_tx.write_all(vec![0xff, 0xfe, 0x80, 0x00]).await;
                    drop(data_tx);
                    drop(tx);
                };
                let (sent, ()) = futures::join!(send, feed);
                sent
            };
            match result {
                Err(err) if matches!(err.error, Error::Other(_)) && err.sent == 0 => Ok(()),
                Err(err) => Err(format!(
                    "expected other with sent=0, got {} after {} message(s)",
                    describe(&err.error),
                    err.sent
                )),
                Ok(()) => Err("an invalid-UTF-8 string stream-message was sent".to_string()),
            }
        }
        "send-via-stream-length-mismatch" => {
            // The payload must total exactly the declared length; both a
            // short and a long payload are producer errors.
            for (declared, actual) in [(16u32, 8usize), (8u32, 16usize)] {
                let ws = connect(config, "/echo", &[]).await?;
                let result = {
                    let (mut tx, rx) = bindings::wit_stream::new();
                    let send = ws.send_via_stream(rx);
                    let feed = async {
                        let (mut data_tx, data_rx) = bindings::wit_stream::new();
                        let message = bindings::lann::websocket::types::StreamMessage {
                            kind: MessageKind::Binary,
                            length: declared,
                            data: data_rx,
                        };
                        let _ = tx.write_all(vec![message]).await;
                        let _ = data_tx.write_all(vec![7u8; actual]).await;
                        drop(data_tx);
                        drop(tx);
                    };
                    let (sent, ()) = futures::join!(send, feed);
                    sent
                };
                match result {
                    Err(err) if matches!(err.error, Error::Other(_)) && err.sent == 0 => {}
                    Err(err) => {
                        return Err(format!(
                            "declared {declared} actual {actual}: expected other with sent=0, \
                             got {} after {}",
                            describe(&err.error),
                            err.sent
                        ))
                    }
                    Ok(()) => {
                        return Err(format!(
                            "declared {declared} actual {actual}: mismatched stream-message \
                             was sent"
                        ))
                    }
                }
            }
            Ok(())
        }
        "send-via-stream-sent-count" => {
            // The server closes after echoing one message; a second
            // streamed message must fail with `closed` and `sent` must
            // count exactly the message that made it.
            let ws = connect(config, "/close-after?count=1&code=1000", &[]).await?;
            let payload = make_payload(0, 64);
            let (mut tx, rx) = bindings::wit_stream::new();
            let send = ws.send_via_stream(rx);
            let feed = async {
                let (mut data_tx, data_rx) = bindings::wit_stream::new();
                let message = bindings::lann::websocket::types::StreamMessage {
                    kind: MessageKind::Binary,
                    length: payload.len() as u32,
                    data: data_rx,
                };
                if !tx.write_all(vec![message]).await.is_empty() {
                    return Err("stream-message writer closed early".to_string());
                }
                if !data_tx.write_all(payload.clone()).await.is_empty() {
                    return Err("payload writer closed early".to_string());
                }
                drop(data_tx);
                // Synchronize: once the echo is back and the connection has
                // fully closed, the next streamed message must fail.
                if receive_binary(&ws).await? != payload {
                    return Err("echo before close corrupted".to_string());
                }
                let _ = ws.wait_closed().await;
                let (mut data_tx, data_rx) = bindings::wit_stream::new();
                let message = bindings::lann::websocket::types::StreamMessage {
                    kind: MessageKind::Binary,
                    length: payload.len() as u32,
                    data: data_rx,
                };
                let _ = tx.write_all(vec![message]).await;
                let _ = data_tx.write_all(payload.clone()).await;
                drop(data_tx);
                drop(tx);
                Ok(())
            };
            let (sent, fed) = futures::join!(send, feed);
            fed?;
            match sent {
                Err(err) if matches!(err.error, Error::Closed) && err.sent == 1 => Ok(()),
                Err(err) => Err(format!(
                    "expected closed with sent=1, got {} after {} message(s)",
                    describe(&err.error),
                    err.sent
                )),
                Ok(()) => Err("send-via-stream succeeded past the close".to_string()),
            }
        }
        "receive-via-stream-overflow" => {
            // An overflow observed through the claimed stream: the backlog
            // is delivered, then the stream simply ends (the end of a
            // stream carries no error value, per the streaming contract).
            let flood_count = (4 * config.max_inbound_buffer_bytes) / 1024;
            let ws = connect(
                config,
                &format!("/burst?count={flood_count}&size=1024"),
                &[],
            )
            .await?;
            let mut stream = ws
                .receive_via_stream()
                .map_err(|e| format!("receive-via-stream: {}", describe(&e)))?;
            // Wait for the overflow close to land before reading, so the
            // flood outpaces consumption deterministically.
            let _ = ws.wait_closed().await;
            let mut received = 0u32;
            loop {
                let (status, batch) = stream.read(Vec::with_capacity(1)).await;
                for message in batch {
                    let bytes = drain_byte_stream(message.data).await;
                    if bytes != burst_payload(received, 1024) {
                        return Err(format!("backlog stream message {received} corrupted"));
                    }
                    received += 1;
                }
                if matches!(
                    status,
                    wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
                ) {
                    break;
                }
            }
            if received == 0 {
                return Err("pre-overflow backlog was not delivered to the stream".to_string());
            }
            if received >= flood_count {
                return Err(format!(
                    "all {flood_count} flooded messages were delivered; the buffer bound did not engage"
                ));
            }
            Ok(())
        }
        "receive-buffer-overflow" => {
            // Flood 4x the configured bound without receiving; the
            // overflow closes the connection. The state stream (terminal
            // always delivered) is the clock-free way to wait for that.
            let flood_count = (4 * config.max_inbound_buffer_bytes) / 1024;
            let ws = connect(
                config,
                &format!("/burst?count={flood_count}&size=1024"),
                &[],
            )
            .await?;
            let _ = ws.wait_closed().await;
            // Drain the pre-overflow backlog, then expect the overflow
            // error.
            let mut drained = 0u32;
            loop {
                match ws.receive().await {
                    Ok(Message::Binary(bytes)) => {
                        if bytes != burst_payload(drained, 1024) {
                            return Err(format!("backlog message {drained} corrupted"));
                        }
                        drained += 1;
                        if drained > flood_count {
                            return Err("received more messages than were sent".to_string());
                        }
                    }
                    Ok(Message::String(_)) => return Err("unexpected text message".to_string()),
                    Err(Error::ReceiveBufferOverflow) => break,
                    Err(other) => {
                        return Err(format!(
                            "expected receive-buffer-overflow after the backlog, got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            if drained == 0 {
                return Err("pre-overflow backlog was not receivable".to_string());
            }
            if drained >= flood_count {
                return Err(format!(
                    "all {flood_count} flooded messages were delivered; the buffer bound did not engage"
                ));
            }
            Ok(())
        }
        "receive-buffer-overflow-unacknowledged" => {
            // Same flood, but the server never answers the overflow-driven
            // close frame: the connection must still reach terminal
            // `closed` within the host's bound, the backlog must stay
            // receivable, and `wait-closed` reports an abnormal closure.
            let flood_count = (4 * config.max_inbound_buffer_bytes) / 1024;
            let ws = connect(
                config,
                &format!("/burst-then-ignore?count={flood_count}&size=1024"),
                &[],
            )
            .await?;
            let _ = ws.wait_closed().await;
            let mut drained = 0u32;
            loop {
                match ws.receive().await {
                    Ok(Message::Binary(bytes)) => {
                        if bytes != burst_payload(drained, 1024) {
                            return Err(format!("backlog message {drained} corrupted"));
                        }
                        drained += 1;
                    }
                    Ok(Message::String(_)) => return Err("unexpected text message".to_string()),
                    Err(Error::ReceiveBufferOverflow) => break,
                    Err(other) => {
                        return Err(format!(
                            "expected receive-buffer-overflow after the backlog, got {}",
                            describe(&other)
                        ))
                    }
                }
            }
            if drained == 0 {
                return Err("pre-overflow backlog was not receivable".to_string());
            }
            match ws.wait_closed().await {
                None => Ok(()),
                Some(info) => Err(format!(
                    "unacknowledged overflow close produced close-info code={}",
                    info.code
                )),
            }
        }
        "overflow-oversized-message" => {
            // A single message larger than the whole bound overflows
            // immediately: it is never delivered, and nothing precedes it
            // in the backlog.
            let oversized = config.max_inbound_buffer_bytes + 1024;
            let ws = connect(config, &format!("/burst?count=1&size={oversized}"), &[]).await?;
            match ws.receive().await {
                Err(Error::ReceiveBufferOverflow) => Ok(()),
                Ok(_) => {
                    Err("an oversized message was delivered past the buffer bound".to_string())
                }
                Err(other) => Err(format!(
                    "expected receive-buffer-overflow, got {}",
                    describe(&other)
                )),
            }
        }
        "overflow-oversized-message-pending" => {
            // The bound holds even when a receiver is already waiting for
            // the message: an oversized message overflows rather than
            // bypassing the buffer into the waiting receive.
            let oversized = config.max_inbound_buffer_bytes + 1024;
            let ws = connect(
                config,
                &format!("/burst-on-message?count=1&size={oversized}"),
                &[],
            )
            .await?;
            // `join!` polls in order: the receive is pending before the
            // trigger message asks the server to burst.
            let pending = ws.receive();
            let trigger = async { send(&ws, Message::Binary(vec![1])).await };
            let (received, sent) = futures::join!(pending, trigger);
            sent?;
            match received {
                Err(Error::ReceiveBufferOverflow) => Ok(()),
                Ok(_) => Err(
                    "an oversized message was handed to a pending receive past the bound"
                        .to_string(),
                ),
                Err(other) => Err(format!(
                    "expected receive-buffer-overflow, got {}",
                    describe(&other)
                )),
            }
        }
        other => Err(format!("unhandled test id {other:?}")),
    }
}
