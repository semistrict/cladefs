// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

#[test]
fn cancelling_the_last_reader_releases_a_cold_tail_slot() {
    let data = noise(PART);
    let bucket = bucket(&data);
    let fetcher = Fetcher::new(bucket.clone(), limits(1), memory(), None);
    let abandoned =
        "tenants/0000000000001/cells/0000000000001/segments/0000000000002.0000000000000";
    // Poll through admission, then drop the only caller during its GET.
    assert!(fetcher.stored_part(abandoned, 0).now_or_never().is_none());
    assert_eq!(bucket.waiting(), 1);
    bucket.complete(0, Fault::None);

    let mut reads = Reads::new();
    let read = reads.read(&Rc::new(fetcher), location(PART), 0, 1, Priority::Demand);
    complete_all(&bucket, &mut reads);
    assert_eq!(reads.result(read), Some(Ok(data[..1].to_vec())));
}

#[test]
fn cancelling_one_cold_tail_reader_keeps_the_other() {
    let data = noise(PART);
    let bucket = bucket(&data);
    let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(1), memory(), None));
    let mut reads = Reads::new();
    let first = reads.read(&fetcher, location(PART), 0, 1, Priority::Demand);
    let second = reads.read(&fetcher, location(PART), 1, 1, Priority::Demand);
    reads.abandon(first);
    let gets = complete_all(&bucket, &mut reads);
    assert_eq!(reads.result(second), Some(Ok(data[1..2].to_vec())));
    assert_eq!(gets.len(), 2, "one tail and one part");
}

#[test]
fn unexpected_cache_read_errors_do_not_issue_gets() {
    let data = noise(PART);
    let mut results = Vec::new();
    for warm_tail in [false, true] {
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fresh =
            || Rc::new(Fetcher::new(bucket.clone(), limits(1), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let read = reads.read(&fresh(), location(PART), 0, 1, Priority::Demand);
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(read), Some(Ok(data[..1].to_vec())));

        let fetcher = fresh();
        if warm_tail {
            assert!(block_on(land(fetcher.shared.tail(KEY))).unwrap().is_some());
        }
        // A directory in place of the object produces EISDIR for a local read.
        let path = cache.path(KEY);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let read = reads.read(&fetcher, location(PART), 0, 1, Priority::Demand);
        // Finish any unexpected GETs before checking the outcome, so both local
        // error paths run and no in-flight operation outlives its cache directory.
        let gets = complete_all(&bucket, &mut reads);
        results.push((gets.len(), matches!(reads.result(read), Some(Err(_)))));
    }
    assert_eq!(results, [(0, true), (0, true)], "(GET count, read failed) for cold and warm tails");
}
