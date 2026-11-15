// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: AES-256-XTS (IEEE 1619-2007, without ciphertext stealing), the cipher a store's raw
//! files are encrypted with a sector at a time (backlog doc-16, TASK-411.5): an open segment's, and
//! a large extent's own, read back by offset until they are framed, need a cipher that keeps
//! lengths and reads any sector alone. aws-lc-rs has none; this is Fuchsia's `storage_xts` (as
//! Fxfs's own `fxfs-crypto` uses it) over RustCrypto's `aes`, both already in the workspace.

use crate::{Salt, Secret};
use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt as _, BlockCipherEncrypt as _, KeyInit as _};
use storage_ptr_slice::MutPtrByteSlice;
use storage_xts::{Tweak, XtsInPlaceProcessor};
use zerocopy::IntoBytes as _;

/// The bytes of a sector: what [`Xts::encrypt`] and [`Xts::decrypt`] take whole, each numbered by
/// its place, which is its tweak.
pub const SECTOR: usize = 4096;

/// The bytes of an AES block, which a data unit's length is a multiple of.
const BLOCK: usize = 16;

/// AES-256-XTS under a key of two halves, AES-256's each: one encrypts the data, the other the
/// tweak, each data unit's number (IEEE 1619-2007, 5.1). Its key schedules are zeroed when dropped.
pub struct Xts {
    data: Aes256,
    tweak: Aes256,
}

impl Xts {
    /// AES-256-XTS under `data`, the key that encrypts the data (IEEE 1619's Key1), and `tweak`,
    /// the key that encrypts the tweak (Key2), which must differ.
    pub fn new(data: &Secret, tweak: &Secret) -> Self {
        assert_ne!(data.expose(), tweak.expose(), "XTS's two keys are the same");
        Self { data: Aes256::new(data.expose().into()), tweak: Aes256::new(tweak.expose().into()) }
    }

    /// AES-256-XTS under the two subkeys `key` derives from `salt` for `info` ([`Secret::derive`]):
    /// a key of its own for each salt and info.
    pub fn derive(key: &Secret, salt: &Salt, info: &[u8]) -> Self {
        Self::new(&key.derive(salt, &[info, b"data"]), &key.derive(salt, &[info, b"tweak"]))
    }

    /// Encrypts `unit` in place, a data unit of a multiple of 16 bytes, numbered `number`.
    pub fn encrypt_unit(&self, number: u128, unit: &mut [u8]) {
        self.unit(number, unit, true);
    }

    /// Decrypts `unit` in place, as [`Xts::encrypt_unit`] encrypted it.
    pub fn decrypt_unit(&self, number: u128, unit: &mut [u8]) {
        self.unit(number, unit, false);
    }

    /// Encrypts `sectors` in place, whole sectors ([`SECTOR`]), the first of them numbered `first`
    /// and each after it one more: the place of each in its file, which a sector's tweak is.
    pub fn encrypt(&self, first: u64, sectors: &mut [u8]) {
        self.sectors(first, sectors, true);
    }

    /// Decrypts `sectors` in place, as [`Xts::encrypt`] encrypted them.
    pub fn decrypt(&self, first: u64, sectors: &mut [u8]) {
        self.sectors(first, sectors, false);
    }

    fn sectors(&self, first: u64, sectors: &mut [u8], encrypt: bool) {
        assert!(
            sectors.len().is_multiple_of(SECTOR),
            "{} bytes are not whole sectors",
            sectors.len()
        );
        for (at, sector) in sectors.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            self.unit(u128::from(first + at as u64), sector, encrypt);
        }
    }

    /// Encrypts or decrypts `unit`, numbered `number`, in place: in a buffer aligned as
    /// `storage_xts` takes it, if `unit` is not.
    fn unit(&self, number: u128, unit: &mut [u8], encrypt: bool) {
        assert!(
            unit.len().is_multiple_of(BLOCK),
            "a unit of {} bytes is not whole blocks",
            unit.len()
        );
        if !unit.as_ptr().cast::<u128>().is_aligned() {
            // On the stack for a sector or less, as raw files' are.
            let (mut sector, mut more) = ([0u128; SECTOR / BLOCK], Vec::new());
            let aligned = match unit.len() <= SECTOR {
                true => &mut sector[..unit.len() / BLOCK],
                false => {
                    more.resize(unit.len() / BLOCK, 0u128);
                    &mut more[..]
                }
            };
            aligned.as_mut_bytes().copy_from_slice(unit);
            self.unit(number, aligned.as_mut_bytes(), encrypt);
            unit.copy_from_slice(aligned.as_bytes());
            return;
        }
        let mut tweak = Tweak::new(number);
        self.tweak.encrypt_block(tweak.as_mut_bytes().try_into().expect("a tweak is a block"));
        let processor = XtsInPlaceProcessor::new(tweak, MutPtrByteSlice::from(unit));
        match encrypt {
            true => self.data.encrypt_with_backend(processor),
            false => self.data.decrypt_with_backend(processor),
        }
    }
}

impl std::fmt::Debug for Xts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Xts(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    fn key(hex: &str) -> Secret {
        Secret::from_bytes(&bytes(hex).try_into().unwrap())
    }

    /// IEEE 1619-2007, Annex B, vectors 10 and 11: AES-256-XTS of 512-byte data units, numbered
    /// 0xff and 0xffff, each bytes 0 to 255 twice; as aws-lc's
    /// `crypto/fipsmodule/modes/xts_test.cc` (ISC) carries them, from OpenSSL 1.1.1d's.
    const KEY1: &str = "2718281828459045235360287471352662497757247093699959574966967627";
    const KEY2: &str = "3141592653589793238462643383279502884197169399375105820974944592";
    const VECTOR_10: &str = concat!(
        "1c3b3a102f770386e4836c99e370cf9bea00803f5e482357a4ae12d414a3e63b5d31e276f8fe4a8d66b317f9",
        "ac683f44680a86ac35adfc3345befecb4bb188fd5776926c49a3095eb108fd1098baec70aaa66999a72a82f2",
        "7d848b21d4a741b0c5cd4d5fff9dac89aeba122961d03a757123e9870f8acf1000020887891429ca2a3e7a7d",
        "7df7b10355165c8b9a6d0a7de8b062c4500dc4cd120c0f7418dae3d0b5781c34803fa75421c790dfe1de1834",
        "f280d7667b327f6c8cd7557e12ac3a0f93ec05c52e0493ef31a12d3d9260f79a289d6a379bc70c50841473d1",
        "a8cc81ec583e9645e07b8d9670655ba5bbcfecc6dc3966380ad8fecb17b6ba02469a020a84e18e8f84252070",
        "c13e9f1f289be54fbc481457778f616015e1327a02b140f1505eb309326d68378f8374595c849d84f4c333ec",
        "4423885143cb47bd71c5edae9be69a2ffeceb1bec9de244fbe15992b11b77c040f12bd8f6a975a44a0f90c29",
        "a9abc3d4d893927284c58754cce294529f8614dcd2aba991925fedc4ae74ffac6e333b93eb4aff0479da9a41",
        "0e4450e0dd7ae4c6e2910900575da401fc07059f645e8b7e9bfdef33943054ff84011493c27b3429eaedb4ed",
        "5376441a77ed43851ad77f16f541dfd269d50d6a5f14fb0aab1cbb4c1550be97f7ab4066193c4caa773dad38",
        "014bd2092fa755c824bb5e54c4f36ffda9fcea70b9c6e693e148c151",
    );
    const VECTOR_11: &str = concat!(
        "77a31251618a15e6b92d1d66dffe7b50b50bad552305ba0217a610688eff7e11e1d0225438e093242d6db274",
        "fde801d4cae06f2092c728b2478559df58e837c2469ee4a4fa794e4bbc7f39bc026e3cb72c33b0888f25b4ac",
        "f56a2a9804f1ce6d3d6e1dc6ca181d4b546179d55544aa7760c40d06741539c7e3cd9d2f6650b2013fd0eeb8",
        "c2b8e3d8d240ccae2d4c98320a7442e1c8d75a42d6e6cfa4c2eca1798d158c7aecdf82490f24bb9b38e108bc",
        "da12c3faf9a21141c3613b58367f922aaa26cd22f23d708dae699ad7cb40a8ad0b6e2784973dcb605684c08b",
        "8d6998c69aac049921871ebb65301a4619ca80ecb485a31d744223ce8ddc2394828d6a80470c092f5ba413c3",
        "378fa6054255c6f9df4495862bbb3287681f931b687c888abf844dfc8fc28331e579928cd12bd2390ae123cf",
        "03818d14dedde5c0c24c8ab018bfca75ca096f2d531f3d1619e785f1ada437cab92e980558b3dce1474afb75",
        "bfedbf8ff54cb2618e0244c9ac0d3c66fb51598cd2db11f9be39791abe447c63094f7c453b7ff87cb5bb36b7",
        "c79efb0872d17058b83b15ab0866ad8a58656c5a7e20dbdf308b2461d97c0ec0024a2715055249cf3b478ddd",
        "4740de654f75ca686e0d7345c69ed50cdc2a8b332b1f8824108ac937eb050585608ee734097fc09054fbff89",
        "eeaeea791f4a7ab1f9868294a4f9e27b42af8100cb9d59cef9645803",
    );

    #[test]
    fn ieee_1619_vectors_encrypt_and_decrypt_as_they_say() {
        let xts = Xts::new(&key(KEY1), &key(KEY2));
        let plain: Vec<u8> = (0..512).map(|at| at as u8).collect();
        for (number, vector) in [(0xff, VECTOR_10), (0xffff, VECTOR_11)] {
            let mut unit = plain.clone();
            xts.encrypt_unit(number, &mut unit);
            assert_eq!(unit, bytes(vector), "vector of unit {number:#x}");
            xts.decrypt_unit(number, &mut unit);
            assert_eq!(unit, plain, "vector of unit {number:#x}, decrypted");
        }
    }

    /// Sectors of 4 KiB, as raw files hold them, at places from the first to past 2^64 / 2, under
    /// keys and of bytes drawn at random: as aws-lc's own AES-256-XTS (`EVP_aes_256_xts`, its tweak
    /// the sector's number, little-endian) encrypts them, and decrypted back.
    #[test]
    fn sectors_encrypt_as_aws_lc_encrypts_them() {
        use aws_lc_rs::rand::fill;
        for round in 0..16u64 {
            let (data, tweak) = (Secret::random(), Secret::random());
            let mut first = [0; 8];
            fill(&mut first).unwrap();
            let first = match round {
                0 => 0,
                1 => 1,
                _ => u64::from_le_bytes(first) >> 1,
            };
            let mut plain = vec![0; 3 * SECTOR];
            fill(&mut plain).unwrap();
            let mut sectors = plain.clone();
            Xts::new(&data, &tweak).encrypt(first, &mut sectors);
            for (at, sector) in sectors.chunks(SECTOR).enumerate() {
                let theirs =
                    aws_lc(&data, &tweak, first + at as u64, &plain[at * SECTOR..][..SECTOR]);
                assert_eq!(sector, &theirs[..], "round {round}, sector {}", first + at as u64);
            }
            Xts::new(&data, &tweak).decrypt(first, &mut sectors);
            assert_eq!(sectors, plain, "round {round}, decrypted");
        }
    }

    /// `sector`, the sector numbered `number`, as aws-lc's AES-256-XTS encrypts it under `data` and
    /// `tweak`, Key1 and Key2.
    fn aws_lc(data: &Secret, tweak: &Secret, number: u64, sector: &[u8]) -> Vec<u8> {
        use aws_lc_sys::{
            EVP_CIPHER_CTX_free, EVP_CIPHER_CTX_new, EVP_EncryptInit_ex, EVP_EncryptUpdate,
            EVP_aes_256_xts,
        };
        let mut key = [0; 64];
        key[..32].copy_from_slice(data.expose());
        key[32..].copy_from_slice(tweak.expose());
        let mut iv = [0; 16];
        iv[..8].copy_from_slice(&number.to_le_bytes());
        let mut out = vec![0; sector.len()];
        let mut len = 0;
        // SAFETY: a context made and freed here; the key, IV, input and output are buffers of the
        // lengths AES-256-XTS takes, alive for the calls.
        unsafe {
            let ctx = EVP_CIPHER_CTX_new();
            assert!(!ctx.is_null());
            let init = EVP_EncryptInit_ex(
                ctx,
                EVP_aes_256_xts(),
                std::ptr::null_mut(),
                key.as_ptr(),
                iv.as_ptr(),
            );
            let update = EVP_EncryptUpdate(
                ctx,
                out.as_mut_ptr(),
                &mut len,
                sector.as_ptr(),
                sector.len() as i32,
            );
            EVP_CIPHER_CTX_free(ctx);
            assert_eq!((init, update, len as usize), (1, 1, sector.len()));
        }
        out
    }

    /// Sectors at any alignment encrypt alike: one not aligned for `storage_xts` goes through a
    /// buffer that is.
    #[test]
    fn unaligned_sectors_encrypt_as_aligned_ones() {
        let xts = Xts::derive(&Secret::random(), &Salt::random(), b"test");
        let plain: Vec<u8> = (0..SECTOR).map(|at| (at * 7) as u8).collect();
        let mut aligned = vec![0u128; SECTOR / BLOCK];
        aligned.as_mut_bytes().copy_from_slice(&plain);
        xts.encrypt(9, aligned.as_mut_bytes());
        let mut unaligned = vec![0; SECTOR + 1];
        unaligned[1..].copy_from_slice(&plain);
        assert!(!unaligned[1..].as_ptr().cast::<u128>().is_aligned());
        xts.encrypt(9, &mut unaligned[1..]);
        assert_eq!(&unaligned[1..], aligned.as_bytes());
        xts.decrypt(9, &mut unaligned[1..]);
        assert_eq!(&unaligned[1..], &plain[..]);
    }

    /// Keys derived from other salts or infos encrypt otherwise; the same, alike.
    #[test]
    fn derived_keys_differ_by_salt_and_info() {
        let (key, salt) = (Secret::random(), Salt::random());
        let encrypted = |salt: &Salt, info: &[u8]| {
            let mut sector = vec![0; SECTOR];
            Xts::derive(&key, salt, info).encrypt(0, &mut sector);
            sector
        };
        assert_eq!(encrypted(&salt, b"a"), encrypted(&salt, b"a"));
        assert_ne!(encrypted(&salt, b"a"), encrypted(&salt, b"b"));
        assert_ne!(encrypted(&salt, b"a"), encrypted(&Salt::random(), b"a"));
    }

    #[test]
    fn xts_prints_no_key() {
        assert_eq!(
            format!("{:?}", Xts::derive(&Secret::random(), &Salt::random(), b"")),
            "Xts(..)"
        );
    }
}
