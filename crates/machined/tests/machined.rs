// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined end to end, driven over gRPC as a VM manager drives it: a machine made, booted with
//! cladefs's guest kernel and libmachine's test initramfs, a command run in it with its stdin and
//! output streamed, files written and read, its console streamed, its vsock reached through its
//! socket, then stopped (its filesystems synced first) and deleted; snapshots, restores and
//! suspensions on KVM; what machined refuses; and TCP's mutual TLS.
//!
//! The guest: on Linux, `CLADEFS_GUEST_KERNEL` (`scripts/guest-kernel.sh`) and the test initramfs
//! in `LIBMACHINE_GUEST_DIR` (else target/guest; `crates/libmachine/scripts/build-test-guest.sh`),
//! on /dev/kvm; on macOS, cladefs's arm64 Image and the test initramfs in target/arm64-guest
//! (`crates/libmachine/scripts/arm64-guest.sh`), or where `CLADEFS_GUEST_KERNEL` and
//! `LIBMACHINE_GUEST_DIR` say. A test that needs one and lacks it returns with a "skipping:" note,
//! which scripts/test.sh's libmachine and hvf parts fail on.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod common;

use common::{fork, guest, hypervisor, import_root};
use machined::client::{MachinedClient, connect_unix};
use machined::proto::{self, exec_request, exec_response};
use tokio_stream::StreamExt as _;
use tonic::Code;
use tonic::transport::Channel;

/// What the test initramfs's init prints once it runs.
const INIT_MARKER: &str = "libmachine-init: up ";
/// How long a test waits for a machine to reach a state.
const STATE_WAIT: Duration = Duration::from_secs(60);

/// A machined of the test's, serving on a Unix socket, its images directory holding `images`.
struct Daemon {
    server: Option<machined::Server>,
    client: MachinedClient<Channel>,
    /// The cell its machines' roots are forked from, if it boots the guest.
    base: Option<proto::Cell>,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// A machined booting `kernel`, its machines' roots forked from a cell of the test
    /// initramfs's files.
    async fn start_guest(kernel: &Path, initramfs: &Path) -> Self {
        let mut daemon = Self::start(&[("kernel", kernel)]).await;
        daemon.base = Some(import_root(&daemon.socket, initramfs, daemon._dir.path()).await);
        daemon
    }

    /// A machine booting the guest's kernel from a root of its own, forked from the base, its
    /// init the test initramfs's, `command_line` after.
    async fn spec(&self, command_line: &str) -> proto::MachineSpec {
        let root =
            fork(&self.socket, self.base.as_ref().expect("a daemon booting the guest")).await;
        proto::MachineSpec {
            vcpus: 2,
            memory_bytes: 512 << 20,
            boot: Some(proto::Boot {
                kernel: "kernel".into(),
                command_line: format!("init=/init {command_line}"),
                ..Default::default()
            }),
            root: Some(root),
            ..Default::default()
        }
    }

    async fn start(images: &[(&str, &Path)]) -> Self {
        Self::start_with(images, |_| {}).await
    }

    async fn start_with(
        images: &[(&str, &Path)],
        configure: impl FnOnce(&mut machined::Config),
    ) -> Self {
        // Short, as vsock sockets' paths must be, and on a disk on Linux, as suspensions need.
        let parent = if cfg!(target_os = "linux") {
            PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        } else {
            std::env::temp_dir()
        };
        let dir = tempfile::tempdir_in(parent).unwrap();
        let image_dir = dir.path().join("images");
        std::fs::create_dir(&image_dir).unwrap();
        for (name, from) in images {
            std::fs::copy(from, image_dir.join(name)).unwrap();
        }
        let mut config = machined::Config {
            cladefs: common::cladefs(&dir.path().join("state")),
            socket: Some(dir.path().join("machined.sock")),
            tcp: None,
            images: image_dir,
            helper: env!("CARGO_BIN_EXE_machined-vm").into(),
            state: dir.path().join("state"),
            hotbed: None,
        };
        configure(&mut config);
        let server = machined::start(config).await.unwrap();
        let socket = server.socket().unwrap().to_owned();
        let client = connect_unix(&socket).await.unwrap();
        Self { server: Some(server), client, base: None, socket, _dir: dir }
    }

    async fn shutdown(mut self) {
        self.server.take().unwrap().shutdown().await.unwrap();
    }
}

async fn create(
    client: &mut MachinedClient<Channel>,
    name: &str,
    spec: proto::MachineSpec,
) -> proto::Machine {
    let request = proto::CreateMachineRequest { name: name.into(), spec: Some(spec) };
    client.create_machine(request).await.unwrap().into_inner().machine.unwrap()
}

fn state_of(machine: &proto::Machine) -> proto::MachineState {
    machine.status.as_ref().unwrap().state()
}

/// The next status a watch says whose state is `state`.
async fn wait_for_state(
    watch: &mut tonic::Streaming<proto::WatchMachineResponse>,
    state: proto::MachineState,
) -> proto::MachineStatus {
    tokio::time::timeout(STATE_WAIT, async {
        loop {
            let status = watch.message().await.unwrap().expect("the watch ended").status.unwrap();
            if status.state() == state {
                return status;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {state:?} within {STATE_WAIT:?}"))
}

/// Runs `argv` in machine `name`, feeding it `stdin` (closing it after) if given: its stdout,
/// stderr and exit.
async fn exec(
    client: &mut MachinedClient<Channel>,
    name: &str,
    argv: &[&str],
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>, proto::exec_exit::Status), tonic::Status> {
    let start = proto::ExecStart {
        name: name.into(),
        argv: argv.iter().map(|a| a.to_string()).collect(),
        stdin: stdin.is_some(),
        ..Default::default()
    };
    let mut requests = vec![exec_request::Request::Start(start)];
    if let Some(data) = stdin {
        requests.push(exec_request::Request::Stdin(data.to_vec()));
        requests.push(exec_request::Request::CloseStdin(proto::ExecCloseStdin {}));
    }
    let requests =
        tokio_stream::iter(requests.into_iter().map(|r| proto::ExecRequest { request: Some(r) }));
    let mut answers = client.exec(requests).await?.into_inner();
    let first = answers.message().await?.and_then(|a| a.response);
    let Some(exec_response::Response::Started(started)) = first else {
        panic!("an exec's first answer is its start: {first:?}");
    };
    assert_ne!(started.pid, 0);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(answer) = answers.message().await? {
        match answer.response.unwrap() {
            exec_response::Response::Stdout(d) => stdout.extend(d),
            exec_response::Response::Stderr(d) => stderr.extend(d),
            exec_response::Response::Exit(exit) => {
                assert!(answers.message().await?.is_none(), "the exit is the last answer");
                return Ok((stdout, stderr, exit.status.unwrap()));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("the exec ended with no exit");
}

/// What the guest wrote to its console so far.
async fn console(client: &mut MachinedClient<Channel>, name: &str) -> String {
    let request = proto::StreamConsoleRequest { name: name.into(), offset: 0, follow: false };
    let mut answers = client.stream_console(request).await.unwrap().into_inner();
    let mut text = Vec::new();
    let mut next = None;
    while let Some(answer) = answers.next().await {
        let answer = answer.unwrap();
        if let Some(next) = next {
            assert_eq!(answer.offset, next, "answers follow one another");
        }
        next = Some(answer.offset + answer.data.len() as u64);
        text.extend(answer.data);
    }
    String::from_utf8_lossy(&text).replace("\r\n", "\n")
}

/// A machine boots from a cell (board 13992): its root is Fxfs on its extent store device, from
/// a cell of machined's embedded daemon, its own (forked from a base the daemon imported); what
/// its guest writes there is the cell's, there again when it starts again, and no other machine's.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_boots_from_its_cell() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    for name in ["one", "two"] {
        create(&mut client, name, daemon.spec("quiet libmachine_action=agent").await).await;
        client.start_machine(proto::StartMachineRequest { name: name.into() }).await.unwrap();
    }
    let (mounts, _, _) = exec(&mut client, "one", &["cat", "/proc/mounts"], None).await.unwrap();
    let mounts = String::from_utf8(mounts).unwrap();
    assert!(mounts.lines().any(|l| l.starts_with("root / cladefs rw")), "{mounts}");
    let write = ["sh", "-c", "echo kept > /kept && sync"];
    let (_, _, exit) = exec(&mut client, "one", &write, None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Code(0));
    let (_, _, exit) = exec(&mut client, "two", &["test", "-e", "/kept"], None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Code(1), "the other machine's root is its own");

    let stop = proto::StopMachineRequest {
        name: "one".into(),
        sync_filesystems: true,
        ..Default::default()
    };
    client.stop_machine(stop).await.unwrap();
    client.start_machine(proto::StartMachineRequest { name: "one".into() }).await.unwrap();
    let (kept, _, exit) = exec(&mut client, "one", &["cat", "/kept"], None).await.unwrap();
    assert_eq!((kept, exit), (b"kept\n".to_vec(), proto::exec_exit::Status::Code(0)));

    // A cell one machine has open, another can't boot from.
    let one = client.get_machine(proto::GetMachineRequest { name: "one".into() }).await;
    let root = one.unwrap().into_inner().machine.unwrap().spec.unwrap().root;
    let twin = proto::MachineSpec { root, ..daemon.spec("quiet libmachine_action=agent").await };
    create(&mut client, "twin", twin).await;
    let started = client.start_machine(proto::StartMachineRequest { name: "twin".into() }).await;
    let refused = started.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    daemon.shutdown().await;
}

/// A machine's balloon (board 13490): what its guest frees goes back to the host by itself
/// (free page reporting), its helper's memory falling with it; and a target the host sets, which
/// the guest's driver inflates to and deflates from.
#[tokio::test(flavor = "multi_thread")]
async fn a_balloon_gives_memory_back() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    let spec = proto::MachineSpec {
        memory_bytes: 1 << 30,
        balloon: true,
        ..daemon.spec("quiet libmachine_action=agent").await
    };
    create(&mut client, "b", spec).await;
    let started = client.start_machine(proto::StartMachineRequest { name: "b".into() }).await;
    let pid = started.unwrap().into_inner().machine.unwrap().pid;
    let balloon =
        |target: Option<u64>| proto::BalloonRequest { name: "b".into(), target_bytes: target };

    // 512 MiB used, then freed.
    let fill = "mount -t tmpfs -o size=600m none /tmp && dd if=/dev/urandom of=/tmp/f bs=1M count=512 2>/dev/null && sync";
    let (_, err, exit) = exec(&mut client, "b", &["sh", "-c", fill], None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Code(0), "{}", String::from_utf8_lossy(&err));
    let full = common::footprint(pid);
    assert!(full > 512 << 20, "the helper holds the guest's 512 MiB: {full}");
    let (_, _, exit) = exec(&mut client, "b", &["rm", "/tmp/f"], None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Code(0));
    let back = tokio::time::timeout(STATE_WAIT, async {
        loop {
            let now = common::footprint(pid);
            if now + (384 << 20) < full {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    let back = match back {
        Ok(back) => back,
        Err(_) => {
            let now = client.balloon(balloon(None)).await.unwrap().into_inner().balloon.unwrap();
            let (dmesg, _, _) = exec(&mut client, "b", &["sh", "-c", "dmesg | grep -i -e balloon -e reporting; ls /sys/bus/virtio/drivers/; cat /sys/bus/virtio/devices/*/features"], None).await.unwrap();
            panic!(
                "the helper still holds {} of {full} bytes; {now:?}; guest: {}",
                common::footprint(pid),
                String::from_utf8_lossy(&dmesg)
            );
        }
    };
    let reported = client.balloon(balloon(None)).await.unwrap().into_inner().balloon.unwrap();
    assert!(reported.discarded_bytes >= 384 << 20, "{reported:?}, {back} of {full}");
    assert_eq!((reported.target_bytes, reported.actual_bytes), (0, 0));

    // Inflated to a target, then deflated.
    let target = 256 << 20;
    let set = client.balloon(balloon(Some(target))).await.unwrap().into_inner().balloon.unwrap();
    assert_eq!(set.target_bytes, target);
    let inflated = tokio::time::timeout(STATE_WAIT, async {
        loop {
            let now = client.balloon(balloon(None)).await.unwrap().into_inner().balloon.unwrap();
            if now.actual_bytes == target {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the guest inflated its balloon to the target");
    assert!(inflated.discarded_bytes > reported.discarded_bytes, "{inflated:?}");
    client.balloon(balloon(Some(0))).await.unwrap();
    tokio::time::timeout(STATE_WAIT, async {
        while client
            .balloon(balloon(None))
            .await
            .unwrap()
            .into_inner()
            .balloon
            .unwrap()
            .actual_bytes
            != 0
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the guest deflated its balloon");
    let (_, _, exit) = exec(&mut client, "b", &["true"], None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Code(0), "the guest runs on");

    // A machine without one has none.
    create(&mut client, "plain", daemon.spec("quiet libmachine_action=agent").await).await;
    client.start_machine(proto::StartMachineRequest { name: "plain".into() }).await.unwrap();
    let none =
        client.balloon(proto::BalloonRequest { name: "plain".into(), target_bytes: None }).await;
    assert_eq!(none.unwrap_err().code(), Code::FailedPrecondition);
    daemon.shutdown().await;
}

/// Each machine runs in a helper process of its own (board 13460): several at once, on
/// Hypervisor.framework too; one whose helper dies ends with that said, while machined and the
/// others run on.
#[tokio::test(flavor = "multi_thread")]
async fn machines_run_in_helpers_of_their_own() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    let mut pids = Vec::new();
    for name in ["a", "b", "c"] {
        create(&mut client, name, daemon.spec("quiet libmachine_action=agent").await).await;
        let started = client.start_machine(proto::StartMachineRequest { name: name.into() }).await;
        let started = started.unwrap().into_inner().machine.unwrap();
        assert_eq!(state_of(&started), proto::MachineState::Running);
        assert_ne!(started.pid, 0);
        assert_ne!(started.pid, std::process::id(), "not machined's own process");
        pids.push(started.pid);
    }
    pids.dedup();
    assert_eq!(pids.len(), 3, "a process each: {pids:?}");
    for name in ["a", "b", "c"] {
        let (stdout, _, exit) = exec(&mut client, name, &["echo", name], None).await.unwrap();
        assert_eq!(
            (String::from_utf8_lossy(&stdout).into_owned(), exit),
            (format!("{name}\n"), proto::exec_exit::Status::Code(0))
        );
    }

    let watch = proto::WatchMachineRequest { name: "b".into() };
    let mut watch = client.watch_machine(watch).await.unwrap().into_inner();
    wait_for_state(&mut watch, proto::MachineState::Running).await;
    // SAFETY: a plain system call, on the test's own machine's helper.
    assert_eq!(unsafe { libc::kill(pids[1] as libc::pid_t, libc::SIGKILL) }, 0);
    let stopped = wait_for_state(&mut watch, proto::MachineState::Stopped).await;
    let exit = stopped.last_exit.unwrap();
    assert_eq!(exit.reason(), proto::ExitReason::Failed);
    assert_eq!(
        exit.detail,
        "its helper process ended with no word of the machine (signal: 9 (SIGKILL))"
    );
    let gone = client.get_machine(proto::GetMachineRequest { name: "b".into() }).await;
    assert_eq!(gone.unwrap().into_inner().machine.unwrap().pid, 0);
    for name in ["a", "c"] {
        let (stdout, _, _) = exec(&mut client, name, &["echo", "on"], None).await.unwrap();
        assert_eq!(stdout, b"on\n", "{name} runs on");
    }
    // Started again, in a new helper.
    let again = client.start_machine(proto::StartMachineRequest { name: "b".into() }).await;
    let again = again.unwrap().into_inner().machine.unwrap();
    assert_ne!(again.pid, pids[1]);
    let (stdout, _, _) = exec(&mut client, "b", &["echo", "back"], None).await.unwrap();
    assert_eq!(stdout, b"back\n");
    daemon.shutdown().await;
    for pid in [pids[0], pids[2], again.pid] {
        // SAFETY: signal 0 tests the process alone.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "helper {pid} outlived machined's shutdown");
    }
}

/// The whole life of a machine, over gRPC: made, booted, a command run with its stdin and
/// output streamed, files in and out, its console, its vsock through its socket, stopped with its
/// filesystems synced, deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_boots_runs_a_command_and_stops() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();

    let made = create(&mut client, "m1", daemon.spec("quiet libmachine_action=agent").await).await;
    assert_eq!(state_of(&made), proto::MachineState::Created);
    assert_eq!(made.vsock_socket, "", "no vsock socket before it runs");
    let watch = proto::WatchMachineRequest { name: "m1".into() };
    let mut watch = client.watch_machine(watch).await.unwrap().into_inner();
    wait_for_state(&mut watch, proto::MachineState::Created).await;

    let started = client.start_machine(proto::StartMachineRequest { name: "m1".into() }).await;
    let started = started.unwrap().into_inner().machine.unwrap();
    assert_eq!(state_of(&started), proto::MachineState::Running);
    wait_for_state(&mut watch, proto::MachineState::Running).await;
    let again = client.start_machine(proto::StartMachineRequest { name: "m1".into() }).await;
    assert_eq!(again.unwrap_err().code(), Code::FailedPrecondition, "it runs already");

    let script = "echo hi; echo err >&2; read x; echo got $x; exit 3";
    let (stdout, stderr, exit) =
        exec(&mut client, "m1", &["sh", "-c", script], Some(b"abc\n")).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&stdout), "hi\ngot abc\n");
    assert_eq!(String::from_utf8_lossy(&stderr), "err\n");
    assert_eq!(exit, proto::exec_exit::Status::Code(3));
    let (_, _, exit) = exec(&mut client, "m1", &["sh", "-c", "kill -9 $$"], None).await.unwrap();
    assert_eq!(exit, proto::exec_exit::Status::Signal(9));

    let chunks = [
        proto::WriteFileRequest {
            name: "m1".into(),
            path: "/tmp/f".into(),
            mode: 0o640,
            data: b"one ".to_vec(),
        },
        proto::WriteFileRequest { data: b"two".to_vec(), ..Default::default() },
    ];
    client.write_file(tokio_stream::iter(chunks)).await.unwrap();
    let read = proto::ReadFileRequest { name: "m1".into(), path: "/tmp/f".into() };
    let mut answers = client.read_file(read).await.unwrap().into_inner();
    let mut data = Vec::new();
    while let Some(answer) = answers.next().await {
        data.extend(answer.unwrap().data);
    }
    assert_eq!(data, b"one two");
    let (stdout, _, _) =
        exec(&mut client, "m1", &["stat", "-c", "%a", "/tmp/f"], None).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&stdout), "640\n");
    let missing = proto::ReadFileRequest { name: "m1".into(), path: "/nope".into() };
    let missing = match client.read_file(missing).await {
        Ok(answers) => answers.into_inner().next().await.unwrap().unwrap_err(),
        Err(status) => status,
    };
    assert_eq!(missing.code(), Code::NotFound, "{missing:?}");

    assert!(console(&mut client, "m1").await.contains(INIT_MARKER));

    // The vsock socket speaks Firecracker's protocol: here to the agent's port, which answers
    // with its preamble.
    let machine = client.get_machine(proto::GetMachineRequest { name: "m1".into() }).await;
    let socket = machine.unwrap().into_inner().machine.unwrap().vsock_socket;
    let preamble = tokio::task::spawn_blocking(move || {
        let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.write_all(format!("CONNECT {}\n", libmachine_agent::DEFAULT_PORT).as_bytes()).unwrap();
        let mut ok = Vec::new();
        let mut byte = [0u8];
        while byte != *b"\n" {
            s.read_exact(&mut byte).unwrap();
            ok.push(byte[0]);
        }
        assert!(ok.starts_with(b"OK "), "{:?}", String::from_utf8_lossy(&ok));
        s.write_all(&libmachine_agent::preamble()).unwrap();
        let mut theirs = [0u8; 8];
        s.read_exact(&mut theirs).unwrap();
        theirs
    })
    .await
    .unwrap();
    assert_eq!(preamble, libmachine_agent::preamble());

    let stop =
        proto::StopMachineRequest { name: "m1".into(), sync_filesystems: true, timeout_ms: 0 };
    let stopped = client.stop_machine(stop).await.unwrap().into_inner().machine.unwrap();
    assert_eq!(state_of(&stopped), proto::MachineState::Stopped);
    let exit = stopped.status.unwrap().last_exit.unwrap();
    assert_eq!(exit.reason(), proto::ExitReason::Stopped);
    assert_eq!(stopped.vsock_socket, "", "no vsock socket once it stopped");
    wait_for_state(&mut watch, proto::MachineState::Stopped).await;
    let refused = exec(&mut client, "m1", &["true"], None).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");

    client
        .delete_machine(proto::DeleteMachineRequest { name: "m1".into(), force: false })
        .await
        .unwrap();
    let gone = client.get_machine(proto::GetMachineRequest { name: "m1".into() }).await;
    assert_eq!(gone.unwrap_err().code(), Code::NotFound);
    let ended = tokio::time::timeout(STATE_WAIT, watch.message()).await.unwrap().unwrap();
    assert!(ended.is_none(), "the watch ends with its machine: {ended:?}");
    daemon.shutdown().await;
}

/// A guest that powers its machine off stops it, its exit saying so; a stopped machine starts
/// again, booting afresh; machined's shutdown stops what runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_that_powers_off_stops_its_machine() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    create(&mut client, "off", daemon.spec("quiet").await).await;
    let watch = proto::WatchMachineRequest { name: "off".into() };
    let mut watch = client.watch_machine(watch).await.unwrap().into_inner();
    for boot in 1..=2 {
        client.start_machine(proto::StartMachineRequest { name: "off".into() }).await.unwrap();
        let stopped = wait_for_state(&mut watch, proto::MachineState::Stopped).await;
        assert_eq!(stopped.last_exit.unwrap().reason(), proto::ExitReason::Poweroff);
        let markers = console(&mut client, "off").await.matches(INIT_MARKER).count();
        assert_eq!(markers, boot, "the console is kept across starts");
    }

    create(&mut client, "left", daemon.spec("quiet libmachine_action=agent").await).await;
    client.start_machine(proto::StartMachineRequest { name: "left".into() }).await.unwrap();
    daemon.shutdown().await;
}

/// Callers that go mid-call leave nothing half-done (hotbed review 150): a start whose caller's
/// deadline passes still finishes, RUNNING, and is stopped whole; an exec whose caller goes has
/// its command killed.
#[tokio::test(flavor = "multi_thread")]
async fn callers_that_go_leave_nothing_half_done() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    create(&mut client, "c", daemon.spec("quiet libmachine_action=agent").await).await;
    let watch = proto::WatchMachineRequest { name: "c".into() };
    let mut watch = client.watch_machine(watch).await.unwrap().into_inner();
    let mut start = tonic::Request::new(proto::StartMachineRequest { name: "c".into() });
    start.set_timeout(Duration::from_millis(1));
    let gone = client.start_machine(start).await.unwrap_err();
    // tonic's client says its own deadline passing as CANCELLED.
    assert_eq!(gone.code(), Code::Cancelled, "{gone:?}");
    wait_for_state(&mut watch, proto::MachineState::Running).await;

    // How many processes named `sleep` the guest runs.
    let sleepers = async |client: &mut MachinedClient<Channel>| {
        let count = "n=0; for f in /proc/[0-9]*/comm; do read c < $f 2>/dev/null && \
                     [ \"$c\" = sleep ] && n=$((n+1)); done; echo $n";
        let (stdout, _, _) = exec(client, "c", &["sh", "-c", count], None).await.unwrap();
        String::from_utf8_lossy(&stdout).trim().parse::<u32>().unwrap()
    };
    // A caller of its own, on a runtime of its own, which dies: its socket closes, as a VM
    // manager's that crashed.
    let socket = daemon.server.as_ref().unwrap().socket().unwrap().to_owned();
    let caller = tokio::runtime::Runtime::new().unwrap();
    let (started_tx, started) = tokio::sync::oneshot::channel();
    caller.spawn(async move {
        let mut caller = connect_unix(&socket).await.unwrap();
        let start = proto::ExecStart {
            name: "c".into(),
            argv: vec!["sleep".into(), "3600".into()],
            ..Default::default()
        };
        let requests = tokio_stream::iter([proto::ExecRequest {
            request: Some(exec_request::Request::Start(start)),
        }])
        .chain(tokio_stream::pending());
        let mut answers = caller.exec(requests).await.unwrap().into_inner();
        let first = answers.message().await.unwrap().unwrap().response;
        started_tx.send(first).unwrap();
        // The caller and its call kept until the runtime goes.
        std::future::pending::<()>().await;
        drop((answers, caller));
    });
    let first = started.await.unwrap();
    assert!(matches!(first, Some(exec_response::Response::Started(_))), "{first:?}");
    assert_eq!(sleepers(&mut client).await, 1);
    tokio::task::spawn_blocking(move || caller.shutdown_background()).await.unwrap();
    tokio::time::timeout(STATE_WAIT, async {
        while sleepers(&mut client).await != 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the command of an exec whose caller went is killed");
    daemon.shutdown().await;
}

/// On KVM a running machine is snapshotted, restored as a new machine with its guest's state,
/// suspended and woken; elsewhere those calls are refused with UNIMPLEMENTED.
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_restores_and_suspensions() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    create(&mut client, "tmpl", daemon.spec("quiet libmachine_action=agent").await).await;
    client.start_machine(proto::StartMachineRequest { name: "tmpl".into() }).await.unwrap();
    let write = proto::WriteFileRequest {
        name: "tmpl".into(),
        path: "/tmp/prepared".into(),
        mode: 0o644,
        data: b"yes".to_vec(),
    };
    client.write_file(tokio_stream::iter([write])).await.unwrap();

    let snapshot = proto::SnapshotMachineRequest { name: "tmpl".into(), snapshot: "s1".into() };
    let snapshot = client.snapshot_machine(snapshot).await;
    if cfg!(target_os = "macos") {
        assert_eq!(snapshot.unwrap_err().code(), Code::Unimplemented);
        let suspend =
            client.suspend_machine(proto::SuspendMachineRequest { name: "tmpl".into() }).await;
        assert_eq!(suspend.unwrap_err().code(), Code::Unimplemented);
        daemon.shutdown().await;
        return;
    }
    snapshot.unwrap();
    let restore = proto::RestoreMachineRequest {
        snapshot: "s1".into(),
        name: "clone".into(),
        labels: [("of".to_owned(), "tmpl".to_owned())].into(),
        vsock_listen_ports: vec![],
    };
    let clone = client.restore_machine(restore).await.unwrap().into_inner().machine.unwrap();
    assert_eq!(state_of(&clone), proto::MachineState::Running);
    assert_eq!(clone.spec.unwrap().labels["of"], "tmpl");
    let (stdout, _, _) = exec(&mut client, "clone", &["cat", "/tmp/prepared"], None).await.unwrap();
    assert_eq!(stdout, b"yes");

    let watch = proto::WatchMachineRequest { name: "tmpl".into() };
    let mut watch = client.watch_machine(watch).await.unwrap().into_inner();
    client.suspend_machine(proto::SuspendMachineRequest { name: "tmpl".into() }).await.unwrap();
    wait_for_state(&mut watch, proto::MachineState::Suspended).await;
    // A call on its guest wakes it.
    let (stdout, _, _) = exec(&mut client, "tmpl", &["echo", "awake"], None).await.unwrap();
    assert_eq!(stdout, b"awake\n");
    wait_for_state(&mut watch, proto::MachineState::Running).await;
    client.suspend_machine(proto::SuspendMachineRequest { name: "tmpl".into() }).await.unwrap();
    wait_for_state(&mut watch, proto::MachineState::Suspended).await;
    client.resume_machine(proto::ResumeMachineRequest { name: "tmpl".into() }).await.unwrap();
    wait_for_state(&mut watch, proto::MachineState::Running).await;

    client.delete_snapshot(proto::DeleteSnapshotRequest { snapshot: "s1".into() }).await.unwrap();
    let gone = client.delete_snapshot(proto::DeleteSnapshotRequest { snapshot: "s1".into() }).await;
    assert_eq!(gone.unwrap_err().code(), Code::NotFound);
    let (stdout, _, _) = exec(&mut client, "clone", &["echo", "lives"], None).await.unwrap();
    assert_eq!(stdout, b"lives\n", "a clone outlives its snapshot");
    let busy = client
        .delete_machine(proto::DeleteMachineRequest { name: "clone".into(), force: false })
        .await;
    assert_eq!(busy.unwrap_err().code(), Code::FailedPrecondition);
    client
        .delete_machine(proto::DeleteMachineRequest { name: "clone".into(), force: true })
        .await
        .unwrap();
    daemon.shutdown().await;
}

/// What machined refuses, each with its code, before anything boots: no guest needed.
#[tokio::test(flavor = "multi_thread")]
async fn requests_that_mean_nothing_are_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let kernel = scratch.path().join("kernel");
    std::fs::write(&kernel, b"not a kernel").unwrap();
    let daemon = Daemon::start(&[("kernel", &kernel), ("initramfs.cpio", &kernel)]).await;
    let mut client = daemon.client.clone();
    let mut refused = Vec::new();
    let mut create = async |name: &str, spec: proto::MachineSpec| {
        let request = proto::CreateMachineRequest { name: name.into(), spec: Some(spec) };
        client.create_machine(request).await.map(drop).map_err(|s| s.code())
    };
    let boot = |kernel: &str| proto::MachineSpec {
        boot: Some(proto::Boot { kernel: kernel.into(), ..Default::default() }),
        root: Some(proto::Cell { tenant: 0, cell: 1 }),
        ..Default::default()
    };
    refused.push(("a bad name", create("Bad", boot("kernel")).await, Code::InvalidArgument));
    refused.push((
        "a name too long",
        create(&"a".repeat(64), boot("kernel")).await,
        Code::InvalidArgument,
    ));
    refused.push((
        "no boot",
        create("a", proto::MachineSpec::default()).await,
        Code::InvalidArgument,
    ));
    refused.push((
        "a kernel outside",
        create("a", boot("../machined.sock")).await,
        Code::InvalidArgument,
    ));
    refused.push((
        "an absolute kernel",
        create("a", boot("/etc/passwd")).await,
        Code::InvalidArgument,
    ));
    refused.push(("no kernel there", create("a", boot("nope")).await, Code::NotFound));
    let memory = proto::MachineSpec { memory_bytes: 4097, ..boot("kernel") };
    refused.push(("RAM not of pages", create("a", memory).await, Code::InvalidArgument));
    let vcpus = proto::MachineSpec { vcpus: u32::MAX, ..boot("kernel") };
    refused.push(("too many vCPUs", create("a", vcpus).await, Code::InvalidArgument));
    let rootless = proto::MachineSpec { root: None, ..boot("kernel") };
    refused.push(("no root", create("a", rootless).await, Code::InvalidArgument));
    let cell0 =
        proto::MachineSpec { root: Some(proto::Cell { tenant: 0, cell: 0 }), ..boot("kernel") };
    refused.push(("a root of cell 0", create("a", cell0).await, Code::InvalidArgument));
    let dax = proto::MachineSpec { root_dax_window_bytes: 4096, ..boot("kernel") };
    refused.push(("a DAX window not of 2 MiB", create("a", dax).await, Code::InvalidArgument));
    let ports = proto::MachineSpec { vsock_listen_ports: vec![5, 5], ..boot("kernel") };
    refused.push(("a vsock port twice", create("a", ports).await, Code::InvalidArgument));
    let forward = proto::MachineSpec {
        network: Some(proto::Network {
            port_forwards: vec![proto::PortForward { host_port: 8080, guest_port: 80 }],
            ..Default::default()
        }),
        ..boot("kernel")
    };
    refused.push(("a port forward", create("a", forward).await, Code::Unimplemented));
    let bad_rule = proto::MachineSpec {
        network: Some(proto::Network {
            egress: vec![proto::EgressRule {
                action: 1,
                host: "*".into(),
                first_port: 0,
                last_port: 70000,
            }],
            ..Default::default()
        }),
        ..boot("kernel")
    };
    let bad_rule_code =
        if cfg!(target_os = "linux") { Code::InvalidArgument } else { Code::Unimplemented };
    refused.push(("a port past 65535", create("a", bad_rule).await, bad_rule_code));
    refused.push(("made", create("a", boot("kernel")).await, Code::Ok));
    refused.push(("a name taken", create("a", boot("kernel")).await, Code::AlreadyExists));
    let refusals: Vec<_> = refused
        .into_iter()
        .map(|(what, got, want)| (what, got.err().unwrap_or(Code::Ok), want))
        .collect();
    for (what, got, want) in &refusals {
        assert_eq!(got, want, "{what}");
    }

    let exec_on_created = exec(&mut client, "a", &["true"], None).await.unwrap_err();
    assert_eq!(exec_on_created.code(), Code::FailedPrecondition);
    let no_argv = exec(&mut client, "a", &[], None).await.unwrap_err();
    assert_eq!(no_argv.code(), Code::InvalidArgument);
    let first_not_start = tokio_stream::iter([proto::ExecRequest {
        request: Some(exec_request::Request::Signal(9)),
    }]);
    let not_start = client.exec(first_not_start).await.unwrap_err();
    assert_eq!(not_start.code(), Code::InvalidArgument);
    let write =
        proto::WriteFileRequest { name: "a".into(), path: "relative".into(), ..Default::default() };
    let relative = client.write_file(tokio_stream::iter([write])).await.unwrap_err();
    assert_eq!(relative.code(), Code::InvalidArgument);

    // A kernel that won't load: the start fails, the machine STOPPED, saying why; on a host that
    // runs VMs, as the start makes one first.
    if !hypervisor() {
        daemon.shutdown().await;
        return;
    }
    let start = client.start_machine(proto::StartMachineRequest { name: "a".into() }).await;
    let start = start.unwrap_err();
    assert_eq!(start.code(), Code::FailedPrecondition, "{start:?}");
    let machine = client.get_machine(proto::GetMachineRequest { name: "a".into() }).await;
    let status = machine.unwrap().into_inner().machine.unwrap().status.unwrap();
    assert_eq!(status.state(), proto::MachineState::Stopped);
    assert_eq!(status.last_exit.unwrap().reason(), proto::ExitReason::BootFailed);
    daemon.shutdown().await;
}

/// The Unix socket is machined's user's alone, made so whatever the umask.
#[tokio::test(flavor = "multi_thread")]
async fn the_socket_is_its_users_alone() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = tempfile::tempdir().unwrap();
    let kernel = scratch.path().join("kernel");
    std::fs::write(&kernel, b"k").unwrap();
    let daemon = Daemon::start(&[("kernel", &kernel)]).await;
    let socket = daemon.server.as_ref().unwrap().socket().unwrap().to_owned();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    daemon.shutdown().await;
    assert!(!socket.exists(), "the socket goes with machined");
}

/// TCP is served with mutual TLS alone: a caller with a certificate of machined's client CA is
/// served; one with another CA's, or none, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_takes_callers_of_its_client_ca_alone() {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    let tls = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls");
    let scratch = tempfile::tempdir().unwrap();
    let kernel = scratch.path().join("kernel");
    std::fs::write(&kernel, b"k").unwrap();
    let daemon = Daemon::start_with(&[("kernel", &kernel)], |config| {
        config.tcp = Some(machined::Tcp {
            address: "127.0.0.1:0".parse().unwrap(),
            cert: tls.join("server.crt"),
            key: tls.join("server.key"),
            client_ca: tls.join("ca.crt"),
        });
    })
    .await;
    let address = daemon.server.as_ref().unwrap().tcp_address().unwrap();
    let read = |name: &str| std::fs::read(tls.join(name)).unwrap();
    let call = async |identity: Option<Identity>| {
        let mut config = ClientTlsConfig::new()
            .domain_name("machined")
            .ca_certificate(Certificate::from_pem(read("ca.crt")));
        if let Some(identity) = identity {
            config = config.identity(identity);
        }
        let endpoint =
            Channel::from_shared(format!("https://{address}")).unwrap().tls_config(config).unwrap();
        let channel = endpoint.connect().await.map_err(|e| e.to_string())?;
        let mut client = MachinedClient::new(channel);
        client
            .list_machines(proto::ListMachinesRequest {})
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    };
    call(Some(Identity::from_pem(read("client.crt"), read("client.key")))).await.unwrap();
    let other = Identity::from_pem(read("other/client.crt"), read("other/client.key"));
    assert!(call(Some(other)).await.is_err(), "a client of another CA");
    assert!(call(None).await.is_err(), "a client with no certificate");
    daemon.shutdown().await;
}
