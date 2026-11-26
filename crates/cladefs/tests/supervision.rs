// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! board 8501: a panic on any of cladefsd's services, a duty of the whole process's, aborts the
//! daemon, its log naming the service (`cladefs::cladefsd::supervised`), as one on the page
//! cache's flusher does (hotbed question 112). Before, only a panic holding a lock shared beyond
//! one cell did; any other ended its thread alone, and the daemon served on half-dead: its memory
//! or disk ungoverned until an OOM or ENOSPC, its VM sockets bound but accepting nothing, its
//! health SERVING, its drain never to come.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::proto::GrantRequest;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

/// What has a service start in a daemon, besides booting it: its environment, and what is done
/// once the daemon serves.
struct Service {
    /// As the daemon's log names it.
    name: &'static str,
    /// The failpoint that has it panic.
    failpoint: String,
    env: Vec<(&'static str, String)>,
    /// Done once the daemon serves VM processes, as some services start only then.
    then: fn(&Path),
}

impl Service {
    /// A service panicking as it starts, through `supervised`'s failpoint.
    fn starting(name: &'static str, env: Vec<(&'static str, String)>, then: fn(&Path)) -> Self {
        Self { name, failpoint: format!("cladefsd::service::{name}"), env, then }
    }
}

fn control(dir: &Path) -> (&'static str, String) {
    ("CLADEFSD_GRPC", dir.join("control.sock").display().to_string())
}

fn nothing(_: &Path) {}

/// An address on localhost no one listens at.
fn free_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

/// A daemon in `dir` whose service `service` panics: how it exited, and what it logged. Fails the
/// test if it lives on past 30 s.
fn panicking(dir: &Path, service: &Service) -> (ExitStatus, String) {
    let socket = dir.join("cladefsd.sock");
    let bucket = dir.join("bucket");
    let disk = dir.join("disk0");
    std::fs::create_dir(&bucket).unwrap();
    std::fs::create_dir(&disk).unwrap();
    let log = dir.join("log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(&socket)
        .env("CLADEFSD_BUCKET", format!("file://{}", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_ROOTS", &disk)
        .env("FAILPOINTS", format!("{}=panic", service.failpoint))
        .envs(service.env.iter().map(|(name, value)| (name, value)))
        .stderr(std::fs::File::create(&log).unwrap());
    // Its abort is what is looked for, no crash to keep: no core dump, which a host's
    // systemd-coredump takes seconds to write (board 2944).
    // SAFETY: setrlimit, which allocates nothing, between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            match libc::setrlimit(libc::RLIMIT_CORE, &none) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        })
    };
    let mut daemon = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut served = false;
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            daemon.kill().unwrap();
            daemon.wait().unwrap();
            panic!(
                "{}: the daemon lived on: {}",
                service.name,
                std::fs::read_to_string(&log).unwrap()
            );
        }
        if !served && std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            served = true;
            (service.then)(dir);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, std::fs::read_to_string(&log).unwrap())
}

#[test]
fn a_panic_on_any_service_aborts_the_daemon_naming_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| {
        let dir = root.path().join(name.replace(' ', "-"));
        std::fs::create_dir(&dir).unwrap();
        dir
    };
    let services = [
        ("memory governor", vec![], nothing as fn(&Path)),
        ("disk governor", vec![], nothing),
        ("cladefsd vms", vec![], nothing),
        ("cladefsd signal", vec![], nothing),
        ("cladefsd http", vec![("CLADEFSD_METRICS", free_address())], nothing),
        ("cladefsd stats", vec![], |dir: &Path| {
            // Its first cell has the daemon make its bucket, whose requests the stats watch: the
            // daemon aborting meanwhile, the opening fails, which is what is looked for.
            drop(cladefs::CellClient::connect_new(dir.join("cladefsd.sock"), 0, 1 << 28));
        }),
    ];
    let mut services: Vec<Service> =
        services.into_iter().map(|(name, env, then)| Service::starting(name, env, then)).collect();
    let peers = dir("peers env");
    services.push(Service::starting(
        "cladefsd peers",
        vec![
            ("CLADEFSD_PEER_ADDRESS", free_address()),
            ("CLADEFSD_PART_CACHE", peers.join("cache").display().to_string()),
        ],
        nothing,
    ));
    // board 9414: the replica directory's sweep and the reaping of migrations abandoned, on the
    // bucket runtime, which serving peers starts; and the VM processes' watchdog, which the first
    // VM process to connect starts.
    let replicated = dir("replicas env");
    let serving_peers = |replicas: bool| {
        let mut env = vec![
            ("CLADEFSD_PEER_ADDRESS", free_address()),
            ("CLADEFSD_PART_CACHE", replicated.join("cache").display().to_string()),
        ];
        if replicas {
            env.push(("CLADEFSD_REPLICAS", replicated.join("replicas").display().to_string()));
        }
        env
    };
    services.push(Service::starting("cladefsd sweep", serving_peers(true), nothing));
    services.push(Service::starting("cladefsd reap", serving_peers(false), nothing));
    services.push(Service::starting("cladefsd watchdog", vec![], |dir: &Path| {
        // Its first VM process starts it: the daemon aborting meanwhile, the opening fails, which
        // is what is looked for.
        drop(cladefs::CellClient::connect(dir.join("cladefsd.sock"), dir.join("cell"), 1 << 28));
    }));
    let controlled = dir("control env");
    services.push(Service::starting("cladefsd ctl", vec![control(&controlled)], nothing));
    services.push(Service::starting(
        "cladefsd tenant",
        vec![control(&controlled)],
        |dir: &Path| {
            let client = cladefs::testing::Control::connect(dir.join("control.sock"));
            // The daemon aborting as the tenant's thread starts, the grant fails, which is what is
            // looked for.
            drop(client.map(|client| {
                client
                    .call(async |client| {
                        client.grant(GrantRequest { tenant: 1, uid: None, gid: None }).await
                    })
                    .map(|response| std::path::PathBuf::from(response.socket))
            }));
        },
    ));
    services.push(Service {
        name: "health reporter",
        failpoint: "cladefsd::health_reporter_gone".into(),
        env: vec![control(&controlled)],
        then: nothing,
    });
    for mut service in services {
        let dir = dir(service.name);
        // The control socket in the service's own directory, which `then` looks in.
        for (name, value) in &mut service.env {
            if *name == "CLADEFSD_GRPC" {
                *value = dir.join("control.sock").display().to_string();
            }
        }
        let (status, said) = panicking(&dir, &service);
        assert_eq!(status.signal(), Some(libc::SIGABRT), "{}: {status}: {said}", service.name);
        assert!(
            local_cell::shared::aborted_holding(&said, service.name),
            "{}: {said}",
            service.name
        );
    }
}
