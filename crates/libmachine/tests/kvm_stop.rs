// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stopping a running machine from another thread, and run timeouts.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use common::{asm16, kvm_available, machine_with};
use iced_x86::code_asm::*;
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig, Waited};

/// Where `counting_loop` keeps its counter.
const COUNTER: u32 = 0x500;

/// A guest that never stops: it increments a counter in memory forever.
fn counting_loop() -> Vec<u8> {
    asm16(|a| {
        let mut top = a.create_label();
        a.set_label(&mut top)?;
        a.inc(dword_ptr(COUNTER))?;
        a.jmp(top)
    })
}

fn counter(m: &Machine) -> u32 {
    u32::from_le_bytes(m.read_memory(COUNTER.into(), 4).unwrap().try_into().unwrap())
}

/// Generous bound for "promptly", so a loaded CI machine does not flake.
const PROMPT: Duration = Duration::from_secs(2);

#[test]
fn stop_from_another_thread() {
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &counting_loop());
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
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &counting_loop());
    m.stop_handle().stop();
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    assert_eq!(counter(&m), 0, "the guest never ran");
    // The request was consumed: the next run goes on until its timeout.
    assert_eq!(m.run_for(Duration::from_millis(20)).unwrap(), ExitReason::TimedOut);
    assert!(counter(&m) > 0);
}

#[test]
fn run_for_times_out() {
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &counting_loop());
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
    if !kvm_available() {
        return;
    }
    let code = counting_loop();
    let mut m = machine_with(MachineConfig::default(), &code);
    assert_eq!(m.run_for(Duration::from_millis(30)).unwrap(), ExitReason::TimedOut);
    let first = counter(&m);
    let rip = m.registers().unwrap().rip;
    assert!((0x1000..0x1000 + code.len() as u64).contains(&rip), "rip {rip:#x} inside the loop");
    assert_eq!(m.run_for(Duration::from_millis(30)).unwrap(), ExitReason::TimedOut);
    assert!(counter(&m) > first, "the counter kept counting from where it was");
}

#[test]
fn every_machine_stops_independently() {
    if !kvm_available() {
        return;
    }
    let machines: Vec<_> =
        (0..8).map(|_| machine_with(MachineConfig::default(), &counting_loop())).collect();
    let handles: Vec<_> = machines.iter().map(Machine::stop_handle).collect();
    let runners: Vec<_> =
        machines.into_iter().map(|mut m| thread::spawn(move || m.run().unwrap())).collect();
    thread::sleep(Duration::from_millis(50));
    // Stop the even ones; the odd ones keep running until stopped later.
    for h in handles.iter().step_by(2) {
        h.stop();
    }
    // The stopped ones return promptly; how promptly depends on scheduling
    // (eight spinning vCPU threads plus other tests share the host), so wait
    // up to PROMPT rather than a fixed sleep.
    let deadline = Instant::now() + PROMPT;
    while !runners.iter().step_by(2).all(|r| r.is_finished()) {
        assert!(Instant::now() < deadline, "stopped machines did not return within {PROMPT:?}");
        thread::sleep(Duration::from_millis(5));
    }
    // The others never stop by themselves (their guests loop forever).
    for (i, r) in runners.iter().enumerate().skip(1).step_by(2) {
        assert!(!r.is_finished(), "machine {i} returned without being stopped");
    }
    for h in handles.iter().skip(1).step_by(2) {
        h.stop();
    }
    for r in runners {
        assert_eq!(r.join().unwrap(), ExitReason::Stopped);
    }
}

fn guest_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    );
    if dir.join("vmlinux").is_file() {
        Some(dir)
    } else {
        eprintln!("skipping: no test guest (run crates/libmachine/scripts/build-test-guest.sh)");
        None
    }
}

/// An idle Linux guest's vCPU waits in HLT inside KVM, where only a signal
/// gets it out: a timeout and a stop must both work there.
#[test]
fn idle_linux_guest_times_out_and_stops() {
    if !kvm_available() {
        return;
    }
    let Some(dir) = guest_dir() else { return };
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=hang".into(),
    };
    let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
    let mut m = Machine::boot_linux(config, &boot).unwrap();
    assert_eq!(m.run_for(Duration::from_millis(500)).unwrap(), ExitReason::TimedOut);
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    assert!(console.contains("libmachine-init: up "), "init ran:\n{console}");

    let handle = m.stop_handle();
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        handle.stop();
    });
    let start = Instant::now();
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    assert!(start.elapsed() < PROMPT);
    stopper.join().unwrap();
}

/// A run waited for in short steps runs undisturbed: its vCPU is kicked
/// once, when the run's timeout ends it, however many waits came before.
#[test]
fn waiting_in_steps_does_not_kick_the_vcpus() {
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &counting_loop());
    let start = Instant::now();
    let mut run = m.start_run(Some(Duration::from_millis(300)));
    let mut waits = 0;
    let exit = loop {
        run = match run.wait_for(Duration::from_millis(5)) {
            Waited::Ended(exit) => break exit.unwrap(),
            Waited::Running(run) => run,
        };
        waits += 1;
        assert_eq!(run.exit_counts().interrupted, 0, "kicked by wait {waits}");
    };
    assert_eq!(exit, ExitReason::TimedOut);
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert!(waits >= 20, "{waits} waits");
    let counts = m.exit_counts();
    assert_eq!(counts.interrupted, 1, "{counts}");
    assert!(counter(&m) > 0);
}

/// Stopping a run, or dropping it, leaves the machine between runs, with
/// no stop request behind: the next run runs.
#[test]
fn a_stopped_or_dropped_run_leaves_the_machine_runnable() {
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &counting_loop());
    let run = m.start_run(None);
    let run = match run.wait_for(Duration::from_millis(20)) {
        Waited::Running(run) => run,
        Waited::Ended(exit) => panic!("the run ended: {exit:?}"),
    };
    assert_eq!(run.stop().unwrap(), ExitReason::Stopped);
    let first = counter(&m);
    assert!(first > 0);

    drop(m.start_run(None));
    assert_eq!(m.run_for(Duration::from_millis(20)).unwrap(), ExitReason::TimedOut);
    assert!(counter(&m) > first);

    // A run that ends before it starts (a stop requested between runs)
    // reports it at the first wait.
    m.stop_handle().stop();
    match m.start_run(None).wait_for(Duration::ZERO) {
        Waited::Ended(exit) => assert_eq!(exit.unwrap(), ExitReason::Stopped),
        Waited::Running(_) => panic!("the stop request was lost"),
    }
}
