// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: [`LocalKms`], a KMS on the host's filesystem, a fake for tests and development
//! (TASK-411.1), with what a real KMS does out of band too: creating, rotating, revoking and
//! destroying tenants' KEKs.

use super::{Error, Kms};
use crate::{KEY_LEN, SALT_LEN, Salt, Secret, Unauthentic, WRAPPED_LEN, Wrapped};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zeroize::Zeroize as _;

/// The [`LocalKms`] `url`, `local:///DIR`, names ([`super::from_url`]).
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    let dir = url.strip_prefix("local://").expect("a local:// URL");
    if !dir.starts_with('/') {
        return Err(format!("{url:?} names no KMS: local:///DIR names an absolute directory"));
    }
    let kms = LocalKms::open(dir).map_err(|err| format!("the KMS at {url}: {err}"))?;
    Ok(std::sync::Arc::new(kms))
}

/// Which KEK wrapped a key [`LocalKms`] wrapped, as its header says: a tenant's, of a version.
/// Versions count from 1, a rotation adding the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KekVersion {
    pub tenant: u64,
    pub version: u32,
}

/// The start of what [`LocalKms`] wraps: its own, then the format of the rest.
const MAGIC: &[u8; 4] = b"fxkw";

/// The format of what [`LocalKms`] wraps.
const FORMAT: u8 = 1;

/// The bytes of a wrapped key's header: [`MAGIC`], [`FORMAT`], the tenant (big-endian), and the
/// version of its KEK (big-endian).
const HEADER_LEN: usize = MAGIC.len() + 1 + 8 + 4;

/// The bytes of a key [`LocalKms`] wrapped: its header, then the key wrapped.
pub const LOCAL_WRAPPED_LEN: usize = HEADER_LEN + WRAPPED_LEN;

/// What HKDF derives a KEK version's wrapping key with, before the header.
const WRAP_INFO: &[u8] = b"cladefs local kms wrap";

/// The suffix of a KEK version's file, after its version in decimal.
const KEK_SUFFIX: &str = ".kek";

/// The file whose presence says a tenant's KEK is revoked.
const REVOKED: &str = "revoked";

/// The file whose presence says a tenant's KEK is destroyed: kept, so that the tenant reads as
/// destroyed, not as never having had a KEK, and is never created again under keys still wrapped
/// for it.
const DESTROYED: &str = "destroyed";

/// A KMS on the host's filesystem: a fake, for tests and development, as `extent_remote`'s local
/// bucket is one of S3. Under its directory, each tenant has one of its own, named by its ID in
/// decimal, holding each version of its KEK, a file of [`KEY_LEN`] bytes named `<version>.kek`,
/// and the files [`REVOKED`] and [`DESTROYED`] when it is so. Its files are only its owner's to
/// read (0600, its directories 0700, on unix), and written durably (`fsync`), a version's whole or
/// not at all.
///
/// What it wraps is a header, [`MAGIC`], [`FORMAT`] (1), the tenant and the version of the KEK
/// (each big-endian), then the key wrapped with AES-KW (RFC 3394, [`Secret::wrap`]) under a key
/// HKDF-SHA256 derives from that version of the KEK with the header as info
/// ([`LOCAL_WRAPPED_LEN`] bytes in all). The header lets an unwrap find the version, and refuse a
/// key wrapped for another tenant by name; deriving the wrapping key from it binds it to the key,
/// so that one changed fails AES-KW's integrity check, under any KEK. AES-KW, not AES-GCM with the
/// header as associated data: it needs no nonce, where GCM's random 96-bit ones under one KEK
/// version would want counting.
///
/// Its I/O is a few small files, done on the calling thread: the futures it returns are ready.
/// Processes may share a directory: a rotation makes its version's file by a hard link, which
/// fails where another made it first, and checks after it that the tenant was not destroyed
/// meanwhile, which marks it so before deleting its files.
pub struct LocalKms {
    dir: PathBuf,
}

impl LocalKms {
    /// The KMS keeping its tenants' KEKs under `dir`, made if missing.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        dir_builder().recursive(true).create(&dir)?;
        Ok(Self { dir })
    }

    /// Makes `tenant`'s KEK, its version 1. It fails with [`Error::Exists`] if the tenant has one,
    /// or [`Error::Destroyed`] if it had one.
    pub fn create(&self, tenant: u64) -> Result<(), Error> {
        let dir = self.tenant_dir(tenant);
        match dir_builder().create(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(match exists(&dir.join(DESTROYED)).map_err(io_of(tenant))? {
                    true => Error::Destroyed { tenant },
                    false => Error::Exists { tenant },
                });
            }
            Err(error) => return Err(io_of(tenant)(error)),
        }
        sync_dir(&self.dir).map_err(io_of(tenant))?;
        self.add_version(tenant, 1)
    }

    /// Adds a version of `tenant`'s KEK, which wraps from then on, the others still unwrapping
    /// what they wrapped: its number.
    pub fn rotate(&self, tenant: u64) -> Result<u32, Error> {
        let mut version = self.newest(tenant)?.version;
        loop {
            version = version.checked_add(1).expect("fewer than 2^32 versions of a KEK");
            match self.add_version(tenant, version) {
                Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => {}
                added => return added.map(|()| version),
            }
        }
    }

    /// Revokes `tenant`'s KEK: it wraps and unwraps nothing ([`Error::Revoked`]) until
    /// [`LocalKms::unrevoke`]. Revoking it again is no error.
    pub fn revoke(&self, tenant: u64) -> Result<(), Error> {
        let dir = self.usable_dir(tenant, true)?;
        match create_file(&dir.join(REVOKED)) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => return Err(io_of(tenant)(error)),
        }
        sync_dir(&dir).map_err(io_of(tenant))
    }

    /// Gives back the use of `tenant`'s KEK, revoked or not.
    pub fn unrevoke(&self, tenant: u64) -> Result<(), Error> {
        let dir = self.usable_dir(tenant, true)?;
        match fs::remove_file(dir.join(REVOKED)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_of(tenant)(error)),
        }
        sync_dir(&dir).map_err(io_of(tenant))
    }

    /// Destroys `tenant`'s KEK, revoked or not: deletes every version of it, after which every
    /// call naming the tenant fails with [`Error::Destroyed`], and nothing wrapped under it
    /// unwraps again. It is as gone as the host's filesystem makes a deleted file; a real KMS
    /// destroys in its own hardware. Destroying it again is no error, and deletes any file a crash
    /// left.
    pub fn destroy(&self, tenant: u64) -> Result<(), Error> {
        let dir = self.tenant_dir(tenant);
        if !exists(&dir).map_err(io_of(tenant))? {
            return Err(Error::NoSuchTenant { tenant });
        }
        match create_file(&dir.join(DESTROYED)) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_of(tenant)(error)),
        }
        sync_dir(&dir).map_err(io_of(tenant))?;
        for version in versions(&dir).map_err(io_of(tenant))? {
            fs::remove_file(kek_path(&dir, version)).map_err(io_of(tenant))?;
        }
        sync_dir(&dir).map_err(io_of(tenant))
    }

    /// The versions of `tenant`'s KEK it keeps, in order.
    pub fn versions(&self, tenant: u64) -> Result<Vec<u32>, Error> {
        let dir = self.usable_dir(tenant, true)?;
        versions(&dir).map_err(io_of(tenant))
    }

    /// Which KEK wrapped `wrapped`, as its header says, whatever KMS directory wrapped it. The
    /// header alone is read: whether it is authentic only an unwrap tells.
    pub fn kek_of(wrapped: &Wrapped) -> Result<KekVersion, Unauthentic> {
        let bytes = &wrapped.0;
        if bytes.len() != LOCAL_WRAPPED_LEN || &bytes[..MAGIC.len()] != MAGIC {
            return Err(Unauthentic("a wrapped key, by its header"));
        }
        if bytes[MAGIC.len()] != FORMAT {
            return Err(Unauthentic("a wrapped key, of another format"));
        }
        let tenant = &bytes[MAGIC.len() + 1..MAGIC.len() + 9];
        let version = &bytes[MAGIC.len() + 9..HEADER_LEN];
        Ok(KekVersion {
            tenant: u64::from_be_bytes(tenant.try_into().expect("8 bytes")),
            version: u32::from_be_bytes(version.try_into().expect("4 bytes")),
        })
    }

    fn tenant_dir(&self, tenant: u64) -> PathBuf {
        self.dir.join(tenant.to_string())
    }

    /// `tenant`'s directory, if its KEK is neither destroyed nor, unless `allow_revoked`, revoked.
    fn usable_dir(&self, tenant: u64, allow_revoked: bool) -> Result<PathBuf, Error> {
        let dir = self.tenant_dir(tenant);
        if !exists(&dir).map_err(io_of(tenant))? {
            return Err(Error::NoSuchTenant { tenant });
        }
        if exists(&dir.join(DESTROYED)).map_err(io_of(tenant))? {
            return Err(Error::Destroyed { tenant });
        }
        if !allow_revoked && exists(&dir.join(REVOKED)).map_err(io_of(tenant))? {
            return Err(Error::Revoked { tenant });
        }
        Ok(dir)
    }

    /// The newest version of `tenant`'s KEK, usable, and its number.
    fn newest(&self, tenant: u64) -> Result<KekVersion, Error> {
        let dir = self.usable_dir(tenant, false)?;
        // A tenant whose directory has no version yet is one being created.
        let version = versions(&dir).map_err(io_of(tenant))?.pop();
        let version = version.ok_or(Error::NoSuchTenant { tenant })?;
        Ok(KekVersion { tenant, version })
    }

    /// Version `kek.version` of `kek.tenant`'s KEK, usable.
    fn kek(&self, kek: KekVersion) -> Result<Secret, Error> {
        let tenant = kek.tenant;
        let dir = self.usable_dir(tenant, false)?;
        match read_kek(&kek_path(&dir, kek.version)) {
            Ok(secret) => Ok(secret),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Destroyed meanwhile, or a version it never had.
                self.usable_dir(tenant, false)?;
                Err(Error::Unauthentic {
                    tenant,
                    source: Unauthentic("a wrapped key, of a KEK version the tenant has not"),
                })
            }
            Err(error) => Err(io_of(tenant)(error)),
        }
    }

    /// Makes version `version` of `tenant`'s KEK, failing with an [`io::ErrorKind::AlreadyExists`]
    /// if another made it first.
    fn add_version(&self, tenant: u64, version: u32) -> Result<(), Error> {
        static STAGED: AtomicU64 = AtomicU64::new(0);
        let dir = self.tenant_dir(tenant);
        let staged = dir.join(format!(
            ".{version}.{}.{}.staged",
            std::process::id(),
            STAGED.fetch_add(1, Ordering::Relaxed)
        ));
        let kek = Secret::random();
        let written = create_file(&staged).and_then(|mut file| {
            file.write_all(kek.expose())?;
            file.sync_all()
        });
        let linked = written.and_then(|()| fs::hard_link(&staged, kek_path(&dir, version)));
        let removed = fs::remove_file(&staged);
        linked.and(removed).and_then(|()| sync_dir(&dir)).map_err(io_of(tenant))?;
        // A destroy that listed the versions before this one was linked marked the tenant
        // destroyed first: this deletes what it could not.
        if exists(&dir.join(DESTROYED)).map_err(io_of(tenant))? {
            fs::remove_file(kek_path(&dir, version)).map_err(io_of(tenant))?;
            return Err(Error::Destroyed { tenant });
        }
        Ok(())
    }

    fn wrap_now(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let kek_version = self.newest(tenant)?;
        let header = header(kek_version);
        let wrapped = wrapping_key(&self.kek(kek_version)?, &header).wrap(key);
        Ok(Wrapped([&header[..], &wrapped.0].concat()))
    }

    fn unwrap_now(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let unauthentic = |source| Error::Unauthentic { tenant, source };
        // Refusals of the tenant's KEK come first, whatever is asked of it.
        self.usable_dir(tenant, false)?;
        let kek_version = Self::kek_of(wrapped).map_err(unauthentic)?;
        if kek_version.tenant != tenant {
            return Err(unauthentic(Unauthentic("a wrapped key, wrapped for another tenant")));
        }
        let (header, key) = wrapped.0.split_at(HEADER_LEN);
        let key = Wrapped(key.to_vec());
        wrapping_key(&self.kek(kek_version)?, header).unwrap(&key).map_err(unauthentic)
    }
}

impl Kms for LocalKms {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        std::future::ready(self.wrap_now(tenant, key))
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        std::future::ready(self.unwrap_now(tenant, wrapped))
    }
}

/// The header of a key wrapped under `kek` (see [`LocalKms`]).
fn header(kek: KekVersion) -> [u8; HEADER_LEN] {
    let mut header = [0; HEADER_LEN];
    header[..MAGIC.len()].copy_from_slice(MAGIC);
    header[MAGIC.len()] = FORMAT;
    header[MAGIC.len() + 1..MAGIC.len() + 9].copy_from_slice(&kek.tenant.to_be_bytes());
    header[MAGIC.len() + 9..].copy_from_slice(&kek.version.to_be_bytes());
    header
}

/// The key AES-KW wraps under for `header`, from the KEK version it names. The KEK is random, so
/// HKDF's salt adds nothing: zeros, as RFC 5869 has for none.
fn wrapping_key(kek: &Secret, header: &[u8]) -> Secret {
    kek.derive(&Salt([0; SALT_LEN]), &[WRAP_INFO, header])
}

fn kek_path(dir: &Path, version: u32) -> PathBuf {
    dir.join(format!("{version}{KEK_SUFFIX}"))
}

/// The versions of the KEK whose files are in `dir`, in order.
fn versions(dir: &Path) -> io::Result<Vec<u32>> {
    let mut versions = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let version = name.to_str().and_then(|name| name.strip_suffix(KEK_SUFFIX));
        if let Some(version) = version.and_then(|version| version.parse().ok()) {
            versions.push(version);
        }
    }
    versions.sort_unstable();
    Ok(versions)
}

/// The KEK `path` holds, its bytes read zeroed once copied.
fn read_kek(path: &Path) -> io::Result<Secret> {
    let mut bytes = fs::read(path)?;
    let kek = <&[u8; KEY_LEN]>::try_from(&bytes[..]).map(Secret::from_bytes);
    let len = bytes.len();
    bytes.zeroize();
    kek.map_err(|_| {
        let message = format!("{}: a KEK of {len} bytes, not {KEY_LEN}", path.display());
        io::Error::new(io::ErrorKind::InvalidData, message)
    })
}

/// A new file at `path`, only its owner's to read and write.
fn create_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// What makes directories only their owner's to list and enter.
fn dir_builder() -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}

/// Makes the entries of `dir` made or removed durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

fn exists(path: &Path) -> io::Result<bool> {
    path.try_exists()
}

fn io_of(tenant: u64) -> impl Fn(io::Error) -> Error {
    move |source| Error::Io { tenant, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Counted;
    use assert_matches::assert_matches;
    use futures::executor::block_on;

    fn kms() -> (tempfile::TempDir, LocalKms) {
        let dir = tempfile::tempdir().unwrap();
        let kms = LocalKms::open(dir.path().join("kms")).unwrap();
        (dir, kms)
    }

    fn wrap(kms: &impl Kms, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        block_on(kms.wrap(tenant, key))
    }

    fn unwrap(kms: &impl Kms, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        block_on(kms.unwrap(tenant, wrapped))
    }

    /// The error's text: each names its tenant, which is what callers report.
    fn refusal<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn wrapped_keys_unwrap_for_their_tenant() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        assert_eq!(wrapped.0.len(), LOCAL_WRAPPED_LEN);
        assert_eq!(LocalKms::kek_of(&wrapped), Ok(KekVersion { tenant: 7, version: 1 }));
        assert_eq!(&wrapped.0[..HEADER_LEN], b"fxkw\x01\0\0\0\0\0\0\0\x07\0\0\0\x01");
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        // A KMS on the same directory, as another process opens it, unwraps it as well.
        let again = LocalKms::open(&kms.dir).unwrap();
        assert_eq!(unwrap(&again, 7, &wrapped).unwrap().expose(), key.expose());
    }

    #[test]
    fn keys_wrapped_for_another_tenant_are_refused() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        kms.create(8).unwrap();
        let wrapped = wrap(&kms, 7, &Secret::random()).unwrap();
        let refused = unwrap(&kms, 8, &wrapped);
        assert_matches!(
            refused,
            Err(Error::Unauthentic { tenant: 8, source: Unauthentic(why) })
                if why == "a wrapped key, wrapped for another tenant"
        );
        assert_eq!(
            refusal(unwrap(&kms, 8, &wrapped)),
            "tenant 8: not authentic: a wrapped key, wrapped for another tenant"
        );
        // Relabelled as tenant 8's, it fails AES-KW's check under 8's KEK.
        let mut relabelled = wrapped.clone();
        relabelled.0[MAGIC.len() + 1..MAGIC.len() + 9].copy_from_slice(&8_u64.to_be_bytes());
        assert_eq!(
            refusal(unwrap(&kms, 8, &relabelled)),
            "tenant 8: not authentic: a wrapped key, by its integrity check"
        );
    }

    #[test]
    fn tampered_keys_are_refused() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let wrapped = wrap(&kms, 7, &Secret::random()).unwrap();
        let tampered = |at: usize| {
            let mut tampered = wrapped.clone();
            tampered.0[at] ^= 1;
            refusal(unwrap(&kms, 7, &tampered))
        };
        assert_eq!(tampered(0), "tenant 7: not authentic: a wrapped key, by its header");
        assert_eq!(
            tampered(MAGIC.len()),
            "tenant 7: not authentic: a wrapped key, of another format"
        );
        assert_eq!(
            tampered(HEADER_LEN + 9),
            "tenant 7: not authentic: a wrapped key, by its integrity check"
        );
        assert_eq!(
            tampered(HEADER_LEN - 1),
            "tenant 7: not authentic: a wrapped key, of a KEK version the tenant has not"
        );
        let short = Wrapped(wrapped.0[..LOCAL_WRAPPED_LEN - 1].to_vec());
        assert_eq!(
            refusal(unwrap(&kms, 7, &short)),
            "tenant 7: not authentic: a wrapped key, by its header"
        );
        // A key wrapped by the bare KEK, as `Secret::wrap` wraps, is not the KMS's either.
        let bare = Secret::random().wrap(&Secret::random());
        assert_eq!(
            refusal(unwrap(&kms, 7, &bare)),
            "tenant 7: not authentic: a wrapped key, by its header"
        );
    }

    /// A version's header changed to name another version the tenant has fails AES-KW's check:
    /// the header is bound to the key, not only read. Version 2 here holds version 1's KEK, so
    /// that only the binding refuses it.
    #[test]
    fn keys_relabelled_to_another_version_are_refused() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let wrapped = wrap(&kms, 7, &Secret::random()).unwrap();
        assert_eq!(kms.rotate(7).unwrap(), 2);
        let tenant = kms.tenant_dir(7);
        fs::copy(kek_path(&tenant, 1), kek_path(&tenant, 2)).unwrap();
        let mut relabelled = wrapped.clone();
        relabelled.0[HEADER_LEN - 4..HEADER_LEN].copy_from_slice(&2_u32.to_be_bytes());
        assert_eq!(
            refusal(unwrap(&kms, 7, &relabelled)),
            "tenant 7: not authentic: a wrapped key, by its integrity check"
        );
    }

    #[test]
    fn rotation_wraps_with_the_new_version_and_old_ones_still_unwrap() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let (old_key, new_key) = (Secret::random(), Secret::random());
        let old = wrap(&kms, 7, &old_key).unwrap();
        assert_eq!(kms.rotate(7).unwrap(), 2);
        assert_eq!(kms.rotate(7).unwrap(), 3);
        assert_eq!(kms.versions(7).unwrap(), [1, 2, 3]);
        let new = wrap(&kms, 7, &new_key).unwrap();
        assert_eq!(LocalKms::kek_of(&old), Ok(KekVersion { tenant: 7, version: 1 }));
        assert_eq!(LocalKms::kek_of(&new), Ok(KekVersion { tenant: 7, version: 3 }));
        assert_eq!(unwrap(&kms, 7, &old).unwrap().expose(), old_key.expose());
        assert_eq!(unwrap(&kms, 7, &new).unwrap().expose(), new_key.expose());
    }

    /// A rotation racing another, as two processes', takes the next version free.
    #[test]
    fn rotation_takes_the_next_version_free() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        kms.add_version(7, 2).unwrap();
        assert_matches!(
            kms.add_version(7, 2),
            Err(Error::Io { tenant: 7, source }) if source.kind() == io::ErrorKind::AlreadyExists
        );
        assert_eq!(kms.rotate(7).unwrap(), 3);
        assert_eq!(kms.versions(7).unwrap(), [1, 2, 3]);
        let staged: Vec<_> = fs::read_dir(kms.tenant_dir(7))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".staged"))
            .collect();
        assert_eq!(staged, Vec::<String>::new());
    }

    #[test]
    fn revoked_keks_wrap_and_unwrap_nothing_until_unrevoked() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        kms.revoke(7).unwrap();
        kms.revoke(7).unwrap();
        assert_matches!(unwrap(&kms, 7, &wrapped), Err(Error::Revoked { tenant: 7 }));
        assert_eq!(refusal(unwrap(&kms, 7, &wrapped)), "tenant 7: its KEK is revoked");
        assert_eq!(refusal(wrap(&kms, 7, &key)), "tenant 7: its KEK is revoked");
        assert_eq!(refusal(kms.rotate(7)), "tenant 7: its KEK is revoked");
        kms.unrevoke(7).unwrap();
        kms.unrevoke(7).unwrap();
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        let again = wrap(&kms, 7, &key).unwrap();
        assert_eq!(unwrap(&kms, 7, &again).unwrap().expose(), key.expose());
    }

    #[test]
    fn destroyed_keks_are_gone_for_good() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        kms.create(8).unwrap();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        let others = wrap(&kms, 8, &key).unwrap();
        kms.rotate(7).unwrap();
        kms.revoke(7).unwrap();
        kms.destroy(7).unwrap();
        kms.destroy(7).unwrap();

        assert_matches!(unwrap(&kms, 7, &wrapped), Err(Error::Destroyed { tenant: 7 }));
        assert_eq!(refusal(unwrap(&kms, 7, &wrapped)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(wrap(&kms, 7, &key)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(kms.rotate(7)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(kms.unrevoke(7)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(kms.revoke(7)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(kms.versions(7)), "tenant 7: its KEK is destroyed");
        assert_eq!(refusal(kms.create(7)), "tenant 7: its KEK is destroyed");
        // No KEK material is left: the tenant's directory holds its marks alone.
        let mut left: Vec<_> = fs::read_dir(kms.tenant_dir(7))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [DESTROYED, REVOKED]);
        // Another tenant's keys are untouched.
        assert_eq!(unwrap(&kms, 8, &others).unwrap().expose(), key.expose());
    }

    /// A rotation that linked its version after a destroy listed the versions deletes it itself.
    #[test]
    fn versions_added_after_a_destroy_are_deleted() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        create_file(&kms.tenant_dir(7).join(DESTROYED)).unwrap();
        assert_matches!(kms.add_version(7, 2), Err(Error::Destroyed { tenant: 7 }));
        assert_eq!(versions(&kms.tenant_dir(7)).unwrap(), [1]);
    }

    #[test]
    fn unknown_tenants_are_named() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let wrapped = wrap(&kms, 7, &Secret::random()).unwrap();
        assert_matches!(unwrap(&kms, 9, &wrapped), Err(Error::NoSuchTenant { tenant: 9 }));
        assert_eq!(refusal(unwrap(&kms, 9, &wrapped)), "tenant 9: no KEK in the KMS");
        assert_eq!(refusal(wrap(&kms, 9, &Secret::random())), "tenant 9: no KEK in the KMS");
        assert_eq!(refusal(kms.rotate(9)), "tenant 9: no KEK in the KMS");
        assert_eq!(refusal(kms.revoke(9)), "tenant 9: no KEK in the KMS");
        assert_eq!(refusal(kms.destroy(9)), "tenant 9: no KEK in the KMS");
        assert_eq!(refusal(kms.create(7)), "tenant 7: a KEK exists already");
    }

    #[test]
    fn kek_files_of_the_wrong_length_fail_as_io_errors() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let path = kek_path(&kms.tenant_dir(7), 1);
        fs::write(&path, [0; 31]).unwrap();
        let refused = wrap(&kms, 7, &Secret::random());
        assert_matches!(
            refused,
            Err(Error::Io { tenant: 7, source }) if source.kind() == io::ErrorKind::InvalidData
        );
        assert_eq!(
            refusal(wrap(&kms, 7, &Secret::random())),
            format!("tenant 7: the KMS failed: {}: a KEK of 31 bytes, not 32", path.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn kek_files_are_their_owners_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        kms.rotate(7).unwrap();
        kms.revoke(7).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let tenant = kms.tenant_dir(7);
        assert_eq!(mode(&kms.dir), 0o700);
        assert_eq!(mode(&tenant), 0o700);
        assert_eq!(mode(&kek_path(&tenant, 1)), 0o600);
        assert_eq!(mode(&kek_path(&tenant, 2)), 0o600);
        assert_eq!(mode(&tenant.join(REVOKED)), 0o600);
        assert_eq!(fs::metadata(kek_path(&tenant, 2)).unwrap().len(), KEY_LEN as u64);
    }

    /// TASK-411.3: a KMS is configured by its URL, `local:///DIR` alone, and reached through a
    /// trait object as through itself.
    #[test]
    fn kmses_are_named_by_their_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kms");
        let shared = crate::kms::from_url(&format!("local://{}", path.display())).unwrap();
        LocalKms::open(&path).unwrap().create(7).unwrap();
        let key = Secret::random();
        let wrapped = block_on(shared.wrap_dyn(7, &key)).unwrap();
        assert_eq!(block_on(shared.unwrap_dyn(7, &wrapped)).unwrap().expose(), key.expose());
        assert_eq!(
            block_on(shared.unwrap_dyn(8, &wrapped)).map(|_| ()).unwrap_err().to_string(),
            "tenant 8: no KEK in the KMS"
        );
        let refused = |url: &str| crate::kms::from_url(url).map(|_| ()).unwrap_err();
        assert_eq!(
            refused("vault://key"),
            "\"vault://key\" names no KMS: local:///DIR, openbao://, aws-kms:// or gcp-kms:// \
             (doc-16)"
        );
        assert_eq!(
            refused("local://relative"),
            "\"local://relative\" names no KMS: local:///DIR names an absolute directory"
        );
    }

    #[test]
    fn counted_kms_counts_its_calls() {
        let (_dir, kms) = kms();
        kms.create(7).unwrap();
        let counted = Counted::new(kms);
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 0));
        let wrapped = wrap(&counted, 7, &Secret::random()).unwrap();
        wrap(&counted, 7, &Secret::random()).unwrap();
        unwrap(&counted, 7, &wrapped).unwrap();
        assert_eq!((counted.wraps(), counted.unwraps()), (2, 1));
        // Refused calls count too: each is a call the KMS was asked.
        counted.inner().revoke(7).unwrap();
        assert_eq!(refusal(unwrap(&counted, 7, &wrapped)), "tenant 7: its KEK is revoked");
        assert_eq!((counted.wraps(), counted.unwraps()), (2, 2));
        counted.reset();
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 0));
    }
}
