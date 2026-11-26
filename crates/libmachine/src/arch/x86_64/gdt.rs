// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Segment descriptors for the boot GDT, and the segment state they load.

/// Descriptor flags (access byte and the high nibble's G/DB/L/AVL bits).
pub(crate) const CODE64: u16 = 0xa09b;
pub(crate) const CODE32: u16 = 0xc09b;
pub(crate) const DATA: u16 = 0xc093;
pub(crate) const TSS: u16 = 0x008b;

/// A segment as the CPU caches it once loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) selector: u16,
    pub(crate) base: u64,
    /// Byte-granular limit.
    pub(crate) limit: u32,
    pub(crate) type_: u8,
    pub(crate) present: bool,
    pub(crate) dpl: u8,
    pub(crate) db: bool,
    pub(crate) s: bool,
    pub(crate) l: bool,
    pub(crate) g: bool,
    pub(crate) avl: bool,
}

/// The 8-byte GDT entry for a segment with `flags`, `base` and `limit`
/// (in units of 4 KiB when the G flag is set).
pub(crate) fn entry(flags: u16, base: u32, limit: u32) -> u64 {
    let (flags, base, limit) = (u64::from(flags), u64::from(base), u64::from(limit));
    ((base & 0xff00_0000) << 32)
        | ((flags & 0xf0ff) << 40)
        | ((limit & 0x000f_0000) << 32)
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0x0000_ffff)
}

/// The segment state loading the GDT entry at `index` produces.
pub(crate) fn segment(index: u16, entry: u64) -> Segment {
    let flags = (entry >> 40) as u16 & 0xf0ff;
    let base = ((entry >> 16) & 0x00ff_ffff) | ((entry >> 32) & 0xff00_0000);
    let raw_limit = ((entry & 0xffff) | ((entry >> 32) & 0x000f_0000)) as u32;
    let g = flags & (1 << 15) != 0;
    Segment {
        selector: index * 8,
        base,
        limit: if g { (raw_limit << 12) | 0xfff } else { raw_limit },
        type_: (flags & 0xf) as u8,
        s: flags & (1 << 4) != 0,
        dpl: ((flags >> 5) & 3) as u8,
        present: flags & (1 << 7) != 0,
        avl: flags & (1 << 12) != 0,
        l: flags & (1 << 13) != 0,
        db: flags & (1 << 14) != 0,
        g,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_match_the_architectural_encodings() {
        // The flat descriptors every x86 boot loader uses.
        assert_eq!(entry(CODE64, 0, 0xfffff), 0x00af_9b00_0000_ffff);
        assert_eq!(entry(CODE32, 0, 0xfffff), 0x00cf_9b00_0000_ffff);
        assert_eq!(entry(DATA, 0, 0xfffff), 0x00cf_9300_0000_ffff);
        assert_eq!(entry(TSS, 0x1234_5678, 0x67), 0x1200_8b34_5678_0067);
    }

    #[test]
    fn segments_decode_their_entries() {
        let code = segment(1, entry(CODE64, 0, 0xfffff));
        assert_eq!((code.selector, code.base, code.limit), (8, 0, 0xffff_ffff));
        assert_eq!((code.type_, code.l, code.db, code.g), (0xb, true, false, true));
        let tss = segment(3, entry(TSS, 0x1234_5678, 0x67));
        assert_eq!((tss.base, tss.limit, tss.g, tss.s), (0x1234_5678, 0x67, false, false));
    }
}
