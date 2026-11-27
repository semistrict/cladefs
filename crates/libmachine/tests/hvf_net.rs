// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest's network, end to end, on Hypervisor.framework: the test guest boots with a network
//! and its agent, a thread runs the machine, and commands in the guest (busybox nc, nslookup,
//! dd) connect to listeners the tests open on the host's loopback, which the guest reaches
//! through its gateway. Nothing needs the internet.
//!
//! The guest is where `hvf_linux` finds it (`crates/libmachine/scripts/arm64-guest.sh`); tests
//! return early with a note when it is missing. A process has one VM: the tests take turns.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::MutexGuard;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{guest, one_machine};
use libmachine::agent::{Agent, Command, Output};
use libmachine::net::{
    ConnectionRecord, DecidedBy, Decider, EgressPolicy, GATEWAY_ADDR, HostPattern, Hosts, Network,
    NetworkConfig, Ports, Verdict,
};
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig, StopHandle};

/// A booted guest with a network and its agent answering, its machine
/// running on a thread of its own.
struct Guest {
    agent: Agent,
    network: Network,
    stop: StopHandle,
    runner: Option<JoinHandle<(ExitReason, Machine)>>,
    _one: MutexGuard<'static, ()>,
}

impl Guest {
    fn boot(policy: EgressPolicy) -> Option<Guest> {
        Self::boot_with(NetworkConfig::new(policy))
    }

    fn boot_with(network: NetworkConfig) -> Option<Guest> {
        let (kernel, initrd) = guest()?;
        let one = one_machine();
        let boot = LinuxBoot {
            kernel,
            initrd: Some(initrd),
            cmdline: "quiet libmachine_action=agent".into(),
        };
        let config =
            MachineConfig { mem_size: 256 << 20, network: Some(network), ..Default::default() };
        let start = Instant::now();
        let mut m = Machine::boot_linux(config, &boot).expect("boot");
        let agent = m.agent().expect("an agent");
        let network = m.network().expect("a network");
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        let guest = Guest { agent, network, stop, runner: Some(runner), _one: one };
        if let Err(e) = guest.agent.wait_ready(Duration::from_secs(60)) {
            let console = guest.finish();
            panic!("agent not ready: {e}; console:\n{console}");
        }
        eprintln!("agent ready {:?} after creating the machine", start.elapsed());
        Some(guest)
    }

    /// Stops the machine; its console.
    fn finish(mut self) -> String {
        self.stop.stop();
        let (_, m) = self.runner.take().unwrap().join().expect("runner");
        String::from_utf8_lossy(&m.serial_output()).into_owned()
    }

    fn sh(&self, script: &str) -> Output {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        self.agent.exec(&cmd).unwrap_or_else(|e| panic!("{script}: {e}"))
    }

    /// The log's records, waiting (a few seconds at most) until there are
    /// `n`: a connection is logged when the stack sees its end, which may
    /// come just after the command in the guest has exited.
    fn log(&self, n: usize) -> Vec<ConnectionRecord> {
        let mut log = Vec::new();
        let start = Instant::now();
        while log.len() < n && start.elapsed() < Duration::from_secs(10) {
            log.extend(self.network.take_log());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(log.len(), n, "{log:#?}");
        log
    }

    /// Runs `script`, which must succeed; its stdout.
    fn ok(&self, script: &str) -> String {
        let out = self.sh(script);
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

fn listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    listener().1
}

/// The guest connects through its gateway to a listener on the host's
/// loopback and they exchange data both ways; the guest's address, route
/// and DNS server came from the kernel command line.
#[test]
fn guest_connects_to_a_host_listener() {
    let (l, port) = listener();
    let Some(g) = Guest::boot(EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port)) else {
        return;
    };
    let config = g.ok("ip addr show eth0; ip route; cat /etc/resolv.conf; hostname");
    assert!(config.contains("inet 10.0.2.2/30"), "{config}");
    assert!(config.contains("default via 10.0.2.1"), "{config}");
    assert!(config.contains("nameserver 10.0.2.1"), "{config}");
    assert!(config.contains("mtu 65520"), "{config}");
    assert!(config.ends_with("\nguest\n"), "the host name: {config}");

    let server = std::thread::spawn(move || {
        let (mut s, peer) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut got = Vec::new();
        let mut buf = [0; 64];
        while !got.ends_with(b"\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "EOF before the line");
            got.extend_from_slice(&buf[..n]);
        }
        s.write_all(b"world from the host\n").unwrap();
        (got, peer)
    });
    let start = Instant::now();
    let out = g.ok(&format!("echo hello from the guest | nc {GATEWAY_ADDR} {port}"));
    let elapsed = start.elapsed();
    let (got, peer) = server.join().unwrap();
    assert_eq!(got, b"hello from the guest\n");
    assert_eq!(out, "world from the host\n");
    assert!(peer.ip().is_loopback(), "the VMM connects from the host: {peer}");
    eprintln!("exchange through the network took {elapsed:?} (an exec included)");
}

/// A connection the policy denies fails at once with "connection refused",
/// even with a listener behind it; so does an allowed one nothing listens
/// for; and a policy change applies to the next connection.
#[test]
fn denied_and_refused_connections_fail_fast() {
    let (l, port) = listener();
    let closed = closed_port();
    let policy = EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, closed);
    let Some(g) = Guest::boot(policy) else {
        return;
    };
    l.set_nonblocking(true).unwrap();
    for (what, target) in [
        ("denied, listener behind it", format!("{GATEWAY_ADDR} {port}")),
        ("denied public address", "1.1.1.1 443".to_string()),
        ("denied private address", "192.168.0.1 80".to_string()),
        ("allowed, nothing listening", format!("{GATEWAY_ADDR} {closed}")),
    ] {
        let start = Instant::now();
        let out = g.sh(&format!("nc {target} </dev/null"));
        let elapsed = start.elapsed();
        assert!(!out.status.success(), "{what}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("refused"), "{what}: {stderr}");
        assert!(elapsed < Duration::from_secs(2), "{what}: took {elapsed:?}");
        eprintln!("{what}: refused after {elapsed:?} (an exec included)");
    }
    assert!(l.accept().is_err(), "the denied connection never reached the host");

    g.network.set_policy(EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port));
    l.set_nonblocking(false).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"now allowed\n").unwrap();
    });
    let out = g.ok(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    server.join().unwrap();
    assert_eq!(out, "now allowed\n");
}

/// The VMM's DNS server answers names the policy could allow, from the
/// host's resolver ("localhost" resolves on every host), through the
/// resolver the guest's resolv.conf names; other names fail at once.
#[test]
fn dns_resolves_allowed_names_only() {
    let policy =
        EgressPolicy::deny_all().allow(HostPattern::name("localhost").unwrap(), Ports::ANY);
    let Some(g) = Guest::boot(policy) else {
        return;
    };
    let out = g.ok("nslookup -type=a localhost");
    assert!(out.contains("10.0.2.1"), "the server: {out}");
    assert!(out.contains("127.0.0.1"), "the answer: {out}");
    let start = Instant::now();
    let out = g.sh("nslookup -type=a not-allowed.example");
    let elapsed = start.elapsed();
    assert!(!out.status.success(), "{out:?}");
    let all =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("NXDOMAIN"), "{all}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

/// Bulk transfer both ways through the stack: 64 MiB each, timed on the
/// host side.
#[test]
fn throughput() {
    const BYTES: usize = 64 << 20;
    let (l, port) = listener();
    let Some(g) = Guest::boot(EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port)) else {
        return;
    };
    // Guest to host.
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let start = Instant::now();
        let mut buf = vec![0; 1 << 20];
        let mut total = 0;
        loop {
            let n = s.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        (total, start.elapsed(), l)
    });
    let blocks = BYTES / 65536;
    g.ok(&format!(
        "dd if=/dev/zero bs=65536 count={blocks} 2>/dev/null | nc {GATEWAY_ADDR} {port}"
    ));
    let (total, upload, l) = server.join().unwrap();
    assert_eq!(total, BYTES);
    let up = g.log(1);
    assert_eq!(up[0].bytes_to_host, BYTES as u64, "{:?}", up[0]);

    // Host to guest.
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let start = Instant::now();
        let chunk = vec![0x5a; 1 << 20];
        for _ in 0..BYTES / chunk.len() {
            s.write_all(&chunk).unwrap();
        }
        drop(s);
        start.elapsed()
    });
    let out = g.ok(&format!("nc {GATEWAY_ADDR} {port} </dev/null | wc -c"));
    let download = server.join().unwrap();
    assert_eq!(out.trim(), BYTES.to_string());
    let down = g.log(1);
    assert_eq!(down[0].bytes_to_guest, BYTES as u64, "{:?}", down[0]);
    let stats = g.network.stats();
    assert_eq!(stats.bytes_to_host, BYTES as u64);
    assert_eq!(stats.bytes_to_guest, BYTES as u64);
    assert_eq!(stats.connections_allowed, 2);
    let rate = |d: Duration| BYTES as f64 / d.as_secs_f64() / (1 << 20) as f64;
    eprintln!(
        "throughput: guest to host {:.0} MiB/s, host to guest {:.0} MiB/s (host side: until \
         the last byte read / written into its socket)",
        rate(upload),
        rate(download)
    );
    assert!(rate(upload) > 20.0, "guest to host {:.1} MiB/s", rate(upload));
    assert!(rate(download) > 20.0, "host to guest {:.1} MiB/s", rate(download));
}

/// A decider decides the connections an ask rule hands it: the guest's
/// connect waits for the answer, an allowed one reaches the host, a denied
/// one is refused at once; both are logged with what decided them, the
/// allowed one with the bytes it moved.
#[test]
fn a_decider_allows_and_denies_connections() {
    let (l, allowed) = listener();
    let (denied_l, denied) = listener();
    let decider = Decider::new(move |r| {
        assert_eq!(r.addr, Ipv4Addr::LOCALHOST);
        Some(if r.port == allowed { Verdict::Allow } else { Verdict::Deny })
    });
    let policy = EgressPolicy::deny_all().ask(Ipv4Addr::LOCALHOST, Ports::ANY);
    let Some(g) = Guest::boot_with(NetworkConfig::new(policy).with_decider(decider)) else {
        return;
    };
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut got = Vec::new();
        let mut buf = [0; 64];
        while !got.ends_with(b"\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        s.write_all(b"decided\n").unwrap();
    });
    let out = g.ok(&format!("echo hello | nc {GATEWAY_ADDR} {allowed}"));
    server.join().unwrap();
    assert_eq!(out, "decided\n");
    denied_l.set_nonblocking(true).unwrap();
    let start = Instant::now();
    let out = g.sh(&format!("nc {GATEWAY_ADDR} {denied} </dev/null"));
    let elapsed = start.elapsed();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refused"));
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(denied_l.accept().is_err(), "never reached the host");

    let mut log = g.log(2);
    log.sort_by_key(|r| r.verdict == Verdict::Deny);
    let (a, d) = (&log[0], &log[1]);
    assert_eq!((a.addr, a.port), (Ipv4Addr::LOCALHOST, allowed));
    assert_eq!((a.verdict, &a.decided_by), (Verdict::Allow, &DecidedBy::Callback));
    assert!(a.connected);
    assert_eq!((a.bytes_to_host, a.bytes_to_guest), (6, 8), "{a:?}");
    assert!(a.names.is_empty());
    assert_eq!((d.port, d.verdict), (denied, Verdict::Deny));
    assert_eq!(d.decided_by, DecidedBy::Callback);
    assert!(!d.connected);
    assert_eq!((d.bytes_to_host, d.bytes_to_guest), (0, 0));
    let stats = g.network.stats();
    assert_eq!(stats.decisions, 2);
    assert_eq!((stats.connections_allowed, stats.connections_denied), (1, 1));
    assert_eq!((stats.bytes_to_host, stats.bytes_to_guest), (6, 8));
    eprintln!("denied by the decider after {elapsed:?} (an exec included); {log:#?}");
}

/// A decider that is stuck holds up neither the guest nor the network:
/// each connection waits its timeout and gets the default (deny, then
/// allow); one that panics gets the default at once.
#[test]
fn a_stuck_or_panicking_decider_gets_its_default() {
    let (l, port) = listener();
    let (release, stuck) = std::sync::mpsc::channel::<()>();
    let stuck = std::sync::Mutex::new(stuck);
    let stuck_decider = Decider::new(move |_| {
        let _ = stuck.lock().unwrap().recv();
        Some(Verdict::Allow)
    });
    let timeout = Duration::from_millis(300);
    let policy = EgressPolicy::deny_all().ask(Ipv4Addr::LOCALHOST, Ports::ANY);
    let config =
        NetworkConfig::new(policy).with_decider(stuck_decider.clone().with_timeout(timeout));
    let Some(g) = Guest::boot_with(config) else {
        return;
    };
    let timed = |script: &str| {
        let start = Instant::now();
        let out = g.sh(script);
        (out, start.elapsed())
    };
    let (out, elapsed) = timed(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("refused"));
    assert!(elapsed >= timeout && elapsed < Duration::from_secs(3), "took {elapsed:?}");
    eprintln!("stuck decider, default deny: refused after {elapsed:?}");

    // Still stuck in the first callback; this one defaults to allow.
    g.network.set_decider(Some(stuck_decider.with_timeout(timeout).with_default(Verdict::Allow)));
    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"by default\n").unwrap();
        l
    });
    let (out, elapsed) = timed(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    let l = server.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"by default\n");
    assert!(elapsed >= timeout, "took {elapsed:?}");
    eprintln!("stuck decider, default allow: connected after {elapsed:?}");

    release.send(()).unwrap();
    g.network.set_decider(Some(Decider::new(|_| {
        panic!("a decider that fails (this panic is expected)")
    })));
    l.set_nonblocking(true).unwrap();
    let (out, elapsed) = timed(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    assert!(!out.status.success());
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    // The allowed connection is logged when it ends, which may come after
    // the next command: count, not order.
    let by: Vec<DecidedBy> = g.log(3).into_iter().map(|r| r.decided_by).collect();
    let count = |d: &DecidedBy| by.iter().filter(|b| *b == d).count();
    assert_eq!(count(&DecidedBy::CallbackTimeout), 2, "{by:?}");
    assert_eq!(count(&DecidedBy::CallbackFailed), 1, "{by:?}");
    let stats = g.network.stats();
    assert_eq!((stats.decisions_timed_out, stats.decisions_failed), (2, 1));
}

/// A host entry names a mock service on the host's loopback: the VMM's
/// DNS answers it with the gateway, and a name rule (no IP rule) lets the
/// guest reach it; an entry the policy does not cover is NXDOMAIN; taking
/// the entry away takes the name rule's reach with it.
#[test]
fn host_entries_name_a_mock_service() {
    let (l, port) = listener();
    let hosts = Hosts::new()
        .with("mock.service", Ipv4Addr::LOCALHOST)
        .unwrap()
        .with("hidden.service", Ipv4Addr::LOCALHOST)
        .unwrap();
    let policy = EgressPolicy::deny_all().allow(HostPattern::name("mock.service").unwrap(), port);
    let Some(g) = Guest::boot_with(NetworkConfig::new(policy).with_hosts(hosts)) else {
        return;
    };
    let out = g.ok("nslookup -type=a mock.service");
    assert!(out.contains("Address: 10.0.2.1"), "the answer: {out}");
    let out = g.sh("nslookup -type=a hidden.service");
    let all =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("NXDOMAIN"), "{all}");

    let server = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"mock\n").unwrap();
        l
    });
    let out = g.ok(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    let l = server.join().unwrap();
    assert_eq!(out, "mock\n");
    let log = g.log(1);
    // Every name the address goes by, the uncovered entry's too.
    assert_eq!(log[0].names, ["hidden.service", "mock.service"]);
    assert!(matches!(log[0].decided_by, DecidedBy::Rule { index: 0, .. }));

    g.network.set_hosts(Hosts::new());
    l.set_nonblocking(true).unwrap();
    let out = g.sh(&format!("nc {GATEWAY_ADDR} {port} </dev/null"));
    assert!(!out.status.success(), "{out:?}");
    assert!(l.accept().is_err());
    assert!(g.network.stats().dns_refused >= 1);
}

/// cladefs's guest kernel, which Firecracker boots too, brings the card up from the command line.
#[test]
fn cladefs_kernel_configures_the_network() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let _one = one_machine();
    let boot = LinuxBoot {
        kernel,
        initrd: Some(initrd),
        cmdline:
            "quiet libmachine_cat=/proc/net/pnp,/sys/class/net/eth0/address,/sys/class/net/eth0/mtu"
                .into(),
    };
    let config = MachineConfig {
        mem_size: 256 << 20,
        network: Some(NetworkConfig::default()),
        ..Default::default()
    };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    assert_eq!(m.run_for(Duration::from_secs(60)).expect("run"), ExitReason::Poweroff);
    let console = String::from_utf8_lossy(&m.serial_output()).replace("\r\n", "\n");
    assert!(console.contains("nameserver 10.0.2.1"), "{console}");
    assert!(console.contains("02:00:00:00:00:02"), "{console}");
    assert!(console.contains("\n65520\n"), "{console}");
}

/// A machine without a network has no card, and no network handle.
#[test]
fn no_network_by_default() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let _one = one_machine();
    let boot = LinuxBoot {
        kernel,
        initrd: Some(initrd),
        cmdline: "quiet libmachine_cat=/sys/class/net".into(),
    };
    let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    assert!(m.network().is_none());
    assert_eq!(m.run_for(Duration::from_secs(60)).expect("run"), ExitReason::Poweroff);
    let console = String::from_utf8_lossy(&m.serial_output()).replace("\r\n", "\n");
    assert!(
        console.contains("libmachine-init: /sys/class/net:\nlo\nlibmachine-init: end"),
        "{console}"
    );
}

#[test]
fn bare_machines_refuse_a_network() {
    let err = Machine::new(MachineConfig {
        network: Some(NetworkConfig::default()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(err, libmachine::Error::InvalidConfig(_)), "{err}");
}
