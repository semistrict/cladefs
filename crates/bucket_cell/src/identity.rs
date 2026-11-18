// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: buckets and their numbers (backlog doc-23, TASK-633.9). A bucket's number is in the
//! high bits of every cell ID it gives ([`crate::objectid`]), so that no two buckets give one ID,
//! and a child in one bucket names a parent in another by its ID alone.
//!
//! The **root bucket**, number [`ROOT`], keeps the record of every bucket: its [`Registry`], at
//! [`layout::BUCKETS`], each bucket's number, the random ID of its identity, and its details (its
//! provider, cloud region, URL, endpoint, KMS and master key's ID; never a key or a credential).
//! Adding a bucket ([`add`]) gives it the next number, sequentially, by a conditional write of the
//! root's registry, which serializes every addition. Every other bucket's root holds a copy of the
//! registry ([`spread`]), of a version no older than the copy before it; the root's is the
//! authority.
//!
//! A bucket's number is in its identity object ([`layout::IDENTITY`]), create-only, never changed:
//! [`format`] makes a bucket never formatted the root of buckets of its own, number 1, with a
//! registry of itself; [`add`] makes it the next bucket of another root's. An allocator formats its
//! bucket before it takes its first block, so that every bucket holding a cell has a number.
//! Numbers 0 ([`NAMED`], the IDs tests name) and 65535 ([`NONE`], as `u64::MAX` names no cell) are
//! no bucket's.

use crate::bucket::{self, Bucket, Created, Updated};
use crate::{Error, layout};
use serde::{Deserialize, Serialize};

/// The number no bucket has, whose cells' IDs tests name, as no allocator gives them.
pub const NAMED: u16 = 0;

/// The number no bucket has, so that no allocator gives `u64::MAX`, which names no cell.
pub const NONE: u16 = u16::MAX;

/// The root bucket's number: the first bucket's, which keeps the record of every bucket.
pub const ROOT: u16 = 1;

/// What a bucket's identity object holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The format of the bucket's objects ([`layout::FORMAT`]), first, so that a reader of any
    /// format reads it.
    pub format: u32,
    /// The bucket's number, in the high bits of every cell ID it gives.
    pub number: u16,
    /// A random number its formatting or its addition chose: which bucket of that number this is.
    pub id: u64,
}

/// What the registry says of a bucket, as its formatting or its addition was told: none of it a
/// key or a credential, which each host keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    /// Its provider: `s3`, `gcs`, `azure`, `file` or `memory`.
    pub provider: Option<String>,
    /// Its cloud region.
    pub region: Option<String>,
    /// Its URL, as hosts reach it.
    pub url: Option<String>,
    /// Its endpoint, if not its provider's own.
    pub endpoint: Option<String>,
    /// The KMS its cells' keys are wrapped under (doc-16), as `CLADEFS_KMS` names it.
    pub kms: Option<String>,
    /// Under the bucket KMS, the ID of the master key that wraps its tenants' KEKs.
    pub master_key_id: Option<String>,
}

impl Details {
    /// These, each missing one taken from `others`.
    fn or(self, others: &Self) -> Self {
        let or = |mine: Option<String>, theirs: &Option<String>| mine.or_else(|| theirs.clone());
        Self {
            provider: or(self.provider, &others.provider),
            region: or(self.region, &others.region),
            url: or(self.url, &others.url),
            endpoint: or(self.endpoint, &others.endpoint),
            kms: or(self.kms, &others.kms),
            master_key_id: or(self.master_key_id, &others.master_key_id),
        }
    }
}

/// A bucket, as the registry records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub number: u16,
    /// Its identity's random ID ([`Identity::id`]).
    pub id: u64,
    pub details: Details,
}

/// The record of every bucket, the root's or a copy of it ([`layout::BUCKETS`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    /// The format of the buckets' objects, first, as an identity's.
    pub format: u32,
    /// Which of the root's registries it is: one more at each change.
    pub version: u64,
    /// Every bucket, by number.
    pub buckets: Vec<Entry>,
}

impl Registry {
    /// The bucket of `number`, if recorded.
    pub fn bucket(&self, number: u16) -> Option<&Entry> {
        self.buckets.iter().find(|entry| entry.number == number)
    }
}

/// Decodes an identity or a registry, refusing another format than this one's.
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    let format: u32 = layout::decode(bytes.get(..4).unwrap_or(bytes))?;
    if format != layout::FORMAT {
        return Err(Error::BucketFormat(format));
    }
    layout::decode(bytes)
}

/// Whether `number` may be a bucket's: neither [`NAMED`] nor [`NONE`].
pub fn given(number: u16) -> bool {
    number != NAMED && number != NONE
}

/// The bucket's identity, if it was formatted or added.
pub async fn get(bucket: &impl Bucket) -> Result<Option<Identity>, Error> {
    match bucket::get(bucket, layout::IDENTITY).await {
        Some(object) => decode(&object.bytes).map(Some),
        None => Ok(None),
    }
}

/// The registry in `bucket`'s root: the root bucket's own, or a copy, if it has one.
pub async fn registry(bucket: &impl Bucket) -> Result<Option<Registry>, Error> {
    Ok(registry_versioned(bucket).await?.map(|(registry, _)| registry))
}

/// The registry in `bucket`'s root, and the version of its object.
async fn registry_versioned(
    bucket: &impl Bucket,
) -> Result<Option<(Registry, bucket::Version)>, Error> {
    match bucket::get(bucket, layout::BUCKETS).await {
        Some(object) => Ok(Some((decode(&object.bytes)?, object.meta.version))),
        None => Ok(None),
    }
}

/// Formats `bucket`, unless it was formatted or added: the root of buckets of its own, number
/// [`ROOT`], its registry recording it alone, with `details`. Its identity either way. Formattings
/// at once agree on the one that landed first; one that stopped short of the registry is finished.
pub async fn format(bucket: &impl Bucket, details: &Details) -> Result<Identity, Error> {
    let identity = match get(bucket).await? {
        Some(found) => found,
        None => {
            let made = Identity { format: layout::FORMAT, number: ROOT, id: rand::random() };
            match bucket::create(bucket, layout::IDENTITY, layout::encode(&made)).await {
                Created::Created(_) => made,
                // Another formatting's, or addition's, first.
                Created::Exists(object) => decode(&object.bytes)?,
            }
        }
    };
    if identity.number == ROOT && registry_versioned(bucket).await?.is_none() {
        let entry = Entry { number: ROOT, id: identity.id, details: details.clone() };
        let registry = Registry { format: layout::FORMAT, version: 1, buckets: vec![entry] };
        // Another formatting's, first, if it exists: of the same identity.
        bucket::create(bucket, layout::BUCKETS, layout::encode(&registry)).await;
    }
    Ok(identity)
}

/// Adds `new`, at `details.url`, to the buckets of the root bucket `root`, whose own details missing
/// from its registry `root_details` gives: the next number, given by a conditional write of the
/// root's registry, then its identity, of that number. Its identity, and the root's registry, which
/// the caller copies to every bucket ([`spread`]). An addition that stopped short of `new`'s
/// identity is finished by another of the same URL. `root` unformatted is formatted, the root;
/// refused if it is another bucket ([`Error::NotRoot`]), as is `new` if it is a bucket already
/// ([`Error::Formatted`]), or if no number is left ([`Error::NoNumbers`]).
pub async fn add(
    root: &impl Bucket,
    root_details: &Details,
    new: &impl Bucket,
    details: Details,
) -> Result<(Identity, Registry), Error> {
    let ours = format(root, root_details).await?;
    if ours.number != ROOT {
        return Err(Error::NotRoot(ours.number));
    }
    let found = get(new).await?;
    let (entry, registry) = loop {
        let (registry, version) = registry_versioned(root)
            .await?
            .ok_or_else(|| Error::Lost(layout::BUCKETS.to_owned()))?;
        // Added before: by this identity, or by an addition of its URL that stopped short of it.
        let added = (registry.buckets.iter()).find(|entry| match found {
            Some(found) => (entry.number, entry.id) == (found.number, found.id),
            None => {
                entry.number != ROOT && details.url.is_some() && entry.details.url == details.url
            }
        });
        if let Some(entry) = added {
            break (entry.clone(), registry);
        }
        if let Some(found) = found {
            return Err(Error::Formatted(found.number));
        }
        let last = registry.buckets.iter().map(|entry| entry.number).max().unwrap_or(ROOT);
        let number = last.checked_add(1).filter(|number| given(*number)).ok_or(Error::NoNumbers)?;
        let entry = Entry { number, id: rand::random(), details: details.clone() };
        let mut changed = registry.clone();
        changed.version += 1;
        for root_entry in changed.buckets.iter_mut().filter(|entry| entry.number == ROOT) {
            root_entry.details = std::mem::take(&mut root_entry.details).or(root_details);
        }
        changed.buckets.push(entry.clone());
        let bytes = layout::encode(&changed);
        match bucket::update(root, layout::BUCKETS, bytes, &version).await {
            Updated::Updated(_) => break (entry, changed),
            // Another's addition, first: the next number.
            Updated::Changed => {}
        }
    };
    let identity = Identity { format: layout::FORMAT, number: entry.number, id: entry.id };
    match bucket::create(new, layout::IDENTITY, layout::encode(&identity)).await {
        Created::Created(_) => Ok((identity, registry)),
        Created::Exists(object) => match decode::<Identity>(&object.bytes)? {
            found if found == identity => Ok((identity, registry)),
            found => Err(Error::Formatted(found.number)),
        },
    }
}

/// `found`, the identity of the bucket the registry records as `entry`, if it is that bucket: of
/// its number and its identity's random ID (TASK-633.3). Otherwise another bucket's, or no bucket's,
/// which a host refuses to use as it.
pub fn verified(entry: &Entry, found: Option<Identity>) -> Result<Identity, Error> {
    match found {
        Some(found) if (found.number, found.id) == (entry.number, entry.id) => Ok(found),
        found => Err(Error::Mismatched { number: entry.number, found: found.map(|f| f.number) }),
    }
}

/// Copies `registry` to `bucket`'s root, unless it holds one as new or newer: no copy goes back.
pub async fn spread(registry: &Registry, bucket: &impl Bucket) -> Result<(), Error> {
    let bytes = layout::encode(registry);
    loop {
        match registry_versioned(bucket).await? {
            Some((copy, _)) if copy.version >= registry.version => return Ok(()),
            Some((_, version)) => {
                match bucket::update(bucket, layout::BUCKETS, bytes.clone(), &version).await {
                    Updated::Updated(_) => return Ok(()),
                    Updated::Changed => {}
                }
            }
            None => match bucket::create(bucket, layout::BUCKETS, bytes.clone()).await {
                Created::Created(_) => return Ok(()),
                Created::Exists(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_bucket::ObjectBucket;
    use std::sync::Arc;

    fn memory() -> ObjectBucket {
        ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "")
    }

    /// Details of a bucket at `url`, in `region`.
    fn at(url: &str, region: &str) -> Details {
        Details {
            provider: Some("memory".into()),
            region: Some(region.into()),
            url: Some(url.into()),
            ..Details::default()
        }
    }

    /// A bucket formatted is the root of buckets of its own, number 1, its registry recording it
    /// alone, with the details it was formatted with; formatting it again changes nothing.
    #[test]
    fn a_bucket_formatted_is_a_root_of_its_own() {
        futures::executor::block_on(async {
            let bucket = memory();
            assert_eq!(get(&bucket).await, Ok(None));
            let details = at("memory:///root", "here");
            let made = format(&bucket, &details).await.unwrap();
            assert_eq!((made.format, made.number), (layout::FORMAT, ROOT));
            let entry = Entry { number: ROOT, id: made.id, details: details.clone() };
            let expected = Registry { format: layout::FORMAT, version: 1, buckets: vec![entry] };
            assert_eq!(registry(&bucket).await, Ok(Some(expected.clone())));
            assert_eq!(format(&bucket, &at("memory:///other", "there")).await, Ok(made));
            assert_eq!(registry(&bucket).await, Ok(Some(expected)));
        });
    }

    /// A formatting that stopped short of the root's registry is finished by the next, which
    /// records the identity the first made.
    #[test]
    fn formattings_stopped_short_of_the_registry_are_finished() {
        futures::executor::block_on(async {
            let bucket = memory();
            let left = Identity { format: layout::FORMAT, number: ROOT, id: 0x51 };
            bucket::create(&bucket, layout::IDENTITY, layout::encode(&left)).await;
            assert_eq!(registry(&bucket).await, Ok(None));
            assert_eq!(format(&bucket, &Details::default()).await, Ok(left));
            let registry = registry(&bucket).await.unwrap().unwrap();
            assert_eq!(registry.bucket(ROOT).map(|entry| entry.id), Some(0x51));
        });
    }

    /// Buckets added take the next numbers, in turn, each recorded in the root's registry with its
    /// details, the root's own filled in; each copy spread to a bucket is the root's.
    #[test]
    fn buckets_added_take_the_next_numbers() {
        futures::executor::block_on(async {
            let (root, second, third) = (memory(), memory(), memory());
            format(&root, &Details::default()).await.unwrap();
            let root_details = at("memory:///root", "here");
            let (two, _) =
                add(&root, &root_details, &second, at("memory:///2", "east")).await.unwrap();
            let (three, registry) =
                add(&root, &root_details, &third, at("memory:///3", "west")).await.unwrap();
            assert_eq!((two.number, three.number), (2, 3));
            assert_eq!(get(&second).await, Ok(Some(two)));
            assert_eq!(get(&third).await, Ok(Some(three)));
            let numbers: Vec<_> = registry.buckets.iter().map(|entry| entry.number).collect();
            assert_eq!((registry.version, numbers), (3, vec![1, 2, 3]));
            assert_eq!(registry.bucket(ROOT).unwrap().details, root_details);
            assert_eq!(registry.bucket(3).unwrap().details, at("memory:///3", "west"));
            assert_eq!(registry.bucket(3).unwrap().id, three.id);
            assert_eq!(super::registry(&root).await, Ok(Some(registry.clone())));
            for bucket in [&root, &second, &third] {
                spread(&registry, bucket).await.unwrap();
                assert_eq!(super::registry(bucket).await, Ok(Some(registry.clone())));
            }
        });
    }

    /// A bucket whose root's registry another addition changes, `rival` its entry, just before
    /// this one's change of it lands: two additions at once, the other first.
    struct Preempted {
        bucket: ObjectBucket,
        rival: std::sync::Mutex<Option<Entry>>,
    }

    impl Bucket for Preempted {
        async fn get(&self, key: &str) -> Result<bucket::Object, bucket::Error> {
            self.bucket.get(key).await
        }

        async fn get_range(
            &self,
            key: &str,
            range: bucket::ByteRange,
        ) -> Result<Vec<u8>, bucket::Error> {
            self.bucket.get_range(key, range).await
        }

        async fn head(&self, key: &str) -> Result<bucket::Meta, bucket::Error> {
            self.bucket.head(key).await
        }

        async fn list(&self, prefix: &str) -> Result<Vec<bucket::Meta>, bucket::Error> {
            self.bucket.list(prefix).await
        }

        async fn list_delimited(
            &self,
            prefix: &str,
            page: bucket::Page<'_>,
        ) -> Result<bucket::Listing, bucket::Error> {
            self.bucket.list_delimited(prefix, page).await
        }

        async fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: bucket::PutMode,
        ) -> Result<bucket::Version, bucket::Error> {
            let updating = key == layout::BUCKETS && matches!(mode, bucket::PutMode::Update(_));
            let rival = updating.then(|| self.rival.lock().unwrap().take()).flatten();
            if let Some(rival) = rival {
                let (mut theirs, version) =
                    registry_versioned(&self.bucket).await.unwrap().unwrap();
                theirs.version += 1;
                theirs.buckets.push(rival);
                let theirs = layout::encode(&theirs).into();
                self.bucket.put(key, theirs, bucket::PutMode::Update(version)).await.unwrap();
            }
            self.bucket.put(key, bytes, mode).await
        }

        async fn delete(&self, key: &str) -> Result<(), bucket::Error> {
            self.bucket.delete(key).await
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// Two additions at once take two numbers: the one whose change of the root's registry was
    /// beaten takes the next, recording both.
    #[test]
    fn additions_at_once_take_numbers_in_turn() {
        futures::executor::block_on(async {
            let rival = Entry { number: 2, id: 0x99, details: at("memory:///rival", "north") };
            let root = Preempted { bucket: memory(), rival: std::sync::Mutex::new(Some(rival)) };
            let new = memory();
            let (added, registry) =
                add(&root, &Details::default(), &new, at("memory:///new", "south")).await.unwrap();
            assert_eq!(added.number, 3);
            let numbers: Vec<_> = registry.buckets.iter().map(|entry| entry.number).collect();
            assert_eq!((registry.version, numbers), (3, vec![1, 2, 3]));
            assert_eq!(registry.bucket(2).map(|entry| entry.id), Some(0x99));
            assert_eq!(super::registry(&root).await, Ok(Some(registry)));
        });
    }

    /// An addition that stopped short of its bucket's identity is finished by another of the same
    /// URL, which takes the number recorded, not the next.
    #[test]
    fn additions_stopped_short_are_finished() {
        futures::executor::block_on(async {
            let (root, new) = (memory(), memory());
            let details = at("memory:///new", "east");
            let (added, _) = add(&root, &Details::default(), &new, details.clone()).await.unwrap();
            bucket::delete(&new, layout::IDENTITY).await;
            let (again, registry) =
                add(&root, &Details::default(), &new, details.clone()).await.unwrap();
            assert_eq!(again, added);
            assert_eq!(get(&new).await, Ok(Some(added)));
            let numbers: Vec<_> = registry.buckets.iter().map(|entry| entry.number).collect();
            assert_eq!(numbers, vec![1, 2]);
            // Added again, it is as it was.
            assert_eq!(add(&root, &Details::default(), &new, details).await.unwrap().0, added);
        });
    }

    /// A bucket already formatted, or added to another root, is no bucket to add; one not the root
    /// adds none; and once every number is given, none is added.
    #[test]
    fn additions_are_refused_where_they_would_number_twice() {
        futures::executor::block_on(async {
            let (root, formatted, added) = (memory(), memory(), memory());
            format(&formatted, &Details::default()).await.unwrap();
            let refused = add(&root, &Details::default(), &formatted, at("memory:///f", "x")).await;
            assert_eq!(refused, Err(Error::Formatted(ROOT)));
            add(&root, &Details::default(), &added, at("memory:///a", "x")).await.unwrap();
            let refused = add(&added, &Details::default(), &memory(), at("memory:///b", "x")).await;
            assert_eq!(refused, Err(Error::NotRoot(2)));

            let full = memory();
            format(&full, &Details::default()).await.unwrap();
            let (mut registry, version) = registry_versioned(&full).await.unwrap().unwrap();
            registry.buckets.push(Entry { number: NONE - 1, id: 1, details: Details::default() });
            bucket::update(&full, layout::BUCKETS, layout::encode(&registry), &version).await;
            let refused = add(&full, &Details::default(), &memory(), at("memory:///c", "x")).await;
            assert_eq!(refused, Err(Error::NoNumbers));
        });
    }

    /// A copy of the registry is never replaced by an older one.
    #[test]
    fn copies_never_go_back() {
        futures::executor::block_on(async {
            let (root, second, third, copy) = (memory(), memory(), memory(), memory());
            let (_, older) =
                add(&root, &Details::default(), &second, at("memory:///2", "x")).await.unwrap();
            let (_, newer) =
                add(&root, &Details::default(), &third, at("memory:///3", "x")).await.unwrap();
            spread(&newer, &copy).await.unwrap();
            spread(&older, &copy).await.unwrap();
            assert_eq!(registry(&copy).await, Ok(Some(newer)));
        });
    }

    /// A bucket of another format is refused, formatted or asked for its identity.
    #[test]
    fn a_bucket_of_another_format_is_refused() {
        futures::executor::block_on(async {
            let bucket = memory();
            let other = Identity { format: layout::FORMAT - 1, number: 3, id: 1 };
            bucket::create(&bucket, layout::IDENTITY, layout::encode(&other)).await;
            let refused = Error::BucketFormat(layout::FORMAT - 1);
            assert_eq!(get(&bucket).await, Err(refused.clone()));
            assert_eq!(format(&bucket, &Details::default()).await, Err(refused));
        });
    }
}
