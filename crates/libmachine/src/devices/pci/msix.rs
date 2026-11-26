// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MSI-X (PCI Local Bus 3.0, section 6.8.2): a function's table of
//! interrupt messages and its pending bit array, delivered by the
//! hypervisor without stopping a vCPU.
//!
//! Each vector has a GSI of its own, routed to the vector's message, and an
//! eventfd the device signals. The eventfd is the GSI's irqfd only while the
//! vector is deliverable (MSI-X enabled, neither the function nor the vector
//! masked): while masked, signals accumulate in the eventfd, which is the
//! vector's pending bit, and when the vector is unmasked the hypervisor
//! delivers what is pending as the irqfd is assigned again.

use crate::sys::{EFD_NONBLOCK, EventFd};

use super::{MsiMessage, PciHost};
use crate::error::{Error, Result};
use crate::snapshot::codec::{Decoder, Encoder};

/// The MSI-X capability ID.
pub(crate) const CAPABILITY_ID: u8 = 0x11;
/// Bytes in a table entry: message address (64-bit), data, vector control.
pub(crate) const ENTRY_SIZE: usize = 16;
/// The capability's length.
pub(crate) const CAPABILITY_LEN: usize = 12;
/// The capability's message control register, at this offset in it.
pub(crate) const CONTROL_OFFSET: usize = 2;
const CONTROL_ENABLE: u16 = 1 << 15;
const CONTROL_FUNCTION_MASK: u16 = 1 << 14;
const VECTOR_CONTROL_MASK: u32 = 1;
/// The most vectors a function may have.
pub(crate) const MAX_VECTORS: u16 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    address: u64,
    data: u32,
    control: u32,
}

impl Entry {
    fn bytes(&self) -> [u8; ENTRY_SIZE] {
        let mut b = [0; ENTRY_SIZE];
        b[0..8].copy_from_slice(&self.address.to_le_bytes());
        b[8..12].copy_from_slice(&self.data.to_le_bytes());
        b[12..16].copy_from_slice(&self.control.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; ENTRY_SIZE]) -> Self {
        Self {
            address: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            data: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            // Only the mask bit is defined; the rest is reserved, read as 0.
            control: u32::from_le_bytes(b[12..16].try_into().unwrap()) & VECTOR_CONTROL_MASK,
        }
    }

    fn masked(&self) -> bool {
        self.control & VECTOR_CONTROL_MASK != 0
    }
}

struct Vector {
    gsi: u32,
    irqfd: EventFd,
    /// The message the GSI is routed to, once it has been.
    routed: Option<MsiMessage>,
    /// Whether `irqfd` is assigned to `gsi` now.
    assigned: bool,
}

/// A function's MSI-X state.
pub(crate) struct Msix {
    entries: Vec<Entry>,
    vectors: Vec<Vector>,
    enabled: bool,
    function_mask: bool,
}

impl Msix {
    /// `count` vectors on GSIs `first_gsi..first_gsi + count`, all masked,
    /// MSI-X disabled, as after reset.
    pub(crate) fn new(count: u16, first_gsi: u32) -> Result<Self> {
        assert!((1..=MAX_VECTORS).contains(&count), "{count} MSI-X vectors");
        let vectors = (0..u32::from(count))
            .map(|i| {
                Ok(Vector {
                    gsi: first_gsi + i,
                    irqfd: EventFd::new(EFD_NONBLOCK).map_err(|source| Error::Hypervisor {
                        op: "create an MSI-X vector's eventfd",
                        source,
                    })?,
                    routed: None,
                    assigned: false,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            entries: vec![
                Entry { address: 0, data: 0, control: VECTOR_CONTROL_MASK };
                usize::from(count)
            ],
            vectors,
            enabled: false,
            function_mask: false,
        })
    }

    pub(crate) fn count(&self) -> u16 {
        self.entries.len() as u16
    }

    /// The table's size in bytes.
    pub(crate) fn table_len(&self) -> usize {
        self.entries.len() * ENTRY_SIZE
    }

    /// The pending bit array's size in bytes (whole qwords).
    pub(crate) fn pba_len(&self) -> usize {
        self.entries.len().div_ceil(64) * 8
    }

    /// The capability's bytes and write mask: the table and the PBA in
    /// `bar` at the given offsets (8-byte aligned).
    pub(crate) fn capability(
        &self,
        bar: u8,
        table_offset: u32,
        pba_offset: u32,
    ) -> ([u8; CAPABILITY_LEN], [u8; CAPABILITY_LEN]) {
        assert!(bar < 6 && table_offset.is_multiple_of(8) && pba_offset.is_multiple_of(8));
        let mut body = [0; CAPABILITY_LEN];
        body[0] = CAPABILITY_ID;
        body[2..4].copy_from_slice(&(self.count() - 1).to_le_bytes());
        body[4..8].copy_from_slice(&(table_offset | u32::from(bar)).to_le_bytes());
        body[8..12].copy_from_slice(&(pba_offset | u32::from(bar)).to_le_bytes());
        let mut writable = [0; CAPABILITY_LEN];
        writable[2..4].copy_from_slice(&(CONTROL_ENABLE | CONTROL_FUNCTION_MASK).to_le_bytes());
        (body, writable)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The guest wrote the message control register.
    pub(crate) fn set_control(&mut self, control: u16, host: &dyn PciHost) -> Result<()> {
        let enabled = control & CONTROL_ENABLE != 0;
        let function_mask = control & CONTROL_FUNCTION_MASK != 0;
        if (enabled, function_mask) == (self.enabled, self.function_mask) {
            return Ok(());
        }
        self.enabled = enabled;
        self.function_mask = function_mask;
        (0..self.vectors.len()).try_for_each(|v| self.sync(v, host))
    }

    /// A guest read of the table.
    pub(crate) fn table_read(&self, offset: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            let o = offset + i;
            *b = self.entries.get(o / ENTRY_SIZE).map_or(0, |e| e.bytes()[o % ENTRY_SIZE]);
        }
    }

    /// A guest write to the table: changes what the affected vectors
    /// deliver and whether they are masked.
    pub(crate) fn table_write(
        &mut self,
        offset: usize,
        data: &[u8],
        host: &dyn PciHost,
    ) -> Result<()> {
        let first = offset / ENTRY_SIZE;
        let last = (offset + data.len()).saturating_sub(1) / ENTRY_SIZE;
        for v in first..=last.min(self.entries.len().saturating_sub(1)) {
            let mut bytes = self.entries[v].bytes();
            for (i, &b) in data.iter().enumerate() {
                let o = offset + i;
                if o / ENTRY_SIZE == v {
                    bytes[o % ENTRY_SIZE] = b;
                }
            }
            let entry = Entry::from_bytes(&bytes);
            if entry != self.entries[v] {
                self.entries[v] = entry;
                self.sync(v, host)?;
            }
        }
        Ok(())
    }

    /// A guest read of the pending bit array: a vector is pending if it is
    /// not deliverable and its eventfd holds a signal.
    pub(crate) fn pba_read(&self, offset: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            let byte = offset + i;
            *b = (0..8)
                .filter(|bit| {
                    let v = byte * 8 + bit;
                    v < self.vectors.len() && !self.vectors[v].assigned && self.pending(v)
                })
                .fold(0, |acc, bit| acc | 1 << bit);
        }
    }

    /// Whether vector `v`'s eventfd holds a signal, without consuming it.
    fn pending(&self, v: usize) -> bool {
        let evt = &self.vectors[v].irqfd;
        match evt.read() {
            Ok(n) => {
                // Put it back: nothing consumes it while it is not an irqfd.
                let _ = evt.write(n);
                true
            }
            Err(_) => false,
        }
    }

    /// The table, the control bits and the pending bits, for a snapshot.
    pub(crate) fn save(&self, e: &mut Encoder) {
        e.bool(self.enabled);
        e.bool(self.function_mask);
        e.count(self.entries.len());
        for (v, entry) in self.entries.iter().enumerate() {
            e.u64(entry.address);
            e.u32(entry.data);
            e.u32(entry.control);
            e.bool(!self.vectors[v].assigned && self.pending(v));
        }
    }

    /// Puts back what [`save`](Self::save) wrote, into a function with as
    /// many vectors, and routes the vectors again: pending ones are
    /// delivered as soon as they are deliverable.
    pub(crate) fn restore(&mut self, d: &mut Decoder<'_>, host: &dyn PciHost) -> Result<()> {
        let enabled = d.bool()?;
        let function_mask = d.bool()?;
        let n = d.count(17)?;
        if n != self.entries.len() {
            return Err(Error::Snapshot(format!(
                "the snapshot's function has {n} MSI-X vectors, this one {}",
                self.entries.len()
            )));
        }
        for v in 0..n {
            self.entries[v] = Entry {
                address: d.u64()?,
                data: d.u32()?,
                control: d.u32()? & VECTOR_CONTROL_MASK,
            };
            if d.bool()? {
                let _ = self.vectors[v].irqfd.write(1);
            }
        }
        self.enabled = enabled;
        self.function_mask = function_mask;
        (0..n).try_for_each(|v| self.sync(v, host))
    }

    /// Signals vector `v`: delivered at once if deliverable, else pending
    /// until it is. Nothing happens while MSI-X is disabled (there is no
    /// INTx), or for an out-of-range vector.
    pub(crate) fn trigger(&self, v: u16) {
        if !self.enabled {
            return;
        }
        if let Some(vector) = self.vectors.get(usize::from(v)) {
            // Fails only if the counter would overflow: it is pending anyway.
            let _ = vector.irqfd.write(1);
        }
    }

    fn deliverable(&self, v: usize) -> bool {
        self.enabled && !self.function_mask && !self.entries[v].masked()
    }

    /// Brings vector `v`'s route and irqfd in line with its entry.
    fn sync(&mut self, v: usize, host: &dyn PciHost) -> Result<()> {
        let deliverable = self.deliverable(v);
        let entry = self.entries[v];
        let vector = &mut self.vectors[v];
        if deliverable {
            let message = MsiMessage { address: entry.address, data: entry.data };
            if vector.routed != Some(message) {
                host.set_msi_route(vector.gsi, message)?;
                vector.routed = Some(message);
            }
            if !vector.assigned {
                host.assign_irqfd(&vector.irqfd, vector.gsi)?;
                vector.assigned = true;
            }
        } else if vector.assigned {
            host.deassign_irqfd(&vector.irqfd, vector.gsi)?;
            vector.assigned = false;
        }
        Ok(())
    }
}

#[cfg(test)]
impl Msix {
    /// Vector `v`'s eventfd, to see what was signalled.
    pub(crate) fn irqfd(&self, v: usize) -> &EventFd {
        &self.vectors[v].irqfd
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{FakeHost, HostCall};
    use super::*;

    fn entry_write(m: &mut Msix, host: &FakeHost, v: usize, address: u64, data: u32, masked: bool) {
        let base = v * ENTRY_SIZE;
        // As Linux writes an entry: dword by dword.
        m.table_write(base, &(address as u32).to_le_bytes(), host).unwrap();
        m.table_write(base + 4, &((address >> 32) as u32).to_le_bytes(), host).unwrap();
        m.table_write(base + 8, &data.to_le_bytes(), host).unwrap();
        m.table_write(base + 12, &u32::from(masked).to_le_bytes(), host).unwrap();
    }

    #[test]
    fn capability_names_the_table_and_pba() {
        let m = Msix::new(3, 24).unwrap();
        let (body, writable) = m.capability(0, 0x4000, 0xc000);
        assert_eq!(body[0], CAPABILITY_ID);
        assert_eq!(u16::from_le_bytes([body[2], body[3]]), 2, "table size is N-1");
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), 0x4000);
        assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 0xc000);
        assert_eq!(u16::from_le_bytes([writable[2], writable[3]]), 0xc000, "enable and mask only");
        assert_eq!(m.table_len(), 48);
        assert_eq!(m.pba_len(), 8);
        assert_eq!(Msix::new(65, 24).unwrap().pba_len(), 16);
    }

    #[test]
    fn entries_start_masked_and_read_back() {
        let host = FakeHost::default();
        let mut m = Msix::new(2, 24).unwrap();
        let mut b = [0; 16];
        m.table_read(16, &mut b);
        assert_eq!(b[12], 1, "masked after reset");
        entry_write(&mut m, &host, 1, 0xfee0_1000, 0x4041, true);
        m.table_read(16, &mut b);
        assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), 0xfee0_1000);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 0x4041);
        // Reserved vector control bits read as zero.
        m.table_write(28, &0xffff_fffeu32.to_le_bytes(), &host).unwrap();
        m.table_read(28, &mut b[..4]);
        assert_eq!(b[..4], [0, 0, 0, 0]);
        // Past the table: zeroes, writes ignored.
        m.table_write(32, &[0xff; 4], &host).unwrap();
        m.table_read(32, &mut b[..4]);
        assert_eq!(b[..4], [0; 4]);
        assert!(host.calls().is_empty(), "MSI-X is disabled: nothing routed");
    }

    #[test]
    fn a_vector_is_routed_and_assigned_only_while_deliverable() {
        let host = FakeHost::default();
        let mut m = Msix::new(2, 30).unwrap();
        entry_write(&mut m, &host, 0, 0xfee0_0000, 0x41, false);
        assert!(host.calls().is_empty(), "MSI-X still disabled");

        m.set_control(CONTROL_ENABLE, &host).unwrap();
        assert_eq!(
            host.take(),
            vec![
                HostCall::Route(30, MsiMessage { address: 0xfee0_0000, data: 0x41 }),
                HostCall::Assign(30)
            ],
            "only the unmasked vector"
        );

        // Masking the vector deassigns its irqfd; the route stays.
        m.table_write(12, &1u32.to_le_bytes(), &host).unwrap();
        assert_eq!(host.take(), vec![HostCall::Deassign(30)]);
        // A new message while masked changes nothing yet.
        m.table_write(8, &0x42u32.to_le_bytes(), &host).unwrap();
        assert!(host.take().is_empty());
        // Unmasked: the new route, then the irqfd.
        m.table_write(12, &0u32.to_le_bytes(), &host).unwrap();
        assert_eq!(
            host.take(),
            vec![
                HostCall::Route(30, MsiMessage { address: 0xfee0_0000, data: 0x42 }),
                HostCall::Assign(30)
            ]
        );
        // Unmasking again with the same message: no ioctl.
        m.table_write(12, &0u32.to_le_bytes(), &host).unwrap();
        assert!(host.take().is_empty());

        // The function mask masks every vector; lifting it restores them.
        m.set_control(CONTROL_ENABLE | CONTROL_FUNCTION_MASK, &host).unwrap();
        assert_eq!(host.take(), vec![HostCall::Deassign(30)]);
        m.set_control(CONTROL_ENABLE, &host).unwrap();
        assert_eq!(host.take(), vec![HostCall::Assign(30)], "the route is unchanged");

        m.set_control(0, &host).unwrap();
        assert_eq!(host.take(), vec![HostCall::Deassign(30)]);
    }

    #[test]
    fn a_masked_vector_is_pending_until_unmasked() {
        let host = FakeHost::default();
        let mut m = Msix::new(9, 24).unwrap();
        m.set_control(CONTROL_ENABLE, &host).unwrap();
        let mut pba = [0; 8];
        m.pba_read(0, &mut pba);
        assert_eq!(pba, [0; 8]);

        m.trigger(8);
        m.trigger(8);
        m.pba_read(0, &mut pba);
        assert_eq!(pba[1], 1, "vector 8 pending");
        m.pba_read(0, &mut pba);
        assert_eq!(pba[1], 1, "reading the PBA does not consume it");

        // Unmasked: the irqfd is assigned, and the hypervisor takes the
        // pending signal from it.
        entry_write(&mut m, &host, 8, 0xfee0_0000, 0x50, false);
        assert_eq!(host.take().last(), Some(&HostCall::Assign(32)));
        m.pba_read(0, &mut pba);
        assert_eq!(pba[1], 0, "a deliverable vector is never pending");
        assert_eq!(m.vectors[8].irqfd.read().unwrap(), 2, "both signals kept for delivery");
    }

    #[test]
    fn nothing_is_signalled_while_msix_is_disabled() {
        let m = Msix::new(1, 24).unwrap();
        m.trigger(0);
        m.trigger(7); // out of range
        assert!(m.vectors[0].irqfd.read().is_err(), "no signal");
    }
}
