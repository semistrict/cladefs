// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the device's guest and the extent daemon tell each other beside requests: the cell's
//! capacity and the shrinks the guest is asked to take, the daemon's asks for the guest's stats, and whether the guest's filesystem is frozen. The device
//! tells the guest of a change with a configuration change interrupt, which the VMM raises
//! ([`ConfigInterrupt`]); the configuration space reads the values as they are
//! ([`Device::read_config`](crate::Device::read_config)).

use std::sync::Arc;

use crate::sync::Mutex;
use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::error;

use crate::BoxError;

/// The VMM's half of the notices: raising the device's configuration change interrupt, which tells
/// the guest to read the configuration again. Its transport's (virtio-PCI's configuration MSI-X
/// vector, or its ISR's configuration bit).
pub trait ConfigInterrupt: Send + Sync {
    /// Raises the configuration change interrupt.
    fn config_changed(&self) -> Result<(), BoxError>;
}

/// Raises `interrupt`, if there is one, logging a failure: the guest reads the configuration
/// when it next looks, as it would have.
fn raise(interrupt: &Mutex<Option<Arc<dyn ConfigInterrupt>>>) -> bool {
    let Some(interrupt) = &*interrupt.lock().expect("Poisoned lock") else {
        return false;
    };
    match interrupt.config_changed() {
        Ok(()) => true,
        Err(err) => {
            error!("extent device: signalling a configuration change: {err:?}");
            false
        }
    }
}

/// cladefs: what the device knows of its cell's capacity, which the daemon changes
/// (`CellClient::on_capacity`): the capacity, the shrink the guest is asked to take, if any
/// (0 if none), and how the guest is told, once the driver activates the device. The daemon's
/// changes and the guest's answers are made one at a time (`changing`), so that an answer to a
/// shrink no longer asked changes nothing (TASK-79.41); the driver reads the fields as they are.
#[derive(Default)]
pub struct Resizing {
    capacity: AtomicU64,
    asked: AtomicU64,
    interrupt: Mutex<Option<Arc<dyn ConfigInterrupt>>>,
    changing: Mutex<()>,
}

impl std::fmt::Debug for Resizing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resizing")
            .field("capacity", &self.capacity)
            .field("asked", &self.asked)
            .finish_non_exhaustive()
    }
}

impl Resizing {
    /// The cell's capacity, as the guest is told it.
    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Acquire)
    }

    /// The capacity the guest is asked to shrink to, 0 if none.
    pub fn asked(&self) -> u64 {
        self.asked.load(Ordering::Acquire)
    }

    /// The capacity, then the shrink asked, as the driver reads them
    /// ([`Device::read_config`](crate::Device::read_config)): in that order, so that a capacity an
    /// answer took is read with the ask it answered gone, as [`Resizing::answered`] clears the
    /// ask before it releases the capacity, which this acquires before it reads the ask.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn known(&self) -> (u64, u64) {
        let capacity = self.capacity();
        (capacity, self.asked())
    }

    /// The cell's capacity is `capacity` bytes, as it opens, before the guest is told anything.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn opened(&self, capacity: u64) {
        self.capacity.store(capacity, Ordering::Release);
    }

    /// Has `interrupt` tell the guest of changes from here on.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_interrupt(&self, interrupt: Option<Arc<dyn ConfigInterrupt>>) {
        *self.interrupt.lock().expect("Poisoned lock") = interrupt;
    }

    /// The daemon set the cell's capacity to `capacity` bytes, or, if `ask`, asks the guest to
    /// shrink to it: the guest is told, with a configuration change.
    pub fn changed(&self, capacity: u64, ask: bool) {
        let _changing = self.changing.lock().expect("Poisoned lock");
        if ask {
            self.asked.store(capacity, Ordering::Release);
        } else {
            self.capacity.store(capacity, Ordering::Release);
            self.asked.store(0, Ordering::Release);
        }
        raise(&self.interrupt);
    }

    /// The guest answered the shrink to `capacity` bytes it was asked to take: it took it, if
    /// `took`. An answer to a shrink no longer asked, the daemon having asked another since, or
    /// told the capacity it kept, changes nothing.
    pub fn answered(&self, capacity: u64, took: bool) {
        let _changing = self.changing.lock().expect("Poisoned lock");
        if self.asked.load(Ordering::Acquire) != capacity {
            return;
        }
        self.asked.store(0, Ordering::Release);
        if took {
            // Releasing the ask cleared with it ([`Resizing::known`]).
            self.capacity.store(capacity, Ordering::Release);
        }
    }
}

/// cladefs: the extent daemon's asks for the guest's stats (`VIRTIO_EXTENT_F_STATS`, TASK-111.4):
/// the latest, which the configuration's `stats_asked` says, and how the guest is told, while the
/// driver that activated the device takes asks.
#[derive(Default)]
pub struct StatsAsks {
    asked: AtomicU64,
    interrupt: Mutex<Option<Arc<dyn ConfigInterrupt>>>,
}

impl std::fmt::Debug for StatsAsks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsAsks").field("asked", &self.asked).finish_non_exhaustive()
    }
}

impl StatsAsks {
    /// The latest ask, 0 before the first.
    pub fn latest(&self) -> u64 {
        self.asked.load(Ordering::Acquire)
    }

    /// Has `interrupt` ask the guest from here on, or, if none, no guest asked: the cell answers
    /// at once, with none of the guest's.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_interrupt(&self, interrupt: Option<Arc<dyn ConfigInterrupt>>) {
        *self.interrupt.lock().expect("Poisoned lock") = interrupt;
    }

    /// The daemon asks `ask`: whether the guest is asked, with a configuration change, as it is
    /// while the driver takes asks. Its answer, `STATS`, answers the asks before it too.
    pub fn ask(&self, ask: u64) -> bool {
        let interrupt = self.interrupt.lock().expect("Poisoned lock");
        let Some(interrupt) = &*interrupt else {
            return false;
        };
        self.asked.store(ask, Ordering::Release);
        match interrupt.config_changed() {
            Ok(()) => true,
            Err(err) => {
                error!("extent device: signalling a configuration change: {err:?}");
                false
            }
        }
    }
}

/// cladefs: what the device's guest and the daemon tell each other beside requests, which the
/// requests are served with: the cell's capacity, whether the guest's filesystem is frozen
/// (`VIRTIO_EXTENT_T_FREEZE`), as a snapshot needs it, and the daemon's asks for the guest's stats
/// (`VIRTIO_EXTENT_F_STATS`).
#[derive(Debug, Default)]
pub struct Notices {
    pub resizing: Resizing,
    frozen: AtomicBool,
    pub stats: StatsAsks,
}

impl Notices {
    /// Whether the guest's filesystem is frozen, as it said last (`VIRTIO_EXTENT_T_FREEZE`).
    pub fn frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// The guest's filesystem is frozen, if `frozen`, or thawed.
    pub(crate) fn set_frozen(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::Release);
    }
}

/// What the guest tells its device beside its requests, as the guest's dialect serves them
/// (`cladefs::extent_requests::Guest`).
impl cladefs::extent_requests::GuestNotices for Notices {
    fn set_frozen(&self, frozen: bool) {
        Notices::set_frozen(self, frozen);
    }

    fn resize_answered(&self, capacity: u64, took: bool) {
        self.resizing.answered(capacity, took);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A configuration change interrupt that counts how often it is raised.
    #[derive(Default)]
    pub(crate) struct Counted(pub(crate) AtomicUsize);

    impl ConfigInterrupt for Counted {
        fn config_changed(&self) -> Result<(), BoxError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// cladefs TASK-79.41: the guest's answer to a shrink it was asked to take, once a newer shrink
    /// was asked, is that of a shrink no longer asked: the newer stays asked, and the capacity is
    /// the daemon's; the answer to the shrink asked last takes it.
    #[test]
    fn answers_to_shrinks_no_longer_asked_change_nothing() {
        let resizing = Resizing::default();
        let known = |resizing: &Resizing| (resizing.capacity(), resizing.asked());
        resizing.changed(8, false);
        resizing.changed(6, true);
        resizing.changed(4, true);
        resizing.answered(6, true);
        assert_eq!(known(&resizing), (8, 4));
        resizing.answered(4, true);
        assert_eq!(known(&resizing), (4, 0));
        // The daemon told the capacity it kept, as for a shrink no answer came for in time.
        resizing.changed(2, true);
        resizing.changed(4, false);
        resizing.answered(2, true);
        assert_eq!(known(&resizing), (4, 0));
    }

    /// The guest is told of a capacity's changes once the driver activates the device.
    #[test]
    fn capacities_changed_raise_the_interrupt_once_activated() {
        let resizing = Resizing::default();
        resizing.changed(8, false);
        let interrupt = Arc::new(Counted::default());
        resizing.set_interrupt(Some(interrupt.clone()));
        resizing.changed(6, true);
        resizing.answered(6, false);
        assert_eq!(interrupt.0.load(Ordering::Relaxed), 1);
        assert_eq!((resizing.capacity(), resizing.asked()), (8, 0));
    }

    /// cladefs TASK-111.4: the daemon's asks for the guest's stats reach the guest, the
    /// configuration's `stats_asked` and a configuration change, while the driver takes asks; while
    /// it doesn't, none is asked, for the cell to answer at once.
    #[test]
    fn stats_are_asked_of_guests_that_take_asks() {
        let asks = StatsAsks::default();
        assert!(!asks.ask(1));
        assert_eq!(asks.latest(), 0);
        let interrupt = Arc::new(Counted::default());
        asks.set_interrupt(Some(interrupt.clone()));
        assert!(asks.ask(2));
        assert_eq!(asks.latest(), 2);
        assert_eq!(interrupt.0.load(Ordering::Relaxed), 1);
        asks.set_interrupt(None);
        assert!(!asks.ask(3));
        assert_eq!(asks.latest(), 2);
    }
}

/// The notices under loom, its every interleaving of their threads and weak orderings.
#[cfg(all(test, feature = "loom"))]
mod loom {
    use super::*;

    /// What the driver reads of a shrink the guest answered, as the guest answers it: the capacity
    /// it took with the ask gone, never with the ask still there, which would have it shrink
    /// again to where it is (TASK-79.41's answers).
    #[test]
    fn capacities_taken_are_read_with_their_asks_answered() {
        ::loom::model(|| {
            let resizing = ::loom::sync::Arc::new(Resizing::default());
            resizing.opened(8);
            resizing.changed(6, true);
            let answering = ::loom::thread::spawn({
                let resizing = resizing.clone();
                move || resizing.answered(6, true)
            });
            let (capacity, asked) = resizing.known();
            answering.join().unwrap();
            assert_ne!((capacity, asked), (6, 6), "the capacity taken read with its ask");
        });
    }
}
