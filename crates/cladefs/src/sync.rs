// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What cladefs's protocols of atomics are made of ([`crate::hearing`]): std's atomics and
//! `OnceLock`; or, with the `loom` feature, loom's (tokio-rs/loom), whose model then runs every
//! interleaving of a model's threads under the orderings each access asks for: a load may read a
//! store that happens-before doesn't hide from it, which Shuttle, seeing every atomic as `SeqCst`,
//! never lets one do (docs/memory-ordering.md, TASK-444.2). Only for the models, as loom's atomics
//! work only in one: `cargo nextest run -p cladefs --features loom --lib loom::`.

#[cfg(not(feature = "loom"))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(feature = "loom")]
pub(crate) use loom::sync::atomic;
#[cfg(feature = "loom")]
pub(crate) use loom_once::OnceLock;

#[cfg(feature = "loom")]
mod loom_once {
    use loom::cell::UnsafeCell;
    use loom::sync::atomic::{AtomicU8, Ordering};

    const EMPTY: u8 = 0;
    const SETTING: u8 = 1;
    const SET: u8 = 2;

    /// std's `OnceLock` as its orderings have it, on loom's atomics: its value written between an
    /// acquiring claim of it and a releasing store that it is set, which `get` loads acquiring, as
    /// std's `Once` does (library/std/src/sys/sync/once). Loom's cell checks that every read of
    /// the value happens after its write. A second `set` fails at once, where std's waits for the
    /// first to finish: a model sets each once.
    pub(crate) struct OnceLock<T> {
        state: AtomicU8,
        value: UnsafeCell<Option<T>>,
    }

    // SAFETY: As std's `OnceLock`: the value is written once, by the one `set` that claimed it,
    // before the releasing store that makes it readable, and read only after an acquiring load of
    // that store.
    unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}

    impl<T> OnceLock<T> {
        pub(crate) fn new() -> Self {
            Self { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(None) }
        }

        pub(crate) fn get(&self) -> Option<&T> {
            if self.state.load(Ordering::Acquire) != SET {
                return None;
            }
            // SAFETY: Set, the value is written for good, and the load above acquired its write.
            self.value.with(|value| unsafe { (*value).as_ref() })
        }

        pub(crate) fn set(&self, value: T) -> Result<(), T> {
            let claimed =
                self.state.compare_exchange(EMPTY, SETTING, Ordering::Acquire, Ordering::Acquire);
            if claimed.is_err() {
                return Err(value);
            }
            // SAFETY: Claimed, nothing else writes the value, nor reads it until it is set.
            self.value.with_mut(|slot| unsafe { *slot = Some(value) });
            self.state.store(SET, Ordering::Release);
            Ok(())
        }
    }

    impl<T> Default for OnceLock<T> {
        fn default() -> Self {
            Self::new()
        }
    }
}
