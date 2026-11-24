// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One runtime for OpenBao, AWS KMS, Google Cloud KMS and Azure Key Vault. Callers can poll their
//! futures on any executor, including `futures::executor::block_on`. The runtime keeps driving
//! their clients' HTTP connections after a caller's runtime shuts down.

use super::Error;
use std::future::Future;
use std::io;
use std::sync::LazyLock;

// Process-wide real Tokio, outside the simulation. Clients and credentials belong to the
// individual KMSes. The runtime outlives callers, including the tasks driving their connections.
static RUNTIME: LazyLock<io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("cladefs-kms")
        .enable_all()
        .build()
});

pub(super) struct Background(&'static tokio::runtime::Runtime);

impl Background {
    pub fn shared() -> io::Result<Self> {
        RUNTIME.as_ref().map(Self).map_err(|err| io::Error::new(err.kind(), err.to_string()))
    }

    /// Runs `call` for `tenant`. Dropping the returned future aborts the call. A panic propagates
    /// to the caller. The runtime reports cancellation as a KMS error.
    pub fn run<T: Send + 'static>(
        &self,
        tenant: u64,
        call: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> impl Future<Output = Result<T, Error>> + Send + 'static {
        let runtime = self.0;
        async move {
            let mut task = Aborting(runtime.spawn(call));
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

/// A task, aborted when dropped: a call whose caller gave up on it.
struct Aborting<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{channel::oneshot, executor::block_on, task::noop_waker_ref};
    use std::sync::mpsc;
    use std::task::Context;
    use std::time::Duration;

    #[test]
    fn dropping_a_call_aborts_it_on_the_shared_runtime() {
        let first = Background::shared().unwrap();
        let second = Background::shared().unwrap();
        assert!(std::ptr::eq(first.0, second.0));

        let (started, running) = oneshot::channel();
        let (alive, stopped) = mpsc::channel::<()>();
        let mut call = Box::pin(first.run(7, async move {
            let _alive = alive;
            started.send(()).unwrap();
            std::future::pending::<Result<(), Error>>().await
        }));
        assert!(call.as_mut().poll(&mut Context::from_waker(noop_waker_ref())).is_pending());
        block_on(running).unwrap();
        drop(call);
        assert_eq!(
            stopped.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(block_on(second.run(7, async { Ok(42) })).unwrap(), 42);
    }
}
