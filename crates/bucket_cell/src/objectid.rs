// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: object IDs, unique across buckets (TASK-364, TASK-633.9): a cell's ID is one. An
//! extent's ID names its cell by the cell's ID alone, as forks share their parents' extents by it,
//! and a host's cache of extents keys them by it; so no two cells may have one ID, whatever their
//! cells, nor, as a fork's child may live in another bucket than its parent (backlog doc-23),
//! whatever their buckets. Records were once keyed by IDs alone, a second cell of an ID refused by
//! its record's create; under cells, they are not, and this is what keeps IDs apart.
//!
//! An ID is `bucket << 48 | block << 24 | n`: its bucket's number ([`crate::identity`]), a block
//! of [`ID_BLOCK`] IDs of the bucket's, and its number in the block. Its bucket is the one that
//! gave it ([`bucket_of`]), whichever bucket holds its cell's objects.
//!
//! An [`Allocator`] gives IDs out of a block it took: a block is taken by creating its object,
//! `objectid/<bucket << 24 | block>` ([`layout::objectid_block`]), which only one taking does, and
//! kept for good, so that no other allocator takes it again, of any process. The IDs of a block go
//! in order, each once: an ID costs no request but its block's first, and a block's object no more
//! than its taking says. An allocator going, its block's IDs not given go with it. Blocks are
//! chosen at random, so that allocators taking blocks at once seldom meet. Before its first block,
//! an allocator learns its bucket's number, formatting the bucket, the root of buckets of its own,
//! if it has none ([`identity::format`]).
//!
//! cladefs: no bucket's number is 0 ([`identity::NAMED`]): IDs under it are those tests name, as a
//! guest might ([`named`]), which no allocator gives (TASK-455). An ID of a block taken is
//! [`given`] to one making alone, which no fork names (doc-7, Object IDs): a root's or a clone's
//! record of one, deleted, goes with its dead cell, no recorder making it again
//! (`record::Record::removable`).

use crate::bucket::{self, Bucket, Created};
use crate::{Error, identity, layout};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// How many IDs a block holds: an ID's low bits number it in its block, its high bits name the
/// block.
pub const ID_BLOCK: u64 = 1 << 24;

/// How many blocks a bucket has: the bits of an ID between its bucket's number and its number in
/// its block.
const BUCKET_BLOCKS: u64 = 1 << 24;

/// Where an ID's bucket's number begins.
const BUCKET_SHIFT: u32 = 48;

/// The number of the bucket that gave `id`: [`identity::NAMED`] if no allocator did.
pub fn bucket_of(id: u64) -> u16 {
    (id >> BUCKET_SHIFT) as u16
}

/// A random ID no allocator gives, not 0, for tests that name a cell's ID themselves: under
/// [`identity::NAMED`], no bucket's number.
pub fn named() -> u64 {
    (rand::random::<u64>() >> (u64::BITS - BUCKET_SHIFT)).max(1)
}

/// What a block's object holds: the bucket layout version first (doc-28, board 10038), then its
/// taker, a random number its allocator chose, so that the taking's create, landed though its
/// answer was lost, is its own when tried again ([`bucket::create`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub(crate) struct BlockTaker {
    pub format: u32,
    pub taker: u64,
}

impl BlockTaker {
    pub(crate) fn of(taker: u64) -> Self {
        Self { format: layout::FORMAT.current, taker }
    }

    #[cfg(test)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = layout::decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = layout::FORMAT.check(format).map_err(|_| Error::BucketFormat(format))?;
        match format {
            31.. => layout::decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }
}

/// Gives object IDs out of blocks it takes in its bucket, each once. One a process has for each
/// bucket, as every one it makes, at once, takes a block of its own.
pub struct Allocator {
    /// Its bucket's number, once learned.
    number: Mutex<Option<u16>>,
    /// Its block, if it took one, and the number in it of the next ID it gives.
    block: Mutex<Option<(u64, u64)>>,
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl Allocator {
    /// One with no block yet.
    pub const fn new() -> Self {
        Self { number: Mutex::new(None), block: Mutex::new(None) }
    }

    /// A new ID of `bucket`'s, given by no other: the next of its block, or the first of a block it
    /// takes, its last given. Refused if `bucket` is of another format.
    pub async fn next(&self, bucket: &impl Bucket) -> Result<u64, Error> {
        loop {
            if let Some(id) = self.given() {
                return Ok(id);
            }
            let taken = take(bucket, self.number(bucket).await?).await?;
            let mut block = self.block.lock().unwrap();
            // Another call's taking may have come first, its block given out by now or not: this
            // one's is given out next, its own wasted otherwise, which no ID of is given.
            if block.is_none_or(|(_, next)| next == ID_BLOCK) {
                *block = Some((taken, 0));
            }
        }
    }

    /// Its bucket's number: learned once, the bucket formatted, a root of its own, if it has none.
    async fn number(&self, bucket: &impl Bucket) -> Result<u16, Error> {
        if let Some(number) = *self.number.lock().unwrap() {
            return Ok(number);
        }
        let details = identity::Details::default();
        let number = identity::format(bucket, &details).await?.number;
        *self.number.lock().unwrap() = Some(number);
        Ok(number)
    }

    /// The next ID of its block, if it has one with any left.
    fn given(&self) -> Option<u64> {
        let mut block = self.block.lock().unwrap();
        let (taken, next) = block.as_mut().filter(|(_, next)| *next < ID_BLOCK)?;
        let id = *taken * ID_BLOCK + *next;
        debug_assert!(identity::given(bucket_of(id)), "{id:x} of no bucket's number");
        *next += 1;
        Some(id)
    }
}

/// Takes a block of the bucket numbered `number`, `bucket`, that no allocator took, at random, and
/// returns it.
async fn take(bucket: &impl Bucket, number: u16) -> Result<u64, crate::bucket::Refused> {
    assert!(identity::given(number), "bucket number {number} is no bucket's");
    let bytes = layout::encode(&BlockTaker::of(rand::random()));
    loop {
        let block = u64::from(number) * BUCKET_BLOCKS + rand::random::<u64>() % BUCKET_BLOCKS;
        match bucket::create(bucket, &layout::objectid_block(block), bytes.clone()).await? {
            Created::Created(_) => return Ok(block),
            // Another's: another block.
            Created::Exists(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_bucket::ObjectBucket;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn memory() -> ObjectBucket {
        ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "")
    }

    /// An allocator gives its block's IDs in order, each once, then takes another block; another
    /// allocator of the bucket takes blocks of its own: no ID is given twice. Every ID carries the
    /// bucket's number, which the first ID formatted it with.
    #[test]
    fn allocators_give_ids_of_blocks_of_their_own() {
        futures::executor::block_on(async {
            let bucket = memory();
            let (one, other) = (Allocator::new(), Allocator::new());
            let first = one.next(&bucket).await.unwrap();
            let number = identity::get(&bucket).await.unwrap().unwrap().number;
            assert_eq!(bucket_of(first), number);
            assert_eq!(one.next(&bucket).await, Ok(first + 1));
            let theirs = other.next(&bucket).await.unwrap();
            assert_eq!(bucket_of(theirs), number);
            assert_ne!(theirs / ID_BLOCK, first / ID_BLOCK, "two allocators took one block");
            // Its block given out, the next ID is another block's.
            one.block.lock().unwrap().as_mut().unwrap().1 = ID_BLOCK - 1;
            let last = one.next(&bucket).await.unwrap();
            assert_eq!(last, first / ID_BLOCK * ID_BLOCK + ID_BLOCK - 1);
            let next = one.next(&bucket).await.unwrap();
            assert_ne!(next / ID_BLOCK, first / ID_BLOCK);
            assert_eq!(bucket_of(next), number);
            let blocks: BTreeSet<u64> =
                (bucket::list(&bucket, "objectid/").await.unwrap().into_iter())
                    .map(|meta| match layout::kind(&meta.key) {
                        layout::Kind::ObjectIds { block } => block,
                        kind => panic!("{} is a {kind:?}", meta.key),
                    })
                    .collect();
            let expected = BTreeSet::from([first / ID_BLOCK, theirs / ID_BLOCK, next / ID_BLOCK]);
            assert_eq!(blocks, expected);
        });
    }

    /// A block taken is no other allocator's, though it chooses it: it chooses again. Every block
    /// is the bucket's number's.
    #[test]
    fn blocks_taken_are_taken_once() {
        futures::executor::block_on(async {
            let bucket = memory();
            let mut taken = BTreeSet::new();
            for _ in 0..64 {
                taken.insert(take(&bucket, 5).await.unwrap());
            }
            assert_eq!(taken.len(), 64);
            assert!(taken.iter().all(|block| block / BUCKET_BLOCKS == 5), "{taken:x?}");
            assert_eq!(bucket::list(&bucket, "objectid/").await.unwrap().len(), 64);
        });
    }

    /// A bucket of another format gives no ID: its allocator is refused, takes no block.
    #[test]
    fn a_bucket_of_another_format_gives_no_id() {
        futures::executor::block_on(async {
            let bucket = memory();
            let other = identity::Identity { format: layout::FORMAT.oldest - 1, number: 3, id: 1 };
            bucket::create(&bucket, layout::IDENTITY, layout::encode(&other)).await.unwrap();
            let refused = Allocator::new().next(&bucket).await;
            assert_eq!(refused, Err(Error::BucketFormat(layout::FORMAT.oldest - 1)));
            assert_eq!(bucket::list(&bucket, "objectid/").await.unwrap().len(), 0);
        });
    }
}
