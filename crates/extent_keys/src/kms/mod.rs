// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what cladefs needs of a KMS, where each tenant's KEK is kept and never leaves
//! (backlog doc-16, Goals): to wrap a key under a tenant's KEK, and to unwrap what it wrapped.
//! Nothing else: creating, rotating, revoking and destroying a KEK are the tenant's, done out of
//! band in its KMS, so they are not in [`Kms`]. Backends, each a module of its own, named by
//! [`from_url`]'s scheme: [`LocalKms`] (`local`), a fake for tests and development, its KEKs files
//! in a directory; OpenBao's Transit engine (`openbao`, TASK-411.16), AWS KMS (`aws-kms`,
//! TASK-411.17) and Google Cloud KMS (`gcp-kms`, TASK-411.18), the real ones (the user's choice of
//! 2026-11-14), each naming a tenant's KEK as [`KeyNames`] says. The bucket KMS (TASK-411.27), its
//! tenants' KEKs kept in the bucket wrapped by a master key, has its keys and formats here
//! ([`bucket`], [`master`]) and its objects in `bucket_cell::bucket_kms`, where buckets are known:
//! it is named by no URL, but chosen where `CLADEFS_KMS` names none and a master key is given.

use crate::{Secret, Unauthentic, Wrapped};
use std::future::Future;
use std::io;

/// Implements [`Kms`] for a backend that runs `inner.wrap_now` / `inner.unwrap_now` on
/// `self.runtime` and exposes `&self.inner.names`.
macro_rules! impl_runtime_kms {
    ($ty:ty) => {
        impl super::Kms for $ty {
            fn wrap(
                &self,
                tenant: u64,
                key: &crate::Secret,
            ) -> impl std::future::Future<Output = Result<crate::Wrapped, super::Error>> + Send
            {
                let (inner, key) = (self.inner.clone(), crate::Secret::from_bytes(key.expose()));
                self.runtime.run(tenant, async move { inner.wrap_now(tenant, &key).await })
            }

            fn unwrap(
                &self,
                tenant: u64,
                wrapped: &crate::Wrapped,
            ) -> impl std::future::Future<Output = Result<crate::Secret, super::Error>> + Send {
                let (inner, wrapped) = (self.inner.clone(), wrapped.clone());
                self.runtime.run(tenant, async move { inner.unwrap_now(tenant, &wrapped).await })
            }

            fn names(&self) -> Option<&super::KeyNames> {
                Some(&self.inner.names)
            }
        }
    };
}

mod aws;
mod azure;
mod background;
pub mod bucket;
#[cfg(test)]
mod fake_http;
mod gcp;
mod http;
mod local;
pub mod master;
mod names;
pub mod openbao;
mod real;
mod retry;
pub mod trust;
pub mod vars;

pub use azure::TOKEN_VAR as AZURE_TOKEN_VAR;
pub use gcp::TOKEN_VAR as GCP_TOKEN_VAR;
pub use local::{KekVersion, LOCAL_WRAPPED_LEN, LocalKms};
pub use names::{KeyNames, OverrideFiles, Staged};
pub use trust::{Origin, Trust};
pub use vars::{KMS_VAR, Vars};

/// A KMS, holding tenants' KEKs, each named by its tenant's ID.
pub trait Kms: Send + Sync + 'static {
    /// `key`, wrapped under `tenant`'s KEK: its newest version, if the KMS keeps several.
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send;

    /// The key `wrapped` holds, if `tenant`'s KEK wrapped it, under any version of it still kept.
    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send;

    /// cladefs: drops what it keeps of `tenant`'s KEK, if it keeps any: the bucket KMS's unwrapped
    /// KEKs (TASK-411.27), as a host told the tenant's keys are revoked or destroyed forgets them.
    /// A KMS keeping none does nothing, as here.
    fn forget(&self, tenant: u64) {
        let _ = tenant;
    }

    /// Whether wrapping a cell KEK ahead of time can save a KMS request on its creation path.
    fn prewrap(&self) -> bool {
        true
    }

    /// cladefs: how it names tenants' KEKs, if by [`KeyNames`]: what a reload of its `keys` file
    /// changes (board 8487). A KMS naming them otherwise has none, as here.
    fn names(&self) -> Option<&KeyNames> {
        None
    }
}

/// cladefs: a KMS a host holds whatever it is, as a trait object ([`Shared`], TASK-411.3): every
/// [`Kms`] is one.
pub trait Dyn: Send + Sync + 'static {
    /// [`Kms::wrap`].
    fn wrap_dyn<'a>(&'a self, tenant: u64, key: &'a Secret) -> Boxed<'a, Wrapped>;

    /// [`Kms::unwrap`].
    fn unwrap_dyn<'a>(&'a self, tenant: u64, wrapped: &'a Wrapped) -> Boxed<'a, Secret>;

    /// [`Kms::forget`].
    fn forget_dyn(&self, tenant: u64);

    /// [`Kms::prewrap`].
    fn prewrap_dyn(&self) -> bool;

    /// [`Kms::names`].
    fn names_dyn(&self) -> Option<&KeyNames>;
}

/// What a [`Dyn`] KMS's calls return.
pub type Boxed<'a, T> = std::pin::Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

impl<K: Kms> Dyn for K {
    fn wrap_dyn<'a>(&'a self, tenant: u64, key: &'a Secret) -> Boxed<'a, Wrapped> {
        Box::pin(self.wrap(tenant, key))
    }

    fn unwrap_dyn<'a>(&'a self, tenant: u64, wrapped: &'a Wrapped) -> Boxed<'a, Secret> {
        Box::pin(self.unwrap(tenant, wrapped))
    }

    fn forget_dyn(&self, tenant: u64) {
        self.forget(tenant)
    }

    fn prewrap_dyn(&self) -> bool {
        self.prewrap()
    }

    fn names_dyn(&self) -> Option<&KeyNames> {
        self.names()
    }
}

/// cladefs: the KMS a host's processes are configured with, shared (TASK-411.3).
pub type Shared = std::sync::Arc<dyn Dyn>;

/// cladefs: the KMS `url` names (TASK-411.3), by its scheme:
///
/// - `local:///DIR`: a [`LocalKms`] keeping its tenants' KEKs under the directory `DIR`;
/// - `openbao://HOST[:PORT]/MOUNT?key=TEMPLATE[&keys=FILE]` (`openbaos://` over TLS): OpenBao's
///   Transit engine mounted at `MOUNT` (TASK-411.16);
/// - `aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]`: AWS KMS in `REGION`, at
///   `endpoint` if given (a VPC endpoint, an emulator) (TASK-411.17);
/// - `gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]`: Google Cloud KMS
///   (TASK-411.18);
/// - `azure-kv://VAULT_HOST?key=TEMPLATE[&keys=FILE][&alg=ALG][&endpoint=URL]`: Azure Key Vault
///   (TASK-633.6).
///
/// cladefs: each over HTTP reads a reply of 16 KiB at most, a token's or OpenBao's of 64 KiB
/// (`reply`), refusing a longer one, and follows no redirect (board 8527): a hostile
/// endpoint sends a host no more than that, and takes no request elsewhere.
///
/// A real KMS names each tenant's KEK as [`KeyNames`] says, from `key` and `keys`. Any other URL is
/// refused, naming it. Its credentials are the process's environment's ([`Vars::from_env`]), the
/// URL the host's own ([`Origin::Host`]).
pub fn from_url(url: &str) -> Result<Shared, String> {
    from_url_with(url, &Vars::from_env()?, Origin::Host)
}

/// cladefs: the same, its credentials those `vars` give, never the process's environment behind
/// them (board 8485): a bucket's own over the host's ([`Vars::numbered`]); and the URL of
/// `origin`, which, one the registry of buckets records, reaches what the host allows alone
/// (board 8525, [`trust`]), refused before any file or connection is opened.
pub fn from_url_with(url: &str, vars: &Vars, origin: Origin) -> Result<Shared, String> {
    from_url_with_files(url, vars, origin, None)
}

/// cladefs: like [`from_url_with`], sharing `keys=FILE` override cells in `files` across KMS
/// instances of the same host (board 10466).
pub fn from_url_with_files(
    url: &str,
    vars: &Vars,
    origin: Origin,
    files: Option<&OverrideFiles>,
) -> Result<Shared, String> {
    match url.split_once("://").map(|(scheme, _)| scheme) {
        Some("local") => local::from_url(url, origin),
        Some("openbao" | "openbaos") => openbao::from_url(url, vars, origin, files),
        Some("aws-kms") => aws::from_url(url, vars, origin, files),
        Some("gcp-kms") => gcp::from_url(url, vars, origin, files),
        Some("azure-kv") => azure::from_url(url, vars, origin, files),
        _ => Err(format!(
            "{url:?} names no KMS: local:///DIR, openbao://, aws-kms://, gcp-kms:// or azure-kv:// \
             (doc-16)"
        )),
    }
}

/// Checks whether a registry entry may reuse an already-open KMS of the same URL and credentials.
/// The same URL parsers enforce the recorded origin on both a new instance and a reused one.
pub fn check_recorded_url(url: &str, trust: &Trust) -> Result<(), String> {
    let origin = Origin::Recorded(trust);
    match url.split_once("://").map(|(scheme, _)| scheme) {
        Some("local") => origin.local().map_err(|why| format!("{url:?}: {why}")),
        Some("openbao" | "openbaos") => openbao::check_url(url, origin),
        Some("aws-kms") => aws::check_url(url, origin),
        Some("gcp-kms") => gcp::check_url(url, origin),
        Some("azure-kv") => azure::check_url(url, origin),
        _ => Err(format!("{url:?} names no KMS")),
    }
}

/// Why a KMS refused, each naming the tenant whose KEK was asked for, so that a cell that cannot
/// open says whose key it lacks.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("tenant {tenant}: no KEK in the KMS")]
    NoSuchTenant { tenant: u64 },
    /// [`LocalKms::create`] of a tenant that has a KEK, or had one.
    #[error("tenant {tenant}: a KEK exists already")]
    Exists { tenant: u64 },
    /// The tenant took cladefs's rights to its KEK away, for now: it may give them back.
    #[error("tenant {tenant}: its KEK is revoked")]
    Revoked { tenant: u64 },
    /// The tenant's KEK is gone for good, and with it everything wrapped under it: the tenant is
    /// shredded (doc-16, Shredding).
    #[error("tenant {tenant}: its KEK is destroyed")]
    Destroyed { tenant: u64 },
    /// cladefs: the version of the tenant's KEK that wrapped this key is destroyed (Cloud KMS's
    /// `DESTROYED`, `DESTROY_SCHEDULED`) or trimmed (OpenBao's `min_available_version`): this key is
    /// gone for good, and the cell it is of with it; the tenant's KEK and its other versions live, its
    /// other cells unharmed (board 8479). Never a refusal of the tenant: the KMS said nothing of its
    /// KEK. Only an unwrap's: a wrap is under the KEK's newest version, whose loss is
    /// [`Error::Destroyed`].
    #[error("tenant {tenant}: version {version} of its KEK, which wrapped this key, is destroyed")]
    VersionGone { tenant: u64, version: String },
    /// cladefs: the version of the tenant's KEK that wrapped this key is disabled (Cloud KMS's
    /// `DISABLED`) or below what the key decrypts (OpenBao's `min_decryption_version`), for now: as
    /// [`Error::VersionGone`], that version's alone, never the tenant's (board 8479, hotbed question
    /// 457).
    #[error("tenant {tenant}: version {version} of its KEK, which wrapped this key, is disabled")]
    VersionDisabled { tenant: u64, version: String },
    /// A wrapped key not wrapped by this tenant's KEK: another tenant's, a version it never had,
    /// tampered with or corrupt.
    #[error("tenant {tenant}: {source}")]
    Unauthentic { tenant: u64, source: Unauthentic },
    /// cladefs: the bucket KMS's (TASK-411.27): the tenant's KEK object is wrapped by a master key,
    /// of ID `id`, that is none of this host's (`held`, their IDs): a host given the wrong keys, or
    /// a rotation's newest before this host has it. Never taken for a tenant with no KEK.
    #[error(
        "tenant {tenant}: its KEK is wrapped by master key {id}, which this host has not (it has \
         {held}): CLADEFS_MASTER_KEY"
    )]
    UnknownMasterKey { tenant: u64, id: String, held: String },
    /// cladefs: the bucket KMS's (TASK-633.6): a key wrapped under the tenant's KEK in another
    /// bucket, of identity `bucket`, than this KMS's: asked of the wrong bucket's KMS, which says
    /// nothing of the tenant's KEK. Never a revocation.
    #[error("tenant {tenant}: a key wrapped in the bucket KMS of another bucket ({bucket:016x})")]
    Elsewhere { tenant: u64, bucket: u64 },
    /// The KMS failed. What was asked may or may not have taken effect.
    #[error("tenant {tenant}: the KMS failed: {source}")]
    Io { tenant: u64, source: io::Error },
}
