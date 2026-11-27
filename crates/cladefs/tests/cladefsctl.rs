// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests of cladefsctl, cladefs's tool for VMs' disks: offline commands (bucket administration,
//! `import`, `import-memory`, read-only `stat`) without a daemon, and live-cell / control commands
//! through an extent daemon's control API (TASK-43.5, board 11705, hotbed question 542).

use cladefs::{ExtentId, VolatileSliceMut};
use cladefs::{OpenCell, VolatileSlice};
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

/// The environment of a host of the multi-node system, as tests have it: a bucket in each
/// process's memory, and placement 1.
const HOST: [(&str, &str); 2] = [("CLADEFSD_BUCKET", "memory:///cells"), ("CLADEFSD_TOKEN", "1")];

/// cladefsctl, acting on cells itself, with `env` in its environment, or through a daemon,
/// `cladefsd`, of its own.
struct Cladefsctl {
    /// The daemon's control socket, and the daemon, which goes with this.
    daemon: Option<(PathBuf, KillOnDrop)>,
    env: Vec<(&'static str, String)>,
}

impl Cladefsctl {
    /// cladefsctl acting offline, as a host whose bucket is in its process's memory, of
    /// placement 1.
    fn local() -> Self {
        Self::with_env(&HOST)
    }

    /// cladefsctl acting offline, with `env`, and nothing else of its kind, in its environment.
    fn with_env(env: &[(&'static str, &str)]) -> Self {
        let env = env.iter().map(|&(name, value)| (name, value.to_owned())).collect();
        Self { daemon: None, env }
    }

    /// cladefsctl acting through a daemon of its own, serving in `dir`, a host as tests have it
    /// ([`HOST`]), with `env` in its environment too.
    fn through_daemon(dir: &Path, env: &[(&str, &str)]) -> Self {
        Self { daemon: Some(daemon(dir, env, std::process::Stdio::inherit())), env: Vec::new() }
    }

    /// Runs cladefsctl with `args`: what it prints, or, if it fails, what it says.
    fn run(&self, args: &[&Path]) -> Result<String, String> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsctl"));
        command.args(args);
        for name in ["CLADEFSD_GRPC", "CLADEFSD_BUCKET", "CLADEFSD_TOKEN", "CLADEFS_CONFIG"] {
            command.env_remove(name);
        }
        if let Some((control, _)) = &self.daemon {
            command.env("CLADEFSD_GRPC", control);
        }
        command.envs(self.env.iter().map(|(name, value)| (name, value)));
        let output = command.output().expect("running cladefsctl failed");
        match output.status.success() {
            true => Ok(String::from_utf8(output.stdout).unwrap()),
            false => Err(String::from_utf8_lossy(&output.stderr).into_owned()),
        }
    }
}

/// An extent daemon serving in `dir`, a host as tests have it ([`HOST`]), with `env` in its
/// environment too, and its standard error `stderr`, once it serves its control API and VM
/// processes: the control socket, and the daemon, which goes with this. Each socket waited for: on
/// macOS a test's VM found no `vms.sock` yet once the control socket served.
fn daemon(dir: &Path, env: &[(&str, &str)], stderr: std::process::Stdio) -> (PathBuf, KillOnDrop) {
    use std::time::{Duration, Instant};
    let disk = dir.join("disk0");
    std::fs::create_dir_all(&disk).unwrap();
    let control = dir.join("control.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(dir.join("vms.sock"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_ROOTS", &disk)
        .stderr(stderr);
    let child = command.envs(HOST).envs(env.iter().copied()).spawn().unwrap();
    let child = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    for socket in [&control, &dir.join("vms.sock")] {
        while std::os::unix::net::UnixStream::connect(socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon never served at {}", socket.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    (control, child)
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// TASK-154: `cladefsctl drop` deletes what `take` made, a copy of a cell this host holds nothing
/// of, and a second finds nothing there.
#[test]
fn copies_are_dropped_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let capacity = Path::new("16777216");
    let cell = cladefsctl.run(&[Path::new("create"), capacity]).unwrap();
    let cell = Path::new(cell.trim());
    cladefsctl.run(&[Path::new("drop"), cell]).unwrap();
    assert!(cladefsctl.run(&[Path::new("drop"), cell]).is_err(), "a second drop found a cell");
    cladefsctl.run(&[Path::new("take"), capacity, cell, Path::new("2")]).unwrap();
    cladefsctl.run(&[Path::new("drop"), cell]).unwrap();
    assert!(cladefsctl.run(&[Path::new("drop"), cell]).is_err(), "a second drop found a cell");
}

/// TASK-395.7: `cladefsctl share` / `unshare` / `drop-versions` manage a cell's versions through
/// the daemon.
#[test]
fn versions_are_shared_and_unshared_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = format!("file://{}", dir.path().join("bucket").display());
    std::fs::create_dir(dir.path().join("bucket")).unwrap();
    let cladefsctl =
        Cladefsctl::through_daemon(dir.path(), &[("CLADEFSD_BUCKET", bucket.as_str())]);
    let base = cladefsctl.run(&[Path::new("create"), Path::new("16777216")]).unwrap();
    let base = Path::new(base.trim());
    let printed = cladefsctl.run(&[Path::new("version"), base]).unwrap();
    let version = printed.trim().to_owned();
    let listed = || cladefsctl.run(&[Path::new("versions"), base]).unwrap();
    assert_eq!(listed(), format!("{version} uploaded\n"));
    let id = Path::new(&version);
    cladefsctl.run(&[Path::new("share"), base, id]).unwrap();
    assert_eq!(listed(), format!("{version} uploaded shared\n"));
    cladefsctl.run(&[Path::new("unshare"), base, id]).unwrap();
    assert_eq!(listed(), format!("{version} uploaded\n"));
    cladefsctl.run(&[Path::new("drop-versions"), base, id]).unwrap();
    assert_eq!(listed(), "");
}

/// TASK-39.4: `cladefsctl delete` deletes a cell whose fork lives: it in its bucket and its
/// directory, the fork living on, a cell of its own, reading what it inherited in the bucket
/// (TASK-165.7). Before, the cell directory stayed, holding what the fork shared of it.
#[test]
fn cells_are_deleted_while_their_forks_live_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let vms = dir.path().join("vms.sock");
    let vm = cladefs::CellClient::connect_new(&vms, 0, 1 << 24).unwrap();
    let base = cladefs::CellId { tenant: 0, id: vm.cell() };
    let extent = ExtentId { cell: base.id, sequence: 1 };
    vm.write(extent, 0, &[VolatileSlice::from(&[7; BLOCK][..])]).unwrap();
    vm.commit(None).unwrap();
    drop(vm);
    let base_str = base.to_string();
    let printed = cladefsctl.run(&[Path::new("fork"), Path::new(&base_str)]).unwrap();
    let fork: cladefs::CellId = printed.trim().parse().unwrap();
    let fork_str = fork.to_string();

    cladefsctl.run(&[Path::new("delete"), Path::new(&base_str)]).unwrap();
    let forked = cladefs::CellClient::connect_cell(&vms, fork).unwrap();
    assert_eq!(forked.cell(), fork.id);
    let mut got = vec![0; BLOCK];
    forked.read(extent, 0, &mut [VolatileSliceMut::from(&mut got[..])]).unwrap();
    assert_eq!(got, [7; BLOCK]);
    let own = ExtentId { cell: fork.id, sequence: 1 };
    forked.write(own, 0, &[VolatileSlice::from(&[9; BLOCK][..])]).unwrap();
    forked.commit(None).unwrap();
    drop(forked);
    let stat = cladefsctl.run(&[Path::new("stat"), Path::new(&fork_str)]).unwrap();
    assert!(stat.contains(&format!("used {}\n", 2 * BLOCK)), "{stat}");
    assert!(stat.contains(&format!("own {BLOCK}\ninherited {BLOCK}\n")), "{stat}");
    assert!(
        cladefsctl.run(&[Path::new("delete"), Path::new(&base_str)]).is_err(),
        "deleted nothing again"
    );
    let forked = cladefs::CellClient::connect_cell(&vms, fork).unwrap();
    let mut got = vec![0; BLOCK];
    forked.read(extent, 0, &mut [VolatileSliceMut::from(&mut got[..])]).unwrap();
    assert_eq!(got, [7; BLOCK]);
    drop(forked);
    // Wait briefly for daemon to observe VM socket close before deleting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cladefsctl.run(&[Path::new("delete"), Path::new(&fork_str)]).is_err() {
        assert!(std::time::Instant::now() < deadline, "fork delete never succeeded");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// TASK-454: `cladefsctl stat` prints how many extents a cell holds, own and inherited, with their
/// bytes: of a new cell, none; of one written, its own, an extent written again or grown counted
/// once; of its fork, all inherited; of the fork written and freed in, what it wrote and kept and
/// what it kept of what it inherited; the cell forked from as before.
#[test]
fn stat_counts_extents_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let vms = dir.path().join("vms.sock");
    let held = |cell: cladefs::CellId| {
        let cell_str = cell.to_string();
        let stat = cladefsctl.run(&[Path::new("stat"), Path::new(&cell_str)]).unwrap();
        let named = ["own", "inherited", "own_extents", "inherited_extents"];
        let value = |name: &str| -> u64 {
            let line = stat.lines().find_map(|line| line.strip_prefix(&format!("{name} ")));
            line.unwrap_or_else(|| panic!("no {name} in {stat}")).parse().unwrap()
        };
        named.map(value)
    };
    let block = BLOCK as u64;
    let base: cladefs::CellId = cladefsctl
        .run(&[Path::new("create"), Path::new("16777216")])
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(held(base), [0, 0, 0, 0]);
    let vm = cladefs::CellClient::connect_cell(&vms, base).unwrap();
    for sequence in 1..=3 {
        let extent = ExtentId { cell: base.id, sequence };
        vm.write(extent, 0, &[VolatileSlice::from(&[sequence as u8; BLOCK][..])]).unwrap();
    }
    vm.write(ExtentId { cell: base.id, sequence: 1 }, 0, &[VolatileSlice::from(&[9; BLOCK][..])])
        .unwrap();
    vm.write(
        ExtentId { cell: base.id, sequence: 2 },
        block,
        &[VolatileSlice::from(&[9; BLOCK][..])],
    )
    .unwrap();
    vm.commit(None).unwrap();
    assert_eq!(held(base), [4 * block, 0, 3, 0]);
    drop(vm);

    let base_str = base.to_string();
    let fork: cladefs::CellId =
        cladefsctl.run(&[Path::new("fork"), Path::new(&base_str)]).unwrap().trim().parse().unwrap();
    assert_eq!(held(fork), [0, 4 * block, 0, 3]);
    let vm = cladefs::CellClient::connect_cell(&vms, fork).unwrap();
    for sequence in 1..=2 {
        let extent = ExtentId { cell: fork.id, sequence };
        vm.write(extent, 0, &[VolatileSlice::from(&[7; BLOCK][..])]).unwrap();
    }
    vm.free(ExtentId { cell: base.id, sequence: 2 }).unwrap();
    vm.free(ExtentId { cell: fork.id, sequence: 2 }).unwrap();
    vm.commit(None).unwrap();
    assert_eq!(held(fork), [block, 2 * block, 1, 2]);
    assert_eq!(held(base), [4 * block, 0, 3, 0]);
    drop(vm);
}

/// `cladefsctl stat` says what a cell holds, through the daemon, which owns its host's cells:
/// offline once (board 11705), the daemon's alone since (board 14305).
#[test]
fn stat_says_what_a_cell_holds() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    // The VM socket, which the daemon binds last, once its control socket serves.
    let vms = dir.path().join("vms.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !vms.exists() {
        assert!(std::time::Instant::now() < deadline, "the daemon never bound its VM socket");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let vm = cladefs::CellClient::connect_new(&vms, 0, 1 << 24).unwrap();
    let cell = cladefs::CellId { tenant: 0, id: vm.cell() }.to_string();
    let extent = cladefs::ExtentId { cell: vm.cell(), sequence: 1 };
    vm.write(extent, 0, &[VolatileSlice::from(&[5; BLOCK][..])]).unwrap();
    vm.commit(None).unwrap();
    drop(vm);
    let stat = cladefsctl.run(&[Path::new("stat"), Path::new(&cell)]).unwrap();
    assert!(stat.contains(&format!("own {BLOCK}\ninherited 0\n")), "{stat}");
    assert!(stat.contains("own_extents 1\ninherited_extents 0\n"), "{stat}");
}

/// cladefsctl changes no cell another process has open: through the daemon, it refuses (`EBUSY`).
#[test]
fn cells_open_elsewhere_are_left_alone_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let vm = cladefs::CellClient::connect_new(dir.path().join("vms.sock"), 0, 1 << 24).unwrap();
    let cell = cladefs::CellId { tenant: 0, id: vm.cell() }.to_string();
    let error =
        cladefsctl.run(&[Path::new("delete"), Path::new(&cell)]).expect_err("deleted an open cell");
    assert!(error.contains("EBUSY (errno 16)"), "{error}");
    drop(vm);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cladefsctl.run(&[Path::new("delete"), Path::new(&cell)]).is_err() {
        assert!(std::time::Instant::now() < deadline, "delete never succeeded after close");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// TASK-633.9: `cladefsctl format-bucket` makes a bucket the root of buckets of its own, number 1;
/// `add-bucket` gives each bucket added the next number, which the cells made in it carry in their
/// IDs' high bits, and records every bucket, with its details, in every bucket's root, which
/// `buckets` prints. Adding a bucket again keeps its number; a bucket already formatted is no
/// bucket to add (`EEXIST`), and a bucket not the root adds none (`EINVAL`).
#[test]
fn buckets_are_added_in_turn_and_recorded_in_every_root() {
    use cladefs::import::testing::{Entry, Kind, layer};
    let dir = tempfile::tempdir().unwrap();
    let url = |name: &str| {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        format!("file://{}", dir.path().join(name).display())
    };
    let (root, east, west, other) = (url("root"), url("east"), url("west"), url("other"));
    let cladefsctl = Cladefsctl::with_env(&[("CLADEFSD_BUCKET", &east), ("CLADEFSD_TOKEN", "1")]);
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args)
    };
    let root_details = ["--region", "home", "--master-key-id", "mk1"];
    let formatted = run(&[&["format-bucket", root.as_str()][..], &root_details].concat());
    assert_eq!(formatted, Ok("1\n".to_owned()));
    let east_details = ["--region", "us-east-1", "--kms", "aws-kms://us-east-1"];
    let added = run(&[&["add-bucket", root.as_str(), east.as_str()][..], &east_details].concat());
    assert_eq!(added, Ok("2\n".to_owned()));
    let added =
        run(&["add-bucket", &root, &west, "--region", "eu-west-1", "--endpoint", "https://e"]);
    assert_eq!(added, Ok("3\n".to_owned()));
    assert_eq!(run(&["add-bucket", &root, &east]), Ok("2\n".to_owned()));
    run(&["format-bucket", &other]).unwrap();
    let refused = run(&["add-bucket", &root, &other]).unwrap_err();
    assert!(refused.contains("the bucket is bucket 1 already"), "{refused}");
    assert!(refused.contains("EEXIST (errno 17)"), "{refused}");
    let refused = run(&["add-bucket", &root, "memory:///nowhere"]).unwrap_err();
    assert!(refused.contains("a memory bucket is one process's"), "{refused}");
    assert!(refused.contains("EINVAL (errno 22)"), "{refused}");
    let refused = run(&["add-bucket", &east, &url("more")]).unwrap_err();
    assert!(refused.contains("bucket 2 is not the root bucket"), "{refused}");
    assert!(refused.contains("EINVAL (errno 22)"), "{refused}");
    let expected = format!(
        "version 3\n\
         1 provider=file region=home url={root} master_key_id=mk1\n\
         2 provider=file region=us-east-1 url={east} kms=aws-kms://us-east-1\n\
         3 provider=file region=eu-west-1 url={west} endpoint=https://e\n"
    );
    for bucket in [&root, &east, &west] {
        assert_eq!(run(&["buckets", bucket]), Ok(expected.clone()), "{bucket}");
    }
    // Bucket commands still use their own synchronous entry point with a control socket set.
    let socket = dir.path().join("unused-control.sock");
    let with_socket = Cladefsctl::with_env(&[("CLADEFSD_GRPC", socket.to_str().unwrap())]);
    assert_eq!(with_socket.run(&[Path::new("buckets"), Path::new(&root)]), Ok(expected));
    // A cell imported into `east`, through a daemon whose bucket it is, carries bucket 2 in its
    // ID's high bits.
    {
        let image = dir.path().join("layer.tar");
        std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(b"hi\n"))]).unwrap())
            .unwrap();
        let host = dir.path().join("host");
        std::fs::create_dir(&host).unwrap();
        let daemon = Cladefsctl::through_daemon(&host, &[("CLADEFSD_BUCKET", &east)]);
        let imported = daemon.run(&[Path::new("import"), Path::new("67108864"), &image]).unwrap();
        let cell: cladefs::CellId = imported.trim().parse().unwrap();
        assert_eq!(cell.id >> 48, 2, "{cell}");
    }
}

/// `cladefsctl import` builds a base image into a cell of the daemon's, the daemon owning its
/// host's cells (board 14305; offline, into a directory, before): its filesystem mounts and reads
/// back cleanly, in the daemon's process.
#[test]
fn images_imported_mount_and_read_back() {
    use cladefs::import::testing::{Entry, Kind, layer};
    use cladefs::{Filesystem, Storage};
    use futures::executor::block_on;
    let dir = tempfile::tempdir().unwrap();
    let daemon = common::daemon::Daemon::new(dir.path());
    let control = dir.path().join("control.sock");
    let cladefsctl = Cladefsctl::with_env(&[("CLADEFSD_GRPC", control.to_str().unwrap())]);
    let image = dir.path().join("layer.tar");
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(b"hello\n"))]).unwrap()).unwrap();
    let imported = cladefsctl.run(&[Path::new("import"), Path::new("67108864"), &image]).unwrap();
    let cell: cladefs::CellId = imported.trim().parse().unwrap();
    let stat = cladefsctl.run(&[Path::new("stat"), Path::new(imported.trim())]).unwrap();
    assert!(stat.contains("capacity 67108864\n"), "{stat}");
    let base = daemon.cell_path(cell);
    let read = block_on(async {
        let fs = Filesystem::mount(Storage::open(&base).await.unwrap()).await.unwrap();
        let (file, _) = fs.root().lookup(b"hello").await.unwrap();
        let mut read = vec![0; 16];
        let len = file.read(0, &mut read).await.unwrap();
        read.truncate(len);
        drop(file);
        fs.close().await.unwrap();
        read
    });
    assert_eq!(read, b"hello\n");
}

/// Every command through the daemon: a cell imported, made a public base, grown, forked, synced
/// to object storage, its fork dropped locally, taken back placed after it, and deleted.
#[tokio::test(flavor = "multi_thread")]
async fn commands_through_the_daemon() {
    use cladefs::import::testing::{Entry, Kind, layer};
    let dir = tempfile::tempdir().unwrap();
    let bucket = format!("file://{}/", dir.path().canonicalize().unwrap().join("bucket").display());
    let env = [("CLADEFSD_BUCKET", bucket.as_str()), ("CLADEFSD_TOKEN", "1")];
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &env);
    let image = dir.path().join("layer.tar");
    std::fs::write(&image, layer(&[Entry::new("hello", Kind::File(b"hello\n"))]).unwrap()).unwrap();
    let run = |args: &[&dyn AsRef<Path>]| {
        let args: Vec<&Path> = args.iter().map(|arg| arg.as_ref()).collect();
        cladefsctl.run(&args).unwrap()
    };

    let base_str = run(&[&"import", &"67108864", &image]).trim().to_owned();
    let base_id: cladefs::CellId = base_str.parse().unwrap();
    run(&[&"public", &base_str.as_str()]);
    run(&[&"resize", &base_str.as_str(), &"134217728"]);
    let fork_str = run(&[&"fork", &base_str.as_str()]).trim().to_owned();
    let fork_id: cladefs::CellId = fork_str.parse().unwrap();
    run(&[&"sync", &fork_str.as_str()]);
    run(&[&"drop", &fork_str.as_str()]);
    run(&[&"take", &"67108864", &fork_str.as_str(), &"2"]);
    run(&[&"place", &fork_str.as_str(), &"3"]);
    run(&[&"delete", &fork_str.as_str()]);
    let fork_dir = dir
        .path()
        .join("disk0/cells")
        .join(base32_id::Padded(fork_id.tenant).to_string())
        .join(base32_id::Padded(fork_id.id).to_string());
    assert!(!fork_dir.exists());

    // As the daemon left the base cell, which serves none of them now.
    drop(cladefsctl);
    let base_dir = dir
        .path()
        .join("disk0/cells")
        .join(base32_id::Padded(base_id.tenant).to_string())
        .join(base32_id::Padded(base_id.id).to_string());
    let base = OpenCell::open(&base_dir).await.unwrap();
    assert_eq!(base.info().unwrap().capacity, 134217728);
    assert!(base.is_public());
}

/// cladefs TASK-111.4: `stats --json` prints the stats of one moment through the daemon: its build
/// and counters, and its VM process's, as its guest answered, with the guest's counters, histograms
/// and build; without `--json`, the same as lines of a path and a value. Without the daemon,
/// refused.
#[test]
fn stats_are_of_one_moment_of_the_daemon_and_its_vms() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let client = cladefs::CellClient::connect_new(dir.path().join("vms.sock"), 0, 1 << 24);
    let client = std::sync::Arc::new(client.unwrap());
    // Its guest answers each ask, off the thread the ask comes on.
    let answering = std::sync::Arc::downgrade(&client);
    client.on_stats_asked(move |ask| {
        let client = answering.clone();
        std::thread::spawn(move || {
            let guest = "seals 3\nseals_hist 0,2,1\ncommit abc\n";
            if let Some(client) = client.upgrade() {
                client.answer_stats(ask, Some(guest)).unwrap();
            }
        });
        true
    });
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args).unwrap()
    };

    let json: serde_json::Value = serde_json::from_str(&run(&["stats", "--json"])).unwrap();
    assert_eq!(json["cladefsd"]["build"], cladefs::BUILD_COMMIT);
    assert_eq!(json["cladefsd"]["gauges"]["connections"], 1);
    assert!(json["taken_unix_ns"].as_u64() > Some(0), "{json}");
    let vm = &json["vms"][0];
    assert_eq!(vm["tenant"], "0");
    assert_eq!(vm["cell"], base32_id::Short(client.cell()).to_string());
    assert_eq!(vm["answered"], true);
    assert_eq!(vm["vmm"]["build"], cladefs::BUILD_COMMIT);
    assert_eq!(vm["vmm"]["counters"]["stage_ops"], 0);
    assert_eq!(vm["window"], serde_json::Value::Null);
    let guest = &vm["guest"];
    assert_eq!(guest["build"], "abc");
    assert_eq!(guest["counters"], serde_json::json!({ "seals": 3 }));
    assert_eq!(guest["histograms"], serde_json::json!({ "seals": [0, 2, 1] }));

    let lines = run(&["stats"]);
    let line = |at: &str| lines.lines().find_map(|line| line.strip_prefix(&format!("{at} ")));
    assert_eq!(line("vms.0.guest.counters.seals"), Some("3"), "{lines}");
    assert_eq!(line("vms.0.guest.histograms.seals.1"), Some("2"), "{lines}");
    assert_eq!(line("cladefsd.build"), Some(cladefs::BUILD_COMMIT), "{lines}");

    // board 8581: what the caller leaves out, as a host of many VMs needs.
    let json = |args: &[&str]| -> serde_json::Value { serde_json::from_str(&run(args)).unwrap() };
    let without = json(&["stats", "--json", "--leave-out", "histograms", "--wait", "5"]);
    assert_eq!(without["vms"][0]["guest"]["counters"], serde_json::json!({ "seals": 3 }));
    assert_eq!(without["vms"][0]["guest"]["histograms"], serde_json::json!({}));
    let without = json(&["stats", "--leave-out", "guests", "--json"]);
    assert_eq!(without["vms"][0]["guest"], serde_json::Value::Null);
    assert_eq!(without["vms"][0]["answered"], true);
    let without = json(&["stats", "--json", "--leave-out", "vms"]);
    assert_eq!(without["vms"], serde_json::json!([]));
    assert_eq!(without["cladefsd"]["gauges"]["connections"], 1);
    for wrong in [["stats", "--leave-out", "cells"], ["stats", "--wait", "0"]] {
        let wrong: Vec<&Path> = wrong.iter().map(Path::new).collect();
        assert!(cladefsctl.run(&wrong).is_err(), "{wrong:?} taken");
    }

    drop(client);
    let refused = Cladefsctl::local().run(&[Path::new("stats"), Path::new("--json")]);
    assert!(refused.unwrap_err().contains("CLADEFSD_GRPC"));
}

/// What `child`, whose standard error is piped, writes there once sent SIGQUIT (TASK-44.13): its
/// threads' stacks, from their heading to the first line after it naming `waiting`, a function a
/// thread of it waits in. Its standard error is read to its end, so that its writes never fail.
#[cfg(target_os = "linux")]
fn stacks_on_sigquit(child: &mut std::process::Child, waiting: &str) -> String {
    use std::io::BufRead;
    use std::time::{Duration, Instant};
    const HEADING: &str = "cladefs: the threads' stacks, on SIGQUIT:";
    let stderr = child.stderr.take().expect("its standard error, piped");
    let (sent, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            // Once the test has what it needs, nothing receives the rest.
            let _ = sent.send(line);
        }
    });
    // SAFETY: Signals a process this test started.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGQUIT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut printed, mut heading) = (String::new(), None);
    loop {
        let line = match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => line,
            Err(err) => panic!("no stack naming {waiting} ({err}); it wrote:\n{printed}"),
        };
        // cladefs: a record of the log, after its time and level (board 8567).
        // cladefs: eu-stack killed past its bound, which no waiting would see past (board 8567).
        assert!(!line.contains("killed: no stacks"), "{line}\n{printed}");
        if heading.is_none() && line.contains(HEADING) {
            heading = Some(printed.len());
        }
        printed += &line;
        printed.push('\n');
        if let Some(heading) = heading.filter(|_| line.contains(waiting)) {
            return printed.split_off(heading);
        }
    }
}

/// TASK-44.13: SIGQUIT has the daemon write its threads' stacks, naming where they wait: here, the
/// thread that waits for SIGTERM (and SIGINT and SIGHUP). It goes on running, and SIGTERM still
/// stops it, successfully. cladefs: board 8567: through its log, which, JSON, is a JSON object a
/// line all through them, as it was not, the dump written past the log.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_writes_its_stacks_on_sigquit() {
    let dir = tempfile::tempdir().unwrap();
    let json = [("CLADEFS_LOG_FORMAT", "json")];
    let (_, mut child) = daemon(dir.path(), &json, std::process::Stdio::piped());
    let stacks = stacks_on_sigquit(&mut child.0, "drain_on_signals");
    for line in stacks.lines() {
        let record: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("{err}: a line not JSON: {line}\n{stacks}"));
        assert_eq!(record["level"], "WARN", "{line}");
    }
    assert_eq!(child.0.try_wait().unwrap(), None, "SIGQUIT stopped the daemon:\n{stacks}");
    // SAFETY: Signals the daemon this test started.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let status = child.0.wait().unwrap();
    assert!(status.success(), "{status}");
}

/// TASK-44.13: SIGQUIT has cladefsctl write its threads' stacks, naming where they wait, as an
/// import that hung needed (TASK-98): here, its call through a daemon that never answers. It goes
/// on waiting.
#[cfg(target_os = "linux")]
#[test]
fn cladefsctl_writes_its_stacks_on_sigquit() {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    // A control socket that takes connections and never answers them.
    let control = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_cladefsctl"))
        .args(["stat", "0/1"])
        .env("CLADEFSD_GRPC", &control)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    // Connected, so past the first thing it does, handling SIGQUIT, which would end it before.
    let deadline = Instant::now() + Duration::from_secs(30);
    let _connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = child.0.try_wait().unwrap() {
                    let mut said = String::new();
                    child.0.stderr.take().unwrap().read_to_string(&mut said).unwrap();
                    panic!("cladefsctl stopped before it connected, {status}: {said}");
                }
                assert!(Instant::now() < deadline, "cladefsctl never connected");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accepting cladefsctl's connection: {err}"),
        }
    };
    let stacks = stacks_on_sigquit(&mut child.0, "through");
    assert_eq!(child.0.try_wait().unwrap(), None, "SIGQUIT stopped cladefsctl:\n{stacks}");
}

/// TASK-71: `cladefsctl grant` gives a tenant a VM socket of its own, given to the user and group
/// named, and prints its path: its VM processes open cells of that tenant alone. Granted again, it
/// prints it again. `revoke` takes it away: its VM process connected keeps its cell, and no other
/// connects; revoked again, there is no grant to revoke.
#[test]
fn tenants_are_granted_and_revoked() {
    use cladefs::{CellClient, Errno};
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    // SAFETY: Reads the process's own ids.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (uid, gid) = (uid.to_string(), gid.to_string());
    let (uid, gid) = (Path::new(&uid), Path::new(&gid));
    let grant = [Path::new("grant"), Path::new("a"), uid, gid];
    let socket = dir.path().join("tenants/a.sock");
    assert_eq!(cladefsctl.run(&grant).unwrap(), format!("{}\n", socket.display()));
    assert_eq!(cladefsctl.run(&grant[..2]).unwrap(), format!("{}\n", socket.display()));
    let vm = CellClient::connect_new(&socket, 10, 1 << 24).unwrap();
    let refused = CellClient::connect_new(&socket, 11, 1 << 24);
    assert_eq!(refused.err().and_then(|err| err.errno()), Some(Errno::EACCES));

    let revoke = [Path::new("revoke"), Path::new("a")];
    assert_eq!(cladefsctl.run(&revoke).unwrap(), "");
    assert!(!socket.exists(), "the revoked tenant's socket stayed");
    assert!(CellClient::connect_new(&socket, 10, 1 << 24).is_err());
    assert_eq!(vm.info().unwrap().capacity, 1 << 24);
    drop(vm);
    let error = cladefsctl.run(&revoke).expect_err("revoked a tenant not granted");
    assert!(error.contains("revoking tenant a"), "{error}");
}

/// TASK-71: tenants' grants are the daemon's: without one, `grant` and `revoke` are refused,
/// saying so.
#[test]
fn grants_need_the_daemon() {
    let cladefsctl = Cladefsctl::local();
    for args in
        [[Path::new("grant"), Path::new("a")].as_slice(), &[Path::new("revoke"), Path::new("a")]]
    {
        let error = cladefsctl.run(args).expect_err("granted without a daemon");
        assert!(error.contains("tenants' grants are the extent daemon's"), "{error}");
    }
}

/// TASK-48: SIGTERM stops the daemon: it exits, successfully. cladefs: board 8567: and so do SIGINT
/// (Ctrl-C) and SIGHUP (its terminal closing), drained as SIGTERM drains it, where their default
/// action killed it, every cell crashed.
#[test]
fn the_daemon_stops_on_sigterm() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let dir = tempfile::tempdir().unwrap();
        let mut cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
        let (_, child) = cladefsctl.daemon.as_mut().unwrap();
        // SAFETY: Signals the daemon this test started.
        assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
        let status = child.0.wait().unwrap();
        assert!(status.success(), "signal {signal}: {status}");
    }
}

/// board 11705, hotbed question 542, board 14305: without `CLADEFSD_GRPC`, every command on a cell
/// refuses, naming `CLADEFSD_GRPC`: imports and stats too, which ran offline before, the daemon
/// owning its host's cells.
#[test]
fn cell_commands_need_the_daemon() {
    let cladefsctl = Cladefsctl::local();
    for args in [
        ["create", "16777216"].as_slice(),
        &["take", "16777216", "0/1"],
        &["import", "67108864", "layer.tar"],
        &["import-memory", "memory.file"],
        &["stat", "0/1"],
        &["place", "0/1", "2"],
        &["sync", "0/1"],
        &["fork", "0/1"],
        &["version", "0/1"],
        &["versions", "0/1"],
        &["drop-versions", "0/1", "1"],
        &["share", "0/1", "1"],
        &["unshare", "0/1", "1"],
        &["fork-version", "0/1", "1"],
        &["delete", "0/1"],
        &["drop", "0/1"],
        &["resize", "0/1", "33554432"],
        &["public", "0/1"],
    ] {
        let paths: Vec<&Path> = args.iter().map(Path::new).collect();
        let error = cladefsctl.run(&paths).expect_err("ran a cell's command without the daemon");
        assert!(
            error.contains("the extent daemon's") && error.contains("CLADEFSD_GRPC"),
            "{args:?}: {error}"
        );
    }
}

/// Commands with more arguments than they take are refused, with the usage.
#[test]
fn commands_with_more_arguments_print_the_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cladefsctl = Cladefsctl::with_env(&[]);
    let image = dir.path().join("layer.tar");
    for args in [
        ["take", "67108864", "0/1f", "2", "3"].as_slice(),
        ["import", "67108864", "layer.tar", "ref", "more"].as_slice(),
    ] {
        let mut all: Vec<&Path> = vec![Path::new(args[0])];
        all.extend(args[1..].iter().map(|arg| match *arg {
            "layer.tar" => image.as_path(),
            arg => Path::new(arg),
        }));
        let error = cladefsctl.run(&all).expect_err("ran with more arguments");
        assert!(error.contains("usage"), "{error}");
        assert!(!path.exists());
    }
}

/// TASK-111.7: `cladefsctl --version` prints the commit it was built of, the library's.
#[test]
fn the_version_is_the_build_commit() {
    let printed = Cladefsctl::with_env(&[]).run(&[Path::new("--version")]).unwrap();
    assert_eq!(printed, format!("cladefsctl {}\n", cladefs::BUILD_COMMIT));
}

/// board 8585: `cells` lists the cells VM processes have open, each with what StatCell says of it,
/// and `counters` the daemon's counters, its bucket's with them; as lines of a path and a value, or
/// with `--json` as one JSON object.
#[test]
fn cells_and_counters_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args).unwrap()
    };
    assert_eq!(run(&["cells"]), "");
    let vm = cladefs::CellClient::connect_new(dir.path().join("vms.sock"), 0, 1 << 24).unwrap();
    let cell_b32 = base32_id::Short(vm.cell()).to_string();

    let lines = run(&["cells"]);
    let line = |at: &str| lines.lines().find_map(|line| line.strip_prefix(&format!("{at} ")));
    assert_eq!(line("cells.0.tenant"), Some("0"), "{lines}");
    assert_eq!(line("cells.0.cell"), Some(cell_b32.as_str()), "{lines}");
    assert_eq!(line("cells.0.disk_id"), Some("disk-0"), "{lines}");
    assert_eq!(line("cells.0.stat.cell"), Some(cell_b32.as_str()), "{lines}");
    assert_eq!(line("cells.0.stat.open"), Some("true"), "{lines}");
    assert_eq!(line("cells.0.stat.capacity"), Some("16777216"), "{lines}");
    assert_eq!(line("cells.0.stat.fsync"), Some("FSYNC_LEVEL_ORDERED"), "{lines}");
    assert_eq!(line("cells.1.cell"), None, "{lines}");
    let json: serde_json::Value = serde_json::from_str(&run(&["cells", "--json"])).unwrap();
    assert_eq!(json["cells"][0]["cell"], cell_b32);
    assert_eq!(json["cells"][0]["disk_id"], "disk-0");
    assert_eq!(json["cells"][0]["stat"]["tenant"], "0");

    let lines = run(&["counters"]);
    let line = |at: &str| lines.lines().find_map(|line| line.strip_prefix(&format!("{at} ")));
    // No guest mapped or faulted.
    assert_eq!(line("maps"), Some("0"), "{lines}");
    assert_eq!(line("faults"), Some("0"), "{lines}");
    let json: serde_json::Value = serde_json::from_str(&run(&["counters", "--json"])).unwrap();
    assert_eq!(json["maps"], 0);
    assert!(json["object_storage"]["requests"]["create"].is_u64(), "{json}");
    drop(vm);
}

/// hotbed question 547: `cladefsctl disks` and `cladefsctl set-disk` inspect and update per-disk
/// caps and placement governance at runtime through the control API.
#[test]
fn disks_and_set_disk_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args)
    };
    let json: serde_json::Value =
        serde_json::from_str(&run(&["disks", "--json"]).unwrap()).unwrap();
    assert_eq!(json["disks"][0]["id"], "disk-0");
    assert_eq!(json["disks"][0]["placement"], "read_write");
    assert_eq!(json["disks"][0]["health"], "online");

    let updated = run(&["set-disk", "disk-0", "--cap", "512MiB", "--placement", "drain"]).unwrap();
    assert!(updated.contains("id disk-0"), "{updated}");
    assert!(updated.contains("placement drain"), "{updated}");
    assert!(updated.contains(&format!("cap_bytes {}", 512u64 << 20)), "{updated}");

    let cleared =
        run(&["set-disk", "disk-0", "--cap", "none", "--placement", "read_write"]).unwrap();
    assert!(cleared.contains("placement read_write"), "{cleared}");
    assert!(cleared.contains("cap_bytes null"), "{cleared}");
}

/// board 8585: the memory's and the disks' calls through cladefsctl, of a tenant granted and of
/// every tenant: what each answers printed, nothing for those answering nothing, and a refusal
/// said with its errno.
#[test]
fn memory_and_disks_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args)
    };
    run(&["grant", "a"]).unwrap();
    let vm = cladefs::CellClient::connect_new(dir.path().join("tenants/a.sock"), 10, 1 << 24);
    let vm = vm.unwrap();

    for args in [
        ["memory-limit", "--tenant", "a", "16777216"].as_slice(),
        &["memory-limit", "--tenant", "a", "none"],
        &["memory-limit", "1073741824"],
        &["memory-limit", "none"],
        &["memory-weight", "a", "3"],
        &["in-flight", "--tenant", "a", "134217728"],
        &["in-flight", "default"],
        &["disk-limit", "--tenant", "a", "1073741824"],
        &["disk-limit", "none"],
        &["disk-weight", "a", "2"],
    ] {
        assert_eq!(run(args), Ok(String::new()), "{args:?}");
    }
    let disk = run(&["tenant-disk", "--tenant", "a"]).unwrap();
    assert!(disk.contains("limit_bytes 1073741824\n"), "{disk}");
    assert!(disk.starts_with("bytes "), "{disk}");
    let json: serde_json::Value =
        serde_json::from_str(&run(&["tenant-disk", "--json"]).unwrap()).unwrap();
    assert_eq!(json["limit_bytes"], serde_json::Value::Null, "{json}");
    let shrunk = run(&["shrink-memory", "--tenant", "a", "0"]).unwrap();
    assert!(shrunk.strip_prefix("bytes ").is_some_and(|bytes| bytes.trim().parse::<u64>().is_ok()));

    // Under a slab (1 MiB), and of a tenant no daemon has.
    let refused = run(&["memory-limit", "4096"]).unwrap_err();
    assert!(refused.contains("limiting the memory of every tenant"), "{refused}");
    assert!(refused.contains("EINVAL"), "{refused}");
    let refused = run(&["tenant-disk", "--tenant", "b"]).unwrap_err();
    assert!(refused.contains("counting the disk of tenant b"), "{refused}");
    assert!(refused.contains("ENOENT"), "{refused}");
    for wrong in [
        ["memory-limit", "--tenant", "a"].as_slice(),
        &["memory-weight", "a", "heavy"],
        &["in-flight", "none"],
        &["tenant-disk", "--tenant"],
    ] {
        assert!(run(wrong).is_err(), "{wrong:?} taken");
    }
    drop(vm);
}

/// board 8585: `set-peers`, `migrate-ephemeral` and `hand-over-ephemeral` reach the daemon's calls,
/// which say what they refuse: no peers, a cell no migration's, a migration of none; board 8577:
/// `abort-ephemeral-migration` too.
#[test]
fn peers_and_migrations_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let address = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let cache = dir.path().join("cache");
    let env = [
        ("CLADEFSD_PEER_ADDRESS", address.as_str()),
        ("CLADEFSD_PART_CACHE", cache.to_str().unwrap()),
    ];
    let cladefsctl = Cladefsctl::through_daemon(dir.path(), &env);
    let run = |args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        cladefsctl.run(&args)
    };
    assert_eq!(run(&["set-peers", &address, "127.0.0.1:1"]), Ok(String::new()));
    assert_eq!(run(&["set-peers"]), Ok(String::new()));
    let refused = run(&["set-peers", "nowhere"]).unwrap_err();
    assert!(refused.contains("setting the daemon's peers"), "{refused}");
    assert!(refused.contains("EINVAL"), "{refused}");

    let cell = run(&["create", "16777216"]).unwrap().trim().to_owned();
    let refused = run(&["migrate-ephemeral", &cell, "127.0.0.1:1"]).unwrap_err();
    assert!(refused.contains(&format!("migrating cell {cell}")), "{refused}");
    assert!(refused.contains("ENOTSUP"), "{refused}");
    let refused = run(&["hand-over-ephemeral", &cell]).unwrap_err();
    assert!(refused.contains(&format!("handing cell {cell} over")), "{refused}");
    assert!(refused.contains("ENOTSUP"), "{refused}");
    let refused = run(&["abort-ephemeral-migration", &cell]).unwrap_err();
    let aborting = format!("aborting the migration of cell {cell}");
    assert!(refused.contains(&aborting), "{refused}");
    // Peer migration RPCs serve ephemeral cells only (board 14467): bucket-backed cells are
    // refused with ENOTSUP.
    assert!(refused.contains("ENOTSUP"), "{refused}");
}

/// board 8585: `--timeout`, or CLADEFSCTL_TIMEOUT, bounds a call the daemon holds, here a GetStats
/// waiting for a guest that never answers: it fails in about the time given, `ETIMEDOUT`, saying
/// the daemon may still complete it.
#[test]
fn calls_end_at_their_timeouts() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let mut cladefsctl = Cladefsctl::through_daemon(dir.path(), &[]);
    let vm = cladefs::CellClient::connect_new(dir.path().join("vms.sock"), 0, 1 << 24).unwrap();
    // Its guest asked, and never answering.
    vm.on_stats_asked(|_| true);
    let timed = |cladefsctl: &Cladefsctl, args: &[&str]| {
        let args: Vec<&Path> = args.iter().map(Path::new).collect();
        let start = Instant::now();
        let refused = cladefsctl.run(&args).unwrap_err();
        (refused, start.elapsed())
    };
    let (refused, took) = timed(&cladefsctl, &["--timeout", "1", "stats", "--wait", "30"]);
    assert!(refused.contains("ETIMEDOUT"), "{refused}");
    assert!(
        refused.contains("no answer within 1s: the extent daemon may still complete"),
        "{refused}"
    );
    assert!(took >= Duration::from_secs(1) && took < Duration::from_secs(10), "{took:?}");

    cladefsctl.env.push(("CLADEFSCTL_TIMEOUT", "1".to_owned()));
    let (refused, took) = timed(&cladefsctl, &["stats", "--wait", "30"]);
    assert!(refused.contains("ETIMEDOUT"), "{refused}");
    assert!(took >= Duration::from_secs(1) && took < Duration::from_secs(10), "{took:?}");
    // A wait under a millisecond is one, not the daemon's default, 2 s, past the timeout
    // (hotbed review 67).
    let args: Vec<&Path> = ["stats", "--json", "--wait", "0.0005"].iter().map(Path::new).collect();
    let json: serde_json::Value = serde_json::from_str(&cladefsctl.run(&args).unwrap()).unwrap();
    assert_eq!(json["vms"][0]["answered"], false, "{json}");
    // A timeout that is none.
    let (refused, _) = timed(&cladefsctl, &["--timeout", "soon", "stats"]);
    assert!(refused.contains("--timeout must be a positive number of seconds"), "{refused}");
    drop(vm);
}

/// Mount calls use the same deadline options as the rest of the control API.
#[cfg(target_os = "linux")]
#[test]
fn mount_calls_end_at_their_timeouts() {
    use cladefs::fuse::control::proto::{
        AttachRequest, AttachResponse, DetachRequest, DetachResponse, ListRequest, ListResponse,
        mount_server::{Mount, MountServer},
    };
    use tonic::{Request, Response, Status};

    struct SlowMount;

    #[tonic::async_trait]
    impl Mount for SlowMount {
        async fn attach(
            &self,
            _: Request<AttachRequest>,
        ) -> Result<Response<AttachResponse>, Status> {
            unreachable!("the test only lists mounts")
        }

        async fn detach(
            &self,
            _: Request<DetachRequest>,
        ) -> Result<Response<DetachResponse>, Status> {
            unreachable!("the test only lists mounts")
        }

        async fn list(&self, _: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            Ok(Response::new(ListResponse { cells: Vec::new() }))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            tonic::transport::Server::builder()
                .add_service(MountServer::new(SlowMount))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::UnixListenerStream::new(listener),
                    async {
                        stopped.await.unwrap();
                    },
                )
                .await
                .unwrap();
        });
    });
    let mut cladefsctl = Cladefsctl::with_env(&[
        ("CLADEFSD_GRPC", socket.to_str().unwrap()),
        ("CLADEFSCTL_TIMEOUT", ""),
    ]);
    let flag = cladefsctl.run(&["--timeout", "1", "mount", "list"].map(Path::new));
    cladefsctl.env.push(("CLADEFSCTL_TIMEOUT", "1".to_owned()));
    let env = cladefsctl.run(&["mount", "list"].map(Path::new));
    shutdown.send(()).unwrap();
    server.join().unwrap();
    for result in [flag, env] {
        let refused = result.unwrap_err();
        assert!(refused.contains("listing the cells"), "{refused}");
        assert!(refused.contains("ETIMEDOUT"), "{refused}");
    }
}

/// board 8585: the control API's calls cladefsctl makes are the daemon's: without one, refused,
/// saying so.
#[test]
fn calls_of_the_daemon_need_it() {
    let cladefsctl = Cladefsctl::local();
    for args in [
        ["cells"].as_slice(),
        &["disks"],
        &["set-disk", "disk-0", "--placement", "drain"],
        &["counters", "--json"],
        &["set-peers", "a:1"],
        &["migrate-ephemeral", "0/1", "a:1"],
        &["hand-over-ephemeral", "0/1"],
        &["wait-ephemeral-hand-over", "0/1"],
        &["abort-ephemeral-migration", "0/1"],
        &["memory-limit", "none"],
        &["memory-weight", "a", "1"],
        &["in-flight", "default"],
        &["shrink-memory", "0"],
        &["disk-limit", "none"],
        &["disk-weight", "a", "1"],
        &["tenant-disk"],
    ] {
        let paths: Vec<&Path> = args.iter().map(Path::new).collect();
        let error = cladefsctl.run(&paths).expect_err("called without a daemon");
        assert!(
            error.contains("the extent daemon's: name its control socket (CLADEFSD_GRPC)"),
            "{error}"
        );
    }
}

/// TASK-45: the daemon refuses to start without the bucket and its placement token, saying why.
#[test]
fn the_daemon_needs_a_bucket_and_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let start = |env: &[(&str, &str)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
        command.arg(dir.path().join("vms.sock"));
        for name in ["CLADEFSD_BUCKET", "CLADEFSD_TOKEN"] {
            command.env_remove(name);
        }
        let output = command.envs(env.iter().copied()).output().unwrap();
        assert!(!output.status.success(), "the daemon started");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let error = start(&[]);
    assert!(error.contains("CLADEFSD_BUCKET names no bucket"), "{error}");
    let error = start(&[("CLADEFSD_BUCKET", "memory:///cells")]);
    assert!(error.contains("CLADEFSD_TOKEN names no placement token"), "{error}");
    let error = start(&[("CLADEFSD_BUCKET", "memory:///b"), ("CLADEFSD_TOKEN", "0")]);
    assert!(error.contains("CLADEFSD_TOKEN names no placement token"), "{error}");
    assert!(!dir.path().join("vms.sock").exists(), "it listened");
}

/// The standard error of a daemon that refused to start in `dir`, a host as tests have it
/// ([`HOST`]), with `env`, and `limit` open files at most if given: it exits, failing, having bound
/// no socket.
fn refused_to_start(dir: &Path, env: &[(&str, &str)], limit: Option<u64>) -> String {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command.arg(dir.join("vms.sock")).env("CLADEFSD_GRPC", dir.join("control.sock"));
    // The disk it owns, `dir`, unless `env` names others (board 14305).
    command.env("CLADEFSD_ROOTS", dir).envs(HOST).envs(env.iter().copied());
    if let Some(limit) = limit {
        // SAFETY: Between fork and exec, a system call alone, async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                match libc::setrlimit(libc::RLIMIT_NOFILE, &limit) {
                    0 => Ok(()),
                    _ => Err(std::io::Error::last_os_error()),
                }
            });
        }
    }
    let child = command.stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut child = KillOnDrop(child);
    // One that started serves until killed: a deadline, not a hang (killed as this drops).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "the daemon started, and served");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(!status.success(), "the daemon started");
    assert!(!dir.join("vms.sock").exists(), "it listened");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut stderr).unwrap();
    stderr
}

/// board 8595: a root of CLADEFSD_ROOTS that is no directory stops the daemon at its start, naming
/// it: it was dropped, its paths refused to every VM while the daemon served.
#[test]
fn the_daemon_refuses_roots_it_cannot_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let (root, missing, file) =
        (dir.path().join("root"), dir.path().join("missing"), dir.path().join("file"));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(&file, b"").unwrap();
    for (bad, said) in [(&missing, "No such file or directory"), (&file, "is no directory")] {
        let roots = std::env::join_paths([&root, bad]).unwrap();
        let error =
            refused_to_start(dir.path(), &[("CLADEFSD_ROOTS", roots.to_str().unwrap())], None);
        assert!(error.contains(&format!("CLADEFSD_ROOTS: {}", bad.display())), "{error}");
        assert!(error.contains(said), "{error}");
    }
}

/// board 8595: a part cache the daemon can't use stops it at its start, naming CLADEFSD_PART_CACHE
/// and why, with no peers too: one on a filesystem that can't bypass the page cache (the
/// `direct::bypass` failpoint), one another daemon holds locked, one under a file; each cell's
/// opening failed EIO while it served.
#[test]
fn the_daemon_refuses_a_part_cache_it_cannot_use() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("first"), dir.path().join("second"));
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let cache = dir.path().join("cache");
    let cache = cache.to_str().unwrap();

    let bypass = [("CLADEFSD_PART_CACHE", cache), ("FAILPOINTS", "direct::bypass=return")];
    let error = refused_to_start(&second, &bypass, None);
    assert!(error.contains(&format!("CLADEFSD_PART_CACHE: {cache}: ")), "{error}");

    let (_control, _running) =
        daemon(&first, &[("CLADEFSD_PART_CACHE", cache)], std::process::Stdio::inherit());
    let error = refused_to_start(&second, &[("CLADEFSD_PART_CACHE", cache)], None);
    assert!(error.contains(&format!("CLADEFSD_PART_CACHE: {cache}: ")), "{error}");

    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let under = file.join("cache");
    let error =
        refused_to_start(&second, &[("CLADEFSD_PART_CACHE", under.to_str().unwrap())], None);
    assert!(error.contains(&format!("CLADEFSD_PART_CACHE: {}: ", under.display())), "{error}");
    assert!(error.contains("Not a directory"), "{error}");
}

/// board 14217: a limit of open files under the daemon's floor is a warning, its doctor's, naming
/// RLIMIT_NOFILE, the floor and the cells it leaves room for, and the daemon starts and serves
/// (the user, 2026-11-26). It refused to start before (board 8595).
#[test]
fn the_daemon_warns_of_too_few_open_files() {
    use std::os::unix::process::CommandExt as _;
    let dir = tempfile::tempdir().unwrap();
    let floor = cladefs::cladefsd::open_files_floor();
    let disk = dir.path().join("disk0");
    std::fs::create_dir_all(&disk).unwrap();
    let control = dir.path().join("control.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command.arg(dir.path().join("vms.sock")).env("CLADEFSD_GRPC", &control);
    command.env("CLADEFSD_ROOTS", &disk).envs(HOST);
    let limit = floor - 1;
    // SAFETY: Between fork and exec, a system call alone, async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
            match libc::setrlimit(libc::RLIMIT_NOFILE, &limit) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
    let mut daemon = KillOnDrop(command.stderr(std::process::Stdio::piped()).spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&control).is_err() {
        assert!(daemon.0.try_wait().unwrap().is_none(), "the daemon exited");
        assert!(std::time::Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut daemon.0.stderr.take().unwrap(), &mut stderr).unwrap();
    // Its own 1024, and each cell's files.
    let floor_cells = cladefs::cladefsd::FLOOR_CELLS;
    let cells = (limit - 1024) / ((floor - 1024) / floor_cells);
    let warned = format!(
        "cladefsd: doctor: open files: RLIMIT_NOFILE is {limit}, under the {floor} for \
         {floor_cells} cells' files at their most: the daemon serves room for {cells}"
    );
    assert!(stderr.contains(&warned), "{stderr}");
}

/// board 14217: `cladefsd --doctor` says what the daemon's doctor finds of the host, a line a
/// check, starting no daemon: with a memory size, as a daemon given one, no governor's check.
#[test]
fn the_doctor_says_what_it_finds_of_the_host() {
    let output = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .args(["--doctor", "1073741824"])
        .output()
        .unwrap();
    let said = String::from_utf8(output.stdout).unwrap();
    // Each line `MARK     CHECK: WHAT IT FOUND`, its mark padded to 8.
    let checks: Vec<&str> =
        (said.lines()).map(|line| line[9..].split_once(": ").unwrap().0).collect();
    assert_eq!(checks, ["page size", "open files", "SO_PEERPIDFD", "vm.max_map_count"], "{said}");
    assert!(said.starts_with("ok       page size: "), "{said}");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
