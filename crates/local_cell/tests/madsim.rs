// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.3: under madsim, a wait on another task's work fails at once, naming itself
//! (`local_cell::blocks!`), rather than stopping the simulation's one thread for good. Built with
//! `RUSTFLAGS="--cfg madsim"`.

#![cfg(madsim)]

/// The bucket runtime's `block_on`, which the daemon's paths are rid of step by step (doc-24).
#[test]
#[should_panic(expected = "would block the simulation's one thread: upload::block_on")]
fn blocking_on_the_bucket_runtime_fails_in_the_simulation() {
    let _ = local_cell::upload::block_on(std::future::ready(()));
}
