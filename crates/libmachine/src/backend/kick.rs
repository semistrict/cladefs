// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Kicking a vCPU thread out of `KVM_RUN` (Linux).
//!
//! One real-time signal serves every machine in the process. Its handler
//! sets KVM's `immediate_exit` in the `kvm_run` of the vCPU that the
//! signalled thread runs, found through a thread-local pointer. Each vCPU
//! has a thread of its own for the machine's life, with the signal
//! unblocked, so a kick either interrupts `KVM_RUN` (which then fails with
//! `EINTR`), or, if it lands while the VMM handles an exit or while the
//! thread waits between runs, makes the next `KVM_RUN` return at once. The
//! run loop clears the flag before it checks why it might stop, so a kick
//! is never lost and a stale one costs one extra exit at most.

use std::cell::Cell;
use std::io;
use std::sync::OnceLock;

use crate::error::{Error, Result};

thread_local! {
    /// This vCPU thread's `kvm_run.immediate_exit`, or null on other threads.
    /// Const-initialized and without a destructor, so the signal handler can
    /// read it.
    static IMMEDIATE_EXIT: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
}

/// The kick signal: the first real-time signal available to applications.
pub(crate) fn signal() -> libc::c_int {
    vmm_sys_util::signal::SIGRTMIN()
}

extern "C" fn handle_kick(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Async-signal-safe: one TLS read and one volatile store.
    let ptr = IMMEDIATE_EXIT.with(Cell::get);
    if !ptr.is_null() {
        // SAFETY: the pointer is set only on the thread of the vCPU it belongs
        // to, and points into its kvm_run mapping, which outlives the setting.
        unsafe { ptr.write_volatile(1) };
    }
}

fn install_handler() -> Result<()> {
    static INSTALLED: OnceLock<std::result::Result<(), i32>> = OnceLock::new();
    let installed = INSTALLED.get_or_init(|| {
        vmm_sys_util::signal::register_signal_handler(signal(), handle_kick).map_err(|e| e.errno())
    });
    installed.map_err(|errno| Error::Hypervisor {
        op: "install the vCPU kick signal handler",
        source: io::Error::from_raw_os_error(errno),
    })
}

/// A vCPU thread, which another thread can kick.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Thread(libc::pthread_t);

impl PartialEq for Thread {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: pthread_equal only compares the two IDs.
        unsafe { libc::pthread_equal(self.0, other.0) != 0 }
    }
}

impl Eq for Thread {}

// SAFETY: a pthread_t names a thread; it points at nothing this thread owns. musl defines it as a
// pointer (glibc as an integer), which Rust would keep to the thread that read it, but any thread
// may pass it to pthread_kill or pthread_equal, the two uses it has here.
unsafe impl Send for Thread {}
// SAFETY: as above: shared, it is only read, and passed by value.
unsafe impl Sync for Thread {}

impl Thread {
    pub(crate) fn current() -> Self {
        // SAFETY: pthread_self has no preconditions.
        Thread(unsafe { libc::pthread_self() })
    }

    pub(crate) fn kick(self) {
        // SAFETY: callers only kick a thread registered as a vCPU thread,
        // which unregisters (under the same lock) before it exits.
        unsafe { libc::pthread_kill(self.0, signal()) };
    }
}

/// The current thread as a vCPU thread: the kick signal unblocked and the
/// TLS pointer set, until dropped.
pub(crate) struct VcpuThreadScope {
    old_mask: libc::sigset_t,
}

impl VcpuThreadScope {
    /// # Safety
    ///
    /// `immediate_exit` must point to the `immediate_exit` byte of the
    /// `kvm_run` of the vCPU this thread runs, and stay valid until the
    /// scope is dropped.
    pub(crate) unsafe fn enter(immediate_exit: *mut u8) -> Result<Self> {
        install_handler()?;

        // SAFETY: plain sigset manipulation on owned values.
        let old_mask = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal());
            let rc = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, &mut old);
            if rc != 0 {
                return Err(Error::Hypervisor {
                    op: "unblock the vCPU kick signal",
                    source: io::Error::from_raw_os_error(rc),
                });
            }
            old
        };
        IMMEDIATE_EXIT.with(|p| p.set(immediate_exit));
        Ok(VcpuThreadScope { old_mask })
    }
}

impl Drop for VcpuThreadScope {
    fn drop(&mut self) {
        IMMEDIATE_EXIT.with(|p| p.set(std::ptr::null_mut()));
        // SAFETY: restores the mask saved in enter().
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut()) };
    }
}
