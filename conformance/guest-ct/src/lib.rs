//! The conformance suite as a `lann:component-test` component.
//!
//! One `#[case]` per incumbent test row; every body delegates to
//! [`body`], the verbatim port of the incumbent guest's dispatch,
//! keyed by the incumbent's flat id — so each delegator documents
//! exactly which row it ported. The flat ids map to the component-test
//! case-name hierarchy by category (the old tag vocabulary):
//! `close-remote` → `websocket/close/remote`.
//!
//! No feature tags: every current target serves the whole surface (the
//! incumbent's `targets.toml` tables were empty). When a capability gap
//! appears, gate it the component-test way — a `[features]` entry in
//! `conformance/driver-ct/targets.toml`, tags on the affected cases, and
//! a `!feature` decline probe — instead of the incumbent's
//! tag-scoped `unsupported` declarations.

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "sut-imports",
        generate_all,
    });
}

mod body;

#[component_test_sdk::suite]
mod websocket {
    mod connect {
        #[case]
        async fn basic() -> Verdict {
            crate::body::case("connect-basic").await
        }
        #[case]
        async fn invalid_url() -> Verdict {
            crate::body::case("connect-invalid-url").await
        }
        #[case]
        async fn invalid_protocols() -> Verdict {
            crate::body::case("connect-invalid-protocols").await
        }
        #[case]
        async fn refused() -> Verdict {
            crate::body::case("connect-refused").await
        }
        #[case]
        async fn rejected() -> Verdict {
            crate::body::case("connect-rejected").await
        }
        #[case]
        async fn redirect() -> Verdict {
            crate::body::case("connect-redirect").await
        }
        #[case]
        async fn timeout() -> Verdict {
            crate::body::case("connect-timeout").await
        }
    }

    mod subprotocol {
        #[case]
        async fn negotiated() -> Verdict {
            crate::body::case("subprotocol-negotiated").await
        }
        #[case]
        async fn none_offered() -> Verdict {
            crate::body::case("subprotocol-none-offered").await
        }
        #[case]
        async fn unoffered_selected() -> Verdict {
            crate::body::case("subprotocol-unoffered-selected").await
        }
        #[case]
        async fn none_selected() -> Verdict {
            crate::body::case("subprotocol-none-selected").await
        }
    }

    mod echo {
        #[case]
        async fn binary() -> Verdict {
            crate::body::case("echo-binary").await
        }
        #[case]
        async fn text() -> Verdict {
            crate::body::case("echo-text").await
        }
        #[case]
        async fn text_unicode() -> Verdict {
            crate::body::case("echo-text-unicode").await
        }
        #[case]
        async fn empty() -> Verdict {
            crate::body::case("echo-empty").await
        }
        #[case]
        async fn large() -> Verdict {
            crate::body::case("echo-large").await
        }
    }

    mod message {
        #[case]
        async fn boundaries() -> Verdict {
            crate::body::case("message-boundaries").await
        }
        #[case]
        async fn binary_text_interleave() -> Verdict {
            crate::body::case("binary-text-interleave").await
        }
        #[case]
        async fn concurrent_send_receive() -> Verdict {
            crate::body::case("concurrent-send-receive").await
        }
        #[case]
        async fn concurrent_receives() -> Verdict {
            crate::body::case("concurrent-receives").await
        }
    }

    mod close {
        #[case]
        async fn local() -> Verdict {
            crate::body::case("close-local").await
        }
        #[case]
        async fn local_default() -> Verdict {
            crate::body::case("close-local-default").await
        }
        #[case]
        async fn local_idempotent() -> Verdict {
            crate::body::case("close-local-idempotent").await
        }
        #[case]
        async fn boundary_codes() -> Verdict {
            crate::body::case("close-boundary-codes").await
        }
        #[case]
        async fn reason_unicode() -> Verdict {
            crate::body::case("close-reason-unicode").await
        }
        #[case]
        async fn invalid_code() -> Verdict {
            crate::body::case("close-invalid-code").await
        }
        #[case]
        async fn reason_too_long() -> Verdict {
            crate::body::case("close-reason-too-long").await
        }
        #[case]
        async fn reason_without_code() -> Verdict {
            crate::body::case("close-reason-without-code").await
        }
        #[case]
        async fn send_after_close() -> Verdict {
            crate::body::case("send-after-close").await
        }
        #[case]
        async fn receive_after_close() -> Verdict {
            crate::body::case("receive-after-close").await
        }
        #[case]
        async fn remote() -> Verdict {
            crate::body::case("close-remote").await
        }
        #[case]
        async fn remote_no_code() -> Verdict {
            crate::body::case("close-remote-no-code").await
        }
        #[case]
        async fn abnormal() -> Verdict {
            crate::body::case("close-abnormal").await
        }
        #[case]
        async fn receive_backlog_before_close() -> Verdict {
            crate::body::case("receive-backlog-before-close").await
        }
        #[case]
        async fn handshake_timeout() -> Verdict {
            crate::body::case("close-handshake-timeout").await
        }
        #[case]
        async fn under_send_backpressure() -> Verdict {
            crate::body::case("close-under-send-backpressure").await
        }
    }

    mod lifecycle {
        /// was: state-lifecycle
        #[case]
        async fn state() -> Verdict {
            crate::body::case("state-lifecycle").await
        }
        #[case]
        async fn wait_closed_latched() -> Verdict {
            crate::body::case("wait-closed-latched").await
        }
        #[case]
        async fn wait_closed_pending() -> Verdict {
            crate::body::case("wait-closed-pending").await
        }
    }

    mod streaming {
        #[case]
        async fn send_via_stream() -> Verdict {
            crate::body::case("send-via-stream").await
        }
        #[case]
        async fn receive_via_stream() -> Verdict {
            crate::body::case("receive-via-stream").await
        }
        /// was: stream-text-round-trip
        #[case]
        async fn text_round_trip() -> Verdict {
            crate::body::case("stream-text-round-trip").await
        }
        #[case]
        async fn send_via_stream_invalid_utf8() -> Verdict {
            crate::body::case("send-via-stream-invalid-utf8").await
        }
        #[case]
        async fn send_via_stream_length_mismatch() -> Verdict {
            crate::body::case("send-via-stream-length-mismatch").await
        }
        #[case]
        async fn send_via_stream_sent_count() -> Verdict {
            crate::body::case("send-via-stream-sent-count").await
        }
        #[case]
        async fn receive_via_stream_once() -> Verdict {
            crate::body::case("receive-via-stream-once").await
        }
        #[case]
        async fn receive_via_stream_end_on_close() -> Verdict {
            crate::body::case("receive-via-stream-end-on-close").await
        }
        #[case]
        async fn receive_via_stream_overflow() -> Verdict {
            crate::body::case("receive-via-stream-overflow").await
        }
    }

    mod flow_control {
        #[case]
        async fn receive_buffer_overflow() -> Verdict {
            crate::body::case("receive-buffer-overflow").await
        }
        #[case]
        async fn receive_buffer_overflow_unacknowledged() -> Verdict {
            crate::body::case("receive-buffer-overflow-unacknowledged").await
        }
        #[case]
        async fn overflow_oversized_message() -> Verdict {
            crate::body::case("overflow-oversized-message").await
        }
        #[case]
        async fn overflow_oversized_message_pending() -> Verdict {
            crate::body::case("overflow-oversized-message-pending").await
        }
    }
}
