// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One VM's memory backend: its memory cell's image served as the VM's guest memory, lazily.
//! See the module docs ([`super`]).

use super::protocol::{self, Entry, FromVmm, GuestRegion, ToVmm};
use super::uffd::{Fault, Uffd};
use crate::server::Tenant;
use block_device::{BlockDevice, Contents};
use cladefs_core::ExtentId;
use cladefs_core::cache::CachedExtent;
use cladefs_core::dax::Source;
use cladefs_core::open_cell::OpenCell;
use cladefs_core::pages::page_size;
use cladefs_core::slabs::{ArenaId, PhysLocation};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long a map waits for Firecracker's answer before the VM is taken for gone.
const ANSWER: Duration = Duration::from_secs(30);

/// Serves the memory of the one VM process that connects to `listener`, its tenant `tenant`'s,
/// from the memory cell at `path`, on a thread of its own, until the VM process goes: what the
/// thread came to.
pub fn serve(
    listener: UnixListener,
    tenant: Arc<Tenant>,
    path: PathBuf,
) -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::Builder::new()
        .name("cladefsd memory".into())
        .spawn(move || {
            let served = Vm::connect(listener, tenant, &path).and_then(|vm| vm.run());
            if let Err(err) = &served {
                log::error!("cladefsd: serving the memory of {}: {err}", path.display());
            }
            served
        })
        .expect("a thread for a VM's memory")
}

/// A piece of a region a file entry maps: of a placed part of an extent.
struct Piece {
    len: u64,
    cached: Arc<CachedExtent>,
    /// The extent's page the piece starts at.
    page: u64,
    /// Where that page is, in its arena: the part's home, or its slab, as it was placed.
    at: PhysLocation,
}

/// A region of the VM's memory, as served.
struct Region {
    described: GuestRegion,
    /// The image's nodes mapped, by index (guest memory's regions alone).
    mapped: HashSet<u64>,
    /// The pieces file entries map, by their offsets in the region.
    pieces: BTreeMap<u64, Piece>,
}

/// One VM's memory, served.
struct Vm {
    tenant: Arc<Tenant>,
    cell: Arc<OpenCell>,
    blocks: Arc<BlockDevice>,
    /// Frames to the VM process, from this thread and the reader.
    writer: Arc<Mutex<UnixStream>>,
    uffd: Uffd,
    regions: Vec<Region>,
    /// Firecracker's answers to maps, by ID, from the reader.
    answers: Receiver<(u64, Result<(), String>)>,
    next_id: u64,
    /// Whether the VM process went, which the reader says.
    gone: Arc<AtomicBool>,
    /// The placements the VM's lists hold: each to be ended.
    placements: Vec<(Arc<CachedExtent>, std::ops::Range<u64>)>,
    runtime: tokio::runtime::Handle,
}

impl Vm {
    /// Takes the VM process that connects to `listener`: its handshake, its userfaultfd, its
    /// memory cell opened, its regions listed lazy, and it told it may start.
    fn connect(listener: UnixListener, tenant: Arc<Tenant>, path: &Path) -> Result<Self, String> {
        let (stream, _) = listener.accept().map_err(|err| format!("accepting: {err}"))?;
        drop(listener);
        let (handshake, mut fds) =
            protocol::recv::<FromVmm>(&stream).map_err(|err| err.to_string())?;
        let FromVmm::Handshake { regions } = handshake else {
            return Err(format!("{handshake:?} in place of a handshake"));
        };
        if fds.len() != 1 {
            return Err(format!("{} descriptors with the handshake, not a userfaultfd", fds.len()));
        }
        let uffd = Uffd::new(fds.remove(0));
        let ps = page_size();
        for region in &regions {
            let aligned = |n: u64| n.is_multiple_of(ps);
            let size = region.size as u64;
            if !aligned(region.base_host_virt_addr)
                || !aligned(size)
                || !aligned(region.offset)
                || region.page_size as u64 != ps
                || region.base_host_virt_addr.checked_add(size).is_none()
                || region.offset.checked_add(size).is_none()
            {
                return Err(format!("{region:?} is not of whole host pages"));
            }
        }
        let runtime = tenant.daemon().runtime().handle().clone();
        // Its writer, in the daemon's table, as a VM process's connection would be: what waits
        // for a writer going to let it go, and keeps any other from opening it meanwhile.
        let arena = tenant.memory_arena().clone();
        let embedded = runtime
            .block_on(crate::embedded::embed_at(tenant.daemon(), path, arena))
            .map_err(|errno| format!("opening its memory cell: {errno:?}"))?;
        let cell = embedded.open_cell().clone();
        let device =
            embedded.device().map_err(|err| format!("its memory cell as a device: {err:#}"))?;
        let blocks = BlockDevice::open(
            storage_device::DeviceHolder::new(device),
            block_device::Options::default(),
        )
        .map_err(|err| format!("its memory cell's image: {err:#}"))?;
        let writer = Arc::new(Mutex::new(stream.try_clone().map_err(|err| err.to_string())?));
        let mut vm = Self {
            tenant,
            cell,
            blocks,
            writer: writer.clone(),
            uffd,
            regions: (regions.into_iter())
                .map(|described| Region {
                    described,
                    mapped: HashSet::new(),
                    pieces: BTreeMap::new(),
                })
                .collect(),
            answers: mpsc::sync_channel(0).1,
            next_id: 0,
            gone: Arc::default(),
            placements: Vec::new(),
            runtime,
        };
        // Its regions listed before anything reads its socket but this thread: guest memory's
        // lazy, the window's (a pmem device's) nothing yet.
        for index in 0..vm.regions.len() {
            let described = &vm.regions[index].described;
            let len = described.size as u64;
            let entry = match described.pmem_id {
                None => Entry::Lazy { offset: 0, len },
                Some(_) => Entry::None { offset: 0, len },
            };
            let id = vm.next_id();
            protocol::send(&stream, &ToVmm::Map { id, region: index, entries: vec![entry] }, &[])
                .map_err(|err| err.to_string())?;
            match protocol::recv::<FromVmm>(&stream).map_err(|err| err.to_string())?.0 {
                FromVmm::Mapped { id: answered } if answered == id => {}
                answer => return Err(format!("{answer:?} in answer to listing region {index}")),
            }
        }
        protocol::send(&stream, &ToVmm::Ready, &[]).map_err(|err| err.to_string())?;
        vm.answers = read_on(stream, writer, vm.gone.clone());
        Ok(vm)
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Serves its faults until the VM process goes.
    fn run(mut self) -> Result<(), String> {
        while !self.gone.load(Ordering::Acquire) {
            // Between maps, no answer is due.
            match self.answers.try_recv() {
                Ok((id, _)) => return Err(format!("an answer to map {id}, which none waited for")),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
            let Some(fault) = self.uffd.next(100).map_err(|err| format!("its faults: {err}"))?
            else {
                continue;
            };
            self.fault(fault)?;
        }
        Ok(())
    }

    /// Resolves `fault`: of a lazy range of guest memory, the image's node around it mapped, its
    /// pages installed, and every fault on the node woken; of a node mapped, its page installed if
    /// a file entry maps it, and the fault woken. A fault this reads may be one woken already: a
    /// second of a node, before its map, on a page the map installed, or zeros.
    fn fault(&mut self, fault: Fault) -> Result<(), String> {
        let ps = page_size();
        let address = fault.address & !(ps - 1);
        let index = (self.regions.iter())
            .position(|r| {
                let base = r.described.base_host_virt_addr;
                (base..base + r.described.size as u64).contains(&address)
            })
            .ok_or_else(|| format!("a fault at {address:#x}, in no region"))?;
        let offset = address - self.regions[index].described.base_host_virt_addr;
        if self.regions[index].described.pmem_id.is_some() {
            return Err(format!(
                "a fault at {address:#x} of a pmem region, which maps nothing yet"
            ));
        }
        let node = self.node_of(index, offset);
        let woken = if self.regions[index].mapped.contains(&node) {
            self.install(index, offset, ps)?;
            address..address + ps
        } else {
            // Woken, each faulting thread finds the node's mapping: installed, zeros, or a fault
            // on it, served here.
            self.map_node(index, node)?
        };
        let (start, len) = (woken.start, woken.end - woken.start);
        self.uffd.wake(start, len).map_err(|err| format!("waking faults: {err}"))
    }

    /// The image's node holding region `index`'s byte `offset`.
    fn node_of(&self, index: usize, offset: u64) -> u64 {
        (self.regions[index].described.offset + offset) / self.blocks.node_bytes()
    }

    /// Maps image node `node`'s part of region `index` anew, over its lazy entry: each run of
    /// the image's data its extent's part, placed, a hole zeros; then installs its pages ahead of
    /// the guest's touches. Returns where it maps, in the VM process.
    fn map_node(&mut self, index: usize, node: u64) -> Result<std::ops::Range<u64>, String> {
        let ps = page_size();
        let described = self.regions[index].described.clone();
        let node_bytes = self.blocks.node_bytes();
        let start = (node * node_bytes).max(described.offset);
        let end = ((node + 1) * node_bytes).min(described.offset + described.size as u64);
        let runs = self.blocks.runs(start, end - start).map_err(|err| format!("{err:#}"))?;
        let mut entries = Vec::new();
        let mut fds: Vec<RawFd> = Vec::new();
        let mut arenas: HashMap<ArenaId, usize> = HashMap::new();
        let mut pieces = Vec::new();
        for (range, contents) in runs {
            let offset = range.start - described.offset;
            let len = range.end - range.start;
            let (extent, at) = match contents {
                None | Some(Contents::Zeros) => {
                    entries.push(Entry::Zeros { offset, len });
                    continue;
                }
                Some(Contents::Extent { extent, offset: at }) => {
                    (ExtentId { cell: extent.cell, sequence: extent.sequence }, at)
                }
            };
            let (cached, within) = (self.runtime)
                .block_on(self.cell.extent_for_list(extent, at, len))
                .map_err(|errno| format!("{extent:?} at {at:#x}: {errno:?}"))?;
            if within > 0 {
                let pages = at / ps..(at + within).div_ceil(ps);
                let placed = (self.runtime)
                    .block_on(cached.place(pages.clone()))
                    .map_err(|errno| format!("placing {extent:?}: {errno:?}"))?;
                self.placements.push((cached.clone(), pages));
                for placement in placed {
                    let part = placement.pages.start * ps..placement.pages.end * ps;
                    let (from, to) = (part.start.max(at), part.end.min(at + within));
                    if from >= to {
                        continue;
                    }
                    let fd = match arenas.get(&placement.at.arena) {
                        Some(fd) => *fd,
                        None => {
                            let memory = (self.tenant.arena_memory(placement.at.arena))
                                .ok_or_else(|| format!("{extent:?} of another arena"))?;
                            fds.push(memory.as_raw_fd());
                            arenas.insert(placement.at.arena, fds.len() - 1);
                            fds.len() - 1
                        }
                    };
                    let file_offset = placement.at.offset + (from - part.start);
                    let piece_offset = offset + (from - at);
                    entries.push(Entry::File {
                        fd,
                        file_offset,
                        offset: piece_offset,
                        len: to - from,
                    });
                    let first = PhysLocation { arena: placement.at.arena, offset: file_offset };
                    let piece = Piece {
                        len: to - from,
                        cached: cached.clone(),
                        page: from / ps,
                        at: first,
                    };
                    pieces.push((piece_offset, piece));
                }
            }
            if within < len {
                entries.push(Entry::Zeros { offset: offset + within, len: len - within });
            }
        }
        let id = self.next_id();
        let map = ToVmm::Map { id, region: index, entries };
        protocol::send(&self.writer.lock().unwrap(), &map, &fds).map_err(|err| err.to_string())?;
        self.answered(id)?;
        let region = &mut self.regions[index];
        region.mapped.insert(node);
        let installs: Vec<(u64, u64)> =
            pieces.iter().map(|(offset, piece)| (*offset, piece.len)).collect();
        region.pieces.extend(pieces);
        // Ahead of the guest's touches: each piece's pages in, installed.
        for (offset, len) in installs {
            self.install(index, offset, len)?;
        }
        let host = |at: u64| described.base_host_virt_addr + (at - described.offset);
        Ok(host(start)..host(end))
    }

    /// Waits for Firecracker's answer to map `id`.
    fn answered(&self, id: u64) -> Result<(), String> {
        // One map at a time: the next answer is this one's.
        match self.answers.recv_timeout(ANSWER) {
            Ok((answered, result)) if answered == id => result,
            Ok((answered, _)) => Err(format!("an answer to map {answered}, waiting for {id}'s")),
            Err(RecvTimeoutError::Timeout) => Err(format!("no answer to map {id}")),
            Err(RecvTimeoutError::Disconnected) => Err("the VM process went".into()),
        }
    }

    /// Installs the `len` bytes at region `index`'s `offset`, of the pieces mapped there: each
    /// page's part paged in, then the page installed from its arena if the part is where its
    /// entry maps it, else a copy of its bytes; under the part's lock, which an eviction takes, so
    /// that nothing moves it between the look and the install (specs/placed-parts,
    /// checkUnderLock).
    fn install(&self, index: usize, offset: u64, len: u64) -> Result<(), String> {
        let ps = page_size();
        let region = &self.regions[index];
        let base = region.described.base_host_virt_addr;
        let mut at = offset;
        while at < offset + len {
            let Some((&piece_offset, piece)) = region.pieces.range(..=at).next_back() else {
                at += ps;
                continue;
            };
            if at >= piece_offset + piece.len {
                at += ps;
                continue;
            }
            let page = piece.page + (at - piece_offset) / ps;
            let expected = PhysLocation {
                arena: piece.at.arena,
                offset: piece.at.offset + (at - piece_offset),
            };
            loop {
                (self.runtime)
                    .block_on(piece.cached.clone().bring_in(page))
                    .map_err(|errno| format!("paging in: {errno:?}"))?;
                let mut installed = None;
                piece.cached.each_present(page..page + 1, &mut |present| {
                    let address = base + at;
                    installed = Some(
                        if let Some(slab) = present.slab
                            && present.at == expected
                        {
                            // In the VM's page tables from now on: punched as it is freed.
                            slab.list();
                            self.uffd.continue_(address, ps)
                        } else {
                            let mut bytes = vec![0; ps as usize];
                            present
                                .slab
                                .map_or(Err(std::io::ErrorKind::NotFound.into()), |slab| {
                                    slab.read(
                                        present.at.offset - slab.location().offset,
                                        &mut bytes,
                                    )
                                })
                                .and_then(|()| self.uffd.copy(address, &bytes))
                        },
                    );
                });
                match installed {
                    // Evicted again before the look: paged in again.
                    None => continue,
                    Some(Ok(())) => break,
                    Some(Err(err)) if err.raw_os_error() == Some(libc::EEXIST) => break,
                    Some(Err(err)) => return Err(format!("installing {:#x}: {err}", base + at)),
                }
            }
            at += ps;
        }
        Ok(())
    }
}

impl Drop for Vm {
    /// Its placements ended, and its socket shut, which the reader holds too: the VM process
    /// sees its backend gone, and goes (Firecracker's `MemoryBackendLost`), rather than leave a
    /// vCPU waiting for ever on a fault no one serves.
    fn drop(&mut self) {
        for (cached, pages) in self.placements.drain(..) {
            cached.unplace(pages);
        }
        let writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = writer.shutdown(Shutdown::Both)
            && err.kind() != std::io::ErrorKind::NotConnected
        {
            log::error!("cladefsd: shutting a VM's memory backend socket: {err}");
        }
    }
}

/// Reads the VM process's frames on a thread of its own: its answers to maps, passed on, and a
/// persist, answered (board 14695 serves it); the VM process going, said in `gone`. An answer
/// waits to be taken before the next frame is read, so that a VM process answering what was never
/// asked holds up its own socket, not the daemon's memory.
fn read_on(
    stream: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    gone: Arc<AtomicBool>,
) -> Receiver<(u64, Result<(), String>)> {
    let (answers, answered) = mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name("cladefsd memory reader".into())
        .spawn(move || {
            loop {
                let message = match protocol::recv::<FromVmm>(&stream) {
                    Ok((message, _)) => message,
                    Err(protocol::FrameError::Gone) => break,
                    Err(err) => {
                        log::error!("cladefsd: a VM's memory backend: {err}");
                        break;
                    }
                };
                let answer = match message {
                    FromVmm::Mapped { id } => (id, Ok(())),
                    FromVmm::MapRefused { id, error } => (id, Err(error)),
                    FromVmm::Persist => {
                        let result = serde_json::json!({
                            "error": "the memory backend persists no memory yet (board 14695)"
                        });
                        let persisted = ToVmm::Persisted { result };
                        if protocol::send(&writer.lock().unwrap(), &persisted, &[]).is_err() {
                            break;
                        }
                        continue;
                    }
                    FromVmm::Handshake { .. } => {
                        log::error!("cladefsd: a VM's memory backend: a second handshake");
                        break;
                    }
                };
                if answers.send(answer).is_err() {
                    break;
                }
            }
            gone.store(true, Ordering::Release);
        })
        .expect("a thread reading a VM's memory backend");
    answered
}
