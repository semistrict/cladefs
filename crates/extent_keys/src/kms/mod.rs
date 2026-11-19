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
//! ([`bucket`], [`master`]) and its objects in `bucket_cell::bucket_kms`, where buckets are
//! known: it is named by no URL, but chosen where `CLADEFS_KMS` names none and a master key is given.

use crate::{Secret, Unauthentic, Wrapped};
use std::future::Future;
use std::io;

mod aws;
mod azure;
pub mod bucket;
mod gcp;
mod local;
pub mod master;
mod names;
pub mod openbao;
mod real;

pub use azure::TOKEN_VAR as AZURE_TOKEN_VAR;
pub use gcp::TOKEN_VAR as GCP_TOKEN_VAR;
pub use local::{KekVersion, LOCAL_WRAPPED_LEN, LocalKms};
pub use names::KeyNames;

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
/// A real KMS names each tenant's KEK as [`KeyNames`] says, from `key` and `keys`. Any other URL is
/// refused, naming it.
pub fn from_url(url: &str) -> Result<Shared, String> {
    match url.split_once("://").map(|(scheme, _)| scheme) {
        Some("local") => local::from_url(url),
        Some("openbao" | "openbaos") => openbao::from_url(url),
        Some("aws-kms") => aws::from_url(url),
        Some("gcp-kms") => gcp::from_url(url),
        Some("azure-kv") => azure::from_url(url),
        _ => Err(format!(
            "{url:?} names no KMS: local:///DIR, openbao://, aws-kms://, gcp-kms:// or azure-kv:// \
             (doc-16)"
        )),
    }
}

/// Why a KMS refused, each naming the tenant whose KEK was asked for, so that a store that cannot
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
