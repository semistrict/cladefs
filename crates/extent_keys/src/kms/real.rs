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

/// The format of the rest, frozen at 1 (doc-28, board 10042).
pub const FORMAT: formats::Format = formats::Format::new("kms real", 1, 1);

/// `ciphertext`, what backend `backend` (its URL's scheme) had of the KMS for a key wrapped under
/// the key named `key`, with the header saying so: [`MAGIC`], [`FORMAT`], the backend's length (a
/// byte) and name, the key's name's length (big-endian, two bytes) and name, then `ciphertext`.
pub(super) fn wrapped(backend: &str, key: &str, ciphertext: &[u8]) -> Wrapped {
    let backend_len = u8::try_from(backend.len()).expect("a backend's name of under 256 bytes");
    let key_len = u16::try_from(key.len()).expect("a key's name of under 64 KiB");
    let mut bytes =
        Vec::with_capacity(MAGIC.len() + 4 + backend.len() + key.len() + ciphertext.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(FORMAT.current as u8);
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
    FORMAT.check(u32::from(format)).map_err(|_| Unauthentic("a wrapped key, of another format"))?;
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
    fn keys_of_other_backends_and_keys_are_refused() {
        let wrapped = wrapped("gcp-kms", "k", b"ciphertext");
        let refused = |backend, key| ciphertext(&wrapped, backend, key).unwrap_err().0;
        assert_eq!(refused("aws-kms", "k"), "a wrapped key, wrapped by another KMS");
        assert_eq!(refused("gcp-kms", "j"), "a wrapped key, wrapped under another key");
        assert_eq!(refused("gcp-kms", "kk"), "a wrapped key, wrapped under another key");
        for wrong in [0u8, FORMAT.current as u8 + 1] {
            let mut other_format = wrapped.clone();
            other_format.0[4] = wrong;
            let refused = ciphertext(&other_format, "gcp-kms", "k").unwrap_err().0;
            assert_eq!(refused, "a wrapped key, of another format");
        }
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<Real>(env!("CARGO_MANIFEST_DIR"));
    }

    /// Real KMS wrapped-key envelopes (`gcp-kms` and `azure-kv`), pinned against accidental change
    /// (doc-28, board 10042).
    struct Real;

    const GCP_KEY: &str = "projects/p/locations/l/keyRings/r/cryptoKeys/k-7";
    const AZURE_KEY: &str = "https://myvault.vault.azure.net/keys/k-7";

    impl formats::golden::Family for Real {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let gcp = wrapped("gcp-kms", GCP_KEY, b"gcp-ciphertext").0;
            // Azure stores a 1-byte version length, version string, then ciphertext inside `real`.
            let mut azure_body = vec![2u8];
            azure_body.extend_from_slice(b"v1");
            azure_body.extend_from_slice(b"azure-ciphertext");
            let azure = wrapped("azure-kv", AZURE_KEY, &azure_body).0;
            vec![("gcp", gcp), ("azure", azure)]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
            if version != 1 {
                return Err(format!("no decoder of version {version}"));
            }
            Self::decode(case, bytes)
        }

        fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
            Self::decode(case, bytes).map(drop)
        }
    }

    impl Real {
        fn decode(case: &str, bytes: &[u8]) -> Result<String, String> {
            let wrapped = Wrapped(bytes.to_vec());
            match case {
                "gcp" => {
                    let ct = ciphertext(&wrapped, "gcp-kms", GCP_KEY).map_err(|e| e.to_string())?;
                    Ok(format!(
                        "Real {{\n    backend: \"gcp-kms\",\n    key: {GCP_KEY:?},\n    ciphertext: {:?},\n}}",
                        ct.escape_ascii().to_string()
                    ))
                }
                "azure" => {
                    let body =
                        ciphertext(&wrapped, "azure-kv", AZURE_KEY).map_err(|e| e.to_string())?;
                    let (&vlen, rest) = body.split_first().ok_or("missing azure version len")?;
                    let (ver, ct) =
                        rest.split_at_checked(vlen.into()).ok_or("truncated azure version")?;
                    let ver = std::str::from_utf8(ver).map_err(|e| e.to_string())?;
                    Ok(format!(
                        "Real {{\n    backend: \"azure-kv\",\n    key: {AZURE_KEY:?},\n    version: {ver:?},\n    ciphertext: {:?},\n}}",
                        ct.escape_ascii().to_string()
                    ))
                }
                _ => Err(format!("no case {case}")),
            }
        }
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
