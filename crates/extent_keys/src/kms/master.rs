// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the master keys the bucket KMS wraps tenants' KEKs under (TASK-411.27, doc-16, The
//! bucket KMS), given a host in its environment: [`MASTER_KEY_VAR`], or the file
//! [`MASTER_KEY_FILE_VAR`] names. Several keys rotate them: the first wraps, any unwraps. Each is
//! known by its ID ([`MasterKeyId`]), a short hash of it, which is all that is ever written or
//! said of it.

use crate::{KEY_LEN, Secret};
use aws_lc_rs::digest;
use std::ffi::OsString;
use zeroize::Zeroize as _;

/// The variable holding the master keys: each 64 hexadecimal digits, separated by commas or white
/// space (newlines among it), the newest first.
pub const MASTER_KEY_VAR: &str = "CLADEFS_MASTER_KEY";

/// The variable naming a file of the master keys instead: exactly [`KEY_LEN`] raw bytes, one key;
/// or text, as [`MASTER_KEY_VAR`] holds it.
pub const MASTER_KEY_FILE_VAR: &str = "CLADEFS_MASTER_KEY_FILE";

/// The bytes of a [`MasterKeyId`].
pub const MASTER_ID_LEN: usize = 8;

/// What a master key's ID hashes before it: so that the hash is of no other use.
const ID_LABEL: &[u8] = b"cladefs master key id\0";

/// A master key's ID: the first [`MASTER_ID_LEN`] bytes of SHA-256 over [`ID_LABEL`] and the key.
/// Said in hexadecimal. It tells which key wrapped a tenant's KEK, and nothing of the key: a key of
/// 256 random bits is not found from its hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MasterKeyId(pub [u8; MASTER_ID_LEN]);

impl MasterKeyId {
    /// The ID of `key`.
    fn of(key: &Secret) -> Self {
        let hash = digest::digest(&digest::SHA256, &[ID_LABEL, key.expose()].concat());
        Self(hash.as_ref()[..MASTER_ID_LEN].try_into().expect("SHA-256 is 32 bytes"))
    }
}

impl std::fmt::Display for MasterKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl std::fmt::Debug for MasterKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MasterKeyId({self})")
    }
}

/// A master key, and its ID.
pub struct MasterKey {
    id: MasterKeyId,
    key: Secret,
}

impl MasterKey {
    /// `key`, as a master key.
    pub fn new(key: Secret) -> Self {
        Self { id: MasterKeyId::of(&key), key }
    }

    pub fn id(&self) -> MasterKeyId {
        self.id
    }

    /// The key itself: for the bucket KMS's wrapping alone.
    pub(crate) fn key(&self) -> &Secret {
        &self.key
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MasterKey({})", self.id)
    }
}

/// A host's master keys, the newest first: it wraps tenants' KEKs; any unwraps those it wrapped.
/// Never empty, no key twice. Printed by their IDs alone.
pub struct MasterKeys(Vec<MasterKey>);

impl MasterKeys {
    /// `keys`, the newest first: refused if none, or one is there twice.
    pub fn new(keys: Vec<Secret>) -> Result<Self, String> {
        Self::named(keys, "master keys")
    }

    /// `keys`, from `source`, which refusals name.
    fn named(keys: Vec<Secret>, source: &str) -> Result<Self, String> {
        if keys.is_empty() {
            return Err(format!("{source}: no key"));
        }
        let keys: Vec<MasterKey> = keys.into_iter().map(MasterKey::new).collect();
        for (at, key) in keys.iter().enumerate() {
            if let Some(first) = keys[..at].iter().position(|other| other.id == key.id) {
                return Err(format!(
                    "{source}: key {} is key {} again ({})",
                    at + 1,
                    first + 1,
                    key.id
                ));
            }
        }
        Ok(Self(keys))
    }

    /// The keys `text` holds, from `source`, which refusals name: each [`KEY_LEN`] bytes in 64
    /// hexadecimal digits, separated by commas or white space, the newest first. A refusal says
    /// which key is not one, never what it holds.
    pub fn parse(text: &str, source: &str) -> Result<Self, String> {
        let words: Vec<&str> =
            text.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).collect();
        let mut keys = Vec::with_capacity(words.len());
        for (at, word) in words.iter().enumerate() {
            let key = hex_key(word).ok_or_else(|| {
                format!(
                    "{source}: key {} of {}: not {} hexadecimal digits",
                    at + 1,
                    words.len(),
                    2 * KEY_LEN
                )
            })?;
            keys.push(key);
        }
        Self::named(keys, source)
    }

    /// The keys a file of `bytes` holds, from `source`: exactly [`KEY_LEN`] bytes, one key, raw;
    /// otherwise text, as [`MasterKeys::parse`] reads it.
    pub fn of_file(bytes: &[u8], source: &str) -> Result<Self, String> {
        if let Ok(raw) = <&[u8; KEY_LEN]>::try_from(bytes) {
            return Self::named(vec![Secret::from_bytes(raw)], source);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| {
            format!(
                "{source}: neither {KEY_LEN} bytes, one key, nor text of keys in hexadecimal \
                 ({} bytes)",
                bytes.len()
            )
        })?;
        Self::parse(text, source)
    }

    /// The master keys the environment gives: [`MASTER_KEY_VAR`]'s, or those of the file
    /// [`MASTER_KEY_FILE_VAR`] names; none if neither is set (or both are empty). Both set is
    /// refused. What was read of them is zeroed once parsed (the environment's own copy stays).
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_vars(&|name| std::env::var_os(name))
    }

    /// cladefs: the same, of the variables `var` gives by name (TASK-633.6): a bucket's own
    /// (`bucket_cell::object_bucket::numbered_options`).
    pub fn from_vars(var: &dyn Fn(&str) -> Option<OsString>) -> Result<Option<Self>, String> {
        let var = |name| var(name).filter(|value| !value.is_empty());
        match (var(MASTER_KEY_VAR), var(MASTER_KEY_FILE_VAR)) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(format!(
                "both {MASTER_KEY_VAR} and {MASTER_KEY_FILE_VAR} are set: give the master keys \
                 one way"
            )),
            (Some(value), None) => {
                let mut text = value.into_string().map_err(|value| {
                    value.into_encoded_bytes().zeroize();
                    format!("{MASTER_KEY_VAR}: not text")
                })?;
                let keys = Self::parse(&text, MASTER_KEY_VAR);
                text.zeroize();
                keys.map(Some)
            }
            (None, Some(path)) => {
                let source = format!("{MASTER_KEY_FILE_VAR} {}", path.to_string_lossy());
                let mut bytes = std::fs::read(&path).map_err(|err| format!("{source}: {err}"))?;
                let keys = Self::of_file(&bytes, &source);
                bytes.zeroize();
                keys.map(Some)
            }
        }
    }

    /// The newest: what wraps.
    pub fn newest(&self) -> &MasterKey {
        &self.0[0]
    }

    /// The key of ID `id`, if it is one of these.
    pub fn get(&self, id: MasterKeyId) -> Option<&MasterKey> {
        self.0.iter().find(|key| key.id == id)
    }

    /// Their IDs, the newest first.
    pub fn ids(&self) -> Vec<MasterKeyId> {
        self.0.iter().map(|key| key.id).collect()
    }

    /// Their IDs, as refusals say them: comma-separated, the newest first.
    pub fn named_ids(&self) -> String {
        self.ids().iter().map(MasterKeyId::to_string).collect::<Vec<_>>().join(", ")
    }
}

impl std::fmt::Debug for MasterKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MasterKeys").field(&self.ids()).finish()
    }
}

/// The key `word` holds in 64 hexadecimal digits, if it does; what was decoded zeroed.
fn hex_key(word: &str) -> Option<Secret> {
    let digits = word.as_bytes();
    if digits.len() != 2 * KEY_LEN {
        return None;
    }
    let mut key = [0; KEY_LEN];
    for (at, pair) in digits.as_chunks::<2>().0.iter().enumerate() {
        let nibble = |digit: u8| (digit as char).to_digit(16);
        match (nibble(pair[0]), nibble(pair[1])) {
            (Some(high), Some(low)) => key[at] = (high << 4 | low) as u8,
            _ => {
                key.zeroize();
                return None;
            }
        }
    }
    let secret = Secret::from_bytes(&key);
    key.zeroize();
    Some(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const TWO: &str = "ff0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1eFF";

    /// Refusals say which key, never what it holds.
    #[test]
    fn keys_not_of_64_hex_digits_are_refused_by_place() {
        let refused = |text: &str| MasterKeys::parse(text, "CLADEFS_MASTER_KEY").unwrap_err();
        assert_eq!(refused(""), "CLADEFS_MASTER_KEY: no key");
        assert_eq!(refused(" ,\n"), "CLADEFS_MASTER_KEY: no key");
        let short = &ONE[..63];
        assert_eq!(refused(short), "CLADEFS_MASTER_KEY: key 1 of 1: not 64 hexadecimal digits");
        let bad = format!("{}g", &ONE[..63]);
        assert_eq!(
            refused(&format!("{TWO},{bad}")),
            "CLADEFS_MASTER_KEY: key 2 of 2: not 64 hexadecimal digits"
        );
        assert_eq!(
            refused(&format!("{ONE},{TWO},{ONE}")),
            "CLADEFS_MASTER_KEY: key 3 is key 1 again (8b1917a7f2bf1306)"
        );
        for refusal in [refused(short), refused(&bad)] {
            assert!(!refusal.contains(&ONE[..16]), "{refusal}");
        }
    }
}
