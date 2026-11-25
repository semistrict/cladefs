// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Thread-safe filesystem test hooks.
//!
//! # Design Architecture & Invariants
//!
//! - **`Hooks<'a>` & `HooksHandle`**: `Hooks<'a>` is the exclusive mutator handle bound to lifetime
//!   `'a` (the test context). `HooksHandle` is shared via `Arc` by `FxFilesystem` options to invoke
//!   registered test callbacks (`on_*` methods) during transaction commits.
//!
//! - **Lifetime Safety**: Incoming hook closures bound to lifetime `'a` are transmuted to `'static`
//!   and stored inside `HooksInner`. `Hooks<'a>` holds the primary `Arc<HooksInner>`, while
//!   `HooksHandle` holds a `Weak<HooksInner>`.
//!
//! - **Cleanup & Synchronization**: When `Hooks<'a>` drops, it drops its `Arc<HooksInner>` and
//!   waits on a `Mutex`/`Condvar` latch. Active readers (`on_*` methods) temporarily upgrade
//!   the `Weak` reference to an `Arc`, ensuring `HooksInner` remains valid during callback
//!   execution. When all readers finish and `HooksInner` drops, `SignalOnDrop` (the last field of
//!   `HooksInner`) drops after `HooksTable`, signaling `detach()`.

use crate::object_store::transaction::Transaction;
use anyhow::Error;
use fuchsia_sync::Mutex;
use std::sync::{Arc, Condvar, Mutex as StdMutex, Weak};

type PreCommitHookFn<'a> = dyn Fn(&Transaction<'_>) -> Result<(), Error> + Send + Sync + 'a;
type SyncHookFn<'a> = dyn Fn() + Send + Sync + 'a;

#[derive(Default)]
struct HooksTable {
    pre_commit: Option<Arc<PreCommitHookFn<'static>>>,
    before_commit: Option<Arc<SyncHookFn<'static>>>,
    waiting_for_journal_space: Option<Arc<SyncHookFn<'static>>>,
}

// cladefs: `Mutex<bool>` + `Condvar` instead of `std::sync::mpsc` so `guest/rust/std` needs no
// `mpsc` implementation (board 11687).
struct SignalOnDrop(Arc<(StdMutex<bool>, Condvar)>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        let (done, cvar) = &*self.0;
        *done.lock().unwrap() = true;
        cvar.notify_all();
    }
}

struct HooksInner {
    table: Mutex<HooksTable>,
    // `_signal_on_drop` MUST be the last field in `HooksInner` so that Rust's struct field drop
    // order drops `table` (and all registered closures) BEFORE `_signal_on_drop` signals the
    // completion latch.
    _signal_on_drop: SignalOnDrop,
}

/// Used to register test hooks for an `FxFilesystem`.
pub struct Hooks<'a> {
    inner: Option<Arc<HooksInner>>,
    done: Arc<(StdMutex<bool>, Condvar)>,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> Hooks<'a> {
    /// Creates a new `Hooks` instance and an associated `HooksHandle`.
    pub fn new() -> (Self, Arc<HooksHandle>) {
        let done = Arc::new((StdMutex::new(false), Condvar::new()));
        let inner = Arc::new(HooksInner {
            table: Mutex::new(HooksTable::default()),
            _signal_on_drop: SignalOnDrop(Arc::clone(&done)),
        });
        let handle = Arc::new(HooksHandle { inner: Arc::downgrade(&inner) });
        (Self { inner: Some(inner), done, _phantom: std::marker::PhantomData }, handle)
    }

    /// Sets the hook executed before committing a transaction.  This hook is called before
    /// any locks required for the commit are taken, so it's before any transaction locks
    /// are upgraded to write locks, before any guards that are taken by calling
    /// `prepare_commit` for objects involved in the transaction, and before the commit mutex
    /// is acquired.
    pub fn set_pre_commit(
        &mut self,
        hook: impl Fn(&Transaction<'_>) -> Result<(), Error> + Send + Sync + 'a,
    ) {
        let boxed: Box<PreCommitHookFn<'a>> = Box::new(hook);
        // SAFETY: `'a` is transmuted to `'static`. This is safe because `Hooks<'a>` owns the
        // `Arc<HooksInner>` and its `detach` method (or `drop`) blocks until all `Arc<HooksInner>`
        // references drop.
        let static_hook: Box<PreCommitHookFn<'static>> = unsafe { std::mem::transmute(boxed) };
        if let Some(inner) = &self.inner {
            inner.table.lock().pre_commit = Some(Arc::from(static_hook));
        }
    }

    /// Sets the hook executed before a transaction starts committing.  This hook is called
    /// after calling `prepare_commit` on all objects involved in the transaction (which
    /// might acquire some locks/guards), but before acquiring the commit mutex.
    pub fn set_before_commit(&mut self, hook: impl Fn() + Send + Sync + 'a) {
        let boxed: Box<SyncHookFn<'a>> = Box::new(hook);
        // SAFETY: `'a` is transmuted to `'static`. This is safe because `Hooks<'a>` owns the
        // `Arc<HooksInner>` and its `detach` method (or `drop`) blocks until all `Arc<HooksInner>`
        // references drop.
        let static_hook: Box<SyncHookFn<'static>> = unsafe { std::mem::transmute(boxed) };
        if let Some(inner) = &self.inner {
            inner.table.lock().before_commit = Some(Arc::from(static_hook));
        }
    }

    /// Sets the hook executed when a transaction or flush operation blocks waiting for journal
    /// space.
    pub fn set_waiting_for_journal_space(&mut self, hook: impl Fn() + Send + Sync + 'a) {
        let boxed: Box<SyncHookFn<'a>> = Box::new(hook);
        // SAFETY: `'a` is transmuted to `'static`. This is safe because `Hooks<'a>` owns the
        // `Arc<HooksInner>` and its `detach` method (or `drop`) blocks until all `Arc<HooksInner>`
        // references drop.
        let static_hook: Box<SyncHookFn<'static>> = unsafe { std::mem::transmute(boxed) };
        if let Some(inner) = &self.inner {
            inner.table.lock().waiting_for_journal_space = Some(Arc::from(static_hook));
        }
    }

    fn detach(&mut self) {
        if let Some(inner) = self.inner.take() {
            drop(inner);
            let (done, cvar) = &*self.done;
            let mut guard = done.lock().unwrap();
            while !*guard {
                guard = cvar.wait(guard).unwrap();
            }
        }
    }
}

impl Drop for Hooks<'_> {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Handle stored on `FxFilesystem` options to trigger registered test hooks.
#[derive(Default)]
pub struct HooksHandle {
    inner: Weak<HooksInner>,
}

impl HooksHandle {
    /// Invokes the registered `pre_commit` hook, if any.
    pub fn on_pre_commit(&self, transaction: &Transaction<'_>) -> Result<(), Error> {
        if let Some(inner) = self.inner.upgrade() {
            // The strong is held until after the hook is called.
            let hook = inner.table.lock().pre_commit.clone();
            if let Some(hook) = hook {
                return hook(transaction);
            }
        }
        Ok(())
    }

    /// Invokes the registered `before_commit` hook, if any.
    pub fn on_before_commit(&self) {
        if let Some(inner) = self.inner.upgrade() {
            // The strong is held until after the hook is called.
            let hook = inner.table.lock().before_commit.clone();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    /// Invokes the registered `waiting_for_journal_space` hook, if any.
    pub fn on_waiting_for_journal_space(&self) {
        if let Some(inner) = self.inner.upgrade() {
            // The strong is held until after the hook is called.
            let hook = inner.table.lock().waiting_for_journal_space.clone();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_hooks_basic() {
        let called = AtomicBool::new(false);
        let (mut hooks, handle) = Hooks::new();

        let called_ref = &called;
        hooks.set_before_commit(move || {
            called_ref.store(true, Ordering::Relaxed);
        });

        handle.on_before_commit();
        assert!(called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_hooks_detached_on_drop() {
        let called = AtomicBool::new(false);
        let handle = {
            let (mut hooks, handle) = Hooks::new();
            let called_ref = &called;
            hooks.set_before_commit(move || {
                called_ref.store(true, Ordering::Relaxed);
            });
            handle
        };

        // hooks dropped here
        handle.on_before_commit();
        assert!(!called.load(Ordering::Relaxed));
    }

    #[test]
    fn test_hooks_waiting_for_journal_space() {
        let called = AtomicBool::new(false);
        let (mut hooks, handle) = Hooks::new();

        let called_ref = &called;
        hooks.set_waiting_for_journal_space(move || {
            called_ref.store(true, Ordering::Relaxed);
        });

        handle.on_waiting_for_journal_space();
        assert!(called.load(Ordering::Relaxed));
    }
}
