// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Snapshots of a VM with an extent-store device (cladefs's TASK-39.1): the guest's Fxfs, written
//! to and frozen, is snapshotted, the VM goes, and the snapshot is restored (`Mode`):
//!
//! - **Zygote:** twice, as a zygote's children are, each into a cell of its own, forked from the
//!   snapshot's version; the host then checks each cell holds what its guest wrote, and not the
//!   other's.
//! - **Suspend:** once, into the cell the VM had, which nothing changed meanwhile but its size:
//!   the guest, told of it by nothing, finds the cell grew as it thaws (TASK-79.52).
//! - **Migrate:** once, as on another host: its extent daemon keeps cells in a bucket, and the
//!   restore names a version and a cell directory it doesn't have here, so it takes the version's
//!   cell from the bucket, and forks one there from it. The bucket is CLADEFSD_BUCKET's, if set, or
//! one in the daemon's memory. With CLADEFS_MIGRATE_PHASE, it runs on two hosts (TASK-28.11,
//!   scripts/migrate-host.sh): `source` snapshots the VM, its cell in the bucket, and
//!   leaves the snapshot, its memory and the VM's root filesystem in CLADEFS_MIGRATE_BUNDLE; then
//!   `target`, on another host, which has nothing of the cell, restores it from there, at the
//!   same paths, the root filesystem's too, which the restored VM goes on reading.
//! - **TenantMigrate:** as Migrate, for a tenant's VM (TASK-154): the tenant's VM process names no
//!   cell, so that the host's VM manager makes the version in the tenant's directory first, of the
//!   cell the daemon says it is, and Firecracker restores through the tenant's socket. Several
//!   times at once (TASK-158), each from a copy of the version of its own, as on hosts of their
//!   own, for tenants of their own, and once from the snapshot's own copy, all at the same moment:
//!   each copy a read-only ancestor, which no restore takes from the others.
//!
//! Each restored guest thaws, reads what was written before the snapshot (with a DAX window,
//! through a mapping made before it, which the restored window maps again), writes its own file,
//! which mints extents of its cell, and remounts.
//!
//! cladefs TASK-92.14: before the snapshot, the guest maps a file of `PIECES` pages, each an extent
//! of its own, read in from a mount before, so that the window maps each page apart (with a DAX
//! window), and reads it; frozen, it holds no private page. Restored, the mapping reads the same.
//! The host says how long each restore (`/snapshot/load`) took, which maps the window's mappings
//! again. The mapping is writable, and every eighth page stored to through it before the freeze:
//! frozen, the guest counts no private page nor dirty one, nor does the host hold one, which a
//! snapshot's version requires. KVM's worker, writing for the guest's loads through the mapping,
//! once left the host a private page or two the guest never counted, and the snapshot was refused
//! (TASK-191). Restored, the mapping and the file read what was stored.
//!
//! fxfs-guest-snapshot-volumes(-dax): a VM with `VOLUMES` volumes mounted on one device freezes,
//! suspends, thaws and uses each (TASK-136.2). Mounted first, with nothing in the journal, the
//! default volume reads the device fewer times than there are volumes: the rest stay closed.
//!
//! fxfs-guest-suspend-read-only-volume and fxfs-guest-suspend-remounted-read-only: a volume
//! mounted read-only beside the default, or the default remounted read-only, frozen with the rest,
//! freezes the filesystem, and the VM is snapshotted (`ReadOnly`): the VFS freezes a read-only
//! superblock without a call to its filesystem, which waited for it, and the device refused the
//! snapshot (TASK-202).
//!
//! fxfs-guest-snapshot-memory-cell-dax: as fxfs-guest-snapshot-dax, the snapshot's memory a
//! version of the memory cell the VM booted with (`Memory::Cell`, TASK-456.3, board 14415): its
//! memory that cell's window from its boot, as every VM's is, the snapshot (`memory_cell: {}`)
//! writing its private pages into it, its untouched pages holes; the two VMs restored from the
//! version, each its memory a fork of it, as a copy-on-write window the extent daemon serves,
//! their pages shared but those each writes. The booted VM's guest, before the snapshot, and the
//! second restored VM's write all the memory they have free, each page a private copy, give it
//! back, and the host inflates the balloon over it (free page reporting off): the private pages'
//! memory, the VM process's memory object, gives back at least half of what the balloon took,
//! then the balloon deflates; the restored guest writes the memory again (board 8497: the
//! balloon's `madvise(MADV_DONTNEED)` freed none of it). The booted VM's memory and the restored
//! VMs' take one path (board 14415), checked the same.
//!
//! The VMs run in Firecracker driven through its API. Their root filesystem's disk is read-only,
//! as the restored VMs share it. The guest tells the host it is frozen over vsock, and each
//! restored guest, whose connection the restore resets, asks the host its name on a new one. The
//! host closes its end as soon as it has read that the guest is frozen, so the guest may be
//! connecting again as the snapshot is taken: a connection whose response is in the restored
//! guest's RX queue, which Firecracker resets there too (TASK-277).

use macros::{guest, host};

pub struct TestFxfsGuestSnapshot {
    /// Whether the device has a DAX window.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    pub dax: bool,
    pub mode: Mode,
    /// How many volumes the guest mounts besides the default: on a cell the host makes, if any.
    pub volumes: usize,
    /// What of the guest's is read-only as it freezes.
    pub read_only: ReadOnly,
    /// What the restored VMs' memory is.
    pub memory: Memory,
}

/// What the restored VMs' memory is (TASK-456.3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    /// The snapshot's memory file, mapped privately.
    File,
    /// A fork of a memory cell the memory file was imported into, each restore's.
    Cell,
}

/// What of the guest's is read-only as it freezes (TASK-202).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReadOnly {
    Nothing,
    /// A volume of its own, `RO_VOLUME`, mounted read-only beside the default, on a cell the host
    /// makes with it, which the guest writes to first, mounted writable.
    Volume,
    /// The default volume, remounted read-only before it freezes, and writable once it thaws. The
    /// pieces' writable mapping goes first, mapped again read-only: the file it maps stays open
    /// for writing while it does, which the VFS's read-only remount refuses (`EBUSY`), any
    /// filesystem's, as an integrator's would be (TASK-280).
    Remounted,
}

/// The volume `ReadOnly::Volume` mounts read-only.
const RO_VOLUME: &str = "ro";

/// As many volumes as a sandbox may have mounted (backlog doc-10).
pub const VOLUMES: usize = 256;

/// How the snapshot is restored.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Zygote,
    Suspend,
    Migrate,
    TenantMigrate,
}

impl Mode {
    /// The restored VMs' names, which each writes to its cell.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    fn restores(self) -> &'static [&'static str] {
        match self {
            Self::Zygote => &["a", "b"],
            Self::Suspend => &["resumed"],
            Self::Migrate => &["moved"],
            Self::TenantMigrate => &["moved", "moved-too", "moved-last", "here"],
        }
    }
}

/// Which part of a migration between hosts runs here (CLADEFS_MIGRATE_PHASE), if one does.
#[cfg_attr(feature = "guest", allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The VM, snapshotted, left in the bundle.
    Source,
    /// The snapshot in the bundle, restored.
    Target,
}

#[cfg_attr(feature = "guest", allow(dead_code))]
impl Phase {
    fn of_env() -> Option<Self> {
        match std::env::var("CLADEFS_MIGRATE_PHASE").ok()?.as_str() {
            "source" => Some(Self::Source),
            "target" => Some(Self::Target),
            other => panic!("CLADEFS_MIGRATE_PHASE is source or target, not {other:?}"),
        }
    }
}

/// The cell's capacity, and what the host grows it to meanwhile, resuming.
const CAPACITY: u64 = 128 << 20;
const GROWN: u64 = CAPACITY * 2;

/// The vsock port the guest says it is frozen on, and asks its name on once restored.
#[cfg_attr(feature = "guest", allow(dead_code))]
const PORT: u32 = 5051;
/// The vsock port a guest restored from a memory cell gives back its memory on, which the host
/// inflates the VM's balloon over (board 8497).
#[cfg_attr(feature = "guest", allow(dead_code))]
const BALLOON_PORT: u32 = 5052;
/// The restore whose guest gives back its memory, of a memory cell's (board 8497).
#[cfg_attr(feature = "guest", allow(dead_code))]
const GIVES_BACK: &str = "b";
/// The VM's memory, in MiB.
#[cfg_attr(feature = "guest", allow(dead_code))]
const RAM_MIB: u64 = 512;
/// What the guest writes before the snapshot.
const BEFORE: &[u8] = b"written before the snapshot\n";
/// The file the guest maps, with a DAX window, before the snapshot, and reads after: a pattern.
#[cfg_attr(feature = "host", allow(dead_code))]
const MAPPED_LEN: usize = 64 << 10;

/// The pattern the mapped file holds.
#[cfg_attr(feature = "host", allow(dead_code))]
fn mapped_pattern() -> Vec<u8> {
    (0..MAPPED_LEN).map(|i| (i % 251) as u8).collect()
}

/// The pages of the file whose pages are each an extent of their own (TASK-92.14): many window
/// mappings, as a zygote's are.
#[cfg_attr(feature = "host", allow(dead_code))]
const PIECES: usize = 1024;

/// What the file of `PIECES` pages holds at its page `page`.
#[cfg_attr(feature = "host", allow(dead_code))]
fn piece(page: usize) -> Vec<u8> {
    (0..4096).map(|i| ((page * 7 + i) % 253) as u8).collect()
}

/// What the file of `PIECES` pages holds as written.
#[cfg_attr(feature = "host", allow(dead_code))]
fn pieces() -> Vec<u8> {
    (0..PIECES).flat_map(piece).collect()
}

/// Whether the guest writes to page `page` of the file of `PIECES` pages, through its mapping,
/// before the freeze: every eighth (TASK-191).
#[cfg_attr(feature = "host", allow(dead_code))]
fn piece_stored(page: usize) -> bool {
    page % 8 == 3
}

/// What the guest writes at the start of page `page` of the file of `PIECES` pages.
#[cfg_attr(feature = "host", allow(dead_code))]
fn stored(page: usize) -> [u8; 8] {
    (page as u64 | 0x5a5a_0000_0000_0000).to_le_bytes()
}

/// What the file of `PIECES` pages holds once stored to.
#[cfg_attr(feature = "host", allow(dead_code))]
fn pieces_stored() -> Vec<u8> {
    let mut pieces = pieces();
    for page in (0..PIECES).filter(|&page| piece_stored(page)) {
        pieces[page * 4096..page * 4096 + 8].copy_from_slice(&stored(page));
    }
    pieces
}

#[host]
mod host {
    use super::*;
    use crate::common::{cladefs_guest_should_run, setup_rootfs};
    use crate::vm::{Firecracker, Vm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};
    use anyhow::Context;
    use cladefs::cladefsd::control::proto::{
        GetStatsRequest, GrantRequest, ListCellsRequest, ListVersionsRequest, ResizeCellRequest,
        ShrinkMemoryRequest, StatCellRequest,
    };
    use cladefs::{Filesystem, Storage};
    use futures::executor::block_on;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    /// The memory cell's version a snapshot into a memory cell made, as `PUT /snapshot/create`
    /// answers.
    #[derive(Clone, Copy, Debug, serde::Deserialize)]
    struct MemoryCellSnapshot {
        tenant: u64,
        cell: u64,
        version: u64,
    }

    /// A short path for a vsock's socket, which Unix sockets' 107 bytes hold: the test's
    /// directory may be deep.
    fn uds(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cladefs-snap-{}-{name}.vsock", std::process::id()))
    }

    /// Reads a line from the guest on the vsock port the VM at `uds` forwards to the host.
    fn listen(uds: &Path) -> anyhow::Result<UnixListener> {
        listen_on(uds, PORT)
    }

    /// Listens for the guest's connections on vsock port `port`, which the VM at `uds` forwards
    /// to the host.
    fn listen_on(uds: &Path, port: u32) -> anyhow::Result<UnixListener> {
        let path = format!("{}_{port}", uds.display());
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).with_context(|| format!("binding {path}"))
    }

    /// The bytes of host memory the private pages of the VM process `pid` hold, of its memory of
    /// `size` bytes, a memory cell's window: its window's private memory's blocks, the one memory
    /// object of the process so named of that size (the DAX window's is of another).
    #[cfg(target_os = "linux")]
    fn private_memory_held(pid: u32, size: u64) -> anyhow::Result<u64> {
        use std::os::unix::fs::MetadataExt as _;
        let mut held = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
            let path = entry?.path();
            // A descriptor closed since it was listed: not the private memory's, which stays.
            let Ok(link) = std::fs::read_link(&path) else { continue };
            if !link.to_string_lossy().starts_with("/memfd:cladefs-private-pages") {
                continue;
            }
            let metadata = std::fs::metadata(&path)?;
            if metadata.len() == size {
                held.push(metadata.blocks() * 512);
            }
        }
        anyhow::ensure!(held.len() == 1, "the memory's private memory: {held:?}");
        Ok(held[0])
    }

    /// macOS's: the VM process's footprint (`ri_phys_footprint`), which holds the private pages of
    /// its memory, restored from a memory cell: the guest's RAM's anonymous ones, the guest's
    /// writes and the kernel's copies of the daemon's pages there (`dax::hvf::ram`, board 13994),
    /// with the rest of what the process holds of its own.
    #[cfg(target_os = "macos")]
    fn private_memory_held(pid: u32, _size: u64) -> anyhow::Result<u64> {
        Ok(crate::common::usage(pid)?.ri_phys_footprint)
    }

    /// The guest of `firecracker`, `name`, its memory of `size` bytes a memory cell's window,
    /// gives back the memory it wrote, on `listener`: the host inflates the VM's balloon over it,
    /// and the private pages' memory gives back at least half of what the balloon took, within a
    /// minute; then it deflates the balloon (board 8497).
    fn inflate_over_private_pages(
        firecracker: &mut Firecracker,
        listener: &UnixListener,
        size: u64,
        name: &str,
    ) -> anyhow::Result<()> {
        let stream =
            firecracker.accept_within(listener, Some(std::time::Duration::from_secs(120)))?;
        let mut guest = BufReader::new(stream.try_clone()?);
        let mut told = |expected: &str| -> anyhow::Result<String> {
            let mut said = String::new();
            guest.read_line(&mut said)?;
            let rest = said.strip_prefix(expected).map(str::trim_end).map(str::to_owned);
            rest.with_context(|| format!("the guest said {said:?}, not {expected:?}"))
        };
        let mut tell = stream;
        let written: u64 = told("written ")?.parse()?;
        let held = || private_memory_held(firecracker.pid(), size);
        let before = held()?;
        anyhow::ensure!(
            before >= written << 20,
            "the guest wrote {written} MiB, its private memory holds {before} bytes"
        );
        writeln!(tell, "free")?;
        told("freed")?;
        let amount = written * 3 / 4;
        let start = std::time::Instant::now();
        firecracker.request("PATCH", "/balloon", &serde_json::json!({ "amount_mib": amount }))?;
        let mut after = held()?;
        while before.saturating_sub(after) < (amount << 20) / 2 {
            anyhow::ensure!(
                start.elapsed() < std::time::Duration::from_secs(60),
                "the private memory held {before} bytes, {after} a minute after the balloon \
                 took {amount} MiB: the balloon reclaimed less than half of what it took"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            after = held()?;
        }
        eprintln!(
            "{name}: the balloon took {amount} MiB, the private memory held {before} bytes, \
             then {after}, {:?} after",
            start.elapsed()
        );
        firecracker.request("PATCH", "/balloon", &serde_json::json!({ "amount_mib": 0 }))?;
        writeln!(tell, "deflated")?;
        Ok(())
    }

    /// Formats the empty cell at `path`, with the default volume, `volumes` others and `RO_VOLUME`.
    fn format(path: &Path, volumes: usize) -> anyhow::Result<()> {
        block_on(async {
            let fs = Filesystem::format(Storage::open(path).await?).await?;
            for volume in 0..volumes {
                fs.create_volume(&format!("v{volume}")).await?;
            }
            fs.create_volume(RO_VOLUME).await?;
            fs.compact().await?;
            fs.close().await?;
            Ok(())
        })
    }

    /// Grants `tenant` a socket on `cladefsd` and shares the snapshot's version of `source` with it.
    fn share_with_tenant(
        cladefsd: &Path,
        source: cladefs::CellId,
        tenant: u64,
    ) -> anyhow::Result<PathBuf> {
        let control = crate::common::control(cladefsd)?;
        let versions = control
            .call(async |client| {
                client
                    .list_versions(ListVersionsRequest { tenant: source.tenant, cell: source.id })
                    .await
            })
            .map(|response| response.versions)?;
        let version = versions.last().context("the snapshot made no version")?;
        let share = cladefs::cladefsd::control::proto::ShareVersionRequest {
            tenant: source.tenant,
            cell: source.id,
            version: version.id,
            shared: true,
        };
        control.call(async |client| client.share_version(share).await)?;
        let id = |name: &str| std::env::var(name).ok().map(|id| id.parse()).transpose();
        let uid = id("CLADEFS_JAIL_UID")?;
        let gid = id("CLADEFS_JAIL_GID")?;
        Ok(control
            .call(async |client| client.grant(GrantRequest { tenant, uid, gid }).await)
            .map(|response| std::path::PathBuf::from(response.socket))?)
    }

    fn read_file(fs: &Filesystem, name: &str) -> Vec<u8> {
        let (file, attr) = block_on(fs.root().lookup(name.as_bytes()))
            .unwrap_or_else(|err| panic!("host: lookup {name}: {err}"));
        let mut buf = vec![0; attr.size as usize];
        let count = block_on(file.read(0, &mut buf)).unwrap();
        buf.truncate(count);
        buf
    }

    /// The files a migration between hosts moves: the snapshot, its memory, and the VM's root
    /// filesystem, which the restored VM goes on reading.
    const BUNDLED: [&str; 3] = ["snapshot", "memory", "rootfs.ext4"];

    /// The bundle a migration between hosts leaves the VM in, and restores it from.
    fn bundle() -> anyhow::Result<PathBuf> {
        Ok(std::env::var_os("CLADEFS_MIGRATE_BUNDLE")
            .context("CLADEFS_MIGRATE_BUNDLE not set")?
            .into())
    }

    impl TestFxfsGuestSnapshot {
        /// The VMs restored here, as `phase` says.
        fn restores(&self, phase: Option<Phase>) -> &'static [&'static str] {
            match phase {
                Some(Phase::Source) => &[],
                _ => self.mode.restores(),
            }
        }

        fn run(&self, test_setup: &TestSetup) -> anyhow::Result<()> {
            let dir = &test_setup.tmp_dir;
            let phase = Phase::of_env().filter(|_| self.mode == Mode::Migrate);
            let root = setup_rootfs(test_setup)?;
            std::fs::create_dir_all(root.join("mnt/fxfs"))?;
            // Migrating, a daemon of the test's own, which keeps cells in a bucket, as the hosts
            // a VM moves between do: the environment's, or one in its memory.
            let cladefsd = match self.mode {
                Mode::Migrate | Mode::TenantMigrate => {
                    // In the temporary directory, as a socket's path is short (SUN_LEN), and a
                    // test's directory, a repeat's of a soak, may not be.
                    let socket = std::env::temp_dir()
                        .join(format!("cladefs-{}-cladefsd.sock", std::process::id()));
                    let named =
                        std::env::var_os("CLADEFSD_BUCKET").is_some_and(|url| !url.is_empty());
                    let bucket = std::ffi::OsStr::new("memory:///migrate");
                    // Beside it, for its tenant's socket, in the jail if there is one.
                    let tenants = std::env::temp_dir()
                        .join(format!("cladefs-{}-tenants", std::process::id()));
                    let mut env = vec![("CLADEFSD_TENANTS", tenants.as_os_str())];
                    if !named {
                        env.push(("CLADEFSD_BUCKET", bucket));
                    }
                    let daemon =
                        crate::common::spawn_daemon(test_setup, &socket, "cladefsd", &env, None)?;
                    std::mem::forget(daemon);
                    socket
                }
                _ => std::env::var_os("CLADEFSD").context("CLADEFSD not set")?.into(),
            };
            let (snapshot, memory) = (dir.join("snapshot"), dir.join("memory"));
            let mut source_cell = None;
            // The snapshot's memory's version, of a memory cell's.
            let mut memory_version = None;
            if phase == Some(Phase::Target) {
                // The VM as the source host left it, where it was there.
                let bundle = bundle()?;
                for name in BUNDLED {
                    std::fs::copy(bundle.join(name), dir.join(name))
                        .with_context(|| format!("taking {name} from the bundle"))?;
                }
            } else {
                let cell = crate::common::create_cell(&cladefsd, CAPACITY)?;
                crate::common::save_cell_id(dir, "source", cell)?;
                source_cell = Some(cell);
                if self.volumes > 0 || self.read_only == ReadOnly::Volume {
                    let disk_root = match self.mode {
                        Mode::Migrate | Mode::TenantMigrate => {
                            crate::common::disk_root_for(dir, "cladefsd")
                        }
                        _ => crate::common::shared_disk_root()?,
                    };
                    format(&crate::common::cell_disk_path(&disk_root, cell), self.volumes)?;
                }
                let mut vm = Vm::new(2, RAM_MIB as u32)?;
                vm.guest_kernel()?;
                // A balloon inflated over a memory cell's private pages, which free page
                // reporting would give back before it (board 8497).
                if self.memory == Memory::Cell {
                    vm.balloon(false);
                }
                let window = if self.dax {
                    crate::common::dax_window(crate::common::Window::Large)?
                } else {
                    0
                };
                vm.cell_id_served("extents", cell, CAPACITY, window, &cladefsd);
                vm.read_only_root();
                let source_uds = uds("source");
                vm.vsock(&source_uds);
                let frozen = listen(&source_uds)?;
                // The booted guest gives back its memory, its memory cell's window's, as a
                // restored one does (board 14415).
                let gives_back = match self.memory {
                    Memory::Cell => Some(listen_on(&source_uds, BALLOON_PORT)?),
                    Memory::File => None,
                };
                let api = Vm::api_socket("source");
                let mut source = vm.spawn_api(&root, &test_setup.test_case, "source", &api)?;
                if let Some(listener) = &gives_back {
                    inflate_over_private_pages(&mut source, listener, RAM_MIB << 20, "source")?;
                }

                // Once the guest's filesystem is frozen, the snapshot, and the VM goes: its
                // memory into a file, or, its memory a memory cell's window as every VM's is,
                // into that cell, a version of it (board 14415).
                let stream = source.accept(&frozen)?;
                let mut said = String::new();
                BufReader::new(stream).read_line(&mut said)?;
                anyhow::ensure!(said == "frozen\n", "the guest said {said:?}");
                source.request("PATCH", "/vm", &serde_json::json!({ "state": "Paused" }))?;
                match self.memory {
                    Memory::File => source.request(
                        "PUT",
                        "/snapshot/create",
                        &serde_json::json!({
                            "snapshot_type": "Full",
                            "snapshot_path": snapshot,
                            "mem_file_path": memory,
                        }),
                    )?,
                    Memory::Cell => {
                        let start = std::time::Instant::now();
                        let version: MemoryCellSnapshot = source.request_json(
                            "PUT",
                            "/snapshot/create",
                            &serde_json::json!({
                                "snapshot_type": "Full",
                                "snapshot_path": snapshot,
                                "memory_cell": {},
                            }),
                        )?;
                        eprintln!("memory: a version of its memory cell in {:?}", start.elapsed());
                        memory_version = Some(version);
                    }
                }
                source.kill()?;
            }
            if self.memory == Memory::Cell {
                // The snapshot left the memory's pages in the daemon's memory: evicted, so that
                // the restores page in what they read, the first of them at least.
                crate::common::control(&cladefsd)?
                    .call(async |client| {
                        client
                            .shrink_memory(ShrinkMemoryRequest { tenant: None, target_bytes: 0 })
                            .await
                    })
                    .map(|response| response.bytes)?;
            }
            if phase == Some(Phase::Source) {
                // For another host to restore: the version's cell is in the bucket.
                let bundle = bundle()?;
                std::fs::create_dir_all(&bundle)?;
                for name in BUNDLED {
                    std::fs::copy(dir.join(name), bundle.join(name))
                        .with_context(|| format!("leaving {name} in the bundle"))?;
                }
            }

            if self.mode == Mode::Suspend {
                resize_once_let_go(&cladefsd, source_cell.context("no source cell")?)?;
            }

            // Record cells present before Zygote restores so `check()` can locate the restored
            // cells placed by the daemon.
            let cells_before_restore: std::collections::BTreeSet<(u64, u64)> =
                if self.mode == Mode::Zygote {
                    crate::common::control(&cladefsd)?
                        .call(async |client| client.list_cells(ListCellsRequest {}).await)?
                        .stores
                        .into_iter()
                        .map(|c| (c.tenant, c.cell))
                        .collect()
                } else {
                    std::collections::BTreeSet::new()
                };

            // Restored, told its name: each into its own cell, but resuming.
            let mut restored = Vec::new();
            let mut loads = Vec::new();
            // Where the guest restored from a memory cell gives back its memory (board 8497).
            let mut gives_back = None;
            // Each restore's tenant, of an ID of its own (TASK-364), where it has one.
            for (&name, id) in self.restores(phase).iter().zip(1..) {
                let overrides = match self.mode {
                    Mode::Zygote => serde_json::json!([
                        { "id": "extents", "path_on_host": "0/0" },
                    ]),
                    Mode::Suspend => serde_json::json!([]),
                    Mode::Migrate => serde_json::json!([{
                        "id": "extents",
                        "path_on_host": "0/0",
                        "cladefsd_socket": cladefsd,
                    }]),
                    // The snapshot's own tenant's: only the cell to name.
                    Mode::TenantMigrate if name == "here" => serde_json::json!([
                        { "id": "extents", "path_on_host": "0/0" },
                    ]),
                    Mode::TenantMigrate => {
                        let source = source_cell.context("no source cell")?;
                        let socket = share_with_tenant(&cladefsd, source, id)?;
                        serde_json::json!([{
                            "id": "extents",
                            "path_on_host": format!("{id}/0"),
                            "cladefsd_socket": socket,
                        }])
                    }
                };
                let uds = uds(name);
                let overrides = Vm::stream_overrides(overrides, &["extents"], &uds)?;
                let listener = listen(&uds)?;
                if self.memory == Memory::Cell && name == GIVES_BACK {
                    gives_back = Some(listen_on(&uds, BALLOON_PORT)?);
                }
                let firecracker = Firecracker::spawn(dir, name, &Vm::api_socket(name), None, &[])?;
                let mut load = serde_json::json!({
                    "snapshot_path": snapshot,
                    "mem_backend": { "backend_type": "File", "backend_path": memory },
                    "resume_vm": true,
                    "vsock_override": { "uds_path": uds },
                    "cladefs_overrides": overrides,
                });
                if self.memory == Memory::Cell {
                    load["mem_backend"] = serde_json::json!({
                        "backend_type": "Cell",
                        "backend_path": "0/0",
                    });
                    let version = memory_version.context("the memory cell made no version")?;
                    load["memory_cell"] = serde_json::json!({
                        "tenant": version.tenant,
                        "cell": version.cell,
                        "version": version.version,
                        "cladefsd_socket": cladefsd,
                    });
                }
                loads.push(load);
                // Each connection the guest makes, as one reset by the restore is made again.
                std::thread::spawn(move || -> std::io::Result<()> {
                    for stream in listener.incoming() {
                        writeln!(stream?, "{name}")?;
                    }
                    Ok(())
                });
                restored.push(firecracker);
            }
            // From a memory cell, one after the other, each run to its end: the second takes
            // almost none of the daemon's memory the first did not, the daemon's pages of the
            // snapshot's memory shared, the second's memory the first's but what each wrote
            // (TASK-456.3). What the daemon holds, a shrink to no less than it says (`u64::MAX`).
            if self.memory == Memory::Cell {
                let control = crate::common::control(&cladefsd)?;
                let held = || {
                    control
                        .call(async |client| {
                            client
                                .shrink_memory(ShrinkMemoryRequest {
                                    tenant: None,
                                    target_bytes: u64::MAX,
                                })
                                .await
                        })
                        .map(|response| response.bytes)
                };
                let mut grew = Vec::new();
                for ((mut firecracker, load), name) in
                    restored.into_iter().zip(&loads).zip(self.restores(phase))
                {
                    let before = held()?;
                    // The windows' pages and private pages while the VM runs, at most: of its
                    // memory's window, and its disk's, a few.
                    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let sampling = {
                        let (stop, cladefsd) = (stop.clone(), cladefsd.clone());
                        std::thread::spawn(move || -> anyhow::Result<(u64, u64)> {
                            let control = crate::common::control(&cladefsd)?;
                            let mut most = (0, 0);
                            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                                let gauges = control
                                    .call(async |client| {
                                        client
                                            .get_stats(GetStatsRequest {
                                                wait_ms: 1,
                                                ..Default::default()
                                            })
                                            .await
                                    })?
                                    .gauges;
                                let gauge = |name: &str| gauges.get(name).copied().unwrap_or(0);
                                most.0 = most.0.max(gauge("window_pages"));
                                most.1 = most.1.max(gauge("window_private_pages"));
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                            Ok(most)
                        })
                    };
                    let start = std::time::Instant::now();
                    firecracker.request("PUT", "/snapshot/load", load)?;
                    eprintln!("{name}: restored in {:?}", start.elapsed());
                    if *name == GIVES_BACK {
                        let listener = gives_back.as_ref().context("no balloon port")?;
                        inflate_over_private_pages(
                            &mut firecracker,
                            listener,
                            RAM_MIB << 20,
                            GIVES_BACK,
                        )?;
                    }
                    let (status, console) = firecracker.wait()?;
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    let (pages, private) = sampling.join().expect("the sampling")?;
                    eprintln!(
                        "{name}: its windows mapped {pages} pages at most, {private} private"
                    );
                    // The first restore's window shares more than half of what it maps: its
                    // private pages are its guest's writes, a third or so of its pages, the
                    // case's files read into its page cache among them. Its loads, of pages its
                    // window installs ahead, take none: with no pages installed ahead, x86 KVM's
                    // asynchronous page fault worker made nine in ten private (TASK-456.3,
                    // docs/research/kvm-async-pf-and-shared-guest-memory.md).
                    anyhow::ensure!(
                        *name != "a" || private * 2 < pages,
                        "{name}: {private} of {pages} pages private: its memory isn't shared"
                    );
                    crate::vm::split_console(std::fs::File::open(&console)?)?;
                    anyhow::ensure!(status.success(), "a restored VM exited {status}");
                    let after = held()?;
                    eprintln!("{name}: the daemon held {before} bytes, then {after}");
                    grew.push(after.saturating_sub(before));
                }
                anyhow::ensure!(
                    grew[1] * 4 < grew[0],
                    "the second restore took {} bytes of the daemon's memory, the first {}: their \
                     pages aren't shared",
                    grew[1],
                    grew[0]
                );
                return Ok(());
            }
            // All at once: no restore of one version waits for another's. cladefs: each timed, as
            // it maps the window's mappings again before the VM resumes (TASK-92.14).
            std::thread::scope(|scope| {
                let loading: Vec<_> = (restored.iter().zip(&loads).zip(self.restores(phase)))
                    .map(|((firecracker, load), name)| {
                        scope.spawn(move || {
                            let start = std::time::Instant::now();
                            firecracker.request("PUT", "/snapshot/load", load)?;
                            eprintln!("{name}: restored in {:?}", start.elapsed());
                            anyhow::Ok(())
                        })
                    })
                    .collect();
                for load in loading {
                    load.join().expect("a load")?;
                }
                anyhow::Ok(())
            })?;
            for firecracker in restored {
                let (status, console) = firecracker.wait()?;
                crate::vm::split_console(std::fs::File::open(&console)?)?;
                anyhow::ensure!(status.success(), "a restored VM exited {status}");
            }
            if self.mode == Mode::Zygote {
                let created: Vec<String> = crate::common::control(&cladefsd)?
                    .call(async |client| client.list_cells(ListCellsRequest {}).await)?
                    .stores
                    .into_iter()
                    .filter(|c| !cells_before_restore.contains(&(c.tenant, c.cell)))
                    .map(|c| cladefs::CellId { tenant: c.tenant, id: c.cell }.to_string())
                    .collect();
                std::fs::write(dir.join("restored.cells"), created.join("\n"))?;
            }
            Ok(())
        }
    }

    /// Grows `cell` to `GROWN`, once the daemon lets go of the VM killed just now: it
    /// refuses a resize (`EBUSY`) while a connection whose VM process is gone holds the cell.
    fn resize_once_let_go(cladefsd: &Path, cell: cladefs::CellId) -> anyhow::Result<()> {
        let control = crate::common::control(cladefsd)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match control
                .call(async |client| {
                    client
                        .resize_cell(ResizeCellRequest {
                            tenant: cell.tenant,
                            cell: cell.id,
                            capacity: GROWN,
                        })
                        .await
                })
                .map(drop)
            {
                Err(error)
                    if error.errno() == Some(cladefs::Errno::EBUSY)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                resized => return Ok(resized?),
            }
        }
    }

    impl Test for TestFxfsGuestSnapshot {
        fn should_run(&self) -> ShouldRun {
            cladefs_guest_should_run()
        }

        fn timeout_secs(&self) -> u64 {
            match (self.volumes, self.memory) {
                (1.., _) => 600,
                // A guest writing its memory twice, and its balloon inflated between (board 8497).
                (0, Memory::Cell) => 300,
                (0, Memory::File) => 120,
            }
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            if let Err(err) = self.run(&test_setup) {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let output = String::from_utf8(stdout).unwrap();
            let restores = self.restores(Phase::of_env().filter(|_| self.mode == Mode::Migrate));
            let expected: String = restores.iter().map(|name| format!("OK {name}\n")).collect();
            if output != expected {
                return TestOutcome::Fail(format!("expected {expected:?}, got {output:?}"));
            }
            // Each cell holds what its guest wrote, checked, and not another's: but a migrated
            // one, whose data the daemon that served it took from its bucket, gone with it.
            let disk_root = crate::common::shared_disk_root().ok();
            let check_cells: Vec<cladefs::CellId> = match self.mode {
                Mode::Zygote if self.memory != Memory::Cell => {
                    let text = std::fs::read_to_string(test_setup.tmp_dir.join("restored.cells"))
                        .expect("restored.cells");
                    text.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| l.trim().parse().expect("cell id"))
                        .collect()
                }
                Mode::Suspend => {
                    vec![
                        crate::common::load_cell_id(&test_setup.tmp_dir, "source").expect("source"),
                    ]
                }
                _ => vec![],
            };
            if !check_cells.is_empty() {
                let cladefsd = std::env::var_os("CLADEFSD").expect("CLADEFSD");
                let control = crate::common::control(Path::new(&cladefsd)).expect("control API");
                let disk_root = disk_root.expect("CLADEFSD_ROOTS");
                let mut seen = std::collections::BTreeSet::new();
                for cell in check_cells {
                    control
                        .call(async |client| {
                            client
                                .stat_cell(StatCellRequest { tenant: cell.tenant, cell: cell.id })
                                .await
                        })
                        .expect("stat restored cell");
                    let path = crate::common::cell_disk_path(&disk_root, cell);
                    let fs = block_on(async {
                        let storage = Storage::open(&path).await.expect("open cell");
                        Filesystem::mount(storage.check().await?).await
                    })
                    .expect("mount cell");
                    let who = String::from_utf8(read_file(&fs, "who")).expect("who utf-8");
                    assert_eq!(read_file(&fs, "before"), BEFORE, "cell {who}: before");
                    assert_eq!(read_file(&fs, "after").len(), 4 << 20, "cell {who}: after");
                    assert!(read_file(&fs, "pieces") == pieces_stored(), "cell {who}: pieces");
                    seen.insert(who);
                }
                let expected_who: std::collections::BTreeSet<String> =
                    restores.iter().map(|s| (*s).to_owned()).collect();
                assert_eq!(seen, expected_who, "restored cells' who markers");
            }
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::ffi::CString;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;

    const MNT: &str = "/mnt/fxfs";
    /// The volume a mount naming none mounts (fs/cladefs's CLADEFS_DEFAULT_VOLUME).
    const FXFS_DEFAULT: &str = "default";
    /// cladefs board 14663: whether the extent-store device's guest sends its data requests over
    /// vsock (`F_STREAM`, bit 12 of virtio's `features`).
    fn stream_device() -> bool {
        let devices: Vec<_> = fs::read_dir("/sys/bus/virtio/drivers/virtio_extent")
            .unwrap()
            .map(|entry| entry.unwrap().path().join("features"))
            .filter(|path| path.exists())
            .collect();
        assert_eq!(devices.len(), 1, "{devices:?}");
        fs::read_to_string(&devices[0]).unwrap().as_bytes().get(12) == Some(&b'1')
    }

    /// `FIFREEZE` and `FITHAW`: `_IOWR('X', 119, int)` and `_IOWR('X', 120, int)`.
    const FIFREEZE: libc::c_ulong = 0xc004_5877;
    const FITHAW: libc::c_ulong = 0xc004_5878;

    fn mount() {
        let (source, target, fstype) = (
            CString::new("extents").unwrap(),
            CString::new(MNT).unwrap(),
            CString::new("cladefs").unwrap(),
        );
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    fn unmount() {
        let target = CString::new(MNT).unwrap();
        assert_eq!(unsafe { libc::umount(target.as_ptr()) }, 0, "umount");
    }

    /// Freezes, or thaws, the volume mounted at `target`.
    fn freeze_at(target: &str, request: libc::c_ulong) {
        let dir = fs::File::open(target).unwrap();
        let ret = unsafe { libc::ioctl(dir.as_raw_fd(), request as _, 0) };
        assert_eq!(ret, 0, "ioctl {request:#x}: {}", std::io::Error::last_os_error());
    }

    /// The filesystem takes the size its cell grew to while the VM was away: it says so, and
    /// holds more than the size it had, which its allocator, which `fallocate` doesn't ask, would
    /// refuse otherwise (ENOSPC).
    fn grown() {
        let stats = nix::sys::statvfs::statvfs(MNT).unwrap();
        assert_eq!(stats.blocks() * stats.fragment_size(), GROWN);
        let past = format!("{MNT}/past-what-it-was");
        let mut file = fs::File::create(&past).unwrap();
        let chunk = vec![0x5a; 1 << 20];
        for _ in 0..(CAPACITY + CAPACITY / 4) >> 20 {
            file.write_all(&chunk).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);
        fs::remove_file(&past).unwrap();
    }

    /// Freezes, or thaws, the filesystem: each of the `volumes`, then the default.
    fn freeze(volumes: usize, request: libc::c_ulong) {
        for volume in 0..volumes {
            freeze_at(&at(volume), request);
        }
        freeze_at(MNT, request);
    }

    /// Where the volume `volume` mounts.
    fn at(volume: usize) -> String {
        format!("{MNT}/v{volume}")
    }

    fn mount_volumes(volumes: usize) {
        for volume in 0..volumes {
            mount_volume(&format!("v{volume}"), &at(volume), 0);
        }
    }

    /// Mounts the volume `name` at `target`, with the mount's `flags`; or, with `MS_REMOUNT`,
    /// mounts it again so.
    fn mount_volume(name: &str, target: &str, flags: libc::c_ulong) {
        let (source, target, fstype, options) = (
            CString::new("extents").unwrap(),
            CString::new(target).unwrap(),
            CString::new("cladefs").unwrap(),
            CString::new(format!("volume={name}")).unwrap(),
        );
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                flags,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(ret, 0, "mount {name} ({flags:#x}): {}", std::io::Error::last_os_error());
    }

    fn unmount_volumes(volumes: usize) {
        for volume in 0..volumes {
            unmount_at(&at(volume));
        }
    }

    fn unmount_at(target: &str) {
        let target_c = CString::new(target).unwrap();
        assert_eq!(unsafe { libc::umount(target_c.as_ptr()) }, 0, "umount {target}");
    }

    /// Where `RO_VOLUME` mounts, read-only.
    fn read_only_at() -> String {
        format!("{MNT}/{RO_VOLUME}")
    }

    /// What the filesystem says of itself as `name`, a count, since it opened.
    fn stat(name: &str) -> Option<u64> {
        let stats = fs::read_to_string("/proc/fs/cladefs/extents").unwrap();
        let line = stats.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' '));
        line.map(|count| count.parse().unwrap())
    }

    /// How many reads of the device the filesystem made since it opened.
    fn device_reads() -> u64 {
        stat("device_reads").expect("device_reads")
    }

    /// A connection to the host on `PORT`.
    fn connect() -> fs::File {
        try_connect().expect("connecting to the host")
    }

    /// A connection to the host on `PORT`, or why not.
    fn try_connect() -> std::io::Result<fs::File> {
        try_connect_to(PORT)
    }

    /// A connection to the host on `port`.
    fn connect_to(port: u32) -> fs::File {
        try_connect_to(port).unwrap_or_else(|err| panic!("connecting to the host on {port}: {err}"))
    }

    /// A connection to the host on `port`, or why not.
    fn try_connect_to(port: u32) -> std::io::Result<fs::File> {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, VsockAddr, connect, socket};
        use std::os::fd::{FromRawFd as _, IntoRawFd as _};
        let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::empty(), None)
            .expect("a vsock socket");
        connect(fd.as_raw_fd(), &VsockAddr::new(2, port))?;
        // SAFETY: the socket's descriptor, which the file owns from here on.
        Ok(unsafe { fs::File::from_raw_fd(fd.into_raw_fd()) })
    }

    /// The name the host gives this restored VM, on a new connection. The restore resets the
    /// guest's vsock connections, which its kernel learns of from an event it may take only after
    /// the old connection has ended, and a new one is made (TASK-50.1.2): one made as the snapshot
    /// was taken connects, as its response was in the RX queue, and then ends, its RST behind it,
    /// nothing read (TASK-277). Each so reset is made again ([`crate::reconnect`]).
    fn name_from_host() -> String {
        let name = crate::reconnect::exchange(try_connect, |host| {
            let mut name = String::new();
            match BufReader::new(host).read_line(&mut name)? {
                0 => Err(std::io::ErrorKind::UnexpectedEof.into()),
                _ => Ok(name),
            }
        });
        name.expect("reading the name from the host").trim_end().to_owned()
    }

    /// Writes the file of `PIECES` pages at `path`, each page an extent of its own: written alone,
    /// last first, and made durable before the next, so that no two follow one another on the
    /// device, whose window maps each apart (TASK-92.14).
    fn write_pieces(path: &str) {
        use std::os::unix::fs::FileExt;
        let file = fs::File::create(path).unwrap();
        for page in (0..PIECES).rev() {
            file.write_all_at(&piece(page), (page * 4096) as u64).unwrap();
            file.sync_data().unwrap();
        }
    }

    /// Maps the file of `PIECES` pages at `path`, shared and writable, which stays mapped; reads
    /// it, so that the window maps each of its pages (TASK-92.14), KVM's worker writing for the
    /// loads; and writes to every eighth page through it (TASK-191).
    fn map_pieces(path: &str) -> &'static [u8] {
        let len = PIECES * 4096;
        let file = fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        // SAFETY: the mapping, never unmapped.
        let mapped = unsafe { std::slice::from_raw_parts_mut(map as *mut u8, len) };
        assert!(mapped == &pieces()[..], "the pieces' mapping before the snapshot");
        for page in (0..PIECES).filter(|&page| piece_stored(page)) {
            mapped[page * 4096..page * 4096 + 8].copy_from_slice(&stored(page));
        }
        mapped
    }

    /// Maps the `len` bytes of `path`, shared and read-only, which stays mapped.
    fn map(path: &str, len: usize) -> &'static [u8] {
        let file = fs::File::open(path).unwrap();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        // SAFETY: the mapping, never unmapped.
        unsafe { std::slice::from_raw_parts(map as *const u8, len) }
    }

    impl Test for TestFxfsGuestSnapshot {
        fn in_guest(self: Box<Self>) {
            let volumes = self.volumes;
            mount();
            if volumes > 0 {
                // With nothing in the journal, each volume opened would read its cell's record.
                let reads = device_reads();
                eprintln!("{reads} reads, with {volumes} volumes closed");
                assert!(reads < volumes as u64, "{reads} reads with {volumes} volumes");
            }
            // Read in again by the next mount, a DAX file, mapped through the window.
            write_pieces(&format!("{MNT}/pieces"));
            // cladefs board 14663: a file read again and again across the snapshot, its pages
            // dropped before each read, so that reads are in flight as it is taken: in each
            // restored VM, their device's transport takes them again, and each completes once,
            // reading what was written.
            let in_flight = format!("{MNT}/in-flight");
            let in_flight_bytes: Vec<u8> = (0..1 << 20).map(|i: usize| (i % 239) as u8).collect();
            fs::write(&in_flight, &in_flight_bytes).unwrap();
            fs::File::open(&in_flight).unwrap().sync_all().unwrap();
            unmount();
            mount();
            let mut mapped_pieces = map_pieces(&format!("{MNT}/pieces"));
            for volume in 0..volumes {
                fs::create_dir(at(volume)).unwrap();
            }
            mount_volumes(volumes);
            for volume in 0..volumes {
                fs::write(format!("{}/before", at(volume)), volume.to_string()).unwrap();
            }
            fs::write(format!("{MNT}/before"), BEFORE).unwrap();
            fs::write(format!("{MNT}/mapped"), mapped_pattern()).unwrap();
            let mapped = map(&format!("{MNT}/mapped"), MAPPED_LEN);
            // Read through the mapping, which with a DAX window maps the window's pages.
            assert!(mapped == &mapped_pattern()[..], "the mapping before the snapshot");
            match self.read_only {
                ReadOnly::Nothing => {}
                // Written to writable first, then mounted read-only, frozen first.
                ReadOnly::Volume => {
                    fs::create_dir(read_only_at()).unwrap();
                    mount_volume(RO_VOLUME, &read_only_at(), 0);
                    fs::write(format!("{}/before", read_only_at()), BEFORE).unwrap();
                    unmount_at(&read_only_at());
                    mount_volume(RO_VOLUME, &read_only_at(), libc::MS_RDONLY);
                    freeze_at(&read_only_at(), FIFREEZE);
                }
                ReadOnly::Remounted => {
                    let len = mapped_pieces.len();
                    // SAFETY: the mapping `map_pieces` made, which nothing uses any more.
                    assert_eq!(unsafe { libc::munmap(mapped_pieces.as_ptr() as *mut _, len) }, 0);
                    mapped_pieces = map(&format!("{MNT}/pieces"), len);
                    let written = pieces_stored();
                    assert!(mapped_pieces == &written[..], "the pieces' mapping, read-only");
                    mount_volume(FXFS_DEFAULT, MNT, libc::MS_REMOUNT | libc::MS_RDONLY)
                }
            }
            // Booted, its memory a memory cell's window as a restored VM's is, the guest gives
            // back its memory as the second restore's does (board 14415).
            if self.memory == Memory::Cell {
                give_back_free_memory("source");
            }
            let counts = || (stat("dax_private_pages"), stat("dax_dirty_pages"));
            eprintln!("before the freeze: (private, dirty) pages {:?}", counts());
            freeze(volumes, FIFREEZE);
            let reading = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let reader = {
                let (reading, path, bytes) = (reading.clone(), in_flight.clone(), in_flight_bytes);
                std::thread::spawn(move || {
                    let mut reads = 0u64;
                    while reading.load(std::sync::atomic::Ordering::Relaxed) {
                        let file = fs::File::open(&path).unwrap();
                        // SAFETY: a call on a descriptor this holds.
                        unsafe {
                            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                        };
                        let mut back = Vec::with_capacity(bytes.len());
                        (&file).read_to_end(&mut back).unwrap();
                        assert!(back == bytes, "read {reads}: the file read otherwise");
                        reads += 1;
                    }
                    reads
                })
            };
            // Frozen, the window holds no private page, its dirty pages written back and every
            // one rebound, which a snapshot's fork requires (backlog doc-13; TASK-92.14).
            let (private, dirty) = counts();
            assert_eq!(
                (private.unwrap_or(0), dirty.unwrap_or(0)),
                (0, 0),
                "frozen with private and dirty pages"
            );
            {
                let mut host = connect();
                host.write_all(b"frozen\n").unwrap();
                // The snapshot is taken, and the VM goes; in each VM restored from it, the
                // connection is reset.
                let mut rest = Vec::new();
                let _ = host.read_to_end(&mut rest);
            }
            eprintln!("restored");
            let name = name_from_host();
            reading.store(false, std::sync::atomic::Ordering::Relaxed);
            let reads = reader.join().unwrap();
            eprintln!("{name}: {reads} reads of the in-flight file across the snapshot");
            // cladefs board 14663: a stream's connection, which the vsock device reset, made
            // again, its requests in flight sent again.
            if stream_device() {
                let records = crate::kernel_log::records();
                assert!(
                    records.iter().any(|record| record.contains("connection lost")
                        && record.contains("connecting again")),
                    "{name}: no stream connected again"
                );
            }
            eprintln!("{name}: thawing");
            freeze(volumes, FITHAW);
            match self.read_only {
                ReadOnly::Nothing => {}
                ReadOnly::Volume => {
                    freeze_at(&read_only_at(), FITHAW);
                    let before = fs::read(format!("{}/before", read_only_at())).unwrap();
                    assert_eq!(before, BEFORE);
                    unmount_at(&read_only_at());
                }
                ReadOnly::Remounted => mount_volume(FXFS_DEFAULT, MNT, libc::MS_REMOUNT),
            }
            eprintln!("{name}: thawed");
            if self.mode == Mode::Suspend {
                grown();
            }
            for volume in 0..volumes {
                let path = at(volume);
                assert_eq!(
                    fs::read(format!("{path}/before")).unwrap(),
                    volume.to_string().as_bytes()
                );
                fs::write(format!("{path}/who"), &name).unwrap();
            }

            assert_eq!(fs::read(format!("{MNT}/before")).unwrap(), BEFORE);
            assert!(mapped == &mapped_pattern()[..], "the mapping after the restore");
            // SAFETY: the mapping `map` made, which nothing uses any more.
            assert_eq!(unsafe { libc::munmap(mapped.as_ptr() as *mut _, MAPPED_LEN) }, 0);
            // Through the mapping, which the window maps again, and read.
            let written = pieces_stored();
            assert!(mapped_pieces == &written[..], "{name}: the pieces' mapping after the restore");
            assert!(fs::read(format!("{MNT}/pieces")).unwrap() == written, "{name}: pieces read");
            // SAFETY: the pieces' mapping, which nothing uses any more.
            let len = mapped_pieces.len();
            assert_eq!(unsafe { libc::munmap(mapped_pieces.as_ptr() as *mut _, len) }, 0);
            fs::write(format!("{MNT}/who"), &name).unwrap();
            eprintln!("{name}: wrote");
            // New extents, of the restored cell's cell.
            let after: Vec<u8> = (0..4 << 20).map(|i: usize| (i % 241) as u8).collect();
            let mut file = fs::File::create(format!("{MNT}/after")).unwrap();
            file.write_all(&after).unwrap();
            file.sync_all().unwrap();
            drop(file);
            unmount_volumes(volumes);
            unmount();
            mount();
            assert_eq!(fs::read(format!("{MNT}/who")).unwrap(), name.as_bytes());
            assert_eq!(fs::read(format!("{MNT}/after")).unwrap(), after);
            assert!(
                fs::read(format!("{MNT}/pieces")).unwrap() == pieces_stored(),
                "{name}: pieces"
            );
            mount_volumes(volumes);
            for volume in 0..volumes {
                assert_eq!(fs::read(format!("{}/who", at(volume))).unwrap(), name.as_bytes());
            }
            unmount_volumes(volumes);
            unmount();
            // Restored from a memory cell, the second restore's guest writes all the memory it
            // has free, each of its pages a private copy of its window's: it holds as many as the
            // guest has pages (TASK-456.3). The first's reads and writes no more than the case's,
            // whose pages its window shares. The second's then gives it back, the host inflating
            // its balloon over it, and writes it again once the balloon deflates (board 8497).
            if self.memory == Memory::Cell && name == GIVES_BACK {
                give_back_free_memory(&name);
                write_free_memory(&name);
            }
            println!("OK {name}");
        }
    }

    /// Writes every page of what memory the guest has available but a margin, as
    /// [`write_free_memory`] does, keeping it, and tells the host on `BALLOON_PORT` how much; once
    /// the host says, frees it, and says so: the host inflates the balloon over it, and says once
    /// it deflated it again (board 8497).
    fn give_back_free_memory(name: &str) {
        let (memory, len) = written_free_memory(name);
        let mut host = connect_to(BALLOON_PORT);
        writeln!(host, "written {}", len >> 20).unwrap();
        let mut reader = BufReader::new(host.try_clone().unwrap());
        let mut said = String::new();
        reader.read_line(&mut said).unwrap();
        assert_eq!(said, "free\n", "{name}: the host said");
        // Each page's one byte written cleared first, so that the pages the balloon doesn't take
        // hold zeros, holes to a snapshot of the memory: the booted VM's are its restores' images,
        // whose sharing they would otherwise cloud (board 14415).
        for at in (0..len).step_by(4096) {
            // SAFETY: within the mapping.
            unsafe { memory.add(at).write_volatile(0) };
        }
        // SAFETY: the mapping, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory.cast(), len) }, 0);
        host.write_all(b"freed\n").unwrap();
        said.clear();
        reader.read_line(&mut said).unwrap();
        assert_eq!(said, "deflated\n", "{name}: the host said");
        eprintln!("{name}: gave back {} MiB of memory", len >> 20);
    }

    /// Writes every page of what memory the guest has available but a margin, and reads it back.
    fn write_free_memory(name: &str) {
        let (memory, len) = written_free_memory(name);
        // SAFETY: the mapping, which nothing uses any more.
        assert_eq!(unsafe { libc::munmap(memory.cast(), len) }, 0);
        eprintln!("{name}: wrote {} MiB of memory", len >> 20);
    }

    /// Maps what memory the guest has available but a margin, writes every page of it, reads it
    /// back, and returns it, mapped, and its length.
    fn written_free_memory(name: &str) -> (*mut u8, usize) {
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
        let line = meminfo.lines().find(|line| line.starts_with("MemAvailable:")).unwrap();
        let kib: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let page = 4096;
        let len = (kib * 1024).saturating_sub(48 << 20) / page * page;
        // SAFETY: new anonymous memory, which this function owns.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(memory, libc::MAP_FAILED);
        let memory = memory.cast::<u8>();
        let byte = |at: usize| (at / page) as u8 ^ 0x5a;
        for at in (0..len).step_by(page) {
            // SAFETY: within the mapping.
            unsafe { memory.add(at).write_volatile(byte(at)) };
        }
        for at in (0..len).step_by(page) {
            // SAFETY: within the mapping.
            assert_eq!(unsafe { memory.add(at).read_volatile() }, byte(at), "{name}: page {at:#x}");
        }
        (memory, len)
    }
}
