// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The FUSE server: a request's bytes in, its reply's out, whatever carries them.
//!
//! A server serves one filesystem, whose root is the mount's ([`Server::new`]); or many cells'
//! (TASK-302), each attached at a path in directories of the server's own, and detached again
//! ([`Server::cells`], [`Server::attach`], [`Server::detach`]): one mount, however many cells.
//! cladefs: a cell's path is names, its last its own and those before it directories of the
//! server's, made as a cell is attached under them and gone once none is: a host's mount of
//! cells attaches each at `TENANT/CELL` (TASK-364).
//!
//! Node IDs and inode numbers say whose node is whose:
//!
//! - One filesystem's node IDs are its object IDs, but the root's, which is FUSE's root ID (1), and
//!   that ID's, which is the root's: the two swap places. Inode numbers are object IDs.
//! - Many cells' are a cell's slot, from 1, in the top 16 bits, and the object ID in the low 48, so
//!   that no two cells' nodes share one: `EOVERFLOW` for an object ID of more bits, which cladefs's
//!   Fxfs, numbering a volume's objects in turn, doesn't reach. Inode numbers are node IDs; the
//!   root directory of cells is FUSE's root ID, and the server's other directories are of slot 0,
//!   each its own, never one another had before. A slot is used again once the kernel has forgotten
//!   every node of the cell detached from it.
//!
//! The directories of cells are the server's: nothing is made, removed or renamed in them, but by
//! attaching and detaching (`EPERM`), and nothing moves between them and a cell, or between
//! cells (`EXDEV`). The kernel looks their names up again each time (they and their absence are
//! cached for no time).
//!
//! The kernel drops a dentry only when it is told to by name ([`Notifier`]), or when it is short
//! of memory: a cell detached waits for it to forget every node of the cell, so the server keeps
//! the names it gave the kernel each node under, and tells it to drop them all. A dentry in use, a
//! file open or a directory someone's current one, goes once it is let go of; those the kernel is
//! given while the cell is detached are given for no time.

use super::abi::*;
use crate::filesystem::{EntriesStart, Filesystem, Node, valid_name};
use crate::node::XattrMode;
use crate::node::{Attr, DirEntry, FileType, Inode, Owner, RenameFlags, S_ISGID, SetAttr, SetTime};
use cladefs_core::error::{Errno, Error, Result};
use event_listener::Event;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zerocopy::{FromBytes, IntoBytes};

/// The most a write carries, and a read asks for: the `max_write` INIT gives the kernel, which
/// sizes its reads by it too (`max_pages`).
pub const MAX_WRITE: u32 = 1 << 20;

/// How long the kernel may cache names, their absence, and attributes: as long as it likes, as
/// the mount is each filesystem's only writer, whose cell its process alone has open.
const TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Where a node ID of many cells' says its cell's slot: the bits above the object ID's.
const SLOT_SHIFT: u32 = 48;

/// The size of the blocks the root directory of cells says it has, which holds none.
const CELLS_BLOCK_SIZE: u32 = 4096;

/// What sends the kernel a notification, as the server has written it: a write to `/dev/fuse`.
/// Fails as the write does: `ENOENT` if the kernel has nothing the notification is of.
pub type Notifier = Box<dyn Fn(&[u8]) -> std::io::Result<()> + Send + Sync>;

/// A FUSE server of a [`Filesystem`], or of many cells' ([`Server::cells`]):
/// [`handle`](Self::handle) takes a request as the kernel sent it, and gives its reply, for
/// whatever carries them, as `Mount` does `/dev/fuse`'s. See the module docs.
pub struct Server {
    shape: Shape,
    /// Whether `INIT` advertises `FUSE_WRITEBACK_CACHE` (default `true`).
    writeback_cache: bool,
    /// `INIT` flags negotiated with the kernel (bitmask of `InitIn::flags | (flags2 << 32)`).
    negotiated: AtomicU64,
    /// Tells the kernel of nodes changed ([`Server::changed`]) and names detached.
    notify: Arc<Notifier>,
    /// The filesystems served, by slot.
    cells: Arc<Mutex<Cells>>,
    /// The nodes the kernel holds.
    nodes: Arc<Mutex<Nodes>>,
    /// The directories open, by handle.
    dirs: Mutex<OpenDirs>,
    /// Notified as the kernel forgets a node: for detaches waiting for it to forget a cell's.
    forgotten: Event,
    /// cladefs: what it did, for the extent daemon's metrics of its mount (board 8565).
    counters: Counters,
    /// cladefs: registration in [`SERVERS`] so [`forget_cells`] can halt and invalidate mounted
    /// cells on `ShredCell` / `ForgetTenantKeys` (TASK-562, board 2520, board 2704).
    _registered: Arc<ServerHandle>,
}

/// Weakly registered handle of a live [`Server`] for [`forget_cells`].
struct ServerHandle {
    many: bool,
    notify: Arc<Notifier>,
    cells: Arc<Mutex<Cells>>,
    nodes: Arc<Mutex<Nodes>>,
}

/// cladefs: live FUSE servers in this process (outside the simulation: FUSE mounts do not run under
/// madsim), held weakly so `bucket::forget` halts any mounted cell in scope and invalidates its
/// kernel page cache and dentries (TASK-562, board 2520, board 2704).
static SERVERS: Mutex<Vec<Weak<ServerHandle>>> = Mutex::new(Vec::new());

/// cladefs: halts every FUSE-mounted cell whose [`crate::CellId`] matches `scope`, and asks the
/// kernel to drop all cached file pages (`off=0, len=0`), attributes, and dentries of those cells
/// (TASK-562, board 2520, board 2704).
pub fn forget_cells(scope: &dyn Fn(cladefs_core::CellId) -> bool) {
    let servers: Vec<Arc<ServerHandle>> = {
        let mut live = SERVERS.lock().unwrap();
        live.retain(|handle| handle.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).collect()
    };
    for server in servers {
        let (inodes, names) = {
            let cells = server.cells.lock().unwrap();
            let matching: Vec<(u16, Option<Name>)> = cells
                .slots
                .iter()
                .filter(|(_, slot)| slot.cell.is_some_and(scope))
                .map(|(&slot, entry)| {
                    let root_name = (server.many && !entry.leaf.is_empty())
                        .then(|| (entry.dir, entry.leaf.as_bytes().into()));
                    (slot, root_name)
                })
                .collect();
            if matching.is_empty() {
                continue;
            }
            let mut nodes = server.nodes.lock().unwrap();
            let mut inodes = Vec::new();
            let mut names = Vec::new();
            for (slot, root_name) in matching {
                nodes.halted.insert(slot);
                if !server.many {
                    inodes.push(ROOT_ID);
                }
                if let Some(name) = root_name {
                    names.push(name);
                }
                for (&nodeid, looked) in &nodes.looked {
                    let of_slot = if server.many { (nodeid >> SLOT_SHIFT) as u16 } else { 0 };
                    if of_slot == slot {
                        inodes.push(nodeid);
                        names.extend(looked.names.iter().cloned());
                    }
                }
            }
            (inodes, names)
        };
        for ino in inodes {
            if let Err(err) = (server.notify)(&inval_inode_pages(ino))
                && err.raw_os_error() != Some(libc::ENOENT)
            {
                log::error!("fuse: invalidating forgotten inode {ino}: {err}");
            }
        }
        for (dir, name) in names {
            if let Err(err) = (server.notify)(&inval_entry(dir, &name))
                && err.raw_os_error() != Some(libc::ENOENT)
            {
                log::error!("fuse: invalidating forgotten entry in dir {dir}: {err}");
            }
        }
    }
}

/// cladefs: the [`crate::CellId`]s of every currently mounted, non-halted FUSE cell in this
/// process, used by `cladefsd::events` to reconcile missed `CellShredded` events when the event
/// tail was trimmed (board 11665).
pub fn live_cells() -> Vec<cladefs_core::CellId> {
    let servers: Vec<Arc<ServerHandle>> = {
        let mut live = SERVERS.lock().unwrap();
        live.retain(|handle| handle.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).collect()
    };
    let mut out = Vec::new();
    for server in servers {
        let cells = server.cells.lock().unwrap();
        let nodes = server.nodes.lock().unwrap();
        for (&slot, entry) in &cells.slots {
            if !nodes.halted.contains(&slot)
                && let Some(cell) = entry.cell
            {
                out.push(cell);
            }
        }
    }
    out
}

/// cladefs: what a server did since it was made ([`Server::counts`]), for the extent daemon's
/// metrics of its mount of cells (board 8565).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The kernel's requests it handled.
    pub requests: u64,
    /// Those it answered with an error but `ENOENT`, a name not there, and `ENOSYS`, an operation
    /// it has not, which the kernel asks of every filesystem: what a filesystem failing answers.
    pub errors: u64,
    /// The cells it attached.
    pub attaches: u64,
    /// The cells it detached.
    pub detaches: u64,
}

/// cladefs: [`Counts`], as a server counts them.
#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    errors: AtomicU64,
    attaches: AtomicU64,
    detaches: AtomicU64,
}

/// What the mount's root is.
enum Shape {
    /// The root directory of the one filesystem, in slot 0: its object ID and FUSE's root ID swap
    /// places as node IDs.
    One { root: Node },
    /// A directory of the server's own, of the cells attached in it, whose attributes it has
    /// ([`Server::cells_attr`]).
    Many { owner: Owner, made: SystemTime },
}

/// The filesystems served: one's in slot 0, or many cells', by slot, from 1, and by path.
#[derive(Default)]
struct Cells {
    slots: BTreeMap<u16, Slot>,
    names: BTreeMap<String, u16>,
    /// Whether the server serves no more: the kernel destroyed the mount, or it was closed. No
    /// cell is attached then.
    closed: bool,
    /// Many cells': the directories of the server's own, by node ID, the root's among them.
    dirs: BTreeMap<u64, Dir>,
    /// The node ID the next directory made takes: never one a directory had before, which the
    /// kernel may hold yet.
    next_dir: u64,
}

struct Slot {
    /// Its path, as it was attached at.
    name: String,
    /// Its tenant: its path's first name, whose requests the mount's workers bound together
    /// ([`Server::tenant_of`]).
    #[cfg_attr(madsim, allow(dead_code))]
    tenant: Arc<str>,
    /// The directory of the server's its root is in, and its name there.
    dir: u64,
    leaf: String,
    /// cladefs: the cell backed by this slot's filesystem, if any (TASK-562, board 2520).
    cell: Option<cladefs_core::CellId>,
    state: State,
}

/// cladefs: a directory of the server's own, of many cells': the root of cells, or one a path a
/// cell is attached at goes through, as `TENANT/CELL` goes through `TENANT` and
/// `TENANT/CELL` (TASK-364).
struct Dir {
    /// Its parent's node ID: the root's is its own.
    parent: u64,
    /// Its name in its parent.
    name: String,
    entries: BTreeMap<String, Entry>,
}

/// What a name in a directory of the server's names.
#[derive(Clone, Copy)]
enum Entry {
    Dir(u64),
    Cell(u16),
}

impl Cells {
    /// Many cells', of none yet: the root directory alone.
    fn many() -> Self {
        let root = Dir { parent: ROOT_ID, name: String::new(), entries: BTreeMap::new() };
        Self { dirs: BTreeMap::from([(ROOT_ID, root)]), next_dir: ROOT_ID + 1, ..Self::default() }
    }

    /// Puts `slot` at `path`, the directories it goes through made where missing: returns the
    /// directory it is in, and its name there. `EEXIST` if a cell has a name the path goes
    /// through, or a directory its last.
    fn put(&mut self, path: &[&str], slot: u16) -> Result<(u64, String)> {
        let (leaf, through) = path.split_last().ok_or(Errno::EINVAL)?;
        let mut dir = ROOT_ID;
        for name in through {
            dir = match self.dirs[&dir].entries.get(*name) {
                Some(Entry::Dir(sub)) => *sub,
                Some(Entry::Cell(_)) => return Err(Errno::EEXIST),
                None => {
                    let made = self.next_dir;
                    self.next_dir += 1;
                    let sub = Dir { parent: dir, name: (*name).into(), entries: BTreeMap::new() };
                    self.dirs.insert(made, sub);
                    let parent = self.dirs.get_mut(&dir).expect("a directory made before");
                    parent.entries.insert((*name).into(), Entry::Dir(made));
                    made
                }
            };
        }
        let entries = &mut self.dirs.get_mut(&dir).expect("a directory").entries;
        if entries.contains_key(*leaf) {
            return Err(Errno::EEXIST);
        }
        entries.insert((*leaf).into(), Entry::Cell(slot));
        Ok((dir, (*leaf).into()))
    }

    /// Whether the directory `dir` of the server's shows: whether a cell attached is under it, not
    /// one being attached or detached alone, which are gone from the mount already.
    fn shows(&self, dir: u64) -> bool {
        self.dirs.get(&dir).is_some_and(|of| {
            of.entries.values().any(|entry| match *entry {
                Entry::Dir(sub) => self.shows(sub),
                Entry::Cell(slot) => matches!(self.slots[&slot].state, State::Attached(_)),
            })
        })
    }

    /// Takes the cell in `slot` away from its path, and its name: the directories of the
    /// server's that no cell is under any more go.
    fn remove(&mut self, slot: u16) {
        let Some(gone) = self.slots.remove(&slot) else { return };
        self.names.remove(&gone.name);
        let (mut dir, mut name) = (gone.dir, gone.leaf);
        while let Some(of) = self.dirs.get_mut(&dir) {
            of.entries.remove(&name);
            if dir == ROOT_ID || !of.entries.is_empty() {
                break;
            }
            let empty = self.dirs.remove(&dir).expect("a directory");
            (dir, name) = (empty.parent, empty.name);
        }
    }
}

enum State {
    /// Its cell is being opened: its name is taken, not yet looked up.
    Opening,
    /// Its filesystem is served.
    Attached(Arc<Filesystem>),
    /// Its filesystem is being detached: its name is no longer looked up, and nodes under it the
    /// kernel still holds are served until it is unmounted.
    Detaching(Arc<Filesystem>),
}

impl State {
    fn filesystem(&self) -> Option<&Arc<Filesystem>> {
        match self {
            Self::Opening => None,
            Self::Attached(fs) | Self::Detaching(fs) => Some(fs),
        }
    }
}

/// A cell attached, as [`Server::attached`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attached {
    /// Its name in the root directory of cells.
    pub name: String,
    /// Whether it is being detached.
    pub detaching: bool,
}

/// The nodes the kernel holds, and, of many cells', the names it has them under.
#[derive(Default)]
struct Nodes {
    /// The nodes the kernel has looked up, by node ID, and how many lookups it holds of each,
    /// which it forgets as it evicts their inodes: a node it holds stays, as its handle holds it,
    /// unlinked or not, until it is forgotten. The mount's root is not among them: it stays while
    /// the server does.
    looked: HashMap<u64, Looked>,
    /// Many cells': the names the kernel was given nodes under, each its directory's node ID and
    /// the name in it, and the node it names, as they are renamed and removed, for a detach to have
    /// the kernel drop them ([`Server::detach`]).
    names: HashMap<Name, u64>,
    /// Many cells': the slots of those being detached, whose nodes are given for no time.
    detaching: HashSet<u16>,
    /// cladefs: slots whose cell was forgotten/shredded (`forget_cells`): every FUSE request on
    /// them fails with `EACCES` (TASK-562, board 2520, board 2704).
    halted: HashSet<u16>,
}

/// A name in a directory, whose node ID is first.
type Name = (u64, Box<[u8]>);

/// A node the kernel has looked up, and how many lookups of it it holds.
struct Looked {
    node: Node,
    lookups: u64,
    /// Many cells': the names in [`Nodes::names`] that are this node's.
    names: Vec<Name>,
}

impl Nodes {
    /// The kernel was given the node `nodeid` at `name`, which no longer names another.
    fn named(&mut self, nodeid: u64, name: Name) {
        match self.names.insert(name.clone(), nodeid) {
            Some(named) if named == nodeid => return,
            Some(other) => self.forget_name(other, &name),
            None => {}
        }
        if let Some(looked) = self.looked.get_mut(&nodeid) {
            looked.names.push(name);
        }
    }

    /// `name` is gone: it names no node.
    fn unnamed(&mut self, name: &Name) {
        if let Some(nodeid) = self.names.remove(name) {
            self.forget_name(nodeid, name);
        }
    }

    /// `from` is renamed `to`, which no longer names another; or, if `exchange`, the two swap.
    fn renamed(&mut self, from: Name, to: Name, exchange: bool) {
        let (moved, replaced) = (self.names.remove(&from), self.names.remove(&to));
        for (nodeid, was) in [(moved, &from), (replaced, &to)] {
            if let Some(nodeid) = nodeid {
                self.forget_name(nodeid, was);
            }
        }
        if let Some(nodeid) = moved {
            self.named(nodeid, to.clone());
        }
        if let Some(nodeid) = replaced.filter(|_| exchange) {
            self.named(nodeid, from);
        }
    }

    /// The node `nodeid` is no longer at `name`.
    fn forget_name(&mut self, nodeid: u64, name: &Name) {
        if let Some(looked) = self.looked.get_mut(&nodeid) {
            looked.names.retain(|had| had != name);
        }
    }
}

/// What a node ID names: a directory of the server's own, of many cells', or a node of the
/// filesystem in a slot.
#[derive(Clone)]
enum Target {
    Cells(u64),
    Node(u16, Node),
}

/// What a lookup found: a node of the filesystem in a slot, or a directory of the server's own.
enum Found {
    Node(u16, (Node, Attr)),
    Cells(u64),
}

#[derive(Default)]
struct OpenDirs {
    /// The handle the next directory opened takes.
    next: u64,
    open: HashMap<u64, Arc<OpenDir>>,
}

/// Bounded read-ahead entries fetched beyond one FUSE `READDIR` reply's worst-case capacity
/// (TASK-619, board 2634).
const READDIR_READAHEAD: usize = 32;
/// Hard upper bound on entries fetched in a single Fxfs `readdir` page (TASK-619, board 2634).
const READDIR_MAX_BATCH: usize = 1024;
/// Bounded count of recent `(emitted_offset, name_before_offset)` bookmarks kept per open
/// directory handle so mid-reply resumes (e.g., musl's 2048-byte `getdents` buffer consuming part
/// of a 4096-byte FUSE `READDIR` reply) and retries resume by name (`EntriesStart::After`) rather
/// than index-seeking the live directory (hotbed review 109).
const READDIR_BOOKMARKS_CAP: usize = READDIR_MAX_BATCH;
/// Minimum wire size of one FUSE `Dirent` (24-byte header + 1-byte name, 8-byte aligned).
const MIN_DIRENT_LEN: usize = (size_of::<Dirent>() + 1).next_multiple_of(8);

/// Streaming cursor over a real Fxfs directory: holds at most one bounded read-ahead batch rather
/// than the whole directory, resuming by name (`EntriesStart::After`) across mutations so each
/// unchanged entry is listed once (TASK-619, board 2634).
struct DirCursor {
    /// Next sequential FUSE offset (starts at 2, after `.` = 0 and `..` = 1).
    next_offset: u64,
    /// Last entry name emitted (or skipped on `ENOENT`) at `next_offset`.
    last_name: Option<Vec<u8>>,
    /// Bounded read-ahead buffer of entries starting at `next_offset`.
    buffered: VecDeque<DirEntry>,
    /// True once Fxfs reached end-of-directory when filling `buffered`.
    eof: bool,
    /// Bounded recent `(offset, last_name_at_offset)` bookmarks (monotone in `offset`, capped at
    /// [`READDIR_BOOKMARKS_CAP`]) covering the start and every emitted dirent offset of recent
    /// replies so mid-reply resumes and retries resume by name in O(log N) rather than falling
    /// back to `EntriesStart::Index` (hotbed review 109).
    recent: VecDeque<(u64, Option<Vec<u8>>)>,
}

impl DirCursor {
    fn new() -> Self {
        Self {
            next_offset: 2,
            last_name: None,
            buffered: VecDeque::new(),
            eof: false,
            recent: VecDeque::new(),
        }
    }

    fn record_bookmark(&mut self, offset: u64, last_name: Option<Vec<u8>>) {
        if let Some((back_off, back_name)) = self.recent.back_mut()
            && *back_off == offset
        {
            *back_name = last_name;
            return;
        }
        if self.recent.len() >= READDIR_BOOKMARKS_CAP {
            self.recent.pop_front();
        }
        self.recent.push_back((offset, last_name));
    }
}

enum DirListing {
    /// Virtual directory of the server's own (tenant/cell directories): small and in-memory.
    Cells(Vec<DirEntry>),
    /// Real Fxfs directory streamed in bounded pages (TASK-619, board 2634).
    Node(futures::lock::Mutex<DirCursor>),
}

/// An open directory: virtual cells directories keep their small listing; Fxfs directories stream
/// lazily in bounded pages so memory per handle does not grow with directory size (TASK-619).
/// Inode numbers are the kernel's.
struct OpenDir {
    dir: Target,
    /// The directory's node ID.
    nodeid: u64,
    /// `.`'s inode.
    inode: u64,
    /// `..`'s inode.
    parent: u64,
    listing: DirListing,
}

impl Server {
    /// A server of `fs`, whose root is the mount's. `notify` sends the kernel the notifications
    /// the server writes, as a change does ([`Server::changed`]).
    pub fn new(fs: Filesystem, notify: Notifier) -> Self {
        let root = fs.root().clone();
        let cell = fs.cell();
        let state = State::Attached(Arc::new(fs));
        let (name, tenant, leaf) = (String::new(), Arc::from(""), String::new());
        let slot = Slot { name, tenant, dir: ROOT_ID, leaf, cell, state };
        let cells = Cells { slots: BTreeMap::from([(0, slot)]), ..Cells::default() };
        Self::of(Shape::One { root }, cells, notify)
    }

    /// A server of many cells, attached by name in its root directory, none yet: `owner`'s,
    /// readable and searchable by all (0755). `notify` sends the kernel the notifications the
    /// server writes, as a detach and a change do.
    pub fn cells(owner: Owner, notify: Notifier) -> Self {
        let shape = Shape::Many { owner, made: SystemTime::now() };
        Self::of(shape, Cells::many(), notify)
    }

    fn of(shape: Shape, cells: Cells, notify: Notifier) -> Self {
        let many = matches!(shape, Shape::Many { .. });
        let notify = Arc::new(notify);
        let cells = Arc::new(Mutex::new(cells));
        let nodes = Arc::new(Mutex::default());
        let registered = Arc::new(ServerHandle {
            many,
            notify: Arc::clone(&notify),
            cells: Arc::clone(&cells),
            nodes: Arc::clone(&nodes),
        });
        {
            static REGISTER_FORGET: std::sync::Once = std::sync::Once::new();
            REGISTER_FORGET.call_once(|| cladefs_core::bucket::register_forget_cells(forget_cells));
            let mut live = SERVERS.lock().unwrap();
            live.retain(|handle| handle.strong_count() > 0);
            live.push(Arc::downgrade(&registered));
        }
        Self {
            shape,
            writeback_cache: true,
            negotiated: AtomicU64::new(0),
            notify,
            cells,
            nodes,
            dirs: Mutex::default(),
            forgotten: Event::new(),
            counters: Counters::default(),
            _registered: registered,
        }
    }

    #[cfg_attr(madsim, allow(dead_code))]
    pub fn with_writeback_cache(mut self, enabled: bool) -> Self {
        self.writeback_cache = enabled;
        self
    }

    #[cfg_attr(madsim, allow(dead_code))]
    pub fn is_many(&self) -> bool {
        matches!(self.shape, Shape::Many { .. })
    }

    fn handle_killpriv_v2(&self) -> bool {
        self.negotiated.load(Ordering::Relaxed) & HANDLE_KILLPRIV_V2 != 0
    }

    /// cladefs: what it did since it was made (board 8565).
    pub fn counts(&self) -> Counts {
        let Counters { requests, errors, attaches, detaches } = &self.counters;
        let load = |count: &AtomicU64| count.load(Ordering::Relaxed);
        Counts {
            requests: load(requests),
            errors: load(errors),
            attaches: load(attaches),
            detaches: load(detaches),
        }
    }

    /// The filesystem served by a server of one ([`Server::new`]), such as to close it once the
    /// kernel is done: the nodes the kernel held go, and the directories it had open.
    ///
    /// # Panics
    ///
    /// If the server is of many cells.
    pub fn into_filesystem(self) -> Filesystem {
        assert!(matches!(self.shape, Shape::One { .. }), "a server of many cells");
        let slots = std::mem::take(&mut self.cells.lock().unwrap().slots);
        drop(self.nodes);
        drop(self.dirs);
        match slots.into_values().next().map(|slot| slot.state) {
            Some(State::Attached(fs)) => {
                Arc::into_inner(fs).expect("the filesystem is the server's alone once it goes")
            }
            _ => unreachable!("a server of one filesystem keeps it"),
        }
    }

    /// Attaches the filesystem `open` gives at `name` in a server of many cells, once it is
    /// open: a path of names separated by `/`, the last the cell's root's and those before it
    /// directories of the server's own, made as needed. `name` is taken from the start, and a
    /// cell attached as `open` is awaited fails. Fails with `EINVAL` for a name no directory
    /// holds, `EEXIST` for a path another cell has, or one going through another cell's,
    /// `EBUSY` for one another cell is being attached or detached at, `ENOSPC` with every slot
    /// taken, `ENOTCONN` once the server serves no more, `ENOTSUP` for a server of one filesystem;
    /// and as `open` does.
    pub async fn attach(
        &self,
        name: &str,
        open: impl Future<Output = Result<Filesystem, Error>>,
    ) -> Result<(), Error> {
        if !matches!(self.shape, Shape::Many { .. }) {
            return Err(Errno::ENOTSUP.into());
        }
        for component in name.split('/') {
            valid_name(component.as_bytes())?;
        }
        let reserved = self.reserve(name)?;
        let fs = Arc::new(open.await?);
        match reserved.attach(fs.clone()) {
            Ok(()) => {
                self.counters.attaches.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(errno) => {
                // The server closed meanwhile.
                fs.unmount().await?;
                Err(errno.into())
            }
        }
    }

    /// Takes `name`, and the lowest slot free whose nodes the kernel has forgotten, for a cell
    /// being opened, until what is returned attaches it or goes.
    fn reserve(&self, name: &str) -> Result<Reserved<'_>> {
        let mut cells = self.cells.lock().unwrap();
        if cells.closed {
            return Err(Errno::ENOTCONN);
        }
        if let Some(slot) = cells.names.get(name) {
            return Err(match cells.slots[slot].state {
                State::Attached(_) => Errno::EEXIST,
                State::Opening | State::Detaching(_) => Errno::EBUSY,
            });
        }
        let nodes = self.nodes.lock().unwrap();
        let held: HashSet<u16> = nodes.looked.keys().map(|&nodeid| self.slot_of(nodeid)).collect();
        drop(nodes);
        let slot = (1..=u16::MAX)
            .find(|slot| !cells.slots.contains_key(slot) && !held.contains(slot))
            .ok_or(Errno::ENOSPC)?;
        let path: Vec<&str> = name.split('/').collect();
        let (dir, leaf) = cells.put(&path, slot)?;
        let tenant = Arc::from(path[0]);
        let reserved =
            Slot { name: name.to_owned(), tenant, dir, leaf, cell: None, state: State::Opening };
        cells.slots.insert(slot, reserved);
        cells.names.insert(name.to_owned(), slot);
        Ok(Reserved { server: self, slot: Some(slot) })
    }

    /// Detaches the cell attached at `name` from a server of many cells: its name goes at once,
    /// and the kernel is told to drop every name it has a node of the cell under, which it does
    /// at once for those not in use, then forgetting their nodes, and for the others once they are
    /// let go of. Unless `force`, the cell is unmounted once the kernel has forgotten every node of
    /// it, as once whatever has a file open in it, or a directory its current one, lets it go; with
    /// `force`, at once, the nodes the kernel still holds of it failing with `ENOTCONN` from then
    /// on. Unmounting it writes back what is dirty, and closes its cell.
    ///
    /// Fails with `ENOENT` if no cell is attached at `name`, `EBUSY` if one is being attached or
    /// detached there, `ENOTSUP` for a server of one filesystem; and as unmounting does. A detach
    /// given up on before it completes leaves the cell detaching for good: callers that may give
    /// up on it run it in a task of its own.
    pub async fn detach(&self, name: &str, force: bool) -> Result<(), Error> {
        let Shape::Many { .. } = &self.shape else {
            return Err(Errno::ENOTSUP.into());
        };
        let (slot, fs) = {
            let mut cells = self.cells.lock().unwrap();
            let slot = *cells.names.get(name).ok_or(Errno::ENOENT)?;
            let detaching = cells.slots.get_mut(&slot).expect("a name's slot");
            let State::Attached(fs) = &detaching.state else {
                return Err(Errno::EBUSY.into());
            };
            let fs = fs.clone();
            detaching.state = State::Detaching(fs.clone());
            (slot, fs)
        };
        // Names given from here on are given for no time; those given before are dropped. Not
        // under the table's lock: the kernel drops a name in a directory once the requests in it
        // under way, which take the lock, are answered, the names they give with them.
        let given: Vec<Name> = {
            let mut nodes = self.nodes.lock().unwrap();
            nodes.detaching.insert(slot);
            let of_slot = nodes.looked.iter().filter(|(id, _)| self.slot_of(**id) == slot);
            of_slot.flat_map(|(_, looked)| looked.names.iter().cloned()).collect()
        };
        for (dir, given) in given {
            // `ENOENT`: the kernel dropped it already, as it dropped a directory it was in.
            if let Err(err) = (self.notify)(&inval_entry(dir, &given))
                && err.raw_os_error() != Some(libc::ENOENT)
            {
                log::error!("fuse: telling the kernel to drop a name of {name}'s: {err}");
            }
        }
        if !force {
            self.forgotten_all(slot).await;
        }
        let unmounted = fs.unmount().await;
        drop(fs);
        self.cells.lock().unwrap().remove(slot);
        let mut nodes = self.nodes.lock().unwrap();
        nodes.detaching.remove(&slot);
        nodes.halted.remove(&slot);
        drop(nodes);
        self.counters.detaches.fetch_add(1, Ordering::Relaxed);
        unmounted
    }

    /// Waits until the kernel holds no node of the cell in `slot`.
    async fn forgotten_all(&self, slot: u16) {
        loop {
            let forgotten = self.forgotten.listen();
            let held = {
                let nodes = self.nodes.lock().unwrap();
                nodes.looked.keys().any(|&nodeid| self.slot_of(nodeid) == slot)
            };
            if !held {
                return;
            }
            forgotten.await;
        }
    }

    /// The cells attached to a server of many, by name, and those being detached.
    pub fn attached(&self) -> Vec<Attached> {
        let cells = self.cells.lock().unwrap();
        (cells.slots.values())
            .filter_map(|slot| match slot.state {
                State::Opening => None,
                State::Attached(_) => Some(Attached { name: slot.name.clone(), detaching: false }),
                State::Detaching(_) => Some(Attached { name: slot.name.clone(), detaching: true }),
            })
            .collect()
    }

    /// Unmounts every filesystem served, writing back what is dirty, and attaches none from then
    /// on: as the kernel destroys the mount. Fails with the first unmount's error, once all are
    /// done.
    pub async fn unmount(&self) -> Result<(), Error> {
        let served: Vec<Arc<Filesystem>> = {
            let mut cells = self.cells.lock().unwrap();
            cells.closed = true;
            cells.slots.values().filter_map(|slot| slot.state.filesystem().cloned()).collect()
        };
        let mut unmounted = Ok(());
        for fs in served {
            let done = fs.unmount().await;
            unmounted = unmounted.and(done);
        }
        unmounted
    }

    /// The tenant whose cells `request`, as the kernel sent it, may wait on, in a server of many
    /// cells: what the mount's workers bound the requests in service of, so that no tenant's, nor
    /// any cell's, waiting on its bucket, can take every worker (board 8539). A node's request is
    /// its cell's tenant's; one of the server's own directories under a tenant's, whose lookups and
    /// listings read its cells' roots, that tenant's (hotbed review 57), as is a lookup of a name
    /// in the root, a tenant's or a cell's own. None for a server of one filesystem, the root's
    /// other requests, which read no cell, and those that never wait on a cell (`INIT`,
    /// `DESTROY`, `FORGET`, `BATCH_FORGET`, `INTERRUPT`).
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn tenant_of(&self, request: &[u8]) -> Option<Arc<str>> {
        let Shape::Many { .. } = self.shape else { return None };
        // Shorter than a header: no cell's, which its handling refuses.
        let (header, body) = InHeader::read_from_prefix(request).ok()?;
        if matches!(
            header.opcode,
            opcode::INIT
                | opcode::DESTROY
                | opcode::FORGET
                | opcode::BATCH_FORGET
                | opcode::INTERRUPT
        ) {
            return None;
        }
        // As a panic left the table, if one did, which the request's handling then answers: a
        // tenant's name is the slot's from its attaching on, a directory's from its making.
        let cells = self.cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = self.slot_of(header.nodeid);
        if slot != 0 {
            return cells.slots.get(&slot).map(|slot| slot.tenant.clone());
        }
        if header.nodeid == ROOT_ID {
            if header.opcode != opcode::LOOKUP {
                return None;
            }
            let name = body.split(|&byte| byte == 0).next()?;
            // Not UTF-8: no tenant's, nor any cell's, which its lookup refuses.
            return std::str::from_utf8(name).ok().map(Arc::from);
        }
        // The directory of the root's that this one is, or is under.
        let mut dir = cells.dirs.get(&header.nodeid)?;
        while dir.parent != ROOT_ID {
            dir = cells.dirs.get(&dir.parent)?;
        }
        Some(Arc::from(dir.name.as_str()))
    }

    /// Undoes the lookups that `reply` to `request` recorded when `/dev/fuse` did not accept the
    /// reply (for instance `ENOENT` on an interrupted request; board 11377). The kernel never took
    /// those lookups and will never send `FORGET` for them, so leaving them in [`Nodes::looked`]
    /// would wedge a non-forced [`Self::detach`] forever waiting for `forgotten_all`.
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn undelivered(&self, request: &[u8], reply: &[u8]) {
        let Ok((in_header, _)) = InHeader::read_from_prefix(request) else { return };
        let Ok((out_header, mut payload)) = OutHeader::read_from_prefix(reply) else { return };
        if out_header.error != 0 {
            return;
        }
        match in_header.opcode {
            opcode::LOOKUP
            | opcode::SYMLINK
            | opcode::MKNOD
            | opcode::MKDIR
            | opcode::LINK
            | opcode::CREATE => {
                let Ok((entry, _)) = EntryOut::read_from_prefix(payload) else { return };
                if entry.nodeid != 0 {
                    self.forget(entry.nodeid, 1);
                }
            }
            opcode::READDIRPLUS => {
                while !payload.is_empty() {
                    let Ok((entry, rest)) = EntryOut::read_from_prefix(payload) else { break };
                    let Ok((dirent, rest)) = Dirent::read_from_prefix(rest) else { break };
                    let Some(name_and_pad) = (dirent.namelen as usize)
                        .checked_add(size_of::<Dirent>())
                        .and_then(|n| n.checked_next_multiple_of(8))
                        .and_then(|n| n.checked_sub(size_of::<Dirent>()))
                    else {
                        break;
                    };
                    let Some((_, next)) = rest.split_at_checked(name_and_pad) else { break };
                    if entry.nodeid != 0 {
                        self.forget(entry.nodeid, 1);
                    }
                    payload = next;
                }
            }
            _ => {}
        }
    }

    /// The reply to `request`, as the kernel sent it, whose handling panicked: `EIO`, unless it
    /// takes none, so that the process waiting on it goes on (board 8539).
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn panicked(request: &[u8]) -> Option<Vec<u8>> {
        // Shorter than a header: nothing to reply to, as its handling has none.
        let (header, _) = InHeader::read_from_prefix(request).ok()?;
        match header.opcode {
            opcode::DESTROY | opcode::FORGET | opcode::BATCH_FORGET | opcode::INTERRUPT => None,
            _ => Some(Reply { unique: header.unique }.error(Errno::EIO)),
        }
    }

    /// The reply to `request` rejected by per-tenant admission once the tenant's waiting buffer is
    /// full: `EAGAIN` in a stack-allocated `OutHeader`, unless the opcode takes no reply
    /// (board 9850, review 131).
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn rejected(request: &[u8]) -> Option<[u8; size_of::<OutHeader>()]> {
        // Shorter than a header: nothing to reply to, as in `panicked`.
        let (header, _) = InHeader::read_from_prefix(request).ok()?;
        match header.opcode {
            opcode::DESTROY | opcode::FORGET | opcode::BATCH_FORGET | opcode::INTERRUPT => None,
            _ => {
                let out = OutHeader {
                    len: size_of::<OutHeader>() as u32,
                    error: -Errno::EAGAIN.raw(),
                    unique: header.unique,
                };
                let mut buf = [0u8; size_of::<OutHeader>()];
                buf.copy_from_slice(out.as_bytes());
                Some(buf)
            }
        }
    }

    /// Handles fire-and-forget opcodes (`FORGET`, `BATCH_FORGET`, `INTERRUPT`) synchronously inline
    /// on the reader thread with zero heap allocation and no async task spawn (board 12092).
    /// Returns `None` if `request` requires async handling or a reply.
    #[cfg_attr(madsim, allow(dead_code))]
    pub fn try_handle_inline(&self, request: &[u8]) -> Option<Result<(), Error>> {
        // Shorter than a header: fall through to the normal async decoder to handle/log it.
        let (header, rest) = InHeader::read_from_prefix(request).ok()?;
        if !matches!(header.opcode, opcode::FORGET | opcode::BATCH_FORGET | opcode::INTERRUPT) {
            return None;
        }
        // A test's: a handler run inline that panics, which the reader says and serves on past,
        // its daemon too (board 12443).
        fail::fail_point!("fuse::inline");
        let body_len = (header.len as usize).checked_sub(size_of::<InHeader>())?;
        let body = rest.get(..body_len)?;
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let mut body = Body(body);
        match header.opcode {
            opcode::FORGET => {
                if let Ok(ForgetIn { nlookup }) = body.read() {
                    self.forget(header.nodeid, nlookup);
                }
            }
            opcode::BATCH_FORGET => self.batch_forget(&mut body),
            opcode::INTERRUPT => {}
            _ => unreachable!(),
        }
        Some(Ok(()))
    }

    /// Handles the request whose bytes are `request`, as the kernel sent it, and returns its
    /// reply, unless it has none (`FORGET`, `BATCH_FORGET`, `INTERRUPT` and `DESTROY`).
    ///
    /// Requests may be handled concurrently, any number at once, as the kernel sends them. Fails if
    /// `request` is too short for a request's header, as then there is nothing to reply to, or if
    /// `DESTROY`'s unmount fails.
    pub async fn handle(&self, request: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let (header, rest) = InHeader::read_from_prefix(request)
            .map_err(|_| Error::msg("the request is shorter than its header"))?;
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let reply = Reply { unique: header.unique };
        let body_len = (header.len as usize).checked_sub(size_of::<InHeader>());
        let Some(body) = body_len.and_then(|len| rest.get(..len)) else {
            return Ok(Some(reply.error(Errno::EINVAL)));
        };
        let mut body = Body(body);
        let payload = match header.opcode {
            opcode::INIT => self.init(&mut body).map(Payload::Bytes),
            opcode::DESTROY => {
                self.unmount().await?;
                return Ok(None);
            }
            opcode::FORGET => {
                if let Ok(ForgetIn { nlookup }) = body.read() {
                    self.forget(header.nodeid, nlookup);
                }
                return Ok(None);
            }
            opcode::BATCH_FORGET => {
                self.batch_forget(&mut body);
                return Ok(None);
            }
            // Requests are not interruptible: each completes, as Fxfs completes it.
            opcode::INTERRUPT => return Ok(None),
            opcode::LOOKUP => self.lookup(&header, &mut body).await.map(Payload::Bytes),
            opcode::GETATTR => self.getattr(&header).await.map(Payload::Bytes),
            opcode::SETATTR => self.setattr(&header, &mut body).await.map(Payload::Bytes),
            opcode::READLINK => self.readlink(&header).await.map(Payload::Bytes),
            opcode::SYMLINK => self.symlink(&header, &mut body).await.map(Payload::Bytes),
            opcode::MKNOD => self.mknod(&header, &mut body).await.map(Payload::Bytes),
            opcode::MKDIR => self.mkdir(&header, &mut body).await.map(Payload::Bytes),
            opcode::UNLINK => self.remove(&header, &mut body, false).await.map(Payload::Bytes),
            opcode::RMDIR => self.remove(&header, &mut body, true).await.map(Payload::Bytes),
            opcode::RENAME => self.rename(&header, &mut body, false).await.map(Payload::Bytes),
            opcode::RENAME2 => self.rename(&header, &mut body, true).await.map(Payload::Bytes),
            opcode::LINK => self.link(&header, &mut body).await.map(Payload::Bytes),
            opcode::CREATE => self.create(&header, &mut body).await.map(Payload::Bytes),
            opcode::READ => self.read(&header, &mut body).await.map(Payload::Prefixed),
            opcode::WRITE => self.write(&header, &mut body).await.map(Payload::Bytes),
            opcode::STATFS => self.statfs(&header).await.map(Payload::Bytes),
            // Files have no handles: there is nothing to open, flush or release. Refused, OPEN and
            // FLUSH are not sent again, and nor is RELEASE once OPEN is not.
            opcode::OPEN | opcode::FLUSH => Err(Errno::ENOSYS),
            opcode::RELEASE => Ok(Payload::Bytes(Vec::new())),
            opcode::FSYNC | opcode::FSYNCDIR => self.fsync(&header).await.map(Payload::Bytes),
            opcode::SETXATTR => self.setxattr(&header, &mut body).await.map(Payload::Bytes),
            opcode::GETXATTR => self.getxattr(&header, &mut body).await.map(Payload::Bytes),
            opcode::LISTXATTR => self.listxattr(&header, &mut body).await.map(Payload::Bytes),
            opcode::REMOVEXATTR => self.removexattr(&header, &mut body).await.map(Payload::Bytes),
            opcode::OPENDIR => self.opendir(&header).await.map(Payload::Bytes),
            opcode::READDIR => self.readdir(&mut body, false).await.map(Payload::Bytes),
            opcode::READDIRPLUS => self.readdir(&mut body, true).await.map(Payload::Bytes),
            opcode::RELEASEDIR => self.releasedir(&mut body).map(Payload::Bytes),
            opcode::FALLOCATE => self.fallocate(&header, &mut body).await.map(Payload::Bytes),
            _ => Err(Errno::ENOSYS),
        };
        Ok(Some(match payload {
            Ok(payload) => reply.ok(payload),
            Err(errno) => {
                if errno != Errno::ENOENT && errno != Errno::ENOSYS {
                    self.counters.errors.fetch_add(1, Ordering::Relaxed);
                }
                reply.error(errno)
            }
        }))
    }

    /// The object ID of the root of a server of one filesystem, which swaps places with FUSE's
    /// root ID as node IDs.
    #[cfg(test)]
    fn root(&self) -> u64 {
        match &self.shape {
            Shape::One { root } => root.inode().0,
            Shape::Many { .. } => panic!("a server of many cells"),
        }
    }

    /// The node ID that is the inode number `inode` (and the inode number that is the node ID
    /// `inode`): its own, but for a server of one filesystem, whose root's and [`ROOT_ID`] swap
    /// places.
    fn swap(&self, inode: u64) -> u64 {
        let Shape::One { root } = &self.shape else { return inode };
        let root = root.inode().0;
        match inode {
            ROOT_ID => root,
            id if id == root => ROOT_ID,
            id => id,
        }
    }

    /// The slot of the cell whose node the node ID `nodeid` is.
    fn slot_of(&self, nodeid: u64) -> u16 {
        match self.shape {
            Shape::One { .. } => 0,
            Shape::Many { .. } => (nodeid >> SLOT_SHIFT) as u16,
        }
    }

    /// The inode number the kernel is given of the object `object` of the cell in `slot`.
    fn inode(&self, slot: u16, object: u64) -> Result<u64> {
        match self.shape {
            Shape::One { .. } => Ok(object),
            Shape::Many { .. } if object >> SLOT_SHIFT != 0 => Err(Errno::EOVERFLOW),
            Shape::Many { .. } => Ok((slot as u64) << SLOT_SHIFT | object),
        }
    }

    /// What the node ID `nodeid` names: the mount's root, a directory of the server's own, or a
    /// node the kernel holds a lookup of. Any other is one the kernel forgot, or never had, or a
    /// directory of the server's gone: `ESTALE`.
    fn target(&self, nodeid: u64) -> Result<Target> {
        match &self.shape {
            Shape::One { root } if nodeid == ROOT_ID => {
                if self.nodes.lock().unwrap().halted.contains(&0) {
                    return Err(Errno::EACCES);
                }
                return Ok(Target::Node(0, root.clone()));
            }
            Shape::Many { .. } if self.slot_of(nodeid) == 0 => {
                let cells = self.cells.lock().unwrap();
                return match cells.dirs.contains_key(&nodeid) {
                    true => Ok(Target::Cells(nodeid)),
                    false => Err(Errno::ESTALE),
                };
            }
            _ => {}
        }
        let nodes = self.nodes.lock().unwrap();
        let slot = self.slot_of(nodeid);
        if nodes.halted.contains(&slot) {
            return Err(Errno::EACCES);
        }
        let looked = nodes.looked.get(&nodeid);
        let node = looked.map(|looked| looked.node.clone()).ok_or(Errno::ESTALE)?;
        Ok(Target::Node(slot, node))
    }

    /// The node whose node ID is `nodeid`, as [`Self::target`] finds it, and its cell's slot:
    /// `cells` if it is the root directory of cells, which the request can't be of.
    fn node(&self, nodeid: u64, cells: Errno) -> Result<(u16, Node)> {
        match self.target(nodeid)? {
            Target::Cells(_) => Err(cells),
            Target::Node(slot, node) => Ok((slot, node)),
        }
    }

    /// The filesystem in `slot`: `ENOTCONN` once it is detached.
    fn filesystem(&self, slot: u16) -> Result<Arc<Filesystem>> {
        let cells = self.cells.lock().unwrap();
        let slot = cells.slots.get(&slot).ok_or(Errno::ENOTCONN)?;
        slot.state.filesystem().cloned().ok_or(Errno::ENOTCONN)
    }

    /// The attributes of the directory of the server's own whose node ID is `dir`.
    fn cells_attr(&self, dir: u64) -> Attr {
        let Shape::Many { owner, made, .. } = &self.shape else {
            unreachable!("only a server of many cells has directories of its own")
        };
        Attr {
            inode: Inode(dir),
            size: 0,
            blocks: 0,
            atime: *made,
            mtime: *made,
            ctime: *made,
            file_type: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: owner.uid,
            gid: owner.gid,
            rdev: 0,
            blksize: CELLS_BLOCK_SIZE,
        }
    }

    /// Looks `name` up in the directory `dir`: a node of its cell; or, in a directory of the
    /// server's own, another of those, or the root directory of a cell attached there.
    async fn lookup_in(&self, dir: &Target, name: &[u8]) -> Result<Found> {
        match dir {
            Target::Node(slot, dir) => Ok(Found::Node(*slot, dir.lookup(name).await?)),
            Target::Cells(dir) => {
                let name = std::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
                let (slot, root) = {
                    let cells = self.cells.lock().unwrap();
                    let entries = &cells.dirs.get(dir).ok_or(Errno::ENOENT)?.entries;
                    let slot = match entries.get(name).ok_or(Errno::ENOENT)? {
                        Entry::Dir(sub) if cells.shows(*sub) => return Ok(Found::Cells(*sub)),
                        Entry::Dir(_) => return Err(Errno::ENOENT),
                        Entry::Cell(slot) => *slot,
                    };
                    let State::Attached(fs) = &cells.slots[&slot].state else {
                        return Err(Errno::ENOENT);
                    };
                    if self.nodes.lock().unwrap().halted.contains(&slot) {
                        return Err(Errno::EACCES);
                    }
                    (slot, fs.root().clone())
                };
                let attr = root.getattr().await?;
                Ok(Found::Node(slot, (root, attr)))
            }
        }
    }

    /// The entry of a node of the cell in `slot` found or made at `name` in the directory
    /// `dir`, which the kernel holds a lookup of from the reply on.
    fn entry(&self, slot: u16, (dir, name): (u64, &[u8]), found: (Node, Attr)) -> Result<EntryOut> {
        let (node, attr) = found;
        let inode = self.inode(slot, attr.inode.0)?;
        let nodeid = self.swap(inode);
        let mut valid = TIMEOUT_SECS;
        if nodeid != ROOT_ID {
            let mut nodes = self.nodes.lock().unwrap();
            let looked = nodes.looked.entry(nodeid);
            looked.or_insert(Looked { node, lookups: 0, names: Vec::new() }).lookups += 1;
            if let Shape::Many { .. } = self.shape {
                match nodes.detaching.contains(&slot) {
                    true => valid = 0,
                    false => nodes.named(nodeid, (dir, name.into())),
                }
            }
        }
        Ok(entry_out(nodeid, inode, valid, &attr))
    }

    /// Tells the kernel that the nodes `nodeids` changed, once a request has changed them and
    /// before its reply (TASK-495). The kernel marks the nodes a request changes stale as its reply
    /// comes, but some of them only that, their `attr_version` left as it was: a directory whose
    /// entries changed (`fuse_dir_changed`), a node renamed or whose extended attributes changed
    /// (`fuse_update_ctime`), and one whose attributes were set. A stat's reply the server sent
    /// before the change, coming after it, then reads as fresh, its old times kept as long as the
    /// mount says, a day. Told first, the kernel moves each one's `attr_version` on, and takes no
    /// reply asked before as fresh. A node the kernel holds no more (`ENOENT`) has none to keep.
    fn changed(&self, nodeids: impl IntoIterator<Item = u64>) {
        for nodeid in nodeids {
            if let Err(err) = (self.notify)(&inval_inode(nodeid))
                && err.raw_os_error() != Some(libc::ENOENT)
            {
                log::error!("fuse: telling the kernel node {nodeid} changed: {err}");
            }
        }
    }

    /// The kernel forgets `nlookup` of its lookups of the node `nodeid`: the last lets it go.
    fn forget(&self, nodeid: u64, nlookup: u64) {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(looked) = nodes.looked.get_mut(&nodeid) else { return };
        looked.lookups = looked.lookups.saturating_sub(nlookup);
        if looked.lookups == 0 {
            let gone = nodes.looked.remove(&nodeid);
            for name in gone.iter().flat_map(|gone| &gone.names) {
                nodes.names.remove(name);
            }
            drop(nodes);
            // Letting a node go may free it: not under the table's lock.
            drop(gone);
            self.forgotten.notify(usize::MAX);
        }
    }

    fn batch_forget(&self, body: &mut Body<'_>) {
        let Ok(BatchForgetIn { count, .. }) = body.read() else { return };
        for _ in 0..count {
            let Ok(ForgetOne { nodeid, nlookup }) = body.read() else { return };
            self.forget(nodeid, nlookup);
        }
    }

    fn init(&self, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let init: InitIn = body.read()?;
        let flags2 = match init.flags as u64 & INIT_EXT {
            0 => 0,
            _ => body.read::<InitInExt>()?.flags2,
        };
        if init.major < KERNEL_VERSION {
            log::error!("fuse: unsupported protocol version {}.{}", init.major, init.minor);
            return Err(Errno::EPROTO);
        }
        if init.major > KERNEL_VERSION {
            // The kernel asks again with a version of the server's.
            let out = InitOut {
                major: KERNEL_VERSION,
                minor: KERNEL_MINOR_VERSION,
                ..Default::default()
            };
            return Ok(out.as_bytes().to_vec());
        }
        if init.minor < OLDEST_MINOR_VERSION {
            log::error!("fuse: unsupported protocol version {}.{}", init.major, init.minor);
            return Err(Errno::EPROTO);
        }
        // The mount is each filesystem's only writer, so the kernel may cache all it reads (not
        // AUTO_INVAL_DATA, which drops a file's cached pages when its modification time changes)
        // and coalesce buffered writes in the page cache (WRITEBACK_CACHE). Opens are not sent
        // (`OPEN` is refused), so not ATOMIC_O_TRUNC: the kernel truncates with `SETATTR`.
        // HANDLE_KILLPRIV_V2 sets `SB_NOSEC` on the superblock so the kernel stops sending
        // `GETXATTR("security.capability")` before every `write(2)` syscall, and instead asks the
        // server to clear suid/sgid and `security.capability` via `WRITE_KILL_SUIDGID` /
        // `FATTR_KILL_SUIDGID`.
        let supported = ASYNC_READ
            | PARALLEL_DIROPS
            | BIG_WRITES
            | ASYNC_DIO
            | if self.writeback_cache { WRITEBACK_CACHE } else { 0 }
            | NO_OPEN_SUPPORT
            | DO_READDIRPLUS
            | READDIRPLUS_AUTO
            | MAX_PAGES
            | CACHE_SYMLINKS
            | EXPLICIT_INVAL_DATA
            | HANDLE_KILLPRIV_V2
            | INIT_EXT
            | NO_EXPORT_SUPPORT;
        let enabled = (((flags2 as u64) << 32) | init.flags as u64) & supported;
        self.negotiated.store(enabled, Ordering::Relaxed);
        let max_pages = (MAX_WRITE as u64).div_ceil(cladefs_core::pages::page_size());
        let out = InitOut {
            major: KERNEL_VERSION,
            minor: KERNEL_MINOR_VERSION,
            max_readahead: init.max_readahead,
            flags: enabled as u32,
            max_background: u16::MAX,
            congestion_threshold: (u16::MAX / 4) * 3,
            max_write: MAX_WRITE,
            // Nanoseconds.
            time_gran: 1,
            max_pages: max_pages as u16,
            map_alignment: 0,
            flags2: (enabled >> 32) as u32,
            unused: [0; 7],
        };
        Ok(out.as_bytes().to_vec())
    }

    async fn lookup(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        // A test's: a handler that panics, which the mount answers `EIO` (board 8539).
        fail::fail_point!("fuse::lookup");
        let [name] = strings(body.rest())?;
        let dir = self.target(header.nodeid)?;
        match self.lookup_in(&dir, name).await {
            Ok(Found::Node(slot, found)) => {
                Ok(self.entry(slot, (header.nodeid, name), found)?.as_bytes().to_vec())
            }
            // A directory of the server's own, which comes and goes as cells do: given for no
            // time, looked up again each time.
            Ok(Found::Cells(sub)) => {
                Ok(entry_out(sub, sub, 0, &self.cells_attr(sub)).as_bytes().to_vec())
            }
            // A missing name is an entry without a node, which the kernel caches, but in the
            // directories of cells, where a cell may be attached at it any time.
            Err(Errno::ENOENT) => {
                let valid = match dir {
                    Target::Cells(_) => 0,
                    Target::Node(..) => TIMEOUT_SECS,
                };
                Ok(EntryOut { entry_valid: valid, ..Default::default() }.as_bytes().to_vec())
            }
            Err(errno) => Err(errno),
        }
    }

    async fn getattr(&self, header: &InHeader) -> Result<Vec<u8>> {
        let (attr, inode, valid) = match self.target(header.nodeid)? {
            // Directories of cells come and go as cells attach and detach; do not cache their
            // attributes for 24h in the kernel (also ensures a dead mount returns ENOTCONN on
            // stat, board 11554).
            Target::Cells(dir) => (self.cells_attr(dir), dir, 0),
            Target::Node(slot, node) => {
                let attr = node.getattr().await?;
                let inode = self.inode(slot, attr.inode.0)?;
                (attr, inode, TIMEOUT_SECS)
            }
        };
        Ok(attr_out_for(&attr, inode, valid).as_bytes().to_vec())
    }

    async fn setattr(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let set: SetattrIn = body.read()?;
        let valid = |flag| set.valid & flag != 0;
        let set_time = |flag, now_flag, secs, nsecs| {
            Ok(if valid(now_flag) {
                Some(SetTime::Now)
            } else if valid(flag) {
                Some(SetTime::At(time(secs, nsecs)?))
            } else {
                None
            })
        };
        let explicit_ctime =
            if valid(FATTR_CTIME) { Some(time(set.ctime, set.ctimensec)?) } else { None };
        let killpriv_v2 = self.handle_killpriv_v2();
        // Linux's `chown_common` sets `ATTR_KILL_SUID | ATTR_KILL_SGID | ATTR_KILL_PRIV` even for
        // a no-op `chown(-1, -1)` on any non-directory. With `FUSE_HANDLE_KILLPRIV_V2`,
        // `fuse_setattr` strips `ATTR_MODE`, and `fuse_do_setattr` only sets `FATTR_KILL_SUIDGID`
        // when `ATTR_UID | ATTR_GID` is present — leaving at most `FATTR_CTIME` and `FATTR_FH`.
        // No other VFS operation sends a `SETATTR` without mode/uid/gid/size/atime/mtime.
        let noop_chown = (set.valid & !(FATTR_CTIME | FATTR_FH)) == 0;
        let kill_suid_sgid = killpriv_v2 && (valid(FATTR_KILL_SUIDGID) || noop_chown);
        let kill_security_cap =
            killpriv_v2 && (kill_suid_sgid || valid(FATTR_UID) || valid(FATTR_GID));
        let set = SetAttr {
            perm: valid(FATTR_MODE).then_some(perm(set.mode)),
            uid: valid(FATTR_UID).then_some(set.uid),
            gid: valid(FATTR_GID).then_some(set.gid),
            size: valid(FATTR_SIZE).then_some(set.size),
            atime: set_time(FATTR_ATIME, FATTR_ATIME_NOW, set.atime, set.atimensec)?,
            mtime: set_time(FATTR_MTIME, FATTR_MTIME_NOW, set.mtime, set.mtimensec)?,
            kill_suid_sgid,
        };
        let (slot, node) = self.node(header.nodeid, Errno::EPERM)?;
        let attr = node.setattr_fuse(&set, explicit_ctime, kill_security_cap).await?;
        // The reply's attributes are the kernel's at once, but a stat's sent before them would
        // be taken after them, as fresh (`fuse_do_setattr` leaves `attr_version` alone).
        self.changed([header.nodeid]);
        Ok(attr_out(&attr, self.inode(slot, attr.inode.0)?).as_bytes().to_vec())
    }

    async fn readlink(&self, header: &InHeader) -> Result<Vec<u8>> {
        self.node(header.nodeid, Errno::EINVAL)?.1.readlink().await
    }

    async fn fsync(&self, header: &InHeader) -> Result<Vec<u8>> {
        if let Target::Node(_, node) = self.target(header.nodeid)? {
            node.fsync().await?;
        }
        Ok(Vec::new())
    }

    /// The owner of a node the caller makes in the directory `dir`, and whether `dir`'s setgid is
    /// set, which a directory made in it takes too. Linux's VFS gives new inodes their owners
    /// (`inode_init_owner`), which FUSE leaves to the server: the caller's, but the group of a
    /// directory whose setgid is set. (Linux clears a new file's own setgid, if its caller isn't
    /// of its group, as it can't tell here, and leaves it.)
    async fn owner(&self, dir: &Node, header: &InHeader) -> Result<(Owner, bool)> {
        let attr = dir.getattr().await?;
        let setgid = attr.perm as u32 & S_ISGID != 0;
        let gid = if setgid { attr.gid } else { header.gid };
        Ok((Owner { uid: header.uid, gid }, setgid))
    }

    async fn symlink(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        // The target is a path, not a name.
        let [name, target] = strings(body.rest())?;
        let (slot, dir) = self.node(header.nodeid, Errno::EPERM)?;
        let (owner, _) = self.owner(&dir, header).await?;
        let made = dir.symlink(name, target, owner).await?;
        self.changed([header.nodeid]);
        Ok(self.entry(slot, (header.nodeid, name), made)?.as_bytes().to_vec())
    }

    async fn mknod(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let MknodIn { mode, rdev, umask, .. } = body.read()?;
        let [name] = strings(body.rest())?;
        // A mode without a type is a regular file's.
        let file_type = FileType::from_mode(mode).unwrap_or(FileType::File);
        let (slot, dir) = self.node(header.nodeid, Errno::EPERM)?;
        let (owner, _) = self.owner(&dir, header).await?;
        let made = dir.mknod(name, file_type, perm(mode & !umask), rdev, owner).await?;
        self.changed([header.nodeid]);
        Ok(self.entry(slot, (header.nodeid, name), made)?.as_bytes().to_vec())
    }

    async fn mkdir(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let MkdirIn { mode, umask } = body.read()?;
        let [name] = strings(body.rest())?;
        let (slot, dir) = self.node(header.nodeid, Errno::EPERM)?;
        let (owner, setgid) = self.owner(&dir, header).await?;
        let perm = perm(mode & !umask) | if setgid { S_ISGID as u16 } else { 0 };
        let made = dir.mkdir(name, perm, owner).await?;
        self.changed([header.nodeid]);
        Ok(self.entry(slot, (header.nodeid, name), made)?.as_bytes().to_vec())
    }

    async fn create(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let CreateIn { mode, umask, .. } = body.read()?;
        let [name] = strings(body.rest())?;
        let (slot, dir) = self.node(header.nodeid, Errno::EPERM)?;
        let (owner, _) = self.owner(&dir, header).await?;
        let made = dir.create(name, perm(mode & !umask), owner).await?;
        self.changed([header.nodeid]);
        let mut reply = self.entry(slot, (header.nodeid, name), made)?.as_bytes().to_vec();
        reply.extend_from_slice(open_out().as_bytes());
        Ok(reply)
    }

    /// Removes a name, as `rmdir` does if `directory`, and as `unlink` does otherwise.
    async fn remove(
        &self,
        header: &InHeader,
        body: &mut Body<'_>,
        directory: bool,
    ) -> Result<Vec<u8>> {
        let [name] = strings(body.rest())?;
        let (_, dir) = self.node(header.nodeid, Errno::EPERM)?;
        match directory {
            true => dir.rmdir(name).await?,
            false => dir.unlink(name).await?,
        }
        // The node unlinked the kernel knows changed (`fuse_entry_unlinked`).
        self.changed([header.nodeid]);
        if let Shape::Many { .. } = self.shape {
            self.nodes.lock().unwrap().unnamed(&(header.nodeid, name.into()));
        }
        Ok(Vec::new())
    }

    async fn rename(
        &self,
        header: &InHeader,
        body: &mut Body<'_>,
        with_flags: bool,
    ) -> Result<Vec<u8>> {
        let (new_dir, flags) = match with_flags {
            true => {
                let Rename2In { newdir, flags, .. } = body.read()?;
                (newdir, flags)
            }
            false => (body.read::<RenameIn>()?.newdir, 0),
        };
        let [name, new_name] = strings(body.rest())?;
        let flags = RenameFlags::from_bits_retain(flags);
        // Nodes of two cells are refused by their filesystem (`EXDEV`).
        match (self.target(header.nodeid)?, self.target(new_dir)?) {
            (Target::Node(slot, dir), Target::Node(_, to)) => {
                let moved = dir.rename(name, &to, new_name, flags).await?;
                // Its ctime too, which the kernel marks stale as it does the directories'.
                let moved = self.swap(self.inode(slot, moved.0)?);
                self.changed([header.nodeid, new_dir, moved]);
                if let Shape::Many { .. } = self.shape {
                    let (from, to) = ((header.nodeid, name.into()), (new_dir, new_name.into()));
                    let exchange = flags.contains(RenameFlags::EXCHANGE);
                    self.nodes.lock().unwrap().renamed(from, to, exchange);
                }
            }
            (Target::Cells(_), Target::Cells(_)) => return Err(Errno::EPERM),
            _ => return Err(Errno::EXDEV),
        }
        Ok(Vec::new())
    }

    async fn link(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let LinkIn { oldnodeid } = body.read()?;
        let [new_name] = strings(body.rest())?;
        let (_, node) = self.node(oldnodeid, Errno::EPERM)?;
        // A node of another cell is refused by its filesystem (`EXDEV`).
        let (slot, dir) = self.node(header.nodeid, Errno::EXDEV)?;
        let attr = node.link(&dir, new_name).await?;
        // The node linked the kernel knows changed: the reply's attributes are its (`fuse_iget`).
        self.changed([header.nodeid]);
        Ok(self.entry(slot, (header.nodeid, new_name), (node, attr))?.as_bytes().to_vec())
    }

    async fn read(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let ReadIn { offset, size, .. } = body.read()?;
        if size > MAX_WRITE {
            return Err(Errno::EINVAL);
        }
        self.node(header.nodeid, Errno::EISDIR)?
            .1
            .read_prefixed(offset, size as usize, size_of::<OutHeader>())
            .await
    }

    async fn write(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let WriteIn { offset, size, write_flags, .. } = body.read()?;
        if size > MAX_WRITE {
            return Err(Errno::EINVAL);
        }
        let data = body.take(size as usize)?;
        let (_, file) = self.node(header.nodeid, Errno::EISDIR)?;
        let write_cache = write_flags & WRITE_CACHE != 0;
        let kill_priv = self.handle_killpriv_v2() && (write_flags & WRITE_KILL_SUIDGID != 0);
        let (written, mode_changed) =
            file.write_fuse(offset, data, !write_cache, kill_priv).await?;
        if mode_changed {
            self.changed([header.nodeid]);
        }
        Ok(WriteOut { size: written as u32, padding: 0 }.as_bytes().to_vec())
    }

    /// The usage of the filesystem the node is of; none of the root directory of cells', which
    /// holds no blocks of its own.
    async fn statfs(&self, header: &InHeader) -> Result<Vec<u8>> {
        let out = match self.target(header.nodeid)? {
            Target::Cells(_) => Kstatfs {
                bsize: CELLS_BLOCK_SIZE,
                frsize: CELLS_BLOCK_SIZE,
                namelen: crate::filesystem::NAME_MAX as u32,
                ..Default::default()
            },
            Target::Node(slot, _) => {
                let statfs = self.filesystem(slot)?.statfs().await?;
                Kstatfs {
                    blocks: statfs.blocks,
                    bfree: statfs.blocks_free,
                    bavail: statfs.blocks_free,
                    bsize: statfs.block_size,
                    namelen: statfs.name_max,
                    frsize: statfs.block_size,
                    ..Default::default()
                }
            }
        };
        Ok(out.as_bytes().to_vec())
    }

    async fn setxattr(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let SetxattrIn { size, flags } = body.read()?;
        let mode = match flags {
            0 => XattrMode::Set,
            XATTR_CREATE => XattrMode::Create,
            XATTR_REPLACE => XattrMode::Replace,
            _ => return Err(Errno::EINVAL),
        };
        // The name, then the value, which ends the body.
        let rest = body.rest();
        let end = rest.iter().position(|&byte| byte == 0).ok_or(Errno::EINVAL)?;
        let (name, value) = (&rest[..end], &rest[end + 1..]);
        if value.len() != size as usize {
            return Err(Errno::EINVAL);
        }
        let name = xattr_name(name)?;
        self.node(header.nodeid, Errno::EPERM)?.1.setxattr(name, value, mode).await?;
        self.changed([header.nodeid]);
        Ok(Vec::new())
    }

    async fn getxattr(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let GetxattrIn { size, .. } = body.read()?;
        let [name] = strings(body.rest())?;
        let name = xattr_name(name)?;
        let value = self.node(header.nodeid, Errno::ENODATA)?.1.getxattr(name).await?;
        sized(value, size)
    }

    async fn listxattr(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let GetxattrIn { size, .. } = body.read()?;
        let names = match self.target(header.nodeid)? {
            Target::Cells(_) => Vec::new(),
            Target::Node(_, node) => node.listxattr().await?,
        };
        // Each name NUL-terminated, as listxattr(2) lists them.
        let list = names.into_iter().flat_map(|name| name.into_iter().chain([0])).collect();
        sized(list, size)
    }

    async fn removexattr(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let [name] = strings(body.rest())?;
        let name = xattr_name(name)?;
        self.node(header.nodeid, Errno::EPERM)?.1.removexattr(name).await?;
        self.changed([header.nodeid]);
        Ok(Vec::new())
    }

    async fn opendir(&self, header: &InHeader) -> Result<Vec<u8>> {
        let (open, open_flags) = match self.target(header.nodeid)? {
            // Cells come and go: the kernel caches none of its entries.
            Target::Cells(dir) => (self.open_cells(dir)?, 0),
            Target::Node(slot, dir) => {
                let open = self.open_dir(header.nodeid, slot, dir)?;
                // The mount is the directory's only writer, so the kernel may cache its entries.
                (open, FOPEN_KEEP_CACHE | FOPEN_CACHE_DIR)
            }
        };
        let mut dirs = self.dirs.lock().unwrap();
        let fh = dirs.next;
        dirs.next += 1;
        dirs.open.insert(fh, Arc::new(open));
        Ok(OpenOut { fh, open_flags, padding: 0 }.as_bytes().to_vec())
    }

    /// The directory of the server's own whose node ID is `dir`, opened: its entries the
    /// directories of the server's in it and the cells attached there. `ESTALE` once it is gone.
    fn open_cells(&self, dir: u64) -> Result<OpenDir> {
        let cells = self.cells.lock().unwrap();
        let of = cells.dirs.get(&dir).ok_or(Errno::ESTALE)?;
        let entries = (of.entries.iter())
            .filter_map(|(name, entry)| {
                let inode = match *entry {
                    Entry::Dir(sub) if cells.shows(sub) => sub,
                    Entry::Dir(_) => return None,
                    Entry::Cell(slot) => {
                        let State::Attached(fs) = &cells.slots[&slot].state else { return None };
                        // Object IDs too large fail as they are looked up.
                        self.inode(slot, fs.root().inode().0).unwrap_or(0)
                    }
                };
                let name = name.clone().into_bytes();
                Some(DirEntry { inode: Inode(inode), file_type: FileType::Directory, name })
            })
            .collect();
        let target = Target::Cells(dir);
        Ok(OpenDir {
            dir: target,
            nodeid: dir,
            inode: dir,
            parent: of.parent,
            listing: DirListing::Cells(entries),
        })
    }

    /// The directory `dir` of the cell in `slot`, whose node ID is `nodeid`, opened lazily: reads
    /// no entries from Fxfs on `OPENDIR` (TASK-619, board 2634).
    fn open_dir(&self, nodeid: u64, slot: u16, dir: Node) -> Result<OpenDir> {
        let inode = self.inode(slot, dir.inode().0)?;
        let parent = match dir.parent()? {
            // A cell's root, which is its own parent, is in a directory of the server's own.
            parent if parent == dir.inode() && matches!(self.shape, Shape::Many { .. }) => {
                let cells = self.cells.lock().unwrap();
                cells.slots.get(&slot).map_or(ROOT_ID, |slot| slot.dir)
            }
            parent => self.inode(slot, parent.0)?,
        };
        Ok(OpenDir {
            dir: Target::Node(slot, dir),
            nodeid,
            inode,
            parent,
            listing: DirListing::Node(futures::lock::Mutex::new(DirCursor::new())),
        })
    }

    /// Lists the open directory's entries from the offset asked, as many as fit in the size asked,
    /// each with its offset, the offset to list the next from: `.`, `..`, then those the
    /// directory holds. At offset 0 the listing is refreshed from the current directory so an
    /// older handle or a rewound handle (`rewinddir`) never serves stale entries into the kernel's
    /// `FOPEN_CACHE_DIR` cache (TASK-554, board 2504). For Fxfs directories, entries are streamed
    /// in bounded pages rather than copied whole on `OPENDIR` (TASK-619, board 2634). With `plus`,
    /// each has its node's entry, and a lookup of it, but `.` and `..`, which the kernel takes no
    /// lookups of, and entries removed since the listing was taken are left out.
    async fn readdir(&self, body: &mut Body<'_>, plus: bool) -> Result<Vec<u8>> {
        let ReadIn { fh, offset, size, .. } = body.read()?;
        if size > MAX_WRITE {
            return Err(Errno::EINVAL);
        }
        let open = self.dirs.lock().unwrap().open.get(&fh).cloned().ok_or(Errno::EBADF)?;
        match &open.listing {
            DirListing::Cells(_) => self.readdir_cells(fh, open, offset, size, plus).await,
            DirListing::Node(cursor) => {
                let Target::Node(slot, dir) = &open.dir else { unreachable!() };
                self.readdir_node(&open, *slot, dir, cursor, offset, size, plus).await
            }
        }
    }

    async fn readdir_cells(
        &self,
        fh: u64,
        open: Arc<OpenDir>,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> Result<Vec<u8>> {
        let open = if offset == 0 {
            let Target::Cells(dir) = open.dir else { unreachable!() };
            let fresh = Arc::new(self.open_cells(dir)?);
            let mut dirs = self.dirs.lock().unwrap();
            if let Some(slot) = dirs.open.get_mut(&fh) {
                *slot = Arc::clone(&fresh);
            }
            fresh
        } else {
            open
        };
        let DirListing::Cells(entries) = &open.listing else { unreachable!() };
        let dots = [(&b"."[..], open.inode), (b"..", open.parent)].map(|(name, inode)| DirEntry {
            inode: Inode(inode),
            file_type: FileType::Directory,
            name: name.to_vec(),
        });
        let listed = dots.iter().chain(entries).enumerate().skip(offset as usize);
        let mut reply = Vec::new();
        for (index, entry) in listed {
            let dirent_len = (size_of::<Dirent>() + entry.name.len()).next_multiple_of(8);
            let len = dirent_len + if plus { size_of::<EntryOut>() } else { 0 };
            if reply.len() + len > size as usize {
                break;
            }
            let mut entry = entry.clone();
            if plus {
                let found = match index {
                    // The kernel takes these from its own dentries.
                    0 | 1 => EntryOut::default(),
                    _ => match self.lookup_in(&open.dir, &entry.name).await {
                        Ok(Found::Cells(sub)) => entry_out(sub, sub, 0, &self.cells_attr(sub)),
                        Ok(Found::Node(slot, found)) => {
                            match self.entry(slot, (open.nodeid, &entry.name), found) {
                                Ok(found) => {
                                    entry.inode = Inode(found.attr.ino);
                                    found
                                }
                                // Failing past entries already in the reply: those go, and the next
                                // read asks again from there.
                                Err(_) if !reply.is_empty() => break,
                                Err(errno) => return Err(errno),
                            }
                        }
                        // Removed since the directory was opened.
                        Err(Errno::ENOENT) => continue,
                        // The entries so far took lookups, which the kernel forgets only if it
                        // gets them.
                        Err(_) if !reply.is_empty() => break,
                        Err(errno) => return Err(errno),
                    },
                };
                reply.extend_from_slice(found.as_bytes());
            }
            push_dirent(&mut reply, &entry, index as u64 + 1, dirent_len);
        }
        Ok(reply)
    }

    #[allow(clippy::too_many_arguments)]
    async fn readdir_node(
        &self,
        open: &OpenDir,
        slot: u16,
        dir: &Node,
        cursor: &futures::lock::Mutex<DirCursor>,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> Result<Vec<u8>> {
        let mut cursor = cursor.lock().await;
        if offset == 0 {
            *cursor = DirCursor::new();
        }
        let mut reply = Vec::new();
        let mut cur_offset = offset;
        let extra = if plus { size_of::<EntryOut>() } else { 0 };
        for (dot_off, name, inode) in
            [(0u64, &b"."[..], open.inode), (1u64, &b".."[..], open.parent)]
        {
            if cur_offset == dot_off {
                let dirent_len = (size_of::<Dirent>() + name.len()).next_multiple_of(8);
                let len = dirent_len + extra;
                if reply.len() + len > size as usize {
                    return Ok(reply);
                }
                if plus {
                    reply.extend_from_slice(EntryOut::default().as_bytes());
                }
                let entry = DirEntry {
                    inode: Inode(inode),
                    file_type: FileType::Directory,
                    name: name.to_vec(),
                };
                cur_offset += 1;
                push_dirent(&mut reply, &entry, cur_offset, dirent_len);
            }
        }
        if cur_offset < 2 {
            return Ok(reply);
        }

        // Align the streaming cursor to `cur_offset`: sequential reads reuse the bounded
        // read-ahead buffer and resume by name (`EntriesStart::After`); retries and mid-reply
        // resumes (e.g., a smaller user `getdents` buffer consuming only part of a `PAGE_SIZE`
        // FUSE reply) match a bounded recent `(offset -> last_name)` bookmark and resume by name
        // rather than index-seeking the live directory (hotbed review 109); arbitrary unseen
        // `seekdir` offsets fall back to an index start.
        let mut index_seek = false;
        if cur_offset == cursor.next_offset {
            let last_name = cursor.last_name.clone();
            cursor.record_bookmark(cur_offset, last_name);
        } else if let Some((_, saved_name)) =
            cursor.recent.iter().rev().find(|(off, _)| *off == cur_offset).cloned()
        {
            cursor.next_offset = cur_offset;
            cursor.last_name = saved_name;
            cursor.buffered.clear();
            cursor.eof = false;
            while cursor.recent.back().is_some_and(|(off, _)| *off > cur_offset) {
                cursor.recent.pop_back();
            }
        } else {
            cursor.next_offset = cur_offset;
            cursor.last_name = None;
            cursor.buffered.clear();
            cursor.eof = false;
            cursor.recent.clear();
            index_seek = true;
        }

        let min_entry_len = MIN_DIRENT_LEN + extra;
        while reply.len() + min_entry_len <= size as usize {
            if cursor.buffered.is_empty() {
                if cursor.eof {
                    break;
                }
                let remaining_bytes = (size as usize).saturating_sub(reply.len());
                let reply_entries = (remaining_bytes / min_entry_len).max(1);
                let batch_limit = (reply_entries + READDIR_READAHEAD).min(READDIR_MAX_BATCH);
                let start = if index_seek {
                    EntriesStart::Index(cur_offset - 2)
                } else if let Some(last) = cursor.last_name.as_deref() {
                    EntriesStart::After(last)
                } else {
                    EntriesStart::Index(0)
                };
                let (mut page, has_more) = match dir.entries_page(start, batch_limit).await {
                    Ok(page) => page,
                    // Entries already placed in `reply` (especially for `READDIRPLUS`, which took
                    // lookups the kernel only forgets if it receives them) must be delivered; the
                    // next read at the reply's end offset will hit the error with an empty reply
                    // (hotbed review 109).
                    Err(_) if !reply.is_empty() => break,
                    Err(errno) => return Err(errno),
                };
                let mut map_err = None;
                for entry in &mut page {
                    match self.inode(slot, entry.inode.0) {
                        Ok(ino) => entry.inode = Inode(ino),
                        Err(errno) => {
                            map_err = Some(errno);
                            break;
                        }
                    }
                }
                if let Some(errno) = map_err {
                    // Deliver entries already encoded in `reply` so taken `READDIRPLUS` lookups
                    // are not leaked; the next read surfaces `errno` on an empty reply (hotbed
                    // review 109).
                    if !reply.is_empty() {
                        break;
                    }
                    return Err(errno);
                }
                cursor.buffered.extend(page);
                cursor.eof = !has_more;
                index_seek = false;
                if cursor.buffered.is_empty() {
                    break;
                }
            }
            let Some(mut entry) = cursor.buffered.pop_front() else { break };
            let dirent_len = (size_of::<Dirent>() + entry.name.len()).next_multiple_of(8);
            let len = dirent_len + extra;
            if reply.len() + len > size as usize {
                cursor.buffered.push_front(entry);
                break;
            }
            if plus {
                let found = match self.lookup_in(&open.dir, &entry.name).await {
                    Ok(Found::Cells(sub)) => entry_out(sub, sub, 0, &self.cells_attr(sub)),
                    Ok(Found::Node(slot, found)) => {
                        match self.entry(slot, (open.nodeid, &entry.name), found) {
                            Ok(found) => {
                                entry.inode = Inode(found.attr.ino);
                                found
                            }
                            // Failing past entries already in the reply: those go, and the next
                            // read asks again from there.
                            Err(_) if !reply.is_empty() => {
                                cursor.buffered.push_front(entry);
                                break;
                            }
                            Err(errno) => {
                                cursor.buffered.push_front(entry);
                                return Err(errno);
                            }
                        }
                    }
                    // Removed since the page was read: advance `last_name` without advancing
                    // `cur_offset` / `next_offset` so the next read at the last emitted dirent's
                    // `d.off` still hits the sequential fast path and skips past this vanished
                    // entry (hotbed review 109).
                    Err(Errno::ENOENT) => {
                        cursor.record_bookmark(cur_offset, Some(entry.name.clone()));
                        cursor.last_name = Some(entry.name);
                        continue;
                    }
                    // The entries so far took lookups, which the kernel forgets only if it gets
                    // them.
                    Err(_) if !reply.is_empty() => {
                        cursor.buffered.push_front(entry);
                        break;
                    }
                    Err(errno) => {
                        cursor.buffered.push_front(entry);
                        return Err(errno);
                    }
                };
                reply.extend_from_slice(found.as_bytes());
            }
            cur_offset += 1;
            push_dirent(&mut reply, &entry, cur_offset, dirent_len);
            cursor.record_bookmark(cur_offset, Some(entry.name.clone()));
            cursor.last_name = Some(entry.name);
            cursor.next_offset = cur_offset;
        }
        Ok(reply)
    }

    fn releasedir(&self, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let ReleaseIn { fh, .. } = body.read()?;
        let closed = self.dirs.lock().unwrap().open.remove(&fh);
        // Its handle on the directory goes: not under the table's lock.
        drop(closed);
        Ok(Vec::new())
    }

    async fn fallocate(&self, header: &InHeader, body: &mut Body<'_>) -> Result<Vec<u8>> {
        let FallocateIn { offset, length, mode, .. } = body.read()?;
        // Only the default mode, 0, which allocates.
        if mode != 0 {
            return Err(Errno::ENOTSUP);
        }
        let killpriv_v2 = self.handle_killpriv_v2();
        let kill_suid_sgid = killpriv_v2 && header.uid != 0;
        let kill_security_cap = killpriv_v2;
        let mode_changed = self
            .node(header.nodeid, Errno::EISDIR)?
            .1
            .allocate_fuse(offset, length, kill_suid_sgid, kill_security_cap)
            .await?;
        if mode_changed {
            self.changed([header.nodeid]);
        }
        Ok(Vec::new())
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cells = self.cells.lock().unwrap();
        let names: Vec<&str> = cells.slots.values().map(|slot| slot.name.as_str()).collect();
        f.debug_struct("Server").field("cells", &names).finish_non_exhaustive()
    }
}

/// A slot and name taken for a cell being opened ([`Server::reserve`]): given back unless it is
/// attached.
struct Reserved<'a> {
    server: &'a Server,
    slot: Option<u16>,
}

impl Reserved<'_> {
    /// Attaches `fs` in the slot: `ENOTCONN` if the server serves no more.
    fn attach(mut self, fs: Arc<Filesystem>) -> Result<()> {
        let mut cells = self.server.cells.lock().unwrap();
        if cells.closed {
            return Err(Errno::ENOTCONN);
        }
        let slot = self.slot.take().expect("attached once");
        let entry = cells.slots.get_mut(&slot).expect("a reserved slot");
        entry.cell = fs.cell();
        entry.state = State::Attached(fs);
        Ok(())
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        let Some(slot) = self.slot else { return };
        self.server.cells.lock().unwrap().remove(slot);
    }
}

const INVAL_INODE_LEN: usize = size_of::<OutHeader>() + size_of::<NotifyInvalInodeOut>();

/// A notification that the node `ino`'s attributes are stale, its cached pages left alone.
fn inval_inode(ino: u64) -> [u8; INVAL_INODE_LEN] {
    inval_inode_msg(ino, -1)
}

/// cladefs: a notification that both `ino`'s attributes and all of its cached page-cache data
/// (`off = 0, len = 0`) are stale and must be dropped by the kernel (TASK-562, board 2520).
fn inval_inode_pages(ino: u64) -> [u8; INVAL_INODE_LEN] {
    inval_inode_msg(ino, 0)
}

fn inval_inode_msg(ino: u64, off: i64) -> [u8; INVAL_INODE_LEN] {
    let body = NotifyInvalInodeOut { ino, off, len: 0 };
    let header = OutHeader { len: INVAL_INODE_LEN as u32, error: notify::INVAL_INODE, unique: 0 };
    let mut message = [0u8; INVAL_INODE_LEN];
    message[..size_of::<OutHeader>()].copy_from_slice(header.as_bytes());
    message[size_of::<OutHeader>()..].copy_from_slice(body.as_bytes());
    message
}

/// A notification that the kernel drop the entry of `name` in the directory `parent`.
fn inval_entry(parent: u64, name: &[u8]) -> Vec<u8> {
    let body = NotifyInvalEntryOut { parent, namelen: name.len() as u32, flags: 0 };
    let len = size_of::<OutHeader>() + size_of::<NotifyInvalEntryOut>() + name.len() + 1;
    let header = OutHeader { len: len as u32, error: notify::INVAL_ENTRY, unique: 0 };
    let mut message = Vec::with_capacity(len);
    message.extend_from_slice(header.as_bytes());
    message.extend_from_slice(body.as_bytes());
    message.extend_from_slice(name);
    message.push(0);
    message
}

/// What remains of a request's body, read from its front.
struct Body<'a>(&'a [u8]);

impl<'a> Body<'a> {
    /// The message at the front: `EINVAL` if too little remains.
    fn read<T: FromBytes>(&mut self) -> Result<T> {
        let (message, rest) = T::read_from_prefix(self.0).map_err(|_| Errno::EINVAL)?;
        self.0 = rest;
        Ok(message)
    }

    /// The `len` bytes at the front: `EINVAL` if fewer remain.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len).ok_or(Errno::EINVAL)?;
        self.0 = rest;
        Ok(taken)
    }

    /// What remains.
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
}

/// A handler's reply payload: either plain bytes or a buffer that already reserves
/// `size_of::<OutHeader>()` leading bytes for in-place header stamping (board 12092).
enum Payload {
    Bytes(Vec<u8>),
    Prefixed(Vec<u8>),
}

/// A reply to the request `unique`.
struct Reply {
    unique: u64,
}

impl Reply {
    fn ok(&self, payload: Payload) -> Vec<u8> {
        match payload {
            Payload::Prefixed(mut buf) => {
                debug_assert!(buf.len() >= size_of::<OutHeader>());
                let header = OutHeader { len: buf.len() as u32, error: 0, unique: self.unique };
                buf[..size_of::<OutHeader>()].copy_from_slice(header.as_bytes());
                buf
            }
            Payload::Bytes(payload) => {
                let len = size_of::<OutHeader>() + payload.len();
                let mut reply = Vec::with_capacity(len);
                let header = OutHeader { len: len as u32, error: 0, unique: self.unique };
                reply.extend_from_slice(header.as_bytes());
                reply.extend_from_slice(&payload);
                reply
            }
        }
    }

    fn error(&self, errno: Errno) -> Vec<u8> {
        let len = size_of::<OutHeader>() as u32;
        OutHeader { len, error: -errno.raw(), unique: self.unique }.as_bytes().to_vec()
    }
}

/// The `N` NUL-terminated strings that make up `body`, which holds no more: `EINVAL` otherwise.
fn strings<const N: usize>(body: &[u8]) -> Result<[&[u8]; N]> {
    let mut strings = body.split_inclusive(|&byte| byte == 0);
    let found: [&[u8]; N] = std::array::from_fn(|_| strings.next().unwrap_or_default());
    (found.iter().all(|string| string.last() == Some(&0)) && strings.next().is_none())
        .then(|| found.map(|string| &string[..string.len() - 1]))
        .ok_or(Errno::EINVAL)
}

/// The namespaces of extended attributes served: those the guest's Fxfs has handlers of. POSIX
/// ACLs' (`system.posix_acl_*`) the kernel refuses itself, as INIT doesn't offer `POSIX_ACL`.
const XATTR_NAMESPACES: [&[u8]; 3] = [b"user.", b"trusted.", b"security."];

/// `name`, an extended attribute's, of a namespace served, as Linux's VFS resolves a name to its
/// filesystem's handler (`xattr_resolve_name`), which FUSE leaves to its servers: `EOPNOTSUPP` of
/// another namespace, `EINVAL` for a namespace's prefix alone.
fn xattr_name(name: &[u8]) -> Result<&[u8]> {
    match XATTR_NAMESPACES.iter().find(|prefix| name.starts_with(prefix)) {
        Some(prefix) if name.len() == prefix.len() => Err(Errno::EINVAL),
        Some(_) => Ok(name),
        None => Err(Errno::ENOTSUP),
    }
}

/// `value` for a request that asked for at most `size` bytes of it, or, asking for 0, its size:
/// `ERANGE` if it is longer, as getxattr(2) and listxattr(2) have it.
fn sized(value: Vec<u8>, size: u32) -> Result<Vec<u8>> {
    match size {
        0 => Ok(GetxattrOut { size: value.len() as u32, padding: 0 }.as_bytes().to_vec()),
        size if value.len() > size as usize => Err(Errno::ERANGE),
        _ => Ok(value),
    }
}

/// The permission bits of `mode`.
fn perm(mode: u32) -> u16 {
    (mode & 0o7777) as u16
}

/// The time `secs` and `nsecs` after the epoch. The kernel's seconds are signed, and times before
/// the epoch, which Fxfs cannot store, are invalid, as are nanoseconds of a second or more.
fn time(secs: u64, nsecs: u32) -> Result<SystemTime> {
    if (secs as i64) < 0 || nsecs >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    UNIX_EPOCH.checked_add(Duration::new(secs, nsecs)).ok_or(Errno::EINVAL)
}

/// The seconds and nanoseconds of `time` since the epoch, which cladefs's times are not before.
fn timespec(time: SystemTime) -> (u64, u32) {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since_epoch.as_secs(), since_epoch.subsec_nanos())
}

/// The kernel's attributes of a node whose are `attr`, its inode number `inode`.
fn fuse_attr(attr: &Attr, inode: u64) -> super::abi::Attr {
    let (atime, atimensec) = timespec(attr.atime);
    let (mtime, mtimensec) = timespec(attr.mtime);
    let (ctime, ctimensec) = timespec(attr.ctime);
    super::abi::Attr {
        ino: inode,
        size: attr.size,
        blocks: attr.blocks,
        atime,
        mtime,
        ctime,
        atimensec,
        mtimensec,
        ctimensec,
        mode: attr.mode(),
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: 0,
    }
}

/// The entry of the node `nodeid`, whose attributes are `attr`, its inode number `inode`, valid
/// for `valid` seconds.
fn entry_out(nodeid: u64, inode: u64, valid: u64, attr: &Attr) -> EntryOut {
    EntryOut {
        nodeid,
        generation: 0,
        entry_valid: valid,
        attr_valid: TIMEOUT_SECS,
        entry_valid_nsec: 0,
        attr_valid_nsec: 0,
        attr: fuse_attr(attr, inode),
    }
}

fn attr_out(attr: &Attr, inode: u64) -> AttrOut {
    attr_out_for(attr, inode, TIMEOUT_SECS)
}

fn attr_out_for(attr: &Attr, inode: u64, attr_valid: u64) -> AttrOut {
    let attr = fuse_attr(attr, inode);
    AttrOut { attr_valid, attr_valid_nsec: 0, dummy: 0, attr }
}

/// A created file's open: it needs no handle, nor flushing as it is closed, and its cached pages
/// stay valid, as the mount is the file's only writer.
fn open_out() -> OpenOut {
    OpenOut { fh: 0, open_flags: FOPEN_KEEP_CACHE | FOPEN_NOFLUSH, padding: 0 }
}

/// Appends `entry`'s dirent, whose next entry is at `next_offset`, padded to `dirent_len`.
fn push_dirent(reply: &mut Vec<u8>, entry: &DirEntry, next_offset: u64, dirent_len: usize) {
    let start = reply.len();
    let dirent = Dirent {
        ino: entry.inode.0,
        off: next_offset,
        namelen: entry.name.len() as u32,
        type_: entry.file_type.dirent_type().into(),
    };
    reply.extend_from_slice(dirent.as_bytes());
    reply.extend_from_slice(&entry.name);
    reply.resize(start + dirent_len, 0);
}

#[cfg(test)]
mod fuzz_tests;

#[cfg(test)]
mod cells_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    fn fuse_send(server: &Server, unique: u64, opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        use zerocopy::{FromBytes, IntoBytes};
        let header = InHeader {
            len: (size_of::<InHeader>() + body.len()) as u32,
            opcode,
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 1,
            padding: 0,
        };
        let mut req = header.as_bytes().to_vec();
        req.extend_from_slice(body);
        let raw = block_on(server.handle(&req)).unwrap().unwrap();
        let (out, payload) = OutHeader::read_from_prefix(&raw).unwrap();
        assert_eq!(out.error, 0, "opcode {opcode} failed with errno {}", -out.error);
        payload.to_vec()
    }

    /// The root's object ID and FUSE's root ID swap places as node IDs, and each other object ID
    /// is its own: whatever Fxfs numbers 1 has the root's object ID for its node ID, so no two
    /// nodes share one.
    #[test]
    fn the_root_and_fuses_root_id_swap_places() {
        let fs = block_on(Filesystem::format(crate::Storage::memory(8 << 20))).unwrap();
        let server = Server::new(fs, Box::new(|_| Ok(())));
        let root = server.root();
        assert_eq!((server.swap(root), server.swap(ROOT_ID)), (ROOT_ID, root));
        for id in [ROOT_ID, root, root + 1, 1 << 40] {
            assert_eq!(server.swap(server.swap(id)), id);
        }
        block_on(server.into_filesystem().close()).unwrap();
    }

    /// TASK-619 (board 2634): `OPENDIR` reads zero entries from Fxfs, a single 4096-byte `READDIR`
    /// of a 100,000-entry directory reads at most one reply's worth plus bounded read-ahead, and
    /// streaming across concurrent directory mutations still lists every unchanged entry once.
    #[test]
    fn opendir_and_one_readdir_do_not_walk_a_large_directory() {
        use fxfs::object_handle::ObjectHandle as _;
        use fxfs::object_store::ObjectDescriptor;
        use fxfs::object_store::directory::Directory;
        use fxfs::object_store::transaction::{LockKey, Options, lock_keys};
        use zerocopy::{FromBytes, IntoBytes};

        const TOTAL: usize = 100_000;
        const BATCH: usize = 500;
        // Build a 100,000-entry directory quickly by inserting links in batched Fxfs transactions
        // on a temporary Tokio runtime (FxFilesystem spawns background tasks on Tokio).
        let device = storage_device::DeviceHolder::new(
            storage_device::fake_device::FakeDevice::new(262_144, 512),
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let device = rt.block_on(async move {
            let fxfs = fxfs::filesystem::FxFilesystem::new_empty(device).await.unwrap();
            let root_vol = fxfs::object_store::volume::root_volume((*fxfs).clone()).await.unwrap();
            let store = root_vol
                .new_volume(
                    crate::filesystem::VOLUME_NAME,
                    fxfs::object_store::NewChildStoreOptions::default(),
                )
                .await
                .unwrap();
            let root_dir = Directory::open(&store, store.root_directory_object_id()).await.unwrap();
            let dir_lock =
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())];
            // One target file that all 100,000 directory entries name (fast, valid Fxfs directory).
            let target_id = {
                let mut txn =
                    store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
                let handle = root_dir.create_child_file(&mut txn, "seed").await.unwrap();
                txn.commit().await.unwrap();
                handle.object_id()
            };
            for chunk in (0..TOTAL).step_by(BATCH) {
                let mut txn =
                    store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
                for i in chunk..(chunk + BATCH).min(TOTAL) {
                    let name = format!("f{i:06}");
                    root_dir
                        .insert_child(&mut txn, &name, target_id, ObjectDescriptor::File)
                        .await
                        .unwrap();
                }
                txn.commit().await.unwrap();
            }
            drop((root_dir, store, root_vol));
            fxfs.close().await.unwrap();
            let device = fxfs.take_device().await;
            device.reopen(false);
            device
        });
        drop(rt);

        let fs = block_on(Filesystem::mount(crate::Storage::from_device(device))).unwrap();
        let server = Server::new(fs, Box::new(|_| Ok(())));
        let send = |unique: u64, opcode: u32, nodeid: u64, body: &[u8]| -> Vec<u8> {
            fuse_send(&server, unique, opcode, nodeid, body)
        };

        // 1. OPENDIR reads zero entries from Fxfs.
        crate::filesystem::READDIR_EMITS.store(0, Ordering::Relaxed);
        let open_payload = send(1, opcode::OPENDIR, ROOT_ID, &[]);
        let (open_out, _) = OpenOut::read_from_prefix(&open_payload).unwrap();
        assert_eq!(
            crate::filesystem::READDIR_EMITS.load(Ordering::Relaxed),
            0,
            "OPENDIR must not walk Fxfs directory entries"
        );

        // 2. One 4096-byte READDIR emits at most one reply's worth + bounded read-ahead.
        let read_in = ReadIn { fh: open_out.fh, offset: 0, size: 4096, ..Default::default() };
        let readdir_payload = send(2, opcode::READDIR, ROOT_ID, read_in.as_bytes());
        assert!(!readdir_payload.is_empty());
        let emits = crate::filesystem::READDIR_EMITS.load(Ordering::Relaxed);
        let max_expected = (4096 / MIN_DIRENT_LEN + READDIR_READAHEAD + 1) as u64;
        assert!(
            emits > 0 && emits <= max_expected,
            "one 4096-byte READDIR emitted {emits} Fxfs entries (expected <= {max_expected}, total {TOTAL})"
        );

        // Clean release.
        let rel = ReleaseIn { fh: open_out.fh, ..Default::default() };
        send(3, opcode::RELEASEDIR, ROOT_ID, rel.as_bytes());
        block_on(server.into_filesystem().close()).unwrap();

        // 3. Streaming across concurrent mutations lists every unchanged entry exactly once, and
        // rewinddir (offset 0) sees newly added entries.
        let fs2 = block_on(Filesystem::format(crate::Storage::memory(16 << 20))).unwrap();
        let root2 = fs2.root().clone();
        for i in 0..80u32 {
            block_on(root2.create(
                format!("entry-{i:03}").as_bytes(),
                0o644,
                Owner { uid: 0, gid: 0 },
            ))
            .unwrap();
        }
        let server2 = Server::new(fs2, Box::new(|_| Ok(())));
        let send2 = |unique: u64, opcode: u32, nodeid: u64, body: &[u8]| -> Vec<u8> {
            let header = InHeader {
                len: (size_of::<InHeader>() + body.len()) as u32,
                opcode,
                unique,
                nodeid,
                uid: 0,
                gid: 0,
                pid: 1,
                padding: 0,
            };
            let mut req = header.as_bytes().to_vec();
            req.extend_from_slice(body);
            let raw = block_on(server2.handle(&req)).unwrap().unwrap();
            let (out, payload) = OutHeader::read_from_prefix(&raw).unwrap();
            assert_eq!(out.error, 0, "opcode {opcode} failed with errno {}", -out.error);
            payload.to_vec()
        };
        let open2 = OpenOut::read_from_prefix(&send2(1, opcode::OPENDIR, ROOT_ID, &[])).unwrap().0;
        let mut offset = 0u64;
        let mut seen = Vec::new();
        let mut mutated = false;
        let mut unique = 2u64;
        loop {
            let ri = ReadIn { fh: open2.fh, offset, size: 512, ..Default::default() };
            let payload = send2(unique, opcode::READDIR, ROOT_ID, ri.as_bytes());
            unique += 1;
            if payload.is_empty() {
                break;
            }
            let mut rest = payload.as_slice();
            while !rest.is_empty() {
                let (d, after) = Dirent::read_from_prefix(rest).unwrap();
                let name = String::from_utf8(after[..d.namelen as usize].to_vec()).unwrap();
                let padded = (size_of::<Dirent>() + d.namelen as usize).next_multiple_of(8)
                    - size_of::<Dirent>();
                rest = &after[padded..];
                offset = d.off;
                if name != "." && name != ".." {
                    seen.push(name);
                }
            }
            if !mutated && seen.len() >= 10 {
                // Remove an early entry and a late entry, and add a new entry mid-listing.
                block_on(root2.unlink(b"entry-000")).unwrap();
                block_on(root2.unlink(b"entry-079")).unwrap();
                block_on(root2.create(b"entry-000-new", 0o644, Owner { uid: 0, gid: 0 })).unwrap();
                mutated = true;
            }
        }
        // Every unchanged entry (001..079) was listed exactly once with no duplicates.
        let mut counts = HashMap::new();
        for name in &seen {
            *counts.entry(name.as_str()).or_insert(0usize) += 1;
        }
        for i in 1..79u32 {
            let expected = format!("entry-{i:03}");
            assert_eq!(counts.get(expected.as_str()).copied(), Some(1), "entry {expected}");
        }
        send2(
            unique,
            opcode::RELEASEDIR,
            ROOT_ID,
            ReleaseIn { fh: open2.fh, ..Default::default() }.as_bytes(),
        );
        block_on(server2.into_filesystem().close()).unwrap();
    }

    /// hotbed review 109 (finding 404): resuming `READDIR` at a mid-reply offset after earlier
    /// entries were unlinked (e.g., musl's 2048-byte `getdents` buffer consuming part of a
    /// 4096-byte FUSE reply during `rm -r`) must resume by name rather than index-seeking the
    /// live directory, and a vanished (`ENOENT`) entry at the end of a `READDIRPLUS` reply must
    /// not advance `next_offset` past the last emitted dirent.
    #[test]
    fn readdir_mid_reply_resume_and_readdirplus_enoent_do_not_skip_entries() {
        use zerocopy::{FromBytes, IntoBytes};

        let parse_dirents = |payload: &[u8], plus: bool| -> Vec<(String, u64)> {
            let mut out = Vec::new();
            let mut rest = payload;
            while !rest.is_empty() {
                if plus {
                    rest = &rest[size_of::<EntryOut>()..];
                }
                let (d, after) = Dirent::read_from_prefix(rest).unwrap();
                let name = String::from_utf8(after[..d.namelen as usize].to_vec()).unwrap();
                let padded = (size_of::<Dirent>() + d.namelen as usize).next_multiple_of(8)
                    - size_of::<Dirent>();
                rest = &after[padded..];
                out.push((name, d.off));
            }
            out
        };

        let fs = block_on(Filesystem::format(crate::Storage::memory(16 << 20))).unwrap();
        let root = fs.root().clone();
        for i in 0..80u32 {
            block_on(root.create(
                format!("entry-{i:03}").as_bytes(),
                0o644,
                Owner { uid: 0, gid: 0 },
            ))
            .unwrap();
        }
        let server = Server::new(fs, Box::new(|_| Ok(())));
        let send = |unique: u64, opcode: u32, nodeid: u64, body: &[u8]| -> Vec<u8> {
            fuse_send(&server, unique, opcode, nodeid, body)
        };

        // 1. Mid-reply READDIR resume after unlinking earlier entries.
        let open = OpenOut::read_from_prefix(&send(1, opcode::OPENDIR, ROOT_ID, &[])).unwrap().0;
        let reply1 = send(
            2,
            opcode::READDIR,
            ROOT_ID,
            ReadIn { fh: open.fh, offset: 0, size: 4096, ..Default::default() }.as_bytes(),
        );
        let entries1 = parse_dirents(&reply1, false);
        let entry009_off = entries1
            .iter()
            .find(|(name, _)| name == "entry-009")
            .expect("entry-009 in first 4096-byte reply")
            .1;
        assert!(
            entries1.len() > 20,
            "first reply must extend past entry-009 so entry-009 is mid-reply"
        );
        for i in 0..5u32 {
            block_on(root.unlink(format!("entry-{i:03}").as_bytes())).unwrap();
        }
        let reply2 = send(
            3,
            opcode::READDIR,
            ROOT_ID,
            ReadIn { fh: open.fh, offset: entry009_off, size: 4096, ..Default::default() }
                .as_bytes(),
        );
        let entries2 = parse_dirents(&reply2, false);
        assert_eq!(
            entries2.first().map(|(n, _)| n.as_str()),
            Some("entry-010"),
            "mid-reply resume must return entry-010 first, not index-seek past unlinks"
        );
        send(
            4,
            opcode::RELEASEDIR,
            ROOT_ID,
            ReleaseIn { fh: open.fh, ..Default::default() }.as_bytes(),
        );

        // 2. READDIRPLUS where the last entry examined on a page vanishes (ENOENT).
        let open_plus =
            OpenOut::read_from_prefix(&send(5, opcode::OPENDIR, ROOT_ID, &[])).unwrap().0;
        // `.` (160) + `..` (160) + 3 entries (`entry-005..007`, 168 each) = 824 bytes.
        let p1 = send(
            6,
            opcode::READDIRPLUS,
            ROOT_ID,
            ReadIn { fh: open_plus.fh, offset: 0, size: 840, ..Default::default() }.as_bytes(),
        );
        let pe1 = parse_dirents(&p1, true);
        assert_eq!(pe1.last().map(|(n, _)| n.as_str()), Some("entry-007"));
        let off_after_007 = pe1.last().unwrap().1;

        // Unlink an earlier entry (entry-005) and the entry right after the next reply's capacity
        // (entry-010): reply 2 fits entry-008 and entry-009 (2 * 168 = 336 <= 400), hits ENOENT
        // on entry-010, and cannot fit entry-011 (3 * 168 = 504 > 400).
        block_on(root.unlink(b"entry-005")).unwrap();
        block_on(root.unlink(b"entry-010")).unwrap();
        let p2 = send(
            7,
            opcode::READDIRPLUS,
            ROOT_ID,
            ReadIn { fh: open_plus.fh, offset: off_after_007, size: 400, ..Default::default() }
                .as_bytes(),
        );
        let pe2 = parse_dirents(&p2, true);
        assert_eq!(
            pe2.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["entry-008", "entry-009"]
        );
        let off_after_009 = pe2.last().unwrap().1;

        let p3 = send(
            8,
            opcode::READDIRPLUS,
            ROOT_ID,
            ReadIn { fh: open_plus.fh, offset: off_after_009, size: 400, ..Default::default() }
                .as_bytes(),
        );
        let pe3 = parse_dirents(&p3, true);
        assert_eq!(
            pe3.first().map(|(n, _)| n.as_str()),
            Some("entry-011"),
            "READDIRPLUS after ENOENT at page boundary must resume at entry-011 without index-seek"
        );
        send(
            9,
            opcode::RELEASEDIR,
            ROOT_ID,
            ReleaseIn { fh: open_plus.fh, ..Default::default() }.as_bytes(),
        );
        block_on(server.into_filesystem().close()).unwrap();
    }

    /// hotbed review 109 (finding 405): when `READDIRPLUS` exhausts `cursor.buffered` mid-reply
    /// and the refill page fails (here `EOVERFLOW` from a hostile cell's object ID >= 2^48),
    /// `readdir_node` must return `Ok(reply)` for the entries that already took lookups and defer
    /// the error to the next call rather than dropping `reply` and leaking `Nodes::looked`.
    #[test]
    fn readdirplus_refill_error_returns_taken_lookups_without_leaking() {
        use fxfs::object_handle::ObjectHandle as _;
        use fxfs::object_store::ObjectDescriptor;
        use fxfs::object_store::directory::Directory;
        use fxfs::object_store::transaction::{LockKey, Options, lock_keys};
        use zerocopy::{FromBytes, IntoBytes};

        const NORMAL: usize = 60;
        let device = storage_device::DeviceHolder::new(
            storage_device::fake_device::FakeDevice::new(16_384, 512),
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let device = rt.block_on(async move {
            let fxfs = fxfs::filesystem::FxFilesystem::new_empty(device).await.unwrap();
            let root_vol = fxfs::object_store::volume::root_volume((*fxfs).clone()).await.unwrap();
            let store = root_vol
                .new_volume(
                    crate::filesystem::VOLUME_NAME,
                    fxfs::object_store::NewChildStoreOptions::default(),
                )
                .await
                .unwrap();
            let root_dir = Directory::open(&store, store.root_directory_object_id()).await.unwrap();
            let dir_lock =
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())];
            let target_id = {
                let mut txn =
                    store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
                let handle = root_dir.create_child_file(&mut txn, "seed").await.unwrap();
                txn.commit().await.unwrap();
                handle.object_id()
            };
            let mut txn =
                store.new_transaction(dir_lock.clone(), Options::default()).await.unwrap();
            for i in 0..NORMAL {
                root_dir
                    .insert_child(
                        &mut txn,
                        &format!("entry-{i:03}"),
                        target_id,
                        ObjectDescriptor::File,
                    )
                    .await
                    .unwrap();
            }
            // Hostile entry past the first page + read-ahead whose object ID overflows the
            // 48-bit per-slot node ID space on a multi-cell server (`EOVERFLOW`).
            root_dir
                .insert_child(&mut txn, "zzz-overflow", (1u64 << 48) + 1, ObjectDescriptor::File)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            drop((root_dir, store, root_vol));
            fxfs.close().await.unwrap();
            let device = fxfs.take_device().await;
            device.reopen(false);
            device
        });
        drop(rt);

        let server = Server::cells(Owner { uid: 0, gid: 0 }, Box::new(|_| Ok(())));
        block_on(server.attach("c", Filesystem::mount(crate::Storage::from_device(device))))
            .unwrap();

        let send_raw = |unique: u64, opcode: u32, nodeid: u64, body: &[u8]| -> (i32, Vec<u8>) {
            let header = InHeader {
                len: (size_of::<InHeader>() + body.len()) as u32,
                opcode,
                unique,
                nodeid,
                uid: 0,
                gid: 0,
                pid: 1,
                padding: 0,
            };
            let mut req = header.as_bytes().to_vec();
            req.extend_from_slice(body);
            let raw = block_on(server.handle(&req)).unwrap().unwrap();
            let (out, payload) = OutHeader::read_from_prefix(&raw).unwrap();
            (out.error, payload.to_vec())
        };

        let (err, lookup_body) = send_raw(1, opcode::LOOKUP, ROOT_ID, b"c\0");
        assert_eq!(err, 0);
        let cell_root = EntryOut::read_from_prefix(&lookup_body).unwrap().0.nodeid;

        let (err, open_body) = send_raw(2, opcode::OPENDIR, cell_root, &[]);
        assert_eq!(err, 0);
        let fh = OpenOut::read_from_prefix(&open_body).unwrap().0.fh;

        let parse_plus = |payload: &[u8]| -> Vec<(u64, String, u64)> {
            let mut out = Vec::new();
            let mut rest = payload;
            while !rest.is_empty() {
                let (eo, after_eo) = EntryOut::read_from_prefix(rest).unwrap();
                let (d, after_d) = Dirent::read_from_prefix(after_eo).unwrap();
                let name = String::from_utf8(after_d[..d.namelen as usize].to_vec()).unwrap();
                let padded = (size_of::<Dirent>() + d.namelen as usize).next_multiple_of(8)
                    - size_of::<Dirent>();
                rest = &after_d[padded..];
                out.push((eo.nodeid, name, d.off));
            }
            out
        };

        // Reply 1 (4096 bytes) fetches ~55 entries into `cursor.buffered` and emits ~22, leaving
        // ~33 buffered before `zzz-overflow`.
        let (err1, p1) = send_raw(
            3,
            opcode::READDIRPLUS,
            cell_root,
            ReadIn { fh, offset: 0, size: 4096, ..Default::default() }.as_bytes(),
        );
        assert_eq!(err1, 0);
        let entries1 = parse_plus(&p1);
        let off1 = entries1.last().unwrap().2;

        // Reply 2 (8192 bytes) drains the buffered entries (taking lookups for each) and then
        // refills mid-reply, hitting `EOVERFLOW` on `zzz-overflow`. It must return Ok with the
        // already-encoded entries so lookups stay balanced.
        let (err2, p2) = send_raw(
            4,
            opcode::READDIRPLUS,
            cell_root,
            ReadIn { fh, offset: off1, size: 8192, ..Default::default() }.as_bytes(),
        );
        assert_eq!(err2, 0, "mid-reply refill error must return already-taken READDIRPLUS entries");
        let entries2 = parse_plus(&p2);
        assert!(!entries2.is_empty());
        let off2 = entries2.last().unwrap().2;

        // Total lookups held on child nodes of `cell_root` must equal the exact number of
        // non-dot `EntryOut`s delivered to the kernel across replies 1 and 2.
        let delivered_lookups: u64 = entries1
            .iter()
            .chain(&entries2)
            .filter(|(_, name, _)| name != "." && name != "..")
            .count() as u64;
        let recorded_lookups: u64 = {
            let nodes = server.nodes.lock().unwrap();
            nodes
                .looked
                .iter()
                .filter(|&(&nid, _)| nid != cell_root)
                .map(|(_, looked)| looked.lookups)
                .sum()
        };
        assert_eq!(
            recorded_lookups, delivered_lookups,
            "READDIRPLUS lookups in Nodes::looked must match delivered EntryOuts"
        );

        // Reply 3 starts with an empty reply, hits the refill EOVERFLOW, and surfaces it without
        // taking any new lookups.
        let (err3, p3) = send_raw(
            5,
            opcode::READDIRPLUS,
            cell_root,
            ReadIn { fh, offset: off2, size: 4096, ..Default::default() }.as_bytes(),
        );
        assert_eq!(err3, -Errno::EOVERFLOW.raw());
        assert!(p3.is_empty());
        let recorded_after: u64 = {
            let nodes = server.nodes.lock().unwrap();
            nodes
                .looked
                .iter()
                .filter(|&(&nid, _)| nid != cell_root)
                .map(|(_, looked)| looked.lookups)
                .sum()
        };
        assert_eq!(recorded_after, delivered_lookups);

        let (err_rel, _) = send_raw(
            6,
            opcode::RELEASEDIR,
            cell_root,
            ReleaseIn { fh, ..Default::default() }.as_bytes(),
        );
        assert_eq!(err_rel, 0);
        block_on(server.unmount()).unwrap();
    }
}
