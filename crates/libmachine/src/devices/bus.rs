// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The machine's I/O ports and MMIO devices, as every vCPU thread reaches
//! them: the vCPU exits that the hypervisor hands to the VMM are dispatched
//! here, from whichever thread took them.
//!
//! Each device guards its own state: the UART sits behind a mutex, the ACPI
//! registers have no state, and the PCI bus locks per function (with its
//! trapped-BAR table behind a read-write lock). On a PC the UART and the ACPI
//! registers are I/O ports; on the arm64 board the UART is MMIO, and power
//! goes through PSCI, which the backend handles.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::{self, Exit, Platform};
use crate::devices::serial::Serial;
use crate::error::{Error, Result};
use crate::exits::{ExitCounters, ExitKind};
use crate::machine::ExitReason;

#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::layout::{
    ACPI_RESET_PORT, ACPI_RESET_VALUE, ACPI_S5_SLP_TYP, ACPI_SLEEP_CONTROL_PORT,
    ACPI_SLEEP_STATUS_PORT,
};

/// COM1 and its eight registers.
#[cfg(target_arch = "x86_64")]
const COM1: std::ops::Range<u16> = 0x3f8..0x400;
/// SLP_EN in the sleep control register.
#[cfg(target_arch = "x86_64")]
const ACPI_SLP_EN: u8 = 1 << 5;
/// The UART's eight registers, a byte apart.
#[cfg(target_arch = "aarch64")]
const UART: std::ops::Range<u64> = {
    use crate::arch::aarch64::layout::UART_ADDR;
    UART_ADDR..UART_ADDR + 8
};
/// The RTC's page.
#[cfg(target_arch = "aarch64")]
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
    #[cfg(target_arch = "aarch64")]
    rtc: crate::devices::rtc::Rtc,
}

impl Bus {
    /// A bare machine's bus: the UART alone, every other access reported.
    pub(crate) fn bare(serial: Serial) -> Self {
        Self {
            platform: Platform::Bare,
            serial: Mutex::new(serial),
            pci: None,
            #[cfg(target_arch = "aarch64")]
            rtc: Default::default(),
        }
    }

    /// The board's bus: the UART, the PCI bus, and on a PC the ACPI registers.
    pub(crate) fn board(serial: Serial, pci: Arc<crate::devices::Pci>) -> Self {
        Self {
            platform: Platform::Board,
            serial: Mutex::new(serial),
            pci: Some(pci),
            #[cfg(target_arch = "aarch64")]
            rtc: Default::default(),
        }
    }

    /// The UART.
    pub(crate) fn serial(&self) -> MutexGuard<'_, Serial> {
        self.serial.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The arm64 board's real-time clock.
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn rtc(&self) -> &crate::devices::rtc::Rtc {
        &self.rtc
    }

    /// The kind of exit an access to `port` is, by the device at it.
    #[cfg(target_arch = "x86_64")]
    fn io_kind(&self, port: u16) -> ExitKind {
        if COM1.contains(&port) {
            ExitKind::Serial
        } else if self.platform == Platform::Board && is_acpi_port(port) {
            ExitKind::Acpi
        } else {
            ExitKind::UnclaimedIo
        }
    }

    /// Writes one byte to `port`: COM1, the ACPI registers (on a PC), or
    /// nothing. `Some(reason)` if the write powers the machine off or
    /// resets it.
    #[cfg(target_arch = "x86_64")]
    fn write_port(&self, port: u16, byte: u8, pc: bool) -> Result<Option<ExitReason>> {
        if COM1.contains(&port) {
            self.serial().write((port - COM1.start) as u8, byte).map_err(|e| {
                Error::Hypervisor {
                    op: "raise the UART's interrupt",
                    source: std::io::Error::other(e.to_string()),
                }
            })?;
            Ok(None)
        } else if pc {
            Ok(acpi_write(port, byte))
        } else {
            Ok(None)
        }
    }

    /// Reads one byte from `port`: COM1, the ACPI registers (zero), or, on a
    /// PC, an unclaimed port (all ones, as on hardware).
    #[cfg(target_arch = "x86_64")]
    fn read_port(&self, port: u16, pc: bool) -> u8 {
        if COM1.contains(&port) {
            self.serial().read((port - COM1.start) as u8)
        } else if pc && is_acpi_port(port) {
            0
        } else if pc {
            0xff
        } else {
            0
        }
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
            #[cfg(target_arch = "x86_64")]
            Exit::IoOut { port, size, data } => {
                counts.count(self.io_kind(port));
                if !pc && !COM1.contains(&port) {
                    return Ok(Some(ExitReason::UnhandledIo { port, size, write: true }));
                }
                // Each access writes its bytes to consecutive ports, as x86
                // does for a device with byte-wide registers; a string
                // instruction is several accesses to the same port.
                let mut ended = None;
                for access in data.chunks(size.into()) {
                    for (i, &byte) in access.iter().enumerate() {
                        let reason = self.write_port(port.wrapping_add(i as u16), byte, pc)?;
                        ended = ended.or(reason);
                    }
                }
                if ended.is_some() {
                    return Ok(ended);
                }
            }
            #[cfg(target_arch = "x86_64")]
            Exit::IoIn { port, size, data } => {
                counts.count(self.io_kind(port));
                if !pc && !COM1.contains(&port) {
                    data.fill(0);
                    return Ok(Some(ExitReason::UnhandledIo { port, size, write: false }));
                }
                for access in data.chunks_mut(size.into()) {
                    for (i, byte) in access.iter_mut().enumerate() {
                        *byte = self.read_port(port.wrapping_add(i as u16), pc);
                    }
                }
            }
            #[cfg(target_arch = "aarch64")]
            Exit::MmioWrite { addr, data } if UART.contains(&addr) => {
                counts.count(ExitKind::Serial);
                self.serial().write((addr - UART.start) as u8, data[0]).map_err(|e| {
                    Error::Hypervisor {
                        op: "raise the UART's interrupt",
                        source: std::io::Error::other(e.to_string()),
                    }
                })?;
            }
            #[cfg(target_arch = "aarch64")]
            Exit::MmioRead { addr, data } if UART.contains(&addr) => {
                counts.count(ExitKind::Serial);
                data.fill(0);
                data[0] = self.serial().read((addr - UART.start) as u8);
            }
            #[cfg(target_arch = "aarch64")]
            Exit::MmioWrite { addr, data } if pc && RTC.contains(&addr) => {
                counts.count(ExitKind::Rtc);
                let mut word = [0u8; 4];
                let len = data.len().min(4);
                word[..len].copy_from_slice(&data[..len]);
                self.rtc.write(addr - RTC.start, u32::from_le_bytes(word));
            }
            #[cfg(target_arch = "aarch64")]
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
            Exit::Startup => counts.count(ExitKind::Startup),
            // The vCPU loop retries a failed access, or ends the run, before the bus sees it.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Exit::AccessFailed(_) => unreachable!("the vCPU loop handles failed accesses"),
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

#[cfg(target_arch = "x86_64")]
fn is_acpi_port(port: u16) -> bool {
    matches!(port, ACPI_SLEEP_CONTROL_PORT | ACPI_SLEEP_STATUS_PORT | ACPI_RESET_PORT)
}

/// A write to the ACPI registers: soft-off (SLP_EN with SLP_TYP = S5) powers
/// off; the reset value in the reset register resets.
#[cfg(target_arch = "x86_64")]
fn acpi_write(port: u16, value: u8) -> Option<ExitReason> {
    match port {
        ACPI_SLEEP_CONTROL_PORT
            if value & ACPI_SLP_EN != 0 && (value >> 2) & 0x7 == ACPI_S5_SLP_TYP =>
        {
            Some(ExitReason::Poweroff)
        }
        ACPI_RESET_PORT if value == ACPI_RESET_VALUE => Some(ExitReason::Reset),
        _ => None,
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn acpi_soft_off_and_reset() {
        let s5 = ACPI_SLP_EN | (ACPI_S5_SLP_TYP << 2);
        assert_eq!(acpi_write(ACPI_SLEEP_CONTROL_PORT, s5), Some(ExitReason::Poweroff));
        // Another sleep state, or S5 without SLP_EN, is not soft-off.
        assert_eq!(acpi_write(ACPI_SLEEP_CONTROL_PORT, ACPI_SLP_EN | (3 << 2)), None);
        assert_eq!(acpi_write(ACPI_SLEEP_CONTROL_PORT, ACPI_S5_SLP_TYP << 2), None);
        assert_eq!(acpi_write(ACPI_RESET_PORT, ACPI_RESET_VALUE), Some(ExitReason::Reset));
        assert_eq!(acpi_write(ACPI_RESET_PORT, 0), None);
        assert_eq!(acpi_write(ACPI_SLEEP_STATUS_PORT, s5), None);
    }
}
