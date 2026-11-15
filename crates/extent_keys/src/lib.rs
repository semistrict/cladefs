// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax's own: the keys a tenant's data is encrypted under at rest, and the primitives that use
//! them (backlog doc-16). A tenant's KEK is in a KMS; it wraps each of the tenant's cells' KEKs,
//! each of which wraps its volumes' DEKs ([`Secret::wrap`]); an object's data is sealed
//! ([`Aead`]) under a subkey of its branch's DEK ([`Secret::derive`]), from a salt of its own
//! ([`Salt`]), and its metadata authenticated ([`Secret::mac`]); raw files on local disk are
//! encrypted a sector at a time ([`xts::Xts`]). Every primitive the workspace encrypts with is
//! here, over aws-lc-rs, but AES-256-XTS, which it has not (`storage_xts` over `aes`), so that one
//! crate holds what is secret and how it is used.

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::key_wrap::{AES_256, KeyEncryptionKey, KeyWrap};
use aws_lc_rs::{hkdf, hmac, rand};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub mod kms;
pub mod xts;

#[cfg(any(test, feature = "testing"))]
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

/// Data, a key or a MAC that is not what was sealed, wrapped or authenticated: tampered with,
/// corrupt, or under another key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not authentic: {0}")]
pub struct Unauthentic(pub &'static str);

/// A secret key of [`KEY_LEN`] bytes: zeroed when dropped, never printed, never serialized, and not
/// [`Clone`], so that each copy is one someone made on purpose. What leaves memory is a key
/// [`Wrapped`] by another, or by a KMS.
pub struct Secret(Box<[u8; KEY_LEN]>);

impl Secret {
    /// A new key, from the system's random generator.
    pub fn random() -> Self {
        let mut key = Box::new([0; KEY_LEN]);
        rand::fill(&mut key[..]).expect("the system's random generator fills a key");
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

    /// The key `wrapped` holds, if this one wrapped it.
    pub fn unwrap(&self, wrapped: &Wrapped) -> Result<Secret, Unauthentic> {
        let mut key = Box::new([0; KEY_LEN]);
        let kek = KeyEncryptionKey::new(&AES_256, &self.0[..]).expect("a 256-bit AES-KW key");
        let len = kek
            .unwrap(&wrapped.0, &mut key[..])
            .map_err(|_| Unauthentic("a wrapped key, by its integrity check"))?
            .len();
        if len != KEY_LEN {
            key.zeroize();
            return Err(Unauthentic("a wrapped key, of the wrong length"));
        }
        Ok(Self(key))
    }

    /// The HMAC-SHA256 of `bytes` under it.
    pub fn mac(&self, bytes: &[u8]) -> [u8; MAC_LEN] {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.0[..]);
        hmac::sign(&key, bytes).as_ref().try_into().expect("HMAC-SHA256 is 32 bytes")
    }

    /// Checks `mac` is the HMAC-SHA256 of `bytes` under it, in constant time.
    pub fn verify(&self, bytes: &[u8], mac: &[u8; MAC_LEN]) -> Result<(), Unauthentic> {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.0[..]);
        hmac::verify(&key, bytes, mac).map_err(|_| Unauthentic("bytes, by their MAC"))
    }
}

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

/// A salt for [`Secret::derive`]: random, one per object sealed, kept with it in the clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Salt(pub [u8; SALT_LEN]);

impl Salt {
    /// A new salt, from the system's random generator.
    pub fn random() -> Self {
        let mut salt = [0; SALT_LEN];
        rand::fill(&mut salt).expect("the system's random generator fills a salt");
        Self(salt)
    }
}

/// A key wrapped by another ([`Secret::wrap`]) or by a KMS: what is kept of a key, in records and
/// cells' objects. Its bytes say nothing of the key without the one that wrapped it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Wrapped(pub Vec<u8>);

/// Which DEK an object's subkey came from: its branch's, of that generation (doc-16, Keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyId {
    pub branch: u64,
    pub generation: u32,
}

/// AES-256-GCM under one key, which seals with nonces its user makes unique for it: a subkey's
/// parts' indices, as each object has a subkey of its own.
pub struct Aead(LessSafeKey);

impl Aead {
    pub fn new(key: &Secret) -> Self {
        let key = UnboundKey::new(&AES_256_GCM, &key.0[..]).expect("a 256-bit AES-GCM key");
        Self(LessSafeKey::new(key))
    }

    /// Seals `in_out` in place under `nonce`, unique for this key, authenticating `aad` with it:
    /// its tag.
    pub fn seal(&self, nonce: u64, aad: &[u8], in_out: &mut [u8]) -> [u8; TAG_LEN] {
        let tag = self
            .0
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
            self.0.open_in_place_separate_tag(nonce_of(nonce), Aad::from(aad), tag, in_out);
        opened.map(|_| ()).map_err(|_| Unauthentic("sealed bytes, by their tag"))
    }
}

/// The 96-bit nonce of `nonce`: zeros, then `nonce` big-endian.
fn nonce_of(nonce: u64) -> Nonce {
    let mut bytes = [0; NONCE_LEN];
    bytes[NONCE_LEN - 8..].copy_from_slice(&nonce.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Secret {
        Secret::from_bytes(&[byte; KEY_LEN])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sealed_bytes_open_to_what_was_sealed() {
        let aead = Aead::new(&key(1));
        let mut data = b"an extent's data".to_vec();
        let tag = aead.seal(7, b"segments/1/0", &mut data);
        assert_ne!(data, b"an extent's data");
        aead.open(7, b"segments/1/0", &mut data, &tag).unwrap();
        assert_eq!(data, b"an extent's data");
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

    /// What objects are sealed with is a format (doc-11): the nonce's layout and the cipher may not
    /// change unnoticed. The values were checked against other implementations on 2026-11-14: the
    /// ciphertext as LibreSSL's AES-256 of the nonce and counter 2, the tag by a GHASH in Python.
    #[test]
    fn sealing_is_as_it_was() {
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

    #[test]
    fn secrets_print_no_bytes() {
        assert_eq!(format!("{:?}", key(0xab)), "Secret(..)");
    }

    #[test]
    fn random_keys_and_salts_differ() {
        assert_ne!(Secret::random().expose(), Secret::random().expose());
        assert_ne!(Salt::random(), Salt::random());
    }
}
