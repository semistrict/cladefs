// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the keys a tenant's data is encrypted under at rest, and the primitives that use
//! them (backlog doc-16). A tenant's KEK is in a KMS; it wraps each of the tenant's cells' KEKs,
//! each of which wraps its cells' DEKs ([`Secret::wrap`]); an object's data is encrypted
//! ([`Aead`]) under a subkey of its cell's DEK ([`Secret::derive`]), from a salt of its own
//! ([`Salt`]), and its metadata authenticated ([`Secret::mac`]); raw files on local disk are
//! encrypted a sector at a time ([`xts::Xts`]). Every primitive the workspace encrypts with is
//! here, over aws-lc-rs, but AES-256-XTS, which it has not (`storage_xts` over `aes`), so that one
//! crate holds what is secret and how it is used.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(feature = "crypto")]
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
#[cfg(feature = "crypto")]
use aws_lc_rs::key_wrap::{AES_256, KeyEncryptionKey, KeyWrap};
#[cfg(feature = "crypto")]
use aws_lc_rs::{hkdf, hmac};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
#[cfg(feature = "crypto")]
use zeroize::Zeroize;

#[cfg(feature = "crypto")]
pub mod kms;
#[cfg(feature = "crypto")]
pub mod random;
#[cfg(feature = "crypto")]
pub mod xts;
#[cfg(not(feature = "crypto"))]
pub mod xts {
    /// The bytes of a sector: kept in keyless builds for frame alignment checks.
    pub const SECTOR: usize = 4096;
}

#[cfg(all(feature = "crypto", any(test, feature = "testing")))]
pub mod testing;

/// The bytes of a key: AES-256's, HMAC-SHA256's, HKDF-SHA256's output.
pub const KEY_LEN: usize = 32;

/// The bytes of a [`Salt`].
pub const SALT_LEN: usize = 32;

/// The bytes of an AES-256-GCM tag.
pub const TAG_LEN: usize = 16;

/// The bytes of an HMAC-SHA256.
pub const MAC_LEN: usize = 32;

/// The bytes of a key wrapped by another (AES-KW, RFC 3394: the key and 8 bytes).
pub const WRAPPED_LEN: usize = KEY_LEN + 8;

/// Data, a key or a MAC that is not what was encrypted, wrapped or authenticated: tampered with,
/// corrupt, or under another key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not authentic: {0}")]
pub struct Unauthentic(pub &'static str);

/// A secret key of [`KEY_LEN`] bytes: zeroed when dropped, never printed, never serialized, and not
/// [`Clone`], so that each copy is one someone made on purpose. What leaves memory is a key
/// [`Wrapped`] by another, or by a KMS.
#[cfg(feature = "crypto")]
pub struct Secret(Box<[u8; KEY_LEN]>);

/// Uninhabited placeholder for [`Secret`] in keyless builds (`default-features = false`): no key
/// can ever be constructed or held, while trait signatures such as `extent_frame::Keys` stay
/// identical.
#[cfg(not(feature = "crypto"))]
pub struct Secret(std::convert::Infallible);

#[cfg(not(feature = "crypto"))]
impl Secret {
    /// Unreachable: no `Secret` can ever be constructed in keyless builds.
    pub fn absurd(&self) -> ! {
        match self.0 {}
    }
}

#[cfg(feature = "crypto")]
impl Secret {
    /// A new key, from the system's random generator.
    pub fn random() -> Self {
        let mut key = Box::new([0; KEY_LEN]);
        random::fill(&mut key[..]);
        Self(key)
    }

    /// The key `bytes` hold, which the caller zeroes if they were secret.
    pub fn from_bytes(bytes: &[u8; KEY_LEN]) -> Self {
        Self(Box::new(*bytes))
    }

    /// Its bytes: for a KMS to wrap, and nothing else that would keep them.
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The key HKDF-SHA256 derives from it, `salt` and `info`: a subkey, one per salt and info,
    /// that tells nothing of it nor of its other subkeys.
    pub fn derive(&self, salt: &Salt, info: &[&[u8]]) -> Secret {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt.0).extract(&self.0[..]);
        let mut key = Box::new([0; KEY_LEN]);
        prk.expand(info, hkdf::HKDF_SHA256)
            .and_then(|okm| okm.fill(&mut key[..]))
            .expect("HKDF-SHA256 expands to its hash's length");
        Self(key)
    }

    /// `key`, wrapped by this one with AES-KW (RFC 3394), which authenticates it.
    pub fn wrap(&self, key: &Secret) -> Wrapped {
        let mut out = [0; WRAPPED_LEN];
        let kek = KeyEncryptionKey::new(&AES_256, &self.0[..]).expect("a 256-bit AES-KW key");
        let wrapped = kek.wrap(&key.0[..], &mut out).expect("AES-KW wraps a 256-bit key");
        Wrapped(wrapped.to_vec())
    }

    /// The key `wrapped` holds, if this one wrapped it. What was unwrapped into is a [`Secret`]
    /// from the start, zeroed however it is refused (board 8589).
    pub fn unwrap(&self, wrapped: &Wrapped) -> Result<Secret, Unauthentic> {
        let mut key = Self(Box::new([0; KEY_LEN]));
        let kek = KeyEncryptionKey::new(&AES_256, &self.0[..]).expect("a 256-bit AES-KW key");
        let len = kek
            .unwrap(&wrapped.0, &mut key.0[..])
            .map_err(|_| Unauthentic("a wrapped key, by its integrity check"))?
            .len();
        if len != KEY_LEN {
            return Err(Unauthentic("a wrapped key, of the wrong length"));
        }
        Ok(key)
    }

    /// The HMAC-SHA256 of `bytes` under it.
    pub fn mac(&self, bytes: &[u8]) -> [u8; MAC_LEN] {
        self.mac_of(&[bytes])
    }

    /// Checks `mac` is the HMAC-SHA256 of `bytes` under it, in constant time.
    pub fn verify(&self, bytes: &[u8], mac: &[u8; MAC_LEN]) -> Result<(), Unauthentic> {
        self.verify_of(&[bytes], mac)
    }

    /// The HMAC-SHA256 under it of `pieces`, one after another.
    pub fn mac_of(&self, pieces: &[&[u8]]) -> [u8; MAC_LEN] {
        let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, &self.0[..]));
        pieces.iter().for_each(|piece| context.update(piece));
        context.sign().as_ref().try_into().expect("HMAC-SHA256 is 32 bytes")
    }

    /// Checks `mac` is the HMAC-SHA256 of `pieces` under it ([`Secret::mac_of`]), in constant
    /// time.
    pub fn verify_of(&self, pieces: &[&[u8]], mac: &[u8; MAC_LEN]) -> Result<(), Unauthentic> {
        aws_lc_rs::constant_time::verify_slices_are_equal(&self.mac_of(pieces), mac)
            .map_err(|_| Unauthentic("bytes, by their MAC"))
    }

    /// cladefs: the HMAC-SHA256 under it of `pieces`, one after another, truncated to its first
    /// [`TAG_LEN`] bytes (RFC 2104, 5; NIST SP 800-107, 5.3.3): a tag as long as an AES-256-GCM
    /// tag, to authenticate what is encrypted otherwise, as a raw file's XTS sectors are
    /// (TASK-411.23). A MAC needing no nonce: tagging the same bytes twice gives the same tag, and
    /// other bytes tell nothing of the key.
    pub fn tag(&self, pieces: &[&[u8]]) -> [u8; TAG_LEN] {
        self.mac_of(pieces)[..TAG_LEN].try_into().expect("HMAC-SHA256 is longer than a tag")
    }

    /// cladefs: checks `tag` is the tag of `pieces` under it ([`Secret::tag`]), in constant time.
    pub fn verify_tag(&self, pieces: &[&[u8]], tag: &[u8; TAG_LEN]) -> Result<(), Unauthentic> {
        aws_lc_rs::constant_time::verify_slices_are_equal(&self.tag(pieces), tag)
            .map_err(|_| Unauthentic("bytes, by their tag"))
    }
}

#[cfg(feature = "crypto")]
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// A salt for [`Secret::derive`]: random, one per object encrypted, kept with it in the clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Salt(pub [u8; SALT_LEN]);

#[cfg(feature = "crypto")]
impl Salt {
    /// A new salt, from the system's random generator.
    pub fn random() -> Self {
        let mut salt = [0; SALT_LEN];
        random::fill(&mut salt);
        Self(salt)
    }
}

/// A key wrapped by another ([`Secret::wrap`]) or by a KMS: what is kept of a key, in records and
/// cells' objects. Its bytes say nothing of the key without the one that wrapped it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TypeFingerprint)]
pub struct Wrapped(pub Vec<u8>);

/// Which DEK an object's subkey came from: its cell's, of that generation (doc-16, Keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyId {
    pub cell: u64,
    pub generation: u32,
}

/// AES-256-GCM under one key, which seals with nonces its user makes unique for it: a subkey's
/// parts' indices, as each object has a subkey of its own.
#[cfg(feature = "crypto")]
pub struct Aead {
    sealing: LessSafeKey,
    /// cladefs: its key, for [`Aead::tag`], which goes through aws-lc's EVP interface.
    key: Secret,
}

#[cfg(feature = "crypto")]
impl Aead {
    pub fn new(key: &Secret) -> Self {
        let unbound = UnboundKey::new(&AES_256_GCM, &key.0[..]).expect("a 256-bit AES-GCM key");
        Self { sealing: LessSafeKey::new(unbound), key: Secret::from_bytes(key.expose()) }
    }

    /// Seals `in_out` in place under `nonce`, unique for this key, authenticating `aad` with it:
    /// its tag.
    pub fn seal(&self, nonce: u64, aad: &[u8], in_out: &mut [u8]) -> [u8; TAG_LEN] {
        let tag = self
            .sealing
            .seal_in_place_separate_tag(nonce_of(nonce), Aad::from(aad), in_out)
            .expect("AES-GCM seals what fits its limits");
        tag.as_ref().try_into().expect("an AES-GCM tag is 16 bytes")
    }

    /// Opens in place what [`Aead::seal`] sealed under `nonce` with `aad`, given its tag; if it is
    /// not authentic, `in_out` holds nothing of use.
    pub fn open(
        &self,
        nonce: u64,
        aad: &[u8],
        in_out: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), Unauthentic> {
        let opened =
            self.sealing.open_in_place_separate_tag(nonce_of(nonce), Aad::from(aad), tag, in_out);
        opened.map(|_| ()).map_err(|_| Unauthentic("sealed bytes, by their tag"))
    }

    /// cladefs: the tag [`Aead::seal`] gives under `nonce`, unique for this key, sealing nothing,
    /// its associated data `pieces` one after another (AES-256-GCM's GMAC): without the copy of
    /// them into one buffer it would take, as aws-lc-rs takes associated data whole, a part's 1 MiB
    /// each time (board 10553). Through aws-lc's EVP interface, which takes it a piece at a time.
    pub fn tag(&self, nonce: u64, pieces: &[&[u8]]) -> [u8; TAG_LEN] {
        use aws_lc_sys::{
            EVP_CIPHER_CTX, EVP_CIPHER_CTX_ctrl, EVP_CIPHER_CTX_free, EVP_CIPHER_CTX_new,
            EVP_CTRL_GCM_GET_TAG, EVP_EncryptFinal_ex, EVP_EncryptInit_ex, EVP_EncryptUpdate,
            EVP_aes_256_gcm,
        };
        use std::ffi::c_int;
        /// The most bytes one update takes: its length is a C `int`.
        const MOST: usize = 1 << 30;
        /// A context of aws-lc's, freed when dropped.
        struct Context(*mut EVP_CIPHER_CTX);
        impl Drop for Context {
            fn drop(&mut self) {
                // SAFETY: a context `EVP_CIPHER_CTX_new` made, freed once, here.
                unsafe { EVP_CIPHER_CTX_free(self.0) }
            }
        }
        let iv = nonce_bytes(nonce);
        let mut tag = [0; TAG_LEN];
        // SAFETY: a context made here, alive for every call on it and freed after them; the key is
        // AES-256's 32 bytes and the IV GCM's 12; each piece is read for its length alone, at most
        // `MOST` bytes, which fits a C int; encrypting nothing, the final call writes no byte of
        // `out`, and the tag is written into `tag`, `TAG_LEN` bytes long.
        unsafe {
            let context = Context(EVP_CIPHER_CTX_new());
            assert!(!context.0.is_null(), "aws-lc made no cipher context");
            let init = EVP_EncryptInit_ex(
                context.0,
                EVP_aes_256_gcm(),
                std::ptr::null_mut(),
                self.key.expose().as_ptr(),
                iv.as_ptr(),
            );
            assert_eq!(init, 1, "AES-256-GCM takes a 256-bit key and a 96-bit nonce");
            for piece in pieces.iter().flat_map(|piece| piece.chunks(MOST)) {
                let mut len: c_int = 0;
                let update = EVP_EncryptUpdate(
                    context.0,
                    std::ptr::null_mut(),
                    &mut len,
                    piece.as_ptr(),
                    piece.len() as c_int,
                );
                assert_eq!(update, 1, "AES-256-GCM takes associated data a piece at a time");
            }
            let mut out = [0u8; TAG_LEN];
            let mut len: c_int = 0;
            let finished = EVP_EncryptFinal_ex(context.0, out.as_mut_ptr(), &mut len);
            assert_eq!((finished, len), (1, 0), "AES-256-GCM tags what it sealed of nothing");
            let got = EVP_CIPHER_CTX_ctrl(
                context.0,
                EVP_CTRL_GCM_GET_TAG,
                TAG_LEN as c_int,
                tag.as_mut_ptr().cast(),
            );
            assert_eq!(got, 1, "AES-256-GCM gives its tag");
        }
        tag
    }

    /// cladefs: checks `tag` is [`Aead::tag`]'s of `pieces` under `nonce`, in constant time, as
    /// [`Aead::open`] checks one of what sealed nothing.
    pub fn verify_tag(
        &self,
        nonce: u64,
        pieces: &[&[u8]],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), Unauthentic> {
        aws_lc_rs::constant_time::verify_slices_are_equal(&self.tag(nonce, pieces), tag)
            .map_err(|_| Unauthentic("sealed bytes, by their tag"))
    }
}

/// The 96-bit nonce of `nonce`: zeros, then `nonce` big-endian.
#[cfg(feature = "crypto")]
fn nonce_bytes(nonce: u64) -> [u8; NONCE_LEN] {
    let mut bytes = [0; NONCE_LEN];
    bytes[NONCE_LEN - 8..].copy_from_slice(&nonce.to_be_bytes());
    bytes
}

/// [`nonce_bytes`], as aws-lc-rs takes a nonce.
#[cfg(feature = "crypto")]
fn nonce_of(nonce: u64) -> Nonce {
    Nonce::assume_unique_for_key(nonce_bytes(nonce))
}

#[cfg(all(test, feature = "crypto"))]
mod tests {
    use super::*;

    fn key(byte: u8) -> Secret {
        Secret::from_bytes(&[byte; KEY_LEN])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sealed_bytes_open_under_nothing_else() {
        let aead = Aead::new(&key(1));
        let mut sealed = b"an extent's data".to_vec();
        let tag = aead.seal(7, b"segments/1/0", &mut sealed);
        let refused = Err(Unauthentic("sealed bytes, by their tag"));

        let mut data = sealed.clone();
        assert_eq!(Aead::new(&key(2)).open(7, b"segments/1/0", &mut data, &tag), refused);
        let mut data = sealed.clone();
        assert_eq!(aead.open(8, b"segments/1/0", &mut data, &tag), refused);
        let mut data = sealed.clone();
        assert_eq!(aead.open(7, b"segments/1/1", &mut data, &tag), refused);
        let mut data = sealed.clone();
        data[3] ^= 1;
        assert_eq!(aead.open(7, b"segments/1/0", &mut data, &tag), refused);
        let mut data = sealed.clone();
        let mut flipped = tag;
        flipped[0] ^= 1;
        assert_eq!(aead.open(7, b"segments/1/0", &mut data, &flipped), refused);
    }

    /// What objects are encrypted with is a format (doc-11): the nonce's layout and the cipher may
    /// not change unnoticed. The values were checked against other implementations on 2026-11-14:
    /// the ciphertext as LibreSSL's AES-256 of the nonce and counter 2, the tag by a GHASH in
    /// Python.
    #[test]
    fn encryption_is_as_it_was() {
        let mut data = [0; 16];
        let tag = Aead::new(&key(1)).seal(7, b"aad", &mut data);
        assert_eq!(hex(&data), "ef922a7cb2a6d40ca3c5567b611615d9");
        assert_eq!(hex(&tag), "175a6cb4014db88f06b9bdfcdd0137de");
    }

    /// The subkey pinned was checked against HKDF-SHA256 (RFC 5869) in Python's hmac on 2026-11-14.
    #[test]
    fn subkeys_differ_by_salt_and_info_and_are_as_they_were() {
        let dek = key(1);
        let salt = Salt([2; SALT_LEN]);
        let subkey = dek.derive(&salt, &[b"part", b"segments/1/0"]);
        assert_eq!(subkey.expose(), dek.derive(&salt, &[b"part", b"segments/1/0"]).expose());
        assert_ne!(subkey.expose(), dek.derive(&Salt([3; SALT_LEN]), &[b"part"]).expose());
        assert_ne!(subkey.expose(), dek.derive(&salt, &[b"mac", b"segments/1/0"]).expose());
        assert_eq!(
            hex(subkey.expose()),
            "e060fc32866159c37ab20c343d31515bcc39b5185c034bf60d967c90691a2e3f"
        );
    }

    #[test]
    fn wrapped_keys_unwrap_under_their_kek_alone() {
        let (kek, dek) = (key(1), Secret::random());
        let wrapped = kek.wrap(&dek);
        assert_eq!(wrapped.0.len(), WRAPPED_LEN);
        assert_eq!(kek.unwrap(&wrapped).unwrap().expose(), dek.expose());
        let refused = Err(Unauthentic("a wrapped key, by its integrity check"));
        assert_eq!(key(2).unwrap(&wrapped).map(|_| ()), refused);
        let mut flipped = wrapped.clone();
        flipped.0[9] ^= 1;
        assert_eq!(kek.unwrap(&flipped).map(|_| ()), refused);
    }

    #[test]
    fn macs_verify_for_their_bytes_alone() {
        let mac = key(1).mac(b"a footer");
        assert_eq!(key(1).verify(b"a footer", &mac), Ok(()));
        let refused = Err(Unauthentic("bytes, by their MAC"));
        assert_eq!(key(1).verify(b"a footer!", &mac), refused);
        assert_eq!(key(2).verify(b"a footer", &mac), refused);
    }

    /// TASK-411.23: a tag is HMAC-SHA256's first 16 bytes of its pieces, one after another, however
    /// they are cut; it verifies for those bytes under that key alone. The tag pinned is the first
    /// half of HMAC-SHA256(32 bytes of 1, "a raw part"), checked in Python's hmac on 2026-11-15.
    #[test]
    fn tags_are_truncated_macs_of_their_pieces() {
        let tag = key(1).tag(&[b"a raw ", b"part"]);
        assert_eq!(tag, key(1).tag(&[b"a raw part"]));
        assert_eq!(tag[..], key(1).mac(b"a raw part")[..TAG_LEN]);
        assert_eq!(hex(&tag), "8a5232c2b730346b5e0e41d305bdd2d3");
        assert_eq!(key(1).verify_tag(&[b"a raw part"], &tag), Ok(()));
        let refused = Err(Unauthentic("bytes, by their tag"));
        assert_eq!(key(1).verify_tag(&[b"a raw part!"], &tag), refused);
        assert_eq!(key(2).verify_tag(&[b"a raw part"], &tag), refused);
        let mut flipped = tag;
        flipped[15] ^= 1;
        assert_eq!(key(1).verify_tag(&[b"a raw part"], &flipped), refused);
    }

    /// cladefs: an AES-GCM tag of pieces is aws-lc-rs's seal of nothing, their concatenation its
    /// associated data, however they are cut: at block boundaries and either side of them, empty
    /// pieces among them, for nonces at their edges (board 10553). It verifies for those bytes,
    /// that nonce and that key alone.
    #[test]
    fn aead_tags_of_pieces_are_seals_of_their_concatenation() {
        let aead = Aead::new(&key(3));
        let bytes: Vec<u8> =
            (0..(1 << 20) + 41).map(|at: usize| (at * 31 + at / 251) as u8).collect();
        let lens = [0, 1, 15, 16, 17, 25, 32, 4096, 1 << 20, (1 << 20) + 41];
        for nonce in [0, 1, 7, u64::MAX - 1, u64::MAX] {
            for len in lens {
                let all = &bytes[..len];
                let sealed = aead.seal(nonce, all, &mut []);
                for cut in [0, 1, 7, 16, 25, len / 2, len.saturating_sub(1), len] {
                    let (first, second) = all.split_at(cut.min(len));
                    let pieces: [&[u8]; 4] = [first, &[], second, &[]];
                    assert_eq!(
                        aead.tag(nonce, &pieces),
                        sealed,
                        "nonce {nonce}, {len} cut at {cut}"
                    );
                    assert_eq!(aead.verify_tag(nonce, &pieces, &sealed), Ok(()));
                }
            }
        }
        let tag = aead.tag(9, &[b"a part's ", b"bytes"]);
        assert_eq!(aead.open(9, b"a part's bytes", &mut [], &tag), Ok(()));
        let refused = Err(Unauthentic("sealed bytes, by their tag"));
        assert_eq!(aead.verify_tag(9, &[b"a part's bytes!"], &tag), refused);
        assert_eq!(aead.verify_tag(10, &[b"a part's bytes"], &tag), refused);
        assert_eq!(Aead::new(&key(4)).verify_tag(9, &[b"a part's bytes"], &tag), refused);
        let mut flipped = tag;
        flipped[0] ^= 0x80;
        assert_eq!(aead.verify_tag(9, &[b"a part's bytes"], &flipped), refused);
    }
}
