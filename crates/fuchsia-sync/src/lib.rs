// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/lib/fuchsia-sync/src/lib.rs`, for the host: parking_lot's locks, without Fuchsia's
// futex-based ones and completions. In the guest kernel (cladefs's TASK-26.6), its std's locks
// (`kernel`).

//! Fuchsia-native synchronization primitives.

#[cfg(target_os = "none")]
mod kernel;
#[cfg(target_os = "none")]
pub use kernel::{RawMutex, RawRwLock};
#[cfg(not(target_os = "none"))]
pub use parking_lot::{RawMutex, RawRwLock};

type RawMutexImpl = RawMutex;
type RawRwLockImpl = RawRwLock;

pub type Mutex<T> = lock_api::Mutex<RawMutexImpl, T>;
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutexImpl, T>;
pub type MappedMutexGuard<'a, T> = lock_api::MappedMutexGuard<'a, RawMutexImpl, T>;

pub type RwLock<T> = lock_api::RwLock<RawRwLockImpl, T>;
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwLockImpl, T>;
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwLockImpl, T>;
