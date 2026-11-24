// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use cipher::array::ArraySize;
use cipher::inout::InOut;
use cipher::typenum::consts::U16;
use cipher::{
    Array, BlockCipherDecBackend, BlockCipherDecClosure, BlockCipherEncBackend,
    BlockCipherEncClosure, BlockSizeUser,
};
use static_assertions::assert_cfg;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// This assumes little-endianness which is likely to always be the case.
assert_cfg!(target_endian = "little");

#[derive(IntoBytes, KnownLayout, FromBytes, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Tweak(pub u128);

impl Tweak {
    pub fn new(val: u128) -> Self {
        Self(val)
    }

    fn update(&mut self) {
        self.0 = (self.0 << 1) ^ ((self.0 as i128 >> 127) as u128 & 0x87);
    }
}

#[cfg(target_arch = "aarch64")]
fn assert_dcz_block_size_is_64() {
    static CHECK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CHECK.get_or_init(|| {
        let dczid: u64;
        unsafe {
            core::arch::asm!("mrs {0}, dczid_el0", out(reg) dczid);
        }
        let dzp = (dczid >> 4) & 1;
        assert_eq!(dzp, 0, "DC ZVA is prohibited on this CPU");
        let bs_bytes = (1usize << (dczid & 0xf)) * 4;
        assert_eq!(bs_bytes, 64, "Expected DC ZVA block size to be 64 bytes, found {bs_bytes}");
    });
}

/// To be used with encrypt|decrypt_with_backend for out-of-place operation.
/// Conforms to IEEE 1619-2007.
pub struct XtsProcessor<'a, 'b> {
    tweak: Tweak,
    src: &'a [u8],
    dst: &'b mut [u8],
}

/// To be used with encrypt|decrypt_with_backend for in-place operation.
/// Conforms to IEEE 1619-2007.
pub struct XtsInPlaceProcessor<'a> {
    tweak: Tweak,
    buf: &'a mut [u8],
}

fn xts_encrypt_chunk<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    mut val: u128,
    tweak: &Tweak,
) -> u128 {
    // XOR plaintext with tweak.
    val ^= tweak.0;

    let arr: &mut Array<u8, U16> = val.as_mut_bytes().try_into().unwrap();
    backend.encrypt_block(InOut::from(arr));

    // XOR ciphertext with tweak.
    val ^ tweak.0
}

fn xts_decrypt_chunk<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    mut val: u128,
    tweak: &Tweak,
) -> u128 {
    // XOR ciphertext with tweak.
    val ^= tweak.0;

    let arr: &mut Array<u8, U16> = val.as_mut_bytes().try_into().unwrap();
    backend.decrypt_block(InOut::from(arr));

    // XOR plaintext with tweak.
    val ^ tweak.0
}

/// cladefs: runs `src`'s blocks through a cipher into `dst`'s, XTS: each XORed with its tweak
/// before and after, `tweak` the first's and left the one after the last's. `N` blocks at a
/// time go through `par`, the backend's parallel blocks, and those left over one at a time through
/// `one`. Upstream ran every block through the backend's `encrypt_block`, its AES rounds
/// latency-bound: 0.78 GB/s on a Ryzen 9 8945HS whose VAES-512 backend takes 64 blocks at once,
/// 72% of a store's write of 256 KiB (TASK-512). `zero_lines` has an out-of-place `dst`'s 64-byte
/// lines zeroed ("DC ZVA") before their first block is written, on ARM64, as upstream did, so
/// that they are not fetched from RAM only to be overwritten. In place, `src` and `dst` alias:
/// each group of blocks is read whole before any of it is written.
/// SAFETY: `src` must be valid for reads of `len` bytes and 16-byte aligned; `dst` must be valid
/// for writes of `len` bytes and 16-byte aligned; `src` and `dst` may either be disjoint or exact
/// aliases (`src == dst`), and `len` must be a multiple of 16.
unsafe fn xts_buffer<N: ArraySize>(
    src: *const u8,
    dst: *mut u8,
    len: usize,
    tweak: &mut Tweak,
    zero_lines: bool,
    par: impl Fn(&mut Array<Array<u8, U16>, N>),
    one: impl Fn(u128, &Tweak) -> u128,
) {
    debug_assert_eq!(len % size_of::<u128>(), 0);
    debug_assert!(src.cast::<u128>().is_aligned());
    debug_assert!(dst.cast::<u128>().is_aligned());
    #[cfg(not(target_arch = "aarch64"))]
    let _ = zero_lines;
    let zero_line = |block: *mut u128| {
        #[cfg(target_arch = "aarch64")]
        if zero_lines && (block as usize).is_multiple_of(64) {
            // SAFETY: the line is dst's, 64-byte aligned, and all of it is written after.
            unsafe {
                core::arch::asm!("dc zva, {0}", in(reg) block, options(nostack, preserves_flags));
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = block;
    };

    let total_blocks = len / size_of::<u128>();
    let groups = total_blocks / N::USIZE;
    let src_u128 = src.cast::<u128>();
    let dst_u128 = dst.cast::<u128>();
    let mut blocks = Array::<Array<u8, U16>, N>::default();
    let mut tweaks = Array::<u128, N>::default();
    let mut idx = 0usize;
    for _ in 0..groups {
        for (k, (block, block_tweak)) in blocks.iter_mut().zip(tweaks.iter_mut()).enumerate() {
            // SAFETY: `idx + k < total_blocks`, and `src_u128` is valid and 16-byte aligned.
            let val = unsafe { src_u128.add(idx + k).read() };
            *block_tweak = tweak.0;
            *block = Array((val ^ tweak.0).to_le_bytes());
            tweak.update();
        }
        par(&mut blocks);
        for (k, (block, block_tweak)) in blocks.iter().zip(tweaks.iter()).enumerate() {
            // SAFETY: `idx + k < total_blocks`, and `dst_u128` is valid and 16-byte aligned.
            unsafe {
                let out = dst_u128.add(idx + k);
                zero_line(out);
                out.write(u128::from_le_bytes(block.0) ^ block_tweak);
            }
        }
        idx += N::USIZE;
    }
    while idx < total_blocks {
        // SAFETY: `idx < total_blocks`, and pointers are valid and 16-byte aligned.
        unsafe {
            let val = src_u128.add(idx).read();
            let out = dst_u128.add(idx);
            zero_line(out);
            out.write(one(val, tweak));
        }
        tweak.update();
        idx += 1;
    }
}

unsafe fn xts_encrypt_buffer<B: BlockCipherEncBackend<BlockSize = U16>>(
    backend: &B,
    src: *const u8,
    dst: *mut u8,
    len: usize,
    tweak: &mut Tweak,
    zero_lines: bool,
) {
    // SAFETY: forwarded from caller.
    unsafe {
        xts_buffer::<B::ParBlocksSize>(
            src,
            dst,
            len,
            tweak,
            zero_lines,
            |blocks| backend.encrypt_par_blocks(InOut::from(blocks)),
            |val, tweak| xts_encrypt_chunk(backend, val, tweak),
        );
    }
}

unsafe fn xts_decrypt_buffer<B: BlockCipherDecBackend<BlockSize = U16>>(
    backend: &B,
    src: *const u8,
    dst: *mut u8,
    len: usize,
    tweak: &mut Tweak,
    zero_lines: bool,
) {
    // SAFETY: forwarded from caller.
    unsafe {
        xts_buffer::<B::ParBlocksSize>(
            src,
            dst,
            len,
            tweak,
            zero_lines,
            |blocks| backend.decrypt_par_blocks(InOut::from(blocks)),
            |val, tweak| xts_decrypt_chunk(backend, val, tweak),
        );
    }
}

impl<'a, 'b> XtsProcessor<'a, 'b> {
    /// `tweak` should be encrypted. `src` and `dst` must have the same length, be 64-byte
    /// aligned, and length must be a multiple of 64 bytes.
    pub fn new(tweak: Tweak, src: &'a [u8], dst: &'b mut [u8]) -> Self {
        assert_eq!(src.len(), dst.len(), "Source and destination lengths must match");
        assert_eq!(src.len() % 64, 0, "length must be a multiple of 64 bytes");
        assert_eq!(src.as_ptr() as usize % 64, 0, "src must be 64-byte aligned");
        assert_eq!(dst.as_ptr() as usize % 64, 0, "dst must be 64-byte aligned");
        #[cfg(target_arch = "aarch64")]
        assert_dcz_block_size_is_64();
        Self { tweak, src, dst }
    }
}

impl BlockSizeUser for XtsProcessor<'_, '_> {
    type BlockSize = U16;
}

impl BlockCipherEncClosure for XtsProcessor<'_, '_> {
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, dst } = self;
        // SAFETY: `src` and `dst` are valid, 64-byte aligned slices of equal length % 64 == 0.
        unsafe {
            xts_encrypt_buffer(backend, src.as_ptr(), dst.as_mut_ptr(), src.len(), &mut tweak, true);
        }
    }
}

impl BlockCipherDecClosure for XtsProcessor<'_, '_> {
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, src, dst } = self;
        // SAFETY: `src` and `dst` are valid, 64-byte aligned slices of equal length % 64 == 0.
        unsafe {
            xts_decrypt_buffer(backend, src.as_ptr(), dst.as_mut_ptr(), src.len(), &mut tweak, true);
        }
    }
}

impl<'a> XtsInPlaceProcessor<'a> {
    /// Creates an XtsInPlaceProcessor for in-place operation on a single buffer.
    pub fn new(tweak: Tweak, buf: &'a mut [u8]) -> Self {
        assert!(buf.as_ptr().cast::<u128>().is_aligned(), "buf must be 16 byte aligned");
        assert_eq!(buf.len() % size_of::<u128>(), 0, "buf length must be a multiple of 16 bytes");
        Self { tweak, buf }
    }
}

impl BlockSizeUser for XtsInPlaceProcessor<'_> {
    type BlockSize = U16;
}

impl BlockCipherEncClosure for XtsInPlaceProcessor<'_> {
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, buf } = self;
        let len = buf.len();
        let ptr = buf.as_mut_ptr();
        // SAFETY: `ptr` is valid for `len` bytes, 16-byte aligned, and `len % 16 == 0`.
        unsafe {
            xts_encrypt_buffer(backend, ptr.cast_const(), ptr, len, &mut tweak, false);
        }
    }
}

impl BlockCipherDecClosure for XtsInPlaceProcessor<'_> {
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, buf } = self;
        let len = buf.len();
        let ptr = buf.as_mut_ptr();
        // SAFETY: `ptr` is valid for `len` bytes, 16-byte aligned, and `len % 16 == 0`.
        unsafe {
            xts_decrypt_buffer(backend, ptr.cast_const(), ptr, len, &mut tweak, false);
        }
    }
}

enum CtsBuf<'a, 'b> {
    Separate { src: &'a [u8], dst: &'b mut [u8] },
    InPlace(&'a mut [u8]),
}

impl CtsBuf<'_, '_> {
    fn len(&self) -> usize {
        match self {
            Self::Separate { src, .. } => src.len(),
            Self::InPlace(buf) => buf.len(),
        }
    }

    fn ptrs(&mut self) -> (*const u8, *mut u8, usize) {
        match self {
            Self::Separate { src, dst } => (src.as_ptr(), dst.as_mut_ptr(), src.len()),
            Self::InPlace(buf) => {
                let len = buf.len();
                let ptr = buf.as_mut_ptr();
                (ptr.cast_const(), ptr, len)
            }
        }
    }
}

/// Handles ciphertext stealing in order to allow non-BlockSize lengths to be used as long as
/// they are >= BlockSize. To be used with encrypt|decrypt_with_backend for out-of-place and
/// in-place operation. Conforms to IEEE 1619-2007.
pub struct XtsCtsProcessor<'a, 'b> {
    tweak: Tweak,
    buf: CtsBuf<'a, 'b>,
}

impl BlockSizeUser for XtsCtsProcessor<'_, '_> {
    type BlockSize = U16;
}

impl<'a, 'b> XtsCtsProcessor<'a, 'b> {
    /// `tweak` should be encrypted. `src` and `dst` must have the same length and be 16 byte
    /// aligned.
    pub fn new(tweak: Tweak, src: &'a [u8], dst: &'b mut [u8]) -> Self {
        assert_eq!(src.len(), dst.len(), "Source and destination lengths must match");
        assert!(src.len() >= size_of::<u128>());
        assert!(src.as_ptr().cast::<u128>().is_aligned(), "src must be 16 byte aligned");
        assert!(dst.as_ptr().cast::<u128>().is_aligned(), "dst must be 16 byte aligned");
        Self { tweak, buf: CtsBuf::Separate { src, dst } }
    }

    /// Creates an XtsCtsProcessor for in-place operation on a single buffer.
    pub fn new_in_place(tweak: Tweak, buf: &'a mut [u8]) -> XtsCtsProcessor<'a, 'a> {
        assert!(buf.len() >= size_of::<u128>());
        assert!(buf.as_ptr().cast::<u128>().is_aligned(), "buf must be 16 byte aligned");
        XtsCtsProcessor { tweak, buf: CtsBuf::InPlace(buf) }
    }
}

impl BlockCipherEncClosure for XtsCtsProcessor<'_, '_> {
    fn call<B: BlockCipherEncBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, mut buf } = self;
        let len = buf.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            let (src, dst, len) = buf.ptrs();
            // SAFETY: validated in constructor (`len % 16 == 0`, 16-byte aligned).
            unsafe { xts_encrypt_buffer(backend, src, dst, len, &mut tweak, false) };
            return;
        }

        // All but the last two blocks are normal.
        if base_size > 16 {
            let (src, dst, _) = buf.ptrs();
            // SAFETY: `base_size - 16` is a multiple of 16 and <= `len`, pointers 16-byte aligned.
            unsafe { xts_encrypt_buffer(backend, src, dst, base_size - 16, &mut tweak, false) };
        }

        let last_full_block_range = (base_size - 16)..base_size;
        let extra = len & 0x0F;

        let (src_last, src_tail): ([u8; 16], [u8; 16]) = {
            let src_slice: &[u8] = match &buf {
                CtsBuf::Separate { src, .. } => src,
                CtsBuf::InPlace(b) => b,
            };
            let last: [u8; 16] =
                src_slice[last_full_block_range.clone()].try_into().expect("Size validated above");
            let mut tail = [0u8; 16];
            tail[..extra].copy_from_slice(&src_slice[base_size..len]);
            (last, tail)
        };

        let last_full_block = u128::from_le_bytes(src_last);
        let intermediate_ciphertext = xts_encrypt_chunk(backend, last_full_block, &tweak);
        let intermediate_cipher_bytes = intermediate_ciphertext.to_le_bytes();

        let mut combined_block_bytes = intermediate_cipher_bytes;
        combined_block_bytes[0..extra].copy_from_slice(&src_tail[..extra]);

        tweak.update();

        let combined_block = u128::from_le_bytes(combined_block_bytes);
        let full_ciphertext = xts_encrypt_chunk(backend, combined_block, &tweak);

        let dst_slice: &mut [u8] = match &mut buf {
            CtsBuf::Separate { dst, .. } => dst,
            CtsBuf::InPlace(b) => b,
        };
        dst_slice[base_size..len].copy_from_slice(&intermediate_cipher_bytes[0..extra]);
        dst_slice[last_full_block_range].copy_from_slice(&full_ciphertext.to_le_bytes());
    }
}

impl BlockCipherDecClosure for XtsCtsProcessor<'_, '_> {
    fn call<B: BlockCipherDecBackend<BlockSize = Self::BlockSize>>(self, backend: &B) {
        let Self { mut tweak, mut buf } = self;
        let len = buf.len();
        let base_size = len & (!0x0F);

        // Fast path for non-CTS (len is an exact multiple of 16):
        if len == base_size {
            let (src, dst, len) = buf.ptrs();
            // SAFETY: validated in constructor (`len % 16 == 0`, 16-byte aligned).
            unsafe { xts_decrypt_buffer(backend, src, dst, len, &mut tweak, false) };
            return;
        }

        // All but the last two blocks are normal.
        if base_size > 16 {
            let (src, dst, _) = buf.ptrs();
            // SAFETY: `base_size - 16` is a multiple of 16 and <= `len`, pointers 16-byte aligned.
            unsafe { xts_decrypt_buffer(backend, src, dst, base_size - 16, &mut tweak, false) };
        }

        let last_full_block_tweak = tweak;
        let last_full_block_range = (base_size - 16)..base_size;

        tweak.update();
        let extra = len & 0x0F;

        let (src_last, src_tail): ([u8; 16], [u8; 16]) = {
            let src_slice: &[u8] = match &buf {
                CtsBuf::Separate { src, .. } => src,
                CtsBuf::InPlace(b) => b,
            };
            let last: [u8; 16] =
                src_slice[last_full_block_range.clone()].try_into().expect("Size validated above");
            let mut tail = [0u8; 16];
            tail[..extra].copy_from_slice(&src_slice[base_size..len]);
            (last, tail)
        };

        let full_ciphertext = u128::from_le_bytes(src_last);
        let decrypted_combined_block = xts_decrypt_chunk(backend, full_ciphertext, &tweak);
        let combined_bytes = decrypted_combined_block.to_le_bytes();

        let mut intermediate_bytes = combined_bytes;
        intermediate_bytes[0..extra].copy_from_slice(&src_tail[..extra]);

        let last_block_ciphertext = u128::from_le_bytes(intermediate_bytes);
        let val = xts_decrypt_chunk(backend, last_block_ciphertext, &last_full_block_tweak);

        let dst_slice: &mut [u8] = match &mut buf {
            CtsBuf::Separate { dst, .. } => dst,
            CtsBuf::InPlace(b) => b,
        };
        dst_slice[base_size..len].copy_from_slice(&combined_bytes[0..extra]);
        dst_slice[last_full_block_range].copy_from_slice(&val.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher::inout::InOut;
    use cipher::typenum::consts::{U1, U3};
    use cipher::{Block, ParBlocksSizeUser};
    use std::cell::RefCell;
    use test_case::test_case;
    use zerocopy::{FromBytes, Immutable, IntoBytes};

    struct MockCipher {
        recorded_blocks: RefCell<Vec<u128>>,
        key: u128,
    }

    impl MockCipher {
        fn new(key: u128) -> Self {
            Self { recorded_blocks: RefCell::new(Vec::new()), key }
        }
    }

    impl BlockSizeUser for MockCipher {
        type BlockSize = U16;
    }

    impl ParBlocksSizeUser for MockCipher {
        type ParBlocksSize = U1;
    }

    impl BlockCipherEncBackend for MockCipher {
        fn encrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            self.recorded_blocks.borrow_mut().push(val);
            val ^= self.key;
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    impl BlockCipherDecBackend for MockCipher {
        fn decrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            self.recorded_blocks.borrow_mut().push(val);
            // Lazy scramble. See above for the encryption method.
            val = val.rotate_right(3);
            val ^= self.key;
            // SAFETY: Block<Self> is Array<u8, U16>, which is 16 bytes.
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    /// A simple fake scrambler that includes a rotate left to keep the XOR from canceling itself
    /// out.
    struct MockNonLinearCipher {
        key: u128,
    }

    impl MockNonLinearCipher {
        fn new(key: u128) -> Self {
            Self { key }
        }
    }

    impl BlockSizeUser for MockNonLinearCipher {
        type BlockSize = U16;
    }

    impl ParBlocksSizeUser for MockNonLinearCipher {
        type ParBlocksSize = U1;
    }

    impl BlockCipherEncBackend for MockNonLinearCipher {
        fn encrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            val = val.rotate_left(11) ^ self.key;
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    impl BlockCipherDecBackend for MockNonLinearCipher {
        fn decrypt_block(&self, mut block: InOut<'_, '_, Block<Self>>) {
            let mut val =
                unsafe { std::ptr::read_unaligned(block.get_in().as_ptr() as *const u128) };
            val = (val ^ self.key).rotate_right(11);
            unsafe {
                std::ptr::write_unaligned(block.get_out().as_mut_ptr() as *mut u128, val);
            }
        }
    }

    /// cladefs: [`MockNonLinearCipher`]'s scramble, `N` blocks at a time as well, counting the
    /// times it was asked for them so (TASK-512).
    struct MockParCipher<N> {
        key: u128,
        par_calls: std::cell::Cell<usize>,
        size: std::marker::PhantomData<N>,
    }

    impl<N> MockParCipher<N> {
        fn new(key: u128) -> Self {
            Self { key, par_calls: Default::default(), size: std::marker::PhantomData }
        }
    }

    impl<N> BlockSizeUser for MockParCipher<N> {
        type BlockSize = U16;
    }

    impl<N: ArraySize> ParBlocksSizeUser for MockParCipher<N> {
        type ParBlocksSize = N;
    }

    impl<N: ArraySize> BlockCipherEncBackend for MockParCipher<N> {
        fn encrypt_block(&self, block: InOut<'_, '_, Block<Self>>) {
            MockNonLinearCipher::new(self.key).encrypt_block(block);
        }

        fn encrypt_par_blocks(&self, mut blocks: InOut<'_, '_, cipher::ParBlocks<Self>>) {
            self.par_calls.set(self.par_calls.get() + 1);
            for i in 0..N::USIZE {
                self.encrypt_block(blocks.get(i));
            }
        }
    }

    impl<N: ArraySize> BlockCipherDecBackend for MockParCipher<N> {
        fn decrypt_block(&self, block: InOut<'_, '_, Block<Self>>) {
            MockNonLinearCipher::new(self.key).decrypt_block(block);
        }

        fn decrypt_par_blocks(&self, mut blocks: InOut<'_, '_, cipher::ParBlocks<Self>>) {
            self.par_calls.set(self.par_calls.get() + 1);
            for i in 0..N::USIZE {
                self.decrypt_block(blocks.get(i));
            }
        }
    }

    /// cladefs: blocks `count` blocks long, 64-byte aligned, each byte its own.
    fn numbered(count: usize) -> Vec<Aligned64<Blocks<4>>> {
        let mut blocks: Vec<Aligned64<Blocks<4>>> =
            (0..count.div_ceil(4)).map(|_| Aligned64(Default::default())).collect();
        for (at, group) in blocks.iter_mut().enumerate() {
            for (i, byte) in group.0.as_mut_bytes().iter_mut().enumerate() {
                *byte = (at * 64 + i).wrapping_mul(7) as u8;
            }
        }
        blocks
    }

    fn bytes(blocks: &[Aligned64<Blocks<4>>], count: usize) -> &[u8] {
        // SAFETY: Aligned64<Blocks<4>> is 64 bytes of u128s, no padding.
        unsafe { std::slice::from_raw_parts(blocks.as_ptr().cast::<u8>(), count * 16) }
    }

    fn bytes_mut(blocks: &mut [Aligned64<Blocks<4>>], count: usize) -> &mut [u8] {
        // SAFETY: as `bytes`.
        unsafe { std::slice::from_raw_parts_mut(blocks.as_mut_ptr().cast::<u8>(), count * 16) }
    }

    /// cladefs: blocks `count` blocks long encrypted in place, XTS, under `cipher`.
    fn encrypted_in_place<C>(cipher: &C, tweak: Tweak, count: usize) -> Vec<Aligned64<Blocks<4>>>
    where
        C: BlockCipherEncBackend<BlockSize = U16>,
    {
        let mut blocks = numbered(count);
        let processor = XtsInPlaceProcessor::new(tweak, bytes_mut(&mut blocks, count).into());
        BlockCipherEncClosure::call(processor, cipher);
        blocks
    }

    const PAR_TWEAK: u128 = 0x0f1e2d3c4b5a69788796a5b4c3d2e1f0;
    const PAR_KEY: u128 = 0x00112233445566778899aabbccddeeff;

    /// cladefs: a cipher taking blocks three at a time encrypts and decrypts in place as one
    /// taking them one at a time does, the blocks left over one at a time, and takes every whole
    /// three at once (TASK-512: upstream took each alone, AES latency-bound).
    #[test_case(1; "one_block")]
    #[test_case(3; "one_group")]
    #[test_case(4; "a_group_and_one")]
    #[test_case(8; "two_groups_and_two")]
    #[test_case(256; "a_sector")]
    fn test_parallel_blocks_in_place_as_single_ones(count: usize) {
        let tweak = Tweak::new(PAR_TWEAK);
        let expected = encrypted_in_place(&MockNonLinearCipher::new(PAR_KEY), tweak, count);

        let par = MockParCipher::<U3>::new(PAR_KEY);
        let mut blocks = encrypted_in_place(&par, tweak, count);
        assert_eq!(bytes(&blocks, count), bytes(&expected, count));
        assert_eq!(par.par_calls.get(), count / 3);

        let processor = XtsInPlaceProcessor::new(tweak, bytes_mut(&mut blocks, count).into());
        BlockCipherDecClosure::call(processor, &par);
        assert_eq!(bytes(&blocks, count), bytes(&numbered(count), count));
        assert_eq!(par.par_calls.get(), 2 * (count / 3));
    }

    /// cladefs: so out of place, whole 64-byte lines of blocks (TASK-512).
    #[test_case(4; "a_group_and_one")]
    #[test_case(12; "four_groups")]
    #[test_case(256; "a_sector")]
    fn test_parallel_blocks_out_of_place_as_single_ones(count: usize) {
        let tweak = Tweak::new(PAR_TWEAK);
        let expected = encrypted_in_place(&MockNonLinearCipher::new(PAR_KEY), tweak, count);
        let plain = numbered(count);

        let par = MockParCipher::<U3>::new(PAR_KEY);
        let mut encrypted = numbered(count);
        let processor = XtsProcessor::new(
            tweak,
            bytes(&plain, count).into(),
            bytes_mut(&mut encrypted, count).into(),
        );
        BlockCipherEncClosure::call(processor, &par);
        assert_eq!(bytes(&encrypted, count), bytes(&expected, count));
        assert_eq!(par.par_calls.get(), count / 3);

        let mut decrypted: Vec<Aligned64<Blocks<4>>> =
            (0..count.div_ceil(4)).map(|_| Aligned64(Blocks::default())).collect();
        let processor = XtsProcessor::new(
            tweak,
            bytes(&encrypted, count).into(),
            bytes_mut(&mut decrypted, count).into(),
        );
        BlockCipherDecClosure::call(processor, &par);
        assert_eq!(bytes(&decrypted, count), bytes(&plain, count));
        assert_eq!(par.par_calls.get(), 2 * (count / 3));
    }

    #[repr(C)]
    #[derive(FromBytes, IntoBytes, Immutable)]
    struct Blocks<const N: usize>([u128; N]);

    #[repr(C, align(64))]
    struct Aligned64<T>(T);

    static_assertions::const_assert!(std::mem::align_of::<Aligned64<Blocks<1>>>() == 64);

    impl<const N: usize> Default for Blocks<N> {
        fn default() -> Self {
            Self([0u128; N])
        }
    }

    #[test]
    fn test_xts_out_of_place() {
        let mut plaintext: Aligned64<Blocks<4>> = Aligned64(Default::default());
        for (i, x) in plaintext.0.as_mut_bytes().iter_mut().enumerate() {
            *x = i as u8;
        }
        let mut ciphertext: Aligned64<Blocks<4>> = Aligned64(Default::default());

        let src = plaintext.0.as_bytes();
        let dst = ciphertext.0.as_mut_bytes();

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        let processor = XtsProcessor::new(tweak, src, dst);
        let cipher = MockCipher::new(key);

        BlockCipherEncClosure::call(processor, &cipher);

        // Verify ciphertext.
        // Since our mock cipher is just XOR with key, the tweak should cancel out.
        // C = P ^ K.
        let expected_c0 =
            u128::from_le_bytes(plaintext.0.as_bytes()[0..16].try_into().unwrap()) ^ key;
        let expected_c1 =
            u128::from_le_bytes(plaintext.0.as_bytes()[16..32].try_into().unwrap()) ^ key;

        let actual_c0 = u128::from_le_bytes(ciphertext.0.as_bytes()[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(ciphertext.0.as_bytes()[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks (should be P ^ T).
        assert_eq!(cipher.recorded_blocks.borrow().len(), 4);

        let p0 = u128::from_le_bytes(plaintext.0.as_bytes()[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(plaintext.0.as_bytes()[16..32].try_into().unwrap());

        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks.borrow()[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks.borrow()[1], p1 ^ t0.0);
    }

    #[test]
    fn test_xts_in_place() {
        let mut buf: Blocks<2> = Default::default();
        for (i, x) in buf.as_mut_bytes().iter_mut().enumerate() {
            *x = i as u8;
        }

        let tweak_val = 0x123456789abcdef0123456789abcdef0u128;
        let tweak = Tweak::new(tweak_val);
        let key = 0xffeeddccbbaa99887766554433221100u128;

        // Save original plaintext for verification.
        let p0 = u128::from_le_bytes(buf.as_bytes()[0..16].try_into().unwrap());
        let p1 = u128::from_le_bytes(buf.as_bytes()[16..32].try_into().unwrap());

        let slice = buf.as_mut_bytes();
        let processor = XtsInPlaceProcessor::new(tweak, slice);
        let cipher = MockCipher::new(key);

        BlockCipherEncClosure::call(processor, &cipher);

        // Verify in-place ciphertext.
        let expected_c0 = p0 ^ key;
        let expected_c1 = p1 ^ key;

        let actual_c0 = u128::from_le_bytes(buf.as_bytes()[0..16].try_into().unwrap());
        let actual_c1 = u128::from_le_bytes(buf.as_bytes()[16..32].try_into().unwrap());

        assert_eq!(actual_c0, expected_c0);
        assert_eq!(actual_c1, expected_c1);

        // Verify recorded blocks.
        assert_eq!(cipher.recorded_blocks.borrow().len(), 2);
        let mut t0 = tweak;
        assert_eq!(cipher.recorded_blocks.borrow()[0], p0 ^ t0.0);
        t0.update();
        assert_eq!(cipher.recorded_blocks.borrow()[1], p1 ^ t0.0);
    }

    #[test_case(16; "exact_block")]
    #[test_case(17; "one_byte_more_than_block")]
    #[test_case(31; "one_byte_less_than_two_blocks")]
    #[test_case(32; "exact_two_blocks")]
    #[test_case(80; "bunch_of_blocks_exact")]
    #[test_case(87; "bunch_of_blocks_cts")]
    fn test_cts_encrypt_decrypt(len: usize) {
        let tweak = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut plaintext_vec = vec![0u128; (len + 15) / 16];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..len].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let plaintext = &plaintext_bytes[..len];

        let mut ciphertext_vec = vec![0u128; (len + 15) / 16];
        let ciphertext_bytes = ciphertext_vec.as_mut_bytes();

        let mut decrypted_vec = vec![0u128; (len + 15) / 16];
        let decrypted_bytes = decrypted_vec.as_mut_bytes();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt out-of-place
        {
            let processor = XtsCtsProcessor::new(tweak, plaintext, &mut ciphertext_bytes[..len]);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        let ciphertext = &ciphertext_bytes[..len];

        // Verify none of the original blocks/content are intact
        assert_ne!(ciphertext, plaintext, "Ciphertext should not match plaintext");
        for chunk_start in (0..len).step_by(16) {
            let chunk_end = (chunk_start + 16).min(len);
            assert_ne!(
                &ciphertext[chunk_start..chunk_end],
                &plaintext[chunk_start..chunk_end],
                "Chunk at {chunk_start}..{chunk_end} should not match plaintext"
            );
        }

        // Decrypt out-of-place
        {
            let processor = XtsCtsProcessor::new(tweak, ciphertext, &mut decrypted_bytes[..len]);
            BlockCipherDecClosure::call(processor, &cipher);
        }

        let decrypted = &decrypted_bytes[..len];

        // Verify decrypted matches original plaintext
        assert_eq!(decrypted, plaintext, "Decrypted text should match original plaintext");
    }

    #[test]
    fn test_cts_encrypt_decrypt_in_place() {
        const LEN: usize = 87;

        let tweak = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut buf_vec = vec![0u128; (LEN + 15) / 16];
        let buf_bytes = buf_vec.as_mut_bytes();
        for (i, b) in buf_bytes[..LEN].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let original_plaintext = buf_bytes[..LEN].to_vec();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt in-place
        {
            let processor = XtsCtsProcessor::new_in_place(tweak, &mut buf_bytes[..LEN]);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // Verify in-place ciphertext differs from original plaintext
        assert_ne!(
            &buf_bytes[..LEN],
            &original_plaintext[..],
            "In-place ciphertext should not match plaintext"
        );
        for chunk_start in (0..LEN).step_by(16) {
            let chunk_end = (chunk_start + 16).min(LEN);
            assert_ne!(
                &buf_bytes[chunk_start..chunk_end],
                &original_plaintext[chunk_start..chunk_end],
                "Chunk at {chunk_start}..{chunk_end} should not match plaintext"
            );
        }

        // Decrypt in-place
        {
            let processor = XtsCtsProcessor::new_in_place(tweak, &mut buf_bytes[..LEN]);
            BlockCipherDecClosure::call(processor, &cipher);
        }

        // Verify decrypted matches original plaintext
        assert_eq!(
            &buf_bytes[..LEN],
            &original_plaintext[..],
            "Decrypted in-place text should match original plaintext"
        );
    }

    #[test_case(4; "four_blocks")]
    #[test_case(8; "eight_blocks")]
    fn test_cts_matches_normal_xts_on_exact_blocks(num_blocks: usize) {
        let tweak = Tweak::new(0x9876543210abcdef9876543210abcdef);
        let key = 0x0123456789abcdef0123456789abcdef;

        let len = num_blocks * 16;
        let mut plaintext_vec = vec![0u128; num_blocks + 4];
        let addr = plaintext_vec.as_ptr() as usize;
        let offset = (64 - (addr % 64)) % 64;
        let plaintext_bytes = &mut plaintext_vec.as_mut_bytes()[offset..offset + len];
        for (i, b) in plaintext_bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }

        let mut normal_cts_vec = vec![0u128; num_blocks + 4];
        let normal_addr = normal_cts_vec.as_ptr() as usize;
        let normal_offset = (64 - (normal_addr % 64)) % 64;
        let normal_cts_bytes =
            &mut normal_cts_vec.as_mut_bytes()[normal_offset..normal_offset + len];

        let mut cts_vec = vec![0u128; num_blocks + 4];
        let cts_addr = cts_vec.as_ptr() as usize;
        let cts_offset = (64 - (cts_addr % 64)) % 64;
        let cts_bytes = &mut cts_vec.as_mut_bytes()[cts_offset..cts_offset + len];

        let cipher = MockNonLinearCipher::new(key);

        // Normal XTS encryption
        {
            let processor = XtsProcessor::new(tweak, &*plaintext_bytes, &mut *normal_cts_bytes);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // CTS XTS encryption
        {
            let processor = XtsCtsProcessor::new(tweak, &*plaintext_bytes, &mut *cts_bytes);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_eq!(
            normal_cts_bytes, cts_bytes,
            "CTS XTS should match normal XTS for {num_blocks} blocks"
        );
    }

    #[test_case(1; "one_block")]
    #[test_case(2; "two_blocks")]
    #[test_case(5; "five_blocks")]
    fn test_cts_matches_inplace_xts_on_exact_blocks(num_blocks: usize) {
        let tweak = Tweak::new(0x9876543210abcdef9876543210abcdef);
        let key = 0x0123456789abcdef0123456789abcdef;

        let len = num_blocks * 16;
        let mut plaintext_vec = vec![0u128; num_blocks];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..len].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let plaintext = &plaintext_bytes[..len];

        let mut normal_cts_vec = vec![0u128; num_blocks];
        let mut cts_vec = vec![0u128; num_blocks];

        let cipher = MockNonLinearCipher::new(key);

        // In-place XTS encryption
        {
            let slice = &mut normal_cts_vec.as_mut_bytes()[..len];
            slice.copy_from_slice(plaintext);
            let processor = XtsInPlaceProcessor::new(tweak, slice);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // CTS XTS encryption
        {
            let processor =
                XtsCtsProcessor::new(tweak, plaintext, &mut cts_vec.as_mut_bytes()[..len]);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_eq!(
            normal_cts_vec, cts_vec,
            "CTS XTS should match in-place XTS for {num_blocks} blocks"
        );
    }

    #[test]
    fn test_cts_different_tweaks_different_ciphertext() {
        const LEN: usize = 87;

        let tweak1 = Tweak::new(0x123456789abcdef0123456789abcdef0);
        let tweak2 = Tweak::new(0xfeef876543210fedcba9876543210fed);
        let key = 0xffeeddccbbaa99887766554433221100;

        let mut plaintext_vec = vec![0u128; (LEN + 15) / 16];
        let plaintext_bytes = plaintext_vec.as_mut_bytes();
        for (i, b) in plaintext_bytes[..LEN].iter_mut().enumerate() {
            *b = ((i % 255) + 1) as u8;
        }
        let plaintext = &plaintext_bytes[..LEN];

        let mut ciphertext1_vec = vec![0u128; (LEN + 15) / 16];
        let ciphertext1_bytes = ciphertext1_vec.as_mut_bytes();

        let mut ciphertext2_vec = vec![0u128; (LEN + 15) / 16];
        let ciphertext2_bytes = ciphertext2_vec.as_mut_bytes();

        let cipher = MockNonLinearCipher::new(key);

        // Encrypt with tweak1
        {
            let processor = XtsCtsProcessor::new(tweak1, plaintext, &mut ciphertext1_bytes[..LEN]);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        // Encrypt with tweak2
        {
            let processor = XtsCtsProcessor::new(tweak2, plaintext, &mut ciphertext2_bytes[..LEN]);
            BlockCipherEncClosure::call(processor, &cipher);
        }

        assert_ne!(
            &ciphertext1_bytes[..LEN],
            &ciphertext2_bytes[..LEN],
            "Different tweaks should produce different ciphertexts"
        );
    }

    #[test]
    #[should_panic(expected = "Source and destination lengths must match")]
    fn test_cts_panic_length_mismatch() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 3];
        let _ = XtsCtsProcessor::new(tweak, src_buf.as_bytes(), dst_buf.as_mut_bytes());
    }

    #[test]
    #[should_panic]
    fn test_cts_panic_too_short() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 2];
        let _ = XtsCtsProcessor::new(
            tweak,
            &src_buf.as_bytes()[..15],
            &mut dst_buf.as_mut_bytes()[..15],
        );
    }

    #[test]
    #[should_panic(expected = "src must be 16 byte aligned")]
    fn test_cts_panic_unaligned_src() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 3];
        let mut dst_buf = vec![0u128; 2];
        let _ = XtsCtsProcessor::new(
            tweak,
            &src_buf.as_bytes()[1..17],
            &mut dst_buf.as_mut_bytes()[..16],
        );
    }

    #[test]
    #[should_panic(expected = "dst must be 16 byte aligned")]
    fn test_cts_panic_unaligned_dst() {
        let tweak = Tweak::new(0);
        let src_buf = vec![0u128; 2];
        let mut dst_buf = vec![0u128; 3];
        let _ = XtsCtsProcessor::new(
            tweak,
            &src_buf.as_bytes()[..16],
            &mut dst_buf.as_mut_bytes()[1..17],
        );
    }
}
