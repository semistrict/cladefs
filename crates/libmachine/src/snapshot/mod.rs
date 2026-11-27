// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Snapshots: a stopped machine written to a file, and machines restored
//! from it, as many as wanted, each with the guest's RAM a copy-on-write
//! mapping of the file.
//!
//! # The file (format version [`FORMAT_VERSION`])
//!
//! ```text
//! 0       header (64 bytes): magic "lmsnap\0\0", format version (u32),
//!         0 (u32), the state's length (u64), the RAM's offset (u64; 0 for
//!         RAM in a file of its own), the state's SHA-256 (32 bytes)
//! 64      the state: what the machine is (CPU model and fingerprint, vCPU
//!         count, RAM size, the memory layout, its balloon, disks and extent
//!         store devices, the caller's data), then the backend's: on KVM its
//!         VM state, every vCPU's, the UART's and every PCI function's with its
//!         device's; on Hypervisor.framework the GIC's and the host's clock,
//!         every vCPU's, the UART's, the RTC's and every PCI function's
//!         (snapshot::codec)
//! RAM     each RAM region in address order, each at a multiple of 2 MiB,
//!         pages of zeroes left as holes
//! ```
//!
//! Or the RAM is in a file of its own ([`Machine::snapshot_with`]), as a
//! Firecracker snapshot's memory file is: the guest's RAM regions one after
//! the other from its start, pages of zeroes left as holes.
//!
//! Version 3 added the devices and the caller's data to what the machine is,
//! the RAM file of its own, and Hypervisor.framework's state.
//!
//! Formats are never converted (AGENTS.md): a file of another version, or
//! one whose state another version of libmachine wrote, is refused with an
//! error that says so. So is one whose machine differs from the one a
//! restore asks for: another CPU model (by the fingerprint, on this host),
//! vCPU count, address map or TSC frequency.
//!
//! The file is written beside its path and renamed over it, read-only for
//! its owner: machines restored from it map it, so it must not change while
//! they run (a file written in place would show its new pages to them).
//!
//! [`Machine::snapshot_with`]: crate::Machine::snapshot_with

pub(crate) mod codec;

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::cpu::CpuModel;
use crate::error::{Error, Result};
use crate::extent::{ExtentCache, ExtentCell, ExtentStore};
use crate::machine::MachineConfig;
use crate::memory::{MemoryLayout, MemoryRegion, RegionKind};
use codec::{Decoder, Encoder};

/// The snapshot format this libmachine writes and reads; any other is
/// refused.
pub const FORMAT_VERSION: u32 = 3;

const MAGIC: [u8; 8] = *b"lmsnap\0\0";
const HEADER_LEN: usize = 64;
/// RAM regions start at multiples of this in the file, so that the host
/// can map them with huge pages.
const RAM_ALIGN: u64 = 2 << 20;
const PAGE: usize = 4096;

/// What a snapshot's machine is, at the head of its state: what a restore
/// builds again before it puts the state back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineHeader {
    pub(crate) cpu_model: CpuModel,
    pub(crate) cpu_fingerprint: String,
    pub(crate) vcpus: u32,
    pub(crate) mem_size: u64,
    pub(crate) layout: MemoryLayout,
    /// Whether it has a balloon.
    pub(crate) balloon: bool,
    /// Its disks, in their order.
    pub(crate) disks: Vec<crate::Disk>,
    /// Its extent store devices, in their order, as virtio_extent saves each once its snapshot
    /// is taken: its configuration, its cell's version (or a migration's), its window's mappings.
    pub(crate) extent_stores: Vec<virtio_extent::persist::State>,
    /// What the caller keeps with the snapshot ([`Snapshot::caller_data`]).
    pub(crate) caller_data: Vec<u8>,
}

fn kind_code(kind: RegionKind) -> u8 {
    match kind {
        RegionKind::Ram => 0,
        RegionKind::Mmio32 => 1,
        RegionKind::Platform => 2,
        RegionKind::Mmio64 => 3,
        RegionKind::DeviceMemory => 4,
    }
}

fn kind_of(code: u8) -> Result<RegionKind> {
    Ok(match code {
        0 => RegionKind::Ram,
        1 => RegionKind::Mmio32,
        2 => RegionKind::Platform,
        3 => RegionKind::Mmio64,
        4 => RegionKind::DeviceMemory,
        _ => return Err(Error::Snapshot(format!("unknown region kind {code}"))),
    })
}

impl MachineHeader {
    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.str(&self.cpu_model.to_string());
        e.str(&self.cpu_fingerprint);
        e.u32(self.vcpus);
        e.u64(self.mem_size);
        e.count(self.layout.regions.len());
        for r in &self.layout.regions {
            e.u64(r.start);
            e.u64(r.size);
            e.u8(kind_code(r.kind));
        }
        e.bool(self.balloon);
        e.count(self.disks.len());
        for disk in &self.disks {
            e.bytes(disk.path.as_os_str().as_bytes());
            e.bool(disk.read_only);
            e.bool(disk.unsafe_flushes);
            e.str(&disk.id);
        }
        e.count(self.extent_stores.len());
        for store in &self.extent_stores {
            e.bytes(&store.encode());
        }
        e.bytes(&self.caller_data);
    }

    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let model = d.str()?;
        let cpu_model = model
            .parse()
            .map_err(|e: String| Error::Snapshot(format!("the snapshot's CPU model: {e}")))?;
        let cpu_fingerprint = d.str()?;
        let vcpus = d.u32()?;
        let mem_size = d.u64()?;
        let n = d.count(17)?;
        let regions = (0..n)
            .map(|_| Ok(MemoryRegion::new(d.u64()?, d.u64()?, kind_of(d.u8()?)?)))
            .collect::<Result<_>>()?;
        let balloon = d.bool()?;
        let n = d.count(10)?;
        let disks = (0..n)
            .map(|_| {
                let path = PathBuf::from(OsStr::from_bytes(d.bytes()?));
                Ok(crate::Disk {
                    path,
                    read_only: d.bool()?,
                    unsafe_flushes: d.bool()?,
                    id: d.str()?,
                })
            })
            .collect::<Result<_>>()?;
        let n = d.count(4)?;
        let extent_stores = (0..n)
            .map(|_| {
                virtio_extent::persist::State::decode(d.bytes()?).map_err(|e| {
                    Error::Snapshot(format!("the snapshot's extent store device: {e}"))
                })
            })
            .collect::<Result<_>>()?;
        let caller_data = d.bytes()?.to_vec();
        Ok(Self {
            cpu_model,
            cpu_fingerprint,
            vcpus,
            mem_size,
            layout: MemoryLayout { regions },
            balloon,
            disks,
            extent_stores,
            caller_data,
        })
    }
}

/// Where each RAM region of `layout` goes in the file, from `ram_offset`.
pub(crate) fn ram_offsets(layout: &MemoryLayout, ram_offset: u64) -> Vec<(MemoryRegion, u64)> {
    let mut at = ram_offset;
    layout
        .of_kind(RegionKind::Ram)
        .map(|r| {
            let offset = at;
            at = (at + r.size).next_multiple_of(RAM_ALIGN);
            (*r, offset)
        })
        .collect()
}

/// Where each RAM region of `layout` goes in a RAM file of its own: one after the other from its
/// start, as Firecracker's memory files hold them.
fn flat_offsets(layout: &MemoryLayout) -> Vec<(MemoryRegion, u64)> {
    let mut at = 0;
    layout
        .of_kind(RegionKind::Ram)
        .map(|r| {
            let offset = at;
            at += r.size;
            (*r, offset)
        })
        .collect()
}

/// Where a snapshot's RAM goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RamAt<'a> {
    /// In the snapshot's file, after its state.
    Inline,
    /// In a file of its own, at this path.
    File(&'a Path),
}

/// Writes a snapshot of a machine: `machine` and `state` (encoded by the
/// machine), and the guest's RAM from `mem`, where `ram` says. Every vCPU is
/// stopped and nothing writes guest memory while this runs.
pub(crate) fn write(
    path: &Path,
    machine: &MachineHeader,
    state: &[u8],
    mem: &GuestMemoryMmap,
    ram: RamAt<'_>,
) -> Result<()> {
    let mut head = Encoder::default();
    machine.encode(&mut head);
    let mut all = head.finish();
    all.extend_from_slice(state);
    let (ram_offset, regions) = match ram {
        RamAt::Inline => {
            let at = (HEADER_LEN as u64 + all.len() as u64).next_multiple_of(RAM_ALIGN);
            (at, ram_offsets(&machine.layout, at))
        }
        RamAt::File(_) => (0, flat_offsets(&machine.layout)),
    };

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&(all.len() as u64).to_le_bytes());
    header.extend_from_slice(&ram_offset.to_le_bytes());
    header.extend_from_slice(&Sha256::digest(&all));
    debug_assert_eq!(header.len(), HEADER_LEN);

    let write_state = |file: &File| -> std::io::Result<()> {
        let mut file = file;
        file.write_all(&header)?;
        file.write_all(&all)
    };
    match ram {
        RamAt::Inline => replace(path, |file| {
            write_state(file)?;
            write_ram(file, mem, &regions)
        }),
        // The RAM first: a state file names RAM that is there.
        RamAt::File(ram_path) => {
            replace(ram_path, |file| write_ram(file, mem, &regions))?;
            replace(path, write_state)
        }
    }
}

/// Writes guest RAM from `mem` into `file` where `regions` say, and sizes the file to their end.
fn write_ram(
    file: &File,
    mem: &GuestMemoryMmap,
    regions: &[(MemoryRegion, u64)],
) -> std::io::Result<()> {
    let end = regions.last().map_or(0, |(r, offset)| offset + r.size);
    file.set_len(end.max(file.metadata()?.len()))?;
    for (region, offset) in regions {
        let host =
            mem.get_host_address(GuestAddress(region.start)).map_err(std::io::Error::other)?;
        // SAFETY: the region is guest RAM, mapped for as long as `mem`
        // lives, and nothing writes it while a snapshot is taken (every
        // vCPU is stopped, the device thread held off).
        let ram = unsafe { std::slice::from_raw_parts(host, region.size as usize) };
        write_sparse(file, ram, *offset)?;
    }
    Ok(())
}

/// Makes the file `path` with what `fill` writes, read-only for its owner: written beside it,
/// synced, and renamed over it, so that no machine mapping an old file at `path` sees it change.
fn replace(path: &Path, fill: impl FnOnce(&File) -> std::io::Result<()>) -> Result<()> {
    let io = |source| Error::Io { path: path.to_owned(), source };
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| Error::InvalidConfig(format!("{} names no file", path.display())))?;
    let mut nonce = [0u8; 8];
    crate::sys::getrandom(&mut nonce);
    let temp =
        dir.join(format!(".{}.{:016x}.tmp", name.to_string_lossy(), u64::from_le_bytes(nonce)));
    let written = (|| -> std::io::Result<()> {
        let file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        fill(&file)?;
        file.sync_data()?;
        file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
        std::fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()
    })();
    if let Err(e) = written {
        // The write's error is the one to report; a temporary file left behind is only that.
        let _ = std::fs::remove_file(&temp);
        return Err(io(e));
    }
    Ok(())
}

/// Writes `data` at `offset`, leaving pages of zeroes as holes.
fn write_sparse(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
    let zero = |page: &[u8]| {
        let (head, words, tail) = as_words(page);
        head.iter().all(|&b| b == 0)
            && words.iter().all(|&w| w == 0)
            && tail.iter().all(|&b| b == 0)
    };
    let mut run: Option<usize> = None;
    let pages = data.len().div_ceil(PAGE);
    for i in 0..=pages {
        let page = data.get(i * PAGE..((i + 1) * PAGE).min(data.len()));
        let keep = page.is_some_and(|p| !zero(p));
        match (keep, run) {
            (true, None) => run = Some(i),
            (false, Some(start)) => {
                let bytes = &data[start * PAGE..(i * PAGE).min(data.len())];
                file.write_all_at(bytes, offset + (start * PAGE) as u64)?;
                run = None;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `bytes` as unaligned head, aligned `u64`s and tail.
fn as_words(bytes: &[u8]) -> (&[u8], &[u64], &[u8]) {
    // SAFETY: any bit pattern is a valid u64; align_to only reinterprets
    // the aligned middle.
    unsafe { bytes.align_to::<u64>() }
}

/// A snapshot file, opened and checked; restore machines from it with
/// [`Machine::restore`](crate::Machine::restore), as many as wanted, from
/// any thread. Opening reads and checks its state once; each restore maps
/// its RAM.
pub struct Snapshot {
    path: PathBuf,
    /// The file holding its RAM: the snapshot's own, or its RAM file.
    ram: Arc<File>,
    machine: MachineHeader,
    /// Each RAM region's offset in its file, in the layout's order.
    ram_offsets: Vec<u64>,
    /// The state after the machine's header, which a restore decodes.
    state: Vec<u8>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("path", &self.path)
            .field("cpu_model", &self.machine.cpu_model)
            .field("vcpus", &self.machine.vcpus)
            .field("mem_size", &self.machine.mem_size)
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// Opens the snapshot at `path` and checks it: its format version, its
    /// state's checksum, and that the file holds all of its RAM. One whose
    /// RAM is in a file of its own is opened with [`open_split`](Self::open_split).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_parts(path.as_ref(), None)
    }

    /// Opens the snapshot at `path` whose RAM is in the file `ram`
    /// ([`Machine::snapshot_with`](crate::Machine::snapshot_with)), and checks
    /// them as [`open`](Self::open) does.
    pub fn open_split(path: impl AsRef<Path>, ram: impl AsRef<Path>) -> Result<Self> {
        Self::open_parts(path.as_ref(), Some(ram.as_ref()))
    }

    fn open_parts(path: &Path, ram_path: Option<&Path>) -> Result<Self> {
        let io = |source| Error::Io { path: path.to_owned(), source };
        let mut file = File::open(path).map_err(io)?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                not_a_snapshot(path, "it is shorter than a snapshot's header")
            } else {
                io(e)
            }
        })?;
        if header[..8] == crate::suspend::FILE_MAGIC {
            return Err(suspended_file(path, &header));
        }
        if header[..8] != MAGIC {
            return Err(not_a_snapshot(path, "it does not start as a snapshot does"));
        }
        let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(Error::Snapshot(format!(
                "{} is a snapshot of format version {version}; this libmachine reads version \
                 {FORMAT_VERSION} only (snapshots are never converted: take a new one)",
                path.display()
            )));
        }
        let (state_len, ram_offset) = (u64_at(16), u64_at(24));
        let file_len = file.metadata().map_err(io)?.len();
        let inline = ram_offset != 0;
        if state_len > file_len || (inline && HEADER_LEN as u64 + state_len > ram_offset) {
            return Err(not_a_snapshot(path, "its header is damaged"));
        }
        let mut state = vec![0; state_len as usize];
        file.read_exact(&mut state)
            .map_err(|_| not_a_snapshot(path, "it ends inside its state"))?;
        if Sha256::digest(&state)[..] != header[32..64] {
            return Err(not_a_snapshot(path, "its state's checksum does not match"));
        }
        let mut d = Decoder::new(&state);
        let machine = MachineHeader::decode(&mut d)?;
        let machine_len = state.len() - d.remaining();
        let (ram, ram_path, regions) = match (inline, ram_path) {
            (true, None) => (file, path, ram_offsets(&machine.layout, ram_offset)),
            (false, Some(ram_path)) => {
                let ram = File::open(ram_path)
                    .map_err(|source| Error::Io { path: ram_path.to_owned(), source })?;
                (ram, ram_path, flat_offsets(&machine.layout))
            }
            (false, None) => {
                return Err(not_a_snapshot(
                    path,
                    "its RAM is in a file of its own (open it with Snapshot::open_split)",
                ));
            }
            (true, Some(ram_path)) => {
                return Err(not_a_snapshot(
                    path,
                    &format!("its RAM is in its own file, not in {}", ram_path.display()),
                ));
            }
        };
        let ram_len =
            ram.metadata().map_err(|source| Error::Io { path: ram_path.into(), source })?;
        let end = regions.last().map_or(0, |(r, offset)| offset + r.size);
        if ram_len.len() < end {
            return Err(not_a_snapshot(
                path,
                &format!(
                    "{} ends at {} bytes, before its RAM's end at {end}",
                    ram_path.display(),
                    ram_len.len()
                ),
            ));
        }
        let page = crate::sys::page_size() as u64;
        if let Some((r, offset)) = regions.iter().find(|(_, offset)| !offset.is_multiple_of(page)) {
            return Err(not_a_snapshot(
                path,
                &format!(
                    "its RAM at {:#x} is at {offset} bytes in {}, which this host's {page}-byte \
                     pages cannot map",
                    r.start,
                    ram_path.display()
                ),
            ));
        }
        let ram_offsets = regions.iter().map(|&(_, offset)| offset).collect();
        let rest = state.split_off(machine_len);
        Ok(Self { path: path.to_owned(), ram: Arc::new(ram), machine, ram_offsets, state: rest })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configuration of the machine the snapshot was taken of: its
    /// devices (its balloon, disks, and extent store devices, each on the cell
    /// it had, which a restore may change), and no network (a machine with
    /// one is never snapshotted). Host-side settings the snapshot does not
    /// record, such as the extent daemon and
    /// [`serial_capture_limit`](MachineConfig::serial_capture_limit), are
    /// the defaults; a restore chooses them.
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            mem_size: self.machine.mem_size as usize,
            cpu_model: self.machine.cpu_model,
            vcpus: self.machine.vcpus,
            network: None,
            serial_capture_limit: MachineConfig::DEFAULT_SERIAL_CAPTURE_LIMIT,
            extent_daemon: None,
            extent_stores: self.machine.extent_stores.iter().map(extent_store).collect(),
            nested_virtualization: false,
            balloon: self.machine.balloon,
            disks: self.machine.disks.clone(),
        }
    }

    /// What the caller kept with the snapshot as it took it
    /// ([`Machine::snapshot_with`](crate::Machine::snapshot_with)): empty if nothing.
    pub fn caller_data(&self) -> &[u8] {
        &self.machine.caller_data
    }

    /// The fingerprint of the CPU the guest saw
    /// ([`Machine::cpu_fingerprint`](crate::Machine::cpu_fingerprint)): a
    /// restore needs the same on its host.
    pub fn cpu_fingerprint(&self) -> &str {
        &self.machine.cpu_fingerprint
    }

    /// The guest's memory map when the snapshot was taken.
    pub fn memory_layout(&self) -> &MemoryLayout {
        &self.machine.layout
    }

    /// The file the guest's RAM is in.
    pub(crate) fn file(&self) -> &Arc<File> {
        &self.ram
    }

    pub(crate) fn ram_offsets(&self) -> &[u64] {
        &self.ram_offsets
    }

    /// Its extent store devices' saved state, in their order.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        allow(dead_code, reason = "KVM restores no extent store device yet")
    )]
    pub(crate) fn extent_states(&self) -> &[virtio_extent::persist::State] {
        &self.machine.extent_stores
    }

    /// The state past the machine's header.
    pub(crate) fn state(&self) -> Decoder<'_> {
        Decoder::new(&self.state)
    }

    /// Checks that a machine of `config` is the snapshot's: what a restore
    /// can tell before it builds one (the CPU fingerprint is checked on the
    /// machine it builds). Its devices are the snapshot's but for where they
    /// are on the host: a disk's file, an extent store device's cell.
    pub(crate) fn check(&self, config: &MachineConfig, layout: &MemoryLayout) -> Result<()> {
        let m = &self.machine;
        if config.network.is_some() {
            return Err(Error::Snapshot(
                "a machine restored from a snapshot has no network (networked machines are \
                 not snapshotted)"
                    .into(),
            ));
        }
        if config.vcpus != m.vcpus {
            return Err(Error::Snapshot(format!(
                "the snapshot was taken of a machine with {} vCPUs; this one would have {}",
                m.vcpus, config.vcpus
            )));
        }
        if *layout != m.layout {
            return Err(Error::Snapshot(format!(
                "the snapshot's memory map ({} bytes of RAM) differs from this machine's ({} \
                 bytes): {:x?} against {:x?}",
                m.mem_size, config.mem_size, m.layout.regions, layout.regions
            )));
        }
        if config.balloon != m.balloon {
            return Err(Error::Snapshot(format!(
                "the snapshot's machine {} a balloon; this one would {}",
                if m.balloon { "has" } else { "has no" },
                if config.balloon { "have one" } else { "have none" },
            )));
        }
        let unplaced = |disk: &crate::Disk| crate::Disk { path: PathBuf::new(), ..disk.clone() };
        if config.disks.iter().map(unplaced).ne(m.disks.iter().map(unplaced)) {
            return Err(Error::Snapshot(format!(
                "the snapshot's disks are {:?}; this machine's would be {:?} (a restore may move \
                 their files, nothing else)",
                m.disks, config.disks
            )));
        }
        let snapshot: Vec<ExtentStore> = m.extent_stores.iter().map(extent_store).collect();
        let unplaced = |store: &ExtentStore| ExtentStore {
            cell: ExtentCell::Directory(PathBuf::new()),
            ..store.clone()
        };
        if config.extent_stores.iter().map(unplaced).ne(snapshot.iter().map(unplaced)) {
            return Err(Error::Snapshot(format!(
                "the snapshot's extent store devices are {snapshot:?}; this machine's would be \
                 {:?} (a restore may open them on other cells, nothing else)",
                config.extent_stores
            )));
        }
        Ok(())
    }

    /// Checks the fingerprint of the CPU a restored machine shows.
    pub(crate) fn check_cpu(&self, model: CpuModel, fingerprint: &str) -> Result<()> {
        if fingerprint != self.machine.cpu_fingerprint {
            return Err(Error::Snapshot(format!(
                "the snapshot's guest saw CPU model {} with fingerprint {}; CPU model {model} \
                 on this host gives {fingerprint}",
                self.machine.cpu_model, self.machine.cpu_fingerprint
            )));
        }
        Ok(())
    }
}

/// The extent store device a snapshot's `state` is of, on the cell it had.
fn extent_store(state: &virtio_extent::persist::State) -> ExtentStore {
    let config = &state.config;
    ExtentStore {
        id: config.id.clone(),
        tag: config.tag.clone(),
        cell: ExtentCell::from_path_on_host(&config.path_on_host),
        capacity: config.capacity,
        dax_window_size: config.dax_window_size,
        dax_private_max_bytes: config.dax_private_max_bytes,
        cache: match config.cache_type {
            virtio_extent::CacheType::Writeback => ExtentCache::Writeback,
            virtio_extent::CacheType::Unsafe => ExtentCache::Unsafe,
        },
    }
}

/// The error for a suspended machine's file ([`crate::suspend`]), whose
/// header says what it lacks.
fn suspended_file(path: &Path, header: &[u8; HEADER_LEN]) -> Error {
    let lacks = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let mut what = Vec::new();
    if lacks & crate::suspend::LACKS_NETWORK != 0 {
        what.push("the host sockets of its guest's network connections");
    }
    if lacks & crate::suspend::LACKS_VSOCK != 0 {
        what.push("its vsock host end");
    }
    if what.is_empty() {
        what.push("what its process holds");
    }
    Error::Snapshot(format!(
        "{} is a suspended machine's file, not a snapshot: it lacks {}, which only the \
         process that suspended the machine holds; it is no template, and no machine is \
         restored from it",
        path.display(),
        what.join(" and ")
    ))
}

fn not_a_snapshot(path: &Path, why: &str) -> Error {
    Error::Snapshot(format!("{} is not a usable snapshot: {why}", path.display()))
}

// Its tests restore KVM machines.
#[cfg(all(test, target_os = "linux"))]
mod tests;
