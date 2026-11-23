// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the runtime a real KMS's calls run on, one of its own (board 8477): OpenBao's,
//! AWS KMS's, Google Cloud KMS's and Azure Key Vault's. Their futures then run whatever polls them,
//! of any executor or of none (`futures::executor::block_on`), waiting for the call alone; and
//! their HTTP clients' connections, and the tasks driving them, are this runtime's, never one a
//! caller made and dropped since, which would leave the client holding connections that fail.

use super::Error;
use std::future::Future;
use std::io;

/// A KMS's runtime, one thread of its own, named for its KMS, shut down without waiting when
/// dropped: whatever drops it, an async task's included, where a runtime may not be waited for.
pub(super) struct Background(Option<tokio::runtime::Runtime>);

impl Background {
    /// A runtime of one thread, named `name`.
    pub fn new(name: &str) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(name)
            .enable_all()
            .build()?;
        Ok(Self(Some(runtime)))
    }

    /// Runs `call`, a call for `tenant`, on it: dropped, its future aborts the call. A call that
    /// panicked panics its caller; one its runtime's shutdown cancelled fails as the KMS's.
    pub fn run<T: Send + 'static>(
        &self,
        tenant: u64,
        call: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> impl Future<Output = Result<T, Error>> + Send + 'static {
        let handle = self.0.as_ref().expect("a runtime until dropped").handle().clone();
        async move {
            let mut task = Aborting(handle.spawn(call));
            match (&mut task.0).await {
                Ok(done) => done,
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => Err(Error::Io {
                    tenant,
                    source: io::Error::other(
                        "the KMS's call was cancelled: its runtime is shut down",
                    ),
                }),
            }
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// A task, aborted when dropped: a call whose caller gave up on it.
struct Aborting<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
