// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Kicking an HVF vCPU, and the waits a vCPU thread makes outside the guest.
//!
//! A kick is `hv_vcpus_exit`, which any thread may call: a running vCPU exits at once, and one
//! not running exits as soon as it next runs. A vCPU thread also waits outside the guest, for
//! the guest to power its vCPU on (PSCI CPU_ON) or for an interrupt (WFI), and a kick wakes
//! those waits too. Its [`CpuSlot`] holds what each needs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use super::ffi::{hv_vcpu_t, hv_vcpus_exit};
use crate::error::Result;

/// No vCPU yet: the slot's thread has not run it.
const NONE: u64 = u64::MAX;

/// What a vCPU's thread, its kickers, the irqfd thread and other vCPUs (PSCI) share of it.
#[derive(Debug)]
pub(crate) struct CpuSlot {
    /// The framework's vCPU, once its thread has created it.
    id: AtomicU64,
    /// Kicked since the run loop last cleared it.
    kicked: AtomicBool,
    state: Mutex<State>,
    /// Signalled on a kick, a CPU_ON for this vCPU, and an interrupt sent to the GIC.
    wake: Condvar,
}

#[derive(Debug)]
struct State {
    power: Powered,
    /// An interrupt was sent since the vCPU last waited for one.
    interrupted: bool,
}

/// What a vCPU thread found when it asked whether its vCPU is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Power {
    /// On: run it.
    On,
    /// Powered on just now: start it at `entry` with `context` in x0.
    Start { entry: u64, context: u64 },
    /// Off, and a kick came: the run loop decides whether to wait again.
    Kicked,
}

/// A vCPU's power state, as PSCI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Powered {
    Off,
    /// CPU_ON came; its thread has not started it yet.
    Starting {
        entry: u64,
        context: u64,
    },
    On,
}

impl CpuSlot {
    /// A vCPU's slot: on if it is the boot vCPU, else off until the guest powers it on.
    pub(crate) fn new(boot: bool) -> Self {
        Self {
            id: AtomicU64::new(NONE),
            kicked: AtomicBool::new(false),
            state: Mutex::new(State {
                power: if boot { Powered::On } else { Powered::Off },
                interrupted: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The vCPU thread created its vCPU: kicks reach it from now.
    pub(crate) fn created(&self, id: hv_vcpu_t) {
        self.id.store(id, Ordering::SeqCst);
        // A kick that came before the vCPU existed is in `kicked`: run_once sees it first.
    }

    /// Kicks the vCPU: out of the guest, or out of a wait, or before it next enters.
    pub(crate) fn kick(&self) {
        self.kicked.store(true, Ordering::SeqCst);
        let id = self.id.load(Ordering::SeqCst);
        if id != NONE {
            // SAFETY: reads one ID from a live local; any thread may ask a vCPU to exit.
            unsafe { hv_vcpus_exit(&id, 1) };
        }
        let _state = self.lock();
        self.wake.notify_all();
    }

    /// Clears the kick: the run loop is about to check why it might stop.
    pub(crate) fn clear_kick(&self) {
        self.kicked.store(false, Ordering::SeqCst);
    }

    /// Takes a kick that came since the run loop cleared it.
    pub(crate) fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::SeqCst)
    }

    /// An interrupt was sent to the GIC: a vCPU waiting for one looks.
    pub(crate) fn wake(&self) {
        let mut state = self.lock();
        state.interrupted = true;
        self.wake.notify_all();
    }

    /// Waits until the vCPU is on, or a kick comes.
    pub(crate) fn wait_until_on(&self) -> Power {
        let mut state = self.lock();
        loop {
            match state.power {
                Powered::On => return Power::On,
                Powered::Starting { entry, context } => {
                    state.power = Powered::On;
                    return Power::Start { entry, context };
                }
                Powered::Off if self.kicked.load(Ordering::SeqCst) => return Power::Kicked,
                Powered::Off => {
                    state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    /// Waits for an interrupt (WFI): until one is sent to the GIC, `deadline` (the vCPU's
    /// timer) passes, or a kick comes. An interrupt sent while the vCPU ran wakes it at once:
    /// at worst a WFI that returns early, which the architecture allows.
    pub(crate) fn wait_for_interrupt(&self, deadline: Option<Instant>) {
        let mut state = self.lock();
        while !state.interrupted && !self.kicked.load(Ordering::SeqCst) {
            match deadline {
                None => state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    state =
                        self.wake.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
        state.interrupted = false;
    }

    /// PSCI CPU_ON: powers the vCPU on at `entry`. False if it is not off.
    pub(crate) fn power_on(&self, entry: u64, context: u64) -> bool {
        let mut state = self.lock();
        if state.power != Powered::Off {
            return false;
        }
        state.power = Powered::Starting { entry, context };
        self.wake.notify_all();
        true
    }

    /// PSCI CPU_OFF: the vCPU stops until powered on again.
    pub(crate) fn power_off(&self) {
        self.lock().power = Powered::Off;
    }

    /// PSCI AFFINITY_INFO: whether the vCPU is on (or being started).
    pub(crate) fn is_on(&self) -> bool {
        self.lock().power != Powered::Off
    }

    /// The power state, for a snapshot.
    pub(crate) fn power(&self) -> super::state::Power {
        use super::state::Power;
        match self.lock().power {
            Powered::Off => Power::Off,
            Powered::Starting { entry, context } => Power::Starting { entry, context },
            Powered::On => Power::On,
        }
    }

    /// Sets the power state a snapshot had, as the vCPU is restored.
    pub(crate) fn set_power(&self, power: super::state::Power) {
        use super::state::Power;
        self.lock().power = match power {
            Power::Off => Powered::Off,
            Power::Starting { entry, context } => Powered::Starting { entry, context },
            Power::On => Powered::On,
        };
    }
}

/// A vCPU a stop or a run's end kicks: its slot.
#[derive(Debug, Clone)]
pub(crate) struct Kicker(Arc<CpuSlot>);

impl PartialEq for Kicker {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Kicker {}

impl Kicker {
    pub(crate) fn kick(&self) {
        self.0.kick();
    }
}

/// What a vCPU thread hands the kick: its vCPU's slot.
pub(crate) type KickTarget = Arc<CpuSlot>;

/// The current thread as a vCPU thread. HVF needs nothing of the thread itself: kicks go to the
/// vCPU's slot.
pub(crate) struct VcpuThreadScope(Kicker);

impl VcpuThreadScope {
    /// # Safety
    ///
    /// None needed here; unsafe as KVM's is, whose target is a raw pointer.
    pub(crate) unsafe fn enter(target: KickTarget) -> Result<Self> {
        Ok(Self(Kicker(target)))
    }

    /// What kicks this thread's vCPU.
    pub(crate) fn kicker(&self) -> Kicker {
        self.0.clone()
    }
}

/// A target for a fake vCPU's thread (tests): a slot no vCPU is created in.
#[cfg(test)]
pub(crate) fn fake_target() -> KickTarget {
    Arc::new(CpuSlot::new(true))
}
