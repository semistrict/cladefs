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
//!         0 (u32), the state's length (u64), the RAM's offset (u64), the
//!         state's SHA-256 (32 bytes)
//! 64      the state: what the machine is (CPU model and fingerprint, vCPU
//!         count, RAM size, the memory layout, each RAM region's offset in
//!         the file), then KVM's VM state, every vCPU's, the UART's and
//!         every PCI function's with its device's (snapshot::codec)
//! RAM     each RAM region in address order, each at a multiple of 2 MiB,
//!         pages of zeroes left as holes
//! ```
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

pub(crate) mod codec;

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::cpu::CpuModel;
use crate::error::{Error, Result};
use crate::machine::MachineConfig;
use crate::memory::{MemoryLayout, MemoryRegion, RegionKind};
use codec::{Decoder, Encoder};

/// The snapshot format this libmachine writes and reads; any other is
/// refused.
pub const FORMAT_VERSION: u32 = 2;

const MAGIC: [u8; 8] = *b"lmsnap\0\0";
const HEADER_LEN: usize = 64;
/// RAM regions start at multiples of this in the file, so that the host
/// can map them with huge pages.
const RAM_ALIGN: u64 = 2 << 20;
const PAGE: usize = 4096;

/// What a snapshot's machine is, at the head of its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineHeader {
    pub(crate) cpu_model: CpuModel,
    pub(crate) cpu_fingerprint: String,
    pub(crate) vcpus: u32,
    pub(crate) mem_size: u64,
    pub(crate) layout: MemoryLayout,
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
        Ok(Self { cpu_model, cpu_fingerprint, vcpus, mem_size, layout: MemoryLayout { regions } })
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

/// Writes a snapshot of a machine: `machine` and `state` (encoded by the
/// machine), and the guest's RAM from `mem`. Every vCPU is stopped and
/// nothing writes guest memory while this runs.
pub(crate) fn write(
    path: &Path,
    machine: &MachineHeader,
    state: &[u8],
    mem: &GuestMemoryMmap,
) -> Result<()> {
    let io = |source| Error::Io { path: path.to_owned(), source };
    let mut head = Encoder::default();
    machine.encode(&mut head);
    let mut all = head.finish();
    all.extend_from_slice(state);
    let ram_offset = (HEADER_LEN as u64 + all.len() as u64).next_multiple_of(RAM_ALIGN);
    let regions = ram_offsets(&machine.layout, ram_offset);
    let end = regions.last().map_or(ram_offset, |(r, offset)| offset + r.size);

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&(all.len() as u64).to_le_bytes());
    header.extend_from_slice(&ram_offset.to_le_bytes());
    header.extend_from_slice(&Sha256::digest(&all));
    debug_assert_eq!(header.len(), HEADER_LEN);

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
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(&header)?;
        file.write_all(&all)?;
        file.set_len(end)?;
        for (region, offset) in &regions {
            let host =
                mem.get_host_address(GuestAddress(region.start)).map_err(std::io::Error::other)?;
            // SAFETY: the region is guest RAM, mapped for as long as `mem`
            // lives, and nothing writes it while a snapshot is taken (every
            // vCPU is stopped, the device thread held off).
            let ram = unsafe { std::slice::from_raw_parts(host, region.size as usize) };
            write_sparse(&file, ram, *offset)?;
        }
        file.sync_data()?;
        file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
        std::fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()
    })();
    if let Err(e) = written {
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
    file: Arc<File>,
    machine: MachineHeader,
    /// Each RAM region's offset in the file, in the layout's order.
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
    /// state's checksum, and that the file holds all of its RAM.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
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
        if state_len > file_len || HEADER_LEN as u64 + state_len > ram_offset {
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
        let regions = ram_offsets(&machine.layout, ram_offset);
        let end = regions.last().map_or(ram_offset, |(r, offset)| offset + r.size);
        if file_len < end {
            return Err(not_a_snapshot(
                path,
                &format!("it ends at {file_len} bytes, before its RAM's end at {end}"),
            ));
        }
        let ram_offsets = regions.iter().map(|&(_, offset)| offset).collect();
        let rest = state.split_off(machine_len);
        Ok(Self { path: path.to_owned(), file: Arc::new(file), machine, ram_offsets, state: rest })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configuration of the machine the snapshot was taken of (no
    /// network, no extent store devices: a machine with either is never
    /// snapshotted). Host-side
    /// settings the snapshot does not record, such as
    /// [`serial_capture_limit`](MachineConfig::serial_capture_limit), are
    /// the defaults; a restore may choose them freely.
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            mem_size: self.machine.mem_size as usize,
            cpu_model: self.machine.cpu_model,
            vcpus: self.machine.vcpus,
            network: None,
            serial_capture_limit: MachineConfig::DEFAULT_SERIAL_CAPTURE_LIMIT,
            extent_daemon: None,
            extent_stores: Vec::new(),
            nested_virtualization: false,
            balloon: false,
            disks: Vec::new(),
        }
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

    pub(crate) fn file(&self) -> &Arc<File> {
        &self.file
    }

    pub(crate) fn ram_offsets(&self) -> &[u64] {
        &self.ram_offsets
    }

    /// The state past the machine's header.
    pub(crate) fn state(&self) -> Decoder<'_> {
        Decoder::new(&self.state)
    }

    /// Checks that a machine of `config` is the snapshot's: what a restore
    /// can tell before it builds one (the CPU fingerprint is checked on the
    /// machine it builds).
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
