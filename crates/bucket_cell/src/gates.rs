// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bucket-stored storage-version upgrade ratchets (`formats`, doc-28).
//!
//! Every formatted bucket stores a 2-knob [`Gates`] object (`storage` + `fxfs`) at
//! [`layout::FORMATS`], written at format time (`identity::format`) with this build's current
//! versions (or copied from the root bucket on `identity::add`). Operators advance gates
//! monotonically via [`ratchet`] once all nodes run the new build, keeping partial upgrades and
//! rollbacks safe while mixed builds run side by side.

use crate::bucket::{self, Bucket, Created, IfChanged, Updated, Version};
use crate::{Error, layout};
use formats::{Gates, Ratchet};

/// Reads and decodes [`layout::FORMATS`] from `bucket`, or `Ok(None)` if not yet created.
pub async fn get(bucket: &impl Bucket) -> Result<Option<Gates>, Error> {
    Ok(get_versioned(bucket).await?.map(|(gates, _)| gates))
}

/// Reads and decodes [`layout::FORMATS`] from `bucket` along with its object version.
async fn get_versioned(bucket: &impl Bucket) -> Result<Option<(Gates, Version)>, Error> {
    match bucket::get(bucket, layout::FORMATS).await? {
        Some(object) => Ok(Some((Gates::decode(&object.bytes)?, object.meta.version))),
        None => Ok(None),
    }
}

/// Ensures [`layout::FORMATS`] exists in `bucket`, creating it (create-only) with
/// [`Gates::current()`] if absent. Validates and returns the resulting [`Gates`].
pub async fn ensure_formatted(bucket: &impl Bucket) -> Result<Gates, Error> {
    if let Some(found) = get(bucket).await? {
        found.validate()?;
        return Ok(found);
    }
    let initial = Gates::current();
    match bucket::create(bucket, layout::FORMATS, initial.encode()).await? {
        Created::Created(_) => Ok(initial),
        Created::Exists(object) => {
            let found = Gates::decode(&object.bytes)?;
            found.validate()?;
            Ok(found)
        }
    }
}

/// Monotonically copies `gates` to `bucket`'s root unless `bucket` already holds versions at or
/// above `gates` in both knobs (never regresses either knob).
pub async fn spread(gates: Gates, bucket: &impl Bucket) -> Result<(), Error> {
    gates.validate()?;
    loop {
        match get_versioned(bucket).await? {
            Some((existing, _))
                if existing.storage >= gates.storage && existing.fxfs >= gates.fxfs =>
            {
                return Ok(());
            }
            Some((existing, version)) => {
                let merged = existing.merge_monotonic(gates);
                match bucket::update(bucket, layout::FORMATS, merged.encode(), &version).await? {
                    Updated::Updated(_) => return Ok(()),
                    Updated::Changed => {}
                }
            }
            None => match bucket::create(bucket, layout::FORMATS, gates.encode()).await? {
                Created::Created(_) => return Ok(()),
                Created::Exists(_) => {}
            },
        }
    }
}

/// Monotonically advances the bucket's [`layout::FORMATS`] ratchet for `storage` and/or `fxfs`
/// using CAS create/update. Returns `(resulting_gates, changed)`.
pub async fn ratchet(
    bucket: &impl Bucket,
    storage: Option<u32>,
    fxfs: Option<u32>,
) -> Result<(Gates, bool), Error> {
    loop {
        match get_versioned(bucket).await? {
            Some((current, version)) => {
                let (next, changed) = current.ratchet(storage, fxfs)?;
                if !changed {
                    return Ok((current, false));
                }
                match bucket::update(bucket, layout::FORMATS, next.encode(), &version).await? {
                    Updated::Updated(_) => return Ok((next, true)),
                    Updated::Changed => continue,
                }
            }
            None => {
                let base = Gates::current();
                let (next, _) = base.ratchet(storage, fxfs)?;
                match bucket::create(bucket, layout::FORMATS, next.encode()).await? {
                    Created::Created(_) => return Ok((next, true)),
                    Created::Exists(_) => continue,
                }
            }
        }
    }
}

/// Loads [`layout::FORMATS`] from the host's home `bucket` at startup and installs it into
/// `ratchet`.
///
/// - If `formats` exists: validates it against this build (failing hard if either knob was
///   ratcheted past what this build supports) and loads it into `ratchet`.
/// - If `formats` is absent (an unformatted or pre-ratchet bucket): leaves `ratchet` at
///   `Gates::current()` and returns `Ok(None)`.
pub async fn load_for_host(
    bucket: &impl Bucket,
    ratchet: &Ratchet,
) -> Result<Option<Gates>, Error> {
    if let Some(gates) = get(bucket).await? {
        ratchet.load(gates)?;
        return Ok(Some(gates));
    }
    Ok(None)
}

/// Conditional poller (`If-None-Match` -> 304 Not Modified) for [`layout::FORMATS`] in a host's
/// home bucket.
#[derive(Clone, Debug, Default)]
pub struct Poller {
    last_version: Option<Version>,
}

impl Poller {
    pub fn new() -> Self {
        Self::default()
    }

    /// Polls `bucket` for changes to [`layout::FORMATS`]. Returns `Ok(None)` when absent or
    /// unchanged.
    pub async fn poll(&mut self, bucket: &impl Bucket) -> Result<Option<Gates>, Error> {
        let object =
            match bucket::get_if_changed(bucket, layout::FORMATS, self.last_version.as_ref())
                .await?
            {
                IfChanged::Missing | IfChanged::Unchanged => return Ok(None),
                IfChanged::Changed(object) => object,
            };
        let gates = Gates::decode(&object.bytes)?;
        gates.validate()?;
        self.last_version = Some(object.meta.version);
        Ok(Some(gates))
    }
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use crate::identity;
    use crate::object_bucket::ObjectBucket;
    use futures::executor::block_on;
    use std::sync::Arc;

    fn memory() -> ObjectBucket {
        ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "")
    }

    #[test]
    fn format_bucket_writes_gates_and_add_bucket_copies_them() {
        block_on(async {
            let root = memory();
            let _ = identity::format(&root, &identity::Details::default()).await.unwrap();
            let initial = get(&root).await.unwrap().expect("formats written at format time");
            assert_eq!(initial, Gates::current());

            // Adding a member bucket copies root's gates.
            let member = memory();
            let _ = identity::add(
                &root,
                &identity::Details::default(),
                &member,
                identity::Details { url: Some("memory:///member".into()), ..Default::default() },
            )
            .await
            .unwrap();
            assert_eq!(get(&member).await.unwrap(), Some(initial));

            // Poller returns gates once, then None on 304 Not Modified.
            let mut poller = Poller::new();
            assert_eq!(poller.poll(&root).await.unwrap(), Some(initial));
            assert_eq!(poller.poll(&root).await.unwrap(), None);

            // Boot against a bucket ratcheted past this build fails hard.
            let future_bucket = memory();
            let future = Gates {
                storage: formats::gates::CURRENT_STORAGE + 1,
                fxfs: formats::gates::CURRENT_FXFS,
            };
            bucket::create(&future_bucket, layout::FORMATS, future.encode()).await.unwrap();
            let ratchet = Ratchet::current();
            let err = load_for_host(&future_bucket, &ratchet).await.unwrap_err();
            assert!(matches!(err, Error::Gates(formats::GateError::IncompatibleGate { .. })));

            // Downgrade is refused by ratchet().
            let err = self::ratchet(&root, Some(0), None).await.unwrap_err();
            assert!(matches!(err, Error::Gates(formats::GateError::CannotDowngrade { .. })));
        });
    }
}
