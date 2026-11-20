// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: live migration of an ephemeral cell from this host, the source, to another, the target
//! (backlog doc-25, TASK-698.4; specs/ephemeral). An ephemeral cell's store is files: its base,
//! its kept segments, which never change once kept but for holes given back, and the rest, its
//! raw segments after the base, their logs, its description and its write-ahead log's start. A
//! migration moves them, its rules as the spec names them:
//!
//! 1. **Prepare** (`MigrateCell`): the target notes the cell in its journal and writes its own
//!    reference to the cell's origin before it takes anything (`targetRefersFirst`), and answers
//!    its incarnation.
//! 2. **Rounds**, while the guest runs: the kept segments not sent yet, each whole, sparse.
//! 3. **Hand-over** (`HandOverCell`), once nothing has the store open here, its VM process gone:
//!    the source gives the cell up first, durably, a marker in its store refusing every opening
//!    from then on (`sourceGivesUpFirst`); then it sends the rest, the holes its kept segments
//!    gave back since they were sent among it; then it asks the target to take the cell over,
//!    naming every file and its length, and the CRC of each but the kept segments'. The target
//!    takes it only holding them all so (`openAtNamed`), remembering the migration accepted for
//!    good (`remembersAccepted`); refusing, it drops what it took and its reference, and never takes
//!    that migration after (`refuseFinal`).
//! 4. Accepted, the source removes its store, then retires its own reference (`sourceRetiresLast`);
//!    refused, it removes its marker and serves the cell again. Not answered, the marker stays,
//!    the store unopened here, and asking again, after a restart too, asks the target again, which
//!    answers as it did.

use super::peers::proto::peer_client::PeerClient;
use super::peers::proto::{
    FileDigest, MigrateAbortRequest, MigrateFileRequest, MigrateHandOverRequest,
    MigratePrepareRequest, Range,
};
use crate::ephemeral_host::{self, Entry, staging};
use crate::{CellId, Errno};
use bucket_cell::layout::Origin;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::Status;

/// The marker of a store given up to a migration, in its directory ([`Marker`]).
pub(crate) const MARKER: &str = "migrating";

/// How much of a file a request carries at most.
const PIECE: usize = 4 << 20;

/// How long a round of the source waits for the next.
const ROUND: Duration = Duration::from_millis(500);

/// What a store given up to a migration says of it, in its marker: what asking the target again
/// takes, after a restart too.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    migration: u64,
    peer: String,
    incarnation: u64,
}

/// A migration from this host under way: its target, and what of each kept segment it sent, its
/// length and the bytes it took of the disk then.
struct Outgoing {
    migration: u64,
    peer: String,
    incarnation: u64,
    client: PeerClient<tonic::transport::Channel>,
    sent: tokio::sync::Mutex<HashMap<String, (u64, u64)>>,
    stopped: std::sync::atomic::AtomicBool,
}

node_local::node_local! {
    /// The migrations from this host under way, by their stores' paths.
    static OUTGOING: Mutex<BTreeMap<PathBuf, Arc<Outgoing>>> = Mutex::new(BTreeMap::new());
}

/// The status of `errno`, for a peer.
fn status(what: impl std::fmt::Display, err: impl std::fmt::Display) -> Status {
    Status::internal(format!("{what}: {err:#}"))
}

/// A client of the peer at `address`.
fn client(address: &str) -> Result<PeerClient<tonic::transport::Channel>, Errno> {
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    super::peers::member(runtime.handle(), address).map_err(|err| {
        log::warn!("cladefsd: migrating to {address}: {err}");
        Errno::EINVAL
    })
}

/// Begins the migration of the ephemeral cell of the store at `path` to the daemon whose peer
/// address is `peer`, its store going at `to` there (`MigrateCell`): prepared there, then rounds
/// sending its kept segments as they come, until its hand-over. Returns the migration's ID.
pub(super) async fn migrate(path: PathBuf, peer: String, to: String) -> Result<u64, Errno> {
    let host = ephemeral_host::host()?;
    let read = path.clone();
    let (base, cell) = super::cells::blocking(move || {
        let base = local_cell::host_base(&read).map_err(|_| Errno::ENOENT)?;
        let cell = local_cell::host_cell(&read).map_err(|_| Errno::ENOENT)?;
        Ok((base.ok_or(Errno::EINVAL)?, cell))
    })
    .await?;
    if OUTGOING.get().lock().unwrap().contains_key(&path) {
        return Err(Errno::EALREADY);
    }
    let entry = host.entry(cell).ok_or(Errno::ENOENT)?;
    let parent = base.parent();
    let migration = rand::random::<u64>() | 1;
    let mut client = client(&peer)?;
    let prepare = MigratePrepareRequest {
        migration,
        tenant: cell.tenant,
        cell: cell.id,
        parent_tenant: parent.cell.tenant,
        parent_cell: parent.cell.id,
        at: parent.at,
        bucket: entry.bucket.clone(),
        to,
    };
    let prepared = client.migrate_prepare(prepare).await.map_err(|refused| {
        log::warn!("cladefsd: preparing migration {migration:x} at {peer}: {refused}");
        Errno::EREMOTEIO
    })?;
    let incarnation = prepared.into_inner().incarnation;
    let outgoing = Arc::new(Outgoing {
        migration,
        peer,
        incarnation,
        client,
        sent: tokio::sync::Mutex::default(),
        stopped: std::sync::atomic::AtomicBool::new(false),
    });
    OUTGOING.get().lock().unwrap().insert(path.clone(), outgoing.clone());
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    runtime.spawn(async move {
        while !outgoing.stopped.load(std::sync::atomic::Ordering::Acquire) {
            if let Err(err) = round(&outgoing, &path).await {
                log::warn!("cladefsd: migration {:x}'s round: {err}", outgoing.migration);
            }
            tokio::time::sleep(ROUND).await;
        }
    });
    Ok(migration)
}

/// A round of `outgoing`: the kept segments of the store at `path` not sent yet, each whole.
async fn round(outgoing: &Outgoing, path: &Path) -> Result<(), Status> {
    let mut sent = outgoing.sent.lock().await;
    for (relative, len) in files(path).map_err(|err| status("listing", err))? {
        if !relative.starts_with("kept/") || sent.contains_key(&relative) {
            continue;
        }
        if outgoing.stopped.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let allocated = send(outgoing, path, &relative, len).await?;
        sent.insert(relative, (len, allocated));
    }
    Ok(())
}

/// The files of the store at `path`, by their paths in it, with their lengths: all but its marker
/// and what deleting or replicating leaves.
fn files(path: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut found = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(path.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if [MARKER, "replica-spool", "store.deleted"].contains(&relative.as_str()) {
                continue;
            }
            let metadata = entry.metadata()?;
            match metadata.is_dir() {
                true => dirs.push(relative),
                false => found.push((relative, metadata.len())),
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Sends the file at `relative` in the store at `path`, `len` bytes long, whole, its data and its
/// holes: returns the bytes it takes of the disk.
async fn send(outgoing: &Outgoing, path: &Path, relative: &str, len: u64) -> Result<u64, Status> {
    let file = std::fs::File::open(path.join(relative)).map_err(|err| status(relative, err))?;
    let (data, allocated) = layout_of(&file, len).map_err(|err| status(relative, err))?;
    let mut client = outgoing.client.clone();
    let mut pieces = Vec::new();
    for (start, end) in data {
        let mut at = start;
        while at < end {
            let take = (end - at).min(PIECE as u64) as usize;
            let mut bytes = vec![0; take];
            file.read_exact_at(&mut bytes, at).map_err(|err| status(relative, err))?;
            pieces.push((at, bytes));
            at += take as u64;
        }
    }
    if pieces.is_empty() {
        pieces.push((0, Vec::new()));
    }
    for (offset, bytes) in pieces {
        let request = MigrateFileRequest {
            migration: outgoing.migration,
            path: relative.to_owned(),
            offset,
            bytes,
            len,
            holes: Vec::new(),
        };
        client.migrate_file(request).await?;
    }
    Ok(allocated)
}

/// The ranges of data of `file`, `len` bytes long, by its holes, and the bytes it takes of the
/// disk: as `lseek`'s `SEEK_DATA` and `SEEK_HOLE` find them.
fn layout_of(file: &std::fs::File, len: u64) -> std::io::Result<(Vec<(u64, u64)>, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let allocated = file.metadata()?.blocks() * 512;
    let fd = file.as_raw_fd();
    let seek = |at: u64, whence: libc::c_int| -> std::io::Result<Option<u64>> {
        // SAFETY: Moves an open file descriptor's offset alone, no memory.
        let found = unsafe { libc::lseek(fd, at as libc::off_t, whence) };
        if found >= 0 {
            return Ok(Some(found as u64));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENXIO) => Ok(None),
            _ => Err(err),
        }
    };
    let mut data = Vec::new();
    let mut at = 0;
    while at < len {
        let Some(start) = seek(at, libc::SEEK_DATA)? else { break };
        let end = seek(start, libc::SEEK_HOLE)?.unwrap_or(len).min(len);
        if end > start {
            data.push((start, end));
        }
        at = end;
    }
    Ok((data, allocated))
}

/// The holes of `file`, `len` bytes long: what lies between its ranges of data.
fn holes_of(file: &std::fs::File, len: u64) -> std::io::Result<Vec<Range>> {
    let (data, _) = layout_of(file, len)?;
    let mut holes = Vec::new();
    let mut at = 0;
    for (start, end) in data.into_iter().chain([(len, len)]) {
        if start > at {
            holes.push(Range { offset: at, len: start - at });
        }
        at = end;
    }
    Ok(holes)
}

/// The CRC32C of the file at `path`.
fn crc_of(path: &Path) -> std::io::Result<u32> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(extent_frame::crc(&bytes))
}

/// Hands the ephemeral cell of the store at `path` over to the target of its migration
/// (`HandOverCell`), once nothing has it open here (`EBUSY` otherwise): see the module's docs.
/// `EAGAIN` if the target refused, the store served here again; `EREMOTEIO` if it did not answer,
/// the store given up still, to be asked again.
pub(super) async fn hand_over(path: PathBuf) -> Result<(), Errno> {
    let host = ephemeral_host::host()?;
    let _held = super::cells::hold_store_alone(&path).await?;
    let marker_path = path.join(MARKER);
    let marked: Option<Marker> =
        std::fs::read(&marker_path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let outgoing = OUTGOING.get().lock().unwrap().get(&path).cloned();
    let (marker, files) = match (marked, &outgoing) {
        // Given up before: asked again, sending nothing more.
        (Some(marker), _) => (marker, None),
        (None, Some(outgoing)) => {
            outgoing.stopped.store(true, std::sync::atomic::Ordering::Release);
            let marker = Marker {
                migration: outgoing.migration,
                peer: outgoing.peer.clone(),
                incarnation: outgoing.incarnation,
            };
            let bytes = serde_json::to_vec(&marker).map_err(|_| Errno::EIO)?;
            write_durably(&marker_path, &bytes).map_err(|err| {
                log::warn!("cladefsd: giving {} up: {err}", path.display());
                Errno::EIO
            })?;
            let files = finish(outgoing, &path).await.map_err(|err| {
                log::warn!("cladefsd: migration {:x}'s last files: {err}", outgoing.migration);
                Errno::EREMOTEIO
            })?;
            (marker, Some(files))
        }
        (None, None) => return Err(Errno::ENOENT),
    };
    let mut client = match &outgoing {
        Some(outgoing) => outgoing.client.clone(),
        None => client(&marker.peer)?,
    };
    let files = files.unwrap_or_default();
    let request = MigrateHandOverRequest {
        migration: marker.migration,
        incarnation: marker.incarnation,
        files,
    };
    let accepted = match client.migrate_hand_over(request).await {
        Ok(answer) => answer.into_inner().accepted,
        Err(unanswered) => {
            log::warn!("cladefsd: migration {:x}'s hand-over: {unanswered}", marker.migration);
            return Err(Errno::EREMOTEIO);
        }
    };
    OUTGOING.get().lock().unwrap().remove(&path);
    if !accepted {
        std::fs::remove_file(&marker_path).map_err(|_| Errno::EIO)?;
        return Err(Errno::EAGAIN);
    }
    // Accepted: the store here goes, then this host's reference (`sourceRetiresLast`).
    let cell = local_cell::host_cell(&path).map_err(|_| Errno::EIO)?;
    let entry = host.entry(cell);
    local_cell::remove_store_dir(&path).map_err(|err| {
        log::warn!("cladefsd: removing {}, handed over: {err:#}", path.display());
        Errno::EIO
    })?;
    if let Some(entry) = entry
        && let Err(err) = host.retire(&entry).await
    {
        log::warn!("cladefsd: retiring {cell}'s reference, handed over: {err:#}");
    }
    Ok(())
}

/// Sends what the rounds of `outgoing` have not of the store at `path`, given up: the kept
/// segments not sent, and the holes of those sent that gave back more since; and every other file,
/// whole. Returns every file of the store, as the hand-over names them.
async fn finish(outgoing: &Outgoing, path: &Path) -> Result<Vec<FileDigest>, Status> {
    let mut sent = outgoing.sent.lock().await;
    let mut named = Vec::new();
    for (relative, len) in files(path).map_err(|err| status("listing", err))? {
        let kept = relative.starts_with("kept/");
        match sent.get(&relative) {
            Some(&(was, allocated)) if kept && was == len => {
                let file =
                    std::fs::File::open(path.join(&relative)).map_err(|e| status(&relative, e))?;
                let (_, now) = layout_of(&file, len).map_err(|err| status(&relative, err))?;
                if now != allocated {
                    let holes = holes_of(&file, len).map_err(|err| status(&relative, err))?;
                    let request = MigrateFileRequest {
                        migration: outgoing.migration,
                        path: relative.clone(),
                        offset: 0,
                        bytes: Vec::new(),
                        len,
                        holes,
                    };
                    outgoing.client.clone().migrate_file(request).await?;
                }
            }
            _ => {
                let allocated = send(outgoing, path, &relative, len).await?;
                sent.insert(relative.clone(), (len, allocated));
            }
        }
        let crc = match kept {
            true => None,
            false => Some(crc_of(&path.join(&relative)).map_err(|err| status(&relative, err))?),
        };
        named.push(FileDigest { path: relative, len, crc });
    }
    Ok(named)
}

/// Writes `bytes` to a file at `path`, whole, durably.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    file.write_all_at(bytes, 0)?;
    file.sync_all()?;
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

/// The target's side: what its peer server does of a migration to it.
pub(super) mod target {
    use super::*;

    /// The host's ephemeral cells' directory, for a peer: `FAILED_PRECONDITION` if it keeps none.
    fn host() -> Result<Arc<ephemeral_host::Host>, Status> {
        ephemeral_host::host().map_err(|_| {
            Status::failed_precondition("no ephemeral cells here (CLADEFSD_EPHEMERAL)")
        })
    }

    /// Prepares the migration `request` names: the cell noted, then its reference written here
    /// before anything is taken, then its staging directory made. Again, it answers as before.
    pub(crate) async fn prepare(request: MigratePrepareRequest) -> Result<u64, Status> {
        let host = host()?;
        let MigratePrepareRequest {
            migration,
            tenant,
            cell,
            parent_tenant,
            parent_cell,
            at,
            bucket,
            to,
        } = request;
        let child = CellId { tenant, id: cell };
        let origin = Origin { parent: CellId { tenant: parent_tenant, id: parent_cell }, at };
        if host.accepted(migration) {
            return Ok(host.incarnation());
        }
        let path = PathBuf::from(to);
        let entry = Entry { child, origin, path: path.clone(), bucket, migration: Some(migration) };
        match host.entry(child) {
            Some(found) if found == entry => return Ok(host.incarnation()),
            Some(_) => return Err(Status::already_exists(format!("cell {child} is here"))),
            None => {}
        }
        if path.exists() {
            return Err(Status::already_exists(format!("{} is there", path.display())));
        }
        host.note(&entry).map_err(|err| status("noting the cell", err))?;
        let bucket =
            crate::host_cell::bucket(&entry.bucket).map_err(|err| status("its bucket", err))?;
        bucket_cell::lifecycle::refer_ephemeral(&*bucket, origin, host.incarnation(), child).await;
        std::fs::create_dir_all(staging(&path, migration)).map_err(|err| status("staging", err))?;
        Ok(host.incarnation())
    }

    /// The staging directory of the migration `migration`, being taken here.
    fn staged(host: &ephemeral_host::Host, migration: u64) -> Result<(Entry, PathBuf), Status> {
        let entry = host.receiving(migration).map_err(|err| status("the journal", err))?;
        let entry =
            entry.ok_or_else(|| Status::not_found(format!("no migration {migration:x}")))?;
        let staged = staging(&entry.path, migration);
        Ok((entry, staged))
    }

    /// Writes the piece of a file `request` carries into its migration's staging directory.
    pub(crate) fn file(request: MigrateFileRequest) -> Result<(), Status> {
        let host = host()?;
        let MigrateFileRequest { migration, path, offset, bytes, len, holes } = request;
        let (_, staged) = staged(&host, migration)?;
        if path.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(Status::invalid_argument(format!("{path:?} leaves the store")));
        }
        let file_path = staged.join(&path);
        if let Some(dir) = file_path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| status(&path, err))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&file_path)
            .map_err(|err| status(&path, err))?;
        file.write_all_at(&bytes, offset).map_err(|err| status(&path, err))?;
        if file.metadata().map_err(|err| status(&path, err))?.len() != len {
            file.set_len(len).map_err(|err| status(&path, err))?;
        }
        for Range { offset, len } in holes {
            punch(&file, offset, len).map_err(|err| status(&path, err))?;
        }
        Ok(())
    }

    /// Takes the migration `request` names over, or refuses it, as the module's docs say.
    pub(crate) async fn hand_over(request: MigrateHandOverRequest) -> Result<bool, Status> {
        let host = host()?;
        let MigrateHandOverRequest { migration, incarnation, files } = request;
        if host.accepted(migration) {
            return Ok(true);
        }
        let Ok((entry, staged)) = staged(&host, migration) else { return Ok(false) };
        let whole = incarnation == host.incarnation() && holds(&staged, &files);
        if !whole {
            refuse(&host, &entry, &staged).await?;
            return Ok(false);
        }
        let moved = || -> std::io::Result<()> {
            sync_tree(&staged)?;
            std::fs::rename(&staged, &entry.path)?;
            std::fs::File::open(entry.path.parent().unwrap_or(Path::new(".")))?.sync_all()
        };
        moved().map_err(|err| status("taking the store", err))?;
        host.accept(migration).map_err(|err| status("remembering it accepted", err))?;
        let taken = Entry { migration: None, ..entry };
        host.note(&taken).map_err(|err| status("noting the cell", err))?;
        Ok(true)
    }

    /// Gives the migration `request` names up, if this host holds it not yet.
    pub(crate) async fn abort(request: MigrateAbortRequest) -> Result<(), Status> {
        let host = host()?;
        if host.accepted(request.migration) {
            return Ok(());
        }
        let Ok((entry, staged)) = staged(&host, request.migration) else { return Ok(()) };
        refuse(&host, &entry, &staged).await
    }

    /// Drops what the migration of `entry` took, staged at `staged`, then its reference.
    async fn refuse(
        host: &ephemeral_host::Host,
        entry: &Entry,
        staged: &Path,
    ) -> Result<(), Status> {
        if staged.exists() {
            std::fs::remove_dir_all(staged).map_err(|err| status("dropping it", err))?;
        }
        host.retire(entry).await.map_err(|err| status("retiring its reference", err))
    }

    /// Whether `staged` holds every one of `files`, of its length, of its CRC if named.
    fn holds(staged: &Path, files: &[FileDigest]) -> bool {
        files.iter().all(|FileDigest { path, len, crc }| {
            let at = staged.join(path);
            let sized = std::fs::metadata(&at).is_ok_and(|metadata| metadata.len() == *len);
            sized && crc.is_none_or(|crc| crc_of(&at).is_ok_and(|found| found == crc))
        })
    }

    /// Syncs every file and directory under `dir`, and it.
    fn sync_tree(dir: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                sync_tree(&path)?;
            } else {
                std::fs::File::open(&path)?.sync_all()?;
            }
        }
        std::fs::File::open(dir)?.sync_all()
    }

    /// Gives back the `len` bytes at `offset` of `file`, keeping its size.
    fn punch(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: Changes the blocks of an open file descriptor's file alone, no memory.
        let punched = unsafe {
            libc::fallocate(file.as_raw_fd(), mode, offset as libc::off_t, len as libc::off_t)
        };
        match punched {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}
