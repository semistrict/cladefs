// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the bucket KMS's keys and formats (TASK-411.27, doc-16, The bucket KMS). Each
//! tenant's KEK is a random key ([`TenantKek`]) kept in the bucket itself, in an object of its own
//! ([`KekObject`]), wrapped by a master key ([`super::master`]); it wraps the keys asked of it
//! ([`wrap`]) as [`super::LocalKms`] does. Where the objects are kept, and how hosts racing to make
//! one agree, is `extent_remote::bucket_kms`'s; what they hold, and the keys, are here, with every
//! other secret.
//!
//! **A KEK object** (`tenants/<tenant>/kek`): [`OBJECT_MAGIC`], [`FORMAT`], its kind, the tenant
//! (big-endian), then, of a KEK, the KEK's ID (random, [`KEK_ID_LEN`] bytes), the master key's ID,
//! and the KEK wrapped with AES-KW under a key HKDF-SHA256 derives from the master key with all of
//! that as info: so bound to the tenant and the master key's ID, a KEK object copied to another
//! tenant, or relabelled another master key, fails AES-KW's check. A tombstone, a shredded tenant's,
//! is the header alone, kind [`TOMBSTONE`]: the tenant had a KEK, now destroyed, never made again.
//!
//! **A key it wraps:** [`WRAPPED_MAGIC`], [`FORMAT`], the tenant, the KEK's ID, then the key wrapped
//! with AES-KW under a key HKDF derives from the KEK with that header as info ([`WRAPPED_LEN`] bytes
//! in all): the header names whose KEK, and which, and is bound to the key.

use super::Error;
use super::master::{MASTER_ID_LEN, MasterKey, MasterKeyId, MasterKeys};
use crate::{SALT_LEN, Salt, Secret, Unauthentic, Wrapped};

/// The start of a KEK object.
pub const OBJECT_MAGIC: &[u8; 4] = b"fxkt";

/// The start of a key the bucket KMS wrapped. [`super::LocalKms`]'s begin `fxkw`, the real
/// backends' `fxkr`.
pub const WRAPPED_MAGIC: &[u8; 4] = b"fxkb";

/// The format of both.
pub const FORMAT: u8 = 1;

/// A KEK object's kind: a KEK.
const KEK: u8 = 1;

/// A KEK object's kind: a tombstone.
const TOMBSTONE: u8 = 2;

/// The bytes of a KEK's ID.
pub const KEK_ID_LEN: usize = 8;

/// The bytes of a KEK object's header, the whole of a tombstone: magic, format, kind, tenant.
const OBJECT_HEADER_LEN: usize = OBJECT_MAGIC.len() + 1 + 1 + 8;

/// The bytes of a KEK object's KEK header: its header, the KEK's ID, the master key's ID.
const KEK_HEADER_LEN: usize = OBJECT_HEADER_LEN + KEK_ID_LEN + MASTER_ID_LEN;

/// The bytes of a KEK object of a KEK.
pub const KEK_OBJECT_LEN: usize = KEK_HEADER_LEN + crate::WRAPPED_LEN;

/// The bytes of a wrapped key's header: magic, format, tenant, KEK ID.
const WRAPPED_HEADER_LEN: usize = WRAPPED_MAGIC.len() + 1 + 8 + KEK_ID_LEN;

/// The bytes of a key the bucket KMS wrapped.
pub const WRAPPED_LEN: usize = WRAPPED_HEADER_LEN + crate::WRAPPED_LEN;

/// What HKDF derives the key a KEK object's KEK is wrapped under with, before its header.
const OBJECT_INFO: &[u8] = b"cladefs bucket kms kek";

/// What HKDF derives the key a wrapped key is wrapped under with, before its header.
const WRAP_INFO: &[u8] = b"cladefs bucket kms wrap";

/// Which of a tenant's KEKs: random, drawn with it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KekId(pub [u8; KEK_ID_LEN]);

impl std::fmt::Display for KekId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl std::fmt::Debug for KekId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KekId({self})")
    }
}

/// A tenant's KEK, unwrapped: zeroed when dropped, never printed.
pub struct TenantKek {
    pub id: KekId,
    key: Secret,
}

impl TenantKek {
    /// A new KEK, and its ID, from the system's random generator.
    pub fn random() -> Self {
        let mut id = [0; KEK_ID_LEN];
        aws_lc_rs::rand::fill(&mut id).expect("the system's random generator fills an ID");
        Self { id: KekId(id), key: Secret::random() }
    }

    /// The KEK `key`, of ID `id`: tests' ([`crate::testing::KnownKms`]).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn of(id: KekId, key: Secret) -> Self {
        Self { id, key }
    }
}

impl std::fmt::Debug for TenantKek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TenantKek({})", self.id)
    }
}

/// What a tenant's KEK object holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KekObject {
    /// Its KEK, wrapped by the master key `master`.
    Kek { tenant: u64, id: KekId, master: MasterKeyId, wrapped: Wrapped },
    /// Nothing: the tenant is shredded, its KEK destroyed.
    Tombstone { tenant: u64 },
}

impl KekObject {
    /// The object of `kek`, `tenant`'s, wrapped by `master`: its bytes.
    pub fn seal(master: &MasterKey, tenant: u64, kek: &TenantKek) -> Vec<u8> {
        let mut header = object_header(KEK, tenant);
        header.extend_from_slice(&kek.id.0);
        header.extend_from_slice(&master.id().0);
        let wrapped = object_key(master, &header).wrap(&kek.key);
        [header, wrapped.0].concat()
    }

    /// A tombstone of `tenant`'s: its bytes.
    pub fn tombstone(tenant: u64) -> Vec<u8> {
        object_header(TOMBSTONE, tenant)
    }

    /// What `bytes`, the object of `tenant`'s KEK, hold: refused, naming the tenant, if they are
    /// no KEK object of its.
    pub fn decode(tenant: u64, bytes: &[u8]) -> Result<Self, Error> {
        let unauthentic = |why| Error::Unauthentic { tenant, source: Unauthentic(why) };
        let header = || unauthentic("its KEK object, by its header");
        let rest = bytes.strip_prefix(OBJECT_MAGIC).ok_or_else(header)?;
        let (&format, rest) = rest.split_first().ok_or_else(header)?;
        if format != FORMAT {
            return Err(unauthentic("its KEK object, of another format"));
        }
        let (&kind, rest) = rest.split_first().ok_or_else(header)?;
        let (of, rest) = rest.split_first_chunk::<8>().ok_or_else(header)?;
        if u64::from_be_bytes(*of) != tenant {
            return Err(unauthentic("its KEK object, another tenant's"));
        }
        match kind {
            TOMBSTONE if rest.is_empty() => Ok(Self::Tombstone { tenant }),
            KEK => {
                let (id, rest) = rest.split_first_chunk::<KEK_ID_LEN>().ok_or_else(header)?;
                let (master, wrapped) =
                    rest.split_first_chunk::<MASTER_ID_LEN>().ok_or_else(header)?;
                if wrapped.len() != crate::WRAPPED_LEN {
                    return Err(header());
                }
                let (id, master) = (KekId(*id), MasterKeyId(*master));
                Ok(Self::Kek { tenant, id, master, wrapped: Wrapped(wrapped.to_vec()) })
            }
            _ => Err(header()),
        }
    }

    /// Its KEK, unwrapped by whichever of `keys` wrapped it: refused, naming the master key's ID
    /// and those of `keys`, if none of them did; and as [`Error::Destroyed`] if it is a tombstone.
    pub fn open(&self, keys: &MasterKeys) -> Result<TenantKek, Error> {
        let (tenant, id, master, wrapped) = match self {
            Self::Tombstone { tenant } => return Err(Error::Destroyed { tenant: *tenant }),
            Self::Kek { tenant, id, master, wrapped } => (*tenant, *id, *master, wrapped),
        };
        let Some(key) = keys.get(master) else {
            return Err(Error::UnknownMasterKey {
                tenant,
                id: master.to_string(),
                held: keys.named_ids(),
            });
        };
        let mut header = object_header(KEK, tenant);
        header.extend_from_slice(&id.0);
        header.extend_from_slice(&master.0);
        let kek = object_key(key, &header).unwrap(wrapped);
        let kek = kek.map_err(|source| Error::Unauthentic { tenant, source })?;
        Ok(TenantKek { id, key: kek })
    }

    /// Its bytes wrapped by the newest of `keys`, if another wrapped it: what a rotation writes.
    /// None if the newest wrapped it, or it is a tombstone.
    pub fn resealed(&self, keys: &MasterKeys) -> Result<Option<Vec<u8>>, Error> {
        match self {
            Self::Kek { master, .. } if *master != keys.newest().id() => {
                let kek = self.open(keys)?;
                Ok(Some(Self::seal(keys.newest(), self.tenant(), &kek)))
            }
            _ => Ok(None),
        }
    }

    pub fn tenant(&self) -> u64 {
        match self {
            Self::Kek { tenant, .. } | Self::Tombstone { tenant } => *tenant,
        }
    }
}

/// `key`, wrapped by `kek`, `tenant`'s KEK.
pub fn wrap(tenant: u64, kek: &TenantKek, key: &Secret) -> Wrapped {
    let header = wrapped_header(tenant, kek.id);
    let wrapped = wrapping_key(&kek.key, &header).wrap(key);
    Wrapped([&header[..], &wrapped.0].concat())
}

/// Whose KEK wrapped `wrapped`, and which, as its header says: the header alone is read.
pub fn kek_of(wrapped: &Wrapped) -> Result<(u64, KekId), Unauthentic> {
    let bytes = &wrapped.0;
    if bytes.len() != WRAPPED_LEN || !bytes.starts_with(WRAPPED_MAGIC) {
        return Err(Unauthentic("a wrapped key, by its header"));
    }
    if bytes[WRAPPED_MAGIC.len()] != FORMAT {
        return Err(Unauthentic("a wrapped key, of another format"));
    }
    let at = WRAPPED_MAGIC.len() + 1;
    let tenant = u64::from_be_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    let id = KekId(bytes[at + 8..WRAPPED_HEADER_LEN].try_into().expect("an ID's bytes"));
    Ok((tenant, id))
}

/// The key `wrapped` holds, if `kek`, `tenant`'s, wrapped it: refused if its header names another
/// tenant or KEK, or it fails AES-KW's check.
pub fn unwrap(tenant: u64, kek: &TenantKek, wrapped: &Wrapped) -> Result<Secret, Unauthentic> {
    let (of, id) = kek_of(wrapped)?;
    if of != tenant {
        return Err(Unauthentic("a wrapped key, wrapped for another tenant"));
    }
    if id != kek.id {
        return Err(Unauthentic("a wrapped key, wrapped by another KEK of the tenant's"));
    }
    let (header, key) = wrapped.0.split_at(WRAPPED_HEADER_LEN);
    wrapping_key(&kek.key, header).unwrap(&Wrapped(key.to_vec()))
}

fn object_header(kind: u8, tenant: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(KEK_OBJECT_LEN);
    header.extend_from_slice(OBJECT_MAGIC);
    header.push(FORMAT);
    header.push(kind);
    header.extend_from_slice(&tenant.to_be_bytes());
    header
}

fn wrapped_header(tenant: u64, id: KekId) -> [u8; WRAPPED_HEADER_LEN] {
    let mut header = [0; WRAPPED_HEADER_LEN];
    header[..WRAPPED_MAGIC.len()].copy_from_slice(WRAPPED_MAGIC);
    header[WRAPPED_MAGIC.len()] = FORMAT;
    let at = WRAPPED_MAGIC.len() + 1;
    header[at..at + 8].copy_from_slice(&tenant.to_be_bytes());
    header[at + 8..].copy_from_slice(&id.0);
    header
}

/// The key a KEK object's KEK is wrapped under, from `master` and the object's header. Both keys
/// are random, so HKDF's salt adds nothing: zeros, as RFC 5869 has for none.
fn object_key(master: &MasterKey, header: &[u8]) -> Secret {
    master.key().derive(&Salt([0; SALT_LEN]), &[OBJECT_INFO, header])
}

/// The key a key is wrapped under, from the tenant's KEK and the wrapped key's header.
fn wrapping_key(kek: &Secret, header: &[u8]) -> Secret {
    kek.derive(&Salt([0; SALT_LEN]), &[WRAP_INFO, header])
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    fn keys(count: usize) -> MasterKeys {
        MasterKeys::new((0..count).map(|_| Secret::random()).collect()).unwrap()
    }

    fn refusal<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn kek_objects_open_under_the_master_key_that_sealed_them() {
        let keys = keys(1);
        let kek = TenantKek::random();
        let bytes = KekObject::seal(keys.newest(), 7, &kek);
        assert_eq!(bytes.len(), KEK_OBJECT_LEN);
        assert_eq!(&bytes[..14], b"fxkt\x01\x01\0\0\0\0\0\0\0\x07");
        assert_eq!(&bytes[14..22], &kek.id.0);
        assert_eq!(&bytes[22..30], &keys.newest().id().0);
        let object = KekObject::decode(7, &bytes).unwrap();
        assert_matches!(&object, KekObject::Kek { tenant: 7, id, master, .. }
            if *id == kek.id && *master == keys.newest().id());
        let opened = object.open(&keys).unwrap();
        assert_eq!((opened.id, opened.key.expose()), (kek.id, kek.key.expose()));
        assert!(!bytes.windows(32).any(|window| window == kek.key.expose()));
        assert!(!bytes.windows(32).any(|window| window == keys.newest().key().expose()));
    }

    /// AC #4: a KEK object wrapped by a master key the host has not is refused naming its ID and
    /// the host's, not taken for a tenant with no KEK.
    #[test]
    fn kek_objects_of_other_master_keys_are_refused_naming_them() {
        let (theirs, ours) = (keys(1), keys(2));
        let object =
            KekObject::decode(7, &KekObject::seal(theirs.newest(), 7, &TenantKek::random()));
        let refused = object.unwrap().open(&ours);
        let expected = format!(
            "tenant 7: its KEK is wrapped by master key {}, which this host has not (it has {}): \
             CLADEFS_MASTER_KEY",
            theirs.newest().id(),
            ours.named_ids()
        );
        assert_eq!(refusal(refused), expected);
    }

    #[test]
    fn kek_objects_tampered_or_moved_are_refused() {
        let keys = keys(1);
        let bytes = KekObject::seal(keys.newest(), 7, &TenantKek::random());
        let opened = |bytes: &[u8], tenant| KekObject::decode(tenant, bytes)?.open(&keys);
        assert_eq!(
            refusal(opened(&bytes, 8)),
            "tenant 8: not authentic: its KEK object, another tenant's"
        );
        // Relabelled as tenant 8's, it fails AES-KW's check: the tenant is bound, not only read.
        let mut moved = bytes.clone();
        moved[6..14].copy_from_slice(&8_u64.to_be_bytes());
        assert_eq!(
            refusal(opened(&moved, 8)),
            "tenant 8: not authentic: a wrapped key, by its integrity check"
        );
        let mut relabelled = bytes.clone();
        relabelled[14] ^= 1;
        assert_eq!(
            refusal(opened(&relabelled, 7)),
            "tenant 7: not authentic: a wrapped key, by its integrity check"
        );
        let mut format = bytes.clone();
        format[4] = 2;
        assert_eq!(
            refusal(opened(&format, 7)),
            "tenant 7: not authentic: its KEK object, of another format"
        );
        for len in 0..bytes.len() {
            assert_eq!(
                refusal(opened(&bytes[..len], 7)),
                "tenant 7: not authentic: its KEK object, by its header",
                "{len} bytes"
            );
        }
    }

    #[test]
    fn tombstones_are_destroyed_kek_objects() {
        let bytes = KekObject::tombstone(7);
        assert_eq!(bytes, b"fxkt\x01\x02\0\0\0\0\0\0\0\x07");
        let object = KekObject::decode(7, &bytes).unwrap();
        assert_eq!(object, KekObject::Tombstone { tenant: 7 });
        assert_eq!(refusal(object.open(&keys(1))), "tenant 7: its KEK is destroyed");
        assert_eq!(object.resealed(&keys(1)).unwrap(), None);
        let longer = [&bytes[..], b"x"].concat();
        assert_eq!(
            refusal(KekObject::decode(7, &longer)),
            "tenant 7: not authentic: its KEK object, by its header"
        );
    }

    /// AC #2: an object of an older master key is resealed under the newest, the KEK the same;
    /// one of the newest is left as it is.
    #[test]
    fn rotations_reseal_under_the_newest_master_key() {
        let old = MasterKeys::new(vec![Secret::random()]).unwrap();
        let old_key = Secret::from_bytes(old.newest().key().expose());
        let keys = MasterKeys::new(vec![Secret::random(), old_key]).unwrap();
        let kek = TenantKek::random();
        let object = KekObject::decode(7, &KekObject::seal(old.newest(), 7, &kek)).unwrap();
        let resealed = object.resealed(&keys).unwrap().unwrap();
        let resealed = KekObject::decode(7, &resealed).unwrap();
        assert_matches!(&resealed, KekObject::Kek { master, .. } if *master == keys.newest().id());
        let newest = MasterKeys::new(vec![Secret::from_bytes(keys.newest().key().expose())]);
        let opened = resealed.open(&newest.unwrap()).unwrap();
        assert_eq!((opened.id, opened.key.expose()), (kek.id, kek.key.expose()));
        assert_eq!(resealed.resealed(&keys).unwrap(), None);
    }

    #[test]
    fn wrapped_keys_unwrap_under_their_tenants_kek() {
        let (kek, key) = (TenantKek::random(), Secret::random());
        let wrapped = wrap(7, &kek, &key);
        assert_eq!(wrapped.0.len(), WRAPPED_LEN);
        assert_eq!(&wrapped.0[..13], b"fxkb\x01\0\0\0\0\0\0\0\x07");
        assert_eq!(kek_of(&wrapped), Ok((7, kek.id)));
        assert_eq!(unwrap(7, &kek, &wrapped).unwrap().expose(), key.expose());
        let refused = |tenant, kek: &TenantKek, wrapped: &Wrapped| {
            unwrap(tenant, kek, wrapped).map(|_| ()).unwrap_err().0
        };
        assert_eq!(refused(8, &kek, &wrapped), "a wrapped key, wrapped for another tenant");
        let other = TenantKek::random();
        assert_eq!(
            refused(7, &other, &wrapped),
            "a wrapped key, wrapped by another KEK of the tenant's"
        );
        let mut relabelled = wrapped.clone();
        relabelled.0[5..13].copy_from_slice(&8_u64.to_be_bytes());
        assert_eq!(refused(8, &kek, &relabelled), "a wrapped key, by its integrity check");
        let mut tampered = wrapped.clone();
        tampered.0[WRAPPED_HEADER_LEN + 3] ^= 1;
        assert_eq!(refused(7, &kek, &tampered), "a wrapped key, by its integrity check");
        let local = Wrapped([&b"fxkw\x01"[..], &[0; 52]].concat());
        assert_eq!(refused(7, &kek, &local), "a wrapped key, by its header");
        let mut format = wrapped.clone();
        format.0[4] = 2;
        assert_eq!(refused(7, &kek, &format), "a wrapped key, of another format");
    }
}
