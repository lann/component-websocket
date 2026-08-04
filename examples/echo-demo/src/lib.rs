//! The example guest component: host-agnostic Rust that drives the
//! `lann:websocket/connections` interface. The same binary runs unchanged
//! under the Wasmtime host (`examples/wasmtime-demo`) and the jco host
//! (`examples/jco-demo`); the conformance suite (`conformance/`) is the
//! behavioral gate — this demo just shows the surface in use.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "websocket-echo-demo",
        generate_all,
    });
}

use bindings::exports::demo::websocket_echo::demo::Guest;
use bindings::lann::websocket::connections::Websocket;
use bindings::lann::websocket::types::{Error, Message};

struct Component;

fn describe(err: &Error) -> String {
    match err {
        Error::InvalidUrl(msg) => format!("invalid-url: {msg}"),
        Error::ConnectFailed(msg) => format!("connect-failed: {msg}"),
        Error::Closed => "closed".to_string(),
        Error::ReceivingViaStream => "receiving-via-stream".to_string(),
        Error::ReceiveBufferOverflow => "receive-buffer-overflow".to_string(),
        Error::InvalidArgument(msg) => format!("invalid-argument: {msg}"),
        Error::Other(msg) => msg.clone(),
    }
}

impl Guest for Component {
    async fn run(url: String, count: u32) -> Result<u32, String> {
        let ws = Websocket::connect(url, Vec::new())
            .await
            .map_err(|e| format!("connect: {}", describe(&e)))?;

        let mut received = 0u32;
        for index in 0..count {
            let payload: Vec<u8> = index.to_le_bytes().to_vec();
            ws.send(Message::Binary(payload.clone()))
                .await
                .map_err(|e| format!("send {index}: {}", describe(&e)))?;
            match ws
                .receive()
                .await
                .map_err(|e| format!("receive {index}: {}", describe(&e)))?
            {
                Message::Binary(bytes) if bytes == payload => received += 1,
                other => {
                    return Err(format!(
                        "echo {index} mismatched (got {})",
                        match other {
                            Message::Binary(_) => "different bytes",
                            Message::String(_) => "text",
                        }
                    ))
                }
            }
        }

        ws.close(Some(1000), "demo complete")
            .map_err(|e| format!("close: {}", describe(&e)))?;
        // The echo server acknowledges the close; wait for the handshake so
        // the round trip is fully clean.
        let _ = ws.wait_closed().await;
        Ok(received)
    }
}

bindings::export!(Component with_types_in bindings);
