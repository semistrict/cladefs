// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a VMM's vCPUs do for the device's DAX window (README.md, "Its vCPUs"): retry a guest's
//! access that fails while cladefs changes the window's memory, and give up on one that keeps
//! failing, saying what it can of it, which stops the VM.
//!
//! cladefs replaces the host memory of a DAX window as it changes the window's mappings, and a
//! guest's access to a page meanwhile fails: KVM's `KVM_RUN` with `EFAULT`, without an address,
//! but for some faults with `KVM_CAP_MEMORY_FAULT_INFO` (`KVM_EXIT_MEMORY_FAULT`). The vCPU
//! retries its access until the change is done, which takes a few system calls
//! (`cladefs::AccessFailures`); one cladefs refuses for good, a store past the window's
//! `private_max` among them, keeps failing, and the vCPU gives up on it. Every VMM hosting the
//! device does this, Firecracker's and libmachine's alike, as this says.

use std::time::Instant;

use log::warn;

/// Where KVM says an access failed (`KVM_EXIT_MEMORY_FAULT`, with `KVM_CAP_MEMORY_FAULT_INFO`):
/// `size` bytes at guest physical address `gpa`, and KVM's `flags`
/// (`KVM_MEMORY_EXIT_FLAG_PRIVATE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryFault {
    pub flags: u64,
    pub gpa: u64,
    pub size: u64,
}

/// A vCPU's guest accesses failing, which it retries, or gives up on as they keep failing.
#[derive(Debug)]
pub struct FailingAccesses {
    /// The vCPU's index, which the log names.
    index: u32,
    failures: cladefs::AccessFailures,
    /// Whether KVM says where some failed (`KVM_CAP_MEMORY_FAULT_INFO`), which a give-up tells.
    memory_fault_info: bool,
}

impl FailingAccesses {
    /// vCPU `index`'s; `memory_fault_info` says whether its KVM has `KVM_CAP_MEMORY_FAULT_INFO`.
    pub fn new(index: u32, memory_fault_info: bool) -> Self {
        Self { index, failures: cladefs::AccessFailures::default(), memory_fault_info }
    }

    /// The vCPU ran, exiting otherwise than with a failed access.
    pub fn ran(&mut self) {
        if let Some(run) = self.failures.ran(Instant::now) {
            ran_on(self.index, &run);
        }
    }

    /// The vCPU's access failed, where KVM said if it did (`fault`): `Ok` to retry it, the window
    /// changing its memory; `Err` with what to tell of it, once it kept failing, as the VM stops.
    /// A retry first lets a store fault that cladefs held back for a worker's write
    /// (`cladefs::guest_access_failed`, backlog doc-13).
    pub fn failed(&mut self, fault: Option<MemoryFault>) -> Result<(), String> {
        match self.failures.failed(Instant::now()) {
            cladefs::Failure::Retry { ended } => {
                if let Some(run) = ended {
                    ran_on(self.index, &run);
                }
                Ok(())
            }
            cladefs::Failure::GiveUp(run) => Err(format!(
                "vCPU {}: KVM_RUN kept failing with EFAULT, {}",
                self.index,
                gave_up(&run, fault, self.memory_fault_info)
            )),
        }
    }
}

/// How often and how long `run` failed, and how many other exits it took meanwhile.
fn failed(run: &cladefs::FailingRun) -> String {
    let cladefs::FailingRun { first, last, failures, exits } = run;
    format!("{failures} times in {:?}, among {exits} other exits", *last - *first)
}

/// A run of vCPU `index`'s accesses failing that ended, the vCPU running on: logs how many, over
/// how long, the first thousand runs of them and every power of two after, which says whether the
/// stores the DAX window refuses stall the vCPUs whose they are, and for how long (TASK-44.12).
fn ran_on(index: u32, run: &cladefs::FailingRun) {
    // A count of the whole process's log lines, which only bounds how often it logs.
    static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let logged = LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if logged <= 1000 || logged.is_power_of_two() {
        warn!(
            "cladefs: vCPU {index}'s KVM_RUN failed with EFAULT {}, then ran on ({logged} such \
             runs so far)",
            failed(run)
        );
    }
}

/// What a vCPU giving up on an access that kept failing, `run`, tells of it, beside `KVM_RUN`'s
/// `EFAULT`, which says nothing more: how long and how often it failed; where, if KVM said
/// (`fault`, which it may with `KVM_CAP_MEMORY_FAULT_INFO`, as `memory_fault_info` says it has or
/// hasn't), and the DAX window's offset there, if it is in one; and what the windows' second
/// stages hold there and are doing, from cladefs (`cladefs::describe_windows`).
fn gave_up(
    run: &cladefs::FailingRun,
    fault: Option<MemoryFault>,
    memory_fault_info: bool,
) -> String {
    let (at, host) = match fault {
        Some(MemoryFault { flags, gpa, size }) => {
            let window = crate::dax_window_at(gpa);
            let within = match window {
                Some((offset, _)) => format!("DAX window offset {offset:#x}"),
                None => "in no DAX window".to_owned(),
            };
            let at = format!(
                "at guest physical address {gpa:#x} ({size:#x} bytes, flags {flags:#x}), {within}"
            );
            (at, window.map(|(_, host)| host))
        }
        None => {
            let info = if memory_fault_info { "has" } else { "hasn't" };
            (format!("where, KVM didn't say (it {info} KVM_CAP_MEMORY_FAULT_INFO)"), None)
        }
    };
    format!("{}, {at}; {}", failed(run), cladefs::describe_windows(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vCPU giving up on an access that kept failing tells where KVM said it failed, and
    /// whether in a DAX window, or that KVM didn't say, and whether it can; then what cladefs's
    /// second stages hold and are doing.
    #[test]
    fn accesses_given_up_on_tell_where() {
        let fault = MemoryFault { flags: 0, gpa: 0x1000, size: 0x1000 };
        let first = Instant::now();
        let last = first + std::time::Duration::from_millis(1500);
        let run = cladefs::FailingRun { first, last, failures: 3, exits: 2 };
        let told = gave_up(&run, Some(fault), true);
        let at = "3 times in 1.5s, among 2 other exits, at guest physical address 0x1000 (0x1000 \
                  bytes, flags 0x0), in no DAX window; ";
        assert!(told.starts_with(at), "{told}");
        let told = gave_up(&run, None, false);
        let unsaid = "3 times in 1.5s, among 2 other exits, where, KVM didn't say (it hasn't \
                      KVM_CAP_MEMORY_FAULT_INFO); ";
        assert!(told.starts_with(unsaid), "{told}");
    }
}
