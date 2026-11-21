// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: live migration of an ephemeral cell from this host, the source, to another, the target
//! (backlog doc-25, TASK-698.4; specs/ephemeral). An ephemeral cell is files: its local checkpoint,
//! its kept segments, which never change once kept but for holes given back, and the rest, its raw
//! segments after the local checkpoint, their logs, its description and its write-ahead log's
//! start. A migration moves them, its rules as the spec names them:
//!
//! 1. **Prepare** (`MigrateCell`): the target notes the cell in its journal and writes its own
//!    reference to the cell's origin before it takes anything (`targetRefersFirst`), and answers
//!    its incarnation. Taking an older migration of the cell, never accepted, it gives that one up
//!    first (`supersedes`): the cell's writer, preparing this one, pursues it no more. A source
//!    whose prepare went unanswered aborts that migration by its ID (`SourceLoses`).
//! 2. **Rounds**, while the guest runs: the kept segments not sent yet, each whole, sparse.
//! 3. **Hand-over** (`HandOverCell`), once nothing has the cell open here, its VM process gone: the
//!    source gives the cell up first, durably, a marker in its cell refusing every opening from
//!    then on (`sourceGivesUpFirst`); then it sends the rest, the holes its kept segments gave back
//!    since they were sent among it; then it asks the target to take the cell over, naming every
//!    file and its length, and the CRC of each but the kept segments'. The target takes it only
//!    holding them all so (`openAtNamed`), remembering the migration accepted for good
//!    (`remembersAccepted`); refusing, it drops what it took and its reference, and never takes
//!    that migration after (`refuseFinal`).
//! 4. Accepted, the source removes its cell, then retires its own reference (`sourceRetiresLast`),
//!    a note beside the cell directory saying it handed the cell over until both are done, so that
//!    asking again after either failed finishes them (board 5665); refused, it removes its marker,
//!    durably, and serves the cell again. Not answered, the marker
//!    stays, the cell unopened here, and asking again, after a restart too, sends again what the
//!    target lacks and asks it again, naming every file as before; the target answers as it did.

use super::peers::proto::peer_client::PeerClient;
use super::peers::proto::{
    FileDigest, MigrateAbortRequest, MigrateFileRequest, MigrateHandOverRequest,
    MigratePrepareRequest, Range,
};
use crate::ephemeral_host::{self, Entry, staging};
use crate::{CellId, Errno};
use bucket_cell::layout::Origin;
use host_fs::FileExt as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::Status;

/// The marker of a cell given up to a migration, in its directory ([`Marker`]).
pub(crate) const MARKER: &str = "migrating";

/// How much of a file a request carries at most.
const PIECE: usize = 4 << 20;

/// How long a round of the source waits for the next.
const ROUND: Duration = Duration::from_millis(500);

/// What a cell given up to a migration says of it, in its marker: what asking the target again
/// takes, after a restart too.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    migration: u64,
    peer: String,
    incarnation: u64,
}

/// What a source notes beside a cell directory it handed over until the directory and its reference
/// are gone ([`handed_over`]): the cell's journal entry, if it has one, whose reference goes.
#[derive(Debug, Serialize, Deserialize)]
struct HandedOver {
    migration: u64,
    entry: Option<Entry>,
}

/// Where a source notes it handed the cell at `path` over ([`HandedOver`]): beside it, as a
/// target stages a migration's files ([`staging`]).
fn handed_over(path: &Path) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.handed-over"))
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

impl Outgoing {
    fn new(
        migration: u64,
        peer: String,
        incarnation: u64,
        client: PeerClient<tonic::transport::Channel>,
    ) -> Self {
        let sent = tokio::sync::Mutex::default();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        Outgoing { migration, peer, incarnation, client, sent, stopped }
    }

    /// The migration a cell's marker names, asked again with nothing known sent, as after a
    /// restart.
    fn resumed(marker: Marker) -> Result<Self, Errno> {
        let client = client(&marker.peer)?;
        let resumed = Self::new(marker.migration, marker.peer, marker.incarnation, client);
        resumed.stopped.store(true, std::sync::atomic::Ordering::Release);
        Ok(resumed)
    }

    fn marker(&self) -> Marker {
        Marker { migration: self.migration, peer: self.peer.clone(), incarnation: self.incarnation }
    }
}

node_local::node_local! {
    /// The migrations from this host under way, by their cells' paths.
    static OUTGOING: Mutex<BTreeMap<PathBuf, Arc<Outgoing>>> = Mutex::new(BTreeMap::new());
}

/// The status of `errno`, for a peer.
fn status(what: impl std::fmt::Display, err: impl std::fmt::Display) -> Status {
    Status::internal(format!("{what}: {err:#}"))
}

/// A client of the peer at `address`.
fn client(address: &str) -> Result<PeerClient<tonic::transport::Channel>, Errno> {
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    super::peers::member(&runtime, address).map_err(|err| {
        log::warn!("cladefsd: migrating to {address}: {err}");
        Errno::EINVAL
    })
}

/// Begins the migration of the ephemeral cell at `path` to the daemon whose peer
/// address is `peer`, its cell going at `to` there (`MigrateCell`): prepared there, then rounds
/// sending its kept segments as they come, until its hand-over. Returns the migration's ID.
pub(super) async fn migrate(path: PathBuf, peer: String, to: String) -> Result<u64, Errno> {
    let host = ephemeral_host::host()?;
    let read = path.clone();
    let (checkpoint, cell) = super::cells::blocking(move || {
        // Given up to a migration already: that one's to hand over first.
        if host_fs::exists(read.join(MARKER)) {
            return Err(Errno::EBUSY);
        }
        let checkpoint = local_cell::host_local_checkpoint(&read).map_err(|_| Errno::ENOENT)?;
        let cell = local_cell::host_cell(&read).map_err(|_| Errno::ENOENT)?;
        Ok((checkpoint.ok_or(Errno::EINVAL)?, cell))
    })
    .await?;
    if OUTGOING.get().lock().unwrap().contains_key(&path) {
        return Err(Errno::EBUSY);
    }
    let entry = host.entry(cell).ok_or(Errno::ENOENT)?;
    let parent = checkpoint.parent();
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
    let prepared = match client.migrate_prepare(prepare).await {
        Ok(prepared) => prepared.into_inner(),
        Err(refused) => {
            log::warn!("cladefsd: preparing migration {migration:x} at {peer}: {refused}");
            // Prepared there or not, its answer lost: aborted by ID, as this host pursues it no
            // more. Not delivered, the target gives it up as it starts, or at the cell's next
            // prepare there (specs/ephemeral, `SourceLoses`, `supersedes`).
            let abort = MigrateAbortRequest { migration };
            if let Err(err) = client.migrate_abort(abort).await {
                log::info!("cladefsd: aborting migration {migration:x} at {peer}: {err}");
            }
            return Err(Errno::ENOTCONN);
        }
    };
    let outgoing = Arc::new(Outgoing::new(migration, peer, prepared.incarnation, client));
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

/// A round of `outgoing`: the kept segments of the cell at `path` not sent yet, each whole.
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

/// The directories of the cell at `path`, by their paths in it: those its files are in, and those
/// it keeps empty, which its opening lists.
fn dirs(path: &Path) -> std::io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        for entry in host_fs::read_dir(path.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if entry.metadata()?.is_dir() && relative != "replica-spool" {
                found.push(relative.clone());
                pending.push(relative);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The files of the cell at `path`, by their paths in it, with their lengths: all but its marker
/// and what deleting or replicating leaves.
fn files(path: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut found = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in host_fs::read_dir(path.join(&dir))? {
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

/// Sends the file at `relative` in the cell at `path`, `len` bytes long, whole, its data and its
/// holes, the last piece carrying the holes, so that it lands as it is here over any copy sent
/// before: returns the bytes it takes of the disk.
async fn send(outgoing: &Outgoing, path: &Path, relative: &str, len: u64) -> Result<u64, Status> {
    let file = host_fs::File::open(path.join(relative)).map_err(|err| status(relative, err))?;
    let (data, allocated) = layout_of(&file, len).map_err(|err| status(relative, err))?;
    let holes = gaps(&data, len);
    let mut client = outgoing.client.clone();
    let mut pieces = Vec::new();
    for &(start, end) in &data {
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
    let last = pieces.len() - 1;
    for (piece, (offset, bytes)) in pieces.into_iter().enumerate() {
        let request = MigrateFileRequest {
            migration: outgoing.migration,
            path: relative.to_owned(),
            offset,
            bytes,
            len,
            holes: if piece == last { holes.clone() } else { Vec::new() },
        };
        client.migrate_file(request).await?;
    }
    Ok(allocated)
}

/// The ranges of data of `file`, `len` bytes long, by its holes, and the bytes it takes of the
/// disk: as `lseek`'s `SEEK_DATA` and `SEEK_HOLE` find them.
fn layout_of(file: &host_fs::File, len: u64) -> std::io::Result<(Vec<(u64, u64)>, u64)> {
    use host_fs::MetadataExt as _;
    let allocated = file.metadata()?.blocks() * 512;
    let data = host_fs::data_ranges(file, len)?;
    Ok((data.into_iter().map(|range| (range.start, range.end)).collect(), allocated))
}

/// The holes of a file `len` bytes long whose ranges of data are `data`: what lies between them.
fn gaps(data: &[(u64, u64)], len: u64) -> Vec<Range> {
    let mut holes = Vec::new();
    let mut at = 0;
    for &(start, end) in data.iter().chain([&(len, len)]) {
        if start > at {
            holes.push(Range { offset: at, len: start - at });
        }
        at = end;
    }
    holes
}

/// The CRC32C of the file at `path`.
fn crc_of(path: &Path) -> std::io::Result<u32> {
    let mut file = host_fs::File::open(path)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(extent_frame::crc(&bytes))
}

/// Hands the ephemeral cell at `path` over to the target of its migration
/// (`HandOverCell`), once nothing has it open here (`EBUSY` otherwise): see the module's docs.
/// `EAGAIN` if the target refused, the cell served here again; `ENOTCONN` if it did not answer,
/// the cell given up still, to be asked again; `EIO` if letting the cell go failed once the target
/// took it, which asking again finishes.
pub(super) async fn hand_over(path: PathBuf) -> Result<(), Errno> {
    let host = ephemeral_host::host()?;
    let _held = super::cells::hold_cell_alone(&path).await?;
    // Handed over before, and asked again: what is left of letting it go.
    let handed = handed_over(&path);
    if let Ok(bytes) = host_fs::read(&handed) {
        let noted: HandedOver = serde_json::from_slice(&bytes).map_err(|err| {
            log::error!("cladefsd: {}: {err}", handed.display());
            Errno::EIO
        })?;
        log::info!("cladefsd: migration {:x} handed over: letting it go", noted.migration);
        return let_go(&host, &path, &handed, &noted).await;
    }
    let marker_path = path.join(MARKER);
    let marked: Option<Marker> =
        host_fs::read(&marker_path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let under_way = OUTGOING.get().lock().unwrap().get(&path).cloned();
    // Held alone, the cell changes no more: rounds are over.
    if let Some(under_way) = &under_way {
        under_way.stopped.store(true, std::sync::atomic::Ordering::Release);
    }
    let outgoing = match (marked, under_way) {
        (None, Some(outgoing)) => {
            let bytes = serde_json::to_vec(&outgoing.marker()).map_err(|_| Errno::EIO)?;
            write_durably(&marker_path, &bytes).map_err(|err| {
                log::warn!("cladefsd: giving {} up: {err}", path.display());
                Errno::EIO
            })?;
            outgoing
        }
        // Given up before: asked again, the marker naming the migration.
        (Some(marker), Some(outgoing)) if outgoing.migration == marker.migration => outgoing,
        (Some(marker), _) => Arc::new(Outgoing::resumed(marker)?),
        (None, None) => return Err(Errno::ENOENT),
    };
    let migration = outgoing.migration;
    // What the target lacks, sent again however often asked; staged there no more, the target
    // accepted the migration or gave it up, as its answer says.
    match finish(&outgoing, &path).await {
        Ok(()) => {}
        Err(gone) if gone.code() == tonic::Code::NotFound => {
            log::info!("cladefsd: migration {migration:x} staged no more: {}", gone.message());
        }
        Err(err) => {
            log::warn!("cladefsd: migration {migration:x}'s last files: {err}");
            return Err(Errno::ENOTCONN);
        }
    }
    // The hand-over names every file of the cell, so that the target takes it only whole
    // (`openAtNamed`).
    let files = named(&path).map_err(|err| {
        log::warn!("cladefsd: naming {}'s files: {err}", path.display());
        Errno::EIO
    })?;
    let request = MigrateHandOverRequest { migration, incarnation: outgoing.incarnation, files };
    let accepted = match outgoing.client.clone().migrate_hand_over(request).await {
        Ok(answer) => answer.into_inner().accepted,
        Err(unanswered) => {
            log::warn!("cladefsd: migration {migration:x}'s hand-over: {unanswered}");
            return Err(Errno::ENOTCONN);
        }
    };
    OUTGOING.get().lock().unwrap().remove(&path);
    if !accepted {
        // Served here again: durably, before any opening, so that no crash brings the marker
        // back, a later migration then asking for this one's hand-over.
        remove_durably(&marker_path).map_err(|err| {
            log::warn!("cladefsd: serving {} again: {err}", path.display());
            Errno::EIO
        })?;
        return Err(Errno::EAGAIN);
    }
    // Accepted: noted so, durably, then let go of, as asking again finds it noted (board 5665).
    let cell = local_cell::host_cell(&path).map_err(|_| Errno::EIO)?;
    let noted = HandedOver { migration, entry: host.entry(cell) };
    let bytes = serde_json::to_vec(&noted).map_err(|_| Errno::EIO)?;
    write_durably(&handed, &bytes).map_err(|err| {
        log::warn!("cladefsd: noting {} handed over: {err}", path.display());
        Errno::EIO
    })?;
    let_go(&host, &path, &handed, &noted).await
}

/// Lets go of the cell at `path`, handed over as `noted` says, noted at `handed`: its cell
/// directory goes, then this host's reference (`sourceRetiresLast`), then the note. `EIO` if the
/// directory or the note does not go, the note left, from which asking again goes on. The note
/// gone, its directory's sync failing answers `Ok` all the same (board 5721): the cell is handed
/// over and let go of, and asking again would find nothing to go on from (`ENOENT`); a crash may
/// bring the note back, of a cell directory gone and a reference retired.
async fn let_go(
    host: &ephemeral_host::Host,
    path: &Path,
    handed: &Path,
    noted: &HandedOver,
) -> Result<(), Errno> {
    local_cell::remove_cell_dir(path).map_err(|err| {
        log::warn!("cladefsd: removing {}, handed over: {err:#}", path.display());
        Errno::EIO
    })?;
    if let Some(entry) = &noted.entry
        && let Err(err) = host.retire(entry).await
    {
        log::warn!("cladefsd: retiring {}'s reference, handed over: {err:#}", entry.child);
    }
    host_fs::remove_file(handed).map_err(|err| {
        log::warn!("cladefsd: {}, let go of: {err}", handed.display());
        Errno::EIO
    })?;
    if let Err(err) = host_fs::sync_dir(handed.parent().unwrap_or(Path::new("."))) {
        log::warn!("cladefsd: {} let go of, not durably: {err}", handed.display());
    }
    Ok(())
}

/// Sends what `outgoing` has not sent of the cell at `path`, given up: its directories; the kept
/// segments not sent, and the holes of those sent that gave back more since; and every other file,
/// whole.
async fn finish(outgoing: &Outgoing, path: &Path) -> Result<(), Status> {
    let mut sent = outgoing.sent.lock().await;
    // Its directories first, the empty ones among them: a directory's path ends with `/`.
    for dir in dirs(path).map_err(|err| status("listing", err))? {
        let request = MigrateFileRequest {
            migration: outgoing.migration,
            path: format!("{dir}/"),
            offset: 0,
            bytes: Vec::new(),
            len: 0,
            holes: Vec::new(),
        };
        outgoing.client.clone().migrate_file(request).await?;
    }
    for (relative, len) in files(path).map_err(|err| status("listing", err))? {
        match sent.get(&relative) {
            Some(&(was, allocated)) if relative.starts_with("kept/") && was == len => {
                let file =
                    host_fs::File::open(path.join(&relative)).map_err(|e| status(&relative, e))?;
                let (data, now) = layout_of(&file, len).map_err(|err| status(&relative, err))?;
                if now != allocated {
                    let request = MigrateFileRequest {
                        migration: outgoing.migration,
                        path: relative.clone(),
                        offset: 0,
                        bytes: Vec::new(),
                        len,
                        holes: gaps(&data, len),
                    };
                    outgoing.client.clone().migrate_file(request).await?;
                    sent.insert(relative, (len, now));
                }
            }
            _ => {
                let allocated = send(outgoing, path, &relative, len).await?;
                sent.insert(relative, (len, allocated));
            }
        }
    }
    Ok(())
}

/// Every file of the cell at `path`, given up, as its hand-over names them: its length, and the
/// CRC of each but the kept segments'.
fn named(path: &Path) -> std::io::Result<Vec<FileDigest>> {
    let mut named = Vec::new();
    for (relative, len) in files(path)? {
        let crc = match relative.starts_with("kept/") {
            true => None,
            false => Some(crc_of(&path.join(&relative))?),
        };
        named.push(FileDigest { path: relative, len, crc });
    }
    Ok(named)
}

/// Removes the file at `path`, durably.
fn remove_durably(path: &Path) -> std::io::Result<()> {
    host_fs::remove_file(path)?;
    host_fs::sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Writes `bytes` to a file at `path`, whole, durably.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file = host_fs::File::create(path)?;
    file.write_all_at(bytes, 0)?;
    file.sync_all()?;
    host_fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
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
    /// before anything is taken, then its staging directory made. Asked again, it prepares again,
    /// each step as it did. An older migration of the cell it is taking, never accepted, it gives
    /// up first: its source, the cell's writer, preparing this one, pursues that one no more, its
    /// answer lost or its daemon restarted since (specs/ephemeral, `supersedes`).
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
            None => {}
            Some(found) if found == entry => {}
            Some(found) if found.migration.is_some_and(|older| !host.accepted(older)) => {
                supersede(&host, &found, &entry).await?;
            }
            Some(_) => return Err(Status::already_exists(format!("cell {child} is here"))),
        }
        if host_fs::exists(&path) {
            return Err(Status::already_exists(format!("{} is there", path.display())));
        }
        host.note(&entry).map_err(|err| status("noting the cell", err))?;
        let bucket =
            crate::host_cell::bucket(&entry.bucket).map_err(|err| status("its bucket", err))?;
        bucket_cell::lifecycle::refer_ephemeral(&*bucket, origin, host.incarnation(), child).await;
        host_fs::create_dir_all(staging(&path, migration)).map_err(|err| status("staging", err))?;
        log::info!("cladefsd: migration {migration:x} of {child} prepared, to {}", path.display());
        Ok(host.incarnation())
    }

    /// Gives up the older migration of `found`, never accepted, for `entry`'s: what it staged
    /// dropped, and its reference too unless `entry`'s is the same.
    async fn supersede(
        host: &ephemeral_host::Host,
        found: &Entry,
        entry: &Entry,
    ) -> Result<(), Status> {
        let older = found.migration.expect("an entry being taken");
        log::info!("cladefsd: migration {older:x} of {} superseded", found.child);
        drop_staged(&staging(&found.path, older)).map_err(|err| status("dropping it", err))?;
        if (found.origin, &found.bucket) != (entry.origin, &entry.bucket) {
            host.retire(found).await.map_err(|err| status("retiring its reference", err))?;
        }
        Ok(())
    }

    /// Removes the directory `staged`, if there, durably: no crash brings it back.
    fn drop_staged(staged: &Path) -> std::io::Result<()> {
        if !host_fs::exists(staged) {
            return Ok(());
        }
        host_fs::remove_dir_all(staged)?;
        host_fs::sync_dir(staged.parent().unwrap_or(Path::new(".")))
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
        // Dropped, as the migration was given up as this request came: not made again.
        if !host_fs::is_dir(&staged) {
            return Err(Status::not_found(format!("migration {migration:x} given up")));
        }
        // A directory, empty or not, its path ending with `/`.
        let left = || Status::invalid_argument(format!("{path:?} leaves the cell"));
        if let Some(dir) = path.strip_suffix('/') {
            let dir = within(&staged, dir).ok_or_else(left)?;
            return host_fs::create_dir_all(dir).map_err(|err| status(&path, err));
        }
        let file_path = within(&staged, &path).ok_or_else(left)?;
        if let Some(dir) = file_path.parent() {
            host_fs::create_dir_all(dir).map_err(|err| status(&path, err))?;
        }
        let file = host_fs::OpenOptions::new()
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
        let (entry, staged) = match staged(&host, migration) {
            Ok(staged) => staged,
            Err(gone) => {
                log::warn!("cladefsd: refusing migration {migration:x}: {}", gone.message());
                return Ok(false);
            }
        };
        let lacking = match incarnation == host.incarnation() {
            true => lacking(&staged, &files),
            false => Some(format!("asked of incarnation {incarnation:x}")),
        };
        if let Some(lacking) = lacking {
            log::warn!("cladefsd: refusing migration {migration:x}: {lacking}");
            refuse(&host, &entry, &staged).await?;
            return Ok(false);
        }
        let moved = || -> std::io::Result<()> {
            sync_tree(&staged)?;
            host_fs::rename(&staged, &entry.path)?;
            host_fs::File::open(entry.path.parent().unwrap_or(Path::new(".")))?.sync_all()
        };
        moved().map_err(|err| status("taking the cell", err))?;
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
        drop_staged(staged).map_err(|err| status("dropping it", err))?;
        host.retire(entry).await.map_err(|err| status("retiring its reference", err))
    }

    /// What `staged` lacks of `files`, each of its length, of its CRC if named: the first file it
    /// lacks so, if any. A cell has files: a hand-over naming none names no cell.
    fn lacking(staged: &Path, files: &[FileDigest]) -> Option<String> {
        if files.is_empty() {
            return Some("the hand-over names no file".to_owned());
        }
        files.iter().find_map(|FileDigest { path, len, crc }| {
            let Some(at) = within(staged, path) else {
                return Some(format!("{path:?} leaves the cell"));
            };
            let found = host_fs::metadata(&at).map(|metadata| metadata.len());
            if found.as_ref().ok() != Some(len) {
                return Some(format!("{path}: {found:?} bytes, not {len}"));
            }
            let found = crc.and_then(|_| crc_of(&at).ok());
            (crc.is_some() && found != *crc).then(|| format!("{path}: CRC {found:?}, not {crc:?}"))
        })
    }

    /// cladefs: the path in `staged` a peer's `path` names, if it names one in it: of names, each
    /// neither empty nor `.` nor `..`, joined by `/` (board 5374). A peer's paths are untrusted
    /// (AGENTS.md, Trust boundaries): an absolute one joined replaced `staged`, and one through
    /// `..` left it, which every use of a peer's path checks here, never by itself.
    fn within(staged: &Path, path: &str) -> Option<PathBuf> {
        let normal = |name: &str| !matches!(name, "" | "." | "..");
        (!path.is_empty() && path.split('/').all(normal)).then(|| staged.join(path))
    }

    /// Syncs every file and directory under `dir`, and it.
    fn sync_tree(dir: &Path) -> std::io::Result<()> {
        for entry in host_fs::read_dir(dir)? {
            let path = entry?.path();
            if host_fs::is_dir(&path) {
                sync_tree(&path)?;
            } else {
                host_fs::File::open(&path)?.sync_all()?;
            }
        }
        host_fs::File::open(dir)?.sync_all()
    }

    /// Gives back the `len` bytes at `offset` of `file`, keeping its size.
    fn punch(file: &host_fs::File, offset: u64, len: u64) -> std::io::Result<()> {
        host_fs::punch_hole(file, offset, len)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Board 5374: a peer's path names a file in the staging directory only as names joined by
        /// `/`; an absolute one, one through `..` or `.`, an empty one or one with an empty name, none.
        #[test]
        fn peers_paths_stay_in_their_staging_directory() {
            let staged = Path::new("/cells/.cell.migrating-1");
            assert_eq!(within(staged, "segments/a.0"), Some(staged.join("segments/a.0")));
            for path in ["", "/etc/passwd", "../x", "a/../../x", "a//b", "./a", "a/.", "a/"] {
                assert_eq!(within(staged, path), None, "{path:?}");
            }
        }

        /// Board 5374: a hand-over naming a file outside its staging directory lacks it, as named,
        /// rather than reading what is there: `/dev/zero` of 0 bytes, read to its end, had no end.
        #[test]
        fn hand_overs_naming_files_outside_lack_them() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("kept"), b"x").unwrap();
            let digest = |path: &str, len| FileDigest { path: path.to_owned(), len, crc: None };
            assert_eq!(lacking(dir.path(), &[digest("kept", 1)]), None);
            assert_eq!(
                lacking(dir.path(), &[digest("kept", 1), digest("/dev/zero", 0)]),
                Some("\"/dev/zero\" leaves the cell".to_owned())
            );
        }
    }
}
