// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: each simulated host's disk, in memory, of `std::fs`'s calls (the crate's docs).
//!
//! A disk is a tree of inodes from its root, `/`: files, holding bytes, and directories, holding
//! entries, each a name and an inode. What is durable is a file's bytes as of its last sync, a
//! directory's entries of files as of its last sync, and directories once made, as
//! `local_cell::MemDir` has them, the model the cells' own crash tests are written to; what
//! changed since, files' writes, and directories' entries made, removed or renamed, each in order,
//! are kept beside. A crash ([`Disk::crashed`]) keeps what was
//! durable and any of what changed since, as the seed says, each change kept or not in order, each
//! block of each write whole, not at all, garbage, or torn, its garbage in the bytes it changes
//! alone (powersafe overwrite: [`garbled`]); what no directory's entry names after goes. No symbolic links; paths are absolute, a relative one taken from the root.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// The granularity of a crash: a write survives it in blocks of this size, each whole or not.
const BLOCK: usize = 512;

/// `on`, the disk's bytes of a block a crash cut `written` short in, as garbage: those `written`
/// changes, at random; those it writes as they were, as they were. A crash damages no byte a
/// write leaves as it is, the sector's others included: powersafe overwrite, as SQLite assumes
/// of Linux's disks by default (`SQLITE_IOCAP_POWERSAFE_OVERWRITE`), and cladefs too (board 5890,
/// decision-3): a log's append, its page cache writing back whole pages, writes again the
/// sector holding the end of the last record synced.
fn garbled(on: &mut [u8], written: &[u8], rng: &mut Rng) {
    let mut noise = vec![0; on.len()];
    rng.fill(&mut noise);
    for ((byte, &new), noise) in on.iter_mut().zip(written).zip(noise) {
        if *byte != new {
            *byte = noise;
        }
    }
}

/// The blocks a file gives back ([`punch_hole`](crate::punch_hole)), and counts allocated.
const HOLE: u64 = 4096;

/// The inode of a disk's root.
const ROOT: u64 = 1;

/// The device every simulated disk is, as `stat` says.
const DEVICE: u64 = 0x5155;

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// A simulated host's disk (see the module's docs).
struct Disk {
    fs: Mutex<Fs>,
}

#[derive(Clone, Default)]
struct Fs {
    inodes: BTreeMap<u64, Inode>,
    next: u64,
    /// The inodes locked ([`File::lock_exclusive`]), by the openings that hold them.
    locks: BTreeMap<u64, u64>,
    /// The openings made, numbered.
    opened: u64,
}

#[derive(Clone)]
enum Inode {
    File(FileData),
    Dir(DirData),
}

#[derive(Clone)]
struct FileData {
    data: Vec<u8>,
    durable: Vec<u8>,
    /// What changed since its last sync, in order.
    changes: Vec<FileChange>,
    /// Its blocks of [`HOLE`] bytes given back, written since none.
    holes: BTreeSet<u64>,
    modified: SystemTime,
    /// The entries naming it.
    links: u32,
}

#[derive(Clone)]
enum FileChange {
    Write(usize, Vec<u8>),
    SetLen(usize),
}

#[derive(Clone, Default)]
struct DirData {
    entries: BTreeMap<OsString, u64>,
    durable: BTreeMap<OsString, u64>,
    /// What changed since its last sync, in order.
    changes: Vec<DirChange>,
}

#[derive(Clone)]
enum DirChange {
    /// An entry made, or put over the one there.
    Set(OsString, u64),
    Remove(OsString),
    /// An entry renamed within the directory, at once.
    Rename(OsString, OsString, u64),
}

impl DirChange {
    fn apply(&self, entries: &mut BTreeMap<OsString, u64>) {
        match self {
            DirChange::Set(name, inode) => drop(entries.insert(name.clone(), *inode)),
            DirChange::Remove(name) => drop(entries.remove(name)),
            DirChange::Rename(from, to, inode) => {
                if entries.remove(from).is_some() {
                    entries.insert(to.clone(), *inode);
                }
            }
        }
    }
}

impl FileData {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            durable: Vec::new(),
            changes: Vec::new(),
            holes: BTreeSet::new(),
            modified: SystemTime::now(),
            links: 0,
        }
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        if self.data.len() < at + bytes.len() {
            self.data.resize(at + bytes.len(), 0);
        }
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
        let (first, last) = (at as u64 / HOLE, (at + bytes.len()).div_ceil(HOLE as usize) as u64);
        for block in first..last {
            self.holes.remove(&block);
        }
        self.changes.push(FileChange::Write(at, bytes.to_vec()));
        self.modified = SystemTime::now();
    }

    fn set_len(&mut self, len: usize) {
        self.data.resize(len, 0);
        self.changes.push(FileChange::SetLen(len));
        self.modified = SystemTime::now();
    }

    fn sync(&mut self) {
        self.durable = self.data.clone();
        self.changes.clear();
    }

    fn allocated(&self) -> u64 {
        let blocks = (self.data.len() as u64).div_ceil(HOLE);
        let holes = self.holes.range(..blocks).count() as u64;
        (blocks - holes) * HOLE
    }
}

impl Fs {
    fn new() -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(ROOT, Inode::Dir(DirData::default()));
        Self { inodes, next: ROOT + 1, locks: BTreeMap::new(), opened: 0 }
    }

    fn file(&mut self, inode: u64) -> io::Result<&mut FileData> {
        match self.inodes.get_mut(&inode) {
            Some(Inode::File(file)) => Ok(file),
            Some(Inode::Dir(_)) => Err(errno(libc::EISDIR)),
            None => Err(errno(libc::EBADF)),
        }
    }

    fn dir(&mut self, inode: u64) -> io::Result<&mut DirData> {
        match self.inodes.get_mut(&inode) {
            Some(Inode::Dir(dir)) => Ok(dir),
            Some(Inode::File(_)) => Err(errno(libc::ENOTDIR)),
            None => Err(errno(libc::ENOENT)),
        }
    }

    /// The inode at `path`.
    fn lookup(&self, path: &Path) -> io::Result<u64> {
        let mut at = ROOT;
        for name in components(path)? {
            at = match self.inodes.get(&at) {
                Some(Inode::Dir(dir)) => {
                    *dir.entries.get(&name).ok_or_else(|| errno(libc::ENOENT))?
                }
                _ => return Err(errno(libc::ENOTDIR)),
            };
        }
        Ok(at)
    }

    /// The directory holding `path`'s entry, and its name: the root has none.
    fn parent(&self, path: &Path) -> io::Result<(u64, OsString)> {
        let mut names = components(path)?;
        let name = names.pop().ok_or_else(|| errno(libc::EEXIST))?;
        let parent: PathBuf = std::iter::once(OsString::from("/")).chain(names).collect();
        let dir = self.lookup(&parent)?;
        match self.inodes.get(&dir) {
            Some(Inode::Dir(_)) => Ok((dir, name)),
            _ => Err(errno(libc::ENOTDIR)),
        }
    }

    fn change(&mut self, dir: u64, change: DirChange) -> io::Result<()> {
        let dir = self.dir(dir)?;
        change.apply(&mut dir.entries);
        dir.changes.push(change);
        Ok(())
    }

    fn make(&mut self, inode: Inode) -> u64 {
        let number = self.next;
        self.next += 1;
        self.inodes.insert(number, inode);
        number
    }

    /// Lets the file `inode` go, once no entry names it and no opening holds it.
    fn unlinked(&mut self, inode: u64) {
        if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
            file.links = file.links.saturating_sub(1);
        }
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        if self.dir(parent)?.entries.contains_key(&name) {
            return Err(errno(libc::EEXIST));
        }
        let made = self.make(Inode::Dir(DirData::default()));
        // Durable at once, as the cells' own simulations have directories (`local_cell::MemDir`),
        // and as a journaling file system's next sync of anything makes it.
        let parent = self.dir(parent)?;
        parent.entries.insert(name.clone(), made);
        parent.durable.insert(name, made);
        Ok(())
    }

    fn remove(&mut self, path: &Path, dir: bool) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        let inode = *self.dir(parent)?.entries.get(&name).ok_or_else(|| errno(libc::ENOENT))?;
        match (self.inodes.get(&inode), dir) {
            (Some(Inode::Dir(_)), false) => return Err(errno(libc::EISDIR)),
            (Some(Inode::File(_)), true) => return Err(errno(libc::ENOTDIR)),
            (Some(Inode::Dir(of)), true) if !of.entries.is_empty() => {
                return Err(errno(libc::ENOTEMPTY));
            }
            _ => {}
        }
        self.change(parent, DirChange::Remove(name))?;
        self.unlinked(inode);
        Ok(())
    }

    fn remove_all(&mut self, path: &Path) -> io::Result<()> {
        let inode = self.lookup(path)?;
        if let Some(Inode::Dir(dir)) = self.inodes.get(&inode) {
            let names: Vec<OsString> = dir.entries.keys().cloned().collect();
            for name in names {
                self.remove_all(&path.join(name))?;
            }
            return self.remove(path, true);
        }
        self.remove(path, false)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let (source, name) = self.parent(from)?;
        let inode = *self.dir(source)?.entries.get(&name).ok_or_else(|| errno(libc::ENOENT))?;
        let (target, to_name) = self.parent(to)?;
        let moving_dir = matches!(self.inodes.get(&inode), Some(Inode::Dir(_)));
        if let Some(&there) = self.dir(target)?.entries.get(&to_name) {
            if there == inode {
                return Ok(());
            }
            match (self.inodes.get(&there), moving_dir) {
                (Some(Inode::Dir(_)), false) => return Err(errno(libc::EISDIR)),
                (Some(Inode::File(_)), true) => return Err(errno(libc::ENOTDIR)),
                (Some(Inode::Dir(of)), true) if !of.entries.is_empty() => {
                    return Err(errno(libc::ENOTEMPTY));
                }
                _ => {}
            }
            self.unlinked(there);
        }
        if moving_dir && self.within(to, inode)? {
            return Err(errno(libc::EINVAL));
        }
        if source == target {
            return self.change(source, DirChange::Rename(name, to_name, inode));
        }
        self.change(target, DirChange::Set(to_name, inode))?;
        self.change(source, DirChange::Remove(name))
    }

    /// Whether `path`, or a directory above it, is the directory `inode`.
    fn within(&self, path: &Path, inode: u64) -> io::Result<bool> {
        let mut at = ROOT;
        for name in components(path)? {
            if at == inode {
                return Ok(true);
            }
            match self.inodes.get(&at) {
                Some(Inode::Dir(dir)) => match dir.entries.get(&name) {
                    Some(next) => at = *next,
                    None => return Ok(false),
                },
                _ => return Ok(false),
            }
        }
        Ok(at == inode)
    }

    /// The disk as a crash leaves it, as `rng` draws it (the module's docs).
    fn crashed(&self, rng: &mut Rng) -> Fs {
        let mut inodes = BTreeMap::new();
        // Each directory's entries as a crash leaves them, then what they reach from the root.
        let mut entries: BTreeMap<u64, BTreeMap<OsString, u64>> = BTreeMap::new();
        for (&number, inode) in &self.inodes {
            if let Inode::Dir(dir) = inode {
                let mut kept = dir.durable.clone();
                for change in &dir.changes {
                    if rng.flip() {
                        change.apply(&mut kept);
                    }
                }
                entries.insert(number, kept);
            }
        }
        let mut reached = vec![ROOT];
        let mut links: BTreeMap<u64, u32> = BTreeMap::new();
        while let Some(number) = reached.pop() {
            if inodes.contains_key(&number) {
                continue;
            }
            match self.inodes.get(&number) {
                Some(Inode::Dir(_)) => {
                    let kept = entries.remove(&number).unwrap_or_default();
                    for &child in kept.values() {
                        *links.entry(child).or_default() += 1;
                        reached.push(child);
                    }
                    let dir = DirData { durable: kept.clone(), entries: kept, changes: Vec::new() };
                    inodes.insert(number, Inode::Dir(dir));
                }
                Some(Inode::File(file)) => {
                    let mut data = file.durable.clone();
                    for change in &file.changes {
                        match change {
                            FileChange::SetLen(len) => {
                                if rng.flip() {
                                    data.resize(*len, 0);
                                }
                            }
                            FileChange::Write(at, bytes) => {
                                for (index, block) in bytes.chunks(BLOCK).enumerate() {
                                    let start = at + index * BLOCK;
                                    let fate = rng.below(4);
                                    if fate == 0 {
                                        continue;
                                    }
                                    if data.len() < start + block.len() {
                                        data.resize(start + block.len(), 0);
                                    }
                                    let into = &mut data[start..start + block.len()];
                                    match fate {
                                        1 => into.copy_from_slice(block),
                                        2 => garbled(into, block, rng),
                                        _ => {
                                            let half = block.len() / 2;
                                            into[..half].copy_from_slice(&block[..half]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let mut crashed = FileData::new();
                    crashed.durable = data.clone();
                    crashed.data = data;
                    crashed.modified = file.modified;
                    inodes.insert(number, Inode::File(crashed));
                }
                None => {}
            }
        }
        for (number, count) in links {
            if let Some(Inode::File(file)) = inodes.get_mut(&number) {
                file.links = count;
            }
        }
        Fs { inodes, next: self.next, locks: BTreeMap::new(), opened: self.opened }
    }
}

/// The names of `path`'s components, from the root, `.` and `..` resolved.
fn components(path: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => drop(names.pop()),
            Component::Normal(name) => names.push(name.to_owned()),
        }
    }
    Ok(names)
}

/// The draws of a crash: SplitMix64, seeded.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn flip(&mut self) -> bool {
        self.next() & 1 == 1
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.next() as u8;
        }
    }
}

/// A simulated host: the simulation's thread, and its node, as `node_local` keys them.
type Host = (std::thread::ThreadId, Option<madsim::task::NodeId>);

/// Every simulated host's disk, which outlives its kills: a host's values are let go of as it is
/// killed (`node_local`), its disk crashed instead, at its first call after.
static DISKS: Mutex<Vec<(Host, Arc<Disk>)>> = Mutex::new(Vec::new());

node_local::node_local! {
    /// Set at a host's first call to its disk since it started: one started again finds its disk
    /// crashed.
    static STARTED: OnceLock<()> = OnceLock::new();
}

/// The current host's disk: outside a simulation's node, its thread's.
fn disk() -> Arc<Disk> {
    let node = madsim::runtime::Handle::try_current().ok().map(|_| madsim::plugin::node());
    let host = (std::thread::current().id(), node);
    let started = STARTED.get().set(()).is_ok();
    let mut disks = DISKS.lock().unwrap();
    let found = disks.iter().position(|(of, _)| *of == host);
    match found {
        Some(at) if started => {
            let seed = match node {
                Some(_) => madsim::rand::random::<u64>(),
                None => 0,
            };
            let crashed = disks[at].1.fs.lock().unwrap().crashed(&mut Rng(seed));
            disks[at].1 = Arc::new(Disk { fs: Mutex::new(crashed) });
            disks[at].1.clone()
        }
        Some(at) => disks[at].1.clone(),
        None => {
            let disk = Arc::new(Disk { fs: Mutex::new(Fs::new()) });
            disks.push((host, disk.clone()));
            disk
        }
    }
}

/// `work` on the current host's file system.
fn with<T>(work: impl FnOnce(&mut Fs) -> io::Result<T>) -> io::Result<T> {
    let disk = disk();
    let mut fs = disk.fs.lock().unwrap();
    work(&mut fs)
}

/// cladefs: calls that change a simulated host's disk failing as the seed says, before they change
/// anything, as a disk's errors fail them (board 5654): so that the daemon's protocols of marker
/// files, renames and syncs are simulated failing partway with their host running on, as they are
/// crashing.
pub mod faults {
    /// How often a call that changes a host's disk fails: `fail` of them, as the seed draws it,
    /// with `EIO`, or, as often, a call adding data with `ENOSPC`. A call that only reads, or
    /// looks, never does.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Odds {
        pub fail: f64,
    }

    impl Odds {
        /// None: every call as the disk has it.
        pub const NONE: Self = Self { fail: 0.0 };
    }

    /// Has the calls of every host's disk of this simulation fail from now on as `odds` says:
    /// [`Odds::NONE`] heals them.
    pub fn set(odds: Odds) {
        let simulation = std::thread::current().id();
        let mut all = super::ODDS.lock().unwrap();
        all.retain(|(of, _)| *of != simulation);
        if odds != Odds::NONE {
            all.push((simulation, odds));
        }
    }
}

/// The odds the disks' calls fail at ([`faults::set`]), of each simulation: by its thread, as madsim
/// runs each on one, so that simulations sharing a process don't share them.
static ODDS: Mutex<Vec<(std::thread::ThreadId, faults::Odds)>> = Mutex::new(Vec::new());

/// Fails `what`, a call changing a simulated host's disk, adding data if `adds`, as the odds say:
/// drawn only while they are above none, so that a seed's run with none draws as before; and
/// only on a host's node, the simulation's own calls never.
fn faulted(what: std::fmt::Arguments<'_>, adds: bool) -> io::Result<()> {
    use madsim::rand::Rng as _;
    let simulation = std::thread::current().id();
    let odds = ODDS.lock().unwrap().iter().find(|(of, _)| *of == simulation).map(|(_, odds)| *odds);
    let Some(odds) = odds.filter(|odds| odds.fail > 0.0) else { return Ok(()) };
    if madsim::runtime::Handle::try_current().is_err() {
        return Ok(());
    }
    let mut rng = madsim::rand::thread_rng();
    if !rng.gen_bool(odds.fail.min(1.0)) {
        return Ok(());
    }
    let code = match adds && rng.gen_bool(0.5) {
        true => libc::ENOSPC,
        false => libc::EIO,
    };
    let error = errno(code);
    log::warn!("host_fs: {what}, failed as the seed says: {error}");
    Err(error)
}

/// What a file of a disk is: a file, or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileType {
    dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }

    pub fn is_file(&self) -> bool {
        !self.dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

/// What `stat` says of a file of a disk.
#[derive(Clone, Debug)]
pub struct Metadata {
    kind: FileType,
    len: u64,
    inode: u64,
    allocated: u64,
    modified: SystemTime,
}

impl Metadata {
    fn of(number: u64, inode: &Inode) -> Self {
        match inode {
            Inode::File(file) => Self {
                kind: FileType { dir: false },
                len: file.data.len() as u64,
                inode: number,
                allocated: file.allocated(),
                modified: file.modified,
            },
            Inode::Dir(dir) => Self {
                kind: FileType { dir: true },
                len: dir.entries.len() as u64,
                inode: number,
                allocated: HOLE,
                modified: SystemTime::UNIX_EPOCH,
            },
        }
    }

    pub fn file_type(&self) -> FileType {
        self.kind
    }

    pub fn is_dir(&self) -> bool {
        self.kind.dir
    }

    pub fn is_file(&self) -> bool {
        !self.kind.dir
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
}

/// An entry of a directory of a disk ([`read_dir`]).
#[derive(Debug)]
pub struct DirEntry {
    path: PathBuf,
    name: OsString,
    metadata: Metadata,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.metadata.kind)
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(self.metadata.clone())
    }
}

/// The entries of a directory of a disk, as they were when it was read.
pub struct ReadDir(std::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        self.0.next().map(Ok)
    }
}

/// How a file of a disk is opened, as `std::fs::OpenOptions` says.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self.write |= append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        let disk = disk();
        let mut fs = disk.fs.lock().unwrap();
        let inode = match fs.lookup(path) {
            Ok(_) if self.create_new => return Err(errno(libc::EEXIST)),
            Ok(inode) => {
                if self.write && matches!(fs.inodes.get(&inode), Some(Inode::Dir(_))) {
                    return Err(errno(libc::EISDIR));
                }
                if self.truncate
                    && let Ok(file) = fs.file(inode)
                    && !file.data.is_empty()
                {
                    faulted(format_args!("truncating {}", path.display()), false)?;
                    file.set_len(0);
                }
                inode
            }
            Err(err)
                if err.raw_os_error() == Some(libc::ENOENT) && (self.create || self.create_new) =>
            {
                let (parent, name) = fs.parent(path)?;
                faulted(format_args!("creating {}", path.display()), true)?;
                let mut file = FileData::new();
                file.links = 1;
                let made = fs.make(Inode::File(file));
                fs.change(parent, DirChange::Set(name, made))?;
                made
            }
            Err(err) => return Err(err),
        };
        fs.opened += 1;
        let opening = fs.opened;
        drop(fs);
        let position = Arc::new(Mutex::new(0));
        Ok(File { disk, inode, opening, append: self.append, position })
    }
}

/// An open file of a disk, or directory, as `std::fs::File` is.
#[derive(Debug)]
pub struct File {
    disk: Arc<Disk>,
    inode: u64,
    /// Its opening's number, which a lock it takes is held by.
    opening: u64,
    append: bool,
    /// Its offset, which its clones share, as a descriptor's `dup`s do.
    position: Arc<Mutex<u64>>,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Disk")
    }
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }

    pub fn create_new(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).open(path)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    fn with<T>(&self, work: impl FnOnce(&mut Fs) -> io::Result<T>) -> io::Result<T> {
        let mut fs = self.disk.fs.lock().unwrap();
        work(&mut fs)
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.with(|fs| {
            let file = fs.file(self.inode)?;
            let adds = len as usize > file.data.len();
            faulted(format_args!("setting inode {}'s length to {len}", self.inode), adds)?;
            file.set_len(len as usize);
            Ok(())
        })
    }

    /// Makes its bytes durable, or, of a directory, its entries.
    pub fn sync_all(&self) -> io::Result<()> {
        self.with(|fs| {
            faulted(format_args!("syncing inode {}", self.inode), false)?;
            match fs.inodes.get_mut(&self.inode) {
                Some(Inode::File(file)) => file.sync(),
                Some(Inode::Dir(dir)) => {
                    dir.durable = dir.entries.clone();
                    dir.changes.clear();
                }
                None => return Err(errno(libc::EBADF)),
            }
            Ok(())
        })
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.sync_all()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        self.with(|fs| {
            let inode = fs.inodes.get(&self.inode).ok_or_else(|| errno(libc::EBADF))?;
            Ok(Metadata::of(self.inode, inode))
        })
    }

    pub fn try_clone(&self) -> io::Result<File> {
        Ok(File {
            disk: self.disk.clone(),
            inode: self.inode,
            opening: self.opening,
            append: self.append,
            position: self.position.clone(),
        })
    }

    /// Gives the `len` bytes at `offset` back to its disk, keeping its size: zeros after.
    pub(crate) fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        self.with(|fs| {
            let file = fs.file(self.inode)?;
            let end = (offset + len).min(file.data.len() as u64);
            if offset >= end {
                return Ok(());
            }
            faulted(format_args!("punching inode {} at {offset}", self.inode), false)?;
            let zeros = vec![0; (end - offset) as usize];
            file.write(offset as usize, &zeros);
            for block in offset.div_ceil(HOLE)..end / HOLE {
                file.holes.insert(block);
            }
            Ok(())
        })
    }

    /// The ranges below `size` that hold data: but for its holes, all of it.
    pub(crate) fn data_ranges(&self, size: u64) -> Vec<std::ops::Range<u64>> {
        let holes = self.with(|fs| Ok(fs.file(self.inode)?.holes.clone())).unwrap_or_default();
        let mut ranges: Vec<std::ops::Range<u64>> = Vec::new();
        for block in 0..size.div_ceil(HOLE) {
            if holes.contains(&block) {
                continue;
            }
            let (start, end) = (block * HOLE, ((block + 1) * HOLE).min(size));
            match ranges.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => ranges.push(start..end),
            }
        }
        ranges
    }

    /// Locks it for its opening, its clones', as `flock` does: `WouldBlock` if another opening
    /// holds it.
    pub(crate) fn lock_exclusive(&self) -> io::Result<()> {
        self.with(|fs| match fs.locks.get(&self.inode) {
            Some(holder) if *holder != self.opening => Err(errno(libc::EWOULDBLOCK)),
            _ => {
                fs.locks.insert(self.inode, self.opening);
                Ok(())
            }
        })
    }

    pub(crate) fn unlock(&self) {
        let _ = self.with(|fs| {
            if fs.locks.get(&self.inode) == Some(&self.opening) {
                fs.locks.remove(&self.inode);
            }
            Ok(())
        });
    }
}

impl Read for &File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut position = self.position.lock().unwrap();
        let read = self.read_at(buf, *position)?;
        *position += read as u64;
        Ok(read)
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for &File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut position = self.position.lock().unwrap();
        if self.append {
            *position = self.metadata()?.len();
        }
        let written = self.write_at(buf, *position)?;
        *position += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for &File {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let mut position = self.position.lock().unwrap();
        let len = self.metadata()?.len();
        let at = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(by) => len.checked_add_signed(by),
            SeekFrom::Current(by) => position.checked_add_signed(by),
        };
        *position = at.ok_or_else(|| errno(libc::EINVAL))?;
        Ok(*position)
    }
}

impl Seek for File {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        (&*self).seek(to)
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    with(|fs| {
        let number = fs.lookup(path.as_ref())?;
        Ok(Metadata::of(number, &fs.inodes[&number]))
    })
}

/// As [`metadata`]: a disk in memory has no symbolic links.
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    metadata(path)
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("making the directory {}", path.display()), true)?;
        fs.create_dir(path)
    })
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    with(|fs| {
        let mut at = PathBuf::from("/");
        for name in components(path.as_ref())? {
            at.push(name);
            match fs.lookup(&at) {
                Ok(inode) if matches!(fs.inodes.get(&inode), Some(Inode::Dir(_))) => {}
                Ok(_) => return Err(errno(libc::EEXIST)),
                Err(_) => {
                    faulted(format_args!("making the directory {}", at.display()), true)?;
                    fs.create_dir(&at)?
                }
            }
        }
        Ok(())
    })
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing {}", path.display()), false)?;
        fs.remove(path, false)
    })
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing the directory {}", path.display()), false)?;
        fs.remove(path, true)
    })
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing {} and all in it", path.display()), false)?;
        fs.remove_all(path)
    })
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    with(|fs| {
        faulted(format_args!("renaming {} to {}", from.display(), to.display()), false)?;
        fs.rename(from, to)
    })
}

pub fn hard_link(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    with(|fs| {
        faulted(
            format_args!("linking {} at {}", from.as_ref().display(), to.as_ref().display()),
            true,
        )?;
        let inode = fs.lookup(from.as_ref())?;
        fs.file(inode)?;
        let (parent, name) = fs.parent(to.as_ref())?;
        if fs.dir(parent)?.entries.contains_key(&name) {
            return Err(errno(libc::EEXIST));
        }
        fs.file(inode)?.links += 1;
        fs.change(parent, DirChange::Set(name, inode))
    })
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    with(|fs| {
        let inode = fs.lookup(path.as_ref())?;
        Ok(fs.file(inode)?.data.clone())
    })
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Writes `contents` to the file at `path`, made or cut first, as `std::fs::write` does: synced
/// not.
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let file = File::create(path)?;
    file.write_all_at(contents.as_ref(), 0)
}

pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let contents = read(from)?;
    write(to, &contents)?;
    Ok(contents.len() as u64)
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref();
    with(|fs| {
        let inode = fs.lookup(path)?;
        let entries = fs.dir(inode)?.entries.clone();
        let read = entries
            .into_iter()
            .map(|(name, number)| DirEntry {
                path: path.join(&name),
                metadata: Metadata::of(number, &fs.inodes[&number]),
                name,
            })
            .collect::<Vec<_>>();
        Ok(ReadDir(read.into_iter()))
    })
}

/// `path`, absolute, `.` and `..` resolved, if something is there.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    with(|fs| fs.lookup(path).map(drop))?;
    Ok(std::iter::once(OsStr::new("/").to_owned()).chain(components(path)?).collect())
}

/// `std::os::unix::fs::FileExt`'s calls, of a disk's files.
pub trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

impl FileExt for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.with(|fs| {
            let file = fs.file(self.inode)?;
            let at = (offset as usize).min(file.data.len());
            let read = buf.len().min(file.data.len() - at);
            buf[..read].copy_from_slice(&file.data[at..at + read]);
            Ok(read)
        })
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        match self.read_at(buf, offset)? {
            read if read == buf.len() => Ok(()),
            _ => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.with(|fs| {
            let len = buf.len();
            faulted(format_args!("writing {len} bytes of inode {} at {offset}", self.inode), true)?;
            fs.file(self.inode)?.write(offset as usize, buf);
            Ok(buf.len())
        })
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.write_at(buf, offset).map(drop)
    }
}

/// `std::os::unix::fs::MetadataExt`'s calls, of a disk's files: those cladefsd makes.
pub trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    /// Of 512 bytes.
    fn blocks(&self) -> u64;
}

impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        DEVICE
    }

    fn ino(&self) -> u64 {
        self.inode
    }

    fn blocks(&self) -> u64 {
        self.allocated / 512
    }
}

/// `std::os::unix::fs::OpenOptionsExt`'s calls: a disk in memory takes no flags, and keeps no
/// modes.
pub trait OpenOptionsExt {
    fn custom_flags(&mut self, flags: i32) -> &mut Self;
    fn mode(&mut self, mode: u32) -> &mut Self;
}

impl OpenOptionsExt for OpenOptions {
    fn custom_flags(&mut self, _: i32) -> &mut Self {
        self
    }

    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}
