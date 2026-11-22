// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: a host's write-ahead log of a cell (doc-14): its closed segments not yet
//! acknowledged, each stamped by its writer's chain, the very object it is to be in the bucket. A
//! segment is in the log before its upload starts, so that every segment that may land in the
//! writer's name is the log's; one leaves it once acknowledged.
//!
//! A writer taking the cell over, here after a crash or elsewhere, puts the log after the
//! cell's end ([`Wal::rebase`]), as the head it took names it (doc-7, The head; TASK-407), by
//! commits: the log keeps its start, the cell's last commit before its first segment, and the
//! cell's last commit says how far the log landed. If it is one of the log's, the log's segments
//! to it are the cell's; if it is that one, or the start, the rest are stamped again on the new
//! writer's chain, after the head's end; otherwise another writer's commits moved the cell on,
//! and the log diverged ([`Error::Diverged`]): its commits, never acknowledged, are lost once
//! `cladefsctl take` drops it. By commits, not by the keys the head names: a log stamped again on a
//! writer's chain is kept so in memory alone (TASK-165.9), a crash after finding it stamped as
//! before, at the keys of none of what landed. A head names a writer's segments only between its
//! commits, so that the log keeps a commit whole until the cell has it whole.

use crate::layout::{Chain, Footer, Head};
use crate::{Error, rules};
use std::collections::VecDeque;

/// What a writer taking a cell over found ([`Writer::taken`]): the head as it took it, what it
/// names, the cell's segments; the cell's last commit, and the forks it holds, by their
/// children and owners; and the writer's chain then, where its first segment goes, at the head's
/// end. What a host's log is put after ([`Wal::rebase`]).
///
/// [`Writer::taken`]: crate::Writer::taken
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakenCell {
    pub head: Head,
    pub last_commit: Option<u64>,
    pub chain: Chain,
}

impl TakenCell {
    /// What a writer whose chain is `chain` found taking a cell whose head is `head`, `state` the
    /// cell's state as it names it.
    pub fn of(head: Head, state: &crate::State, chain: Chain) -> Self {
        Self { head, last_commit: state.last_commit, chain }
    }
}

/// What putting a host's log after the end of a cell took out of it ([`Wal::rebase`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Rebased<T> {
    /// The segments the cell holds, landed and named by its head, by their footers, with what
    /// the host keeps of each.
    pub landed: Vec<(Footer, T)>,
}

/// A host's write-ahead log of a cell: its start, the cell's last commit before its first
/// segment, which the guest's history goes on from; then its segments, in order, by their footers,
/// with what the host keeps of each, `T`: its data, or the files holding it.
#[derive(Clone, Debug)]
pub struct Wal<T> {
    start: Option<u64>,
    segments: VecDeque<(Footer, T)>,
}

impl<T> Wal<T> {
    /// An empty log, whose next segment goes on from `start`.
    pub fn new(start: Option<u64>) -> Self {
        Self { start, segments: VecDeque::new() }
    }

    /// cladefs: the log a host's disk held, after `start`, of `segments`, in order, stamped as they
    /// were, which needn't follow each other: what follows the cell is the log's commits, and
    /// [`Wal::rebase`] stamps it anew (TASK-165.9).
    pub fn of(start: Option<u64>, segments: impl IntoIterator<Item = (Footer, T)>) -> Self {
        Self { start, segments: segments.into_iter().collect() }
    }

    /// The cell's last commit before its first segment.
    pub fn start(&self) -> Option<u64> {
        self.start
    }

    /// Its segments, in order.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = &(Footer, T)> {
        self.segments.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// The guest's last commit: the last its segments make, or its start.
    pub fn head(&self) -> Option<u64> {
        self.heads().last().copied().flatten()
    }

    /// Adds a segment its writer closed, the next of its chain.
    pub fn push(&mut self, footer: Footer, data: T) {
        if let Some((last, _)) = self.segments.back() {
            debug_assert_eq!(
                (footer.segment, footer.epoch),
                (last.segment + 1, last.epoch),
                "a chain's"
            );
        }
        self.segments.push_back((footer, data));
    }

    /// Takes out its segments to number `through`, acknowledged, and returns them: its start is
    /// then the last commit they make.
    pub fn acknowledge(&mut self, through: u64) -> Vec<(Footer, T)> {
        let count =
            self.segments.iter().take_while(|(footer, _)| footer.segment <= through).count();
        self.take(count)
    }

    /// Takes out its first `count` segments, and returns them: its start is then the last commit
    /// they make.
    fn take(&mut self, count: usize) -> Vec<(Footer, T)> {
        self.start = self.heads()[count];
        self.segments.drain(..count).collect()
    }

    /// The guest's last commit as of each of its first `n` segments, `n` from none to all: its
    /// start, then the last commit each makes, or the one before's, for a segment that makes none
    /// (its fork's alone).
    fn heads(&self) -> Vec<Option<u64>> {
        let mut head = self.start;
        let mut heads = vec![head];
        for (footer, _) in &self.segments {
            head = footer.contents.commits.last().copied().or(head);
            heads.push(head);
        }
        heads
    }

    /// Puts the log after the end of the cell a writer took over, as `taken` says it found it,
    /// by commits: takes out and returns the segments the cell holds, those to the one ending
    /// its last commit, if it is one of the log's, and after it those making no commit, a fork's
    /// alone, whose forks the cell holds (a head names a writer's segments between commits, so
    /// that a fork's alone, after them, is the cell's as its fork is); and stamps the rest again
    /// on the writer's chain, after the head's end, their data as it was and their extents of their
    /// own at the writer's keys; the writer then takes them as it takes any segment it closed. The
    /// log's start is then the cell's last commit, which what it stamped again follows.
    ///
    /// Fails with [`Error::Diverged`], the log unchanged, if another writer's commits moved the
    /// cell on (`wal_follows`): its last commit is neither one of the log's nor its start. An
    /// empty log never diverges: taking a cell with one is how a host takes it, wherever it was.
    pub fn rebase(&mut self, taken: &TakenCell) -> Result<Rebased<T>, Error> {
        let last = taken.last_commit;
        let heads = self.heads();
        let landed = match heads.iter().position(|&head| head == last) {
            Some(landed) => landed,
            None if self.segments.is_empty() || !rules().wal_follows => 0,
            None => return Err(Error::Diverged),
        };
        let landed = self.take(landed);
        let mut chain = taken.chain;
        for (footer, _) in &mut self.segments {
            *footer = chain.close(footer.contents.clone());
        }
        self.start = taken.last_commit;
        Ok(Rebased { landed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Commit, Data, SegmentBuilder, Unframed};
    use crate::layout::test_cell;
    use crate::layout::{self, Contents, Continued};
    use crate::object_bucket::ObjectBucket;
    use crate::{Writer, head, lifecycle};
    use object_store::memory::InMemory;
    use std::sync::Arc;
    use storage_device::ExtentId;

    fn memory() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""))
    }

    /// A bucket holding cell 1, a root.
    async fn bucket() -> Arc<ObjectBucket> {
        let bucket = memory();
        lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
        bucket
    }

    /// A writer of cell 1, a host's, taking it over as `holder`.
    async fn open(bucket: &Arc<ObjectBucket>, holder: u64) -> Writer<ObjectBucket> {
        Writer::open(bucket.clone(), test_cell(1), holder, 0).await.unwrap()
    }

    /// An empty log of the cell `writer` just took.
    fn log(writer: &Writer<ObjectBucket>) -> Wal<Data> {
        Wal::new(writer.taken().last_commit)
    }

    fn commit(tag: u64) -> Commit {
        Commit { tag, ..Commit::default() }
    }

    /// Closes `batch` into the next segment of `writer`, which takes it, and of `wal`: returns its
    /// number.
    fn close(
        writer: &Writer<ObjectBucket>,
        wal: &mut Wal<Data>,
        batch: impl Into<SegmentBuilder>,
    ) -> u64 {
        let Unframed { footer, data } = batch.into().close(&mut writer.chain());
        let segment = writer.add_segment(data.frame(&footer, writer.keys().encrypting())).unwrap();
        wal.push(footer, data);
        segment
    }

    /// Uploads `writer`'s segment `segment` and acknowledges it, with those before it, as the host
    /// does.
    async fn land(writer: &Writer<ObjectBucket>, segment: u64) {
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
    }

    /// Puts `wal` after the end of the cell `writer` just took, as opening a cell does, and
    /// lands what it stamped again: the commits of the segments it found the cell held.
    async fn rebase(writer: &Writer<ObjectBucket>, wal: &mut Wal<Data>) -> Result<Vec<u64>, Error> {
        let Rebased { landed } = wal.rebase(writer.taken())?;
        for (footer, data) in wal.segments() {
            let segment =
                writer.add_segment(data.frame(footer, writer.keys().encrypting())).unwrap();
            land(writer, segment).await;
        }
        wal.acknowledge(writer.state().through.unwrap());
        Ok(landed.iter().flat_map(|(footer, _)| footer.contents.commits.clone()).collect())
    }

    /// The commits the segments cell 1's head names make, in order: each once, if its history
    /// is whole.
    async fn history(bucket: &Arc<ObjectBucket>) -> Vec<u64> {
        let named = head::get(&**bucket, test_cell(1)).await.unwrap().unwrap_or_default();
        let mut commits = Vec::new();
        for (segment, epoch) in named.segments(0) {
            let key = layout::segment(test_cell(1), segment, epoch);
            let object = crate::bucket::get(&**bucket, &key).await.unwrap().unwrap();
            commits.extend(layout::Segment::footer(&object.bytes).unwrap().contents.commits);
        }
        commits
    }

    /// A log whose segments all landed, though never acknowledged, its host crashing first, is not
    /// the cell's: no head names them (TASK-407). Stamped again whole after the head's end, they
    /// land again, at the new writer's keys, each commit once.
    #[test]
    fn logs_landed_unacknowledged_are_stamped_again() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let segments = [close(&first, &mut wal, commit(1)), close(&first, &mut wal, commit(2))];
            for segment in segments {
                first.upload(segment).await.unwrap();
            }
            let second = open(&bucket, 11).await;
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert!(wal.is_empty());
            assert_eq!(wal.start(), Some(2));
            assert_eq!(history(&bucket).await, [1, 2]);
        });
    }

    /// TASK-165.2's case, which a rebase by the footers recovery read declared diverged: a log
    /// whose segment the head names, which another host's writer then checkpointed past, is the
    /// cell's, though recovery, starting from the checkpoint, reads none of its footers: the head
    /// names it by its number and epoch.
    #[test]
    fn logs_acknowledged_under_a_later_checkpoint_are_the_cells() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let segment = close(&first, &mut wal, commit(1));
            land(&first, segment).await;
            let elsewhere = open(&bucket, 11).await;
            assert_eq!(elsewhere.checkpoint().await.unwrap(), Some(segment));
            let third = open(&bucket, 12).await;
            assert_eq!(rebase(&third, &mut wal).await, Ok(vec![1]));
            assert!(wal.is_empty());
            assert_eq!(third.last_commit(), Some(1));
        });
    }

    /// A log stamped again by a writer taking the cell, in memory alone, as a cell keeps it
    /// (TASK-165.9), which then landed it, under its own epoch, and acknowledged it, the host
    /// crashing before its log let go of it: the next writer finds it stamped as before, at keys
    /// none of what landed is at, and finds it the cell's by its commits (found by local_cell's
    /// crash rounds once the log was put after the cell by the keys the head names).
    #[test]
    fn logs_stamped_again_in_memory_are_the_cells_by_their_commits() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            close(&first, &mut wal, commit(1));
            close(&first, &mut wal, commit(2));
            let on_disk = wal.clone();
            let second = open(&bucket, 11).await;
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1, 2]);
            let mut again = on_disk;
            let third = open(&bucket, 12).await;
            assert_eq!(rebase(&third, &mut again).await, Ok(vec![1, 2]));
            assert!(again.is_empty());
            assert_eq!(history(&bucket).await, [1, 2]);
        });
    }

    /// A log whose first segment the head names, the second not: the first is the cell's, the
    /// second is stamped again after the head's end, and lands, each commit once.
    #[test]
    fn logs_partly_acknowledged_go_on_after_the_heads_end() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let segment = close(&first, &mut wal, commit(1));
            let second_segment = close(&first, &mut wal, commit(2));
            land(&first, segment).await;
            first.upload(second_segment).await.unwrap();
            let second = open(&bucket, 11).await;
            let chain = second.taken().chain;
            assert_eq!(chain.next_segment(), segment + 1, "the head's end");
            assert_eq!(wal.rebase(second.taken()).map(|rebased| rebased.landed.len()), Ok(1));
            let [(footer, _)] = &wal.segments().collect::<Vec<_>>()[..] else {
                panic!("{} segments stamped again", wal.segments().len())
            };
            let place = (footer.segment, footer.epoch, footer.contents.commits.clone());
            assert_eq!(place, (chain.next_segment(), second.epoch(), vec![2]));
            assert_eq!(wal.start(), Some(1));
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1, 2]);
            assert_eq!(second.last_commit(), Some(2));
        });
    }

    /// cladefs: a log whose commit spans two segments, the first landed, the second not: no head
    /// names the first, as its writer acknowledges whole commits alone, so both are stamped again
    /// after the head's end, and land, the commit once, its runs in both sealed (TASK-92.41).
    #[test]
    fn logs_of_commits_spanning_segments_land_whole() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let extent = |sequence| ExtentId::new(1, sequence);
            let mut close_contents = |contents: Contents| {
                let footer = first.chain().close(contents);
                let data = Data::default();
                let segment =
                    first.add_segment(data.frame(&footer, first.keys().encrypting())).unwrap();
                wal.push(footer, data);
                segment
            };
            let continued = Continued { commit: 1, extents: vec![(extent(1), 0, 4096)] };
            let segment =
                close_contents(Contents { continued: Some(continued), ..Contents::default() });
            let ending = Contents {
                commits: vec![1],
                extents: vec![(extent(2), 0, 4096)],
                next_sequence: 3,
                ..Contents::default()
            };
            close_contents(ending);
            first.upload(segment).await.unwrap();
            let second = open(&bucket, 11).await;
            assert_eq!(second.taken().chain.next_segment(), segment, "the head names neither");
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1]);
            let sealed: Vec<ExtentId> = second.state().extents.keys().copied().collect();
            assert_eq!(sealed, [extent(1), extent(2)]);
        });
    }

    /// cladefs: a log whose segment ending commit 1 goes on with commit 2, landed alone, the
    /// segment ending commit 2 not: no head names it, as its writer acknowledges whole commits
    /// alone, so both are stamped again after the head's end, and land, each commit once, both
    /// runs sealed (TASK-345's case, which diverged when the log was put after the cell by
    /// commits, though no other writer moved the cell on).
    #[test]
    fn logs_whose_segment_ending_a_commit_and_going_on_landed_alone_go_on() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let extent = |sequence| ExtentId::new(1, sequence);
            let mut close_contents = |contents: Contents| {
                let footer = first.chain().close(contents);
                let data = Data::default();
                let segment =
                    first.add_segment(data.frame(&footer, first.keys().encrypting())).unwrap();
                wal.push(footer, data);
                segment
            };
            let continued = Continued { commit: 2, extents: vec![(extent(2), 4096, 4096)] };
            let segment = close_contents(Contents {
                commits: vec![1],
                extents: vec![(extent(1), 0, 4096)],
                next_sequence: 2,
                continued: Some(continued.clone()),
                ..Contents::default()
            });
            let ending = Contents {
                commits: vec![2],
                extents: vec![(extent(3), 0, 4096)],
                next_sequence: 4,
                ..Contents::default()
            };
            close_contents(ending);
            first.upload(segment).await.unwrap();
            assert_eq!(first.acknowledge().await, Ok(None), "no whole commit to acknowledge");
            let second = open(&bucket, 11).await;
            assert_eq!(second.taken().last_commit, None);
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1, 2]);
            let sealed: Vec<ExtentId> = second.state().extents.keys().copied().collect();
            assert_eq!(sealed, [extent(1), extent(2), extent(3)]);
            assert_eq!(second.last_commit(), Some(2));
        });
    }

    /// A log whose second segment landed, its first not: neither is acknowledged, a writer
    /// acknowledging in order, and both are stamped again after the head's end.
    #[test]
    fn logs_landed_past_a_gap_are_stamped_again() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            close(&first, &mut wal, commit(1));
            let segment = close(&first, &mut wal, commit(2));
            first.upload(segment).await.unwrap();
            let second = open(&bucket, 11).await;
            assert_eq!(second.taken().chain.next_segment(), segment - 1, "the head names neither");
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1, 2]);
        });
    }

    /// A log none of whose segments landed is stamped again whole: its extents in its data as they
    /// were, those of their own at the new writer's keys, each read back as written.
    #[test]
    fn logs_none_landed_are_stamped_again_whole() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let sequence = first.chain().next_sequence();
            let (inside, own) = (ExtentId::new(1, sequence), ExtentId::new(1, sequence + 1));
            let extents = vec![(inside, b"inside".to_vec())];
            let owns = vec![(own.sequence, b"own".to_vec())];
            close(&first, &mut wal, Commit { tag: 1, extents, own: owns, ..Commit::default() });
            close(&first, &mut wal, commit(2));
            let second = open(&bucket, 11).await;
            assert_eq!(wal.rebase(second.taken()).map(|rebased| rebased.landed.len()), Ok(0));
            let (footer, _) = wal.segments().next().unwrap();
            let key = layout::extent(layout::TEST_CELL, second.epoch(), footer.segment, own);
            assert_eq!(footer.own().map(|(_, location)| location.key).collect::<Vec<_>>(), [key]);
            assert_eq!(footer.contents.next_sequence, second.chain().next_sequence());
            assert_eq!(rebase(&second, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1, 2]);
            let state = second.state();
            for (extent, bytes) in [(inside, &b"inside"[..]), (own, b"own")] {
                assert_eq!(
                    crate::testing::read(&bucket, extent, &state.extents[&extent]).await,
                    Ok(bytes.into())
                );
            }
        });
    }

    /// A log after whose start another writer's commit came, acknowledged, diverged: unchanged,
    /// its commits put after no other's.
    #[test]
    fn logs_another_writer_moved_on_from_diverge() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            close(&first, &mut wal, commit(1));
            let elsewhere = open(&bucket, 11).await;
            let segment = close(&elsewhere, &mut log(&elsewhere), commit(3));
            land(&elsewhere, segment).await;
            let third = open(&bucket, 12).await;
            assert_eq!(rebase(&third, &mut wal).await, Err(Error::Diverged));
            let commits: Vec<_> = wal.segments().map(|(f, _)| f.contents.commits.clone()).collect();
            assert_eq!((wal.start(), commits), (None, vec![vec![1]]));
            assert_eq!(history(&bucket).await, [3]);
        });
    }

    /// TASK-165.2's other case: a log after which only another writer's taking came, a takeover
    /// that went no further, goes on after the head's end.
    #[test]
    fn logs_only_a_takeover_came_after_go_on() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            close(&first, &mut wal, commit(1));
            drop(open(&bucket, 11).await);
            let third = open(&bucket, 12).await;
            assert_eq!(rebase(&third, &mut wal).await, Ok(vec![]));
            assert_eq!(history(&bucket).await, [1]);
        });
    }

    /// An empty log never diverges, wherever the cell went: its start is the cell's last
    /// commit.
    #[test]
    fn empty_logs_go_on_from_the_cells_end() {
        futures::executor::block_on(async {
            let bucket = bucket().await;
            let first = open(&bucket, 10).await;
            let mut wal = log(&first);
            let elsewhere = open(&bucket, 11).await;
            let segment = close(&elsewhere, &mut log(&elsewhere), commit(3));
            land(&elsewhere, segment).await;
            let third = open(&bucket, 12).await;
            assert_eq!(rebase(&third, &mut wal).await, Ok(vec![]));
            assert_eq!((wal.start(), wal.head()), (Some(3), Some(3)));
        });
    }

    /// Acknowledging takes out the segments to the number acknowledged; the start is the last
    /// commit they make, or, those making none, the start before.
    #[test]
    fn acknowledged_segments_leave_the_log() {
        let mut chain = Chain::new(test_cell(1), 2, 5, 0);
        let mut wal = Wal::new(Some(7));
        let mut push = |commits: Vec<u64>| {
            wal.push(chain.close(Contents { commits, ..Contents::default() }), ());
        };
        push(vec![]);
        push(vec![8, 9]);
        push(vec![]);
        push(vec![10]);
        assert_eq!((wal.start(), wal.head()), (Some(7), Some(10)));
        let numbers = |taken: Vec<(Footer, ())>| -> Vec<u64> {
            taken.iter().map(|(footer, _)| footer.segment).collect()
        };
        assert_eq!(numbers(wal.acknowledge(5)), [5]);
        assert_eq!(wal.start(), Some(7));
        assert_eq!(numbers(wal.acknowledge(7)), [6, 7]);
        assert_eq!((wal.start(), wal.head()), (Some(9), Some(10)));
        assert_eq!(numbers(wal.acknowledge(7)), Vec::<u64>::new());
        assert_eq!(numbers(wal.acknowledge(20)), [8]);
        assert_eq!((wal.start(), wal.head(), wal.is_empty()), (Some(10), Some(10), true));
    }
}
