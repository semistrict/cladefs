// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Restores onto the vsock's host end, end to end (board 13211): the test guest
//! (`crates/libmachine/scripts/build-test-guest.sh`) boots with its agent, streams run both
//! ways through the agent and through a raw [`VsockStream`], and the machine
//! is snapshotted, dropped and restored onto the host end it had, its
//! streams carrying on; a plain restore still resets them.
//!
//! In the crate rather than in `tests/` because `Machine::restore_with`,
//! `Vsock::detach` and the agent's reconnect count are internal. Skips with
//! a note when /dev/kvm or the guest is missing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::agent::{Agent, Command, Event, Process};
use crate::{ExitReason, LinuxBoot, Machine, MachineConfig, Snapshot, StopHandle};

use super::VsockStream;

const MEM: usize = 256 << 20;
/// Bytes each stream carries.
const LEN: usize = 8 << 20;
/// Readers stop here until the restore, so that every stream's buffers are
/// full (data in flight, the writer waiting) when the snapshot is taken.
/// The buffers between the ends hold about 2 MiB (the agent client keeps
/// the last 1 MiB of stdin, pipes and socket buffers the rest): a writer
/// with less than that left would be done before the snapshot.
const HALF: usize = 1 << 20;
const CHUNK: usize = 64 << 10;
/// The host port the guest's `libmachine-agent connect` reaches.
const PORT: u32 = 5000;
/// Generous bound for anything one stream waits on.
const PATIENCE: Duration = Duration::from_secs(60);

fn guest() -> Option<PathBuf> {
    let kvm = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm");
    if kvm.is_err() {
        eprintln!("skipping: /dev/kvm is not accessible");
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

/// A directory of the test's own for its snapshot, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/vsock-restore-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config() -> MachineConfig {
    MachineConfig { mem_size: MEM, ..Default::default() }
}

fn boot(dir: &Path) -> Machine {
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=agent".into(),
    };
    Machine::boot_linux(config(), &boot).expect("boot")
}

/// A machine running on a thread of its own.
struct Running {
    stop: StopHandle,
    runner: Option<JoinHandle<(ExitReason, Machine)>>,
}

impl Running {
    fn start(mut m: Machine) -> Self {
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        Self { stop, runner: Some(runner) }
    }

    /// Stops the run between two exits: the machine.
    fn stop(mut self) -> Machine {
        self.stop.stop();
        let (exit, m) = self.runner.take().unwrap().join().expect("runner");
        assert_eq!(exit, ExitReason::Stopped, "{}", console(&m));
        m
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(runner) = self.runner.take() {
            self.stop.stop();
            let _ = runner.join();
        }
    }
}

fn console(m: &Machine) -> String {
    String::from_utf8_lossy(&m.serial_output()).into_owned()
}

fn agent_of(m: &Machine) -> Agent {
    m.agent().expect("a PC machine").timeout(Some(PATIENCE))
}

/// Byte `i` of stream `seed`: a sequence in which a byte lost, doubled or
/// moved shows.
fn pattern(seed: u64, i: usize) -> u8 {
    let x = (i as u64 ^ seed << 40).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (x >> 56) as u8
}

fn chunk(seed: u64, from: usize, len: usize) -> Vec<u8> {
    (from..from + len).map(|i| pattern(seed, i)).collect()
}

/// Where a stream's reader is, and the gate that holds readers at `HALF`.
#[derive(Default)]
struct Progress {
    read: AtomicUsize,
    written: AtomicUsize,
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

/// Checks `data`, the next bytes of stream `seed` at `progress.read`, and
/// waits at the gate on reaching `HALF`.
fn take(seed: u64, data: &[u8], progress: &Progress, gate: &Gate) {
    let at = progress.read.load(Ordering::SeqCst);
    assert!(at + data.len() <= LEN, "stream {seed}: more than was sent");
    for (k, &b) in data.iter().enumerate() {
        assert_eq!(b, pattern(seed, at + k), "stream {seed}: byte {}", at + k);
    }
    progress.read.store(at + data.len(), Ordering::SeqCst);
    if at < HALF && at + data.len() >= HALF {
        gate.wait();
    }
}

/// Writes stream `seed` into the agent process's stdin, then closes it.
fn feed_stdin(p: Arc<Process>, seed: u64, progress: Arc<Progress>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for from in (0..LEN).step_by(CHUNK) {
            p.write_stdin(&chunk(seed, from, CHUNK))
                .unwrap_or_else(|e| panic!("stream {seed}: stdin at {from}: {e}"));
            progress.written.store(from + CHUNK, Ordering::SeqCst);
        }
        p.close_stdin().unwrap();
    })
}

/// Reads the agent process's stdout as stream `seed`, to its exit.
fn drain_stdout(
    p: Arc<Process>,
    seed: u64,
    progress: Arc<Progress>,
    gate: Arc<Gate>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stderr = Vec::new();
        loop {
            match p.next_event() {
                Ok(Some(Event::Stdout(data))) => take(seed, &data, &progress, &gate),
                Ok(Some(Event::Stderr(data))) => stderr.extend(data),
                Ok(Some(Event::Exit(status))) => {
                    assert!(
                        status.success(),
                        "stream {seed}: {status:?}, stderr {}",
                        String::from_utf8_lossy(&stderr)
                    );
                }
                Ok(None) => break,
                Err(e) => panic!("stream {seed}: stdout: {e}"),
            }
        }
        assert_eq!(progress.read.load(Ordering::SeqCst), LEN, "stream {seed}");
    })
}

/// Writes stream `seed` into the raw stream, then shuts its writing down.
fn feed_stream(s: Arc<VsockStream>, seed: u64, progress: Arc<Progress>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for from in (0..LEN).step_by(CHUNK) {
            (&*s)
                .write_all(&chunk(seed, from, CHUNK))
                .unwrap_or_else(|e| panic!("stream {seed}: write at {from}: {e}"));
            progress.written.store(from + CHUNK, Ordering::SeqCst);
        }
        s.shutdown(std::net::Shutdown::Write).unwrap();
    })
}

/// Reads the raw stream as stream `seed`, to its end.
fn drain_stream(
    s: Arc<VsockStream>,
    seed: u64,
    progress: Arc<Progress>,
    gate: Arc<Gate>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0; CHUNK];
        loop {
            // Never past `HALF` before the gate opens.
            let at = progress.read.load(Ordering::SeqCst);
            let room = if at < HALF { (HALF - at).min(CHUNK) } else { CHUNK };
            match (&*s).read(&mut buf[..room]) {
                Ok(0) => break,
                Ok(n) => take(seed, &buf[..n], &progress, &gate),
                Err(e) => panic!("stream {seed}: read at {at}: {e}"),
            }
        }
        assert_eq!(progress.read.load(Ordering::SeqCst), LEN, "stream {seed}");
    })
}

/// Waits until `done` holds, at most `PATIENCE`.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The machine is snapshotted with its host end detached, dropped, and
/// restored onto that host end, while four streams are in flight: the
/// guest's `libmachine-agent connect` (an agent process) copies its stdin
/// to a raw `VsockStream` the host accepted and that stream back to its
/// stdout, and `cat` echoes its stdin. Readers wait at 1 MiB while writers
/// fill every buffer between them, so bytes sit in the host end, the
/// device and the guest when the snapshot is taken. After the restore,
/// every byte arrives once, in order, the raw stream is never reset and
/// the agent's processes never reconnect.
#[test]
fn streams_carry_on_through_a_restore_onto_their_host_end() {
    let Some(dir) = guest() else { return };
    let files = Dir::new("carry-on");
    let m = boot(&dir);
    let vsock = m.vsock().unwrap();
    let agent = agent_of(&m);
    let running = Running::start(m);
    agent.wait_ready(PATIENCE).expect("agent ready");

    let listener = vsock.listen(PORT).unwrap();
    let connect = Arc::new(
        agent.spawn(Command::new("libmachine-agent").args(["connect", &PORT.to_string()])).unwrap(),
    );
    let s = Arc::new(listener.accept(Some(PATIENCE)).unwrap());
    s.set_read_timeout(Some(PATIENCE)).unwrap();
    s.set_write_timeout(Some(PATIENCE)).unwrap();
    let cat = Arc::new(agent.spawn(&Command::new("cat")).unwrap());

    let gate = Arc::new(Gate::default());
    // 1: stdin of connect to the raw stream; 2: the raw stream to connect's
    // stdout; 3: cat.
    let progress: Vec<Arc<Progress>> = (0..3).map(|_| Arc::default()).collect();
    let threads = vec![
        feed_stdin(connect.clone(), 1, progress[0].clone()),
        drain_stream(s.clone(), 1, progress[0].clone(), gate.clone()),
        feed_stream(s.clone(), 2, progress[1].clone()),
        drain_stdout(connect.clone(), 2, progress[1].clone(), gate.clone()),
        feed_stdin(cat.clone(), 3, progress[2].clone()),
        drain_stdout(cat.clone(), 3, progress[2].clone(), gate.clone()),
    ];
    wait_until("every reader at the gate", || {
        progress.iter().all(|p| p.read.load(Ordering::SeqCst) >= HALF)
    });
    // Let the writers fill what they can.
    std::thread::sleep(Duration::from_millis(300));

    let mut m = running.stop();
    for (i, p) in progress.iter().enumerate() {
        let (read, written) = (p.read.load(Ordering::SeqCst), p.written.load(Ordering::SeqCst));
        eprintln!("stream {}: {read} read, {written} written", i + 1);
        assert!(written < LEN, "stream {}: all written before the snapshot", i + 1);
        assert!(written > read, "stream {}: nothing in flight", i + 1);
    }
    vsock.detach();
    let path = files.file("machine");
    m.snapshot(&path).expect("snapshot");
    drop(m);
    let snapshot = Snapshot::open(&path).unwrap();
    let restored = Machine::restore_with(&snapshot, snapshot.config(), Some(vsock.clone()))
        .expect("restore onto the host end");
    assert!(Arc::ptr_eq(restored.vsock().unwrap().shared(), vsock.shared()));
    let running = Running::start(restored);
    gate.open();
    for t in threads {
        t.join().expect("a stream failed");
    }
    assert_eq!(connect.reconnects(), 0, "connect's link was reset");
    assert_eq!(cat.reconnects(), 0, "cat's link was reset");
    assert_eq!(vsock.generation(), 0, "the same machine, not a clone");
    // New connections work too.
    let out = agent.exec(Command::new("echo").arg("after")).unwrap();
    assert_eq!(out.stdout, b"after\n");
    drop(running.stop());
}

/// A plain restore makes a new host end and resets the connections the
/// guest had (board 13211's other half, as before): the clone's guest drops its
/// connection to the host, so its `libmachine-agent connect`, whose stdin
/// is done, ends; and the old host end, its machine dropped, is gone.
#[test]
fn a_plain_restore_still_resets_the_guests_connections() {
    let Some(dir) = guest() else { return };
    let files = Dir::new("plain");
    let m = boot(&dir);
    let vsock = m.vsock().unwrap();
    let agent = agent_of(&m);
    let running = Running::start(m);
    agent.wait_ready(PATIENCE).expect("agent ready");
    let listener = vsock.listen(PORT).unwrap();
    let connect =
        agent.spawn(Command::new("libmachine-agent").args(["connect", &PORT.to_string()])).unwrap();
    connect.close_stdin().unwrap();
    let s = listener.accept(Some(PATIENCE)).unwrap();
    s.set_read_timeout(Some(PATIENCE)).unwrap();
    // Its stdin is done; it copies what the host sends to its stdout.
    let mut rest = Vec::new();
    (&s).read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    // A line: its stdout is line-buffered.
    (&s).write_all(b"before\n").unwrap();
    match connect.next_event().unwrap() {
        Some(Event::Stdout(data)) => assert_eq!(data, b"before\n"),
        other => panic!("{other:?}"),
    }
    let pid = connect.pid();

    let mut m = running.stop();
    let path = files.file("machine");
    m.snapshot(&path).expect("snapshot");
    drop(m);
    let e = (&s).write(b"x").unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotConnected, "{e}");
    drop((connect, s, listener));

    let snapshot = Snapshot::open(&path).unwrap();
    let clone = Machine::restore(&snapshot, snapshot.config()).expect("restore");
    let clone_vsock = clone.vsock().unwrap();
    assert!(!Arc::ptr_eq(clone_vsock.shared(), vsock.shared()));
    assert_ne!(clone_vsock.generation(), 0, "a clone");
    let clone_agent = agent_of(&clone);
    let running = Running::start(clone);
    clone_agent.wait_ready(PATIENCE).expect("the clone's agent");
    // The guest's connect sees its connection reset and ends.
    let check = format!("test ! -e /proc/{pid} || grep -q '^State:.*Z' /proc/{pid}/status");
    wait_until("the clone's connect to end", || {
        clone_agent.exec(Command::new("sh").args(["-c", &check])).unwrap().status.success()
    });
    drop(running.stop());
}
