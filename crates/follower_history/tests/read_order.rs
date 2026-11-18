// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-534: named and held reads by the same reader share one observation order.

use follower_history::{File, HeldRead, History, Read};

fn observed(
    first_held: bool,
    second_held: bool,
    observations: [(usize, u8); 2],
) -> Result<(), String> {
    let mut history = History::new();
    history.operation(3, [("a", Some(File { object_id: 100, contents: 1 }))]);
    history.synced(5);
    history.operation(6, [("a", Some(File { object_id: 100, contents: 2 }))]);
    for (held, (reader, value)) in [first_held, second_held].into_iter().zip(observations) {
        if held {
            history.held_read(HeldRead {
                reader,
                commits: 7..=7,
                object_id: 100,
                found: Ok(value),
            });
        } else {
            history.read(Read { reader, commits: 7..=7, name: "a", found: Ok(Some(value)) });
        }
    }
    history.check()
}

#[test]
fn a_reader_moves_forward_across_named_and_held_reads() {
    for first in [false, true] {
        for second in [false, true] {
            assert!(observed(first, second, [(0, 1), (0, 2)]).is_ok());
            assert!(observed(first, second, [(0, 2), (1, 1)]).is_ok());
            assert!(
                observed(first, second, [(0, 2), (0, 1)]).is_err(),
                "accepted a backwards read: first held={first}, second held={second}"
            );
        }
    }
}
