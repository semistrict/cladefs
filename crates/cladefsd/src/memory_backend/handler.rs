// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One VM's memory backend: its memory cell's image served as the VM's guest memory, lazily.
//! See the module docs ([`super`]).

use super::pagemap;
use super::peer::Peer;
use super::protocol::{self, Entry, FromVmm, GuestRegion, ToVmm};
use super::uffd::{Fault, Next, Uffd};
use crate::server::Tenant;
use block_device::{BlockDevice, Contents};
use cladefs_core::cache::CachedExtent;
use cladefs_core::dax::Source;
use cladefs_core::open_cell::OpenCell;
use cladefs_core::pages::page_size;
use cladefs_core::slabs::{ArenaId, PhysLocation};
use cladefs_core::{CellVersion, ExtentId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::Shutdown;
use std::ops::Range;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long a map waits for Firecracker's answer before the VM is taken for gone.
const ANSWER: Duration = Duration::from_secs(30);

/// How much of a VM's private memory a persist reads at a time.
const PERSIST_CHUNK: usize = 1 << 20;

/// Serves the memory of the one VM process that connects to `listener`, its tenant `tenant`'s,
/// from the memory cell at `path`, on a thread of its own, until the VM process goes: what the
/// thread came to. A persist past `persist_deadline` kills the VM process: its snapshot then
/// fails, rather than hold it paused for ever.
pub fn serve(
    listener: UnixListener,
    tenant: Arc<Tenant>,
    path: PathBuf,
    persist_deadline: Duration,
) -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::Builder::new()
        .name("cladefsd memory".into())
        .spawn(move || {
            let connected = Vm::connect(listener, tenant, &path, persist_deadline);
            let served = connected.and_then(|vm| vm.run());
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

/// The parts a node's map placed, each a run of an extent's pages: unplaced as it goes, once
/// nothing maps them.
#[derive(Default)]
struct Placed(Vec<(Arc<CachedExtent>, Range<u64>)>);

impl Drop for Placed {
    fn drop(&mut self) {
        for (cached, pages) in self.0.drain(..) {
            cached.unplace(pages);
        }
    }
}

/// A region of the VM's memory, as served.
struct Region {
    described: GuestRegion,
    /// The image's nodes mapped, by index, and the parts each placed (guest memory's regions
    /// alone).
    nodes: HashMap<u64, Placed>,
    /// The pieces file entries map, by their offsets in the region.
    pieces: BTreeMap<u64, Piece>,
}

/// A persist's deadline: the VM process killed if it is not dropped in time, which the VM
/// process going then ends its handler, whatever it waits for.
struct Deadline(#[allow(dead_code, reason = "dropped, it lets the watch go")] mpsc::Sender<()>);

impl Deadline {
    fn arm(peer: &Peer, deadline: Duration) -> std::io::Result<Self> {
        let peer = peer.try_clone()?;
        let (armed, watch) = mpsc::channel::<()>();
        std::thread::Builder::new().name("cladefsd persist deadline".into()).spawn(move || {
            if watch.recv_timeout(deadline) == Err(RecvTimeoutError::Timeout) {
                log::error!("cladefsd: a VM's persist passed its deadline, {deadline:?}: killed");
                if let Err(err) = peer.kill() {
                    log::error!("cladefsd: killing a VM process past its persist: {err}");
                }
            }
        })?;
        Ok(Self(armed))
    }
}

/// What the reader rings as it passes an event on, or goes: an eventfd, which the fault thread
/// waits on with its faults, so that a persist starts at once.
struct Bell(OwnedFd);

impl Bell {
    fn new() -> std::io::Result<Self> {
        // SAFETY: Makes an eventfd, which this process owns at once.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: The descriptor just made.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn ring(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: Writes 8 bytes to the eventfd it owns. Full (it never is), it is rung already.
        unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    /// Silences it.
    fn answer(&self) {
        let mut count = [0u8; 8];
        // SAFETY: Reads the eventfd's count, 8 bytes, which resets it; not rung, nothing.
        unsafe { libc::read(self.0.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
    }
}

/// What the reader passes on of the VM process's frames.
enum Event {
    /// Firecracker's answer to a map, by its ID.
    Answer(u64, Result<(), String>),
    /// Persist guest memory: device state is saved, the vCPUs paused.
    Persist,
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
    /// What the reader passes on: Firecracker's answers to maps, and persists.
    events: Receiver<Event>,
    /// What the reader rings as it passes one on.
    bell: Arc<Bell>,
    /// Whether a persist came as a map waited for its answer: served after.
    persist_due: bool,
    next_id: u64,
    /// Whether the VM process went, which the reader says.
    gone: Arc<AtomicBool>,
    /// The VM process, whose memory a persist reads.
    peer: Peer,
    /// How long a persist may take.
    persist_deadline: Duration,
    runtime: tokio::runtime::Handle,
}

impl Vm {
    /// Takes the VM process that connects to `listener`: its handshake, its userfaultfd, its
    /// memory cell opened, its regions listed lazy, and it told it may start.
    fn connect(
        listener: UnixListener,
        tenant: Arc<Tenant>,
        path: &Path,
        persist_deadline: Duration,
    ) -> Result<Self, String> {
        let (stream, _) = listener.accept().map_err(|err| format!("accepting: {err}"))?;
        drop(listener);
        let peer = Peer::of(&stream).map_err(|err| format!("the VM process connected: {err}"))?;
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
                    nodes: HashMap::new(),
                    pieces: BTreeMap::new(),
                })
                .collect(),
            events: mpsc::sync_channel(0).1,
            bell: Arc::new(Bell::new().map_err(|err| format!("an eventfd: {err}"))?),
            persist_due: false,
            next_id: 0,
            gone: Arc::default(),
            peer,
            persist_deadline,
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
        vm.events = read_on(stream, vm.gone.clone(), vm.bell.clone());
        Ok(vm)
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Serves its faults, and its persists, until the VM process goes.
    fn run(mut self) -> Result<(), String> {
        while !self.gone.load(Ordering::Acquire) {
            if std::mem::take(&mut self.persist_due) {
                self.persist()?;
                continue;
            }
            let next = self.uffd.next(100, Some(self.bell.0.as_fd()));
            match next.map_err(|err| format!("its faults: {err}"))? {
                Next::Fault(fault) => {
                    self.fault(fault)?;
                    continue;
                }
                // Rung as the reader passed an event on: what shortens the wait for it.
                Next::Also => self.bell.answer(),
                Next::Nothing => {}
            }
            match self.events.try_recv() {
                Ok(Event::Persist) => self.persist()?,
                // Between maps, no answer is due.
                Ok(Event::Answer(id, _)) => {
                    return Err(format!("an answer to map {id}, which none waited for"));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
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
        let woken = if self.regions[index].nodes.contains_key(&node) {
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

    /// Maps image node `node`'s part of region `index` anew, over its lazy entry, or, in a
    /// persist, over its mapping: each run of the image's data its extent's part, placed, a hole
    /// zeros; then installs its pages ahead of the guest's touches. What the node's mapping
    /// placed before, it unplaces. Returns where it maps, in the VM process.
    fn map_node(&mut self, index: usize, node: u64) -> Result<Range<u64>, String> {
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
        let mut placed_parts = Placed::default();
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
                placed_parts.0.push((cached.clone(), pages));
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
        let (from, to) = (start - described.offset, end - described.offset);
        let stale: Vec<u64> = region.pieces.range(from..to).map(|(offset, _)| *offset).collect();
        for offset in stale {
            region.pieces.remove(&offset);
        }
        let installs: Vec<(u64, u64)> =
            pieces.iter().map(|(offset, piece)| (*offset, piece.len)).collect();
        region.pieces.extend(pieces);
        // What it mapped before, mapped no more.
        drop(region.nodes.insert(node, placed_parts));
        // Ahead of the guest's touches: each piece's pages in, installed.
        for (offset, len) in installs {
            self.install(index, offset, len)?;
        }
        let host = |at: u64| described.base_host_virt_addr + (at - described.offset);
        Ok(host(start)..host(end))
    }

    /// Waits for Firecracker's answer to map `id`.
    fn answered(&mut self, id: u64) -> Result<(), String> {
        loop {
            // One map at a time: the next answer is this one's.
            match self.events.recv_timeout(ANSWER) {
                Ok(Event::Answer(answered, result)) if answered == id => return result,
                Ok(Event::Answer(answered, _)) => {
                    return Err(format!("an answer to map {answered}, waiting for {id}'s"));
                }
                // Served once this map is done.
                Ok(Event::Persist) => self.persist_due = true,
                Err(RecvTimeoutError::Timeout) => return Err(format!("no answer to map {id}")),
                Err(RecvTimeoutError::Disconnected) => return Err("the VM process went".into()),
            }
        }
    }

    /// Persists its guest memory, Firecracker having saved its device state, its vCPUs paused,
    /// and answers what came of it: the version made, or why none was. Its private pages written
    /// into the memory cell's image, the image committed, a version made of it (an ephemeral
    /// cell checkpointed); then each node they were in mapped anew, from the image, so that what
    /// was private is shared again. Failing before the remap, it leaves the VM as it was, its
    /// pages private still; a remap failing ends the VM, its memory then neither.
    fn persist(&mut self) -> Result<(), String> {
        let _deadline = Deadline::arm(&self.peer, self.persist_deadline)
            .map_err(|err| format!("a persist's deadline: {err}"))?;
        let kept = (self.keep_private_pages_serving_faults())
            .and_then(|touched| Ok((touched, self.version()?)));
        let result = match kept {
            Ok((touched, version)) => {
                for (index, node) in touched {
                    self.map_node(index, node)?;
                }
                version
            }
            Err(err) => {
                log::error!("cladefsd: persisting a VM's memory: {err}");
                serde_json::json!({ "error": err })
            }
        };
        let persisted = ToVmm::Persisted { result };
        protocol::send(&self.writer.lock().unwrap(), &persisted, &[]).map_err(|err| err.to_string())
    }

    /// [`keep_private_pages`], on a thread of its own, this one serving the faults its reads
    /// meet meanwhile: none, its vCPUs paused, unless the VM process discarded a page it had since
    /// its scan (`MADV_DONTNEED`), whose read would wait on the userfaultfd that only this thread
    /// serves, for ever, a kill of the VM process ending nothing. Each such page is installed
    /// zeros, so that the read ends, and the persist fails.
    fn keep_private_pages_serving_faults(&self) -> Result<BTreeSet<(usize, u64)>, String> {
        let ps = page_size();
        let zeros = vec![0u8; ps as usize];
        std::thread::scope(|scope| {
            let (peer, regions, blocks) = (&self.peer, &self.regions[..], &*self.blocks);
            let keeping = scope.spawn(move || keep_private_pages(peer, regions, blocks));
            let mut faulted: Result<(), String> = Ok(());
            // No return while it runs: the scope would wait for it.
            while !keeping.is_finished() {
                let address = match self.uffd.next(10, None) {
                    Ok(Next::Fault(fault)) => fault.address & !(ps - 1),
                    Ok(Next::Nothing | Next::Also) => continue,
                    Err(err) => {
                        // Made blocking: its faults go unread, and a read waiting on one waits
                        // for ever; then nothing ends it.
                        faulted = faulted.and(Err(format!("its faults: {err}")));
                        break;
                    }
                };
                let installed = match self.uffd.copy(address, &zeros) {
                    Err(err) if err.raw_os_error() != Some(libc::EEXIST) => Err(err),
                    _ => self.uffd.wake(address, ps),
                };
                let at = format!("a fault at {address:#x} as its memory was persisted");
                faulted = faulted.and(Err(match installed {
                    Ok(()) => format!("{at}: a page discarded since its scan"),
                    Err(err) => format!("{at}, served zeros: {err}"),
                }));
            }
            let kept =
                keeping.join().map_err(|_| "keeping its private pages panicked".to_owned())?;
            faulted.and(kept)
        })
    }

    /// A version of its memory cell, made now; of an ephemeral cell, a checkpoint: what a persist
    /// answers.
    fn version(&self) -> Result<serde_json::Value, String> {
        let cell = self.cell.cell();
        if self.cell.is_ephemeral() {
            let committed = (self.runtime)
                .block_on(Box::pin(self.cell.quiesce_ephemeral()))
                .map_err(|err| format!("checkpointing its memory cell: {err:#}"))?;
            let checkpoint = serde_json::json!({
                "tenant": cell.tenant, "cell": cell.id, "committed": committed
            });
            return Ok(serde_json::json!({ "ephemeral": checkpoint }));
        }
        let version = (self.runtime)
            .block_on(Box::pin(self.cell.make_version(None)))
            .map_err(|err| format!("making a version of its memory cell: {err:#}"))?;
        let version = CellVersion { tenant: cell.tenant, cell: cell.id, version };
        Ok(serde_json::json!({ "version": version }))
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
        let writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = writer.shutdown(Shutdown::Both)
            && err.kind() != std::io::ErrorKind::NotConnected
        {
            log::error!("cladefsd: shutting a VM's memory backend socket: {err}");
        }
    }
}

/// Reads the VM process's frames on a thread of its own: its answers to maps, and its persists,
/// passed on; the VM process going, said in `gone`. Each waits to be taken before the next frame
/// is read, so that a VM process sending what was never asked holds up its own socket, not the
/// daemon's memory.
fn read_on(stream: UnixStream, gone: Arc<AtomicBool>, bell: Arc<Bell>) -> Receiver<Event> {
    let (events, passed) = mpsc::sync_channel(0);
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
                let event = match message {
                    FromVmm::Mapped { id } => Event::Answer(id, Ok(())),
                    FromVmm::MapRefused { id, error } => Event::Answer(id, Err(error)),
                    FromVmm::Persist => Event::Persist,
                    FromVmm::Handshake { .. } => {
                        log::error!("cladefsd: a VM's memory backend: a second handshake");
                        break;
                    }
                };
                bell.ring();
                if events.send(event).is_err() {
                    break;
                }
            }
            gone.store(true, Ordering::Release);
            bell.ring();
        })
        .expect("a thread reading a VM's memory backend");
    passed
}

/// Writes the private pages of `peer`, the VM process whose memory `regions` are, into its memory
/// cell's image, `blocks`, an all-zero one as a hole, and commits it: the nodes they were in, by
/// region.
fn keep_private_pages(
    peer: &Peer,
    regions: &[Region],
    blocks: &BlockDevice,
) -> Result<BTreeSet<(usize, u64)>, String> {
    let ps = page_size();
    let pagemap = peer.pagemap().map_err(|err| format!("its pagemap: {err}"))?;
    let mut touched = BTreeSet::new();
    let mut buf = vec![0u8; PERSIST_CHUNK];
    for (index, region) in regions.iter().enumerate() {
        let described = &region.described;
        // A window's private pages are board 14696's.
        if described.pmem_id.is_some() {
            continue;
        }
        let base = described.base_host_virt_addr;
        let runs = pagemap::private_runs(&pagemap, base, base + described.size as u64)
            .map_err(|err| format!("scanning its pagemap: {err}"))?;
        // A pagemap of a process gone scans as empty.
        peer.check().map_err(|err| format!("scanning its pagemap: {err}"))?;
        for run in runs {
            let mut at = run.start;
            while at < run.end {
                let bytes = &mut buf[..(run.end - at).min(PERSIST_CHUNK as u64) as usize];
                (peer.read(at, bytes))
                    .map_err(|err| format!("reading its memory at {at:#x}: {err}"))?;
                write_pages(blocks, described.offset + (at - base), bytes, ps)?;
                at += bytes.len() as u64;
            }
            let node_of = |at: u64| (described.offset + (at - base)) / blocks.node_bytes();
            let nodes = node_of(run.start)..=node_of(run.end - 1);
            touched.extend(nodes.map(|node| (index, node)));
        }
    }
    blocks.flush().map_err(|err| format!("committing its image: {err:#}"))?;
    Ok(touched)
}

/// Writes `bytes`, whole pages of `ps` bytes, at `offset` in `blocks`: runs of all-zero pages
/// discarded, holes, the rest written.
fn write_pages(blocks: &BlockDevice, offset: u64, bytes: &[u8], ps: u64) -> Result<(), String> {
    let ps = ps as usize;
    let zero = |page: usize| bytes[page * ps..(page + 1) * ps].iter().all(|byte| *byte == 0);
    let pages = bytes.len() / ps;
    let mut page = 0;
    while page < pages {
        let zeros = zero(page);
        let mut end = page + 1;
        while end < pages && zero(end) == zeros {
            end += 1;
        }
        let (at, run) = (offset + (page * ps) as u64, &bytes[page * ps..end * ps]);
        let written =
            if zeros { blocks.discard(at, run.len() as u64) } else { blocks.write(at, run) };
        written.map_err(|err| format!("writing its image at {at:#x}: {err:#}"))?;
        page = end;
    }
    Ok(())
}
