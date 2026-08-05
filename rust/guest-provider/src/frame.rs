//! RFC 6455 frame codec: an incremental parser over received byte chunks
//! and a client-side (masked) frame builder.
//!
//! No extensions are negotiated, so any reserved bit is a protocol
//! violation. Violations tear the transport down (an abnormal closure);
//! they are never surfaced as data.

use crate::util::random_bytes;

/// A complete inbound frame.
#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) fin: bool,
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

pub(crate) const OP_CONTINUATION: u8 = 0x0;
pub(crate) const OP_TEXT: u8 = 0x1;
pub(crate) const OP_BINARY: u8 = 0x2;
pub(crate) const OP_CLOSE: u8 = 0x8;
pub(crate) const OP_PING: u8 = 0x9;
pub(crate) const OP_PONG: u8 = 0xA;

/// The incremental frame parser. Feed bytes with [`extend`], pull frames
/// with [`next_frame`].
///
/// [`extend`]: Parser::extend
/// [`next_frame`]: Parser::next_frame
pub(crate) struct Parser {
    buffer: Vec<u8>,
    /// Single-frame cap (the transport cap: messages past it take the
    /// immediate-teardown overflow path, mirroring the reference).
    pub(crate) max_frame_bytes: usize,
}

/// One parse step's outcome.
pub(crate) enum Parsed {
    /// A complete frame.
    Frame(Frame),
    /// More bytes are needed.
    Incomplete,
    /// The peer violated the protocol (reserved bits, a masked
    /// server frame, an oversized control frame, ...). The label is a
    /// development aid; violations tear down without surfacing it.
    Violation(#[allow(dead_code)] &'static str),
    /// A frame larger than the transport cap.
    TooLarge,
}

impl Parser {
    pub(crate) fn new(max_frame_bytes: usize) -> Parser {
        Parser {
            buffer: Vec::new(),
            max_frame_bytes,
        }
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub(crate) fn next_frame(&mut self) -> Parsed {
        let buf = &self.buffer;
        if buf.len() < 2 {
            return Parsed::Incomplete;
        }
        let b0 = buf[0];
        let b1 = buf[1];
        let fin = b0 & 0x80 != 0;
        if b0 & 0x70 != 0 {
            return Parsed::Violation("reserved bits set with no extension negotiated");
        }
        let opcode = b0 & 0x0F;
        let masked = b1 & 0x80 != 0;
        if masked {
            return Parsed::Violation("server frames must not be masked");
        }
        let len7 = (b1 & 0x7F) as usize;
        let (payload_len, header_len) = match len7 {
            126 => {
                if buf.len() < 4 {
                    return Parsed::Incomplete;
                }
                (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
            }
            127 => {
                if buf.len() < 10 {
                    return Parsed::Incomplete;
                }
                let mut be = [0u8; 8];
                be.copy_from_slice(&buf[2..10]);
                let len = u64::from_be_bytes(be);
                if len > usize::MAX as u64 {
                    return Parsed::TooLarge;
                }
                (len as usize, 10)
            }
            n => (n, 2),
        };
        if opcode >= OP_CLOSE {
            // Control frames: unfragmented, payload at most 125 bytes.
            if !fin {
                return Parsed::Violation("fragmented control frame");
            }
            if payload_len > 125 {
                return Parsed::Violation("control frame payload over 125 bytes");
            }
        }
        if payload_len > self.max_frame_bytes {
            return Parsed::TooLarge;
        }
        let total = header_len + payload_len;
        if buf.len() < total {
            return Parsed::Incomplete;
        }
        let payload = buf[header_len..total].to_vec();
        self.buffer.drain(..total);
        Parsed::Frame(Frame {
            fin,
            opcode,
            payload,
        })
    }
}

/// Build one masked client frame.
pub(crate) fn build_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = random_bytes::<4>();
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    frame
}

/// Build the close frame payload for a local close: an empty payload when
/// no code is given (the peer observes 1005), else code + reason.
pub(crate) fn close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    match code {
        None => Vec::new(),
        Some(code) => {
            let mut payload = Vec::with_capacity(2 + reason.len());
            payload.extend_from_slice(&code.to_be_bytes());
            payload.extend_from_slice(reason.as_bytes());
            payload
        }
    }
}

/// Parse a peer close frame payload: none/empty maps to 1005 with an
/// empty reason; a 1-byte or invalid-UTF-8 payload is a violation.
pub(crate) fn parse_close_payload(payload: &[u8]) -> Result<(u16, String), &'static str> {
    match payload.len() {
        0 => Ok((1005, String::new())),
        1 => Err("close frame with a 1-byte payload"),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            let reason = std::str::from_utf8(&payload[2..])
                .map_err(|_| "close reason is not valid UTF-8")?;
            Ok((code, reason.to_string()))
        }
    }
}
