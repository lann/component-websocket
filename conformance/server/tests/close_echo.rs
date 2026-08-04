//! Pins the close-frame acknowledgement the conformance corpus depends
//! on: the `close-local*` and `close-boundary-codes` rows assert that a
//! client's close frame comes back with the same code and reason, which
//! this server produces via tungstenite's automatic close reply. That
//! echo-the-whole-frame behavior is not RFC-required (RFC 6455 only says
//! an endpoint "typically echos the status code"), so a tungstenite
//! upgrade that stops echoing must fail here — implicating the suite's
//! server — rather than failing every conformance target at once.

use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn close_frames_are_echoed_verbatim() {
    let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap())
        .await
        .expect("spawn echo server");
    let url = format!("{}/echo", server.base_url());
    let (mut ws, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    ws.send(Message::Close(Some(CloseFrame {
        code: 4321.into(),
        reason: "suite-pinned reason".into(),
    })))
    .await
    .expect("send close frame");

    let mut acknowledged = None;
    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Close(frame)) => {
                acknowledged = frame;
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("read failed before the close acknowledgement: {err}"),
        }
    }
    let frame = acknowledged.expect("an acknowledgement close frame");
    assert_eq!(u16::from(frame.code), 4321);
    assert_eq!(frame.reason.as_str(), "suite-pinned reason");
}

#[tokio::test]
async fn codeless_close_frames_are_echoed_codeless() {
    let server = conformance_echod::spawn("127.0.0.1:0".parse().unwrap())
        .await
        .expect("spawn echo server");
    let url = format!("{}/echo", server.base_url());
    let (mut ws, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    ws.send(Message::Close(None)).await.expect("send close");

    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Close(frame)) => {
                assert!(
                    frame.is_none(),
                    "code-less close acknowledged with a frame body: {frame:?}"
                );
                return;
            }
            Ok(_) => {}
            Err(err) => panic!("read failed before the close acknowledgement: {err}"),
        }
    }
    panic!("connection ended without a close acknowledgement");
}
