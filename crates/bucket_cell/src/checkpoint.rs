// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Checkpoints: a cell's state as of a segment (doc-7, Checkpoints): a table of where its extents
//! are (`extent_frame::table`), as local cells' checkpoints are, whose footer holds the rest of
//! the state. A writer recovering the cell reads it whole; a host after one extent looks it up by
//! its table ([`Table`]), through its fetcher, which reads the checkpoint's tail, with the footer,
//! and the part holding one block ([`Fetcher::lookup`](crate::fetch::Fetcher::lookup)).

use crate::Error;
use crate::layout::{self, Location};
use crate::recovery::{Lengths, State};
use bincode::Options as _;
use extent_frame::table::{self, Block, Entry, TableFooter};
use storage_device::ExtentId;

pub use extent_frame::table::TABLE_BLOCK;

/// The checkpoint of `state`. cladefs: authenticated, not encrypted, under the DEK `encrypting`
/// gives: metadata, which the collector reads holding no key (doc-16, TASK-411.3).
pub fn encode(state: &State, encrypting: layout::EncryptionKey<'_>) -> Vec<u8> {
    encode_kept(state, encrypting).0
}

/// The checkpoint of `state`, as [`encode`] makes it, and the lengths it keeps: cladefs: those of
/// the segments its extents are in alone ([`State::named_lengths`], doc-27), whoever writes it, a
/// writer or a fork's making, as no later checkpoint at its key replaces it. In its table's
/// appendix, which a recovery reads with the rest and a lookup never does
/// (`extent_frame::table::Appendix`): each by the segment's number and its writer's epoch, as its
/// key names them, as varints, a few bytes each, not its key's tens.
pub fn encode_kept(state: &State, encrypting: layout::EncryptionKey<'_>) -> (Vec<u8>, Lengths) {
    let lengths = state.named_lengths();
    let numbered: Vec<(u64, u64, u64)> = (lengths.iter())
        .filter_map(|(key, len)| match layout::kind(key) {
            layout::Kind::Segment { cell, segment, epoch } if cell == state.cell => {
                Some((segment, epoch, *len))
            }
            _ => {
                debug_assert!(false, "{key}'s length kept, no segment of {}", state.cell);
                None
            }
        })
        .collect();
    let appendix = varint().serialize(&numbered).expect("objects serialize");
    let rest = State { extents: Default::default(), ..state.clone() };
    let entries = (state.extents.iter())
        .map(|(extent, location)| (*extent, location.key.as_str(), location.offset, location.len));
    (table::encode(entries, layout::encode(&rest), &appendix, encrypting), lengths)
}

/// How a checkpoint's lengths are encoded in its table's appendix: their numbers as varints.
fn varint() -> impl bincode::Options {
    bincode::DefaultOptions::new()
}

fn located(entry: Entry) -> (ExtentId, Location) {
    let Entry { id, key, offset, len } = entry;
    (id, Location { key, offset, len })
}

/// The state a checkpoint holds: read by a reader holding no key, which reads an authenticated
/// one's, checking its CRCs alone, as the collector does (TASK-411.3).
pub fn decode(bytes: &[u8]) -> Result<State, Error> {
    decode_under(bytes, &extent_frame::NoKeys)
}

/// cladefs: the state a checkpoint holds, read by a reader holding `keys`: checked against its tags
/// and MACs if they hold its key, and refused if they hold its cell encrypted and not its key
/// (TASK-411.3).
pub fn decode_under(bytes: &[u8], keys: &dyn extent_frame::Keys) -> Result<State, Error> {
    let table::Decoded { footer, entries, appendix } = table::decode(bytes, keys)?;
    let mut state: State = layout::decode(&footer.rest)?;
    state.extents.extend(entries.into_iter().map(located));
    let numbered: Vec<(u64, u64, u64)> =
        varint().deserialize(&appendix).map_err(|err| Error::Corrupt(err.to_string()))?;
    state.lengths = (numbered.into_iter())
        .map(|(segment, epoch, len)| (layout::segment(state.cell, segment, epoch), len))
        .collect();
    Ok(state)
}

/// A checkpoint's table, as its footer says: where in its data each block of its entries is, and
/// the rest of its state. A host's fetcher keeps it with the checkpoint's tail, and looks an extent
/// up by the block that would hold its entry
/// ([`Fetcher::lookup`](crate::fetch::Fetcher::lookup)).
#[derive(Debug)]
pub struct Table {
    footer: TableFooter,
    state: State,
}

impl Table {
    /// The table a checkpoint's footer, `footer`, says.
    pub(crate) fn decode(footer: &[u8]) -> Result<Self, Error> {
        let footer = TableFooter::decode(footer)?;
        let state = layout::decode(&footer.rest)?;
        Ok(Self { footer, state })
    }

    /// The state, without its extents, nor its lengths, which its appendix holds
    /// ([`encode_kept`]).
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The block that would hold `extent`'s entry, if the checkpoint has one: where in its data.
    pub(crate) fn block(&self, extent: ExtentId) -> Option<&Block> {
        self.footer.block(extent)
    }

    /// Where `extent` is, if `bytes`, those of `block`, hold its entry.
    pub(crate) fn find(
        &self,
        bytes: &[u8],
        block: &Block,
        extent: ExtentId,
    ) -> Result<Option<Location>, Error> {
        let found = self.footer.entries(bytes, block)?.into_iter().map(located);
        Ok(found.into_iter().find(|(found, _)| *found == extent).map(|(_, location)| location))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::{Bucket, ByteRange, Meta, Object, PutMode, Version};
    use crate::layout::test_cell;
    use crate::testing::{Direct, SimBucket};
    use std::future::Future;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// How the tests' checkpoints are authenticated: under the tests' DEK (`testing::encrypting`).
    fn encrypting() -> layout::EncryptionKey<'static> {
        crate::testing::encrypting(test_cell(1))
    }

    fn state(extents: u64) -> State {
        state_in(extents, 1000)
    }

    /// A state of `extents` extents, `per_object` in each object.
    fn state_in(extents: u64, per_object: u64) -> State {
        let mut state = State::new(test_cell(3));
        for sequence in 0..extents {
            let key = layout::segment(test_cell(3), sequence / per_object, 1);
            let location = Location { key, offset: sequence * 4096, len: 4096 };
            state.extents.insert(ExtentId::new(3, sequence), location);
        }
        for object in 0..extents.div_ceil(per_object) {
            state.lengths.insert(layout::segment(test_cell(3), object, 1), per_object * 4096);
        }
        state.root = Some(b"root".to_vec());
        state.through = Some(7);
        state.capacity = Some(1 << 30);
        state
    }

    #[test]
    fn checkpoints_hold_states() {
        for extents in [0, 1, 1819, 1820, 1821, 10_000] {
            let state = state(extents);
            assert_eq!(decode(&encode(&state, encrypting())).unwrap(), state);
            // cladefs: and the length of a segment it holds nothing in, freed, goes (doc-27).
            let mut freed = state.clone();
            freed.lengths.insert(layout::segment(test_cell(3), 99, 1), 4096);
            assert_eq!(decode(&encode(&freed, encrypting())).unwrap(), state);
        }
        let mut bytes = encode(&state(3000), encrypting());
        bytes[100] ^= 1;
        assert!(decode(&bytes).is_err());
    }

    /// A bucket that counts the requests made of it, and the bytes read from it.
    struct Counting {
        bucket: Direct,
        requests: AtomicU64,
        read: AtomicU64,
    }

    impl Counting {
        /// The requests made of it, and the bytes read from it, so far.
        fn counted(&self) -> (u64, u64) {
            (self.requests.load(Ordering::Relaxed), self.read.load(Ordering::Relaxed))
        }
    }

    impl Bucket for Counting {
        fn get(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<Object, crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let read = &self.read;
            let get = self.bucket.get(key);
            async move {
                let object = get.await?;
                read.fetch_add(object.bytes.len() as u64, Ordering::Relaxed);
                Ok(object)
            }
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let read = &self.read;
            let get = self.bucket.get_range(key, range);
            async move {
                let bytes = get.await?;
                read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(bytes)
            }
        }

        fn head(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<Meta, crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.bucket.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.bucket.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send
        {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.bucket.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.bucket.put(key, bytes, mode)
        }

        fn delete(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<(), crate::bucket::Error>> + Send {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.bucket.delete(key)
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// TASK-90: a host looking extents up in a checkpoint, through its fetcher, reads the
    /// checkpoint's tail, which holds its table, once, with one ranged read, and a second, of the
    /// tail whole, if it is longer than the first read, as that of 2,000,000 extents is; then, for
    /// each lookup, the parts of its data the one block that would hold the extent's entry is in:
    /// not the checkpoint whole. Its keys front-coded (TASK-364), a tail that long names more
    /// objects than a thousand extents' each.
    #[test]
    fn lookups_read_one_block() {
        use crate::fetch::{Budget, Fetcher, Limits, Memory};
        use crate::object::TAIL;
        let state = state_in(2_000_000, 250);
        let bytes = encode(&state, encrypting());
        let key = layout::checkpoint(test_cell(3), 7, 1);
        let bucket = Counting {
            bucket: Direct(SimBucket::new()),
            requests: AtomicU64::new(0),
            read: AtomicU64::new(0),
        };
        let bucket = std::sync::Arc::new(bucket);
        let budget = Budget::new(Limits::UNBOUNDED);
        let fetcher = Fetcher::new(bucket.clone(), budget, Memory::new(), None);
        let frame = extent_frame::Frame::of(&bytes).unwrap();
        let (tail, stored) =
            (frame.tail(), extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored);
        let table =
            Table::decode(extent_frame::footer(&bytes, &extent_frame::NoKeys).unwrap()).unwrap();
        futures::executor::block_on(async {
            bucket.put(&key, bytes.clone().into(), PutMode::Create).await.unwrap();
            let mut counted = bucket.counted();
            let mut lookup = async |extent: ExtentId| {
                let found = fetcher.lookup(&key, extent).await;
                let before = std::mem::replace(&mut counted, bucket.counted());
                (found, (counted.0 - before.0, counted.1 - before.1))
            };
            // The parts the block that would hold `extent`'s entry is in: how many, of how many
            // bytes stored.
            let block = |extent: ExtentId| {
                let block = table.block(extent).unwrap();
                let parts = frame.parts_of(block.offset..block.offset + u64::from(block.len));
                let len: u64 = parts.clone().map(|part| stored[part as usize].len).sum();
                (parts.end - parts.start, len)
            };
            // The tail, longer than a first read of it, read with a second.
            assert!(tail.end - tail.start > TAIL);
            let opened = (2, TAIL + tail.end - tail.start);
            let first = ExtentId::new(3, 0);
            let (found, read) = lookup(first).await;
            assert_eq!(found.as_ref().map(Option::as_ref), Ok(state.extents.get(&first)));
            assert_eq!(read, (opened.0 + block(first).0, opened.1 + block(first).1));
            assert!(opened.1 < bytes.len() as u64 / 20, "opening read {opened:?}");
            for sequence in [1, 1_000_000, 1_999_999] {
                let extent = ExtentId::new(3, sequence);
                let (found, read) = lookup(extent).await;
                assert_eq!(found.as_ref().map(Option::as_ref), Ok(state.extents.get(&extent)));
                assert_eq!(read, block(extent));
                assert!(read.1 < extent_frame::PART / 2, "{read:?} read");
            }
            // Past the last extent, its block is read; before the first, none: IDs sort by
            // sequence number, then cell, as the table's entries do.
            let past = ExtentId::new(3, 2_000_000);
            assert_eq!(lookup(past).await, (Ok(None), block(past)));
            assert_eq!(lookup(ExtentId::new(2, 0)).await, (Ok(None), (0, 0)));
        });
    }
}
