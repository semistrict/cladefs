// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's head (doc-7, The head; TASK-407): its writer's taking and its commit, one
//! object ([`Head`]). Every object of a cell's is written once, at a key naming its writer's
//! epoch, and is the cell's only once the head names it: a writer acknowledges its segments by
//! swapping the head to name them, with the condition that it is still the version it last wrote,
//! its own taking's. A takeover swaps it to the next epoch, so that the writer before fails at its
//! next swap: the head is the fence. A deletion swaps it to [`Head::FINAL`], which no writer takes,
//! and nothing swaps after.
//!
//! Every swap is a conditional write of bytes unique to its writer, a head naming its holder, so
//! that one that landed though its answer was lost is recognised by reading it back.

use crate::bucket::{self, Bucket, Object, PutMode, Version};
use crate::layout::{self, CellId, Head};
use crate::{Error, rules};
use bytes::Bytes;

/// A head as its writer last swapped it, or a taking took it: what it holds, and the version its
/// next swap is conditional on.
#[derive(Clone, Debug)]
pub struct Held {
    pub head: Head,
    version: Version,
}

/// `cell`'s head as a read finds it, for a taking of it after ([`take_from`]).
pub struct Read(Option<Object>);

impl Read {
    /// cladefs: what a read of the head of a cell known to have none finds, without the read
    /// (`crate::known`, TASK-449): a clone its host just made, which no writer took yet. Wrong, a
    /// taking from it finds the head there and goes on from it, its create being conditional.
    pub fn absent() -> Self {
        Self(None)
    }

    /// The head read, if there was one.
    pub fn head(&self) -> Result<Option<Head>, Error> {
        self.0.as_ref().map(|object| Head::decode(&object.bytes)).transpose()
    }
}

/// Reads `cell`'s head for a taking of it after ([`take_from`]), as with other reads a taking
/// needs, at once (TASK-165.11).
pub async fn read(bucket: &impl Bucket, cell: CellId) -> Read {
    Read(bucket::get(bucket, &layout::head(cell)).await)
}

/// `cell`'s head, if it has one.
pub async fn get(bucket: &impl Bucket, cell: CellId) -> Result<Option<Head>, Error> {
    match bucket::get(bucket, &layout::head(cell)).await {
        Some(object) => Ok(Some(Head::decode(&object.bytes)?)),
        None => Ok(None),
    }
}

/// The epoch of `cell`'s head: 0 if it has none.
pub async fn epoch(bucket: &impl Bucket, cell: CellId) -> Result<u64, Error> {
    Ok(get(bucket, cell).await?.map_or(0, |head| head.epoch))
}

/// Takes `cell`'s head, of the epoch after its last, as `holder`, which must differ from every
/// other taking's, of the placement whose fencing token is `token`: what it names, the segments the
/// cell holds and its newest checkpoint, kept. Fails for a deleted cell's, and for one a later
/// placement took, whose token is higher ([`Error::Stale`]): a placement cut off takes no cell
/// back from the one after it (TASK-39.7).
pub async fn take(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<Held, Error> {
    take_from(bucket, cell, read(bucket, cell).await, holder, token).await
}

/// Takes `cell`'s head as [`take`] does, starting from `read`, what a read of it before found
/// ([`read`]), rather than reading it again: its write is conditional on what `read` found, so that
/// another's taking since, which changed it, is read and gone on from, as after a read of its own.
/// A round trip less, on a real bucket, for every opening of a store (TASK-165.11).
pub async fn take_from(
    bucket: &impl Bucket,
    cell: CellId,
    read: Read,
    holder: u64,
    token: u64,
) -> Result<Held, Error> {
    let key = layout::head(cell);
    let mut found = read.0;
    loop {
        let head = match &found {
            None => Head { epoch: 1, holder, token, ..Head::default() },
            Some(object) => match Head::decode(&object.bytes)? {
                head if head.epoch == Head::FINAL => return Err(Error::Deleted),
                head if rules().tokens && head.token > token => return Err(Error::Stale),
                // cladefs: naming what it named, its versions among them (rule `versions_carried`,
                // TASK-476); without the rule, as a test shows it needed, none.
                head => match rules().versions_carried {
                    true => Head { epoch: head.epoch + 1, holder, token, ..head },
                    false => {
                        Head { epoch: head.epoch + 1, holder, token, versions: Vec::new(), ..head }
                    }
                },
            },
        };
        let version = found.as_ref().map(|object| object.meta.version.clone());
        match write(bucket, &key, &head, version).await {
            Ok(held) => return Ok(held),
            Err(changed) => found = changed,
        }
    }
}

/// Swaps `cell`'s head from `held`, as its holder last wrote it, to `next`: with the condition
/// that it is still that version (rule `head_swap`), and so that taking still, which it fails
/// otherwise ([`Error::Fenced`]): another taking, or the cell's deletion, swapped it since.
pub async fn swap(
    bucket: &impl Bucket,
    cell: CellId,
    held: &Held,
    next: Head,
) -> Result<Held, Error> {
    let key = layout::head(cell);
    if !rules().head_swap {
        // Written whatever is there: a test's, which shows the condition needed.
        let bytes = Bytes::from(layout::encode(&next));
        let version = overwrite(bucket, &key, bytes).await;
        return Ok(Held { head: next, version });
    }
    write(bucket, &key, &next, Some(held.version.clone())).await.map_err(|_| Error::Fenced)
}

/// Ends `cell`'s head, as a deletion does first: swapped to its final epoch, which no writer
/// takes, as `holder` of the placement whose fencing token is `token`, unless a later placement
/// took it ([`Error::Stale`]). What it names, it keeps: the head the deletion recovers the cell
/// from, which nothing swaps after, so that every deletion of the cell finds the same forks
/// (TASK-183); but its versions, which go with the cell (doc-18, TASK-476). Starts from `found`,
/// what a read of it found ([`read`]). Returns it.
pub(crate) async fn end(
    bucket: &impl Bucket,
    cell: CellId,
    found: Read,
    holder: u64,
    token: u64,
) -> Result<Head, Error> {
    let key = layout::head(cell);
    let mut found = found.0;
    loop {
        let head = match &found {
            None => Head::default(),
            Some(object) => Head::decode(&object.bytes)?,
        };
        if head.epoch == Head::FINAL {
            return Ok(head);
        }
        if rules().delete_tokens && head.token > token {
            return Err(Error::Stale);
        }
        let ended =
            Head { epoch: Head::FINAL, holder, token: u64::MAX, versions: Vec::new(), ..head };
        let version = found.as_ref().map(|object| object.meta.version.clone());
        match write(bucket, &key, &ended, version).await {
            Ok(held) => return Ok(held.head),
            Err(changed) => found = changed,
        }
    }
}

/// Writes `head` at `key`: created where `version` is none, else updated from it. A write that
/// landed though its answer was lost, its bytes read back, is its own. Fails with what is there
/// now, if another's write came first.
async fn write(
    bucket: &impl Bucket,
    key: &str,
    head: &Head,
    version: Option<Version>,
) -> Result<Held, Option<Object>> {
    let bytes = Bytes::from(layout::encode(head));
    let mode = match &version {
        None => PutMode::Create,
        Some(version) => PutMode::Update(version.clone()),
    };
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), mode.clone()).await {
            Ok(version) => return Ok(Held { head: head.clone(), version }),
            Err(
                bucket::Error::AlreadyExists
                | bucket::Error::Precondition
                | bucket::Error::NotFound,
            ) => {
                return match bucket::get(bucket, key).await {
                    Some(object) if object.bytes == bytes => {
                        Ok(Held { head: head.clone(), version: object.meta.version })
                    }
                    found => Err(found),
                };
            }
            Err(bucket::Error::Unavailable) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}

/// Writes `bytes` at `key` whatever is there: its version.
async fn overwrite(bucket: &impl Bucket, key: &str, bytes: Bytes) -> Version {
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Overwrite).await {
            Ok(version) => return version,
            Err(_) => {
                failures += 1;
                bucket.pause(failures).await;
            }
        }
    }
}
