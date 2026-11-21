// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a test's host's runtime, its uploaders' alone, beside the test's own, which the test
//! awaits on (board 2948): so that a host lost, or crashed, takes its uploads with it.

/// A host's runtime of its own, of two threads. Stopped as it is dropped, its tasks let go of where
/// they are: a runtime's drop, which waits for them, may not happen in a test's async body.
pub struct HostRuntime(Option<tokio::runtime::Runtime>);

impl HostRuntime {
    pub fn new() -> Self {
        let mut building = tokio::runtime::Builder::new_multi_thread();
        Self(Some(building.worker_threads(2).enable_all().build().unwrap()))
    }

    /// What the host's uploaders spawn on.
    pub fn handle(&self) -> &tokio::runtime::Handle {
        self.0.as_ref().expect("a host's runtime, running").handle()
    }

    /// Stops it, as its host is lost: nothing of its tasks goes on.
    #[allow(dead_code, reason = "the crash tests' hosts crash, their runtimes going on")]
    pub fn stop(mut self) {
        self.0.take().expect("a host's runtime, running").shutdown_background();
    }
}

impl Drop for HostRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}
