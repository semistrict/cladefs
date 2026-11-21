// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: views of a cell as of a commit (TASK-42.2): what they read while the writer goes on,
//! what they keep of its frees, and for how long, their pins, and their cutting off. Their
//! concurrency with the writer is tests/shuttle/main.rs's.

use extent_frame::segment::Location;
use local_cell::{BucketData, CutOff, Description, LocalCell, MemDir, NotHeld, Options, View};
use std::time::Duration;
use storage_device::{ExtentId, NoExtent};

const BLOCK: usize = 4096;
const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

/// Small segments and large extents, so that tests make several of each.
const OPTIONS: Options = Options {
    max_extent: 64 << 20,
    segment_size: 64 << 10,
    segment_age: Duration::from_secs(3600),
    large_extent: 32 << 10,
    open_files: 2,
    view_lag: None,
    view_kept_bytes: None,
    held_most: 256 << 10,
};

fn extent(sequence: u64) -> ExtentId {
    ExtentId::new(CELL.id, sequence)
}

/// `blocks` blocks of bytes each their offset modulo 251, plus `seed`.
fn pattern(seed: u8, blocks: usize) -> Vec<u8> {
    (0..blocks * BLOCK).map(|i| (i % 251) as u8 ^ seed).collect()
}

fn create(options: Options) -> LocalCell<MemDir> {
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 20, cell: CELL };
    LocalCell::create(MemDir::new(), description, options).unwrap()
}

/// Writes extent `sequence`, of `blocks` blocks of its pattern.
async fn write(store: &LocalCell<MemDir>, sequence: u64, blocks: usize) {
    store.write(extent(sequence), 0, &pattern(sequence as u8, blocks)).await.unwrap();
}

/// Extent `sequence`, as `view` reads `blocks` blocks of it.
fn read(
    store: &LocalCell<MemDir>,
    view: &View,
    sequence: u64,
    blocks: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    let mut data = vec![0; blocks * BLOCK];
    store.read_view(view, extent(sequence), 0, &mut data)?;
    Ok(data)
}

fn error_is<E: std::error::Error + PartialEq + Send + Sync + 'static, T>(
    result: Result<T, anyhow::Error>,
    expected: E,
) {
    let Err(error) = result else { panic!("succeeded, expecting {expected:?}") };
    assert_eq!(error.downcast_ref::<E>(), Some(&expected), "{error:?}");
}

/// The bytes of frees views keep, which count as used: as the store's usage says, and its
/// `kept_for_views` alone (TASK-92.17).
fn kept(store: &LocalCell<MemDir>) -> u64 {
    let kept = store.usage().unwrap().kept_for_views;
    assert_eq!(store.kept_for_views().unwrap(), kept);
    assert_eq!(store.info().unwrap().used, store_bytes(store) + kept);
    kept
}

/// The bytes the store's extents hold.
fn store_bytes(store: &LocalCell<MemDir>) -> u64 {
    store.extents().unwrap().iter().map(|(_, len)| len).sum()
}

/// cladefs: a view reads what its commit holds that the writer freed after it in the bucket, once
/// the segment holding it is acknowledged there, and leaves the store: its read fails saying where
/// in the bucket the data is ([`BucketData`]), for the fetcher to read there, as the store's own
/// do (TASK-165.8). What the uploader kept of the frees, the view keeps, as before.
#[tokio::test(flavor = "multi_thread")]
async fn views_read_frees_acknowledged_in_the_bucket() {
    let store = create(OPTIONS);
    write(&store, 1, 2).await;
    store.commit(None).await.unwrap();
    let view = store.view();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    assert!(store.close_now().await.unwrap());
    store.flush().await.unwrap();
    let closed = store.closed(usize::MAX).await.unwrap();
    let keys: Vec<String> = (closed.iter())
        .map(|closed| {
            bucket_cell::layout::segment(
                closed.footer.cell,
                closed.footer.segment,
                closed.footer.epoch,
            )
        })
        .collect();
    for closed in &closed {
        store.acknowledged(closed.number).await.unwrap();
    }
    assert_eq!(store.usage().unwrap().segments, 1);
    let location = Location { key: keys[0].clone(), offset: 0, len: 2 * BLOCK as u64 };
    error_is(read(&store, &view, 1, 2), BucketData { extent: extent(1), location });
}

/// A view reads the commit it is of, and its root, while the writer frees what it holds, in
/// segments and in files of their own, writes more and commits.
#[tokio::test(flavor = "multi_thread")]
async fn views_read_the_commit_they_are_of() {
    let store = create(OPTIONS);
    write(&store, 1, 2).await;
    // Large: in a file of its own.
    write(&store, 2, 10).await;
    write(&store, 3, 1).await;
    store.commit(Some(b"one")).await.unwrap();
    let view = store.view();
    assert_eq!((view.commit(), view.root()), (1, &b"one"[..]));

    store.free(extent(1)).unwrap();
    store.free(extent(2)).unwrap();
    write(&store, 4, 3).await;
    store.commit(Some(b"two")).await.unwrap();
    write(&store, 5, 1).await;
    store.commit(None).await.unwrap();

    assert_eq!(read(&store, &view, 1, 2).unwrap(), pattern(1, 2));
    assert_eq!(read(&store, &view, 2, 10).unwrap(), pattern(2, 10));
    assert_eq!(read(&store, &view, 3, 1).unwrap(), pattern(3, 1));
    assert_eq!((view.commit(), view.root()), (1, &b"one"[..]));
    // A view opened now is of the latest commit, and its root.
    let latest = store.view();
    assert_eq!((latest.commit(), latest.root()), (3, &b"two"[..]));
    assert_eq!(read(&store, &latest, 4, 3).unwrap(), pattern(4, 3));
}

/// The frees after a view's commit are kept, and count as used, until it advances past them, or
/// goes: then the next commit lets them go.
#[tokio::test(flavor = "multi_thread")]
async fn kept_frees_go_once_views_advance_or_drop() {
    let store = create(OPTIONS);
    write(&store, 1, 3).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 3 * BLOCK as u64);

    view.advance().unwrap();
    assert_eq!(view.commit(), 2);
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
    error_is(read(&store, &view, 1, 3), NoExtent(extent(1)));

    drop(view);
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    let other = store.view();
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), BLOCK as u64);
    drop(other);
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
    // cladefs: kept still for the bucket, until the segments holding them are acknowledged there
    // (TASK-165.9).
    assert_eq!(store.usage().unwrap().kept_bytes, 4 * BLOCK as u64);
}

/// What views keep takes room as writes in progress do: it is used, and not available.
#[tokio::test(flavor = "multi_thread")]
async fn kept_frees_take_room() {
    let store = create(OPTIONS);
    write(&store, 1, 3).await;
    store.commit(None).await.unwrap();
    let _view = store.view();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    let write = store.begin_write(extent(2), 0, 2 * BLOCK as u64).await.unwrap();
    let info = store.info().unwrap();
    assert_eq!(info.used, 3 * BLOCK as u64);
    assert_eq!(info.available, info.capacity - 5 * BLOCK as u64);
    drop(write);
}

/// A view locates the parts of what it holds, freed since, as [`LocalCell::locate`] does what the
/// store holds; an open extent it holds not.
#[tokio::test(flavor = "multi_thread")]
async fn views_locate_what_they_hold() {
    let store = create(OPTIONS);
    write(&store, 1, 3).await;
    store.commit(None).await.unwrap();
    let view = store.view();
    let held = store.locate(extent(1), 0, 3 * BLOCK as u64).unwrap();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    let parts = store.locate_view(&view, extent(1), 0, 3 * BLOCK as u64).unwrap();
    assert_eq!(parts.len(), held.len());
    assert_eq!(parts.iter().map(|part| part.len()).sum::<u64>(), 3 * BLOCK as u64);
    write(&store, 2, 1).await;
    error_is(store.locate_view(&view, extent(2), 0, BLOCK as u64), NoExtent(extent(2)));
}

/// A view opened after a free, but before the commit that frees it, is of the commit before,
/// which holds it: it is kept. One opened after that commit keeps nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn views_keep_what_their_commit_holds() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    store.free(extent(1)).unwrap();
    let before = store.view();
    store.commit(None).await.unwrap();
    assert_eq!(read(&store, &before, 1, 1).unwrap(), pattern(1, 1));
    drop(before);
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();
    let after = store.view();
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
    error_is(read(&store, &after, 2, 1), NoExtent(extent(2)));
}

/// A view refuses extents its commit doesn't hold, sealed by a later one: a follower's journal
/// replay reads on until it finds nothing, and must end at its view's commit. Those sealed while
/// no view lived are older than any view.
#[tokio::test(flavor = "multi_thread")]
async fn views_refuse_extents_of_later_commits() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    error_is(read(&store, &view, 2, 1), NoExtent(extent(2)));
    view.advance().unwrap();
    assert_eq!(read(&store, &view, 2, 1).unwrap(), pattern(2, 1));
    drop(view);
    write(&store, 3, 1).await;
    store.commit(None).await.unwrap();
    let view = store.view();
    assert_eq!(read(&store, &view, 3, 1).unwrap(), pattern(3, 1));
}

/// An extent freed while open, before a commit sealed it, was in no view's commit: it is not kept.
#[tokio::test(flavor = "multi_thread")]
async fn open_extents_freed_are_not_kept() {
    let store = create(OPTIONS);
    let _view = store.view();
    write(&store, 1, 2).await;
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
}

/// A duplicate of a view is as of its commit, and keeps its frees when it goes, but not its pins.
#[tokio::test(flavor = "multi_thread")]
async fn duplicates_keep_what_their_commit_holds() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    let view = store.view();
    view.pin(extent(2)).unwrap();
    let copy = view.duplicate().unwrap();
    assert_eq!((copy.commit(), copy.root()), (view.commit(), view.root()));
    store.free(extent(1)).unwrap();
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();
    drop(view);
    store.commit(None).await.unwrap();
    assert_eq!(read(&store, &copy, 1, 1).unwrap(), pattern(1, 1));
    assert_eq!(read(&store, &copy, 2, 1).unwrap(), pattern(2, 1));
}

/// Of the frees kept, a view reads only those after its commit, which its commit holds: not one
/// freed before, which an older view keeps.
#[tokio::test(flavor = "multi_thread")]
async fn views_read_only_what_their_commit_holds() {
    let store = create(OPTIONS);
    write(&store, 1, 2).await;
    store.commit(None).await.unwrap();
    let older = store.view();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    let newer = store.view();
    assert_eq!(read(&store, &older, 1, 2).unwrap(), pattern(1, 2));
    error_is(read(&store, &newer, 1, 2), NoExtent(extent(1)));
    assert_eq!(store.view_extent(&older, extent(1)).unwrap(), 2 * BLOCK as u64);
    error_is(store.view_extent(&newer, extent(1)), NoExtent(extent(1)));
    // But for what it pins.
    newer.pin(extent(1)).unwrap();
    assert_eq!(read(&store, &newer, 1, 2).unwrap(), pattern(1, 2));
}

/// A view advanced keeping holds what the commits since the one it was as of held, those
/// between included, until it lets go of them: what a follower applying them reads.
#[tokio::test(flavor = "multi_thread")]
async fn views_advanced_keeping_hold_the_commits_between() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    store.free(extent(1)).unwrap();
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();
    view.advance_keeping().unwrap();
    assert_eq!(view.commit(), store.last_commit().unwrap());
    assert_eq!(read(&store, &view, 1, 1).unwrap(), pattern(1, 1));
    assert_eq!(read(&store, &view, 2, 1).unwrap(), pattern(2, 1));
    view.let_go(view.commit());
    assert_eq!(view.since(), view.commit());
    error_is(read(&store, &view, 1, 1), NoExtent(extent(1)));
    error_is(read(&store, &view, 2, 1), NoExtent(extent(2)));
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
}

/// A view lets go of the commits before the one named, which a let-go arriving after its next
/// advance names: it holds the commits since that one still, and a let-go of an earlier one
/// changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn views_let_go_of_the_commits_before_the_one_named() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    write(&store, 2, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    let first = view.commit();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    view.advance_keeping().unwrap();
    let second = view.commit();
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();
    view.advance_keeping().unwrap();

    view.let_go(second);
    assert_eq!(view.since(), second);
    error_is(read(&store, &view, 1, 1), NoExtent(extent(1)));
    assert_eq!(read(&store, &view, 2, 1).unwrap(), pattern(2, 1));
    view.let_go(first);
    assert_eq!(view.since(), second);
    view.let_go(u64::MAX);
    assert_eq!(view.since(), view.commit());
    error_is(read(&store, &view, 2, 1), NoExtent(extent(2)));
}

/// An extent's length as of a view: of one its commit holds, still or freed since; not of one
/// written after it.
#[tokio::test(flavor = "multi_thread")]
async fn views_know_their_extents_lengths() {
    let store = create(OPTIONS);
    write(&store, 1, 3).await;
    store.commit(None).await.unwrap();
    let view = store.view();
    write(&store, 2, 1).await;
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(store.view_extent(&view, extent(1)).unwrap(), 3 * BLOCK as u64);
    error_is(store.view_extent(&view, extent(2)), NoExtent(extent(2)));
}

/// A view waits for a commit after its own: what tells a follower to advance.
#[tokio::test(flavor = "multi_thread")]
async fn views_wait_for_newer_commits() {
    let store = std::sync::Arc::new(create(OPTIONS));
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    assert_eq!(view.newer(Duration::from_millis(10)).await.unwrap(), None);
    let committing = {
        let store = store.clone();
        tokio::spawn(async move {
            write(&store, 2, 1).await;
            store.commit(None).await.unwrap();
            store.last_commit().unwrap()
        })
    };
    let newer = view.newer(Duration::from_secs(60)).await.unwrap();
    let epoch = committing.await.unwrap();
    assert_eq!(newer, Some(epoch));
    view.advance().unwrap();
    assert_eq!(view.newer(Duration::from_millis(10)).await.unwrap(), None);
}

/// A pinned extent is kept, whatever commit its view advances to, until unpinned.
#[tokio::test(flavor = "multi_thread")]
async fn pins_keep_extents_past_advances() {
    let store = create(OPTIONS);
    write(&store, 1, 2).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    view.pin(extent(1)).unwrap();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    view.advance().unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(read(&store, &view, 1, 2).unwrap(), pattern(1, 2));
    assert_eq!(kept(&store), 2 * BLOCK as u64);
    assert_eq!(view.pinned(), vec![extent(1)]);

    view.unpin(extent(1));
    assert_eq!(view.pinned(), vec![]);
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
    error_is(read(&store, &view, 1, 2), NoExtent(extent(1)));
}

/// A view stale for `view_lag` is cut off: its reads, advances and pins fail, and it keeps
/// nothing. Views opened since are not.
#[tokio::test(flavor = "multi_thread")]
async fn views_lagging_too_long_are_cut_off() {
    let store = create(Options { view_lag: Some(Duration::ZERO), ..OPTIONS });
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    assert_eq!(read(&store, &view, 1, 1).unwrap(), pattern(1, 1));
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();

    error_is(read(&store, &view, 1, 1), CutOff);
    error_is(view.advance(), CutOff);
    error_is(view.pin(extent(1)), CutOff);
    assert_eq!(kept(&store), 0);
    let fresh = store.view();
    write(&store, 2, 1).await;
    assert_eq!(fresh.commit(), 2);
    error_is(read(&store, &fresh, 2, 1), NoExtent(extent(2)));
}

/// A store's views, as the followers' measurements report them: how many, how many commits the
/// furthest behind is, and that it has been behind since; none once cut off.
#[tokio::test(flavor = "multi_thread")]
async fn views_report_how_far_behind_they_are() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    assert_eq!(store.view_stats(), local_cell::ViewStats::default());
    let mut behind = store.view();
    let _latest = store.view();
    store.commit(None).await.unwrap();
    store.commit(None).await.unwrap();
    let stats = store.view_stats();
    assert_eq!((stats.views, stats.behind), (2, 2));
    assert!(stats.stale_for > Duration::ZERO, "{stats:?}");
    behind.advance().unwrap();
    store.cut_views();
    assert_eq!(store.view_stats(), local_cell::ViewStats::default());
}

/// Views the store cuts off (its writer gone with writes uncommitted) fail from then on, and the
/// next commit keeps nothing for them; views opened after aren't cut off.
#[tokio::test(flavor = "multi_thread")]
async fn views_cut_off_by_the_store_fail() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    let mut view = store.view();
    store.free(extent(1)).unwrap();
    store.cut_views();

    error_is(read(&store, &view, 1, 1), CutOff);
    error_is(view.advance(), CutOff);
    error_is(view.pin(extent(1)), CutOff);
    error_is(view.newer(Duration::ZERO).await, CutOff);
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), 0);
    let fresh = store.view();
    assert_eq!(fresh.commit(), 2);
    assert_eq!(fresh.newer(Duration::ZERO).await.unwrap(), None);
}

/// Views keeping more than `view_kept_bytes` of frees are cut off, the oldest first, until those
/// left keep no more.
#[tokio::test(flavor = "multi_thread")]
async fn views_keeping_too_much_are_cut_off() {
    let store = create(Options { view_kept_bytes: Some(2 * BLOCK as u64), ..OPTIONS });
    write(&store, 1, 1).await;
    write(&store, 2, 2).await;
    store.commit(None).await.unwrap();
    let old = store.view();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    assert_eq!(kept(&store), BLOCK as u64);
    let young = store.view();
    store.free(extent(2)).unwrap();
    store.commit(None).await.unwrap();

    // The old view would keep both, three blocks; the young one, the second's two.
    error_is(read(&store, &old, 1, 1), CutOff);
    assert_eq!(read(&store, &young, 2, 2).unwrap(), pattern(2, 2));
    assert_eq!(kept(&store), 2 * BLOCK as u64);
}

/// A view's duplicate is a view of its own: counted, and let go of, on its own (TASK-44.1).
#[test]
fn duplicates_are_views_of_their_own() {
    let store = create(OPTIONS);
    let view = store.view();
    let duplicate = view.duplicate().unwrap();
    let another = store.view();
    assert_eq!(store.view_stats().views, 3);
    drop(duplicate);
    assert_eq!(store.view_stats().views, 2);
    drop((view, another));
    assert_eq!(store.view_stats().views, 0);
}

/// A view reading an extent freed before its commit, kept for another, finds it not held: it let
/// go of it; another reader finds no extent, and no view's letting go.
#[tokio::test(flavor = "multi_thread")]
async fn views_find_what_they_let_go_of_not_held() {
    let store = create(OPTIONS);
    write(&store, 1, 1).await;
    store.commit(None).await.unwrap();
    store.free(extent(1)).unwrap();
    store.commit(None).await.unwrap();
    let view = store.view();
    let error = read(&store, &view, 1, 1).unwrap_err();
    assert_eq!(error.downcast_ref::<NotHeld>(), Some(&NotHeld(extent(1))), "{error:?}");
    let error = store.read(extent(1), 0, &mut [0; BLOCK]).unwrap_err();
    assert_eq!(error.downcast_ref::<NotHeld>(), None, "{error:?}");
    assert_eq!(error.downcast_ref::<NoExtent>(), Some(&NoExtent(extent(1))), "{error:?}");
}
