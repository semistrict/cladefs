// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a file system in memory that a crash leaves as a power loss would: cladefs's one crash
//! model of a host's disk, which each simulated host's disk is (`sim`, under madsim), and the
//! directories of the cells' own crash tests (`local_cell::MemDisk`), so that the two crash alike
//! (board 8264).
//!
//! It is a tree of inodes from its root: files, holding bytes, and directories, holding entries,
//! each a name and an inode. What is durable is a file's bytes as of its last sync, a directory's
//! entries as of its last sync, and a directory once made and anything synced since, as a sync of
//! anything commits a journaling file system's journal, as ext4 and XFS have it (board 5748). What
//! changed since, files' writes and lengths set, and directories' entries made, removed or renamed,
//! each in order, is kept beside. A crash ([`Fs::crashed`]) keeps what was durable and any of what
//! changed since, as its seed says, each change kept or not in order, each sector of each write
//! whole, not at all, and, corrupting, garbage, or torn, its garbage in the bytes it changes alone
//! ([`crate::crash`]); what no entry reaches from the root after goes.
//!
//! It checks what a POSIX file system checks, failing as it does (`ENOENT`, `EEXIST`, `ENOTDIR`,
//! `EISDIR`, `ENOTEMPTY`, `EINVAL`), and no more: a rename goes over a file there, as `rename(2)`
//! does. What fails besides, a disk's faults, a crash interrupting a run of operations, a disk
//! full, its users add. No symbolic links; a path's names are taken from the root, `.` and `..`
//! resolved: `a/b` is `/a/b`, and the empty path the root.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// The blocks a file gives back ([`Fs::punch`]), and counts allocated.
const HOLE: u64 = 4096;

/// The inode of the root.
const ROOT: u64 = 1;

pub(crate) fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// A file system in memory (see the module's docs).
#[derive(Clone)]
pub struct Fs {
    inodes: BTreeMap<u64, Inode>,
    next: u64,
    /// The directories made since the last sync of anything: their parents, names and inodes
    /// (board 5748).
    making: Vec<(u64, OsString, u64)>,
}

/// What `stat` says of a file or directory of an [`Fs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub inode: u64,
    pub dir: bool,
    /// A file's bytes; a directory's entries.
    pub len: u64,
    /// The bytes it takes of the disk: a file's, but for its holes ([`Fs::punch`]); a block, a
    /// directory's.
    pub allocated: u64,
    pub modified: SystemTime,
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
    fn new(data: Vec<u8>) -> Self {
        Self {
            durable: data.clone(),
            data,
            changes: Vec::new(),
            holes: BTreeSet::new(),
            modified: SystemTime::now(),
        }
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        if self.data.len() < at + bytes.len() {
            self.data.resize(at + bytes.len(), 0);
        }
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
        let blocks = at as u64 / HOLE..(at + bytes.len()).div_ceil(HOLE as usize) as u64;
        self.holes.retain(|block| !blocks.contains(block));
        self.changes.push(FileChange::Write(at, bytes.to_vec()));
        self.modified = SystemTime::now();
    }

    fn allocated(&self) -> u64 {
        let blocks = (self.data.len() as u64).div_ceil(HOLE);
        let holes = self.holes.range(..blocks).count() as u64;
        (blocks - holes) * HOLE
    }
}

impl Inode {
    fn stat(&self, inode: u64) -> Stat {
        match self {
            Inode::File(file) => Stat {
                inode,
                dir: false,
                len: file.data.len() as u64,
                allocated: file.allocated(),
                modified: file.modified,
            },
            Inode::Dir(dir) => Stat {
                inode,
                dir: true,
                len: dir.entries.len() as u64,
                allocated: HOLE,
                modified: SystemTime::UNIX_EPOCH,
            },
        }
    }
}

impl Default for Fs {
    fn default() -> Self {
        Self::new()
    }
}

impl Fs {
    /// An empty file system: its root, durable.
    pub fn new() -> Self {
        let inodes = BTreeMap::from([(ROOT, Inode::Dir(DirData::default()))]);
        Self { inodes, next: ROOT + 1, making: Vec::new() }
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

    /// The inode `names` lead to from the root.
    fn resolve(&self, names: &[OsString]) -> io::Result<u64> {
        let mut at = ROOT;
        for name in names {
            at = match self.inodes.get(&at) {
                Some(Inode::Dir(dir)) => {
                    *dir.entries.get(name).ok_or_else(|| errno(libc::ENOENT))?
                }
                _ => return Err(errno(libc::ENOTDIR)),
            };
        }
        Ok(at)
    }

    /// The inode at `path`.
    pub fn lookup(&self, path: &Path) -> io::Result<u64> {
        self.resolve(&components(path))
    }

    /// The directory holding `path`'s entry, and its name: the root has none.
    pub fn parent(&self, path: &Path) -> io::Result<(u64, OsString)> {
        let mut names = components(path);
        let name = names.pop().ok_or_else(|| errno(libc::EEXIST))?;
        let dir = self.resolve(&names)?;
        match self.inodes.get(&dir) {
            Some(Inode::Dir(_)) => Ok((dir, name)),
            _ => Err(errno(libc::ENOTDIR)),
        }
    }

    /// What `stat` says of `inode`.
    pub fn stat(&self, inode: u64) -> io::Result<Stat> {
        Ok(self.inodes.get(&inode).ok_or_else(|| errno(libc::EBADF))?.stat(inode))
    }

    /// The bytes the file `inode` holds now, durable or not.
    pub fn data(&self, inode: u64) -> io::Result<&[u8]> {
        match self.inodes.get(&inode) {
            Some(Inode::File(file)) => Ok(&file.data),
            Some(Inode::Dir(_)) => Err(errno(libc::EISDIR)),
            None => Err(errno(libc::EBADF)),
        }
    }

    /// The entries of the directory at `path`, in order of their names.
    pub fn list(&self, path: &Path) -> io::Result<Vec<(OsString, Stat)>> {
        match self.inodes.get(&self.lookup(path)?) {
            Some(Inode::Dir(dir)) => Ok((dir.entries.iter())
                .map(|(name, &inode)| (name.clone(), self.inodes[&inode].stat(inode)))
                .collect()),
            _ => Err(errno(libc::ENOTDIR)),
        }
    }

    /// Every file and directory reachable from the root, but the root, by its path from it: a file
    /// once by each of its names.
    pub fn walk(&self) -> Vec<(PathBuf, Stat)> {
        let mut found = Vec::new();
        let mut dirs = vec![(PathBuf::new(), ROOT)];
        while let Some((path, number)) = dirs.pop() {
            let Some(Inode::Dir(dir)) = self.inodes.get(&number) else { continue };
            for (name, &child) in &dir.entries {
                let (path, stat) = (path.join(name), self.inodes[&child].stat(child));
                if stat.dir {
                    dirs.push((path.clone(), child));
                }
                found.push((path, stat));
            }
        }
        found
    }

    /// What it holds, as a disk counts it: `file` of each file's length, each file once however
    /// many names it has, and `dir` for each directory but the root.
    pub fn used(&self, file: impl Fn(u64) -> u64, dir: u64) -> u64 {
        let (mut seen, mut reached, mut used) = (BTreeSet::new(), vec![ROOT], 0);
        while let Some(number) = reached.pop() {
            if !seen.insert(number) {
                continue;
            }
            match &self.inodes[&number] {
                Inode::File(of) => used += file(of.data.len() as u64),
                Inode::Dir(of) => {
                    used += if number == ROOT { 0 } else { dir };
                    reached.extend(of.entries.values());
                }
            }
        }
        used
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

    /// Makes an empty file at `path`, where nothing is: its entry durable once its directory is
    /// synced.
    pub fn create(&mut self, path: &Path) -> io::Result<u64> {
        let (parent, name) = self.parent(path)?;
        if self.dir(parent)?.entries.contains_key(&name) {
            return Err(errno(libc::EEXIST));
        }
        let made = self.make(Inode::File(FileData::new(Vec::new())));
        self.change(parent, DirChange::Set(name, made))?;
        Ok(made)
    }

    /// Puts at `path`, over any file there, at once, a file of `bytes` made, written and synced
    /// beside it: a crash keeps the one there before or this, until its directory is synced.
    pub fn replace(&mut self, path: &Path, bytes: &[u8]) -> io::Result<u64> {
        let (parent, name) = self.parent(path)?;
        if let Some(&there) = self.dir(parent)?.entries.get(&name)
            && let Some(Inode::Dir(_)) = self.inodes.get(&there)
        {
            return Err(errno(libc::EISDIR));
        }
        let made = self.make(Inode::File(FileData::new(bytes.to_vec())));
        self.change(parent, DirChange::Set(name, made))?;
        Ok(made)
    }

    /// Makes a directory at `path`: an entry of its parent's like a file's, durable at the next
    /// sync of anything (board 5748): a crash before loses it, or keeps it, with all under it.
    pub fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        if self.dir(parent)?.entries.contains_key(&name) {
            return Err(errno(libc::EEXIST));
        }
        let made = self.make(Inode::Dir(DirData::default()));
        self.making.push((parent, name.clone(), made));
        self.change(parent, DirChange::Set(name, made))
    }

    /// Removes the entry at `path`: of a file, or of an empty directory if `dir`.
    pub fn remove(&mut self, path: &Path, dir: bool) -> io::Result<()> {
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
        self.change(parent, DirChange::Remove(name))
    }

    /// Removes the entry at `path` and, a directory, all under it, each entry of each directory
    /// first.
    pub fn remove_all(&mut self, path: &Path) -> io::Result<()> {
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

    /// Renames the entry at `from` to `to`, over what is there, as `rename(2)` does: within a
    /// directory at once, a crash leaving it at one or the other; from one directory to another,
    /// its entry in each durable as that directory is synced, so that a crash may leave it at both,
    /// or at neither.
    pub fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
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
        }
        if moving_dir && self.within(to, inode) {
            return Err(errno(libc::EINVAL));
        }
        if source == target {
            return self.change(source, DirChange::Rename(name, to_name, inode));
        }
        self.change(target, DirChange::Set(to_name, inode))?;
        self.change(source, DirChange::Remove(name))
    }

    /// Whether `path`, or a directory above it, is the directory `inode`.
    fn within(&self, path: &Path, inode: u64) -> bool {
        let mut at = ROOT;
        for name in components(path) {
            if at == inode {
                return true;
            }
            match self.inodes.get(&at) {
                Some(Inode::Dir(dir)) => match dir.entries.get(&name) {
                    Some(next) => at = *next,
                    None => return false,
                },
                _ => return false,
            }
        }
        at == inode
    }

    /// Makes `to`, where nothing is, another name of the file at `from` (a hard link): durable once
    /// `to`'s directory is synced.
    pub fn link(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let inode = self.lookup(from)?;
        self.file(inode)?;
        let (parent, name) = self.parent(to)?;
        if self.dir(parent)?.entries.contains_key(&name) {
            return Err(errno(libc::EEXIST));
        }
        self.change(parent, DirChange::Set(name, inode))
    }

    /// Writes `bytes` at `offset` of the file `inode`, past its end if they go there.
    pub fn write(&mut self, inode: u64, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.file(inode)?.write(offset as usize, bytes);
        Ok(())
    }

    /// Cuts or extends the file `inode` to `len` bytes, extended with zeros.
    pub fn set_len(&mut self, inode: u64, len: u64) -> io::Result<()> {
        let file = self.file(inode)?;
        file.data.resize(len as usize, 0);
        file.changes.push(FileChange::SetLen(len as usize));
        file.modified = SystemTime::now();
        Ok(())
    }

    /// Gives the `len` bytes at `offset` of the file `inode` back to the disk, keeping its size:
    /// written as zeros, as a crash keeps or loses a write, and its whole blocks given back.
    pub fn punch(&mut self, inode: u64, offset: u64, len: u64) -> io::Result<()> {
        let file = self.file(inode)?;
        let end = offset.saturating_add(len).min(file.data.len() as u64);
        if offset >= end {
            return Ok(());
        }
        let (from, to) = (offset as usize, end as usize);
        file.data[from..to].fill(0);
        file.changes.push(FileChange::Write(from, vec![0; to - from]));
        file.holes.extend(offset.div_ceil(HOLE)..end / HOLE);
        file.modified = SystemTime::now();
        Ok(())
    }

    /// The ranges of the file `inode` below `size` that hold data: but for its holes, all of it.
    pub fn data_ranges(&self, inode: u64, size: u64) -> io::Result<Vec<Range<u64>>> {
        let holes = match self.inodes.get(&inode) {
            Some(Inode::File(file)) => &file.holes,
            Some(Inode::Dir(_)) => return Err(errno(libc::EISDIR)),
            None => return Err(errno(libc::EBADF)),
        };
        let mut ranges: Vec<Range<u64>> = Vec::new();
        for block in (0..size.div_ceil(HOLE)).filter(|block| !holes.contains(block)) {
            let (start, end) = (block * HOLE, ((block + 1) * HOLE).min(size));
            match ranges.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => ranges.push(start..end),
            }
        }
        Ok(ranges)
    }

    /// Makes the file `inode`'s bytes durable, or, of a directory, its entries; and commits the
    /// journal, every directory made since durable (board 5748).
    pub fn sync(&mut self, inode: u64) -> io::Result<()> {
        match self.inodes.get_mut(&inode) {
            Some(Inode::File(file)) => {
                file.durable = file.data.clone();
                file.changes.clear();
            }
            Some(Inode::Dir(dir)) => {
                dir.durable = dir.entries.clone();
                dir.changes.clear();
            }
            None => return Err(errno(libc::EBADF)),
        }
        self.journaled();
        Ok(())
    }

    /// A sync's commit of the file system's journal: every directory made since durable in its
    /// parent, if its parent holds it still (board 5748).
    fn journaled(&mut self) {
        for (parent, name, made) in std::mem::take(&mut self.making) {
            if let Some(Inode::Dir(dir)) = self.inodes.get_mut(&parent)
                && dir.entries.get(&name) == Some(&made)
            {
                dir.durable.insert(name, made);
            }
        }
    }

    /// The file system as a crash leaves it, as `seed` draws it (the module's docs): what was
    /// written since its file's last sync also garbage, or torn, if `corrupting`.
    pub fn crashed(&self, seed: u64, corrupting: bool) -> Fs {
        let mut rng = Rng(seed);
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
        while let Some(number) = reached.pop() {
            if inodes.contains_key(&number) {
                continue;
            }
            match self.inodes.get(&number) {
                Some(Inode::Dir(_)) => {
                    let kept = entries.remove(&number).unwrap_or_default();
                    reached.extend(kept.values());
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
                                let draw = &mut |n| rng.below(n);
                                crate::crash::write(&mut data, *at, bytes, corrupting, draw);
                            }
                        }
                    }
                    let crashed = FileData { modified: file.modified, ..FileData::new(data) };
                    inodes.insert(number, Inode::File(crashed));
                }
                None => {}
            }
        }
        Fs { inodes, next: self.next, making: Vec::new() }
    }
}

/// The names of `path`'s components, from the root, `.` and `..` resolved.
pub(crate) fn components(path: &Path) -> Vec<OsString> {
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => drop(names.pop()),
            Component::Normal(name) => names.push(name.to_owned()),
        }
    }
    names
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash::SECTOR as BLOCK;

    fn read(fs: &Fs, path: &str) -> Vec<u8> {
        fs.data(fs.lookup(Path::new(path)).unwrap()).unwrap().to_vec()
    }

    fn names(fs: &Fs, path: &str) -> Vec<String> {
        let listed = fs.list(Path::new(path)).unwrap().into_iter();
        listed.map(|(name, _)| name.into_string().unwrap()).collect()
    }

    #[test]
    fn crashes_keep_what_was_synced() {
        let p = Path::new;
        let mut fs = Fs::new();
        fs.create_dir(p("d")).unwrap();
        fs.sync(ROOT).unwrap();
        let synced = fs.create(p("d/synced")).unwrap();
        fs.write(synced, 0, &[1; BLOCK]).unwrap();
        fs.sync(synced).unwrap();
        fs.sync(fs.lookup(p("d")).unwrap()).unwrap();
        // Neither the file nor its entry is synced.
        let unsynced = fs.create(p("d/unsynced")).unwrap();
        fs.write(unsynced, 0, &[2; BLOCK]).unwrap();
        fs.write(synced, BLOCK as u64, &[3; 4 * BLOCK]).unwrap();
        // cladefs: a directory made, nothing synced since: kept or lost, with all under it (board
        // 5748).
        fs.create_dir(p("e")).unwrap();
        fs.create(p("e/f")).unwrap();

        let mut seen_unsynced = [false; 2];
        let mut seen_e = [false; 2];
        let mut lens = BTreeSet::new();
        for seed in 0..64 {
            let crashed = fs.crashed(seed, false);
            let data = read(&crashed, "d/synced");
            assert_eq!(data[..BLOCK], [1; BLOCK]);
            // Each block written since the sync is there whole, or not at all.
            for block in data[BLOCK..].chunks(BLOCK) {
                assert!(block == [3; BLOCK] || block == [0; BLOCK], "{block:?}");
            }
            lens.insert(data.len());
            seen_unsynced[names(&crashed, "d").len() - 1] = true;
            let listed = names(&crashed, "");
            match listed.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["d"] => seen_e[0] = true,
                ["d", "e"] => seen_e[1] = true,
                _ => panic!("{listed:?}"),
            }
        }
        assert_eq!(seen_unsynced, [true, true]);
        assert_eq!(seen_e, [true, true]);
        // Anything synced after, as a journaling file system's commit makes it, it is kept.
        let later = fs.create(p("d/later")).unwrap();
        fs.sync(later).unwrap();
        for seed in 0..16 {
            assert!(names(&fs.crashed(seed, false), "").contains(&"e".to_owned()), "seed {seed}");
        }
        assert!(lens.len() > 2, "{lens:?}");
    }

    /// cladefs: a corrupting crash keeps what was synced, and leaves each sector written since
    /// whole, as it was, garbage, or torn at a random offset, its left part or its right part
    /// written, the rest as it was or garbage (TASK-680, board 5748).
    #[test]
    fn corrupting_crashes_keep_what_was_synced_and_mangle_the_rest() {
        let mut fs = Fs::new();
        let file = fs.create(Path::new("f")).unwrap();
        fs.write(file, 0, &[1; 2 * BLOCK]).unwrap();
        fs.sync(file).unwrap();
        fs.sync(ROOT).unwrap();
        fs.write(file, 0, &[3; 2 * BLOCK]).unwrap();
        // A sector of `first` bytes, then of `then` from where they stop, if it is one.
        let cut = |block: &[u8], first: u8, then: u8| {
            let at = block.iter().position(|&byte| byte != first)?;
            (at > 0 && block[at..].iter().all(|&byte| byte == then)).then_some(at)
        };
        let (mut torn, mut cuts, mut garbage) = ([false; 2], BTreeSet::new(), false);
        let mut whole = [false; 2];
        for seed in 0..64 {
            let data = read(&fs.crashed(seed, true), "f");
            assert_eq!(data.len(), 2 * BLOCK);
            for block in data.chunks(BLOCK) {
                match block {
                    _ if block == [1; BLOCK] => whole[0] = true,
                    _ if block == [3; BLOCK] => whole[1] = true,
                    _ => match (cut(block, 3, 1), cut(block, 1, 3)) {
                        (Some(at), _) => (torn[0], _) = (true, cuts.insert(at)),
                        (_, Some(at)) => (torn[1], _) = (true, cuts.insert(at)),
                        _ => garbage = true,
                    },
                }
            }
        }
        assert_eq!((whole, torn, garbage), ([true, true], [true, true], true));
        assert!(cuts.len() > 2, "torn at {cuts:?} alone");
    }

    /// cladefs: a corrupting crash damages no byte a write leaves as it was, the block's others
    /// included (powersafe overwrite, board 5890, decision-3): a log's append, written back a page
    /// at a time, writes again the end of the record synced before it, which every crash keeps,
    /// whatever it makes of the bytes after.
    #[test]
    fn corrupting_crashes_damage_no_byte_written_as_it_was() {
        const SYNCED: usize = BLOCK + 188;
        let mut fs = Fs::new();
        let file = fs.create(Path::new("f")).unwrap();
        fs.write(file, 0, &[1; SYNCED]).unwrap();
        fs.sync(file).unwrap();
        fs.sync(ROOT).unwrap();
        let mut page = vec![1; SYNCED];
        page.resize(2 * BLOCK, 3);
        fs.write(file, 0, &page).unwrap();
        let mut garbage = false;
        for seed in 0..64 {
            let data = read(&fs.crashed(seed, true), "f");
            assert_eq!(data[..SYNCED], [1; SYNCED], "seed {seed} damaged what was synced");
            garbage |= data.len() == 2 * BLOCK && data[SYNCED..].iter().any(|&byte| byte != 3);
        }
        assert!(garbage, "no crash made garbage of the new bytes");
    }
}
