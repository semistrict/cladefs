// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A PCI function's type-0 configuration space (PCI Local Bus 3.0, chapter
//! 6): the 256-byte header and capability list, with a write mask saying
//! which bits the guest may change, and memory BARs that size themselves
//! the standard way (a written all-ones reads back as the size mask).

use std::ops::Range;

/// The standard configuration space; offsets past it (PCI Express's
/// extended space) read as zero and ignore writes.
pub(crate) const CONFIG_SIZE: usize = 256;

pub(crate) const VENDOR_ID: usize = 0x00;
pub(crate) const DEVICE_ID: usize = 0x02;
pub(crate) const COMMAND: usize = 0x04;
pub(crate) const STATUS: usize = 0x06;
pub(crate) const REVISION_ID: usize = 0x08;
pub(crate) const CLASS_CODE: usize = 0x09;
pub(crate) const CACHE_LINE_SIZE: usize = 0x0c;
pub(crate) const LATENCY_TIMER: usize = 0x0d;
pub(crate) const HEADER_TYPE: usize = 0x0e;
pub(crate) const BAR0: usize = 0x10;
pub(crate) const SUBSYSTEM_VENDOR_ID: usize = 0x2c;
pub(crate) const SUBSYSTEM_ID: usize = 0x2e;
pub(crate) const CAPABILITIES_POINTER: usize = 0x34;
pub(crate) const INTERRUPT_LINE: usize = 0x3c;
pub(crate) const INTERRUPT_PIN: usize = 0x3d;
/// Where the capability list starts: right after the header.
const FIRST_CAPABILITY: usize = 0x40;

/// Command register: the function decodes its memory BARs.
pub(crate) const COMMAND_MEMORY: u16 = 1 << 1;
/// Command register bits the guest may set: memory decode, bus mastering,
/// parity and SERR# reporting, and INTx disable. (No I/O BARs, so no I/O
/// decode.)
const COMMAND_WRITABLE: u16 = (1 << 1) | (1 << 2) | (1 << 6) | (1 << 8) | (1 << 10);
/// Status register: the function has a capability list.
const STATUS_CAPABILITIES: u16 = 1 << 4;

const BAR_MEMORY_64: u32 = 0b10 << 1;
const BAR_PREFETCHABLE: u32 = 1 << 3;
/// The low bits of a memory BAR that hold its type rather than its address.
const BAR_FLAGS_MASK: u64 = 0xf;

/// The identity in a function's header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
    pub(crate) vendor_id: u16,
    pub(crate) device_id: u16,
    pub(crate) revision_id: u8,
    /// Base class, subclass and programming interface, as 0xBBSSPP.
    pub(crate) class_code: u32,
    pub(crate) subsystem_vendor_id: u16,
    pub(crate) subsystem_id: u16,
}

/// A memory BAR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bar {
    /// A power of two, at least 16 bytes (4 KiB in practice: a BAR is
    /// mapped and trapped by the page).
    pub(crate) size: u64,
    /// A 64-bit BAR, which takes this BAR register and the next.
    pub(crate) is_64bit: bool,
    pub(crate) prefetchable: bool,
    /// The window the BAR must lie in to be decoded: outside it (while the
    /// guest sizes it, or if it moves it somewhere else) the BAR is off, so
    /// it can never cover RAM or another platform device.
    pub(crate) window: Range<u64>,
}

/// A type-0 configuration space.
pub(crate) struct ConfigSpace {
    regs: [u8; CONFIG_SIZE],
    writable: [u8; CONFIG_SIZE],
    bars: [Option<Bar>; 6],
    /// The last capability's offset, whose next pointer the next one sets.
    last_capability: Option<usize>,
    next_free: usize,
}

impl ConfigSpace {
    /// A function with no BARs and no capabilities yet, its memory decode
    /// off and no interrupt pin (interrupts are MSI-X only).
    pub(crate) fn new(header: Header) -> Self {
        let mut this = Self {
            regs: [0; CONFIG_SIZE],
            writable: [0; CONFIG_SIZE],
            bars: Default::default(),
            last_capability: None,
            next_free: FIRST_CAPABILITY,
        };
        this.set_u16(VENDOR_ID, header.vendor_id);
        this.set_u16(DEVICE_ID, header.device_id);
        this.regs[REVISION_ID] = header.revision_id;
        this.regs[CLASS_CODE..CLASS_CODE + 3]
            .copy_from_slice(&header.class_code.to_le_bytes()[..3]);
        this.regs[HEADER_TYPE] = 0;
        this.set_u16(SUBSYSTEM_VENDOR_ID, header.subsystem_vendor_id);
        this.set_u16(SUBSYSTEM_ID, header.subsystem_id);
        this.regs[INTERRUPT_PIN] = 0;
        this.writable[COMMAND..COMMAND + 2].copy_from_slice(&COMMAND_WRITABLE.to_le_bytes());
        // Registers software owns, which the function ignores.
        this.writable[CACHE_LINE_SIZE] = 0xff;
        this.writable[LATENCY_TIMER] = 0xff;
        this.writable[INTERRUPT_LINE] = 0xff;
        this
    }

    /// Adds memory BAR `index` at `addr` (which must be aligned to its size).
    pub(crate) fn add_bar(&mut self, index: usize, bar: Bar, addr: u64) {
        assert!(bar.size.is_power_of_two() && bar.size >= 16, "BAR size {:#x}", bar.size);
        assert!(addr.is_multiple_of(bar.size), "BAR at {addr:#x} is not aligned to its size");
        assert!(index < 6 && (!bar.is_64bit || index < 5), "BAR {index}");
        let flags = if bar.is_64bit { BAR_MEMORY_64 } else { 0 }
            | if bar.prefetchable { BAR_PREFETCHABLE } else { 0 };
        let address_mask = !(bar.size - 1) & !BAR_FLAGS_MASK;
        let off = BAR0 + 4 * index;
        self.set_u32(off, flags | addr as u32);
        self.writable[off..off + 4].copy_from_slice(&(address_mask as u32).to_le_bytes());
        if bar.is_64bit {
            self.set_u32(off + 4, (addr >> 32) as u32);
            self.writable[off + 4..off + 8]
                .copy_from_slice(&((address_mask >> 32) as u32).to_le_bytes());
        } else {
            assert!(addr + bar.size <= 1 << 32, "32-bit BAR above 4 GiB");
        }
        self.bars[index] = Some(bar);
    }

    /// Appends a capability and returns its offset. `body` starts with the
    /// capability ID; its next pointer (byte 1) is filled in. `writable` is
    /// the write mask of its bytes (empty: read-only).
    pub(crate) fn add_capability(&mut self, body: &[u8], writable: &[u8]) -> usize {
        assert!(body.len() >= 2 && (writable.is_empty() || writable.len() == body.len()));
        let off = self.next_free;
        assert!(off + body.len() <= CONFIG_SIZE, "capabilities overflow the configuration space");
        self.regs[off..off + body.len()].copy_from_slice(body);
        self.regs[off + 1] = 0;
        if !writable.is_empty() {
            self.writable[off..off + body.len()].copy_from_slice(writable);
        }
        match self.last_capability {
            Some(prev) => self.regs[prev + 1] = off as u8,
            None => {
                self.regs[CAPABILITIES_POINTER] = off as u8;
                let status = self.u16_at(STATUS) | STATUS_CAPABILITIES;
                self.set_u16(STATUS, status);
            }
        }
        self.last_capability = Some(off);
        self.next_free = (off + body.len()).next_multiple_of(4);
        off
    }

    /// Reads `data.len()` bytes at `offset`, as the guest sees them.
    pub(crate) fn read(&self, offset: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            *b = self.regs.get(offset + i).copied().unwrap_or(0);
        }
    }

    /// A guest write: only the writable bits change.
    pub(crate) fn write(&mut self, offset: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let o = offset + i;
            if o < CONFIG_SIZE {
                self.regs[o] = (self.regs[o] & !self.writable[o]) | (b & self.writable[o]);
            }
        }
    }

    /// The registers, for a snapshot.
    pub(crate) fn regs(&self) -> &[u8; CONFIG_SIZE] {
        &self.regs
    }

    /// Puts back registers [`regs`](Self::regs) gave, of a function built
    /// the same way: only bits the guest can write may differ from this
    /// one's, or the snapshot is of another device.
    pub(crate) fn restore_regs(&mut self, regs: &[u8]) -> crate::error::Result<()> {
        let fits = regs.len() == CONFIG_SIZE
            && regs
                .iter()
                .zip(self.regs.iter().zip(&self.writable))
                .all(|(&theirs, (&ours, &writable))| (theirs ^ ours) & !writable == 0);
        if !fits {
            return Err(crate::error::Error::Snapshot(format!(
                "the snapshot's PCI function {:04x}:{:04x} is not built as this one",
                self.u16_at(VENDOR_ID),
                self.u16_at(DEVICE_ID)
            )));
        }
        self.regs.copy_from_slice(regs);
        Ok(())
    }

    pub(crate) fn u16_at(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.regs[offset], self.regs[offset + 1]])
    }

    pub(crate) fn u32_at(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.regs[offset..offset + 4].try_into().unwrap())
    }

    /// Sets a register as the function (not the guest) does.
    pub(crate) fn set_u16(&mut self, offset: usize, value: u16) {
        self.regs[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn set_u32(&mut self, offset: usize, value: u32) {
        self.regs[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Turns memory decode on or off, as firmware does.
    pub(crate) fn set_memory_decode(&mut self, on: bool) {
        let command = self.u16_at(COMMAND);
        let command = if on { command | COMMAND_MEMORY } else { command & !COMMAND_MEMORY };
        self.set_u16(COMMAND, command);
    }

    /// The address BAR `index` holds now.
    pub(crate) fn bar_addr(&self, index: usize) -> u64 {
        let off = BAR0 + 4 * index;
        let low = u64::from(self.u32_at(off)) & !BAR_FLAGS_MASK;
        match &self.bars[index] {
            Some(bar) if bar.is_64bit => low | u64::from(self.u32_at(off + 4)) << 32,
            _ => low,
        }
    }

    /// Where each BAR is decoded now: `Some(address)` while memory decode is
    /// on and the BAR lies wholly inside its window.
    pub(crate) fn decoded_bars(&self) -> [Option<u64>; 6] {
        let decode = self.u16_at(COMMAND) & COMMAND_MEMORY != 0;
        std::array::from_fn(|i| {
            let bar = self.bars[i].as_ref()?;
            let addr = self.bar_addr(i);
            let inside = addr >= bar.window.start
                && addr.checked_add(bar.size).is_some_and(|end| end <= bar.window.end);
            (decode && inside).then_some(addr)
        })
    }

    #[cfg(test)]
    pub(crate) fn bar(&self, index: usize) -> Option<&Bar> {
        self.bars.get(index)?.as_ref()
    }
}

/// Whether `[a, a + a_len)` and `[b, b + b_len)` overlap.
pub(crate) fn overlaps(a: usize, a_len: usize, b: usize, b_len: usize) -> bool {
    a < b + b_len && b < a + a_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            vendor_id: 0x1af4,
            device_id: 0x1044,
            revision_id: 1,
            class_code: 0xff_00_00,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 4,
        }
    }

    fn read32(c: &ConfigSpace, off: usize) -> u32 {
        let mut b = [0; 4];
        c.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    fn write32(c: &mut ConfigSpace, off: usize, v: u32) {
        c.write(off, &v.to_le_bytes());
    }

    /// What a guest learns sizing a BAR: write all ones, read back.
    fn size_bar(c: &mut ConfigSpace, index: usize) -> (u64, u32) {
        let off = BAR0 + 4 * index;
        let orig = read32(c, off);
        let is_64 = orig & 0x6 == 0x4;
        write32(c, off, !0);
        let mut mask = u64::from(read32(c, off) & !0xf);
        if is_64 {
            let orig_hi = read32(c, off + 4);
            write32(c, off + 4, !0);
            mask |= u64::from(read32(c, off + 4)) << 32;
            write32(c, off + 4, orig_hi);
        } else {
            mask |= 0xffff_ffff_0000_0000;
        }
        write32(c, off, orig);
        ((!mask).wrapping_add(1), orig & 0xf)
    }

    #[test]
    fn header_reads_back_and_is_read_only() {
        let mut c = ConfigSpace::new(header());
        assert_eq!(read32(&c, 0), 0x1044_1af4);
        assert_eq!(read32(&c, 8), 0xff00_0001, "class code and revision");
        assert_eq!(read32(&c, SUBSYSTEM_VENDOR_ID), 0x0004_1af4);
        write32(&mut c, 0, 0xdead_beef);
        write32(&mut c, 8, 0);
        assert_eq!(read32(&c, 0), 0x1044_1af4);
        assert_eq!(read32(&c, 8), 0xff00_0001);
        assert_eq!(c.regs[INTERRUPT_PIN], 0, "no INTx");
        // Interrupt line is the OS's scratch register.
        c.write(INTERRUPT_LINE, &[0x0b]);
        assert_eq!(c.regs[INTERRUPT_LINE], 0x0b);
        // Past the standard space: zeroes, writes ignored.
        write32(&mut c, 0x100, !0);
        assert_eq!(read32(&c, 0x100), 0);
    }

    #[test]
    fn command_register_takes_only_its_writable_bits() {
        let mut c = ConfigSpace::new(header());
        c.write(COMMAND, &0xffffu16.to_le_bytes());
        assert_eq!(c.u16_at(COMMAND), COMMAND_WRITABLE);
        assert_eq!(c.u16_at(COMMAND) & 1, 0, "no I/O decode");
    }

    #[test]
    fn bars_size_themselves_and_keep_their_flags() {
        let mut c = ConfigSpace::new(header());
        c.add_bar(
            0,
            Bar {
                size: 0x1_0000,
                is_64bit: false,
                prefetchable: false,
                window: 0xc000_0000..0xe000_0000,
            },
            0xc001_0000,
        );
        c.add_bar(
            2,
            Bar { size: 1 << 30, is_64bit: true, prefetchable: true, window: 1 << 40..1 << 41 },
            (1 << 40) + (1 << 30),
        );
        assert_eq!(c.bar_addr(0), 0xc001_0000);
        assert_eq!(c.bar_addr(2), (1 << 40) + (1 << 30));
        assert_eq!(size_bar(&mut c, 0), (0x1_0000, 0));
        assert_eq!(size_bar(&mut c, 2), (1 << 30, 0b1100), "64-bit, prefetchable");
        // Sizing restored the addresses.
        assert_eq!(c.bar_addr(0), 0xc001_0000);
        assert_eq!(c.bar_addr(2), (1 << 40) + (1 << 30));
        // Unused BARs read as zero and stay so.
        write32(&mut c, BAR0 + 4, !0);
        assert_eq!(read32(&c, BAR0 + 4), 0);
        write32(&mut c, BAR0 + 16, !0);
        assert_eq!(read32(&c, BAR0 + 16), 0);
    }

    #[test]
    fn a_bar_decodes_only_with_memory_on_and_inside_its_window() {
        let mut c = ConfigSpace::new(header());
        let window = 0xc000_0000..0xe000_0000;
        c.add_bar(
            0,
            Bar { size: 0x1000, is_64bit: false, prefetchable: false, window },
            0xc000_0000,
        );
        assert_eq!(c.decoded_bars()[0], None, "decode starts off");
        c.set_memory_decode(true);
        assert_eq!(c.decoded_bars()[0], Some(0xc000_0000));
        // Mid-sizing, the BAR holds its size mask: outside the window, off.
        write32(&mut c, BAR0, !0);
        assert_eq!(c.decoded_bars()[0], None);
        // Moved by the guest within the window: decoded at the new address.
        write32(&mut c, BAR0, 0xd000_0000);
        assert_eq!(c.decoded_bars()[0], Some(0xd000_0000));
        // Over the window's end: off.
        write32(&mut c, BAR0, 0xdfff_f000);
        assert_eq!(c.decoded_bars()[0], Some(0xdfff_f000));
        write32(&mut c, BAR0, 0xe000_0000);
        assert_eq!(c.decoded_bars()[0], None);
        write32(&mut c, BAR0, 0xd000_0000);
        c.write(COMMAND, &0u16.to_le_bytes());
        assert_eq!(c.decoded_bars()[0], None, "the guest turned decode off");
    }

    #[test]
    fn capabilities_chain_from_the_pointer() {
        let mut c = ConfigSpace::new(header());
        assert_eq!(c.u16_at(STATUS) & STATUS_CAPABILITIES, 0);
        let a = c.add_capability(&[0x09, 0xee, 5, 1, 2], &[]);
        let b = c.add_capability(&[0x11, 0, 0, 0], &[0, 0, 0xff, 0]);
        assert_eq!(c.u16_at(STATUS) & STATUS_CAPABILITIES, STATUS_CAPABILITIES);
        assert_eq!(c.regs[CAPABILITIES_POINTER] as usize, a);
        assert_eq!(a, 0x40);
        assert_eq!(b, 0x48, "dword-aligned after the first");
        assert_eq!(c.regs[a + 1] as usize, b, "the builder sets next pointers");
        assert_eq!(c.regs[b + 1], 0, "the last ends the list");
        // Writable bytes as masked, the rest read-only.
        c.write(b, &[0xff, 0xff, 0x12, 0xff]);
        let mut got = [0; 4];
        c.read(b, &mut got);
        assert_eq!(got, [0x11, 0, 0x12, 0]);
    }

    #[test]
    fn overlap() {
        assert!(overlaps(0, 4, 2, 2));
        assert!(overlaps(2, 2, 0, 4));
        assert!(!overlaps(0, 2, 2, 2));
        assert!(!overlaps(4, 4, 0, 4));
    }
}
