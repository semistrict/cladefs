// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest agent over virtio-vsock, end to end, on Hypervisor.framework: the test guest
//! (`crates/libmachine/scripts/arm64-guest.sh`) boots with `libmachine_action=agent`, a thread
//! runs the machine, and the tests talk to the agent from others, as callers do.
//!
//! Tests return early with a note when the guest is missing. A process has one VM: the tests
//! take turns.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::io::{Read, Write};
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{guest, one_machine};
use libmachine::agent::{Agent, Command, Event, ExitStatus};
use libmachine::{
    Error, ExitReason, LinuxBoot, Machine, MachineConfig, StopHandle, Vsock, is_guest_stopped,
};

/// Generous bound for "promptly", so that a loaded host does not flake.
const PROMPT: Duration = Duration::from_secs(2);

/// A booted guest with its agent answering, its machine running on a
/// thread of its own.
struct Guest {
    agent: Agent,
    vsock: Vsock,
    stop: StopHandle,
    /// The thread running the machine; it returns the machine when its
    /// run ends.
    runner: Option<JoinHandle<(ExitReason, Machine)>>,
    _one: MutexGuard<'static, ()>,
}

impl Guest {
    fn boot() -> Option<Guest> {
        Self::boot_with("")
    }

    /// With `extra` on the kernel's command line.
    fn boot_with(extra: &str) -> Option<Guest> {
        let (kernel, initrd) = guest()?;
        let one = one_machine();
        let boot = LinuxBoot {
            kernel,
            initrd: Some(initrd),
            cmdline: format!("quiet libmachine_action=agent {extra}"),
        };
        let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
        let mut m = Machine::boot_linux(config, &boot).expect("boot");
        let agent = m.agent().expect("a board machine has an agent");
        let vsock = m.vsock().expect("a board machine has a vsock");
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        let guest = Guest { agent, vsock, stop, runner: Some(runner), _one: one };
        let start = Instant::now();
        if let Err(e) = guest.agent.wait_ready(Duration::from_secs(60)) {
            let (exit, console) = guest.finish();
            panic!("agent not ready: {e}; exit {exit:?}, console:\n{console}");
        }
        eprintln!("agent ready {:?} after creating the machine", start.elapsed());
        Some(guest)
    }

    /// Stops the machine: how its run ended, and its console.
    fn finish(mut self) -> (ExitReason, String) {
        self.stop.stop();
        let (exit, m) = self.runner.take().unwrap().join().expect("runner");
        (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
    }

    fn exec(&self, argv: &[&str]) -> libmachine::agent::Output {
        let mut cmd = Command::new(argv[0]);
        cmd.args(&argv[1..]);
        self.agent.exec(&cmd).unwrap_or_else(|e| panic!("{argv:?}: {e}"))
    }

    fn sh(&self, script: &str) -> String {
        let out = self.exec(&["sh", "-c", script]);
        assert!(out.status.success(), "{script}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        if let Some(runner) = self.runner.take() {
            self.stop.stop();
            let _ = runner.join();
        }
    }
}

/// The agent's hello frame's length on the wire, from the protocol itself.
fn hello_len() -> usize {
    libmachine_agent::Frame::Hello { instance: 0, generation: 0 }.encode().len()
}

fn exited(code: i32) -> ExitStatus {
    ExitStatus { code: Some(code), signal: None }
}

#[test]
fn exec_gives_output_and_exit_code() {
    let Some(g) = Guest::boot() else { return };
    let out = g.exec(&["sh", "-c", "echo out; echo err >&2; exit 7"]);
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
    assert_eq!(out.status, exited(7));

    // Environment, arguments and working directory as given.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo \"$FOO|$1|$(pwd)|$HOME\"", "argv0", "first arg"])
        .env("FOO", "bar baz")
        .current_dir("/tmp");
    let out = g.agent.exec(&cmd).unwrap();
    assert_eq!(out.stdout, b"bar baz|first arg|/tmp|\n", "{out:?}");
    assert!(out.status.success());
    // Exactly that environment.
    let env = g.exec(&["env"]).stdout;
    let path = format!("PATH={}\n", libmachine::agent::DEFAULT_PATH);
    assert_eq!(String::from_utf8_lossy(&env), path);

    // Output larger than any buffer on the way.
    let out = g.exec(&["sh", "-c", "head -c 3000000 /dev/zero; head -c 100000 /dev/zero >&2"]);
    assert_eq!((out.stdout.len(), out.stderr.len()), (3_000_000, 100_000));
    let (exit, console) = g.finish();
    assert_eq!(exit, ExitReason::Stopped, "console:\n{console}");
}

#[test]
fn failing_commands() {
    let Some(g) = Guest::boot() else { return };
    assert_eq!(g.exec(&["false"]).status, exited(1));
    let killed = g.exec(&["sh", "-c", "kill -9 $$"]).status;
    assert_eq!(killed, ExitStatus { code: None, signal: Some(9) });
    // A program that does not exist, by path and by name.
    for missing in ["/no/such/program", "no-such-program"] {
        let err = g.agent.exec(&Command::new(missing)).unwrap_err();
        let Error::Guest { source, what } = &err else { panic!("{missing}: {err}") };
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound, "{what}");
        assert!(what.contains(missing), "{what}");
    }
    let err = g.agent.exec(Command::new("true").current_dir("/no/such/dir")).unwrap_err();
    assert!(
        matches!(&err, Error::Guest { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
        "{err}"
    );
}

#[test]
fn stdin_streams_to_a_running_process() {
    let Some(g) = Guest::boot() else { return };
    // Interactive: each line comes back before the next is written.
    let p = g.agent.spawn(&Command::new("cat")).unwrap();
    assert!(p.pid() > 1);
    for line in ["hello\n", "world\n"] {
        p.write_stdin(line.as_bytes()).unwrap();
        let mut echoed = Vec::new();
        while echoed.len() < line.len() {
            match p.next_event().unwrap() {
                Some(Event::Stdout(d)) => echoed.extend(d),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(echoed, line.as_bytes());
    }
    p.close_stdin().unwrap();
    let out = p.wait().unwrap();
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");

    // Megabytes both ways at once: flow control in each direction.
    let data: Vec<u8> = (0..4u32 << 20).map(|i| (i * 7 / 3) as u8).collect();
    let p = g.agent.spawn(&Command::new("cat")).unwrap();
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            p.write_stdin(&data).unwrap();
            p.close_stdin().unwrap();
        });
        p.wait().unwrap()
    });
    assert!(out.stdout == data, "{} bytes back", out.stdout.len());

    // Stdin given up front, to a command that reads it all.
    let out = g.agent.exec(Command::new("wc").arg("-c").stdin(vec![1u8; 3 << 20])).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3145728");
    // ...and to one that reads none of it.
    let out = g.agent.exec(Command::new("true").stdin(vec![1u8; 3 << 20])).unwrap();
    assert!(out.status.success());

    // A signal reaches the process group.
    let p = g.agent.spawn(Command::new("sleep").arg("1000")).unwrap();
    p.signal(15).unwrap();
    assert_eq!(p.wait().unwrap().status.signal, Some(15));
}

#[test]
fn files_copy_both_ways() {
    let Some(g) = Guest::boot() else { return };
    let data: Vec<u8> = (0..3u32 << 20).map(|i| (i ^ (i >> 11)) as u8).collect();
    g.agent.write_file("/tmp/blob", &data, 0o750).unwrap();
    assert_eq!(g.sh("stat -c %a:%s /tmp/blob"), "750:3145728\n");
    assert!(g.agent.read_file("/tmp/blob").unwrap() == data);
    // Overwritten whole; nothing left beside it.
    g.agent.write_file("/tmp/blob", b"short", 0o600).unwrap();
    assert_eq!(g.agent.read_file("/tmp/blob").unwrap(), b"short");
    assert_eq!(g.sh("ls -a /tmp"), ".\n..\nblob\n");
    assert_eq!(g.agent.read_file("/tmp/empty").ok(), None);
    g.agent.write_file("/tmp/empty", b"", 0o644).unwrap();
    assert_eq!(g.agent.read_file("/tmp/empty").unwrap(), b"");

    // Host files in and out.
    let dir = std::env::temp_dir().join(format!("libmachine-agent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let host_in = dir.join("in");
    std::fs::write(&host_in, b"#!/bin/sh\necho from a copied script\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&host_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    g.agent.copy_to_guest(&host_in, "/tmp/script").unwrap();
    assert_eq!(g.sh("/tmp/script"), "from a copied script\n");
    g.sh("echo guest data > /tmp/out");
    let host_out = dir.join("out");
    g.agent.copy_from_guest("/tmp/out", &host_out).unwrap();
    assert_eq!(std::fs::read(&host_out).unwrap(), b"guest data\n");
    std::fs::remove_dir_all(&dir).unwrap();

    // Errors carry the guest's errno.
    let kind = |e: Error| match e {
        Error::Guest { source, .. } => source.raw_os_error(),
        other => panic!("{other}"),
    };
    assert_eq!(kind(g.agent.read_file("/no/such").unwrap_err()), Some(2));
    assert_eq!(kind(g.agent.read_file("/tmp").unwrap_err()), Some(21), "EISDIR");
    assert_eq!(kind(g.agent.write_file("/no/dir/f", b"x", 0o644).unwrap_err()), Some(2));
    // A path naming a directory, however it is spelt (Rust's Path drops a
    // trailing "/" or "."; the kernel does not).
    for dir in ["/tmp/", "/tmp/.", "/tmp/..", "/tmp//", "/"] {
        let err = g.agent.write_file(dir, b"x", 0o644).unwrap_err();
        assert_eq!(kind(err), Some(21), "{dir}");
    }
    let left = g.sh("ls -a / /tmp | grep -c libmachine- || true");
    assert_eq!(left.trim(), "0", "no temporary file left behind");
}

#[test]
fn many_execs_in_a_row_and_at_once() {
    let Some(g) = Guest::boot() else { return };
    let start = Instant::now();
    let n = 200;
    for _ in 0..n {
        assert!(g.agent.exec(&Command::new("true")).unwrap().status.success());
    }
    eprintln!("{n} execs of true in a row: {:?} each", start.elapsed() / n);
    std::thread::scope(|s| {
        for t in 0..8 {
            let agent = g.agent.clone();
            s.spawn(move || {
                for i in 0..25 {
                    let out = agent.exec(Command::new("echo").arg(format!("{t}-{i}"))).unwrap();
                    assert_eq!(out.stdout, format!("{t}-{i}\n").into_bytes());
                }
            });
        }
    });
    // Nothing piles up in the guest.
    let zombies = g.sh("grep -l '^State:.*Z' /proc/[0-9]*/status | wc -l");
    assert_eq!(zombies.trim(), "0");
}

/// Waits until `ps` in the guest does not show `what`.
fn gone_from_ps(g: &Guest, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while g.sh("ps").contains(what) {
        assert!(Instant::now() < deadline, "{what} still runs");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn resets_are_met_by_connecting_again() {
    let Some(g) = Guest::boot() else { return };
    // A running command's connection reset: the command runs on, and its
    // process takes it up again.
    let p = g.agent.spawn(Command::new("sleep").arg("1000")).unwrap();
    g.vsock.reset_connections();
    std::thread::sleep(Duration::from_millis(50));
    assert!(g.sh("ps").contains("sleep 1000"), "it still runs");
    p.kill().unwrap();
    assert_eq!(p.wait().unwrap().status.signal, Some(9));
    drop(p);
    // Dropped after a reset: taken up again to be released, so killed.
    let p = g.agent.spawn(Command::new("sleep").arg("1001")).unwrap();
    g.vsock.reset_connections();
    drop(p);
    gone_from_ps(&g, "sleep 1001");
    // Dropped before its connection was reset: killed as well.
    let p = g.agent.spawn(Command::new("sleep").arg("1002")).unwrap();
    drop(p);
    gone_from_ps(&g, "sleep 1002");
    // The agent still listens.
    assert_eq!(g.sh("echo still here"), "still here\n");

    // Under a storm of resets, reads are made again until they get the
    // whole file, and commands are sent again until they have run, once.
    let data = vec![0x5au8; 256 << 10];
    g.agent.write_file("/tmp/data", &data, 0o644).unwrap();
    let storm = AtomicBool::new(true);
    let resets = AtomicUsize::new(0);
    let (reads, ok) = std::thread::scope(|s| {
        s.spawn(|| {
            while storm.load(Ordering::Relaxed) {
                g.vsock.reset_connections();
                resets.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let _calm = Calm(&storm);
        let agent = g.agent.clone().retry_for(Duration::from_secs(10));
        let start = Instant::now();
        let mut reads = 0;
        while start.elapsed() < Duration::from_millis(500) {
            assert!(agent.read_file("/tmp/data").unwrap() == data);
            reads += 1;
        }
        let mut ok = 0;
        while start.elapsed() < Duration::from_millis(1000) {
            let cmd = Command::new("sh").args(["-c", "echo x >> /tmp/count"]).clone();
            let out = agent.exec(&cmd).unwrap();
            assert!(out.status.success());
            ok += 1;
        }
        storm.store(false, Ordering::Relaxed);
        (reads, ok)
    });
    let resets = resets.load(Ordering::Relaxed);
    assert!(resets >= 50, "only {resets} resets");
    let ran = g.sh("cat /tmp/count 2>/dev/null | wc -l");
    let ran: usize = ran.trim().parse().unwrap();
    eprintln!("{resets} resets: {reads} reads, {ok} execs, {ran} lines");
    assert_eq!(ran, ok, "each ran once");
}

/// A small deterministic generator, for the reset storms below.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Ends a reset storm when dropped, also when the work under it panics
/// (else the storm, and the test, would never end).
struct Calm<'a>(&'a AtomicBool);

impl Drop for Calm<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Resets every connection at random moments (0 to `max_ms` apart) while
/// `work` runs; returns its result and how many resets there were.
fn under_resets<T>(g: &Guest, seed: u64, max_ms: u64, work: impl FnOnce() -> T) -> (T, usize) {
    let storm = AtomicBool::new(true);
    let resets = AtomicUsize::new(0);
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            let mut rng = Rng(seed);
            while storm.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_micros(rng.below(max_ms * 1000 + 1)));
                g.vsock.reset_connections();
                resets.fetch_add(1, Ordering::Relaxed);
            }
        });
        let _calm = Calm(&storm);
        work()
    });
    (out, resets.load(Ordering::Relaxed))
}

/// board 13131's test: connections reset at random points of many execs, each
/// appending its number to a file and printing a few kilobytes: every exec
/// succeeds, every command ran exactly once, and every output arrived once,
/// in order.
#[test]
fn execs_under_random_resets_run_exactly_once() {
    let Some(g) = Guest::boot() else { return };
    let (threads, each) = (4, 60);
    let expected_output: String = (1..=2000).map(|i| format!("{i}\n")).collect();
    // Resets 0 to 8 ms apart: many land inside each exec. (Much closer
    // than an exchange takes and nothing gets through: each reset starts
    // every connection over.)
    let (_, resets) = under_resets(&g, 0x5eed, 8, || {
        std::thread::scope(|s| {
            for t in 0..threads {
                let agent = g.agent.clone().retry_for(Duration::from_secs(10));
                let expected_output = &expected_output;
                s.spawn(move || {
                    for i in 0..each {
                        let n = t * 1000 + i;
                        let script = format!("echo {n} >> /tmp/once; seq 1 2000; echo {n} >&2");
                        let out = agent
                            .exec(Command::new("sh").args(["-c", &script]))
                            .unwrap_or_else(|e| panic!("exec {n}: {e}"));
                        assert!(out.status.success(), "{n}: {out:?}");
                        assert!(
                            out.stdout == expected_output.as_bytes(),
                            "{n}: {} bytes of output",
                            out.stdout.len()
                        );
                        assert_eq!(out.stderr, format!("{n}\n").into_bytes());
                    }
                });
            }
        })
    });
    let log = g.agent.read_file("/tmp/once").unwrap();
    let mut ran: Vec<u32> =
        String::from_utf8(log).unwrap().lines().map(|l| l.parse().unwrap()).collect();
    ran.sort();
    let mut expected: Vec<u32> =
        (0..threads).flat_map(|t| (0..each).map(move |i| t * 1000 + i)).collect();
    expected.sort();
    eprintln!("{} execs under {resets} resets", expected.len());
    assert!(resets >= 50, "only {resets} resets");
    assert_eq!(ran, expected, "each command ran exactly once");
    // No command is left running.
    assert!(!g.sh("ps").contains("seq 1 2000"));
}

/// A spawned process's stdin and output both stream across resets: what
/// it gets is what was written, each byte once, and so is what comes back;
/// signals arrive once.
#[test]
fn a_process_streams_through_resets() {
    let Some(g) = Guest::boot() else { return };
    let agent = g.agent.clone().retry_for(Duration::from_secs(10));
    let data: Vec<u8> = (0..3u32 << 20).map(|i| (i * 31 / 7) as u8).collect();
    let (out, resets) = under_resets(&g, 0xcafe, 8, || {
        let p = agent.spawn(Command::new("sh").args(["-c", "cat; echo done >&2"])).unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                for chunk in data.chunks(10_000) {
                    p.write_stdin(chunk).unwrap();
                }
                p.close_stdin().unwrap();
            });
            p.wait().unwrap()
        })
    });
    eprintln!("3 MiB through cat under {resets} resets");
    assert!(resets >= 10, "only {resets} resets");
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(out.stderr, b"done\n");
    assert!(out.stdout == data, "{} bytes back", out.stdout.len());

    // A signal sent across a reset arrives (that each arrives once is the
    // client's unit tests': a shell's trap cannot count signals that come
    // close together).
    let p = agent.spawn(Command::new("sleep").arg("1004")).unwrap();
    let (_, _) = under_resets(&g, 0xbeef, 4, || {
        std::thread::sleep(Duration::from_millis(30));
        p.signal(15).unwrap();
    });
    assert_eq!(p.wait().unwrap().status.signal, Some(15));
}

/// A reset while everything is backed up: stdin the process has not taken
/// in flight, and output nobody has read. Taking the process up again sends
/// that stdin again, which the process takes only once its output drains:
/// the replay must not keep the output from being read (board 13193: the reader
/// waited for the writing lock the replaying writer held, until the next
/// reset; a storm of resets on a loaded host never let it end).
#[test]
fn a_reset_while_stdin_and_output_are_backed_up() {
    let Some(g) = Guest::boot() else { return };
    let agent = g.agent.clone().retry_for(Duration::from_secs(5));
    let data: Vec<u8> = (0..4u32 << 20).map(|i| (i * 13 / 5) as u8).collect();
    let p = agent.spawn(&Command::new("cat")).unwrap();
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            for chunk in data.chunks(64 << 10) {
                p.write_stdin(chunk).unwrap();
            }
            p.close_stdin().unwrap();
        });
        // Nothing reads yet: cat's output fills what holds it, then cat
        // stops taking stdin, and stdin backs up behind it.
        std::thread::sleep(Duration::from_millis(500));
        g.vsock.reset_connections();
        p.wait().unwrap()
    });
    assert!(out.status.success(), "{:?}", out.status);
    assert!(out.stdout == data, "{} bytes back", out.stdout.len());
}

/// An agent that restarted forgot what it ran: a process whose agent
/// restarted fails with ConnectionReset rather than taking the new agent's
/// answer for its command's.
#[test]
fn an_agent_that_restarted_is_not_taken_for_the_one_before() {
    let Some(g) = Guest::boot() else { return };
    let start = "libmachine-agent --port 101 </dev/null >/dev/null 2>&1 & echo $! > /tmp/a101";
    g.sh(start);
    let second = g.agent.clone().port(101);
    second.wait_ready(Duration::from_secs(10)).unwrap();
    let p = second.spawn(Command::new("sleep").arg("1003")).unwrap();
    g.sh("kill -9 $(cat /tmp/a101)");
    g.sh(start);
    second.wait_ready(Duration::from_secs(10)).unwrap();
    let err = p.wait().unwrap_err();
    assert!(matches!(&err, Error::ConnectionReset(m) if m.contains("restarted")), "{err}");
    // The new one serves new commands.
    let out = second.exec(Command::new("echo").arg("new")).unwrap();
    assert_eq!(out.stdout, b"new\n");
    g.sh("kill -9 $(cat /tmp/a101); kill $(pidof sleep) || true");
}

/// board 13133's test: a guest that powers off, resets or triple-faults fails
/// what waits on it at once (a process's wait, a raw stream's read), and
/// what comes next (connects, agent calls), with a reset; a later run of
/// the machine does not bring it back (libmachine does not reboot it).
#[test]
fn a_guest_that_stops_fails_what_waits_on_it_at_once() {
    for (extra, stopper, expected) in
        [("", "poweroff", ExitReason::Poweroff), ("", "reboot", ExitReason::Reset)]
    {
        let Some(mut g) = Guest::boot_with(extra) else {
            return;
        };
        let p = g.agent.spawn(Command::new("sleep").arg("1000")).unwrap();
        // A raw connection to the agent, past its preamble and hello: the
        // agent waits for this side's preamble, so a read waits.
        let raw = g.vsock.connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(5)).unwrap();
        let mut hello = vec![0; 8 + hello_len()];
        (&raw).read_exact(&mut hello).unwrap();
        std::thread::scope(|s| {
            let waiter = s.spawn(|| p.wait());
            let reader = s.spawn(|| (&raw).read(&mut [0; 1]));
            let stopping = {
                let agent = g.agent.clone();
                s.spawn(move || agent.exec(Command::new(stopper).arg("-f")))
            };
            let (exit, m) = g.runner.take().unwrap().join().unwrap();
            let stopped_at = Instant::now();
            assert_eq!(exit, expected, "{extra} {stopper}");
            let waited = waiter.join().unwrap().unwrap_err();
            assert!(matches!(waited, Error::GuestGone(_)), "{waited}");
            // A reset: the guest's driver may have reset the device on its
            // way down, before the run ended.
            let read = reader.join().unwrap().unwrap_err();
            assert_eq!(read.kind(), std::io::ErrorKind::ConnectionReset, "{read}");
            // The command that stopped it may or may not have answered.
            match stopping.join().unwrap() {
                Ok(_) | Err(Error::GuestGone(_)) => {}
                Err(e) => panic!("{stopper}: {e}"),
            }
            assert!(stopped_at.elapsed() < PROMPT, "{:?}", stopped_at.elapsed());
            // What comes next fails at once too.
            let start = Instant::now();
            let write = (&raw).write(b"lmag").unwrap_err();
            assert!(is_guest_stopped(&write), "{write}");
            let connect = g
                .vsock
                .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(60))
                .unwrap_err();
            assert!(is_guest_stopped(&connect), "{connect}");
            let ping = g.agent.ping().unwrap_err();
            assert!(matches!(ping, Error::GuestGone(_)), "{ping}");
            let spawn = g.agent.spawn(&Command::new("true")).unwrap_err();
            assert!(matches!(spawn, Error::GuestGone(_)), "{spawn}");
            let ready = g.agent.wait_ready(Duration::from_secs(60)).unwrap_err();
            assert!(matches!(ready, Error::GuestGone(_)), "{ready}");
            assert!(start.elapsed() < PROMPT, "{:?}", start.elapsed());
            // Running the machine again does not reboot the guest: still
            // stopped.
            let mut m = m;
            let again = m.run_for(Duration::from_millis(300));
            eprintln!("{extra} {stopper}: {exit:?}, then {again:?}");
            let connect = g
                .vsock
                .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(60))
                .unwrap_err();
            assert!(is_guest_stopped(&connect), "{connect}");
            // Gone with the machine.
            drop(m);
            let connect = g
                .vsock
                .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(60))
                .unwrap_err();
            assert_eq!(connect.kind(), std::io::ErrorKind::NotConnected);
        });
        drop(p);
    }
}

/// A connection made right after a reset must not be reset by it: the
/// guest takes the transport reset event a little later, and resets every
/// connection it has then.
#[test]
fn connections_after_a_reset_survive_it() {
    let Some(g) = Guest::boot() else { return };
    for i in 0..100 {
        g.vsock.reset_connections();
        let out = g
            .agent
            .exec(Command::new("echo").arg(i.to_string()))
            .unwrap_or_else(|e| panic!("exec {i} after a reset: {e}"));
        assert_eq!(out.stdout, format!("{i}\n").into_bytes());
    }
}

#[test]
fn guest_initiated_connections() {
    let Some(g) = Guest::boot() else { return };
    let listener = g.vsock.listen(5000).unwrap();
    let mut cmd = Command::new("libmachine-agent");
    cmd.args(["connect", "5000"]).stdin("hello from the guest");
    let agent = g.agent.clone();
    let exec = std::thread::spawn(move || agent.exec(&cmd));
    let mut s = listener.accept(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(s.local_port(), 5000);
    let mut got = String::new();
    s.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello from the guest");
    s.write_all(b"hello from the host").unwrap();
    drop(s);
    let out = exec.join().unwrap().unwrap();
    assert_eq!(out.stdout, b"hello from the host", "{out:?}");
    assert!(out.status.success());

    // Nothing listening: refused in the guest.
    let out = g.exec(&["libmachine-agent", "connect", "5001"]);
    assert_eq!(out.status, exited(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("reset"), "{out:?}");
}

#[test]
fn the_agent_refuses_another_protocol_version() {
    let Some(g) = Guest::boot() else { return };
    // Version 1 (no request IDs, no hello) is refused, not converted.
    let mut s = g.vsock.connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(5)).unwrap();
    s.write_all(b"lmag\x01\0\0\0").unwrap();
    let mut answer = Vec::new();
    s.read_to_end(&mut answer).unwrap();
    assert_eq!(answer[..8], libmachine_agent::preamble(), "the agent's own version");
    assert_eq!(answer[12], 0x88, "its hello");
    let message = String::from_utf8_lossy(&answer[8 + hello_len()..]);
    assert!(message.contains("version 1"), "{message}");
    // A port nobody listens on is refused.
    let err = g.vsock.connect(4242, Duration::from_secs(5)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[test]
fn a_stopped_or_gone_machine_fails_instead_of_hanging() {
    let Some(mut g) = Guest::boot() else { return };
    let p = g.agent.spawn(&Command::new("cat")).unwrap();
    // The machine stops: waits for the guest time out.
    g.stop.stop();
    let (exit, m) = g.runner.take().unwrap().join().unwrap();
    assert_eq!(exit, ExitReason::Stopped);
    let agent = g.agent.clone().timeout(Some(Duration::from_millis(200)));
    let err = agent.ping().unwrap_err();
    assert!(matches!(err, Error::TimedOut(_)), "{err}");
    // The machine is gone: what waited fails, and so does what comes next.
    let started = Instant::now();
    let waiter = std::thread::spawn(move || p.wait());
    std::thread::sleep(Duration::from_millis(50));
    drop(m);
    assert!(waiter.join().unwrap().is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    let err = g.agent.ping().unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = g.agent.wait_ready(Duration::from_secs(60)).unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// However a run ends, a guest that powered off fails what waits on it: here
/// the run is driven through `start_run` and `wait_for`, as Python's `run()`
/// does, not through `Machine::run`. (Found merging board 13133 onto board 13129:
/// the "guest stopped" hook ran only in `run`/`run_for`, so a run ended
/// through a `Run` left vsock callers waiting.)
#[test]
fn a_guest_that_stops_fails_its_vsock_however_the_run_was_waited_on() {
    let Some((kernel, initrd)) = guest() else { return };
    let _one = one_machine();
    let boot = LinuxBoot {
        kernel,
        initrd: Some(initrd),
        // The test init powers the machine off by itself.
        cmdline: "quiet".into(),
    };
    let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    let vsock = m.vsock().expect("a board machine has a vsock");
    let mut run = m.start_run(Some(Duration::from_secs(60)));
    let exit = loop {
        match run.wait_for(Duration::from_millis(20)) {
            libmachine::Waited::Ended(outcome) => break outcome.expect("run"),
            libmachine::Waited::Running(r) => run = r,
        }
    };
    assert_eq!(exit, ExitReason::Poweroff);
    let start = Instant::now();
    let err = vsock
        .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(5))
        .expect_err("the guest is gone");
    assert!(libmachine::is_guest_stopped(&err), "{err}");
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
}
