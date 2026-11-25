// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Container images as filesystems (TASK-39.5): an image's layers, tar archives of what each
//! changes, applied in turn to a [`Filesystem`], such as a base cell's, which VMs boot forks of.
//!
//! A layer is applied as the OCI image spec says (`layer.md`, "Applying Changesets"):
//!
//! - Each entry makes a file, directory, symlink, hard link, or device or FIFO node, with the
//!   entry's permissions, owner, times and extended attributes (PAX `SCHILY.xattr.*` records). It
//!   replaces what its path names already, except that a directory over a directory keeps its
//!   entries, taking the new one's attributes.
//! - A whiteout, `.wh.NAME`, removes what the layers below made at `NAME`, and an opaque whiteout,
//!   `.wh..wh..opq`, what they made in its directory: whatever the order of the layer's entries,
//!   what the layer itself makes stays, and a directory it keeps, making something in it, or
//!   giving it attributes, is as the layer says, not as the layers below left it.
//! - The directories an entry's path passes through that no entry made are made, root's, with
//!   permissions 0755, at the Unix epoch.
//!
//! A `docker export` tarball is an image of one layer. An image in an OCI image layout, as
//! `skopeo copy` or `docker save` makes, is [`OciImage`]: its layers, gzipped or not, which are
//! checked against their digests as they are applied.
//!
//! What an image makes depends on the image alone: the same image gives the same files, with the
//! same contents, owners, permissions, times and attributes; only their inode numbers and change
//! times differ. Directories' times are set once every layer is applied, as the entries made in a
//! directory change its times. A file's runs of zeros a block long are left as holes.

#[cfg(feature = "testing")]
pub mod testing;

use crate::error::{Errno, Error};
use crate::filesystem::{Filesystem, Node};
use crate::node::{FileType, Owner, SetAttr, SetTime, XattrMode};
use crate::storage::Storage;
use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The prefix of a whiteout's name.
const WHITEOUT: &[u8] = b".wh.";
/// The name of an opaque whiteout.
const OPAQUE: &[u8] = b".wh..wh..opq";
/// The PAX records' prefix of an extended attribute's name.
const PAX_XATTR: &str = "SCHILY.xattr.";
/// How much of a file is read from a layer at a time, and written to it at most at once.
const CHUNK: usize = 1 << 20;
/// The size of the holes left in a file: of its blocks this long that hold only zeros.
const HOLE: usize = 4096;
/// The root's path.
const ROOT: &[u8] = b"";

/// Makes a base: a cell directory at `path`, which must not exist, of `capacity` bytes, holding a
/// filesystem of the image `image`, checked: the image `reference` names in an OCI image layout
/// (see [`OciImage::open`]), or, if `image` is a file, the image of that one layer, which
/// `reference` cannot name. A cell that cannot be made whole is deleted.
#[cfg(not(madsim))]
pub async fn make_base(
    path: impl AsRef<Path>,
    capacity: u64,
    image: &Path,
    reference: Option<&str>,
) -> Result<(), Error> {
    let host = crate::Host::process();
    let keeping = crate::host_cell::keeping_on(host).map_err(Error::new)?;
    let made = (capacity, crate::host_cell::DEFAULT_TENANT);
    make_base_dir_on(host, path.as_ref(), made, keeping, |storage| fill(storage, image, reference))
        .await
}

/// cladefs: makes a base at `path` on `host`, its cell kept as `keeping` says (TASK-237).
#[cfg_attr(not(any(target_os = "linux", madsim)), allow(dead_code))]
pub(crate) async fn make_base_keeping_on(
    host: &std::sync::Arc<crate::Host>,
    path: impl AsRef<Path>,
    (capacity, tenant): (u64, u64),
    (image, reference): (&Path, Option<&str>),
    keeping: local_cell::upload::Options,
) -> Result<(), Error> {
    let filled = |storage| fill(storage, image, reference);
    make_base_dir_on(host, path.as_ref(), (capacity, tenant), keeping, filled).await
}

/// The bytes [`import_memory`] reads of a memory file at once.
pub(crate) const MEMORY_IMPORT_BYTES: usize = 16 << 20;

/// The cell capacity a memory of `size` bytes takes at most: its pages, the tree naming them (a
/// node of 170 refs a page) and their owners (8 bytes a page), and room for a commit's worth of
/// pages written again besides its compactions.
pub fn memory_capacity(size: u64) -> u64 {
    size + size / 128 + size / 512 + (64 << 20)
}

/// A run of pages of a buffer: all zeros, so many bytes of them, or not, these bytes.
pub(crate) enum PageRun<'a> {
    Zeros(usize),
    Data(&'a [u8]),
}

/// The runs of `page`-byte pages of `bytes`, whole pages, by their offsets in it.
pub(crate) fn runs_of_zeros(bytes: &[u8], page: usize) -> Vec<(usize, PageRun<'_>)> {
    let mut runs: Vec<(usize, PageRun<'_>)> = Vec::new();
    for (index, chunk) in bytes.chunks(page).enumerate() {
        let at = index * page;
        let zeros = chunk.iter().all(|byte| *byte == 0);
        match (runs.last_mut(), zeros) {
            (Some((_, PageRun::Zeros(len))), true) => *len += chunk.len(),
            (Some((start, PageRun::Data(data))), false) => *data = &bytes[*start..at + chunk.len()],
            (_, true) => runs.push((at, PageRun::Zeros(chunk.len()))),
            (_, false) => runs.push((at, PageRun::Data(chunk))),
        }
    }
    runs
}

/// Makes a memory cell at `path`, which must not exist, holding the snapshot memory file `file`
/// (board 11222): non-empty, a whole number of 4 KiB pages, zero pages discarded as holes,
/// committed at once, and optionally making an initial version. Returns the new cell's ID and
/// its initial version if requested. A cell that cannot be made whole is deleted.
#[cfg(not(madsim))]
pub async fn import_memory(
    path: impl AsRef<Path>,
    file: &Path,
    make_version: bool,
) -> Result<(crate::CellId, Option<u64>), Error> {
    let host = crate::Host::process();
    let keeping = crate::host_cell::keeping_on(host).map_err(Error::new)?;
    let tenant = crate::host_cell::DEFAULT_TENANT;
    import_memory_keeping_on(host, path.as_ref(), file, tenant, make_version, keeping).await
}

/// cladefs: makes a memory cell at `path` on `host` as [`import_memory`] does, of `tenant`'s, kept
/// as `keeping` says (board 11222).
#[cfg_attr(not(any(target_os = "linux", madsim)), allow(dead_code))]
pub(crate) async fn import_memory_keeping_on(
    host: &std::sync::Arc<crate::Host>,
    path: &Path,
    file: &Path,
    tenant: u64,
    make_version: bool,
    keeping: local_cell::upload::Options,
) -> Result<(crate::CellId, Option<u64>), Error> {
    let page = crate::OpenCell::BLOCK_SIZE as u64;
    let source_path = file.to_owned();
    let (mut source, size) = local_cell::upload::blocking(move || -> Result<_, Error> {
        let opened = std::fs::File::open(&source_path).map_err(|err| {
            let errno = match err.kind() {
                std::io::ErrorKind::NotFound => Errno::ENOENT,
                std::io::ErrorKind::PermissionDenied => Errno::EACCES,
                _ => Errno::EIO,
            };
            Error::new(
                anyhow::Error::new(errno).context(format!("{}: {err}", source_path.display())),
            )
        })?;
        let metadata = opened.metadata().map_err(Error::new)?;
        if !metadata.is_file() {
            return Err(Error::new(Errno::EINVAL)
                .context(format!("{} is not a regular file", source_path.display())));
        }
        let size = metadata.len();
        if size == 0 || !size.is_multiple_of(page) {
            return Err(Error::new(Errno::EINVAL).context(format!(
                "memory file {} is {size} bytes, not a non-empty multiple of {page}",
                source_path.display()
            )));
        }
        Ok((opened, size))
    })?;
    if local_cell::upload::blocking(|| Storage::is_unmade_on(host, path)) {
        Storage::delete_on(host, path).await?;
    } else if local_cell::upload::blocking(|| host_fs::exists(path)) {
        return Err(Error::new(Errno::EEXIST));
    }
    let capacity = memory_capacity(size);
    let storage = Storage::create_unmade_kind_keeping_on(
        host,
        path,
        (capacity, tenant),
        bucket_cell::cell::Kind::Memory,
        keeping,
    )
    .await?;
    let unfinished = Unfinished { host: host.clone(), path: Some(path.to_owned()) };
    let (device, kind) = storage.into_parts();
    let open_cell = match kind {
        crate::storage::Kind::Image { cell, .. } => {
            cell.upgrade().expect("its device holds the open cell")
        }
        crate::storage::Kind::Memory => unreachable!("created on disk"),
    };
    let write_all = move || -> Result<(), Error> {
        let blocks = block_device::BlockDevice::open(device, block_device::Options::default())
            .map_err(Error::new)?;
        blocks.resize(size).map_err(Error::new)?;
        let page = blocks.block_size() as usize;
        let mut buffer = vec![0u8; MEMORY_IMPORT_BYTES];
        let mut offset = 0u64;
        while offset < size {
            let len = (size - offset).min(MEMORY_IMPORT_BYTES as u64) as usize;
            let chunk = &mut buffer[..len];
            source.read_exact(chunk).map_err(Error::new)?;
            for (at, run) in runs_of_zeros(chunk, page) {
                let at = offset + at as u64;
                match run {
                    PageRun::Zeros(len) => blocks.discard(at, len as u64).map_err(Error::new)?,
                    PageRun::Data(bytes) => blocks.write(at, bytes).map_err(Error::new)?,
                }
            }
            offset += len as u64;
        }
        blocks.flush().map_err(Error::new)?;
        Ok(())
    };
    #[cfg(not(madsim))]
    let written = tokio::task::spawn_blocking(move || host_fs::may_block(write_all))
        .await
        .map_err(Error::new)
        .and_then(|res| res);
    #[cfg(madsim)]
    let written = host_fs::may_block(write_all);
    if let Err(err) = written {
        crate::open_cell::release(open_cell).await;
        unfinished.delete().await;
        return Err(err);
    }
    let cell = open_cell.cell();
    let version = if make_version {
        match open_cell.make_version(None).await {
            Ok(version) => Some(version),
            Err(err) => {
                crate::open_cell::release(open_cell).await;
                unfinished.delete().await;
                return Err(err);
            }
        }
    } else {
        None
    };
    crate::open_cell::release(open_cell).await;
    if let Err(err) = local_cell::upload::blocking(|| Storage::made_on(host, path)) {
        unfinished.delete().await;
        return Err(err);
    }
    unfinished.finish();
    Ok((cell, version))
}

/// Makes a cell directory at `path` on `host`, of `capacity` bytes, a new cell of `tenant`'s.
async fn make_base_dir_on<F: Future<Output = anyhow::Result<()>>>(
    host: &std::sync::Arc<crate::Host>,
    path: &Path,
    (capacity, tenant): (u64, u64),
    keeping: local_cell::upload::Options,
    fill: impl FnOnce(Storage) -> F,
) -> Result<(), Error> {
    // cladefs: what an import its process died in left, unmade, is no cell: made anew (TASK-196).
    if local_cell::upload::blocking(|| Storage::is_unmade_on(host, path)) {
        Storage::delete_on(host, path).await?;
    }
    let storage =
        Storage::create_unmade_keeping_on(host, path, (capacity, tenant), keeping).await?;
    let unfinished = Unfinished { host: host.clone(), path: Some(path.to_owned()) };
    // The future filling it, which owns its storage, closes it as it ends, failed or not, before
    // the deletion, which needs it closed.
    if let Err(err) = fill(storage).await {
        unfinished.delete().await;
        return Err(Error::new(err));
    }
    // Whole once filled, and closed: until then, its making's dying leaves no cell.
    local_cell::upload::blocking(|| Storage::made_on(host, path))?;
    unfinished.finish();
    Ok(())
}

/// A cell being made, at the path it holds, which is deleted unless it is
/// [`finish`](Self::finish)ed: as filling it fails ([`delete`](Self::delete)), or panics, or the
/// future making it is dropped, as this drops. The future filling it, which owns its storage, is
/// made after this, so it is dropped first, closing the storage, which [`Storage::delete_on`] needs
/// closed.
struct Unfinished {
    host: std::sync::Arc<crate::Host>,
    path: Option<PathBuf>,
}

impl Unfinished {
    /// Keeps the cell, made whole.
    fn finish(mut self) {
        self.path = None;
    }

    /// Deletes the cell, half made.
    async fn delete(mut self) {
        delete_half_made(self.host.clone(), self.path.take().expect("a cell being made")).await;
    }
}

/// Deletes the cell half made at `path` ([`Unfinished`]).
async fn delete_half_made(host: std::sync::Arc<crate::Host>, path: PathBuf) {
    if let Err(err) = Storage::delete_on(&host, &path).await {
        log::error!("cladefs: deleting the cell half made at {}: {err:#}", path.display());
    }
}

impl Drop for Unfinished {
    /// cladefs: given up on, or panicking: deleted by a task of the runtime the making ran on, its
    /// own; or, on a thread no runtime runs, waiting here, an outermost entry's (board 5614). A
    /// task its host dies before leaves the cell unmade, which its next making deletes
    /// (TASK-196).
    fn drop(&mut self) {
        let Some(path) = self.path.take() else { return };
        let deleting = delete_half_made(self.host.clone(), path);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn(deleting)),
            // No runtime: the thread is the dropper's to block.
            Err(_) => local_cell::upload::wait_on_host(self.host.local(), deleting),
        }
    }
}

/// Formats `storage` with a filesystem of the image `image`, as [`make_base`] has it, and checks
/// it.
async fn fill(storage: Storage, image: &Path, reference: Option<&str>) -> anyhow::Result<()> {
    let fs = Filesystem::format(storage).await?;
    let imported = async {
        let mut importer = Importer::new(&fs);
        if image.is_dir() {
            OciImage::open(image, reference)?.import(&mut importer).await?;
        } else {
            ensure!(reference.is_none(), "a layer holds one image, which no reference names");
            let layer = std::fs::File::open(image).context("opening the layer")?;
            importer.apply(layer).await?;
        }
        Ok(importer.finish().await?)
    }
    .await;
    let storage = fs.close().await?;
    if let Err(err) = imported {
        storage.close().await;
        return Err(err);
    }
    storage.check().await?.close().await;
    Ok(())
}

/// Applies the layers of an image to a filesystem, in turn, bottom first, and then
/// [`finish`](Self::finish)es, setting the directories' times. See the module docs.
pub struct Importer {
    /// The directories found or made, by path, each a handle to it: the root's at [`ROOT`].
    dirs: BTreeMap<Vec<u8>, Node>,
    /// The times of the directories the layers made, and of those they passed through, by path,
    /// which are set once every layer is applied.
    times: BTreeMap<Vec<u8>, Times>,
    /// What the layer being applied made new, which holds nothing the layers below made, and
    /// which its whiteouts keep.
    made: BTreeSet<Vec<u8>>,
    /// The directories the layers below made that the layer being applied keeps, by path: those
    /// it made something in, or gave attributes to, with the attributes its entry says, if one
    /// does. Its whiteouts remove what the layers below made in them, and give them those
    /// attributes, or an implicit directory's (TASK-79.98).
    kept: BTreeMap<Vec<u8>, Option<Metadata>>,
    /// What files' contents are read into from the layers, a chunk at a time: one buffer for all
    /// of them, not one each, as most are far smaller.
    chunk: Box<[u8]>,
}

/// A node's access and modification times.
#[derive(Clone, Copy)]
struct Times {
    atime: SystemTime,
    mtime: SystemTime,
}

impl Times {
    const EPOCH: Self = Self { atime: UNIX_EPOCH, mtime: UNIX_EPOCH };

    fn set_attr(self) -> SetAttr {
        SetAttr {
            atime: Some(SetTime::At(self.atime)),
            mtime: Some(SetTime::At(self.mtime)),
            ..Default::default()
        }
    }
}

/// What a layer's entry says of the node it makes, beyond its type.
#[derive(Clone)]
struct Metadata {
    perm: u16,
    owner: Owner,
    times: Times,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Metadata {
    /// A directory's that an entry's path passes through, which no entry made.
    fn implicit() -> Self {
        Self { perm: 0o755, owner: Owner { uid: 0, gid: 0 }, times: Times::EPOCH, xattrs: vec![] }
    }
}

impl Importer {
    /// An importer of layers into `fs`, which holds what the layers below them made, or nothing.
    pub fn new(fs: &Filesystem) -> Self {
        let dirs = BTreeMap::from([(ROOT.to_vec(), fs.root().clone())]);
        let chunk = vec![0; CHUNK].into_boxed_slice();
        Self { dirs, times: BTreeMap::new(), made: BTreeSet::new(), kept: BTreeMap::new(), chunk }
    }

    /// The root directory.
    fn root(&self) -> Node {
        self.dirs[ROOT].clone()
    }

    /// Applies the layer `layer`, a tar archive, gzipped or not, read as it is applied.
    pub async fn apply(&mut self, layer: impl Read + Send) -> Result<(), Error> {
        self.apply_layer(layer).await.map_err(Error::new)
    }

    async fn apply_layer<'r>(&mut self, layer: impl Read + Send + 'r) -> anyhow::Result<()> {
        let mut layer = BufReader::with_capacity(CHUNK, layer);
        let magic = layer.fill_buf().context("reading the layer")?;
        let layer: Box<dyn Read + Send + 'r> = match magic {
            [0x1f, 0x8b, ..] => Box::new(flate2::bufread::MultiGzDecoder::new(layer)),
            [0x28, 0xb5, 0x2f, 0xfd, ..] => {
                return Err(refused("zstd-compressed layers are not supported"));
            }
            _ => Box::new(layer),
        };
        self.made.clear();
        self.kept.clear();
        // cladefs: walked synchronously in place with a `Send` reader built on `tar::Header`, so
        // the walk stays `Send` across filesystem awaits without `tar::Archive`'s `!Send`
        // `RefCell` or an unmaintained async tar fork (boards 5614, 10238).
        let mut archive = TarReader::new(layer);
        while let Some(entry) = archive.next_entry()? {
            let path = entry.path.clone();
            self.entry(&entry, &mut archive)
                .await
                .with_context(|| format!("applying {:?}", String::from_utf8_lossy(&path)))?;
        }
        Ok(())
    }

    /// Applies the layer's entry `entry`.
    async fn entry(
        &mut self,
        entry: &TarEntry,
        archive: &mut TarReader<impl Read + Send>,
    ) -> anyhow::Result<()> {
        let entry_type = entry.header.entry_type();
        let path = normalize(&entry.path)?;
        let Some((parent, name)) = split(&path) else {
            // The root, which only takes the entry's attributes.
            if !entry_type.is_dir() {
                return Err(refused("the root is not a directory"));
            }
            let metadata = metadata(entry)?;
            return self.update_directory(&self.root(), &path, metadata).await;
        };
        if name == OPAQUE {
            return self.opaque(parent).await;
        }
        if let Some(whited_out) = name.strip_prefix(WHITEOUT) {
            return self.remove_lower(&join(parent, whited_out)).await;
        }
        let metadata = metadata(entry)?;
        let dir = self.directory(parent).await?;
        match dir.lookup(name).await {
            Ok((existing, attr))
                if entry_type.is_dir() && attr.file_type == FileType::Directory =>
            {
                let existing = self.remember_directory(&path, existing);
                if !self.made.contains(&path) {
                    self.kept.insert(path.clone(), Some(metadata.clone()));
                }
                return self.update_directory(&existing, &path, metadata).await;
            }
            Ok((existing, _)) => {
                drop(existing);
                self.remove(&path).await?;
            }
            Err(Errno::ENOENT) => {}
            Err(errno) => return Err(errno.into()),
        }
        self.made.insert(path.clone());
        let (perm, owner) = (metadata.perm, metadata.owner);
        let (node, attr) = match entry_type {
            tar::EntryType::Directory => {
                let (made, _) = dir.mkdir(name, perm, owner).await?;
                let made = self.remember_directory(&path, made);
                return self.update_directory(&made, &path, metadata).await;
            }
            tar::EntryType::Link => {
                let target = entry.link_name.as_deref().ok_or_else(|| refused("no link target"))?;
                let target = normalize(target)?;
                let target = self.find(&target).await?;
                let target = target.ok_or_else(|| refused("the link's target is missing"))?;
                // A hard link is its target's node, which keeps the target's attributes: the
                // entry's own are not set.
                target.link(&dir, name).await?;
                return Ok(());
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                dir.create(name, perm, owner).await?
            }
            tar::EntryType::Symlink => {
                let target =
                    entry.link_name.as_deref().ok_or_else(|| refused("no symlink target"))?;
                dir.symlink(name, target, owner).await?
            }
            tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => {
                let (file_type, rdev) = match entry_type {
                    tar::EntryType::Fifo => (FileType::Fifo, 0),
                    tar::EntryType::Char => (FileType::CharDevice, device(&entry.header)?),
                    _ => (FileType::BlockDevice, device(&entry.header)?),
                };
                dir.mknod(name, file_type, perm, rdev, owner).await?
            }
            other => return Err(refused(format!("entries of type {other:?} are not supported"))),
        };
        if attr.file_type == FileType::File {
            write_contents(&node, archive, entry.size, &mut self.chunk).await?;
        }
        set_metadata(&node, &metadata).await
    }

    /// Gives `dir`, the directory at `path`, the attributes `metadata` says, but its times, which
    /// are set once every layer is applied.
    async fn update_directory(
        &mut self,
        dir: &Node,
        path: &[u8],
        metadata: Metadata,
    ) -> anyhow::Result<()> {
        let set = SetAttr {
            perm: Some(metadata.perm),
            uid: Some(metadata.owner.uid),
            gid: Some(metadata.owner.gid),
            ..Default::default()
        };
        dir.setattr(&set).await?;
        set_xattrs(dir, &metadata.xattrs).await?;
        self.times.insert(path.to_vec(), metadata.times);
        Ok(())
    }

    /// The directory at `path`, made, with the directories it is in, if it isn't there.
    async fn directory(&mut self, path: &[u8]) -> anyhow::Result<Node> {
        Ok(self.walk(path, true).await?.expect("made"))
    }

    /// The directory at `path`, if there is one.
    async fn find_directory(&mut self, path: &[u8]) -> anyhow::Result<Option<Node>> {
        self.walk(path, false).await
    }

    /// The directory at `path`, walking to it from the root, remembering the directories on the
    /// way: if `make`, making those that aren't there, keeping those that are, and failing if one
    /// isn't a directory.
    async fn walk(&mut self, path: &[u8], make: bool) -> anyhow::Result<Option<Node>> {
        let (mut dir, mut end) = (self.root(), 0);
        for name in path.split(|&byte| byte == b'/').filter(|name| !name.is_empty()) {
            end += if end == 0 { name.len() } else { name.len() + 1 };
            let prefix = &path[..end];
            dir = match self.dirs.get(prefix) {
                Some(remembered) => remembered.clone(),
                None => {
                    let found = match dir.lookup(name).await {
                        Ok((found, attr)) if attr.file_type == FileType::Directory => found,
                        Ok(_) => {
                            ensure!(!make, Errno::ENOTDIR);
                            return Ok(None);
                        }
                        Err(Errno::ENOENT) if make => {
                            let implicit = Metadata::implicit();
                            let (made, _) = dir.mkdir(name, implicit.perm, implicit.owner).await?;
                            self.made.insert(prefix.to_vec());
                            self.times.insert(prefix.to_vec(), implicit.times);
                            made
                        }
                        Err(Errno::ENOENT) => return Ok(None),
                        Err(errno) => return Err(errno.into()),
                    };
                    self.remember_directory(prefix, found)
                }
            };
            if make && !self.made.contains(prefix) && !self.kept.contains_key(prefix) {
                self.kept.insert(prefix.to_vec(), None);
            }
        }
        Ok(Some(dir))
    }

    /// Remembers `dir`, a handle to the directory at `path`, unless one is remembered there, and
    /// returns the handle remembered.
    fn remember_directory(&mut self, path: &[u8], dir: Node) -> Node {
        self.dirs.entry(path.to_vec()).or_insert(dir).clone()
    }

    /// The node at `path`, if there is one; the directories it is in are not made.
    async fn find(&mut self, path: &[u8]) -> anyhow::Result<Option<Node>> {
        let Some((parent, name)) = split(path) else {
            return Ok(Some(self.root()));
        };
        let Some(dir) = self.find_directory(parent).await? else {
            return Ok(None);
        };
        match dir.lookup(name).await {
            Ok((found, _)) => Ok(Some(found)),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno.into()),
        }
    }

    /// Removes what is at `path`, if anything, with everything in it, if it is a directory.
    async fn remove(&mut self, path: &[u8]) -> anyhow::Result<()> {
        let Some((parent, name)) = split(path) else {
            bail!("the root cannot be removed");
        };
        let Some(dir) = self.find_directory(parent).await? else {
            return Ok(());
        };
        // Let go of what is remembered of what is removed: of `path`, and of the paths in it, which
        // sort together, so that letting go of them visits them alone, not all that is remembered.
        let inside = inside(path);
        self.dirs.remove(path);
        self.times.remove(path);
        self.made.remove(path);
        self.kept.remove(path);
        self.dirs.extract_if(inside.clone(), |_, _| true).for_each(drop);
        self.times.extract_if(inside.clone(), |_, _| true).for_each(drop);
        self.made.extract_if(inside.clone(), |_| true).for_each(drop);
        self.kept.extract_if(inside, |_, _| true).for_each(drop);
        remove(&dir, name.to_vec()).await
    }

    /// Removes what the layers below made at `path`, a whiteout's, keeping what the layer being
    /// applied made: all of what it made new, and of a directory it keeps, what it made in it,
    /// the directory as the layer says, or as an implicit one, as it would be had the whiteout
    /// come before the layer's entries.
    ///
    /// Iterative so a whiteout over a deep kept directory chain never overflows the thread's stack
    /// (TASK-541, board 2478).
    async fn remove_lower(&mut self, path: &[u8]) -> anyhow::Result<()> {
        self.remove_lower_all(vec![path.to_vec()]).await
    }

    async fn remove_lower_all(&mut self, mut pending: Vec<Vec<u8>>) -> anyhow::Result<()> {
        while let Some(path) = pending.pop() {
            if self.made.contains(&path) {
                continue;
            }
            let Some(said) = self.kept.get(&path).cloned() else {
                self.remove(&path).await?;
                continue;
            };
            let dir = self.directory(&path).await?;
            let metadata = said.unwrap_or_else(Metadata::implicit);
            for xattr in dir.listxattr().await? {
                dir.removexattr(&xattr).await?;
            }
            self.update_directory(&dir, &path, metadata).await?;
            for name in entries(&dir).await? {
                pending.push(join(&path, &name));
            }
        }
        Ok(())
    }

    /// Removes what the layers below made in the directory at `path`, an opaque whiteout's.
    async fn opaque(&mut self, path: &[u8]) -> anyhow::Result<()> {
        let Some(dir) = self.find_directory(path).await? else {
            return Ok(());
        };
        let pending = entries(&dir).await?.into_iter().map(|name| join(path, &name)).collect();
        self.remove_lower_all(pending).await
    }

    /// Sets the times of the directories the layers made or passed through, which are final once
    /// every layer is applied.
    pub async fn finish(self) -> Result<(), Error> {
        for (path, times) in &self.times {
            self.dirs[path].setattr(&times.set_attr()).await.map_err(|errno| {
                Error::new(
                    anyhow::Error::from(errno).context(format!(
                        "setting the times of {:?}",
                        String::from_utf8_lossy(path)
                    )),
                )
            })?;
        }
        Ok(())
    }
}

/// Writes the contents of the file entry `entry` to `file`, a new file, reading them into `chunk`
/// a chunk at a time, and leaving their runs of zeros a block long as holes.
async fn write_contents(
    file: &Node,
    archive: &mut TarReader<impl Read + Send>,
    size: u64,
    chunk: &mut [u8],
) -> anyhow::Result<()> {
    // Where the data written ends: the file's size, as it was empty.
    let mut written = 0;
    let mut offset = 0;
    while offset < size {
        let len = archive.read_entry_data(chunk)?;
        ensure!(len > 0, "the layer ends in the file, at {offset} of {size} bytes");
        // One write a run of data: a write waits for Fxfs's pager to let it dirty its pages,
        // which it asks for together, so a run of blocks costs one round trip, not one each.
        for run in data_runs(&chunk[..len]) {
            written = offset + run.end as u64;
            write_all(file, offset + run.start as u64, &chunk[run]).await?;
        }
        offset += len as u64;
    }
    // A hole at the end, which no write extended the file over.
    if written < size {
        let set = SetAttr { size: Some(size), ..Default::default() };
        file.setattr(&set).await?;
    }
    Ok(())
}

/// The ranges of `data` to write, leaving the rest as holes: the runs of its consecutive blocks of
/// [`HOLE`] bytes, the last of them maybe shorter, that hold more than zeros.
fn data_runs(data: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    fn holds_data((_, block): &(usize, &[u8])) -> bool {
        block.iter().any(|&byte| byte != 0)
    }
    let mut blocks = data.chunks(HOLE).enumerate().peekable();
    std::iter::from_fn(move || {
        let (first, block) = blocks.find(holds_data)?;
        let start = first * HOLE;
        let mut end = start + block.len();
        while let Some((_, block)) = blocks.next_if(holds_data) {
            end += block.len();
        }
        Some(start..end)
    })
}

async fn write_all(file: &Node, mut offset: u64, mut data: &[u8]) -> anyhow::Result<()> {
    while !data.is_empty() {
        let written = file.write(offset, data).await?;
        ensure!(written > 0, "a write wrote nothing");
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

/// Gives `node`, a new node, the times and extended attributes `metadata` says.
async fn set_metadata(node: &Node, metadata: &Metadata) -> anyhow::Result<()> {
    set_xattrs(node, &metadata.xattrs).await?;
    node.setattr(&metadata.times.set_attr()).await?;
    Ok(())
}

async fn set_xattrs(node: &Node, xattrs: &[(Vec<u8>, Vec<u8>)]) -> anyhow::Result<()> {
    for (name, value) in xattrs {
        node.setxattr(name, value, XattrMode::Set).await.with_context(|| {
            format!("setting the extended attribute {:?}", String::from_utf8_lossy(name))
        })?;
    }
    Ok(())
}

/// Removes `name` from directory `dir`, with everything in it, if it is a directory: depth first,
/// a directory's entries before it, from a stack of the steps left to take rather than by
/// recursing, which takes a poll frame of the thread's stack for each level of the tree. However
/// deep the tree, removing it takes as much of the thread's stack as removing one node does.
async fn remove(dir: &Node, name: Vec<u8>) -> anyhow::Result<()> {
    let mut steps = vec![Removal::Entry { dir: dir.clone(), name }];
    while let Some(step) = steps.pop() {
        match step {
            Removal::Entry { dir, name } => {
                let (node, attr) = dir.lookup(&name).await?;
                if attr.file_type != FileType::Directory {
                    drop(node);
                    dir.unlink(&name).await?;
                    continue;
                }
                let children = entries(&node).await?;
                steps.push(Removal::Directory { parent: dir, name });
                // Popped in the order the directory lists them.
                let children = children.into_iter().rev();
                steps.extend(children.map(|name| Removal::Entry { dir: node.clone(), name }));
            }
            // Its entries are removed, and with them the handles to it that their steps held.
            Removal::Directory { parent, name } => parent.rmdir(&name).await?,
        }
    }
    Ok(())
}

/// A step of [`remove`]'s.
enum Removal {
    /// Removes the entry `name` of directory `dir`, whatever its type: a non-directory, or the
    /// entries of a directory, then the directory.
    Entry { dir: Node, name: Vec<u8> },
    /// Removes the directory `name` from `parent`, once its entries are removed.
    Directory { parent: Node, name: Vec<u8> },
}

/// The names of the entries of directory `dir`.
async fn entries(dir: &Node) -> anyhow::Result<Vec<Vec<u8>>> {
    Ok(dir.entries().await?.into_iter().map(|entry| entry.name).collect())
}

/// What a fault of the image itself is: `EINVAL`, as a caller tells a bad image from a failed host
/// by it (TASK-79.88). A tar header's own parse errors stay as they are, I/O errors, as a read of
/// the image failing is.
fn refused(what: impl std::fmt::Display + Send + Sync + 'static) -> anyhow::Error {
    anyhow::Error::new(Errno::EINVAL).context(what)
}

/// What `entry` says of the node it makes: its header, and its PAX records, which take
/// precedence.
fn metadata(entry: &TarEntry) -> anyhow::Result<Metadata> {
    let header = &entry.header;
    let perm = (header.mode().map_err(refused)? & 0o7777) as u16;
    let too_large = |_| refused("the owner's uid or gid is too large");
    let mut uid = header.uid().map_err(refused)?;
    let mut gid = header.gid().map_err(refused)?;
    let mtime = since_epoch(Duration::from_secs(header.mtime().map_err(refused)?))?;
    let mut times = Times { atime: mtime, mtime };
    let mut atime = None;
    let mut xattrs = BTreeMap::new();
    for (key, value) in &entry.pax {
        if let Some(name) = key.strip_prefix(PAX_XATTR) {
            xattrs.insert(name.as_bytes().to_vec(), value.clone());
            continue;
        }
        let text = || std::str::from_utf8(value);
        match key.as_str() {
            "uid" => {
                uid = text()
                    .map_err(|_| refused("failed to parse pax uid"))?
                    .parse()
                    .map_err(|_| refused("failed to parse pax uid"))?;
            }
            "gid" => {
                gid = text()
                    .map_err(|_| refused("failed to parse pax gid"))?
                    .parse()
                    .map_err(|_| refused("failed to parse pax gid"))?;
            }
            "mtime" => times.mtime = pax_time(text().map_err(|_| refused("the PAX time"))?)?,
            "atime" => atime = Some(pax_time(text().map_err(|_| refused("the PAX time"))?)?),
            _ => {}
        }
    }
    let uid = u32::try_from(uid).map_err(too_large)?;
    let gid = u32::try_from(gid).map_err(too_large)?;
    times.atime = atime.unwrap_or(times.mtime);
    // BTreeMap keeps the last value per key (so local PAX overrides global PAX) and yields
    // names in sorted order, however the layer lists them.
    let xattrs = xattrs.into_iter().collect();
    Ok(Metadata { perm, owner: Owner { uid, gid }, times, xattrs })
}

/// The time a PAX record gives, as seconds since the Unix epoch, with a fraction or not. Times
/// before the epoch are the epoch.
fn pax_time(value: &str) -> anyhow::Result<SystemTime> {
    let (secs, fraction) = value.split_once('.').unwrap_or((value, ""));
    let bad = || refused(format!("the PAX time {value:?}"));
    let secs: i64 = secs.parse().map_err(|_| bad())?;
    let Ok(secs) = u64::try_from(secs) else {
        return Ok(UNIX_EPOCH);
    };
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad());
    }
    let nanos = format!("{:0<9}", &fraction[..fraction.len().min(9)]).parse::<u32>()?;
    since_epoch(Duration::new(secs, nanos))
}

/// The time `since` after the Unix epoch, unless it is past what a `SystemTime` holds (`EINVAL`,
/// as for a time past what Fxfs's nanoseconds hold): a header holds its seconds in 64 bits, and
/// the host's time may hold fewer.
fn since_epoch(since: Duration) -> anyhow::Result<SystemTime> {
    let time = UNIX_EPOCH.checked_add(since).ok_or(Errno::EINVAL);
    time.with_context(|| format!("the time {since:?} since the Unix epoch is out of range"))
}

/// The device a device node's header names, as Linux encodes it for user space (`new_encode_dev`,
/// as glibc's `makedev`), which Fxfs keeps.
fn device(header: &tar::Header) -> anyhow::Result<u32> {
    let major =
        header.device_major().map_err(refused)?.ok_or_else(|| refused("no device major number"))?;
    let minor =
        header.device_minor().map_err(refused)?.ok_or_else(|| refused("no device minor number"))?;
    if major >= 1 << 12 || minor >= 1 << 20 {
        return Err(refused(format!("device {major}:{minor} is too large")));
    }
    Ok((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
}

/// The most bytes of an entry's path, Linux's `PATH_MAX` less its NUL: what no path of a file
/// system the image is of holds more of. cladefs: an image is untrusted (AGENTS.md, Trust
/// boundaries): past it, an entry's path, of any length in a PAX record, made the importer hold a
/// copy of each of its prefixes, memory quadratic in its depth (board 2526).
const MAX_PATH: usize = 4095;

/// `path`, an entry's, relative to the root: without leading slashes, `.` or empty components.
/// Paths through `..` are refused, as images are not to name what is outside them; and those
/// longer than [`MAX_PATH`].
fn normalize(path: &[u8]) -> anyhow::Result<Vec<u8>> {
    if path.len() > MAX_PATH {
        return Err(refused(format!("a path of {} bytes, past {MAX_PATH}", path.len())));
    }
    let mut normal = Vec::with_capacity(path.len());
    for component in path.split(|&byte| byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                let path = String::from_utf8_lossy(path);
                return Err(refused(format!("the path {path:?} goes through ..")));
            }
            component => {
                if !normal.is_empty() {
                    normal.push(b'/');
                }
                normal.extend_from_slice(component);
            }
        }
    }
    Ok(normal)
}

/// `path`'s parent and last component, unless it is the root.
fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    match path.iter().rposition(|&byte| byte == b'/') {
        _ if path.is_empty() => None,
        Some(slash) => Some((&path[..slash], &path[slash + 1..])),
        None => Some((&[], path)),
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() { name.to_vec() } else { [dir, b"/", name].concat() }
}

/// The paths in the directory at `path`, which is not the root, in byte order: from `path/` up to
/// `path0`, as `0` follows `/`.
fn inside(path: &[u8]) -> Range<Vec<u8>> {
    [path, b"/"].concat()..[path, b"0"].concat()
}

/// Maximum size of a single tar extension payload (PAX or GNU long name/link), and maximum
/// cumulative retained PAX metadata per entry or across global PAX headers, bounding untrusted
/// archive memory (AGENTS.md, Trust boundaries).
const MAX_EXTENSION: u64 = 1 << 20;
const MAX_RETAINED_PAX: usize = 1 << 20;
/// Maximum consecutive extension headers before a real entry.
const MAX_EXTENSIONS: usize = 64;

/// Whether a PAX key affects entry resolution or metadata; unknown keys are ignored rather than
/// retained in `global_pax` or `local_pax`.
fn relevant_pax_key(key: &str) -> bool {
    matches!(key, "path" | "linkpath" | "size" | "uid" | "gid" | "mtime" | "atime")
        || key.starts_with(PAX_XATTR)
        || key.starts_with("GNU.sparse.")
}

/// A tar entry's parsed metadata, owned so it is `Send` across filesystem awaits.
struct TarEntry {
    header: tar::Header,
    path: Vec<u8>,
    link_name: Option<Vec<u8>>,
    size: u64,
    pax: Vec<(String, Vec<u8>)>,
}

/// cladefs: streaming `Send` tar archive reader built directly on `tar::Header` (board 10238).
/// Replaces `astral-tokio-tar` (unmaintained, RUSTSEC-2026-0440) without `tar::Archive`'s `!Send`
/// `RefCell`.
struct TarReader<R> {
    layer: R,
    global_pax: BTreeMap<String, Vec<u8>>,
    global_pax_bytes: usize,
    remaining: u64,
    padding: u64,
}

impl<R: Read> TarReader<R> {
    fn new(layer: R) -> Self {
        Self { layer, global_pax: BTreeMap::new(), global_pax_bytes: 0, remaining: 0, padding: 0 }
    }

    fn read_entry_data(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let cap = usize::try_from(self.remaining).unwrap_or(usize::MAX).min(buf.len());
        if cap == 0 {
            return Ok(0);
        }
        let read = read_full(&mut self.layer, &mut buf[..cap]).context("reading the layer")?;
        self.remaining -= read as u64;
        Ok(read)
    }

    fn skip_remaining(&mut self) -> anyhow::Result<()> {
        let mut scratch = [0u8; 4096];
        while self.remaining > 0 {
            let want = usize::try_from(self.remaining).unwrap_or(scratch.len()).min(scratch.len());
            let read =
                read_full(&mut self.layer, &mut scratch[..want]).context("reading the layer")?;
            if read == 0 {
                return Err(refused("the layer ends in the entry"));
            }
            self.remaining -= read as u64;
        }
        while self.padding > 0 {
            let want = usize::try_from(self.padding).unwrap_or(scratch.len()).min(scratch.len());
            let read =
                read_full(&mut self.layer, &mut scratch[..want]).context("reading the layer")?;
            if read == 0 {
                self.padding = 0;
                break;
            }
            self.padding -= read as u64;
        }
        Ok(())
    }

    fn read_extension_payload(&mut self, size: u64) -> anyhow::Result<Vec<u8>> {
        if size > MAX_EXTENSION {
            return Err(refused(format!(
                "tar extension header of {size} bytes exceeds {MAX_EXTENSION}"
            )));
        }
        let len = usize::try_from(size).map_err(|_| refused("tar extension header too large"))?;
        let mut buf = vec![0u8; len];
        let read = read_full(&mut self.layer, &mut buf).context("reading the layer")?;
        if read != len {
            return Err(refused("truncated tar extension header"));
        }
        self.padding = tar_padding(size);
        self.skip_remaining()?;
        Ok(buf)
    }

    fn next_entry(&mut self) -> anyhow::Result<Option<TarEntry>> {
        self.skip_remaining()?;
        let mut gnu_longname: Option<Vec<u8>> = None;
        let mut gnu_longlink: Option<Vec<u8>> = None;
        let mut local_pax: Vec<(String, Vec<u8>)> = Vec::new();
        let mut local_pax_bytes: usize = 0;
        let mut extensions: usize = 0;
        loop {
            let mut raw = [0u8; 512];
            let read = read_full(&mut self.layer, &mut raw).context("reading the layer")?;
            let at_end = if read == 0 {
                true
            } else if read < raw.len() {
                return Err(refused("truncated tar header"));
            } else {
                raw.iter().all(|&b| b == 0)
            };
            if at_end {
                if gnu_longname.is_some() || gnu_longlink.is_some() || !local_pax.is_empty() {
                    return Err(refused("tar extension header was not followed by a member"));
                }
                return Ok(None);
            }
            verify_tar_checksum(&raw)?;
            let header = tar::Header::from_byte_slice(&raw).clone();
            let header_size = header.entry_size().map_err(refused)?;
            match header.entry_type() {
                tar::EntryType::GNULongName => {
                    extensions += 1;
                    if extensions > MAX_EXTENSIONS {
                        return Err(refused("too many consecutive tar extension headers"));
                    }
                    let mut bytes = self.read_extension_payload(header_size)?;
                    while bytes.last() == Some(&0) {
                        bytes.pop();
                    }
                    gnu_longname = Some(bytes);
                }
                tar::EntryType::GNULongLink => {
                    extensions += 1;
                    if extensions > MAX_EXTENSIONS {
                        return Err(refused("too many consecutive tar extension headers"));
                    }
                    let mut bytes = self.read_extension_payload(header_size)?;
                    while bytes.last() == Some(&0) {
                        bytes.pop();
                    }
                    gnu_longlink = Some(bytes);
                }
                tar::EntryType::XHeader => {
                    extensions += 1;
                    if extensions > MAX_EXTENSIONS {
                        return Err(refused("too many consecutive tar extension headers"));
                    }
                    let bytes = self.read_extension_payload(header_size)?;
                    for (key, value) in parse_pax(&bytes)? {
                        if !relevant_pax_key(&key) {
                            continue;
                        }
                        let added = key
                            .len()
                            .checked_add(value.len())
                            .ok_or_else(|| refused("PAX metadata exceeds limit"))?;
                        local_pax_bytes = local_pax_bytes
                            .checked_add(added)
                            .filter(|&total| total <= MAX_RETAINED_PAX)
                            .ok_or_else(|| refused("PAX metadata exceeds limit"))?;
                        local_pax.push((key, value));
                    }
                }
                tar::EntryType::XGlobalHeader => {
                    extensions += 1;
                    if extensions > MAX_EXTENSIONS {
                        return Err(refused("too many consecutive tar extension headers"));
                    }
                    let bytes = self.read_extension_payload(header_size)?;
                    for (key, value) in parse_pax(&bytes)? {
                        if !relevant_pax_key(&key) {
                            continue;
                        }
                        if value.is_empty() {
                            if let Some(prev) = self.global_pax.remove(&key) {
                                self.global_pax_bytes =
                                    self.global_pax_bytes.saturating_sub(key.len() + prev.len());
                            }
                            continue;
                        }
                        let prev_bytes =
                            self.global_pax.get(&key).map_or(0, |prev| key.len() + prev.len());
                        let new_bytes = key
                            .len()
                            .checked_add(value.len())
                            .ok_or_else(|| refused("global PAX metadata exceeds limit"))?;
                        let next_total = self
                            .global_pax_bytes
                            .saturating_sub(prev_bytes)
                            .checked_add(new_bytes)
                            .filter(|&total| total <= MAX_RETAINED_PAX)
                            .ok_or_else(|| refused("global PAX metadata exceeds limit"))?;
                        self.global_pax_bytes = next_total;
                        self.global_pax.insert(key, value);
                    }
                }
                _ => {
                    if header.entry_type() == tar::EntryType::GNUSparse
                        || local_pax.iter().any(|(k, _)| k.starts_with("GNU.sparse."))
                        || self.global_pax.keys().any(|k| k.starts_with("GNU.sparse."))
                    {
                        return Err(refused("GNU sparse tar entries are not supported"));
                    }
                    let pax_find = |list: &[(String, Vec<u8>)], name: &str| {
                        list.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone())
                    };
                    let path = pax_find(&local_pax, "path")
                        .or(gnu_longname)
                        .or_else(|| self.global_pax.get("path").cloned())
                        .unwrap_or_else(|| header.path_bytes().into_owned());
                    let link_name = pax_find(&local_pax, "linkpath")
                        .or(gnu_longlink)
                        .or_else(|| self.global_pax.get("linkpath").cloned())
                        .or_else(|| header.link_name_bytes().map(|b| b.into_owned()))
                        .filter(|b| !b.is_empty());
                    let size = match pax_find(&local_pax, "size")
                        .or_else(|| self.global_pax.get("size").cloned())
                    {
                        Some(raw_size) => std::str::from_utf8(&raw_size)
                            .map_err(|_| refused("failed to parse pax size"))?
                            .parse::<u64>()
                            .map_err(|_| refused("failed to parse pax size"))?,
                        None => header_size,
                    };
                    let mut pax: Vec<(String, Vec<u8>)> =
                        self.global_pax.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    pax.extend(local_pax);
                    self.remaining = size;
                    self.padding = tar_padding(size);
                    return Ok(Some(TarEntry { header, path, link_name, size, pax }));
                }
            }
        }
    }
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn tar_padding(size: u64) -> u64 {
    let rem = size % 512;
    if rem == 0 { 0 } else { 512u64.saturating_sub(rem) }
}

/// Validates a 512-byte tar header's checksum against both unsigned and signed byte sums.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn verify_tar_checksum(raw: &[u8; 512]) -> anyhow::Result<()> {
    let header = tar::Header::from_byte_slice(raw);
    let expected = header.cksum().map_err(refused)?;
    let mut unsigned_sum: u32 = 0;
    let mut signed_sum: i32 = 0;
    for (idx, &byte) in raw.iter().enumerate() {
        let effective = if (148..156).contains(&idx) { b' ' } else { byte };
        unsigned_sum = unsigned_sum
            .checked_add(u32::from(effective))
            .ok_or_else(|| refused("tar checksum overflow"))?;
        signed_sum = signed_sum
            .checked_add(i32::from(effective as i8))
            .ok_or_else(|| refused("tar checksum overflow"))?;
    }
    if expected == unsigned_sum || i64::from(expected) == i64::from(signed_sum) {
        Ok(())
    } else {
        Err(refused("tar header checksum mismatch"))
    }
}

/// Parses PAX extended header records (`<len> <key>=<value>\n`) from untrusted tar bytes.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_pax(data: &[u8]) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut pos: usize = 0;
    while pos < data.len() {
        let rest = data.get(pos..).ok_or_else(|| refused("truncated PAX record"))?;
        let space = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| refused("PAX record missing length delimiter"))?;
        let len_bytes = rest.get(..space).ok_or_else(|| refused("PAX record length"))?;
        let len_str = std::str::from_utf8(len_bytes).map_err(|_| refused("PAX record length"))?;
        let record_len: usize = len_str.parse().map_err(|_| refused("PAX record length"))?;
        let body_start = space.checked_add(1).ok_or_else(|| refused("PAX record overflow"))?;
        if record_len <= body_start {
            return Err(refused("PAX record length too small"));
        }
        let end = pos.checked_add(record_len).ok_or_else(|| refused("PAX record overflow"))?;
        let record = data.get(pos..end).ok_or_else(|| refused("truncated PAX record"))?;
        if record.last() != Some(&b'\n') {
            return Err(refused("PAX record missing trailing newline"));
        }
        let body_end = record_len.checked_sub(1).ok_or_else(|| refused("PAX record length"))?;
        let body = record.get(body_start..body_end).ok_or_else(|| refused("PAX record body"))?;
        let eq = body
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| refused("PAX record missing '='"))?;
        let key_bytes = body.get(..eq).ok_or_else(|| refused("PAX record key"))?;
        let key =
            std::str::from_utf8(key_bytes).map_err(|_| refused("PAX record key not UTF-8"))?;
        let val_start = eq.checked_add(1).ok_or_else(|| refused("PAX record overflow"))?;
        let value = body.get(val_start..).ok_or_else(|| refused("PAX record value"))?;
        out.push((key.to_owned(), value.to_vec()));
        pos = end;
    }
    Ok(out)
}

/// Reads from `reader` into `buf` until it is full or `reader` ends, and returns how much it read.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut len = 0;
    while len < buf.len() {
        match reader.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(read) => len += read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(len)
}

/// An image in an OCI image layout (the OCI image spec's `image-layout.md`): a directory with an
/// `index.json` naming its images' manifests, and the blobs they name, under `blobs/sha256`.
pub struct OciImage {
    layout: PathBuf,
    layers: Vec<Descriptor>,
}

/// A blob's descriptor: its digest, and, in an index, its annotations and platform.
#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    annotations: HashMap<String, String>,
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

/// An index of manifests, or a manifest, of layers.
#[derive(Deserialize)]
struct Blob {
    manifests: Option<Vec<Descriptor>>,
    layers: Option<Vec<Descriptor>>,
}

/// The annotations that name an image in an index: OCI's, the tag, and containerd's, which
/// `docker save` writes, the image's full name.
const REF_NAMES: [&str; 2] = ["org.opencontainers.image.ref.name", "io.containerd.image.name"];

impl OciImage {
    /// The image `reference`, which an annotation of its manifest in the layout's index names
    /// (`org.opencontainers.image.ref.name`, or `io.containerd.image.name`), or the layout's only
    /// image; for this host's architecture, if it is one of several platforms'.
    pub fn open(layout: impl AsRef<Path>, reference: Option<&str>) -> Result<Self, Error> {
        let layout = layout.as_ref();
        Self::open_layout(layout, reference)
            .with_context(|| format!("opening the OCI image layout {}", layout.display()))
            .map_err(Error::new)
    }

    fn open_layout(layout: &Path, reference: Option<&str>) -> anyhow::Result<Self> {
        let index = read_small(&layout.join("index.json")).context("reading index.json")?;
        let index: Blob = serde_json::from_slice(&index).context("parsing index.json")?;
        let manifests = index.manifests.context("index.json lists no manifests")?;
        let names = |descriptor: &Descriptor| -> Vec<String> {
            REF_NAMES.iter().filter_map(|key| descriptor.annotations.get(*key)).cloned().collect()
        };
        let mut matching: Vec<Descriptor> = match reference {
            Some(reference) => manifests
                .into_iter()
                .filter(|descriptor| names(descriptor).iter().any(|name| name == reference))
                .collect(),
            None => manifests,
        };
        let manifest = match (matching.len(), reference) {
            (1, _) => matching.remove(0),
            (0, Some(reference)) => bail!("no image is named {reference:?}"),
            (_, _) => {
                let named: Vec<Vec<String>> = matching.iter().map(names).collect();
                bail!("the layout holds several images, so one must be named: {named:?}")
            }
        };
        let mut layers = None;
        let mut descriptor = manifest;
        // An index of the image's manifests for several platforms, or the manifest.
        for _ in 0..8 {
            let blob: Blob = serde_json::from_slice(&read_blob(layout, &descriptor.digest)?)
                .with_context(|| format!("parsing {}", descriptor.digest))?;
            if let Some(found) = blob.layers {
                layers = Some(found);
                break;
            }
            let manifests = blob.manifests.context("a blob is neither a manifest nor an index")?;
            descriptor = platform(manifests)?;
        }
        let layers = layers.context("indexes nest too deeply")?;
        Ok(Self { layout: layout.to_owned(), layers })
    }

    /// Applies the image's layers to `importer`'s filesystem, checking each against its digest
    /// first.
    pub async fn import(&self, importer: &mut Importer) -> Result<(), Error> {
        self.import_hashed(importer, |_| {}).await
    }

    /// [`OciImage::import`], `hashed` called with each layer's blob's path once it is hashed,
    /// before it is applied: a test's, to change the layout between the two.
    pub(crate) async fn import_hashed(
        &self,
        importer: &mut Importer,
        mut hashed: impl FnMut(&Path),
    ) -> Result<(), Error> {
        for layer in &self.layers {
            // cladefs: its whole blob hashed, off the runtime's other tasks (board 5614), the
            // file opened once (board 8541), before anything of it is applied.
            let verified = || verified_blob(&self.layout, &layer.digest);
            let blob = local_cell::upload::blocking(verified).map_err(Error::new)?;
            hashed(&blob.path);
            // And what is applied hashed again as it is read, to its end: its inode, written in
            // place since, as the file the import holds open may be, whatever its size and times
            // say (hotbed review 57, board 9703), holds what it did, or the import is refused,
            // whatever its applying said of the bytes written meanwhile. A blob replaced by
            // another in the layout leaves the inode held as it was (board 10493).
            let mut applying = Hashing { file: blob.file, hasher: Sha256::new() };
            let applied = importer.apply_layer(&mut applying).await;
            let drained = local_cell::upload::blocking(|| {
                std::io::copy(&mut applying, &mut std::io::sink())
                    .with_context(|| format!("reading {}", blob.path.display()))
            });
            drained.map_err(Error::new)?;
            if check_digest(&layer.digest, applying.hasher.finalize().as_slice()).is_err() {
                let changed = format!("{} changed as it was imported", blob.path.display());
                return Err(Error::new(refused(changed)));
            }
            applied.context(layer.digest.clone()).map_err(Error::new)?;
        }
        Ok(())
    }
}

/// The manifest of `manifests`, an index's, for this host's platform.
fn platform(manifests: Vec<Descriptor>) -> anyhow::Result<Descriptor> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    manifests
        .into_iter()
        .find(|descriptor| {
            descriptor.platform.as_ref().is_some_and(|platform| {
                platform.os == "linux" && platform.architecture == architecture
            })
        })
        .with_context(|| format!("the image has no manifest for linux/{architecture}"))
}

/// The path of the blob `digest` names in `layout`. Only SHA-256 digests are supported.
fn blob_path(layout: &Path, digest: &str) -> anyhow::Result<PathBuf> {
    let hex = digest.strip_prefix("sha256:").with_context(|| format!("the digest {digest:?}"))?;
    ensure!(
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the digest {digest:?} is not a SHA-256 digest"
    );
    Ok(layout.join("blobs/sha256").join(hex))
}

/// The most bytes `index.json`, a manifest or an index holds: what registries take
/// (distribution-spec's 4 MiB), past which one is refused before it is read (board 8541).
const MAX_MANIFEST: u64 = 4 << 20;

/// The file at `path` in an image's layout, opened once, for what is read of it to be what is
/// checked of it, whatever the layout holds there after (board 8541): refused (`EINVAL`) if a
/// symlink, which may name any file of the host, or anything but a regular file. Opened without
/// waiting, as a FIFO's opening would for a writer (hotbed review 57): reads of a regular file
/// wait as ever.
fn open_regular(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let opened = std::fs::File::options().read(true).custom_flags(flags).open(path);
    let file = match opened {
        Ok(file) => file,
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(refused(format!("{} is a symlink", path.display())));
        }
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!("opening {}", path.display())));
        }
    };
    let metadata = file.metadata().with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() {
        return Err(refused(format!("{} is not a regular file", path.display())));
    }
    Ok(file)
}

/// The file at `path` in an image's layout, as [`open_regular`] opens it, read whole: refused
/// (`EINVAL`) past [`MAX_MANIFEST`], before it is read, or as it is if it grows meanwhile.
fn read_small(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = open_regular(path)?;
    let len = file.metadata().with_context(|| format!("reading {}", path.display()))?.len();
    let too_large = || refused(format!("{} is larger than {MAX_MANIFEST} bytes", path.display()));
    if len > MAX_MANIFEST {
        return Err(too_large());
    }
    let mut read = Vec::with_capacity(len as usize);
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut read)
        .with_context(|| format!("reading {}", path.display()))?;
    if read.len() as u64 > MAX_MANIFEST {
        return Err(too_large());
    }
    Ok(read)
}

/// The blob `digest` names in `layout`, a manifest or an index, checked against it.
fn read_blob(layout: &Path, digest: &str) -> anyhow::Result<Vec<u8>> {
    let blob = read_small(&blob_path(layout, digest)?)?;
    check_digest(digest, Sha256::digest(&blob).as_slice())?;
    Ok(blob)
}

/// A layer's blob, opened once and checked against its digest ([`verified_blob`]).
struct Verified {
    /// Where it is in the layout: no longer what is read.
    path: PathBuf,
    /// The blob, rewound: what is applied.
    file: std::fs::File,
}

/// A layer's blob as it is applied, hashing what is read of it ([`OciImage::import_hashed`]).
struct Hashing {
    file: std::fs::File,
    hasher: Sha256,
}

impl Read for Hashing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.file.read(buf)?;
        self.hasher.update(&buf[..len]);
        Ok(len)
    }
}

/// The blob `digest` names in `layout`, a layer, opened once, its contents checked against it,
/// and rewound: what is applied (board 8541).
fn verified_blob(layout: &Path, digest: &str) -> anyhow::Result<Verified> {
    use std::io::Seek as _;
    let path = blob_path(layout, digest)?;
    let mut file = open_regular(&path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; CHUNK];
    loop {
        let len = read_full(&mut file, &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if len == 0 {
            break;
        }
        hasher.update(&buf[..len]);
    }
    check_digest(digest, hasher.finalize().as_slice())?;
    file.rewind().with_context(|| format!("rewinding {}", path.display()))?;
    Ok(Verified { path, file })
}

fn check_digest(digest: &str, sha256: &[u8]) -> anyhow::Result<()> {
    let actual: String = sha256.iter().map(|byte| format!("{byte:02x}")).collect();
    ensure!(digest == format!("sha256:{actual}"), "the blob {digest} holds sha256:{actual}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot;
    use futures::executor::block_on;
    use futures::future::{Either, select};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// How a cell an import here makes keeps its cell: where no host's daemon serves it.
    fn keeping() -> local_cell::upload::Options {
        crate::host_cell::keeping().unwrap()
    }

    /// A cell whose filling panics is deleted as the panic unwinds, as one whose filling fails
    /// is, once the filesystem filling it has dropped, closing it.
    #[test]
    fn a_cell_whose_filling_panics_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let made = catch_unwind(AssertUnwindSafe(|| {
            block_on(make_base_dir_on(
                crate::Host::process(),
                &path,
                (64 << 20, crate::host_cell::DEFAULT_TENANT),
                keeping(),
                |storage| async {
                    let _fs = Filesystem::format(storage).await?;
                    panic!("filling the cell panicked");
                },
            ))
        }));
        let panic = made.expect_err("filling the cell did not panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"filling the cell panicked"));
        assert!(!path.exists(), "the cell is left half made");
    }

    /// A cell whose making is given up on, its future dropped as a filesystem fills it, is
    /// deleted, once the filesystem filling it has dropped, closing it: one closed after would
    /// leave the cell half made.
    #[test]
    fn a_cell_given_up_on_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        block_on(async {
            let (formatted, filling) = oneshot::channel();
            let made = Box::pin(make_base_dir_on(
                crate::Host::process(),
                &path,
                (64 << 20, crate::host_cell::DEFAULT_TENANT),
                keeping(),
                |storage| async {
                    let _fs = Filesystem::format(storage).await?;
                    formatted.send(()).unwrap();
                    std::future::pending().await
                },
            ));
            let Either::Right((formatted, made)) = select(made, filling).await else {
                panic!("the cell was made");
            };
            formatted.unwrap();
            drop(made);
        });
        assert!(!path.exists(), "the cell is left half made");
    }

    /// Where [`child_dies_importing`] imports, which [`a_cell_whose_importer_died_is_made_anew`]
    /// names.
    const CHILD_CELL_DIR: &str = "CLADEFS_IMPORT_TEST_STORE";

    /// An import whose process dies, for [`a_cell_whose_importer_died_is_made_anew`]: fills the
    /// cell at [`CHILD_CELL_DIR`] with a file, synced, says so, and waits to be killed.
    #[test]
    #[ignore = "run by a_cell_whose_importer_died_is_made_anew"]
    fn child_dies_importing() {
        use crate::import::testing::{Entry, Kind, layer};
        let Ok(path) = std::env::var(CHILD_CELL_DIR) else { return };
        let half = layer(&[Entry::new("half", Kind::File(b"half an image"))]).unwrap();
        let made = block_on(make_base_dir_on(
            crate::Host::process(),
            Path::new(&path),
            (64 << 20, crate::host_cell::DEFAULT_TENANT),
            keeping(),
            |storage| async move {
                let fs = Filesystem::format(storage).await?;
                let mut importer = Importer::new(&fs);
                importer.apply(&half[..]).await?;
                fs.root().fsync().await?;
                println!("FILLED");
                std::future::pending().await
            },
        ));
        panic!("the import ended: {made:?}");
    }

    /// TASK-196: a cell whose import's process died as it filled it, killed or its host crashing,
    /// is no cell: what the import made of it opens as none, and an import to its path makes it
    /// anew. On main the half-made cell opened, a filesystem holding part of its image, and
    /// every import to the path after failed with `EEXIST` (`ImportImage`'s ALREADY_EXISTS),
    /// which a caller that retries an import its daemon's restart cut off takes for the cell.
    #[test]
    fn a_cell_whose_importer_died_is_made_anew() {
        use crate::import::testing::{Entry, Kind, layer};
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "import::tests::child_dies_importing", "--ignored", "--nocapture"])
            .env(CHILD_CELL_DIR, &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let filled = stdout.lines().map(Result::unwrap).any(|line| line == "FILLED");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(filled, "the child's import never filled the cell");

        let opened = block_on(async { Filesystem::mount(Storage::open(&path).await?).await });
        assert!(opened.is_err(), "the cell half made opens");
        drop(opened);
        let image = dir.path().join("whole.tar");
        let whole = layer(&[Entry::new("whole", Kind::File(b"the whole image"))]).unwrap();
        std::fs::write(&image, whole).unwrap();
        block_on(make_base(&path, 64 << 20, &image, None)).unwrap();
        let names = block_on(async {
            let fs = Filesystem::mount(Storage::open(&path).await.unwrap()).await.unwrap();
            let entries = fs.root().entries().await.unwrap();
            fs.close().await.unwrap();
            entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>()
        });
        assert_eq!(names, [b"whole".to_vec()]);
    }

    /// A chunk of a file is written a run of data at a time: the blocks that hold data, together
    /// while they are consecutive, a short last one too, but not the blocks of zeros between.
    #[test]
    #[allow(clippy::single_range_in_vec_init, reason = "a chunk of data is one run")]
    fn contents_are_written_a_run_of_data_at_a_time() {
        let runs = |data: &[u8]| data_runs(data).collect::<Vec<_>>();
        assert_eq!(runs(&vec![0x5a; CHUNK]), [0..CHUNK]);
        // Typed: regex-automata, which the `loom` feature brings in, compares ranges with spans.
        assert_eq!(runs(&vec![0; CHUNK]), [] as [std::ops::Range<usize>; 0]);
        // Data in blocks 1 and 2, 5, and the short last one.
        let mut chunk = vec![0; 8 * HOLE + 100];
        for at in [HOLE, 3 * HOLE - 1, 5 * HOLE + 7, 8 * HOLE + 99] {
            chunk[at] = 1;
        }
        assert_eq!(runs(&chunk), [HOLE..3 * HOLE, 5 * HOLE..6 * HOLE, 8 * HOLE..8 * HOLE + 100]);
        // A short last block after one of data is in its run.
        chunk[7 * HOLE] = 1;
        assert_eq!(runs(&chunk), [HOLE..3 * HOLE, 5 * HOLE..6 * HOLE, 7 * HOLE..8 * HOLE + 100]);
    }

    /// The paths in a directory are the ones its range holds: not its own, nor its neighbours'
    /// whose names begin with its name.
    #[test]
    fn the_paths_in_a_directory_sort_together() {
        let inside = inside(b"o/x");
        for path in [&b"o/x/a"[..], b"o/x/a/b", b"o/x/\xff"] {
            assert!(inside.contains(&path.to_vec()), "{path:?}");
        }
        for path in [&b"o/x"[..], b"o/x-y", b"o/x.y", b"o/x0", b"o/xy", b"o/w/x"] {
            assert!(!inside.contains(&path.to_vec()), "{path:?}");
        }
    }

    /// An index's manifest for this host is the one for Linux on its architecture.
    #[test]
    fn manifests_are_linuxs_on_this_architecture() {
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let descriptor = |digest: &str, os: &str, architecture: &str| Descriptor {
            digest: digest.to_owned(),
            annotations: HashMap::new(),
            platform: Some(Platform { architecture: architecture.to_owned(), os: os.to_owned() }),
        };
        let manifests = || {
            vec![
                Descriptor {
                    digest: "none".to_owned(),
                    annotations: HashMap::new(),
                    platform: None,
                },
                descriptor("other", "linux", "riscv64"),
                descriptor("windows", "windows", architecture),
                descriptor("this", "linux", architecture),
            ]
        };
        assert_eq!(platform(manifests()).unwrap().digest, "this");
        let mut others = manifests();
        others.pop();
        let error = platform(others).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!("the image has no manifest for linux/{architecture}")
        );
    }

    #[test]
    fn tar_reader_refuses_malformed_pax_and_bad_checksum() {
        assert_eq!(parse_pax(b"12 path=a/b\n").unwrap(), [("path".to_owned(), b"a/b".to_vec())]);
        for bad in [
            &b"notalen a=b\n"[..],
            b"3 a=b\n",
            b"13 path=a/b\n",
            b"12 path=a/b ",
            b"12 noequals\n",
            b"999999999999999999999999999 a=b\n",
        ] {
            let err = parse_pax(bad).unwrap_err();
            assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{bad:?}: {err:#}");
        }
        let mut raw = [0u8; 512];
        raw[0] = b'x';
        let err = verify_tar_checksum(&raw).unwrap_err();
        assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{err:#}");
        raw[148..155].copy_from_slice(b"0000000");
        let err = verify_tar_checksum(&raw).unwrap_err();
        assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{err:#}");
    }

    /// Hotbed review 106: GNU sparse entries (both `GNUSparse` type `'S'` and PAX `GNU.sparse.*`),
    /// dangling extension headers at EOF or zero blocks, and unbounded global PAX accumulation are
    /// each refused with `EINVAL`.
    #[test]
    fn tar_reader_refuses_sparse_dangling_extensions_and_unbounded_global_pax() {
        let append_raw = |out: &mut Vec<u8>, kind: tar::EntryType, name: &str, payload: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_path(name).unwrap();
            h.set_size(payload.len() as u64);
            h.set_mode(0o644);
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(0);
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            out.extend_from_slice(payload);
            let pad = tar_padding(payload.len() as u64) as usize;
            out.resize(out.len() + pad, 0);
        };
        let pax_record = |k: &str, v: &[u8]| {
            let body_len = k.len() + 1 + v.len() + 1;
            let mut total = body_len + 2;
            loop {
                let prefix = format!("{total} ");
                if prefix.len() + body_len == total {
                    let mut rec = prefix.into_bytes();
                    rec.extend_from_slice(k.as_bytes());
                    rec.push(b'=');
                    rec.extend_from_slice(v);
                    rec.push(b'\n');
                    return rec;
                }
                total = prefix.len() + body_len;
            }
        };

        // 1. GNU sparse entry type 'S' is refused instead of silently writing packed bytes.
        let mut sparse_s = Vec::new();
        append_raw(&mut sparse_s, tar::EntryType::GNUSparse, "sparse", b"packed");
        let err = TarReader::new(sparse_s.as_slice()).next_entry().err().expect("refused");
        assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{err:#}");

        // 2. PAX GNU.sparse.* entry is refused.
        let mut sparse_pax = Vec::new();
        let rec = pax_record("GNU.sparse.major", b"1");
        append_raw(&mut sparse_pax, tar::EntryType::XHeader, "PaxHeaders", &rec);
        append_raw(&mut sparse_pax, tar::EntryType::Regular, "sparse", b"packed");
        let err = TarReader::new(sparse_pax.as_slice()).next_entry().err().expect("refused");
        assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{err:#}");

        // 3. Dangling GNULongName / GNULongLink / XHeader at EOF or zero block is refused.
        for (kind, payload) in [
            (tar::EntryType::GNULongName, b"long-name-without-member\0".as_slice()),
            (tar::EntryType::GNULongLink, b"long-link-without-member\0".as_slice()),
            (tar::EntryType::XHeader, b"12 path=a/b\n".as_slice()),
        ] {
            let mut dangling_eof = Vec::new();
            append_raw(&mut dangling_eof, kind, "ext", payload);
            let err = TarReader::new(dangling_eof.as_slice()).next_entry().err().expect("refused");
            assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{kind:?} eof: {err:#}");

            let mut dangling_zero = dangling_eof;
            dangling_zero.extend_from_slice(&[0u8; 512]);
            let err = TarReader::new(dangling_zero.as_slice()).next_entry().err().expect("refused");
            assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{kind:?} zero: {err:#}");
        }

        // 4. Unknown global PAX keys are not retained, and retained global PAX keys are bounded.
        let mut unknown_global = Vec::new();
        for i in 0..20 {
            let rec = pax_record(&format!("VENDOR.junk.{i}"), &vec![b'x'; 100_000]);
            append_raw(&mut unknown_global, tar::EntryType::XGlobalHeader, "pax", &rec);
            append_raw(&mut unknown_global, tar::EntryType::Directory, "./", b"");
        }
        let mut reader = TarReader::new(unknown_global.as_slice());
        while reader.next_entry().unwrap().is_some() {}
        assert_eq!(reader.global_pax_bytes, 0);

        let mut huge_global = Vec::new();
        for i in 0..20 {
            let rec = pax_record(&format!("SCHILY.xattr.user.k{i}"), &vec![b'x'; 100_000]);
            append_raw(&mut huge_global, tar::EntryType::XGlobalHeader, "pax", &rec);
            append_raw(&mut huge_global, tar::EntryType::Directory, "./", b"");
        }
        let mut reader = TarReader::new(huge_global.as_slice());
        let mut refused_err = None;
        loop {
            match reader.next_entry() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => {
                    refused_err = Some(err);
                    break;
                }
            }
        }
        let err = refused_err.expect("unbounded global PAX should be refused");
        assert_eq!(err.downcast_ref::<Errno>(), Some(&Errno::EINVAL), "{err:#}");

        // 5. Local PAX xattrs override global PAX xattrs regardless of lexical value order.
        for (global_val, local_val) in
            [(b"z-global".as_slice(), b"a-local".as_slice()), (b"a-global", b"z-local")]
        {
            let mut archive = Vec::new();
            let global_rec = pax_record("SCHILY.xattr.user.origin", global_val);
            let local_rec = pax_record("SCHILY.xattr.user.origin", local_val);
            append_raw(&mut archive, tar::EntryType::XGlobalHeader, "pax-global", &global_rec);
            append_raw(&mut archive, tar::EntryType::XHeader, "pax-local", &local_rec);
            append_raw(&mut archive, tar::EntryType::Regular, "file", b"data");
            let mut reader = TarReader::new(archive.as_slice());
            let entry = reader.next_entry().unwrap().expect("file entry");
            let meta = metadata(&entry).unwrap();
            assert_eq!(meta.xattrs, vec![(b"user.origin".to_vec(), local_val.to_vec())]);
        }
    }
}
