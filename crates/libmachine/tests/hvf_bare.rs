// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bare arm64 machines on Hypervisor.framework running guests that never end on their own:
//! stopping a run from another thread, run timeouts, a stopped machine keeping its state, what
//! each exit is counted as, and the UART's bounded capture of what a guest writes. A process has
//! one VM: the tests take turns.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{ENTRY, assemble, hypervisor, one_machine};
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use libmachine::{ExitCounts, ExitReason, Machine, MachineConfig};

/// Where `counting_loop` keeps its counter: past its code.
const COUNTER: u64 = ENTRY + 0x1000;
/// The UART's transmit register.
const UART: u32 = 0x0802_0000;

/// A guest that never stops: it increments a counter in memory forever.
fn counting_loop() -> Vec<u8> {
    assemble(|ops| {
        dynasm!(ops
            ; .arch aarch64
            ; movz x1, (COUNTER & 0xffff) as u32
            ; movk x1, ((COUNTER >> 16) & 0xffff) as u32, lsl 16
            ; top:
            ; ldr x0, [x1]
            ; add x0, x0, 1
            ; str x0, [x1]
            ; b <top
        );
    })
}

fn bare(code: &[u8]) -> Machine {
    let mut m =
        Machine::new(MachineConfig { mem_size: 64 << 20, ..Default::default() }).expect("machine");
    m.load(ENTRY, code).expect("load");
    m.set_entry(ENTRY).expect("entry");
    m
}

fn counter(m: &Machine) -> u64 {
    u64::from_le_bytes(m.read_memory(COUNTER, 8).unwrap().try_into().unwrap())
}

/// Generous bound for "promptly", so that a loaded host does not flake.
const PROMPT: Duration = Duration::from_secs(2);

#[test]
fn stop_from_another_thread() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let mut m = bare(&counting_loop());
    let handle = m.stop_handle();
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let at = Instant::now();
        handle.stop();
        at
    });
    let exit = m.run().unwrap();
    let returned = Instant::now();
    let stopped_at = stopper.join().unwrap();
    assert_eq!(exit, ExitReason::Stopped);
    assert!(returned - stopped_at < PROMPT, "run returned {:?} after stop", returned - stopped_at);
    assert!(counter(&m) > 0, "the guest ran before it was stopped");
}

#[test]
fn stop_before_run_is_not_lost() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let mut m = bare(&counting_loop());
    m.stop_handle().stop();
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    assert_eq!(counter(&m), 0, "the guest never ran");
    // The request was consumed: the next run goes on until its timeout.
    assert_eq!(m.run_for(Duration::from_millis(20)).unwrap(), ExitReason::TimedOut);
    assert!(counter(&m) > 0);
}

#[test]
fn run_for_times_out() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let mut m = bare(&counting_loop());
    let start = Instant::now();
    assert_eq!(m.run_for(Duration::from_millis(100)).unwrap(), ExitReason::TimedOut);
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(100), "returned early: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(100) + PROMPT, "returned late: {elapsed:?}");
    // A zero timeout returns without running the guest.
    let before = counter(&m);
    assert_eq!(m.run_for(Duration::ZERO).unwrap(), ExitReason::TimedOut);
    assert_eq!(counter(&m), before);
}

#[test]
fn a_stopped_machine_keeps_its_state_and_resumes() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let mut m = bare(&counting_loop());
    assert_eq!(m.run_for(Duration::from_millis(30)).unwrap(), ExitReason::TimedOut);
    let first = counter(&m);
    assert_eq!(m.run_for(Duration::from_millis(30)).unwrap(), ExitReason::TimedOut);
    assert!(counter(&m) > first, "the counter kept counting from where it was");
}

/// A timeout stops the vCPU with exactly one kick, and reading the counts while the machine is
/// stopped gives the same answer twice.
#[test]
fn a_timeout_is_one_interrupted_exit() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let mut m = bare(&counting_loop());
    for runs in 1..=3 {
        assert_eq!(m.run_for(Duration::from_millis(20)).unwrap(), ExitReason::TimedOut);
        let c = m.exit_counts();
        assert_eq!((c.interrupted, c.total()), (runs, runs), "{c}");
        assert_eq!(m.exit_counts(), c);
    }
}

/// Every exit is counted by the device it reached: the UART, and an address no device claims,
/// which ends the run (a read is answered first, so the next run goes on).
#[test]
fn every_exit_is_counted_by_its_kind() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    let code = assemble(|ops| {
        dynasm!(ops
            ; .arch aarch64
            ; movz x1, UART & 0xffff
            ; movk x1, UART >> 16, lsl 16
            ; movz w0, b'x' as u32
            ; strb w0, [x1]
            ; strb w0, [x1]
            ; strb w0, [x1]
            // The UART's line status register.
            ; ldrb w0, [x1, 5]
            // No device here: unclaimed MMIO, which ends the run.
            ; movz x2, 0x0400, lsl 16
            ; strb w0, [x2]
            ; ldrb w0, [x2]
            // PSCI SYSTEM_OFF.
            ; movz x0, 0x0008
            ; movk x0, 0x8400, lsl 16
            ; hvc 0
            ; b 0
        );
    });
    let mut m = bare(&code);
    assert_eq!(m.exit_counts(), ExitCounts::default(), "nothing before a run");
    let mut exits = Vec::new();
    loop {
        let exit = m.run().unwrap();
        exits.push(exit.clone());
        if exit == ExitReason::Poweroff {
            break;
        }
        assert!(exits.len() < 10, "{exits:?}");
    }
    assert_eq!(exits.len(), 3, "{exits:?}");
    let c = m.exit_counts();
    assert_eq!((c.serial, c.unclaimed_mmio), (4, 2), "{c}");
    assert_eq!((c.pci_config, c.pci_bar, c.rtc, c.halt), (0, 0, 0, 0), "{c}");
}

#[derive(Clone, Default)]
struct Counter(Arc<AtomicU64>);
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A machine keeps only a bounded tail of its serial output (board 13163), and its console sees
/// every byte.
#[test]
fn serial_output_keeps_a_bounded_tail_and_the_console_sees_everything() {
    if !hypervisor() {
        return;
    }
    let _one = one_machine();
    const LIMIT: usize = 4 << 10;
    const CHUNKS: u32 = 16; // 64 KiB
    const BUF: u64 = ENTRY + 0x1000;
    // Writes CHUNKS x 4096 bytes of the buffer to the UART, then powers off.
    let code = assemble(|ops| {
        dynasm!(ops
            ; .arch aarch64
            ; movz x1, UART & 0xffff
            ; movk x1, UART >> 16, lsl 16
            ; movz w5, CHUNKS
            ; outer:
            ; movz x3, (BUF & 0xffff) as u32
            ; movk x3, ((BUF >> 16) & 0xffff) as u32, lsl 16
            ; movz w4, 4096
            ; inner:
            ; ldrb w0, [x3], 1
            ; strb w0, [x1]
            ; subs w4, w4, 1
            ; b.ne <inner
            ; subs w5, w5, 1
            ; b.ne <outer
            ; movz x0, 0x0008
            ; movk x0, 0x8400, lsl 16
            ; hvc 0
            ; b 0
        );
    });
    let config =
        MachineConfig { mem_size: 64 << 20, serial_capture_limit: LIMIT, ..Default::default() };
    let mut m = Machine::new(config).expect("machine");
    m.load(ENTRY, &code).expect("load");
    m.set_entry(ENTRY).expect("entry");
    let pattern: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    m.load(BUF, &pattern).unwrap();
    let console = Counter::default();
    m.set_console(console.clone());

    assert_eq!(m.run().unwrap(), ExitReason::Poweroff);

    assert_eq!(console.0.load(Ordering::Relaxed), u64::from(CHUNKS) * 4096);
    let kept = m.serial_output();
    assert_eq!(kept.len(), LIMIT);
    assert_eq!(&kept[kept.len() - 4096..], &pattern[..], "the tail is the most recent output");
    assert_eq!(m.take_serial_output().len(), LIMIT);
    assert!(m.serial_output().is_empty());
}
