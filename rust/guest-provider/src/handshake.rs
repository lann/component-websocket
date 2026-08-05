//! The RFC 6455 opening handshake: build the upgrade request, parse and
//! validate the 101 response. Failure text becomes `connect-failed`
//! diagnostics (implementation-defined; guests must not match on it).

use crate::util::{base64, random_bytes, sha1};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Response header cap: a well-behaved 101 fits in a fraction of this.
pub(crate) const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

/// A prepared upgrade request and the accept value its response must echo.
pub(crate) struct Request {
    pub(crate) bytes: Vec<u8>,
    pub(crate) expected_accept: String,
}

pub(crate) fn build_request(
    host_header: &str,
    path_and_query: &str,
    protocols: &[String],
) -> Request {
    let key = base64(&random_bytes::<16>());
    let expected_accept = base64(&sha1(format!("{key}{WS_GUID}").as_bytes()));
    let mut request = format!(
        "GET {path_and_query} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: {key}\r\n"
    );
    if !protocols.is_empty() {
        request.push_str(&format!(
            "Sec-WebSocket-Protocol: {}\r\n",
            protocols.join(", ")
        ));
    }
    request.push_str("\r\n");
    Request {
        bytes: request.into_bytes(),
        expected_accept,
    }
}

/// The validated pieces of a 101 response.
pub(crate) struct Response {
    /// The subprotocol the server selected, or empty.
    pub(crate) negotiated: String,
}

/// Parse and validate the response header block (everything before the
/// terminating CRLFCRLF; the caller keeps any bytes after it — they are
/// the start of frame data).
pub(crate) fn parse_response(header: &[u8], expected_accept: &str) -> Result<Response, String> {
    let text = std::str::from_utf8(header).map_err(|_| "response is not UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut status_parts = status_line.splitn(3, ' ');
    let version = status_parts.next().unwrap_or("");
    let status = status_parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.1") {
        return Err(format!("unexpected response version: {status_line:?}"));
    }
    if status != "101" {
        return Err(format!("server answered the upgrade with status {status}"));
    }

    let mut upgrade_ok = false;
    let mut connection_ok = false;
    let mut accept: Option<&str> = None;
    let mut negotiated: Option<&str> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade_ok = value.eq_ignore_ascii_case("websocket"),
            "connection" => {
                connection_ok = value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
            }
            "sec-websocket-accept" => accept = Some(value),
            "sec-websocket-protocol" => negotiated = Some(value),
            _ => {}
        }
    }
    if !upgrade_ok {
        return Err("response did not upgrade to websocket".to_string());
    }
    if !connection_ok {
        return Err("response Connection header does not include Upgrade".to_string());
    }
    match accept {
        Some(value) if value == expected_accept => {}
        Some(_) => return Err("Sec-WebSocket-Accept does not match the key".to_string()),
        None => return Err("response is missing Sec-WebSocket-Accept".to_string()),
    }
    Ok(Response {
        negotiated: negotiated.unwrap_or("").to_string(),
    })
}
