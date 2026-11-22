// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/lib.rs`, without `virtual_device`, which reads an image within a
// file as a device: cladefs's extent store has no images within files (backlog doc-4).

//! Fxfs is a log-structured filesystem for [Fuchsia](https://fuchsia.dev/).
//!
//! For a high-level overview, please refer to the
//! [RFC](/docs/contribute/governance/rfcs/0136_fxfs.md).
//!
//! Where possible, Fxfs code tries to be target agnostic.
//! Fuchsia specific bindings are primarily found under [server].

// TODO(https://fxbug.dev/439053417): Investigate why the recursion limit was bumped
// to unblock the toolchain.
#![recursion_limit = "256"]

pub mod checksum;
pub mod drop_event;

#[macro_use]
mod debug_assert_not_too_long;

pub mod blob_metadata;
pub mod errors;
pub mod filesystem;
pub mod follower;
pub mod fsck;
pub mod future_with_guard;
pub mod hooks;
pub mod log;
pub mod lsm_tree;
pub mod metrics;
pub mod object_handle;
pub mod object_store;
pub mod range;
pub mod round;
pub mod serialized_types;
mod stable_hash;
#[cfg(any(test, feature = "benchmark", fuzz))]
pub mod testing;
pub mod zerocopy_serialization;

/// cladefs: constructs and boxes a future inside its own non-inlined stack frame so the caller's
/// async state machine and poll stack frame never reserve space for the callee's future
/// (TASK-26.9, 16 KiB guest kernel stacks).
#[inline(never)]
pub(crate) fn boxed<'a, F, Fut>(make: F) -> futures::future::BoxFuture<'a, Fut::Output>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future + Send + 'a,
{
    Box::pin(make())
}

/// cladefs: like [`boxed`], but keeps the concrete future type so callers in a recursive
/// `async fn` chain (`allocate_in` -> `sync`) do not trip `Send` auto-trait cycles (TASK-26.9).
#[inline(never)]
pub(crate) fn boxed_pin<F, Fut>(make: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(make())
}

/// cladefs: runs `make()` on a fresh task stack (`fuchsia_async::Task::spawn` — a fresh 16 KiB
/// kernel worker stack in the guest and executor depth 0 on the host), constructing the inner
/// future inside the spawned task so deep mount, replay, compaction, and super-block chains stay
/// well within 16 KiB guest kernel stacks (TASK-26.9).
#[inline(never)]
pub(crate) fn offload<F, Fut>(make: F) -> fuchsia_async::Task<Fut::Output>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fuchsia_async::Task::spawn(async move { crate::boxed(make).await })
}
