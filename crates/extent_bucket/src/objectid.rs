// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: object IDs, unique in their bucket (TASK-364): a branch's ID is one. An extent's ID names
//! its branch by the branch's ID alone, as forks share their parents' extents by it, and a host's
//! cache of extents keys them by it; so no two branches of a bucket may have one ID, whatever their
//! cells. Records were once keyed by IDs alone, a second branch of an ID refused by its record's
//! create; under cells, they are not, and this is what keeps IDs apart.
//!
//! An [`Allocator`] gives IDs out of a block of [`ID_BLOCK`] of them it took: a block is taken by
//! creating its object, `objectid/<block>` ([`layout::objectid_block`]), which only one taking
//! does, and kept for good, so that no other allocator takes it again, of any process. The IDs of a
//! block go in order, each once: an ID costs no request but its block's first, and a block's
//! object no more than its taking says. An allocator going, its block's IDs not given go with it.
//! Blocks are chosen at random, so that allocators taking blocks at once seldom meet.
//!
//! cladefs: block 0 no allocator takes: its IDs are those tests name, as a guest might, which no
//! allocator gives (TASK-455). An ID of a block taken is [`given`] to one making alone, which
//! no fork names (doc-7, Object IDs): a root's or a clone's record of one, deleted, goes with its
//! dead cell, no recorder making it again (`record::Record::removable`).

use crate::bucket::{self, Bucket, Created};
use crate::layout;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// How many IDs a block holds: an ID's low bits number it in its block, its high bits name the
/// block.
pub const ID_BLOCK: u64 = 1 << 24;

/// How many blocks there are: none holds `u64::MAX`, which names no branch. Block 0, the IDs tests
/// name, no allocator takes.
const ID_BLOCKS: u64 = u64::MAX / ID_BLOCK;

/// What a block's object holds: its taker, a random number its allocator chose, so that the
/// taking's create, landed though its answer was lost, is its own when tried again
/// ([`bucket::create`]).
#[derive(Serialize, Deserialize)]
struct BlockTaker {
    taker: u64,
}

/// Gives object IDs out of blocks it takes in its bucket, each once. One a process has for each
/// bucket, as every one it makes, at once, takes a block of its own.
pub struct Allocator {
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
        Self { block: Mutex::new(None) }
    }

    /// A new ID of `bucket`'s, given by no other: the next of its block, or the first of a block it
    /// takes, its last given.
    pub async fn next(&self, bucket: &impl Bucket) -> u64 {
        loop {
            if let Some(id) = self.given() {
                return id;
            }
            let taken = take(bucket).await;
            let mut block = self.block.lock().unwrap();
            // Another call's taking may have come first, its block given out by now or not: this
            // one's is given out next, its own wasted otherwise, which no ID of is given.
            if block.is_none_or(|(_, next)| next == ID_BLOCK) {
                *block = Some((taken, first(taken)));
            }
        }
    }

    /// The next ID of its block, if it has one with any left.
    fn given(&self) -> Option<u64> {
        let mut block = self.block.lock().unwrap();
        let (taken, next) = block.as_mut().filter(|(_, next)| *next < ID_BLOCK)?;
        let id = *taken * ID_BLOCK + *next;
        debug_assert!(id >= ID_BLOCK, "block 0's IDs are named, never given");
        *next += 1;
        Some(id)
    }
}

/// The number in `block` of its first ID.
fn first(block: u64) -> u64 {
    assert_ne!(block, 0, "block 0's IDs are named, never given");
    0
}

/// cladefs: whether `id` is one an allocator gave: of a block an allocator took, whose object is
/// there, kept for good, and not block 0, the IDs tests name (TASK-455). A request a call: a
/// GET of the block's object.
pub async fn given(bucket: &impl Bucket, id: u64) -> bool {
    let block = id / ID_BLOCK;
    block != 0 && bucket::get(bucket, &layout::objectid_block(block)).await.is_some()
}

/// Takes a block of `bucket`'s that no allocator took, at random, and returns it.
async fn take(bucket: &impl Bucket) -> u64 {
    let bytes = layout::encode(&BlockTaker { taker: rand::random() });
    loop {
        let block = 1 + rand::random::<u64>() % (ID_BLOCKS - 1);
        match bucket::create(bucket, &layout::objectid_block(block), bytes.clone()).await {
            Created::Created(_) => return block,
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
    /// allocator of the bucket takes blocks of its own: no ID is given twice.
    #[test]
    fn allocators_give_ids_of_blocks_of_their_own() {
        futures::executor::block_on(async {
            let bucket = memory();
            let (one, other) = (Allocator::new(), Allocator::new());
            let first = one.next(&bucket).await;
            assert_eq!(one.next(&bucket).await, first + 1);
            let theirs = other.next(&bucket).await;
            assert_ne!(theirs / ID_BLOCK, first / ID_BLOCK, "two allocators took one block");
            // Its block given out, the next ID is another block's.
            one.block.lock().unwrap().as_mut().unwrap().1 = ID_BLOCK - 1;
            let last = one.next(&bucket).await;
            assert_eq!(last, first / ID_BLOCK * ID_BLOCK + ID_BLOCK - 1);
            let next = one.next(&bucket).await;
            assert_ne!(next / ID_BLOCK, first / ID_BLOCK);
            let blocks: BTreeSet<u64> = (bucket::list(&bucket, "objectid/").await.into_iter())
                .map(|meta| match layout::kind(&meta.key) {
                    layout::Kind::ObjectIds { block } => block,
                    kind => panic!("{} is a {kind:?}", meta.key),
                })
                .collect();
            let expected = BTreeSet::from([first / ID_BLOCK, theirs / ID_BLOCK, next / ID_BLOCK]);
            assert_eq!(blocks, expected);
        });
    }

    /// A block taken is no other allocator's, though it chooses it: it chooses again.
    #[test]
    fn blocks_taken_are_taken_once() {
        futures::executor::block_on(async {
            let bucket = memory();
            let mut taken = BTreeSet::new();
            for _ in 0..64 {
                taken.insert(take(&bucket).await);
            }
            assert_eq!(taken.len(), 64);
            assert!(!taken.contains(&0), "block 0 taken");
            assert_eq!(bucket::list(&bucket, "objectid/").await.len(), 64);
        });
    }

    /// No ID of block 0 is given, 0 among them, which names no branch: no allocator takes the
    /// block, whose IDs tests name, and none of them is given though its object were there
    /// (TASK-455). An allocator's ID is given; one of a block no allocator took is not.
    #[test]
    fn no_id_is_0() {
        futures::executor::block_on(async {
            let bucket = memory();
            bucket::create(&bucket, &layout::objectid_block(0), b"by hand".to_vec()).await;
            assert_eq!((given(&bucket, 0).await, given(&bucket, 1).await), (false, false));
            let id = Allocator::new().next(&bucket).await;
            assert!(id >= ID_BLOCK, "{id} of block 0");
            let untaken = if id / ID_BLOCK == 1 { 2 } else { 1 } * ID_BLOCK;
            assert_eq!((given(&bucket, id).await, given(&bucket, untaken).await), (true, false));
        });
    }
}
