// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a real KMS's backend keeps of a key it wrapped, the same for each (TASK-411.16,
//! .17, .18): a header naming the backend and the key that wrapped it, then what the KMS gave
//! back. The header lets an unwrap refuse a key another backend wrapped, or another key, as not
//! authentic, before asking the KMS anything (TASK-411.18 AC #3); the KMS's own ciphertext, bound
//! to its tenant by the backend's associated data, is what makes it authentic.

use crate::{Unauthentic, Wrapped};

/// The start of what a real KMS's backend wraps: its own, then the format of the rest. Not
/// [`super::LocalKms`]'s (`fxkw`).
const MAGIC: &[u8; 4] = b"fxkr";

/// The format of the rest.
const FORMAT: u8 = 1;

/// `ciphertext`, what backend `backend` (its URL's scheme) had of the KMS for a key wrapped under
/// the key named `key`, with the header saying so: [`MAGIC`], [`FORMAT`], the backend's length (a
/// byte) and name, the key's name's length (big-endian, two bytes) and name, then `ciphertext`.
pub(super) fn wrapped(backend: &str, key: &str, ciphertext: &[u8]) -> Wrapped {
    let backend_len = u8::try_from(backend.len()).expect("a backend's name of under 256 bytes");
    let key_len = u16::try_from(key.len()).expect("a key's name of under 64 KiB");
    let mut bytes =
        Vec::with_capacity(MAGIC.len() + 4 + backend.len() + key.len() + ciphertext.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(FORMAT);
    bytes.push(backend_len);
    bytes.extend_from_slice(backend.as_bytes());
    bytes.extend_from_slice(&key_len.to_be_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(ciphertext);
    Wrapped(bytes)
}

/// The KMS's ciphertext in `wrapped`, if its header says backend `backend` wrapped it under the
/// key named `key`. The header alone is read: whether the ciphertext is authentic only the KMS
/// tells.
pub(super) fn ciphertext<'a>(
    wrapped: &'a Wrapped,
    backend: &str,
    key: &str,
) -> Result<&'a [u8], Unauthentic> {
    let header = Unauthentic("a wrapped key, by its header");
    let rest = wrapped.0.strip_prefix(MAGIC).ok_or(header)?;
    let (&format, rest) = rest.split_first().ok_or(header)?;
    if format != FORMAT {
        return Err(Unauthentic("a wrapped key, of another format"));
    }
    let (&backend_len, rest) = rest.split_first().ok_or(header)?;
    let (named, rest) = rest.split_at_checked(backend_len.into()).ok_or(header)?;
    if named != backend.as_bytes() {
        return Err(Unauthentic("a wrapped key, wrapped by another KMS"));
    }
    let (key_len, rest) = rest.split_first_chunk::<2>().ok_or(header)?;
    let (named, ciphertext) =
        rest.split_at_checked(u16::from_be_bytes(*key_len).into()).ok_or(header)?;
    if named != key.as_bytes() {
        return Err(Unauthentic("a wrapped key, wrapped under another key"));
    }
    Ok(ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_name_the_backend_and_the_key() {
        let wrapped = wrapped("gcp-kms", "projects/p/k", b"ciphertext");
        assert_eq!(&wrapped.0[..], b"fxkr\x01\x07gcp-kms\x00\x0cprojects/p/kciphertext");
        assert_eq!(ciphertext(&wrapped, "gcp-kms", "projects/p/k"), Ok(&b"ciphertext"[..]));
    }

    #[test]
    fn keys_of_other_backends_and_keys_are_refused() {
        let wrapped = wrapped("gcp-kms", "k", b"ciphertext");
        let refused = |backend, key| ciphertext(&wrapped, backend, key).unwrap_err().0;
        assert_eq!(refused("aws-kms", "k"), "a wrapped key, wrapped by another KMS");
        assert_eq!(refused("gcp-kms", "j"), "a wrapped key, wrapped under another key");
        assert_eq!(refused("gcp-kms", "kk"), "a wrapped key, wrapped under another key");
        let mut other_format = wrapped.clone();
        other_format.0[4] = 2;
        let refused = ciphertext(&other_format, "gcp-kms", "k").unwrap_err().0;
        assert_eq!(refused, "a wrapped key, of another format");
    }

    #[test]
    fn short_and_foreign_bytes_are_refused_by_their_header() {
        let whole = wrapped("gcp-kms", "key", b"");
        let local = Wrapped([&b"fxkw\x01"[..], &[0; 52]].concat());
        for bytes in (0..whole.0.len()).map(|len| Wrapped(whole.0[..len].to_vec())).chain([local]) {
            let refused = ciphertext(&bytes, "gcp-kms", "key").unwrap_err();
            assert_eq!(refused, Unauthentic("a wrapped key, by its header"), "{bytes:?}");
        }
        assert_eq!(ciphertext(&whole, "gcp-kms", "key"), Ok(&b""[..]));
    }
}
