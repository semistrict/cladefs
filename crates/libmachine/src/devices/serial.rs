// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The console: rust-vmm's 16550 UART model, COM1 on a PC with its interrupt delivered through
//! a KVM irqfd, MMIO on the arm64 board with its interrupt an SPI of the GIC.

use vm_superio::Trigger;
use vm_superio::serial::NoEvents;

/// Where the UART's interrupt goes.
pub(crate) enum IrqLine {
    /// Nowhere: the bare platform has no interrupt controller.
    None,
    /// An eventfd registered as an irqfd for the UART's GSI.
    #[cfg(target_os = "linux")]
    EventFd(crate::sys::EventFd),
    /// An SPI of the VM's GIC, raised as an edge.
    #[cfg(target_os = "macos")]
    Spi(std::sync::Arc<crate::backend::hvf::Shared>, u32),
}

impl Trigger for IrqLine {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        match self {
            IrqLine::None => Ok(()),
            #[cfg(target_os = "linux")]
            IrqLine::EventFd(evt) => evt.write(1),
            #[cfg(target_os = "macos")]
            IrqLine::Spi(vm, intid) => crate::backend::hvf::raise_spi(vm, *intid),
        }
    }
}

/// Where the guest's transmitted bytes go: the most recent `limit` of them
/// kept for `Machine::serial_output`, and every one copied to a console
/// writer if one is set.
///
/// The console sits behind a mutex (never contended: only the vCPU thread
/// writes) so that any `Send` writer works while `Machine` stays `Sync`.
pub(crate) struct Output {
    /// The most recent output, at most `limit` bytes once trimmed; it may
    /// hold up to half as much again between trims (amortized cost).
    collected: Vec<u8>,
    limit: usize,
    pub(crate) console: Option<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
}

impl Output {
    pub(crate) fn new(limit: usize) -> Self {
        Self { collected: Vec::new(), limit, console: None }
    }

    /// The most recent output, at most `limit` bytes.
    pub(crate) fn recent(&self) -> &[u8] {
        let skip = self.collected.len().saturating_sub(self.limit);
        &self.collected[skip..]
    }

    /// Takes the most recent output, leaving nothing.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.collected);
        let skip = out.len().saturating_sub(self.limit);
        out.drain(..skip);
        out
    }

    pub(crate) fn len(&self) -> usize {
        self.recent().len()
    }
}

impl std::io::Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.collected.extend_from_slice(buf);
        // Trim once the excess reaches half the limit, so trimming costs
        // amortized O(1) per byte and memory stays under 1.5 x limit.
        if self.collected.len() > self.limit + self.limit / 2 {
            let skip = self.collected.len() - self.limit;
            self.collected.drain(..skip);
            self.collected.shrink_to(self.limit + self.limit / 2);
        }
        if let Some(console) = &mut self.console {
            let console = console.get_mut().unwrap_or_else(|e| e.into_inner());
            // The guest must not stall on a failing console: drop it instead.
            if console.write_all(buf).and_then(|()| console.flush()).is_err() {
                self.console = None;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The machine's [`Output`], which the UART writes to: the machine keeps
/// it, so that the captured output and the console carry on across the
/// UARTs a suspension's restores make ([`crate::suspend`]).
pub(crate) type SharedOutput = std::sync::Arc<std::sync::Mutex<Output>>;

/// What the UART writes through: the shared output, locked per write
/// (never contended for long: only a vCPU thread writes, and readers copy).
pub(crate) struct Writer(pub(crate) SharedOutput);

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A new output keeping the most recent `limit` bytes.
pub(crate) fn output(limit: usize) -> SharedOutput {
    std::sync::Arc::new(std::sync::Mutex::new(Output::new(limit)))
}

/// The UART.
pub(crate) type Serial = vm_superio::Serial<IrqLine, NoEvents, Writer>;

/// The UART, writing to `output`.
pub(crate) fn new(irq: IrqLine, output: SharedOutput) -> Serial {
    vm_superio::Serial::new(irq, Writer(output))
}

/// The UART's registers and receive FIFO, for a snapshot.
pub(crate) fn save(serial: &Serial, e: &mut crate::snapshot::codec::Encoder) {
    let s = serial.state();
    for b in [
        s.baud_divisor_low,
        s.baud_divisor_high,
        s.interrupt_enable,
        s.interrupt_identification,
        s.line_control,
        s.line_status,
        s.modem_control,
        s.modem_status,
        s.scratch,
    ] {
        e.u8(b);
    }
    e.bytes(&s.in_buffer);
}

/// A UART in the state [`save`] wrote, raising `irq`, writing to `output`
/// (a host-side thing, not part of the snapshot).
pub(crate) fn restore(
    d: &mut crate::snapshot::codec::Decoder<'_>,
    irq: IrqLine,
    output: SharedOutput,
) -> crate::error::Result<Serial> {
    let state = vm_superio::serial::SerialState {
        baud_divisor_low: d.u8()?,
        baud_divisor_high: d.u8()?,
        interrupt_enable: d.u8()?,
        interrupt_identification: d.u8()?,
        line_control: d.u8()?,
        line_status: d.u8()?,
        modem_control: d.u8()?,
        modem_status: d.u8()?,
        scratch: d.u8()?,
        in_buffer: d.bytes()?.to_vec(),
    };
    vm_superio::Serial::from_state(&state, irq, NoEvents, Writer(output))
        .map_err(|e| crate::error::Error::Snapshot(format!("the snapshot's UART state: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn keeps_the_most_recent_bytes_up_to_the_limit() {
        let mut out = Output::new(4);
        out.write_all(b"ab").unwrap();
        assert_eq!(out.recent(), b"ab");
        for chunk in [&b"cdef"[..], b"g", b"hijklmnop"] {
            out.write_all(chunk).unwrap();
            assert!(out.collected.len() <= 6, "never more than 1.5 x limit");
        }
        assert_eq!(out.recent(), b"mnop");
        assert_eq!(out.take(), b"mnop");
        assert_eq!(out.recent(), b"");
        let mut none = Output::new(0);
        none.write_all(b"x").unwrap();
        assert_eq!(none.recent(), b"");
    }
}
