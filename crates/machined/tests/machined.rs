// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined end to end, driven over gRPC as hotbed's Mac worker drives it: a machine made, booted
//! with cladefs's arm64 guest kernel and libmachine's test initramfs on a cell of its own, with a
//! network and a balloon, its console streamed (the guest's init runs a script the machine's
//! command line gives it: `libmachine_action=sh`), its vsock reached through its socket, then
//! deleted; what machined refuses; and TCP's mutual TLS.
//!
//! The guest: cladefs's arm64 Image and the test initramfs in target/arm64-guest
//! (`crates/libmachine/scripts/arm64-guest.sh`), or where `CLADEFS_GUEST_KERNEL` and
//! `LIBMACHINE_GUEST_DIR` say. A test that needs one and lacks it returns with a "skipping:" note.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod common;

use common::{fork, guest, hypervisor, import_root};
use machined::client::{MachinedClient, connect_unix};
use machined::proto;
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
        // Short, as vsock sockets' paths must be.
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
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

/// Polls machine `name` until its state is `state`: its status then.
async fn wait_for_state(
    client: &mut MachinedClient<Channel>,
    name: &str,
    state: proto::MachineState,
) -> proto::MachineStatus {
    tokio::time::timeout(STATE_WAIT, async {
        loop {
            let got = client.get_machine(proto::GetMachineRequest { name: name.into() }).await;
            let status = got.unwrap().into_inner().machine.unwrap().status.unwrap();
            if status.state() == state {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name}: no {state:?} within {STATE_WAIT:?}"))
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

/// Polls machine `name`'s console until it holds `needle`: the console then.
async fn console_has(client: &mut MachinedClient<Channel>, name: &str, needle: &str) -> String {
    tokio::time::timeout(STATE_WAIT, async {
        loop {
            let text = console(client, name).await;
            if text.contains(needle) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name}: no {needle:?} on its console within {STATE_WAIT:?}"))
}

/// A command line making the guest's init run `script` and power off.
fn sh(script: &str) -> String {
    format!("quiet libmachine_action=sh libmachine_sh=\"{script}\"")
}

async fn start(client: &mut MachinedClient<Channel>, name: &str) -> proto::Machine {
    let started = client.start_machine(proto::StartMachineRequest { name: name.into() }).await;
    started.unwrap().into_inner().machine.unwrap()
}

/// A machine boots from a cell (board 13992): its root is Fxfs on its extent store device, from
/// a cell of machined's embedded daemon, its own (forked from a base the daemon imported); what
/// its guest writes there is the cell's, there for the next machine on it, and no other machine's.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_boots_from_its_cell() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    let write = sh("grep ' / ' /proc/mounts; echo kept > /kept && sync");
    let one = daemon.spec(&write).await;
    let root = one.root;
    create(&mut client, "one", one).await;
    start(&mut client, "one").await;
    let stopped = wait_for_state(&mut client, "one", proto::MachineState::Stopped).await;
    assert_eq!(stopped.last_exit.unwrap().reason(), proto::ExitReason::Poweroff);
    let text = console_has(&mut client, "one", "sh exited 0").await;
    assert!(text.contains("root / cladefs rw"), "{text}");

    // Another machine's root is its own.
    create(&mut client, "two", daemon.spec(&sh("test -e /kept; echo kept=$?")).await).await;
    start(&mut client, "two").await;
    let text = console_has(&mut client, "two", "sh exited").await;
    assert!(text.contains("kept=1"), "{text}");

    // The cell the first wrote, under a machine after it.
    let spec = proto::MachineSpec { root, ..daemon.spec(&sh("cat /kept")).await };
    create(&mut client, "after", spec).await;
    start(&mut client, "after").await;
    let text = console_has(&mut client, "after", "sh exited").await;
    assert!(text.contains("\nkept\n"), "{text}");

    // A cell one machine has open, another can't boot from.
    let held = daemon.spec("quiet libmachine_action=hang").await;
    let twin = proto::MachineSpec { root: held.root, ..daemon.spec("quiet").await };
    create(&mut client, "held", held).await;
    start(&mut client, "held").await;
    create(&mut client, "twin", twin).await;
    let started = client.start_machine(proto::StartMachineRequest { name: "twin".into() }).await;
    let refused = started.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    daemon.shutdown().await;
}

/// A machine's balloon (board 13490), which hotbed asks for: what its guest frees goes back to the
/// host by itself (free page reporting), its helper's memory falling with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_balloon_gives_memory_back() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    // 512 MiB used and held for the host to see (the sleep), then freed.
    let script = "cat /sys/bus/virtio/devices/*/device; \
        mount -t tmpfs -o size=600m none /tmp && \
        dd if=/dev/urandom of=/tmp/f bs=1M count=512 2>/dev/null && sync; \
        echo filled; sleep 15; rm /tmp/f; echo freed; sleep 3600";
    let spec = proto::MachineSpec {
        memory_bytes: 1 << 30,
        balloon: true,
        ..daemon.spec(&sh(script)).await
    };
    create(&mut client, "b", spec).await;
    let pid = start(&mut client, "b").await.pid;
    let text = console_has(&mut client, "b", "filled").await;
    // virtio-balloon's device type is 5.
    assert!(text.contains("0x0005"), "a balloon device among the guest's: {text}");
    let full = common::footprint(pid);
    assert!(full > 512 << 20, "the helper holds the guest's 512 MiB: {full}");
    console_has(&mut client, "b", "freed").await;
    tokio::time::timeout(STATE_WAIT, async {
        while common::footprint(pid) + (384 << 20) > full {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("the helper still holds {} of {full} bytes", common::footprint(pid))
    });
    daemon.shutdown().await;
}

/// A machine's network (board 14131), as hotbed's egress rule gives it, the one rule allowing "*":
/// its guest reaches a host listener through its gateway, which stands for the host's loopback,
/// where a rule allows it, and is refused where none does.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_reaches_what_its_network_allows() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    let allowed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = allowed.local_addr().unwrap().port();
    let denied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let denied_port = denied.local_addr().unwrap().port();
    let rule = proto::EgressRule {
        action: proto::EgressAction::Allow.into(),
        host: "127.0.0.1".into(),
        first_port: u32::from(port),
        last_port: u32::from(port),
    };
    let script = format!(
        "echo hello from the guest | nc 10.0.2.1 {port}; echo sent=$?; \
         nc 10.0.2.1 {denied_port} </dev/null; echo refused=$?"
    );
    let spec = proto::MachineSpec {
        network: Some(proto::Network { egress: vec![rule], ..Default::default() }),
        ..daemon.spec(&sh(&script)).await
    };
    create(&mut client, "n", spec).await;
    let served = std::thread::spawn(move || {
        let (mut conn, _) = allowed.accept().unwrap();
        let mut got = String::new();
        conn.read_to_string(&mut got).unwrap();
        got
    });
    start(&mut client, "n").await;
    let text = console_has(&mut client, "n", "sh exited").await;
    assert_eq!(served.join().unwrap(), "hello from the guest\n");
    assert!(text.contains("sent=0"), "{text}");
    assert!(text.contains("refused=1"), "a port its rules don't allow: {text}");
    daemon.shutdown().await;
}

/// Each machine runs in a helper process of its own (board 13460): several at once; one whose
/// helper dies ends with that said, while machined and the others run on.
#[tokio::test(flavor = "multi_thread")]
async fn machines_run_in_helpers_of_their_own() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    let mut pids = Vec::new();
    for name in ["a", "b", "c"] {
        create(&mut client, name, daemon.spec("quiet libmachine_action=hang").await).await;
        let started = start(&mut client, name).await;
        assert_eq!(state_of(&started), proto::MachineState::Running);
        assert_ne!(started.pid, 0);
        assert_ne!(started.pid, std::process::id(), "not machined's own process");
        pids.push(started.pid);
    }
    pids.dedup();
    assert_eq!(pids.len(), 3, "a process each: {pids:?}");
    for name in ["a", "b", "c"] {
        console_has(&mut client, name, INIT_MARKER).await;
    }

    // SAFETY: a plain system call, on the test's own machine's helper.
    assert_eq!(unsafe { libc::kill(pids[1] as libc::pid_t, libc::SIGKILL) }, 0);
    let stopped = wait_for_state(&mut client, "b", proto::MachineState::Stopped).await;
    let exit = stopped.last_exit.unwrap();
    assert_eq!(exit.reason(), proto::ExitReason::Failed);
    assert_eq!(
        exit.detail,
        "its helper process ended with no word of the machine (signal: 9 (SIGKILL))"
    );
    let gone = client.get_machine(proto::GetMachineRequest { name: "b".into() }).await;
    assert_eq!(gone.unwrap().into_inner().machine.unwrap().pid, 0);
    for name in ["a", "c"] {
        let got = client.get_machine(proto::GetMachineRequest { name: name.into() }).await;
        let machine = got.unwrap().into_inner().machine.unwrap();
        assert_eq!(state_of(&machine), proto::MachineState::Running, "{name} runs on");
    }
    // Started again, in a new helper.
    let again = start(&mut client, "b").await;
    assert_ne!(again.pid, pids[1]);
    daemon.shutdown().await;
    for pid in [pids[0], pids[2], again.pid] {
        // SAFETY: signal 0 tests the process alone.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "helper {pid} outlived machined's shutdown");
    }
}

/// The life of a machine, over gRPC: made, booted, its console streamed, its vsock through its
/// socket, deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_is_made_booted_and_deleted() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();

    let made = create(&mut client, "m1", daemon.spec("quiet libmachine_action=agent").await).await;
    assert_eq!(state_of(&made), proto::MachineState::Created);
    assert_eq!(made.vsock_socket, "", "no vsock socket before it runs");
    let listed = client.list_machines(proto::ListMachinesRequest {}).await;
    assert_eq!(listed.unwrap().into_inner().machines.len(), 1);

    let started = start(&mut client, "m1").await;
    assert_eq!(state_of(&started), proto::MachineState::Running);
    let again = client.start_machine(proto::StartMachineRequest { name: "m1".into() }).await;
    assert_eq!(again.unwrap_err().code(), Code::FailedPrecondition, "it runs already");
    let delete = proto::DeleteMachineRequest { name: "m1".into(), force: false };
    let refused = client.delete_machine(delete).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "it runs: {refused:?}");

    // A console followed, which ends with its machine.
    let follow = proto::StreamConsoleRequest { name: "m1".into(), offset: 0, follow: true };
    let mut following = client.stream_console(follow).await.unwrap().into_inner();
    let first = tokio::time::timeout(STATE_WAIT, following.next()).await.unwrap();
    assert!(!first.unwrap().unwrap().data.is_empty());
    console_has(&mut client, "m1", INIT_MARKER).await;

    // The vsock socket speaks Firecracker's protocol: here to the agent's port, which answers
    // with its preamble.
    let machine = client.get_machine(proto::GetMachineRequest { name: "m1".into() }).await;
    let socket = machine.unwrap().into_inner().machine.unwrap().vsock_socket;
    let preamble = tokio::task::spawn_blocking(move || {
        // The guest's agent listens once its init has started it: refused until then.
        let until = std::time::Instant::now() + STATE_WAIT;
        loop {
            let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let port = libmachine_agent::DEFAULT_PORT;
            s.write_all(format!("CONNECT {port}\n").as_bytes()).unwrap();
            let mut ok = Vec::new();
            let mut byte = [0u8];
            let answered = loop {
                match s.read_exact(&mut byte) {
                    Ok(()) => ok.push(byte[0]),
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break false,
                    Err(e) => panic!("the vsock socket: {e}"),
                }
                if byte == *b"\n" {
                    break true;
                }
            };
            if !answered {
                assert!(std::time::Instant::now() < until, "no agent behind the vsock socket");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            assert!(ok.starts_with(b"OK "), "{:?}", String::from_utf8_lossy(&ok));
            s.write_all(&libmachine_agent::preamble()).unwrap();
            let mut theirs = [0u8; 8];
            s.read_exact(&mut theirs).unwrap();
            return theirs;
        }
    })
    .await
    .unwrap();
    assert_eq!(preamble, libmachine_agent::preamble());

    client
        .delete_machine(proto::DeleteMachineRequest { name: "m1".into(), force: true })
        .await
        .unwrap();
    let gone = client.get_machine(proto::GetMachineRequest { name: "m1".into() }).await;
    assert_eq!(gone.unwrap_err().code(), Code::NotFound);
    let ended =
        tokio::time::timeout(STATE_WAIT, async { while following.next().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the console stream ends with its machine");
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
    for boot in 1..=2 {
        start(&mut client, "off").await;
        let stopped = wait_for_state(&mut client, "off", proto::MachineState::Stopped).await;
        assert_eq!(stopped.last_exit.unwrap().reason(), proto::ExitReason::Poweroff);
        let markers = console(&mut client, "off").await.matches(INIT_MARKER).count();
        assert_eq!(markers, boot, "the console is kept across starts");
    }

    create(&mut client, "left", daemon.spec("quiet libmachine_action=hang").await).await;
    start(&mut client, "left").await;
    daemon.shutdown().await;
}

/// Callers that go mid-call leave nothing half-done (hotbed review 150): a start whose caller's
/// deadline passes still finishes, RUNNING, and is deleted whole.
#[tokio::test(flavor = "multi_thread")]
async fn callers_that_go_leave_nothing_half_done() {
    let Some((kernel, initramfs)) = guest() else { return };
    let daemon = Daemon::start_guest(&kernel, &initramfs).await;
    let mut client = daemon.client.clone();
    create(&mut client, "c", daemon.spec("quiet libmachine_action=hang").await).await;
    let mut start = tonic::Request::new(proto::StartMachineRequest { name: "c".into() });
    start.set_timeout(Duration::from_millis(1));
    let gone = client.start_machine(start).await.unwrap_err();
    // tonic's client says its own deadline passing as CANCELLED.
    assert_eq!(gone.code(), Code::Cancelled, "{gone:?}");
    wait_for_state(&mut client, "c", proto::MachineState::Running).await;
    client
        .delete_machine(proto::DeleteMachineRequest { name: "c".into(), force: true })
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
    refused.push(("a port past 65535", create("a", bad_rule).await, Code::InvalidArgument));
    refused.push(("made", create("a", boot("kernel")).await, Code::Ok));
    refused.push(("a name taken", create("a", boot("kernel")).await, Code::AlreadyExists));
    let refusals: Vec<_> = refused
        .into_iter()
        .map(|(what, got, want)| (what, got.err().unwrap_or(Code::Ok), want))
        .collect();
    for (what, got, want) in &refusals {
        assert_eq!(got, want, "{what}");
    }

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
