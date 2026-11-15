// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what one thread hears of another's news through atomics, a listener set once on one
//! side, the news on the other: that something happened, once ([`Occurrence`]), such as a VM
//! process's daemon going; and the latest of a value that rises ([`Latest`]), such as a follower's
//! writer's commits. Their models run under loom (the `loom` feature, [`crate::sync`]).
//!
//! The news and the listener meet in one read-modify-write each side makes of one atomic, never in
//! a store on one side and a load on the other: a load may miss a store that nothing ordered
//! before it, so that each side, storing its own and then loading the other's, may miss the
//! other's, store buffering, which no ordering short of `SeqCst` fences forbids. The
//! read-modify-writes of one atomic are in one order, which the later sees the earlier in
//! (TASK-444.2.1, TASK-444.2.2).

use crate::sync::OnceLock;
use crate::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// What a listener is told once something happened.
pub(crate) type Listener = Box<dyn Fn() + Send + Sync>;

/// That something happened, at most once, and what is told of it: once, whether it listened before
/// or after.
pub(crate) struct Occurrence {
    /// [`HAPPENED`] and [`LISTENING`], each set once.
    state: AtomicU8,
    listener: OnceLock<Listener>,
}

/// That it happened.
const HAPPENED: u8 = 1;
/// That it has a listener.
const LISTENING: u8 = 2;

impl Occurrence {
    pub(crate) fn new() -> Self {
        Self { state: AtomicU8::new(0), listener: OnceLock::new() }
    }

    /// Whether it happened. Relaxed, as it publishes nothing: what asks after something that
    /// followed it (a VM process after a request failed, which its receiving thread fails once the
    /// daemon went) sees it.
    pub(crate) fn happened(&self) -> bool {
        self.state.load(Ordering::Relaxed) & HAPPENED != 0
    }

    /// It happened: the listener is told, if it listened before; one that listens later is told
    /// as it does. Again, nothing more.
    pub(crate) fn happen(&self) {
        // Acquiring: a listener seen listening, its `listen` published it.
        let before = self.state.fetch_or(HAPPENED, Ordering::Acquire);
        if before & (HAPPENED | LISTENING) == LISTENING {
            (self.listener.get().expect("the listener of an occurrence listened to"))();
        }
    }

    /// Has `listener` told once it happens: at once, if it did. Once, of an occurrence.
    pub(crate) fn listen(&self, listener: Listener) {
        if self.listener.set(listener).is_err() {
            panic!("an occurrence listened to twice");
        }
        // Releasing: publishes the listener to a `happen` that sees it listening.
        let before = self.state.fetch_or(LISTENING, Ordering::Release);
        if before & HAPPENED != 0 {
            (self.listener.get().unwrap())();
        }
    }
}

/// What a listener is told of each rise of a value.
pub(crate) type RiseListener = Box<dyn Fn(u64) + Send + Sync>;

/// The latest of a value that only rises, and what is told of its rises.
pub(crate) struct Latest {
    latest: AtomicU64,
    listener: OnceLock<RiseListener>,
}

impl Latest {
    pub(crate) fn new() -> Self {
        Self { latest: AtomicU64::new(0), listener: OnceLock::new() }
    }

    /// The latest value. Relaxed, as it publishes nothing beside itself.
    pub(crate) fn latest(&self) -> u64 {
        self.latest.load(Ordering::Relaxed)
    }

    /// The value is `value`, if that is a rise: the listener is told, if one listened before.
    pub(crate) fn rise(&self, value: u64) {
        // Acquiring: a listener's `listen` before this in the value's order published it.
        if self.latest.fetch_max(value, Ordering::Acquire) < value
            && let Some(listener) = self.listener.get()
        {
            listener(value);
        }
    }

    /// Has `listener` told of each rise from now on, and returns the latest value as it began to
    /// listen: each rise is heard from one or the other, at least, a rise as it begins perhaps from
    /// both. Once, of a value.
    pub(crate) fn listen(&self, listener: RiseListener) -> u64 {
        if self.listener.set(listener).is_err() {
            panic!("a value's rises listened to twice");
        }
        // A read-modify-write, which a rise's comes before or after: before, this returns what it
        // rose to; after, the rise's acquires this, releasing, and with it the listener.
        self.latest.fetch_max(0, Ordering::Release)
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A listener is told of an occurrence once, whether it listened before it or after, and
    /// told nothing of it happening again.
    #[test]
    fn occurrences_are_told_once_to_listeners_before_and_after() {
        for listening_first in [true, false] {
            let occurrence = Occurrence::new();
            let told = Arc::new(Mutex::new(0));
            let listen = || {
                let told = told.clone();
                occurrence.listen(Box::new(move || *told.lock().unwrap() += 1));
            };
            if listening_first {
                listen();
                assert!(!occurrence.happened());
                assert_eq!(*told.lock().unwrap(), 0);
                occurrence.happen();
            } else {
                occurrence.happen();
                listen();
            }
            occurrence.happen();
            assert!(occurrence.happened());
            assert_eq!(*told.lock().unwrap(), 1, "listening first: {listening_first}");
        }
    }

    /// Listening returns the value's latest, and the listener is told of rises after it alone.
    #[test]
    fn listeners_are_told_of_rises_after_they_listened() {
        let latest = Latest::new();
        latest.rise(2);
        let told = Arc::new(Mutex::new(Vec::new()));
        let heard = latest.listen({
            let told = told.clone();
            Box::new(move |value| told.lock().unwrap().push(value))
        });
        assert_eq!((heard, latest.latest()), (2, 2));
        for value in [1, 3, 3, 5, 4] {
            latest.rise(value);
        }
        assert_eq!(*told.lock().unwrap(), vec![3, 5]);
        assert_eq!(latest.latest(), 5);
    }
}

/// The protocols under loom, its every interleaving of their threads and weak orderings: each
/// listener hears of what it listens for, once it listened, however the two sides' atomics meet.
#[cfg(all(test, feature = "loom"))]
mod loom {
    use super::*;
    use crate::sync::atomic::AtomicUsize;
    use ::loom::sync::Arc;

    /// An occurrence's listener is told of it once, whether it listened before it happened or
    /// after, as a VM process's on its daemon going is: stopping its VM (`DaemonConnection::
    /// on_lost`, TASK-444.2.1).
    #[test]
    fn occurrences_are_told_once() {
        ::loom::model(|| {
            let occurrence = Arc::new(Occurrence::new());
            let told = Arc::new(AtomicUsize::new(0));
            let happening = ::loom::thread::spawn({
                let occurrence = occurrence.clone();
                move || occurrence.happen()
            });
            let telling = told.clone();
            occurrence.listen(Box::new(move || {
                telling.fetch_add(1, Ordering::Relaxed);
            }));
            happening.join().unwrap();
            assert!(occurrence.happened());
            assert_eq!(told.load(Ordering::Relaxed), 1, "told so many times");
        });
    }

    /// What listens to a value's rises hears of each, or of a later one, from the rise or from
    /// what listening returns, as a follower's guest hears of its writer's commits
    /// (virtio_extent's `hear_commits`, TASK-444.2.2).
    #[test]
    fn listeners_hear_of_every_rise() {
        ::loom::model(|| {
            let latest = Arc::new(Latest::new());
            let rising = ::loom::thread::spawn({
                let latest = latest.clone();
                move || latest.rise(5)
            });
            let heard = Arc::new(AtomicU64::new(0));
            let hearing = heard.clone();
            let latest_heard = latest.listen(Box::new(move |value| {
                hearing.fetch_max(value, Ordering::Relaxed);
            }));
            heard.fetch_max(latest_heard, Ordering::Relaxed);
            rising.join().unwrap();
            assert_eq!(heard.load(Ordering::Relaxed), 5, "the rise to 5 unheard");
        });
    }
}
