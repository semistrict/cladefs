// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

#[test]
fn free_space_admission_reserves_space_for_unwritten_fills() {
    for pending_tail in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART, 0).unwrap());
        let object = Object::noise(KEY, PART, 1);
        if !pending_tail {
            object.keep_tail(&cache);
        }
        let needed = if pending_tail { object.tail_len() } else { object.stored(0).len };

        let scenario = fail::FailScenario::setup();
        let (paused, started) = std::sync::mpsc::sync_channel(0);
        let (resume, resumed) = std::sync::mpsc::sync_channel(0);
        let resumed = std::sync::Mutex::new(resumed);
        fail::cfg_callback("direct::bypass", move || {
            if std::thread::current().name() == Some("paused cache fill") {
                paused.send(()).unwrap();
                resumed.lock().unwrap().recv().unwrap();
            }
        })
        .unwrap();
        let writer = {
            let cache = cache.clone();
            std::thread::Builder::new()
                .name("paused cache fill".into())
                .spawn(move || {
                    if pending_tail {
                        object.keep_tail(&cache);
                    } else {
                        object.keep_part(&cache, 0);
                    }
                })
                .unwrap()
        };
        // The fill is admitted, but has yet to write any disk blocks. Present the next
        // admission with a deterministic free-space sample: room for exactly one fill.
        started.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let floor = 16 * PART;
        let (fits, evicted) = cache.kept.lock().unwrap().make_room(
            (cache.capacity(), floor),
            floor + needed,
            needed,
            needed,
        );
        resume.send(()).unwrap();
        writer.join().unwrap();
        scenario.teardown();
        assert!(evicted.is_empty());
        assert!(!fits, "pending tail = {pending_tail}: two fills spent the same space");
    }
}
