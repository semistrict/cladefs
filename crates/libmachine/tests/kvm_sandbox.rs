// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The async Sandbox API end to end, on the test guest
//! (`crates/libmachine/scripts/build-test-guest.sh`, booted with `libmachine_action=agent`):
//! many sandboxes in one process, cancellation, closing and a guest that
//! stops while calls wait.
//!
//! Every guest here has 256 MiB of RAM. The test with many sandboxes holds
//! `GUESTS` for writing, the others for reading, so that it never stacks
//! on top of them in the memory-capped check.
//!
//! Tests return early with a note when /dev/kvm or the guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::kvm_available;
use libmachine::agent::{Command, Event, ExitStatus};
use libmachine::sandbox::{Ended, Sandbox, SandboxBuilder};
use libmachine::{Error, ExitReason, LinuxBoot};

/// Generous bound for "promptly", so that a loaded host does not flake.
const PROMPT: Duration = Duration::from_secs(2);
/// How many sandboxes run at once in `many_sandboxes_at_once`.
const MANY: usize = 32;

/// Held across awaits, by tests on runtimes of their own: tokio's lock
/// works across runtimes.
static GUESTS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

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

/// A sandbox of the test guest, `cmdline` on its kernel command line.
fn builder_with(dir: &std::path::Path, cmdline: &str) -> SandboxBuilder {
    Sandbox::builder(LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: cmdline.into(),
    })
    .mem_size(256 << 20)
    .ready_timeout(Duration::from_secs(60))
}

fn builder(dir: &std::path::Path) -> SandboxBuilder {
    builder_with(dir, "quiet libmachine_action=agent")
}

async fn create(dir: &std::path::Path) -> Sandbox {
    builder(dir).create().await.expect("create a sandbox")
}

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    cmd
}

fn exited(code: i32) -> ExitStatus {
    ExitStatus { code: Some(code), signal: None }
}

/// This process's threads whose name starts with `prefix` (names are cut
/// to 15 bytes).
fn threads_named(prefix: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|name| name.starts_with(prefix))
        .count()
}

/// A field of /proc/self/status, its first number.
fn status(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|l| l.starts_with(field))
        .unwrap_or_else(|| panic!("no {field} in /proc/self/status"));
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Waits until `cond` holds, at most `limit`.
async fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

/// board 13101: many sandboxes in one process at once, each created, used
/// (exec, files both ways) and closed concurrently; then no sandbox thread
/// is left, so nothing leaks per sandbox or per command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_sandboxes_at_once() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _all = GUESTS.write().await;
    let host_dir = std::env::temp_dir().join(format!("libmachine-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(&host_dir).unwrap();
    let threads_before = status("Threads:");
    let start = Instant::now();
    let barrier = Arc::new(tokio::sync::Barrier::new(MANY + 1));
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..MANY {
        let (dir, host_dir, barrier) = (dir.clone(), host_dir.clone(), barrier.clone());
        tasks.spawn(async move {
            let sb = create(&dir).await;
            let out = sb.exec(&sh(&format!("echo $(({i} * {i}))"))).await.unwrap();
            assert_eq!(out.stdout, format!("{}\n", i * i).into_bytes());
            assert_eq!(out.status, exited(0));

            let data: Vec<u8> = (0..100_000u32).map(|n| (n * 7 + i as u32) as u8).collect();
            sb.write_file("/tmp/data", data.clone(), 0o600).await.unwrap();
            let out = sb.exec(Command::new("wc").args(["-c", "/tmp/data"])).await.unwrap();
            assert!(out.stdout.starts_with(b"100000 "), "{out:?}");
            assert_eq!(sb.read_file("/tmp/data").await.unwrap(), data);

            let (to, back) = (host_dir.join(format!("to{i}")), host_dir.join(format!("back{i}")));
            std::fs::write(&to, format!("sandbox {i}")).unwrap();
            sb.copy_to_guest(&to, "/tmp/copied").await.unwrap();
            sb.copy_from_guest("/tmp/copied", &back).await.unwrap();
            assert_eq!(std::fs::read(&back).unwrap(), format!("sandbox {i}").into_bytes());

            // All up at once here.
            barrier.wait().await;
            barrier.wait().await;
            // Latency of a command while all run.
            let t = Instant::now();
            for _ in 0..10 {
                sb.exec(&Command::new("true")).await.unwrap();
            }
            let per_exec = t.elapsed() / 10;
            assert_eq!(sb.close().await, Ended::Closed);
            per_exec
        });
    }
    barrier.wait().await;
    let up = start.elapsed();
    let (threads, rss) = (status("Threads:"), status("VmRSS:"));
    eprintln!(
        "{MANY} sandboxes up and used in {up:?}: {threads} threads ({threads_before} before), \
         {} MiB resident",
        rss / 1024
    );
    // Each sandbox: its vCPU, device and run threads; no output thread is
    // left once commands end. The blocking pool keeps idle threads for a
    // while; they are counted too.
    assert_eq!(threads_named("libmachine-run"), MANY);
    assert!(
        eventually(PROMPT, || threads_named("libmachine-out") == 0).await,
        "{} output threads left after their commands ended",
        threads_named("libmachine-out")
    );
    barrier.wait().await;
    let mut latencies = Vec::new();
    while let Some(per_exec) = tasks.join_next().await {
        latencies.push(per_exec.unwrap());
    }
    latencies.sort();
    eprintln!(
        "exec latency with {MANY} sandboxes running at once: median {:?}, worst {:?}",
        latencies[MANY / 2],
        latencies[MANY - 1]
    );

    // A create whose future is dropped while the guest boots leaves no
    // machine behind.
    let dropped = tokio::time::timeout(Duration::from_millis(30), create(&dir)).await;
    assert!(dropped.is_err(), "booted within 30 ms?");

    // Closed sandboxes have freed their machines and threads.
    assert!(
        eventually(PROMPT, || threads_named("libmachine-run") == 0
            && threads_named("vcpu") == 0
            && threads_named("libmachine-dev") == 0)
        .await,
        "{} run, {} vCPU threads left",
        threads_named("libmachine-run"),
        threads_named("vcpu")
    );
    std::fs::remove_dir_all(&host_dir).unwrap();
}

/// Dropping an exec's future, or a running process, kills the command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_future_kills_its_command() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let sb = create(&dir).await;
    let alive = async |pid: &str| {
        sb.exec(&sh(&format!("kill -0 {pid} 2>/dev/null"))).await.unwrap().status.success()
    };

    let cmd = sh("echo $$ > /tmp/pid; exec sleep 1000");
    let exec = sb.exec(&cmd);
    assert!(tokio::time::timeout(Duration::from_millis(500), exec).await.is_err());
    let pid = String::from_utf8(sb.read_file("/tmp/pid").await.unwrap()).unwrap();
    let pid = pid.trim().to_owned();
    assert!(pid.parse::<u32>().is_ok(), "{pid:?}");
    let deadline = Instant::now() + PROMPT;
    while alive(&pid).await {
        assert!(Instant::now() < deadline, "the dropped exec's command still runs");
    }

    let p = sb.spawn(Command::new("sleep").arg("1000")).await.unwrap();
    let pid = p.pid().to_string();
    assert!(alive(&pid).await);
    drop(p);
    let deadline = Instant::now() + PROMPT;
    while alive(&pid).await {
        assert!(Instant::now() < deadline, "the dropped process's command still runs");
    }
    assert_eq!(sb.close().await, Ended::Closed);
}

/// Closing a sandbox while a command runs fails the command's wait at
/// once, and every call after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_while_a_command_runs_fails_it() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let sb = Arc::new(create(&dir).await);
    let running = {
        let sb = sb.clone();
        tokio::spawn(async move { sb.exec(Command::new("sleep").arg("1000")).await })
    };
    let streaming = sb.spawn(&Command::new("cat")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = Instant::now();
    assert_eq!(sb.close().await, Ended::Closed);
    let err = running.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = streaming.next_event().await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    // And it stays so.
    let err = streaming.next_event().await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = streaming.write_stdin(b"x".to_vec()).await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = sb.exec(&Command::new("true")).await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    let err = sb.read_file("/etc/passwd").await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    assert!(start.elapsed() < PROMPT, "{:?}", start.elapsed());
    assert_eq!(sb.close().await, Ended::Closed, "closing again");
    assert_eq!(sb.ended(), Some(Ended::Closed));
}

/// A guest that powers off ends the sandbox: calls waiting on it fail at
/// once, and `wait` says how it ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_that_powers_off_fails_waiting_calls() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let sb = Arc::new(create(&dir).await);
    let p = Arc::new(sb.spawn(Command::new("sleep").arg("1000")).await.unwrap());
    let waiting = {
        let p = p.clone();
        tokio::spawn(async move { p.wait().await })
    };
    let reading = {
        let sb = sb.clone();
        tokio::spawn(async move { sb.exec(&sh("echo started; exec sleep 1000")).await })
    };
    assert_eq!(sb.ended(), None);
    match sb.exec(Command::new("poweroff").arg("-f")).await {
        Ok(_) | Err(Error::GuestGone(_)) => {}
        Err(e) => panic!("poweroff: {e}"),
    }
    let stopped = Instant::now();
    assert_eq!(sb.wait().await, Ended::Guest(ExitReason::Poweroff));
    let err = waiting.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    match reading.await.unwrap() {
        Err(Error::GuestGone(_)) => {}
        other => panic!("{other:?}"),
    }
    let err = sb.spawn(&Command::new("true")).await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    assert!(stopped.elapsed() < PROMPT, "{:?}", stopped.elapsed());
    // Closing a sandbox whose guest stopped says how it stopped.
    assert_eq!(sb.close().await, Ended::Guest(ExitReason::Poweroff));
}

/// A streaming process: stdin in, output out, its events read with a
/// cancel-safe await, output under backpressure, stdin through exec.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_processes() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let sb = create(&dir).await;

    let p = sb.spawn(&Command::new("cat")).await.unwrap();
    assert!(p.pid() > 1);
    p.write_stdin(b"hello\n".to_vec()).await.unwrap();
    assert_eq!(p.next_event().await.unwrap(), Some(Event::Stdout(b"hello\n".to_vec())));
    p.close_stdin().await.unwrap();
    assert_eq!(p.next_event().await.unwrap(), Some(Event::Exit(exited(0))));
    assert_eq!(p.next_event().await.unwrap(), None);

    // Reads cancelled again and again (a select with a short timer) lose
    // nothing.
    let p = sb
        .spawn(&sh("for i in 1 2 3 4 5; do echo $i; echo e$i >&2; sleep 0.05; done; exit 4"))
        .await
        .unwrap();
    let (mut stdout, mut stderr, mut exit, mut cancelled) = (Vec::new(), Vec::new(), None, 0);
    while exit.is_none() {
        tokio::select! {
            event = p.next_event() => match event.unwrap().unwrap() {
                Event::Stdout(d) => stdout.extend(d),
                Event::Stderr(d) => stderr.extend(d),
                Event::Exit(status) => exit = Some(status),
            },
            () = tokio::time::sleep(Duration::from_millis(5)) => cancelled += 1,
        }
    }
    assert_eq!(stdout, b"1\n2\n3\n4\n5\n");
    assert_eq!(stderr, b"e1\ne2\ne3\ne4\ne5\n");
    assert_eq!(exit, Some(exited(4)));
    assert!(cancelled > 10, "only {cancelled} reads were cancelled");

    // More output than every queue on the way holds.
    let out = sb.exec(&sh("head -c 5000000 /dev/zero")).await.unwrap();
    assert_eq!(out.stdout.len(), 5_000_000);
    // Stdin through exec, more than one chunk.
    let out = sb.exec(Command::new("wc").arg("-c").stdin(vec![b'x'; 300_000])).await.unwrap();
    assert_eq!(out.stdout.trim_ascii(), b"300000");
    // A signal.
    let p = sb.spawn(Command::new("sleep").arg("1000")).await.unwrap();
    p.signal(15).await.unwrap();
    let out = p.wait().await.unwrap();
    assert_eq!(out.status.signal, Some(15));
    // Errors in the guest are the guest's.
    let err = sb.exec(&Command::new("/no/such/program")).await.unwrap_err();
    assert!(
        matches!(&err, Error::Guest { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
        "{err}"
    );
    let err = sb.read_file("/no/such/file").await.unwrap_err();
    assert!(matches!(err, Error::Guest { .. }), "{err}");
    assert!(!sb.console_output().is_empty());
    sb.close().await;
}

/// Dropping a sandbox stops its guest and frees its machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_sandbox_frees_its_machine() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let sb = create(&dir).await;
    let vsock = sb.vsock().clone();
    let agent = sb.agent().clone();
    agent.ping().unwrap();
    drop(sb);
    // NotConnected: the machine is gone, not merely stopped.
    let freed = eventually(PROMPT, || {
        vsock
            .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(1))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotConnected)
    })
    .await;
    assert!(freed, "the dropped sandbox's machine is still there");
    assert!(matches!(agent.ping(), Err(Error::GuestGone(_))));
}

/// A guest that stops while it boots fails `create` at once, saying how
/// it stopped and quoting its console; a kernel that is not there fails
/// before any machine runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_boot_fails_create_at_once() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    // Without libmachine_action=agent, the test init powers off.
    let start = Instant::now();
    let err = builder_with(&dir, "libmachine_action=poweroff").create().await.unwrap_err();
    let Error::GuestGone(message) = &err else {
        panic!("{err}");
    };
    assert!(message.contains("Poweroff"), "{message}");
    let console = message.split("console ended with:\n").nth(1).unwrap_or_default();
    assert!(!console.trim().is_empty(), "no console in: {message}");
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());

    let err = Sandbox::builder(LinuxBoot {
        kernel: dir.join("no-such-kernel"),
        initrd: None,
        cmdline: String::new(),
    })
    .create()
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Io { .. } | Error::Boot(_)), "{err}");
}

/// A directory of a test's own for snapshot files, removed when dropped.
struct SnapshotDir(PathBuf);

impl SnapshotDir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/snapshot-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for SnapshotDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_of(path: &std::path::Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize().to_vec()
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

async fn stdout(sb: &Sandbox, script: &str) -> String {
    let out = sb.exec(&sh(script)).await.unwrap_or_else(|e| panic!("{script}: {e}"));
    assert_eq!(out.status, exited(0), "{script}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// How many clones `sandboxes_from_a_snapshot_are_clones_of_their_template`
/// restores at once.
const CLONES: usize = 8;

/// board 13167: a template sandbox, booted and prepared, is snapshotted while
/// it runs on; sandboxes created from the snapshot at once each run
/// commands on their own copy of the template's guest (its file there,
/// their writes their own, the snapshot file unchanged), with their own
/// random numbers, the right time and the VM generation ID changed; and a
/// sandbox from a snapshot is created much faster than one that boots.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandboxes_from_a_snapshot_are_clones_of_their_template() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _all = GUESTS.write().await;
    let files = SnapshotDir::new("sandbox-clones");
    let path = files.file("template");

    let start = Instant::now();
    let template = create(&dir).await;
    let booted = start.elapsed();
    template.write_file("/tmp/marker", b"template\n".to_vec(), 0o644).await.unwrap();
    let template_random = stdout(&template, "head -c 16 /dev/urandom | od -An -tx1").await;
    let start = Instant::now();
    let snapshot = Arc::new(template.snapshot(&path).await.expect("snapshot"));
    let snapshotted = start.elapsed();
    assert_eq!(snapshot.config().mem_size, 256 << 20);
    // The template runs on, and can be snapshotted again.
    assert_eq!(stdout(&template, "cat /tmp/marker").await, "template\n");
    assert_eq!(template.ended(), None);
    assert_eq!(template.close().await, Ended::Closed);
    let before = sha256_of(&path);
    // Long enough that a clock left at the snapshot's time shows.
    tokio::time::sleep(Duration::from_secs(2)).await;
    // One alone, against the boot.
    let start = Instant::now();
    let alone =
        Sandbox::from_snapshot(snapshot.clone()).create().await.expect("create from the snapshot");
    let restored = start.elapsed();
    assert_eq!(stdout(&alone, "cat /tmp/marker").await, "template\n");
    assert_eq!(alone.close().await, Ended::Closed);
    assert!(
        restored < booted,
        "creating from the snapshot ({restored:?}) was not faster than booting ({booted:?})"
    );

    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..CLONES {
        let snapshot = snapshot.clone();
        tasks.spawn(async move {
            let start = Instant::now();
            let sb = Sandbox::from_snapshot(snapshot)
                .ready_timeout(Duration::from_secs(60))
                .create()
                .await
                .expect("create from the snapshot");
            let created = start.elapsed();
            assert_eq!(stdout(&sb, "cat /tmp/marker").await, "template\n");
            sb.write_file("/tmp/marker", format!("clone {i}\n").into_bytes(), 0o644).await.unwrap();
            let random = stdout(&sb, "head -c 16 /dev/urandom | od -An -tx1").await;
            let host_before = now_secs();
            let guest_time: f64 = stdout(&sb, "date +%s").await.trim().parse().unwrap();
            let host_after = now_secs();
            assert!(
                guest_time >= host_before.floor() - 1.0 && guest_time <= host_after + 1.0,
                "clone {i}: the guest says {guest_time}, the host {host_before}..{host_after}"
            );
            (i, sb, created, random)
        });
    }
    let mut clones = Vec::new();
    while let Some(clone) = tasks.join_next().await {
        clones.push(clone.unwrap());
    }
    clones.sort_by_key(|c| c.0);
    for (i, sb, _, random) in &clones {
        assert_eq!(
            stdout(sb, "cat /tmp/marker").await,
            format!("clone {i}\n"),
            "each clone sees only its own write"
        );
        assert_ne!(*random, template_random, "clone {i} drew the template's numbers");
        let log = stdout(sb, "dmesg").await;
        assert!(
            log.contains("crng reseeded due to virtual machine fork"),
            "clone {i}: no VMGENID reseed in its log:\n{log}"
        );
    }
    for (i, a) in clones.iter().enumerate() {
        for b in &clones[i + 1..] {
            assert_ne!(a.3, b.3, "two clones drew the same random bytes");
        }
    }
    let mut created: Vec<Duration> = clones.iter().map(|c| c.2).collect();
    created.sort();
    eprintln!(
        "a sandbox booted in {booted:?}; snapshotted in {snapshotted:?}; one created from the \
         snapshot in {restored:?}, {CLONES} at once in median {:?} (min {:?}, max {:?}) each, \
         ready (agent answered, RNG reseeded)",
        created[CLONES / 2],
        created[0],
        created[CLONES - 1]
    );
    for (_, sb, _, _) in &clones {
        assert_eq!(sb.close().await, Ended::Closed);
    }
    drop(clones);
    assert_eq!(sha256_of(&path), before, "the snapshot file is unchanged");
}

/// Snapshots of a sandbox: two asked for at once are both taken and the
/// sandbox runs on; one asked of a closed sandbox, or racing its close,
/// fails with GuestGone rather than hanging; a networked sandbox is
/// refused and runs on; a sandbox from a snapshot with another machine
/// (vCPU count) is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_of_a_sandbox_and_their_refusals() {
    let Some(dir) = guest_dir() else {
        return;
    };
    let _one = GUESTS.read().await;
    let files = SnapshotDir::new("sandbox-snapshots");
    let sb = create(&dir).await;
    let (a, b) = tokio::join!(sb.snapshot(files.file("a")), sb.snapshot(files.file("b")));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.config().vcpus, b.config().vcpus);
    assert_eq!(stdout(&sb, "echo on").await, "on\n");
    assert_eq!(sb.ended(), None, "a snapshot does not end the sandbox");

    // Another machine than the snapshot's is refused.
    let err = Sandbox::from_snapshot(Arc::new(a)).vcpus(2).create().await.unwrap_err();
    assert!(matches!(err, Error::Snapshot(_)), "{err}");

    // Racing a close: taken, or GuestGone; never stuck.
    let (taken, ended) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(sb.snapshot(files.file("c")), sb.close())
    })
    .await
    .expect("a snapshot racing a close hung");
    assert_eq!(ended, Ended::Closed);
    assert!(matches!(taken, Ok(_) | Err(Error::GuestGone(_))), "{:?}", taken.err());
    let err = sb.snapshot(files.file("d")).await.unwrap_err();
    assert!(matches!(err, Error::GuestGone(_)), "{err}");
    drop(b);

    let networked = builder(&dir)
        .network(libmachine::net::NetworkConfig::new(libmachine::net::EgressPolicy::deny_all()))
        .create()
        .await
        .unwrap();
    let err = networked.snapshot(files.file("net")).await.unwrap_err();
    assert!(matches!(&err, Error::Snapshot(m) if m.contains("network")), "{err}");
    assert_eq!(stdout(&networked, "echo on").await, "on\n");
    assert!(!files.file("net").exists());
}
