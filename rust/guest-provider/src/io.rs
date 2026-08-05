//! The in-memory `Read`/`Write` transport tungstenite's sans-IO core
//! drives. Reads pull from a buffer the pump feeds with bytes from the
//! wit inbound stream (`WouldBlock` when empty); writes append to a
//! buffer the pump drains into the wit outbound stream. The buffers are
//! shared handles, so the pump keeps feeding and draining while
//! tungstenite's state machines own the `VirtualIo` itself.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;

#[derive(Default)]
struct Buffers {
    inbound: VecDeque<u8>,
    /// The wit inbound stream ended: reads drain the buffer then report
    /// EOF instead of `WouldBlock`.
    eof: bool,
    outbound: Vec<u8>,
}

/// The shared feed/drain handle.
#[derive(Clone, Default)]
pub(crate) struct IoHandle {
    buffers: Rc<RefCell<Buffers>>,
}

impl IoHandle {
    pub(crate) fn new() -> IoHandle {
        IoHandle::default()
    }

    /// The `Read + Write` endpoint to hand to tungstenite.
    pub(crate) fn io(&self) -> VirtualIo {
        VirtualIo {
            buffers: Rc::clone(&self.buffers),
        }
    }

    pub(crate) fn feed(&self, bytes: &[u8]) {
        self.buffers.borrow_mut().inbound.extend(bytes);
    }

    pub(crate) fn set_eof(&self) {
        self.buffers.borrow_mut().eof = true;
    }

    /// Take everything tungstenite has written since the last drain.
    pub(crate) fn drain_outbound(&self) -> Vec<u8> {
        std::mem::take(&mut self.buffers.borrow_mut().outbound)
    }
}

pub(crate) struct VirtualIo {
    buffers: Rc<RefCell<Buffers>>,
}

impl Read for VirtualIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut buffers = self.buffers.borrow_mut();
        if buffers.inbound.is_empty() {
            return if buffers.eof {
                Ok(0)
            } else {
                Err(std::io::ErrorKind::WouldBlock.into())
            };
        }
        let n = buf.len().min(buffers.inbound.len());
        for slot in buf.iter_mut().take(n) {
            *slot = buffers.inbound.pop_front().expect("length checked");
        }
        Ok(n)
    }
}

impl Write for VirtualIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffers.borrow_mut().outbound.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
