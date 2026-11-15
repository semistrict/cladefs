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
//! 2026-11-14), each naming a tenant's KEK as [`KeyNames`] says.

use crate::{Secret, Unauthentic, Wrapped};
use std::future::Future;
use std::io;

mod aws;
mod gcp;
mod local;
mod names;
mod openbao;

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
}

/// cladefs: a KMS a host holds whatever it is, as a trait object ([`Shared`], TASK-411.3): every
/// [`Kms`] is one.
pub trait Dyn: Send + Sync + 'static {
    /// [`Kms::wrap`].
    fn wrap_dyn<'a>(&'a self, tenant: u64, key: &'a Secret) -> Boxed<'a, Wrapped>;

    /// [`Kms::unwrap`].
    fn unwrap_dyn<'a>(&'a self, tenant: u64, wrapped: &'a Wrapped) -> Boxed<'a, Secret>;
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
}

/// cladefs: the KMS a host's processes are configured with, shared (TASK-411.3).
pub type Shared = std::sync::Arc<dyn Dyn>;

/// cladefs: the KMS `url` names (TASK-411.3), by its scheme:
///
/// - `local:///DIR`: a [`LocalKms`] keeping its tenants' KEKs under the directory `DIR`;
/// - `openbao://HOST[:PORT]/MOUNT?key=TEMPLATE[&keys=FILE]` (`openbaos://` over TLS): OpenBao's
///   Transit engine mounted at `MOUNT` (TASK-411.16);
/// - `aws-kms://REGION?key=TEMPLATE[&keys=FILE]`: AWS KMS in `REGION` (TASK-411.17);
/// - `gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]`: Google Cloud KMS
///   (TASK-411.18).
///
/// A real KMS names each tenant's KEK as [`KeyNames`] says, from `key` and `keys`. Any other URL is
/// refused, naming it.
pub fn from_url(url: &str) -> Result<Shared, String> {
    match url.split_once("://").map(|(scheme, _)| scheme) {
        Some("local") => local::from_url(url),
        Some("openbao" | "openbaos") => openbao::from_url(url),
        Some("aws-kms") => aws::from_url(url),
        Some("gcp-kms") => gcp::from_url(url),
        _ => Err(format!(
            "{url:?} names no KMS: local:///DIR, openbao://, aws-kms:// or gcp-kms:// (doc-16)"
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
    /// The KMS failed. What was asked may or may not have taken effect.
    #[error("tenant {tenant}: the KMS failed: {source}")]
    Io { tenant: u64, source: io::Error },
}
