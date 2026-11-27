// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A microVM's memory as a cladefs memory cell (TASK-456, board 13994), as cladefs's Firecracker
//! has it (vendor/firecracker's `cladefs_memory.rs`): restored from one, the guest's RAM is the
//! cell's window (`MemoryCell::window_in_ram`), the daemon's pages shared until written; a
//! snapshot into it writes the RAM's private pages and makes a version of the cell, or
//! checkpoints an ephemeral one; a cold-booted microVM's snapshot into one makes a new cell of
//! the pages its RAM holds.
//!
//! A `Diff` snapshot holds what the guest wrote since the last snapshot. KVM's dirty log and
//! vm-memory's bitmap tell Firecracker; Hypervisor.framework tells nothing of the writes to a
//! page already the VM's own. So the diff is of contents ([`Diffs`]): the window's private pages
//! are all the guest wrote since the restore, the RAM's own copies, and of them, those whose
//! contents changed since the last snapshot, each page's digest kept as it was then, are what
//! the guest wrote since; the VMM's own writes, its devices', are among them as the guest's are.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use cladefs::CellVersion;
use cladefs::memory_cell::MemoryCell;
use libmachine::GuestRam;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The page a diff is of: the host's (16 KiB on Apple Silicon), whole, as macOS's file systems
/// keep a sparse file's holes: APFS allocates a host page for a smaller write, its rest zeros, and
/// `SEEK_DATA` says so, so that a page of the cell's (4 KiB) written alone would carry its
/// neighbours as zeros into the diff, which, applied (`MemoryCell::apply_diff_files`), overwrite
/// what an older diff holds there (measured 2026-11-27: fxfs-guest-live-migration's destination
/// lost 320 pages so). The window's private pages are host pages too.
fn page() -> u64 {
    cladefs::pages::page_size()
}

/// cladefs's `memory_cell` of `PUT /snapshot/load`: the cell a `Cell` memory backend restores
/// from, as Firecracker's `MemoryCellConfig` says it.
pub(crate) struct LoadCell {
    /// The backend's path: the memory cell's store, a directory or a managed cell's `TENANT/ID`.
    store: PathBuf,
    from: CellVersion,
    cladefsd_socket: PathBuf,
    /// An ephemeral cell reopened in place, rather than a version forked.
    ephemeral: bool,
    /// Diff snapshots' memory files, oldest first, applied to the cell before its window is made.
    diff_files: Vec<PathBuf>,
}

impl LoadCell {
    /// The cell `config`, a load's `memory_cell`, names, at `store`, the backend's path.
    pub(crate) fn parse(store: &str, config: &Value) -> Result<Self, String> {
        if !config.is_object() {
            return Err(
                "a Cell memory backend needs memory_cell, naming the cell and its daemon".into()
            );
        }
        let number = |key: &str| config[key].as_u64().unwrap_or(0);
        let ephemeral = config["ephemeral"].as_bool().unwrap_or(false);
        let from = CellVersion {
            tenant: number("tenant"),
            cell: number("cell"),
            version: number("version"),
        };
        if ephemeral && (from.version != 0 || (from.cell == 0 && from.tenant != 0)) {
            return Err(
                "an ephemeral memory cell has no version, and a tenant only with a cell".into()
            );
        }
        let cladefsd_socket = (config["cladefsd_socket"].as_str())
            .ok_or("memory_cell names no cladefsd_socket")?
            .into();
        let diff_files = match &config["diff_files"] {
            Value::Null => Vec::new(),
            Value::Array(files) => (files.iter())
                .map(|file| file.as_str().map(PathBuf::from).ok_or("a diff file is a path"))
                .collect::<Result<_, _>>()?,
            _ => return Err("memory_cell's diff_files is a list of paths".into()),
        };
        Ok(Self { store: store.into(), from, cladefsd_socket, ephemeral, diff_files })
    }
}

/// A restored microVM's memory: its memory cell, and the RAM that is its window.
pub(crate) struct CellMemory {
    /// Shared with what takes the pages the guest gives back ([`CellMemory::take_given_back`]).
    cell: Arc<MemoryCell>,
    ram: Arc<GuestRam>,
    /// What the last snapshot held of the RAM's private pages.
    diffs: Diffs,
}

impl CellMemory {
    /// The memory `load` names, of `size` bytes, its window the RAM made for it, every page of it
    /// there as this returns, the guest's RAM not mapped yet.
    pub(crate) fn open(load: &LoadCell, size: usize) -> Result<Self, String> {
        let size64 = size as u64;
        let opening = |e: cladefs::Error| {
            format!("opening the memory cell at {}: {e:#}", load.store.display())
        };
        let cell = match load.ephemeral {
            true => {
                let expected = (load.from.cell != 0)
                    .then_some(cladefs::CellId { tenant: load.from.tenant, id: load.from.cell });
                MemoryCell::open_ephemeral_existing(
                    &load.cladefsd_socket,
                    &load.store,
                    expected,
                    size64,
                )
            }
            false => MemoryCell::open_forked(&load.cladefsd_socket, &load.store, load.from, size64),
        }
        .map_err(opening)?;
        if !load.diff_files.is_empty() {
            let diffs: Vec<&Path> = load.diff_files.iter().map(PathBuf::as_path).collect();
            cell.apply_diff_files(&diffs)
                .map_err(|e| format!("applying memory diff files to the memory cell: {e:#}"))?;
        }
        let ram = GuestRam::reserve(size).map_err(|e| e.to_string())?;
        cell.window_in_ram(ram.clone())
            .map_err(|e| format!("making the memory cell the guest's memory: {e:#}"))?;
        Ok(Self { cell: Arc::new(cell), ram, diffs: Diffs::default() })
    }

    /// The RAM that is its window.
    pub(crate) fn ram(&self) -> &Arc<GuestRam> {
        &self.ram
    }

    /// Makes the last snapshot one of all of the RAM taken now, of `base`, a memory file's: the
    /// next diff is of what the guest writes from now.
    ///
    /// # Safety
    ///
    /// As [`CellMemory::snapshot`]'s.
    pub(crate) unsafe fn snapshotted(&mut self, base: NonNull<u8>) -> Result<(), String> {
        // SAFETY: As the caller promises.
        unsafe { self.diffs.since(&self.cell, base) }
    }

    /// Has the pages the guest gives back (its balloon's) reclaimed through the window, the RAM's
    /// own copies there ended (`MemoryCell::reclaim`, board 8497).
    pub(crate) fn take_given_back(&self) {
        let cell = Arc::downgrade(&self.cell);
        self.ram.on_given_back(move |offset, len| match cell.upgrade() {
            Some(cell) => cell.reclaim(offset, len).map_err(|e| format!("{e:#}")),
            // The memory went with the microVM: nothing to give back to.
            None => Ok(()),
        });
    }

    /// Snapshots the memory into its cell, ephemeral or not as it was opened (`ephemeral` must
    /// say so, or not at all of a cell that is): its private pages written, committed and
    /// mapped anew from the cell, a version made, or the ephemeral cell checkpointed. Answered as
    /// Firecracker answers it.
    ///
    /// # Safety
    ///
    /// The microVM is paused; `base` is its RAM, the window.
    pub(crate) unsafe fn snapshot(
        &mut self,
        base: NonNull<u8>,
        ephemeral: bool,
    ) -> Result<Value, String> {
        let failed =
            |e: cladefs::Error| format!("snapshotting guest memory into its memory cell: {e:#}");
        let made = if self.cell.is_ephemeral() {
            // SAFETY: As the caller promises.
            let cell = unsafe { self.cell.snapshot_window_ephemeral(base) }.map_err(failed)?;
            json!({ "tenant": cell.tenant, "cell": cell.id, "version": 0, "ephemeral": true })
        } else if ephemeral {
            return Err("memory_cell.ephemeral can't snapshot a microVM restored from a bucket's \
                        memory cell"
                .into());
        } else {
            // SAFETY: As the caller promises.
            let version = unsafe { self.cell.snapshot_window(base) }.map_err(failed)?;
            json!({ "tenant": version.tenant, "cell": version.cell, "version": version.version })
        };
        // No private page left: the next diff is of what the guest writes from now.
        // SAFETY: As the caller promises.
        unsafe { self.diffs.since(&self.cell, base) }?;
        Ok(made)
    }

    /// Writes what the guest wrote since the last snapshot to a sparse file at `path` of the
    /// memory's size, at its offsets, as Firecracker's `Diff` snapshot writes it.
    ///
    /// # Safety
    ///
    /// As [`CellMemory::snapshot`]'s.
    pub(crate) unsafe fn write_diff(
        &mut self,
        base: NonNull<u8>,
        path: &Path,
    ) -> Result<(), String> {
        use std::os::unix::fs::FileExt as _;
        // SAFETY: As the caller promises.
        let dirty = unsafe { self.diffs.dirty(&self.cell, base) }?;
        let io = |e: std::io::Error| format!("writing the memory diff {}: {e}", path.display());
        let file = std::fs::File::create(path).map_err(io)?;
        let size = self.cell.size();
        file.set_len(size).map_err(io)?;
        let mut written = 0;
        for run in dirty {
            // SAFETY: Within the RAM, which the paused microVM doesn't change.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    base.as_ptr().add(run.start as usize),
                    (run.end - run.start) as usize,
                )
            };
            file.write_all_at(bytes, run.start).map_err(io)?;
            punch_hole(&file, written, run.start - written).map_err(io)?;
            written = run.end;
        }
        punch_hole(&file, written, size - written).map_err(io)?;
        file.sync_all().map_err(io)
    }
}

/// Makes the `len` bytes at `offset` of `file` a hole (`F_PUNCHHOLE`): a diff's pages the guest
/// didn't write. APFS gives a file grown past what was written no holes of its own on a volume of
/// the boot container, its gaps allocated zeros (measured 2026-11-27): which a diff applied
/// (`SEEK_DATA`, `MemoryCell::apply_diff_files`) would take for pages of zeros the guest wrote.
fn punch_hole(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    if len == 0 {
        return Ok(());
    }
    let hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: len as libc::off_t,
    };
    // SAFETY: A plain system call on an open descriptor, with a struct of its own.
    match unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &hole) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// What a memory's last snapshot held of its window's private pages: each one's digest then,
/// by offset, a host page ([`page`]).
#[derive(Default)]
struct Diffs {
    digests: HashMap<u64, [u8; 16]>,
}

impl Diffs {
    /// The pages of `cell`'s window, at `base`, the guest wrote since the last snapshot, in runs:
    /// its private pages whose contents changed since, or that weren't private then; then the
    /// last snapshot is this one.
    ///
    /// # Safety
    ///
    /// `base` is `cell`'s window, which nothing changes meanwhile.
    unsafe fn dirty(
        &mut self,
        cell: &MemoryCell,
        base: NonNull<u8>,
    ) -> Result<Vec<Range<u64>>, String> {
        let runs = cell.private_runs().map_err(|e| format!("the memory's private pages: {e:#}"))?;
        let mut digests = HashMap::with_capacity(self.digests.len());
        let mut dirty: Vec<Range<u64>> = Vec::new();
        let page = page();
        for run in runs {
            for at in (run.start..run.end).step_by(page as usize) {
                // SAFETY: A page of the window, as the caller promises.
                let bytes = unsafe {
                    std::slice::from_raw_parts(base.as_ptr().add(at as usize), page as usize)
                };
                let digest: [u8; 16] = Sha256::digest(bytes)[..16].try_into().expect("16 bytes");
                if self.digests.get(&at) != Some(&digest) {
                    match dirty.last_mut() {
                        Some(last) if last.end == at => last.end += page,
                        _ => dirty.push(at..at + page),
                    }
                }
                digests.insert(at, digest);
            }
        }
        self.digests = digests;
        Ok(dirty)
    }

    /// Makes the last snapshot one taken now, of `cell`'s window at `base`, which held all of it.
    ///
    /// # Safety
    ///
    /// As [`Diffs::dirty`]'s.
    unsafe fn since(&mut self, cell: &MemoryCell, base: NonNull<u8>) -> Result<(), String> {
        // SAFETY: As the caller promises.
        unsafe { self.dirty(cell, base) }.map(drop)
    }
}

/// Snapshots a cold-booted microVM's memory, the `size` bytes at `base`, into a new memory cell
/// at `path` of the daemon at `socket`, ephemeral if `ephemeral`: the pages its RAM holds, those
/// of zeros as holes, committed, and a version made of it, or the ephemeral cell left as it is.
/// Answered as Firecracker answers it.
///
/// # Safety
///
/// The microVM is paused; `base` is its RAM.
pub(crate) unsafe fn snapshot_into_new(
    socket: &Path,
    path: &Path,
    base: NonNull<u8>,
    size: usize,
    ephemeral: bool,
) -> Result<Value, String> {
    let failed = |e: cladefs::Error| format!("snapshotting guest memory into a memory cell: {e:#}");
    let cell = match ephemeral {
        true => MemoryCell::open_ephemeral(socket, path, size as u64),
        false => MemoryCell::open(socket, path, size as u64),
    }
    .map_err(|e| format!("opening the memory cell at {}: {e:#}", path.display()))?;
    // SAFETY: As the caller promises.
    unsafe { cell.write_resident(base) }.map_err(failed)?;
    cell.commit().map_err(failed)?;
    let (tenant, id) = (cell.tenant(), cell.cell());
    if ephemeral {
        cell.close().map_err(failed)?;
        return Ok(json!({ "tenant": tenant, "cell": id, "version": 0, "ephemeral": true }));
    }
    let version = cell.make_version().map_err(failed)?;
    cell.close().map_err(failed)?;
    Ok(json!({ "tenant": version.tenant, "cell": version.cell, "version": version.version }))
}
