// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: each simulated host's disk, in memory, of `std::fs`'s calls (the crate's docs).
//!
//! A disk is a file system in memory ([`crate::mem`], the crash model the cells' own crash tests'
//! directories are too), from its root, `/`, a relative path taken from it: a host started again,
//! killed before, finds it as a crash left it ([`mem::Fs::crashed`]), corrupting what was written
//! since its files' last syncs, its locks of files gone. Its calls fail as the simulation's faults
//! say, besides ([`faults`]).

use crate::blocking::assert_may_block;
use crate::mem::{self, components, errno};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The device every simulated disk is, as `stat` says.
const DEVICE: u64 = 0x5155;

/// A simulated host's disk (see the module's docs): its file system, and its locks of files.
struct Disk {
    fs: Mutex<mem::Fs>,
    locks: Mutex<Locks>,
}

struct Locks {
    /// The inodes locked ([`File::lock_exclusive`]), by the openings that hold them.
    held: BTreeMap<u64, u64>,
    /// The openings made, numbered.
    opened: u64,
}

impl Disk {
    fn new(fs: mem::Fs, opened: u64) -> Arc<Self> {
        let locks = Mutex::new(Locks { held: BTreeMap::new(), opened });
        Arc::new(Self { fs: Mutex::new(fs), locks })
    }
}

/// A simulated host: the simulation's thread, and its node, as `node_local` keys them.
type Host = (std::thread::ThreadId, Option<madsim::task::NodeId>);

/// Every simulated host's disk, which outlives its kills: paired with the node generation
/// (`node_local::generation()`) at which it was last used, so a host started again after a kill
/// or restart finds its disk crashed at its first call after.
static DISKS: Mutex<Vec<(Host, u64, Arc<Disk>)>> = Mutex::new(Vec::new());

/// The current host's disk: outside a simulation's node, its thread's.
fn disk() -> Arc<Disk> {
    let node = madsim::runtime::Handle::try_current().ok().map(|_| madsim::plugin::node());
    let host = (std::thread::current().id(), node);
    let generation = node_local::generation();
    let mut disks = DISKS.lock().unwrap();
    let found = disks.iter().position(|(of, _, _)| *of == host);
    match found {
        Some(at) if disks[at].1 != generation => {
            let seed = match node {
                Some(_) => madsim::rand::random::<u64>(),
                None => 0,
            };
            let crashed = disks[at].2.fs.lock().unwrap().crashed(seed, true);
            let opened = disks[at].2.locks.lock().unwrap().opened;
            disks[at].1 = generation;
            disks[at].2 = Disk::new(crashed, opened);
            disks[at].2.clone()
        }
        Some(at) => disks[at].2.clone(),
        None => {
            let disk = Disk::new(mem::Fs::new(), 0);
            disks.push((host, generation, disk.clone()));
            disk
        }
    }
}

/// cladefs: loses simulated `node`'s disk, as a host replaced is, its disk wiped or swapped (board
/// 14495): its next start finds an empty disk, nothing of what it held, synced or not. Call it once
/// the node is killed, before it starts again.
pub fn lose(node: madsim::task::NodeId) {
    let host = (std::thread::current().id(), Some(node));
    DISKS.lock().unwrap().retain(|(of, _, _)| *of != host);
}

/// `work` on the current host's file system.
fn with<T>(work: impl FnOnce(&mut mem::Fs) -> io::Result<T>) -> io::Result<T> {
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
    /// with `EIO`, or, as often, a call adding data with `ENOSPC`; cladefs: and how often a read of
    /// a file's data does, `reads` of them, with `EIO`, as a drive's unreadable sector fails it
    /// (board 5748). A call that only looks never does.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Odds {
        pub fail: f64,
        pub reads: f64,
    }

    impl Odds {
        /// None: every call as the disk has it.
        pub const NONE: Self = Self { fail: 0.0, reads: 0.0 };
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
    let Some(fail) = drawn(|odds| odds.fail) else { return Ok(()) };
    let mut rng = madsim::rand::thread_rng();
    if !rng.gen_bool(fail.min(1.0)) {
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

/// cladefs: fails `what`, a read of a file's data on a simulated host's disk, as the odds of reads
/// failing say, with `EIO` (board 5748), drawn as [`faulted`] draws.
fn read_faulted(what: std::fmt::Arguments<'_>) -> io::Result<()> {
    use madsim::rand::Rng as _;
    let Some(reads) = drawn(|odds| odds.reads) else { return Ok(()) };
    if !madsim::rand::thread_rng().gen_bool(reads.min(1.0)) {
        return Ok(());
    }
    let error = errno(libc::EIO);
    log::warn!("host_fs: {what}, failed as the seed says: {error}");
    Err(error)
}

/// The odds `of` the simulation's faults say, if above none and on a host's node: the
/// simulation's own calls never fail.
fn drawn(of: impl Fn(&faults::Odds) -> f64) -> Option<f64> {
    let simulation = std::thread::current().id();
    let odds = ODDS.lock().unwrap().iter().find(|(at, _)| *at == simulation).map(|(_, odds)| *odds);
    let odds = odds.map(|odds| of(&odds)).filter(|odds| *odds > 0.0)?;
    madsim::runtime::Handle::try_current().is_ok().then_some(odds)
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
    fn of(stat: mem::Stat) -> Self {
        let mem::Stat { inode, dir, len, allocated, modified } = stat;
        Self { kind: FileType { dir }, len, inode, allocated, modified }
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

    #[track_caller]
    pub fn file_type(&self) -> io::Result<FileType> {
        assert_may_block("DirEntry::file_type");
        Ok(self.metadata.kind)
    }

    #[track_caller]
    pub fn metadata(&self) -> io::Result<Metadata> {
        assert_may_block("DirEntry::metadata");
        Ok(self.metadata.clone())
    }
}

/// The entries of a directory of a disk, as they were when it was read.
pub struct ReadDir(std::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    #[track_caller]
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        assert_may_block("ReadDir::next");
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

    #[track_caller]
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        assert_may_block("OpenOptions::open");
        let path = path.as_ref();
        let disk = disk();
        let mut fs = disk.fs.lock().unwrap();
        let inode = match fs.lookup(path) {
            Ok(_) if self.create_new => return Err(errno(libc::EEXIST)),
            Ok(inode) => {
                let stat = fs.stat(inode)?;
                if self.write && stat.dir {
                    return Err(errno(libc::EISDIR));
                }
                if self.truncate && !stat.dir && stat.len > 0 {
                    faulted(format_args!("truncating {}", path.display()), false)?;
                    fs.set_len(inode, 0)?;
                }
                inode
            }
            Err(err)
                if err.raw_os_error() == Some(libc::ENOENT) && (self.create || self.create_new) =>
            {
                fs.parent(path)?;
                faulted(format_args!("creating {}", path.display()), true)?;
                fs.create(path)?
            }
            Err(err) => return Err(err),
        };
        drop(fs);
        let opening = {
            let mut locks = disk.locks.lock().unwrap();
            locks.opened += 1;
            locks.opened
        };
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
    #[track_caller]
    pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
        assert_may_block("File::open");
        OpenOptions::new().read(true).open(path)
    }

    #[track_caller]
    pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
        assert_may_block("File::create");
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }

    #[track_caller]
    pub fn create_new(path: impl AsRef<Path>) -> io::Result<File> {
        assert_may_block("File::create_new");
        OpenOptions::new().read(true).write(true).create_new(true).open(path)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    fn with<T>(&self, work: impl FnOnce(&mut mem::Fs) -> io::Result<T>) -> io::Result<T> {
        let mut fs = self.disk.fs.lock().unwrap();
        work(&mut fs)
    }

    #[track_caller]
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        assert_may_block("File::set_len");
        self.with(|fs| {
            let adds = len > fs.data(self.inode)?.len() as u64;
            faulted(format_args!("setting inode {}'s length to {len}", self.inode), adds)?;
            fs.set_len(self.inode, len)
        })
    }

    /// Makes its bytes durable, or, of a directory, its entries.
    #[track_caller]
    pub fn sync_all(&self) -> io::Result<()> {
        assert_may_block("File::sync_all");
        self.with(|fs| {
            faulted(format_args!("syncing inode {}", self.inode), false)?;
            fs.sync(self.inode)
        })
    }

    #[track_caller]
    pub fn sync_data(&self) -> io::Result<()> {
        assert_may_block("File::sync_data");
        self.sync_all()
    }

    #[track_caller]
    pub fn metadata(&self) -> io::Result<Metadata> {
        assert_may_block("File::metadata");
        self.with(|fs| Ok(Metadata::of(fs.stat(self.inode)?)))
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
            // Nothing to give back past its end: no call to fail.
            if offset >= fs.data(self.inode)?.len() as u64 {
                return Ok(());
            }
            faulted(format_args!("punching inode {} at {offset}", self.inode), false)?;
            fs.punch(self.inode, offset, len)
        })
    }

    /// The ranges below `size` that hold data: but for its holes, all of it.
    pub(crate) fn data_ranges(&self, size: u64) -> io::Result<Vec<std::ops::Range<u64>>> {
        self.with(|fs| fs.data_ranges(self.inode, size))
    }

    /// Locks it for its opening, its clones', as `flock` does: `WouldBlock` if another opening
    /// holds it.
    pub(crate) fn lock_exclusive(&self) -> io::Result<()> {
        let mut locks = self.disk.locks.lock().unwrap();
        match locks.held.get(&self.inode) {
            Some(holder) if *holder != self.opening => Err(errno(libc::EWOULDBLOCK)),
            _ => {
                locks.held.insert(self.inode, self.opening);
                Ok(())
            }
        }
    }

    pub(crate) fn unlock(&self) {
        let mut locks = self.disk.locks.lock().unwrap();
        if locks.held.get(&self.inode) == Some(&self.opening) {
            locks.held.remove(&self.inode);
        }
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

#[track_caller]
pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    assert_may_block("metadata");
    with(|fs| Ok(Metadata::of(fs.stat(fs.lookup(path.as_ref())?)?)))
}

/// As [`metadata`]: a disk in memory has no symbolic links.
#[track_caller]
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    assert_may_block("symlink_metadata");
    metadata(path)
}

#[track_caller]
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("create_dir");
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("making the directory {}", path.display()), true)?;
        fs.create_dir(path)
    })
}

#[track_caller]
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("create_dir_all");
    with(|fs| {
        let mut at = PathBuf::from("/");
        for name in components(path.as_ref()) {
            at.push(name);
            match fs.lookup(&at) {
                Ok(inode) if fs.stat(inode)?.dir => {}
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

#[track_caller]
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_file");
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing {}", path.display()), false)?;
        fs.remove(path, false)
    })
}

#[track_caller]
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_dir");
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing the directory {}", path.display()), false)?;
        fs.remove(path, true)
    })
}

#[track_caller]
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_dir_all");
    let path = path.as_ref();
    with(|fs| {
        faulted(format_args!("removing {} and all in it", path.display()), false)?;
        fs.remove_all(path)
    })
}

#[track_caller]
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("rename");
    let (from, to) = (from.as_ref(), to.as_ref());
    with(|fs| {
        faulted(format_args!("renaming {} to {}", from.display(), to.display()), false)?;
        fs.rename(from, to)
    })
}

#[track_caller]
pub fn hard_link(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("hard_link");
    with(|fs| {
        faulted(
            format_args!("linking {} at {}", from.as_ref().display(), to.as_ref().display()),
            true,
        )?;
        fs.link(from.as_ref(), to.as_ref())
    })
}

#[track_caller]
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    assert_may_block("read");
    with(|fs| {
        read_faulted(format_args!("reading {}", path.as_ref().display()))?;
        Ok(fs.data(fs.lookup(path.as_ref())?)?.to_vec())
    })
}

#[track_caller]
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    assert_may_block("read_to_string");
    String::from_utf8(read(path)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Writes `contents` to the file at `path`, made or cut first, as `std::fs::write` does: synced
/// not.
#[track_caller]
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    assert_may_block("write");
    let file = File::create(path)?;
    file.write_all_at(contents.as_ref(), 0)
}

#[track_caller]
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    assert_may_block("copy");
    let contents = read(from)?;
    write(to, &contents)?;
    Ok(contents.len() as u64)
}

#[track_caller]
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    assert_may_block("read_dir");
    let path = path.as_ref();
    with(|fs| {
        let read = (fs.list(path)?.into_iter())
            .map(|(name, stat)| DirEntry {
                path: path.join(&name),
                metadata: Metadata::of(stat),
                name,
            })
            .collect::<Vec<_>>();
        Ok(ReadDir(read.into_iter()))
    })
}

/// `path`, absolute, `.` and `..` resolved, if something is there.
#[track_caller]
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    assert_may_block("canonicalize");
    let path = path.as_ref();
    with(|fs| fs.lookup(path).map(drop))?;
    Ok(std::iter::once(OsStr::new("/").to_owned()).chain(components(path)).collect())
}

/// `std::os::unix::fs::FileExt`'s calls, of a disk's files.
pub trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

impl FileExt for File {
    #[track_caller]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        assert_may_block("FileExt::read_at");
        self.with(|fs| {
            let len = buf.len();
            read_faulted(format_args!("reading {len} bytes of inode {} at {offset}", self.inode))?;
            let data = fs.data(self.inode)?;
            let at = (offset as usize).min(data.len());
            let read = buf.len().min(data.len() - at);
            buf[..read].copy_from_slice(&data[at..at + read]);
            Ok(read)
        })
    }

    #[track_caller]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        assert_may_block("FileExt::read_exact_at");
        match self.read_at(buf, offset)? {
            read if read == buf.len() => Ok(()),
            _ => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }

    #[track_caller]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        assert_may_block("FileExt::write_at");
        self.with(|fs| {
            let len = buf.len();
            faulted(format_args!("writing {len} bytes of inode {} at {offset}", self.inode), true)?;
            fs.write(self.inode, offset, buf)?;
            Ok(buf.len())
        })
    }

    #[track_caller]
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        assert_may_block("FileExt::write_all_at");
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
