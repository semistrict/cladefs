// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The machine's I/O ports and MMIO devices, as every vCPU thread reaches
//! them: the vCPU exits that the hypervisor hands to the VMM are dispatched
//! here, from whichever thread took them.
//!
//! Each device guards its own state: the UART sits behind a mutex, and the PCI bus locks per
//! function (with its trapped-BAR table behind a read-write lock). On the arm64 board the UART is
//! MMIO, and power goes through PSCI, which the backend handles.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::{self, Exit, Platform};
use crate::devices::serial::Serial;
use crate::error::{Error, Result};
use crate::exits::{ExitCounters, ExitKind};
use crate::machine::ExitReason;

/// The UART's eight registers, a byte apart.
const UART: std::ops::Range<u64> = {
    use crate::arch::aarch64::layout::UART_ADDR;
    UART_ADDR..UART_ADDR + 8
};
/// The RTC's page.
const RTC: std::ops::Range<u64> = {
    use crate::arch::aarch64::layout::{RTC_ADDR, RTC_SIZE};
    RTC_ADDR..RTC_ADDR + RTC_SIZE
};

/// Every device a vCPU exit can reach.
pub(crate) struct Bus {
    platform: Platform,
    serial: Mutex<Serial>,
    pci: Option<Arc<crate::devices::Pci>>,
    /// The arm64 board's real-time clock.
    rtc: crate::devices::rtc::Rtc,
}

impl Bus {
    /// A bare machine's bus: the UART alone, every other access reported.
    pub(crate) fn bare(serial: Serial) -> Self {
        Self {
            platform: Platform::Bare,
            serial: Mutex::new(serial),
            pci: None,
            rtc: Default::default(),
        }
    }

    /// The board's bus: the UART, the RTC and the PCI bus.
    pub(crate) fn board(serial: Serial, pci: Arc<crate::devices::Pci>) -> Self {
        Self {
            platform: Platform::Board,
            serial: Mutex::new(serial),
            pci: Some(pci),
            rtc: Default::default(),
        }
    }

    /// The UART.
    pub(crate) fn serial(&self) -> MutexGuard<'_, Serial> {
        self.serial.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The arm64 board's real-time clock.
    pub(crate) fn rtc(&self) -> &crate::devices::rtc::Rtc {
        &self.rtc
    }

    /// Handles one vCPU exit, and counts it in `counts` by the device it
    /// reached. `Some(reason)` ends the run: the guest powered off or reset,
    /// or on a bare machine made an access no device handles (reads are
    /// answered with zeroes first, so running again resumes it).
    pub(crate) fn handle(
        &self,
        exit: Exit<'_>,
        vm: &backend::Vm,
        counts: &ExitCounters,
    ) -> Result<Option<ExitReason>> {
        let pc = self.platform == Platform::Board;
        match exit {
            Exit::MmioWrite { addr, data } if UART.contains(&addr) => {
                counts.count(ExitKind::Serial);
                self.serial().write((addr - UART.start) as u8, data[0]).map_err(|e| {
                    Error::Hypervisor {
                        op: "raise the UART's interrupt",
                        source: std::io::Error::other(e.to_string()),
                    }
                })?;
            }
            Exit::MmioRead { addr, data } if UART.contains(&addr) => {
                counts.count(ExitKind::Serial);
                data.fill(0);
                data[0] = self.serial().read((addr - UART.start) as u8);
            }
            Exit::MmioWrite { addr, data } if pc && RTC.contains(&addr) => {
                counts.count(ExitKind::Rtc);
                let mut word = [0u8; 4];
                let len = data.len().min(4);
                word[..len].copy_from_slice(&data[..len]);
                self.rtc.write(addr - RTC.start, u32::from_le_bytes(word));
            }
            Exit::MmioRead { addr, data } if pc && RTC.contains(&addr) => {
                counts.count(ExitKind::Rtc);
                let word = self.rtc.read(addr - RTC.start).to_le_bytes();
                data.fill(0);
                let len = data.len().min(4);
                data[..len].copy_from_slice(&word[..len]);
            }
            Exit::MmioWrite { addr, data } => {
                if let Some(pci) = &self.pci {
                    // Copied, so that the device may use the VM (to route an
                    // interrupt, map a BAR) while handling the write.
                    let mut buf = [0u8; 8];
                    let len = data.len().min(buf.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    let handled = pci.mmio_write(addr, &buf[..len], vm);
                    if !matches!(handled, Ok(false)) {
                        counts.count(pci_kind(addr));
                        return handled.map(|_| None);
                    }
                }
                counts.count(ExitKind::UnclaimedMmio);
                if !pc {
                    return Ok(Some(ExitReason::UnhandledMmio {
                        addr,
                        size: data.len().min(8) as u8,
                        write: true,
                    }));
                }
            }
            Exit::MmioRead { addr, data } => {
                if self.pci.as_ref().is_some_and(|pci| pci.mmio_read(addr, data)) {
                    counts.count(pci_kind(addr));
                    return Ok(None);
                }
                counts.count(ExitKind::UnclaimedMmio);
                data.fill(0xff);
                if !pc {
                    return Ok(Some(ExitReason::UnhandledMmio {
                        addr,
                        size: data.len() as u8,
                        write: false,
                    }));
                }
            }
            Exit::Interrupted => counts.count(ExitKind::Interrupted),
            Exit::Handled(kind) => counts.count(kind),
            Exit::Done(reason) => {
                counts.count(match reason {
                    ExitReason::Halt => ExitKind::Halt,
                    _ => ExitKind::Other,
                });
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }
}

/// The kind of an MMIO exit the PCI bus handled: configuration space
/// (ECAM), else a function's BAR.
fn pci_kind(addr: u64) -> ExitKind {
    use crate::arch::layout::{PCI_ECAM_ADDR, PCI_ECAM_SIZE};
    if (PCI_ECAM_ADDR..PCI_ECAM_ADDR + PCI_ECAM_SIZE).contains(&addr) {
        ExitKind::PciConfig
    } else {
        ExitKind::PciBar
    }
}
