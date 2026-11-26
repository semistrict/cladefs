// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Suspending a sandbox while it waits (board 13203, board 13213), on the test
//! guest (`crates/libmachine/scripts/build-test-guest.sh`): its whole machine goes to a file
//! and leaves the process (VM, threads, RAM), its connections stay open,
//! and what comes for it wakes it, on the same connection.
//!
//! Tests return early with a note when /dev/kvm or the guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::kvm_available;
use libmachine::agent::{Command, Event};
use libmachine::net::{EgressPolicy, NetworkConfig};
use libmachine::sandbox::{Ended, Sandbox, SandboxBuilder};
use libmachine::{Error, LinuxBoot};
use sha2::{Digest, Sha256};

const MIB: u64 = 1 << 20;
/// How long the slow server takes to answer.
const DELAY: Duration = Duration::from_secs(2);

fn guest_dir() -> Option<PathBuf> {
    if !kvm_available() {
        return None;
    }
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    );
    if ["vmlinux", "initramfs.cpio"].iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        eprintln!(
            "skipping: no test guest in {} (run crates/libmachine/scripts/build-test-guest.sh)",
            dir.display()
        );
        None
    }
}

fn builder(dir: &std::path::Path) -> SandboxBuilder {
    Sandbox::builder(LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=agent".into(),
    })
    .mem_size(256 << 20)
    .ready_timeout(Duration::from_secs(60))
}

/// Where suspended guests' RAM goes: on the disk target/ is on.
fn ram_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    cmd
}

async fn stdout(sb: &Sandbox, script: &str) -> String {
    let out = sb.exec(&sh(script)).await.expect("exec");
    assert!(out.status.success(), "{script}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// This process's resident memory, in bytes.
fn resident() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

/// The guest's monotonic clock, from its /proc/uptime line.
fn uptime(line: &str) -> f64 {
    line.split_whitespace().next().unwrap().parse().unwrap()
}

/// The stdout and stderr of a process, to its end.
async fn output(p: &libmachine::sandbox::SandboxProcess) -> (String, String, Option<i32>) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        match p.next_event().await.expect("an event") {
            Some(Event::Stdout(b)) => out.extend(b),
            Some(Event::Stderr(b)) => err.extend(b),
            Some(Event::Exit(status)) => {
                assert!(p.next_event().await.unwrap().is_none());
                return (
                    String::from_utf8_lossy(&out).into_owned(),
                    String::from_utf8_lossy(&err).into_owned(),
                    status.code,
                );
            }
            None => panic!("no exit event"),
        }
    }
}

/// A server on the host's loopback that takes one HTTP request, says it
/// has it, then answers `reply` to it after `delay`: what a guest's call to
/// a slow LLM looks like from the host.
fn slow_server(
    delay: Duration,
    reply: Option<Vec<u8>>,
) -> (u16, mpsc::Receiver<()>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (asked, asked_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            let n = conn.read(&mut buf).unwrap();
            assert!(n > 0, "the request ended early: {request:?}");
            request.extend_from_slice(&buf[..n]);
        }
        asked.send(()).unwrap();
        std::thread::sleep(delay);
        match reply {
            Some(body) => {
                let head = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                conn.write_all(head.as_bytes()).unwrap();
                conn.write_all(&body).unwrap();
            }
            // A reset: closed with data unread would do, a zero linger
            // says it outright.
            None => {
                socket2::SockRef::from(&conn).set_linger(Some(Duration::ZERO)).unwrap();
                drop::<TcpStream>(conn);
            }
        }
    });
    (port, asked_rx, server)
}

async fn networked(dir: &std::path::Path, port: u16) -> Sandbox {
    builder(dir)
        .network(NetworkConfig::new(EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port)))
        .create()
        .await
        .expect("create a networked sandbox")
}

/// The case board 13203 is for: the guest asks a slow server, the sandbox is
/// suspended while it waits (its RAM leaves the process, its vCPUs stop),
/// and the answer wakes it and completes the request on the same
/// connection, every byte there; the guest's clock ran on meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_answer_wakes_a_suspended_sandbox_on_the_same_connection() {
    let Some(dir) = guest_dir() else { return };
    let body: Vec<u8> = (0..600_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let digest = hex(&Sha256::digest(&body));
    let (port, asked, server) = slow_server(DELAY, Some(body));
    let sb = networked(&dir, port).await;
    // RAM the guest uses, for the suspension to give back: 48 MiB of a
    // file in its RAM-backed root.
    stdout(&sb, "head -c 50331648 /dev/urandom > /tmp/ballast").await;
    let ballast = stdout(&sb, "sha256sum /tmp/ballast").await;

    let p = sb
        .spawn(&sh(&format!(
            "cat /proc/uptime; wget -q -O /tmp/answer http://10.0.2.1:{port}/answer; \
             echo rc=$?; cat /proc/uptime; sha256sum /tmp/answer"
        )))
        .await
        .unwrap();
    asked.recv_timeout(Duration::from_secs(30)).expect("the guest's request reached the server");
    let before = resident();
    let asked_at = Instant::now();
    sb.suspend(ram_dir()).await.expect("suspend the waiting guest");
    assert!(sb.suspended());
    let after = resident();
    assert!(
        before.saturating_sub(after) >= 40 * MIB,
        "resident before the suspension {} MiB, after {} MiB",
        before / MIB,
        after / MIB
    );
    // Nothing but the answer wakes it.
    tokio::time::sleep(DELAY / 2).await;
    assert!(sb.suspended(), "woke before the answer");

    let (out, err, code) = output(&p).await;
    assert!(asked_at.elapsed() >= DELAY);
    assert!(!sb.suspended());
    assert_eq!(code, Some(0), "{out}\n{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[1], "rc=0", "{out}\n{err}");
    assert!(lines[3].starts_with(&digest), "{out}");
    let waited = uptime(lines[2]) - uptime(lines[0]);
    assert!(
        waited >= DELAY.as_secs_f64() - 0.05,
        "the guest's clock counted {waited} s of a {DELAY:?} wait"
    );
    assert_eq!(stdout(&sb, "sha256sum /tmp/ballast").await, ballast);
    let console = String::from_utf8_lossy(&sb.console_output()).into_owned();
    for complaint in ["soft lockup", "rcu_sched", "stall", "clocksource"] {
        assert!(!console.contains(complaint), "{console}");
    }
    server.join().unwrap();
    assert_eq!(sb.close().await, Ended::Closed);
}

/// A server that resets the connection while the guest is suspended: the
/// reset wakes it and reaches the guest as one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_while_suspended_reaches_the_guest() {
    let Some(dir) = guest_dir() else { return };
    let (port, asked, server) = slow_server(DELAY / 2, None);
    let sb = networked(&dir, port).await;
    let p = sb
        .spawn(&sh(&format!("wget -q -O /tmp/answer http://10.0.2.1:{port}/answer; echo rc=$?")))
        .await
        .unwrap();
    asked.recv_timeout(Duration::from_secs(30)).unwrap();
    sb.suspend(ram_dir()).await.unwrap();
    let (out, err, _) = output(&p).await;
    assert_eq!(out.trim(), "rc=1", "{out}\n{err}");
    assert!(err.contains("reset"), "{err}");
    assert!(!sb.suspended());
    server.join().unwrap();
}

/// Without a network: an agent call wakes a suspended guest, so does
/// `resume`, and a close ends it; the guest's own timers do not wake it
/// (its ticker writes nothing while it is suspended), and what is in its
/// RAM is there after each suspension.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_calls_and_resume_wake_a_suspended_guest_and_close_ends_it() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    stdout(&sb, "head -c 8388608 /dev/urandom > /tmp/data").await;
    let data = stdout(&sb, "sha256sum /tmp/data").await;
    let ticker =
        sb.spawn(&sh("while :; do echo tick >> /tmp/ticks; usleep 50000; done")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let ticks = async || -> u32 { stdout(&sb, "wc -l < /tmp/ticks").await.trim().parse().unwrap() };
    for round in 0..3 {
        let before = ticks().await;
        sb.suspend(ram_dir()).await.expect("suspend");
        assert!(sb.suspended());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(sb.suspended(), "round {round}: woke by itself");
        if round == 1 {
            sb.resume().await.unwrap();
            assert!(!sb.suspended());
        }
        // The call wakes it (rounds 0 and 2). Awake, the ticker would
        // have ticked 10 times; suspended, at most one or two just before
        // and the one overdue at the wake.
        let after = ticks().await;
        assert!(after - before <= 3, "round {round}: {before} ticks, then {after}");
        assert!(!sb.suspended());
        assert_eq!(stdout(&sb, "sha256sum /tmp/data").await, data);
    }
    let awake = ticks().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(ticks().await > awake + 2, "the ticker runs on once awake");
    drop(ticker);

    sb.suspend(ram_dir()).await.unwrap();
    let start = Instant::now();
    assert_eq!(sb.close().await, Ended::Closed);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!sb.suspended());
    let err = sb.suspend(ram_dir()).await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = sb.resume().await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
}

/// A suspension right after a call: the call's vsock connection is still
/// closing, its last packet on the way to the guest. A guest with that
/// waiting is not idle; suspended anyway, the packet would wake it at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspension_right_after_a_call_holds() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    let mut woke = Vec::new();
    for round in 0..20 {
        assert_eq!(stdout(&sb, "echo hi").await, "hi\n");
        sb.suspend(ram_dir()).await.expect("suspend");
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !sb.suspended() {
            woke.push(round);
        }
    }
    assert!(woke.is_empty(), "woke at once in rounds {woke:?}");
    assert_eq!(sb.close().await, Ended::Closed);
}

/// With several vCPUs, every one must be idle: a busy one keeps the guest
/// awake, and once it idles the guest suspends and an agent call wakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_vcpu_must_be_idle() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).vcpus(2).create().await.unwrap();
    assert_eq!(stdout(&sb, "nproc").await, "2\n");
    let spin = sb.spawn(&sh("taskset 2 sh -c 'while :; do :; done'")).await.unwrap();
    let err = sb.suspend(ram_dir()).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err}");
    spin.kill().await.unwrap();
    // Its end (killed) is all that matters here.
    let _ = spin.wait().await;
    sb.suspend(ram_dir()).await.expect("suspend once idle");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sb.suspended());
    assert_eq!(stdout(&sb, "echo awake").await, "awake\n");
    assert!(!sb.suspended());
    assert_eq!(sb.close().await, Ended::Closed);
}

/// A guest that never idles is not suspended: the call fails, and the
/// guest runs on. So is a directory on tmpfs, which would keep the RAM in
/// memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_guests_and_memory_directories_are_refused() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    if std::path::Path::new("/dev/shm").is_dir() {
        let err = sb.suspend("/dev/shm").await.unwrap_err();
        assert!(matches!(&err, Error::InvalidConfig(m) if m.contains("tmpfs")), "{err}");
        assert!(!sb.suspended());
    }
    let spin = sb.spawn(&sh("while :; do :; done")).await.unwrap();
    let start = Instant::now();
    let err = sb.suspend(ram_dir()).await.unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err}");
    assert!(start.elapsed() >= libmachine::sandbox::SUSPEND_IDLE_WAIT);
    assert!(!sb.suspended());
    assert_eq!(stdout(&sb, "echo on").await, "on\n");
    spin.kill().await.unwrap();
    assert_eq!(sb.close().await, Ended::Closed);
}

/// How many of this process's threads have a name starting with `prefix`
/// (meaningful under cargo-nextest, which runs each test in a process of
/// its own; `cargo test` runs them side by side in one).
fn threads_named(prefix: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|name| name.starts_with(prefix))
        .count()
}

/// A field of /proc/self/status, its first number (kB for sizes).
fn status(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|l| l.starts_with(field))
        .unwrap_or_else(|| panic!("no {field} in /proc/self/status"));
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// The bytes this process has written through write calls (/proc/self/io).
fn written() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    let line = io.lines().find(|l| l.starts_with("wchar:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// The machine's threads now: vCPU, device, network (and its decider),
/// and the suspension's watcher. (Names are cut to 15 bytes in /proc:
/// "libmachine-devices" is "libmachine-devi".)
fn machine_threads() -> [usize; 4] {
    [
        threads_named("vcpu"),
        threads_named("libmachine-dev"),
        threads_named("libmachine-net"),
        threads_named("libmachine-wat"),
    ]
}

/// Suspends `sb`, retrying while its guest is busy for a moment (a
/// suspension right after host traffic often is), up to 10 s.
async fn suspend_soon(sb: &Sandbox) {
    let start = Instant::now();
    loop {
        match sb.suspend(ram_dir()).await {
            Ok(()) => return,
            Err(Error::Busy(why)) if start.elapsed() < Duration::from_secs(10) => {
                eprintln!("busy, again: {why}");
            }
            Err(e) => panic!("suspend: {e}"),
        }
    }
}

/// What board 13213 is for: a suspended sandbox holds no machine. Its vCPU,
/// device and network threads are gone, and so is the guest's RAM, mapping
/// and all (the process's virtual size drops by about the RAM's size);
/// only its watcher thread is left. The answer on the held connection
/// brings the machine back, every thread with it, and the guest carries on
/// on that connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_sandbox_holds_no_machine() {
    let Some(dir) = guest_dir() else { return };
    let body: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let digest = hex(&Sha256::digest(&body));
    let (port, asked, server) = slow_server(DELAY, Some(body));
    let sb = networked(&dir, port).await;
    stdout(&sb, "head -c 50331648 /dev/urandom > /tmp/ballast").await;
    let p = sb
        .spawn(&sh(&format!(
            "wget -q -O /tmp/answer http://10.0.2.1:{port}/answer; echo rc=$?; \
             sha256sum /tmp/answer"
        )))
        .await
        .unwrap();
    asked.recv_timeout(Duration::from_secs(30)).unwrap();
    let awake = machine_threads();
    assert!(
        awake[0] == 1 && awake[1] == 1 && awake[2] >= 1 && awake[3] == 0,
        "vCPU, device, network, watcher threads awake: {awake:?}"
    );
    let (size, resident_before) = (status("VmSize:"), resident());
    suspend_soon(&sb).await;
    assert!(sb.suspended());
    assert_eq!(machine_threads(), [0, 0, 0, 1], "while suspended");
    let freed = size.saturating_sub(status("VmSize:")) << 10;
    assert!(
        freed >= 200 * MIB,
        "the process's virtual size fell by {} MiB: the guest's 256 MiB are still mapped",
        freed / MIB
    );
    assert!(
        resident_before.saturating_sub(resident()) >= 40 * MIB,
        "resident before {} MiB, after {} MiB",
        resident_before / MIB,
        resident() / MIB
    );

    let (out, err, code) = output(&p).await;
    assert_eq!(code, Some(0), "{out}\n{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "rc=0", "{out}\n{err}");
    assert!(lines[1].starts_with(&digest), "{out}");
    assert!(!sb.suspended());
    let back = machine_threads();
    assert!(
        back[0] == 1 && back[1] == 1 && back[2] >= 1 && back[3] == 0,
        "threads once woken: {back:?}"
    );
    server.join().unwrap();
    assert_eq!(sb.close().await, Ended::Closed);
    assert_eq!(machine_threads()[..2], [0, 0]);
}

/// Waits until the sandbox's console holds `text`, at most 5 s: the guest's
/// console write returns before its UART has sent every byte.
async fn console_shows(sb: &Sandbox, text: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if String::from_utf8_lossy(&sb.console_output()).contains(text) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// The console carries on across suspensions, even one that comes right
/// after the guest wrote to it: its UART raised an interrupt for the
/// guest's driver (transmit done), which KVM delivers asynchronously; a
/// suspension that freed the VM with it on its way lost it, and the
/// driver, waiting for it, sent nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_carries_on_across_suspensions() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    for round in 0..10 {
        // A few KiB: the UART sends them a FIFO at a time, an interrupt
        // between each, while the suspension tries for an idle guest.
        stdout(&sb, &format!("(yes before-{round} | head -c 4096; echo) > /dev/console")).await;
        suspend_soon(&sb).await;
        stdout(&sb, &format!("echo after-{round} > /dev/console")).await;
        assert!(
            console_shows(&sb, &format!("after-{round}")).await,
            "round {round}: the console stopped: {}",
            String::from_utf8_lossy(&sb.console_output())
        );
    }
    assert_eq!(sb.close().await, Ended::Closed);
}

/// Closing a suspended sandbox fails what waits on its guest at once (the
/// output of a command still running), as closing a running one does: its
/// vsock host end, which no device serves while it is suspended, is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_suspended_sandbox_fails_what_waits_on_it() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    let p = sb.spawn(&sh("sleep 60")).await.unwrap();
    suspend_soon(&sb).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sb.suspended());
    assert_eq!(sb.close().await, Ended::Closed);
    let waited = tokio::time::timeout(Duration::from_secs(10), p.wait())
        .await
        .expect("the command's wait ends once the sandbox is closed");
    let err = waited.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
}

/// The first suspension writes the RAM the guest uses; the next one, after
/// a wake that changed little, writes little: only what changed, into the
/// same file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_suspension_writes_only_what_changed() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    stdout(&sb, "head -c 50331648 /dev/urandom > /tmp/ballast").await;
    let ballast = stdout(&sb, "sha256sum /tmp/ballast").await;
    let before = written();
    suspend_soon(&sb).await;
    let first = written() - before;
    assert!(first >= 40 * MIB, "the first suspension wrote {} MiB", first / MIB);
    // A call wakes it; it changes a little of its RAM.
    assert_eq!(stdout(&sb, "echo hi").await, "hi\n");
    let before = written();
    suspend_soon(&sb).await;
    let second = written() - before;
    assert!(
        second < 8 * MIB,
        "the second suspension wrote {} KiB (the first {} MiB)",
        second >> 10,
        first / MIB
    );
    assert_eq!(stdout(&sb, "sha256sum /tmp/ballast").await, ballast);
    assert_eq!(sb.close().await, Ended::Closed);
}

/// A server that sends its answer in parts, with pauses: the guest, which
/// reads it, is suspended in every pause, and each part wakes it. Every
/// byte arrives once and in order on the one connection, and the sandbox's
/// handles carry on across the suspensions: the network's counters count
/// on, the console keeps what the guest writes, the agent answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_carries_on_through_many_suspensions() {
    const PARTS: usize = 5;
    const PART: usize = 200_000;
    const PAUSE: Duration = Duration::from_millis(1500);
    let Some(dir) = guest_dir() else { return };
    let body: Vec<u8> = (0..(PARTS * PART) as u32).map(|i| (i * 31 % 253) as u8).collect();
    let digest = hex(&Sha256::digest(&body));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (paused, paused_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            let n = conn.read(&mut buf).unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
        }
        let head = format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        conn.write_all(head.as_bytes()).unwrap();
        for part in body.chunks(PART) {
            std::thread::sleep(PAUSE);
            conn.write_all(part).unwrap();
            paused.send(()).unwrap();
        }
    });
    let sb = networked(&dir, port).await;
    let network = sb.network().unwrap().clone();
    let p = sb
        .spawn(&sh(&format!(
            "wget -q -O /tmp/answer http://10.0.2.1:{port}/answer; echo rc=$?; \
             sha256sum /tmp/answer"
        )))
        .await
        .unwrap();
    let mut suspensions = 0;
    let mut to_guest = network.stats().bytes_to_guest;
    for part in 0..PARTS {
        // The part before is out; the guest takes it and waits again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if part + 1 < PARTS {
            match sb.suspend(ram_dir()).await {
                Ok(()) => suspensions += 1,
                Err(Error::Busy(why)) => eprintln!("part {part}: busy: {why}"),
                Err(e) => panic!("suspend: {e}"),
            }
        }
        let _ = sb.console_output();
        paused_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let now = network.stats().bytes_to_guest;
        assert!(now >= to_guest, "the network's counters went back");
        to_guest = now;
    }
    let (out, err, code) = output(&p).await;
    assert_eq!(code, Some(0), "{out}\n{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "rc=0", "{out}\n{err}");
    assert!(lines[1].starts_with(&digest), "{out}");
    assert!(suspensions >= PARTS - 2, "suspended in {suspensions} of {} pauses", PARTS - 1);
    assert!(network.stats().bytes_to_guest >= (PARTS * PART) as u64);
    server.join().unwrap();
    assert_eq!(sb.close().await, Ended::Closed);
}

/// The machine-level view: a suspended `Machine` keeps every handle a
/// caller holds (its stop handle, agent and vsock, exit counts, serial
/// output), reads its RAM from its file, refuses what needs its vCPUs, and
/// its memory slot handles, the freed VM's, map nothing; a snapshot of it
/// restores and suspends it again; an agent call wakes it, and it runs on.
#[test]
fn a_suspended_machine_keeps_its_handles() {
    use libmachine::{ExitReason, HostMemory, Machine, MachineConfig, RegionKind, Woken};
    let Some(dir) = guest_dir() else { return };
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=agent".into(),
    };
    let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
    let m = Machine::boot_linux(config, &boot).unwrap();
    let agent = m.agent().unwrap().timeout(Some(Duration::from_secs(60)));
    let stop = m.stop_handle();
    let old_slots = m.memory_slots();
    let window = m.memory_layout().of_kind(RegionKind::Mmio64).next().unwrap().start;
    let start = |mut m: Machine| {
        std::thread::spawn(move || {
            let exit = m.run().expect("run");
            (exit, m)
        })
    };
    let runner = start(m);
    agent.wait_ready(Duration::from_secs(60)).unwrap();
    assert_eq!(agent.exec(&sh("echo hi")).unwrap().stdout, b"hi\n");
    stop.stop();
    let (exit, mut m) = runner.join().unwrap();
    assert_eq!(exit, ExitReason::Stopped);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match m.suspend(ram_dir()) {
            Ok(()) => break,
            Err(Error::Busy(_)) if Instant::now() < deadline => {
                m.run_for(Duration::from_millis(2)).unwrap();
            }
            Err(e) => panic!("suspend: {e}"),
        }
    }
    assert!(m.suspended());
    assert_eq!(machine_threads(), [0, 0, 0, 1]);
    let counts = m.exit_counts();
    assert!(counts.total() > 0);
    let serial = m.serial_output();
    let probe = m.read_memory(0x10_0000, 64 << 10).unwrap();
    assert!(probe.iter().any(|&b| b != 0), "the kernel is there");
    let err = m.registers().unwrap_err();
    assert!(err.to_string().contains("suspended"), "{err}");
    for slots in [old_slots, m.memory_slots()] {
        let err = slots.map(window, HostMemory::new(4096).unwrap()).unwrap_err();
        assert!(err.to_string().contains("suspended"), "{err}");
    }

    // A snapshot of it: restored for it, suspended again after.
    let snap = ram_dir().join(format!("suspended-{}.snap", std::process::id()));
    m.snapshot(&snap).unwrap();
    let opened = libmachine::Snapshot::open(&snap).unwrap();
    assert_eq!(opened.config().mem_size, 256 << 20);
    std::fs::remove_file(&snap).unwrap();
    assert!(m.suspended());
    assert_eq!(machine_threads(), [0, 0, 0, 1], "suspended again");
    assert_eq!(m.read_memory(0x10_0000, 64 << 10).unwrap(), probe);

    // An agent call wakes it; the old stop handle stops it.
    let caller = {
        let agent = agent.clone();
        std::thread::spawn(move || agent.exec(&sh("echo woke")).unwrap().stdout)
    };
    assert_eq!(m.wait_woken(Some(Duration::from_secs(30))), Woken::Device);
    let runner = start(m);
    assert_eq!(caller.join().unwrap(), b"woke\n");
    stop.stop();
    let (exit, m) = runner.join().unwrap();
    assert_eq!(exit, ExitReason::Stopped);
    assert!(!m.suspended());
    assert!(m.exit_counts().total() > counts.total(), "the counts count on");
    assert!(m.serial_output().starts_with(&serial[..serial.len().min(64)]));
    let slot = m
        .memory_slots()
        .map(window, HostMemory::new(4096).unwrap())
        .expect("a new handle maps once the machine runs");
    drop(slot);
}

/// How long suspensions and wakes take, for the notes (run in a release
/// build: `cargo test --release -p libmachine --test kvm_suspend --
/// --ignored --nocapture suspension_timings`): a 256 MiB guest that wrote
/// 32 MiB, its first suspension, later ones each after a waking call, and
/// a call that wakes it against one to a guest awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement, for a release build"]
async fn suspension_timings() {
    let Some(dir) = guest_dir() else { return };
    let sb = builder(&dir).create().await.unwrap();
    stdout(&sb, "head -c 33554432 /dev/urandom > /tmp/ballast").await;
    let median = |mut v: Vec<Duration>| {
        v.sort();
        (v[v.len() / 2], v[v.len() - 1])
    };
    let mut awake = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        stdout(&sb, "true").await;
        awake.push(t.elapsed());
    }
    let t = Instant::now();
    suspend_soon(&sb).await;
    let first = t.elapsed();
    // Waking calls at once after a suspension (the freed VM may still be
    // being destroyed), and after the guest has slept a while.
    let (mut later, mut at_once, mut after) = (Vec::new(), Vec::new(), Vec::new());
    for round in 0..40 {
        if round % 2 == 1 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let t = Instant::now();
        stdout(&sb, "true").await;
        if round % 2 == 1 { &mut after } else { &mut at_once }.push(t.elapsed());
        let t = Instant::now();
        suspend_soon(&sb).await;
        later.push(t.elapsed());
    }
    let (awake, awake_max) = median(awake);
    let (later, later_max) = median(later);
    let (at_once, at_once_max) = median(at_once);
    let (after, after_max) = median(after);
    eprintln!(
        "first suspension {first:?}; later ones median {later:?} (max {later_max:?}); \
         a waking call median {at_once:?} (max {at_once_max:?}) at once, {after:?} (max \
         {after_max:?}) after 200 ms; awake {awake:?} (max {awake_max:?})"
    );
    assert_eq!(sb.close().await, Ended::Closed);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
