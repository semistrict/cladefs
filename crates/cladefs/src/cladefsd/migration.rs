// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: live migration of an ephemeral cell from this host, the source, to another, the target
//! (backlog doc-25, TASK-698.4; specs/ephemeral). An ephemeral cell is files: its local checkpoint,
//! its kept segments, which never change once kept but for holes given back, and the rest, its raw
//! segments after the local checkpoint, their logs, its description and its write-ahead log's
//! start. A migration moves them, its rules as the spec names them:
//!
//! 1. **Prepare** (`MigrateCell`): the source notes the cell's next epoch, durably, then asks the
//!    target, again with the same migration while its answer is lost (board 5734). The target
//!    notes the cell in its journal and writes its own reference to the cell's origin before it
//!    takes anything (`targetRefersFirst`), and answers its incarnation and a token new to this
//!    take of the migration. Taking an older migration of the cell, by its epoch, never accepted,
//!    it gives that one up first (`supersedes`): the cell's writer, preparing this one, pursues it
//!    no more; a prepare older than the migration it takes, arriving late, it refuses (board
//!    5732). A source whose prepare went unanswered aborts that migration by its ID
//!    (`SourceLoses`).
//! 2. **Rounds**, while the guest runs: the kept segments not sent yet, each whole, sparse; and
//!    word that the migration goes on, as a target gives up one it hears nothing of for
//!    `CLADEFSD_MIGRATION_IDLE` (board 5734; `TargetAbort`).
//! 3. **Hand-over** (`HandOverCell`), once nothing has the cell open here, its VM process gone: the
//!    source gives the cell up first, durably, a marker in its cell refusing every opening from
//!    then on (`sourceGivesUpFirst`); then it sends the rest, the holes its kept segments gave back
//!    since they were sent among it; then it asks the target to take the cell over, naming the
//!    incarnation and token its prepare's answer gave (`boundToPrepare`), and every file and its
//!    length, and the CRC of each but the kept segments'. The target takes it only holding them
//!    all so (`openAtNamed`), remembering the migration accepted (`remembersAccepted`); refusing,
//!    it drops what it took and its reference, and never takes that migration after
//!    (`refuseFinal`). Another incarnation of the target's, which knows nothing of it, answers
//!    neither.
//! 4. Accepted, the source notes so in the cell's journal entry, durably, then removes its cell,
//!    tells the target it may forget it accepted the migration, and retires its own reference
//!    and the entry (`sourceRetiresLast`), so that asking again, or the daemon starting again,
//!    finishes them (board 5730); refused, it removes its marker, durably, and serves the cell
//!    again. Not answered, the marker stays, the cell unopened here, and asking again, after a
//!    restart too, sends again what the target lacks and asks it again, naming every file as
//!    before; the target answers as it did.
//!
//! The target serves one request of a cell's migrations at a time, each run to its end however
//! its asker goes, its bucket calls bounded (board 5732).

use super::peers::proto::peer_client::PeerClient;
use super::peers::proto::{
    FileDigest, MigrateAbortRequest, MigrateFileRequest, MigrateHandOverRequest,
    MigratePrepareRequest, MigratePrepareResponse, MigrateReleaseRequest, MigrateRenewRequest,
    Range,
};
use crate::ephemeral_host::{self, Entry, Migrating, staging};
use crate::{CellId, Errno};
use bucket_cell::layout::Origin;
use host_fs::FileExt as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::Status;

/// The marker of a cell given up to a migration, in its directory ([`Marker`]).
pub(crate) const MARKER: &str = "migrating";

/// Where the marker is staged as it is written ([`write_durably`]), which a failure or a crash may
/// leave, as no part of the cell.
const MARKER_STAGED: &str = "migrating.new";

/// How much of a file a request carries at most.
const PIECE: usize = 4 << 20;

/// How long a round of the source waits for the next.
const ROUND: Duration = Duration::from_millis(500);

/// How often a source's rounds tell its target the migration goes on: well within any
/// `CLADEFSD_MIGRATION_IDLE` (board 5734).
const RENEW: Duration = Duration::from_secs(1);

/// How long a target keeps a migration it hears nothing of, unless `CLADEFSD_MIGRATION_IDLE` says
/// otherwise (board 5734): a source down this long finds its hand-over refused, and serves again.
const IDLE: Duration = Duration::from_secs(300);

/// How long a target's call to the bucket, serving a migration's request, waits at most: past it,
/// the request fails, what it noted undone (board 5732).
const BUCKET_BOUND: Duration = Duration::from_secs(30);

/// How long a source waits for each try of its prepare, the target's bucket calls within.
const PREPARE_BOUND: Duration = Duration::from_secs(45);

/// How often a source tries its prepare, its answer lost, before it gives the migration up.
const PREPARE_TRIES: u32 = 3;

/// What a cell given up to a migration says of it, in its marker: what asking the target again
/// takes, after a restart too.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    migration: u64,
    peer: String,
    incarnation: u64,
    token: u64,
}

/// A migration from this host under way: its target, its prepare's answer, and what of each kept
/// segment it sent, its length and the bytes it took of the disk then.
struct Outgoing {
    migration: u64,
    peer: String,
    incarnation: u64,
    token: u64,
    client: PeerClient<tonic::transport::Channel>,
    sent: tokio::sync::Mutex<HashMap<String, (u64, u64)>>,
    stopped: std::sync::atomic::AtomicBool,
}

impl Outgoing {
    fn new(
        Marker { migration, peer, incarnation, token }: Marker,
        client: PeerClient<tonic::transport::Channel>,
    ) -> Self {
        let sent = tokio::sync::Mutex::default();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        Outgoing { migration, peer, incarnation, token, client, sent, stopped }
    }

    /// The migration a cell's marker names, asked again with nothing known sent, as after a
    /// restart.
    fn resumed(marker: Marker) -> Result<Self, Errno> {
        let client = client(&marker.peer)?;
        let resumed = Self::new(marker, client);
        resumed.stopped.store(true, std::sync::atomic::Ordering::Release);
        Ok(resumed)
    }

    fn marker(&self) -> Marker {
        let (migration, peer) = (self.migration, self.peer.clone());
        Marker { migration, peer, incarnation: self.incarnation, token: self.token }
    }
}

/// A migration from this host of a cell: being prepared, or under way.
enum Underway {
    Preparing,
    Moving(Arc<Outgoing>),
}

node_local::node_local! {
    /// The migrations from this host, by their cells' paths.
    static OUTGOING: Mutex<BTreeMap<PathBuf, Underway>> = Mutex::new(BTreeMap::new());
}

/// The path of a cell whose migration is being prepared, held in [`OUTGOING`] from before the
/// prepare is asked, so that two migrations of one cell at once never both prepare (board 5732):
/// let go unless the migration begins.
struct Reserved(PathBuf);

impl Reserved {
    /// The path of the cell at `path` reserved: `EBUSY` if a migration of it is under way.
    fn new(path: &Path) -> Result<Self, Errno> {
        let mut outgoing = OUTGOING.get().lock().unwrap();
        if outgoing.contains_key(path) {
            return Err(Errno::EBUSY);
        }
        outgoing.insert(path.to_owned(), Underway::Preparing);
        Ok(Reserved(path.to_owned()))
    }

    /// The migration under way, `outgoing`: the cell's from now on.
    fn begin(self, outgoing: Arc<Outgoing>) {
        OUTGOING.get().lock().unwrap().insert(self.0.clone(), Underway::Moving(outgoing));
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let mut outgoing = OUTGOING.get().lock().unwrap();
        if let Some(Underway::Preparing) = outgoing.get(&self.0) {
            outgoing.remove(&self.0);
        }
    }
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

/// How long this host, a target, keeps a migration it hears nothing of: `CLADEFSD_MIGRATION_IDLE`
/// seconds, five minutes by default (board 5734).
pub(super) fn idle() -> Result<Duration, String> {
    let Some(set) = node_local::env::var_os("CLADEFSD_MIGRATION_IDLE") else { return Ok(IDLE) };
    match set.to_str().and_then(|seconds| seconds.parse::<u64>().ok()) {
        Some(0) | None => {
            Err(format!("CLADEFSD_MIGRATION_IDLE must be a positive number of seconds: {set:?}"))
        }
        Some(seconds) => Ok(Duration::from_secs(seconds)),
    }
}

/// Begins the migration of the ephemeral cell at `path` to the daemon whose peer
/// address is `peer`, its cell going at `to` there (`MigrateCell`): prepared there, then rounds
/// sending its kept segments as they come, until its hand-over. Returns the migration's ID.
pub(super) async fn migrate(path: PathBuf, peer: String, to: String) -> Result<u64, Errno> {
    let host = ephemeral_host::host()?;
    let reserved = Reserved::new(&path)?;
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
    let entry = host.entry(cell).ok_or(Errno::ENOENT)?;
    // Taken here by a migration not yet accepted, or handed over and being let go of.
    if entry.migrating.is_some() {
        return Err(Errno::EBUSY);
    }
    // The cell's next epoch, noted before any target hears of it: newer than every migration of
    // the cell before, this host's restarted too.
    let entry = Entry { epoch: entry.epoch + 1, ..entry };
    host.note(&entry).map_err(|err| {
        log::warn!("cladefsd: noting {}'s epoch: {err:#}", entry.child);
        Errno::EIO
    })?;
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
        epoch: entry.epoch,
    };
    let MigratePrepareResponse { incarnation, token } =
        match prepare_retrying(&mut client, prepare).await {
            Ok(prepared) => prepared,
            Err(refused) => {
                log::warn!("cladefsd: preparing migration {migration:x} at {peer}: {refused}");
                // Prepared there or not, its answer lost: aborted by ID, as this host pursues it
                // no more. Not delivered, the target gives it up as it starts, idle, or at the
                // cell's next prepare there (specs/ephemeral, `SourceLoses`, `TargetAbort`,
                // `supersedes`).
                let abort = MigrateAbortRequest { migration };
                if let Err(err) = client.migrate_abort(abort).await {
                    log::info!("cladefsd: aborting migration {migration:x} at {peer}: {err}");
                }
                return Err(Errno::ENOTCONN);
            }
        };
    let marker = Marker { migration, peer, incarnation, token };
    let outgoing = Arc::new(Outgoing::new(marker, client));
    reserved.begin(outgoing.clone());
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    runtime.spawn(async move {
        let mut renewed = tokio::time::Instant::now();
        while !outgoing.stopped.load(std::sync::atomic::Ordering::Acquire) {
            if let Err(err) = round(&outgoing, &path).await {
                log::warn!("cladefsd: migration {:x}'s round: {err}", outgoing.migration);
            }
            if renewed.elapsed() >= RENEW {
                renew(&outgoing).await;
                renewed = tokio::time::Instant::now();
            }
            tokio::time::sleep(ROUND).await;
        }
    });
    Ok(migration)
}

/// Asks a target to prepare a migration, `prepare`, again with the same migration while its answer
/// is lost, [`PREPARE_TRIES`] times at most (board 5734): the target's answer, or the last
/// failure.
async fn prepare_retrying(
    client: &mut PeerClient<tonic::transport::Channel>,
    prepare: MigratePrepareRequest,
) -> Result<MigratePrepareResponse, Status> {
    let mut tries = 0;
    loop {
        tries += 1;
        let asked = tokio::time::timeout(PREPARE_BOUND, client.migrate_prepare(prepare.clone()));
        let answered = asked.await.unwrap_or_else(|_| {
            Err(Status::deadline_exceeded(format!("no answer within {PREPARE_BOUND:?}")))
        });
        match answered {
            Ok(prepared) => return Ok(prepared.into_inner()),
            Err(lost) if tries < PREPARE_TRIES && unanswered(&lost) => {
                log::info!("cladefsd: migration {:x}'s prepare, again: {lost}", prepare.migration);
                tokio::time::sleep(ROUND * tries).await;
            }
            Err(failed) => return Err(failed),
        }
    }
}

/// Whether `status` says a request went unanswered, rather than refused: worth asking again.
fn unanswered(status: &Status) -> bool {
    use tonic::Code;
    matches!(
        status.code(),
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown
    )
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

/// Tells `outgoing`'s target the migration goes on (board 5734). Given up there, its rounds stop:
/// its hand-over will be refused, the cell served here again.
async fn renew(outgoing: &Outgoing) {
    let renewal = MigrateRenewRequest { migration: outgoing.migration };
    match outgoing.client.clone().migrate_renew(renewal).await {
        Ok(_) => {}
        Err(gone) if gone.code() == tonic::Code::NotFound => {
            log::warn!("cladefsd: migration {:x} given up by its target", outgoing.migration);
            outgoing.stopped.store(true, std::sync::atomic::Ordering::Release);
        }
        Err(err) => log::info!("cladefsd: renewing migration {:x}: {err}", outgoing.migration),
    }
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

/// The files of the cell at `path`, by their paths in it, with their lengths: all but its marker,
/// staged or not, and what deleting or replicating leaves.
fn files(path: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut found = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in host_fs::read_dir(path.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if [MARKER, MARKER_STAGED, "replica-spool", "store.deleted"]
                .contains(&relative.as_str())
            {
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
    // Handed over before, and asked again: what is left of letting it go (board 5730).
    let handed = host.handed_over(&path).map_err(|err| {
        log::error!("cladefsd: the journal: {err:#}");
        Errno::EIO
    })?;
    if !handed.is_empty() {
        for entry in &handed {
            log::info!("cladefsd: {} handed over: letting it go", entry.child);
            let_go(&host, entry).await?;
        }
        return Ok(());
    }
    let marker_path = path.join(MARKER);
    let marked: Option<Marker> =
        host_fs::read(&marker_path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let under_way = match OUTGOING.get().lock().unwrap().get(&path) {
        // Its prepare not answered yet: nothing to hand over.
        Some(Underway::Preparing) => return Err(Errno::EBUSY),
        Some(Underway::Moving(outgoing)) => Some(outgoing.clone()),
        None => None,
    };
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
        // Under way again, after a restart too, until its marker goes durably (board 5721).
        (Some(marker), _) => {
            let resumed = Arc::new(Outgoing::resumed(marker)?);
            let under_way = Underway::Moving(resumed.clone());
            OUTGOING.get().lock().unwrap().insert(path.clone(), under_way);
            resumed
        }
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
    // (`openAtNamed`), and the take its prepare answered (`boundToPrepare`).
    let files = named(&path).map_err(|err| {
        log::warn!("cladefsd: naming {}'s files: {err}", path.display());
        Errno::EIO
    })?;
    let (incarnation, token) = (outgoing.incarnation, outgoing.token);
    let request = MigrateHandOverRequest { migration, incarnation, files, token };
    let accepted = match outgoing.client.clone().migrate_hand_over(request).await {
        Ok(answer) => answer.into_inner().accepted,
        Err(unanswered) => {
            log::warn!("cladefsd: migration {migration:x}'s hand-over: {unanswered}");
            return Err(Errno::ENOTCONN);
        }
    };
    if !accepted {
        // Served here again: durably, before any opening, so that no crash brings the marker
        // back, a later migration then asking for this one's hand-over. Under way until then
        // (board 5721): its marker unlinked, its directory's sync failing, asking again finds the
        // migration under way, writes its marker again, is refused again, and removes it again,
        // where it would find neither and answer ENOENT.
        remove_durably(&marker_path).map_err(|err| {
            log::warn!("cladefsd: serving {} again: {err}", path.display());
            Errno::EIO
        })?;
        OUTGOING.get().lock().unwrap().remove(&path);
        return Err(Errno::EAGAIN);
    }
    OUTGOING.get().lock().unwrap().remove(&path);
    // Accepted: noted so in the cell's entry, durably, then let go of, as asking again, or the
    // daemon starting again, finds it noted (board 5730).
    let cell = local_cell::host_cell(&path).map_err(|_| Errno::EIO)?;
    let entry = host.entry(cell).ok_or(Errno::EIO)?;
    let peer = outgoing.peer.clone();
    let handed = Entry { migrating: Some(Migrating::HandedOver { id: migration, peer }), ..entry };
    host.note(&handed).map_err(|err| {
        log::warn!("cladefsd: noting {} handed over: {err:#}", path.display());
        Errno::EIO
    })?;
    let_go(&host, &handed).await
}

/// Lets go of the cell `entry` notes handed over: its cell directory goes, if it is the cell's
/// still, then its target is told it may forget it accepted the migration, then this host's
/// reference and the entry go (`sourceRetiresLast`). `EIO` if the directory does not go, the entry
/// kept, from which asking again goes on.
async fn let_go(host: &ephemeral_host::Host, entry: &Entry) -> Result<(), Errno> {
    let Some(Migrating::HandedOver { id: migration, peer }) = &entry.migrating else {
        return Err(Errno::EINVAL);
    };
    let (path, child) = (entry.path.clone(), entry.child);
    super::cells::blocking(move || {
        // The cell, or its removal stopped part way: gone. Another cell put there since, its own.
        let its = match local_cell::host_cell(&path) {
            Ok(cell) => cell == child,
            Err(_) => local_cell::deleting_host(&path),
        };
        if !its {
            return Ok(());
        }
        local_cell::remove_cell_dir(&path).map_err(|err| {
            log::warn!("cladefsd: removing {}, handed over: {err:#}", path.display());
            Errno::EIO
        })
    })
    .await?;
    crate::faults::crashpoint!("migration::let_go");
    release(peer, *migration).await;
    match host.retire(entry).await {
        Ok(()) => Ok(()),
        // The entry gone, its directory's sync failing: let go of all the same (board 5721), as
        // asking again would find nothing to go on from (`ENOENT`). A crash may bring the entry
        // back, which the daemon starting lets go of again, each step again as before.
        Err(err) if host.entry(child).is_none() => {
            log::warn!("cladefsd: {child} let go of, its entry's removal not durable: {err:#}");
            Ok(())
        }
        Err(err) => {
            log::warn!("cladefsd: retiring {child}'s reference, handed over: {err:#}");
            Err(Errno::EIO)
        }
    }
}

/// Tells the target at `peer` it may forget it accepted `migration`, its cell let go of here
/// (board 5734). Unheard, the target keeps remembering it: one empty file.
async fn release(peer: &str, migration: u64) {
    let Ok(mut client) = client(peer) else { return };
    let released = MigrateReleaseRequest { migration };
    let asked = tokio::time::timeout(BUCKET_BOUND, client.migrate_release(released));
    match asked.await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => log::info!("cladefsd: releasing migration {migration:x} at {peer}: {err}"),
        Err(_) => log::info!("cladefsd: releasing migration {migration:x} at {peer}: no answer"),
    }
}

/// Lets go of every cell this host's journal notes handed over, as the daemon starts, before it
/// serves (board 5730): a crash part way through letting one go. Those it fails to it leaves to
/// asking again.
pub(super) async fn let_go_handed_over(host: &ephemeral_host::Host) -> anyhow::Result<usize> {
    let mut let_go_of = 0;
    for entry in host.entries()? {
        if !matches!(entry.migrating, Some(Migrating::HandedOver { .. })) {
            continue;
        }
        match let_go(host, &entry).await {
            Ok(()) => let_go_of += 1,
            Err(err) => log::warn!("cladefsd: letting {} go, handed over: {err}", entry.child),
        }
    }
    Ok(let_go_of)
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

/// Removes the file at `path`, durably: gone already, its directory synced still, so that removing
/// it again finishes a removal whose sync failed.
fn remove_durably(path: &Path) -> std::io::Result<()> {
    match host_fs::remove_file(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        removed => removed?,
    }
    host_fs::sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Writes `bytes` to a file at `path`, whole, durably: staged beside it ([`staged`]), synced, then
/// renamed over it, so that `path` holds them whole or as it was, whatever fails or crashes on the
/// way (board 5721: a marker created and its write failing was left empty, read as none, still
/// refusing the cell's openings). The staged file goes if a step fails.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staged = staged(path);
    let written = (|| {
        let file = host_fs::File::create(&staged)?;
        file.write_all_at(bytes, 0)?;
        file.sync_all()?;
        host_fs::rename(&staged, path)
    })();
    if let Err(err) = written {
        let _ = host_fs::remove_file(&staged);
        return Err(err);
    }
    host_fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

/// Where [`write_durably`] stages what it writes at `path`: beside it, its name and `.new`.
fn staged(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".new");
    path.with_file_name(name)
}

/// The target's side: what its peer server does of a migration to it.
pub(super) mod target {
    use super::*;
    use tokio::time::Instant;

    node_local::node_local! {
        /// The cells migrations are taking here, each held by the request of its migrations
        /// being served ([`Taking`]).
        static TAKING: Mutex<HashMap<CellId, Arc<tokio::sync::Mutex<()>>>> =
            Mutex::new(HashMap::new());
    }

    node_local::node_local! {
        /// When each migration being taken here was last heard of, by its ID (board 5734).
        static HEARD: Mutex<HashMap<u64, Instant>> = Mutex::new(HashMap::new());
    }

    node_local::node_local! {
        /// The migrations this daemon, since it started, prepared to take, the only ones it
        /// takes on with (board 5721): what a migration staged before is written but not synced,
        /// which its host's restart may have lost part of, so that a migration being taken does
        /// not outlive its daemon (specs/ephemeral: `Crash` makes a cell being received none),
        /// whether or not its start, giving it up, could retire its entry.
        static PREPARED: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());
    }

    /// The host's ephemeral cells' directory, for a peer: `FAILED_PRECONDITION` if it keeps none.
    fn host() -> Result<Arc<ephemeral_host::Host>, Status> {
        ephemeral_host::host().map_err(|_| {
            Status::failed_precondition("no ephemeral cells here (CLADEFSD_EPHEMERAL)")
        })
    }

    /// The journal's failure, for a peer.
    fn journal(err: anyhow::Error) -> Status {
        status("the journal", err)
    }

    /// A cell a migration is taking here, held while one request of its migrations is served, so
    /// that each sees the journal entry and staging directory as the one before left them (board
    /// 5732).
    struct Taking {
        child: CellId,
        held: Option<tokio::sync::OwnedMutexGuard<()>>,
    }

    /// Holds `child` ([`Taking`]), once no other request of its migrations is served.
    async fn taking(child: CellId) -> Taking {
        let lock = TAKING.get().lock().unwrap().entry(child).or_default().clone();
        Taking { child, held: Some(lock.lock_owned().await) }
    }

    impl Drop for Taking {
        fn drop(&mut self) {
            self.held = None;
            // Its lock gone unless another request holds it, or waits for it.
            let mut taking = TAKING.get().lock().unwrap();
            if taking.get(&self.child).is_some_and(|lock| Arc::strong_count(lock) == 1) {
                taking.remove(&self.child);
            }
        }
    }

    /// The entry of the cell migration `migration` is taking here, held ([`Taking`]), if one is.
    /// One a daemon before this one prepared it gives up ([`PREPARED`]), its start having failed
    /// to, its disk failing.
    async fn held(
        host: &ephemeral_host::Host,
        migration: u64,
    ) -> Result<Option<(Taking, Entry)>, Status> {
        let Some(entry) = host.receiving(migration).map_err(journal)? else { return Ok(None) };
        let taking = taking(entry.child).await;
        // As the request served before this one left it.
        let Some(entry) = host.receiving(migration).map_err(journal)? else { return Ok(None) };
        if !PREPARED.get().lock().unwrap().contains(&migration) {
            log::warn!("cladefsd: migration {migration:x} prepared by a daemon before: given up");
            refuse(host, &entry).await?;
            return Ok(None);
        }
        Ok(Some((taking, entry)))
    }

    /// Notes, durably, the cell migration `migration`, accepted, took as taken: its entry written
    /// again, unless the cell went on since. A noting that failed, or whose sync did, is made good
    /// so before the acceptance is answered again or forgotten (boards 5721, 5734): forgotten
    /// first, a crash bringing back an entry being taken had this host's start take it for a
    /// migration never accepted, removing the cell, the one copy its source let go of.
    async fn noted_taken(host: &ephemeral_host::Host, migration: u64) -> Result<(), Status> {
        let Some(child) = host.accepted_cell(migration) else { return Ok(()) };
        let _taking = taking(child).await;
        let Some(entry) = host.entry(child) else { return Ok(()) };
        let its = match entry.migrating {
            None => true,
            Some(Migrating::Receiving { id, .. }) => id == migration,
            Some(Migrating::HandedOver { .. }) => false,
        };
        if its {
            let taken = Entry { migrating: None, ..entry };
            host.note(&taken).map_err(|err| status("noting the cell", err))?;
        }
        Ok(())
    }

    /// Notes migration `migration` heard of now (board 5734).
    fn heard(migration: u64) {
        HEARD.get().lock().unwrap().insert(migration, Instant::now());
    }

    /// Runs `work`, a request's, to its end however its asker goes: a request cancelled leaves
    /// none of its steps half taken, each its bucket calls bounded.
    async fn to_the_end<T: Send + 'static>(
        work: impl std::future::Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Status> {
        let runtime = crate::bucket::bucket_runtime().map_err(|err| status("its runtime", err))?;
        let done = runtime.spawn(work).await;
        done.unwrap_or_else(|err| Err(status("serving it", err)))
    }

    /// Prepares the migration `request` names: the cell noted, then its reference written here
    /// before anything is taken, then its staging directory made. Returns this host's incarnation
    /// and the token of its take of it. Asked again, it prepares again, each step as it did,
    /// answering the same. An older migration of the cell it is taking, never accepted, it gives
    /// up first: its source, the cell's writer, preparing this one, pursues that one no more, its
    /// answer lost or its daemon restarted since (specs/ephemeral, `supersedes`); a migration older
    /// than the one it takes, by the cell's epoch, it refuses (`ABORTED`, board 5732).
    pub(crate) async fn prepare(request: MigratePrepareRequest) -> Result<(u64, u64), Status> {
        to_the_end(take(request)).await
    }

    async fn take(request: MigratePrepareRequest) -> Result<(u64, u64), Status> {
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
            epoch,
        } = request;
        let child = CellId { tenant, id: cell };
        let origin = Origin { parent: CellId { tenant: parent_tenant, id: parent_cell }, at };
        let _taking = taking(child).await;
        // Its source has its answer: asked again, late.
        if host.accepted(migration) {
            return Err(Status::already_exists(format!("migration {migration:x} accepted")));
        }
        let path = PathBuf::from(to);
        let found = host.entry(child);
        let prepared = |id: &u64| PREPARED.get().lock().unwrap().contains(id);
        let mut superseded = None;
        let token = match found.as_ref().map(|found| (found, &found.migrating)) {
            None => None,
            Some((_, Some(Migrating::Receiving { id, token })))
                if *id == migration && prepared(id) =>
            {
                Some(*token)
            }
            // Prepared by a daemon before this one, whose staging it trusts not: taken afresh.
            Some((found, Some(Migrating::Receiving { id, .. }))) if *id == migration => {
                host.drop_moved(found).map_err(journal)?;
                None
            }
            Some((found, Some(Migrating::Receiving { id: older, .. }))) if found.epoch < epoch => {
                log::info!("cladefsd: migration {older:x} of {child} superseded by {migration:x}");
                supersede(&host, found, origin, &bucket).await?;
                superseded = Some(staging(&found.path, *older));
                None
            }
            Some((found, Some(Migrating::Receiving { id: newer, .. }))) => {
                return Err(Status::aborted(format!(
                    "migration {migration:x} of {child}, of epoch {epoch}, is older than \
                     {newer:x}, of epoch {}",
                    found.epoch
                )));
            }
            // Handed over from here, coming back before its letting go finished: finished first.
            Some((found, Some(Migrating::HandedOver { .. }))) => {
                super::let_go(&host, found).await.map_err(|err| {
                    Status::unavailable(format!("cell {child} is being let go here: {err}"))
                })?;
                None
            }
            // Noted, but held here no more: its letting go stopped part way, its disk failing
            // (board 5721). This host holds none of it, as the spec's `Prepare` asks.
            Some((found, None)) if matches!(host.holds(found), Ok(false)) => {
                log::info!("cladefsd: {child} noted here, held not: its entry retired");
                retire(&host, found).await?;
                None
            }
            Some((_, None)) => return Err(Status::already_exists(format!("cell {child} is here"))),
        };
        if token.is_none() && host_fs::exists(&path) {
            return Err(Status::already_exists(format!("{} is there", path.display())));
        }
        let token = token.unwrap_or_else(|| rand::random::<u64>() | 1);
        let migrating = Some(Migrating::Receiving { id: migration, token });
        let entry = Entry { child, origin, path, bucket, epoch, migrating };
        host.note(&entry).map_err(|err| status("noting the cell", err))?;
        heard(migration);
        // The older migration's files dropped once no entry names it: dropped part way, a staging
        // an entry names is one a retry of its migration may take for whole (board 5721).
        if let Some(older) = superseded
            && let Err(err) = drop_staged(&older)
        {
            log::warn!("cladefsd: dropping {}, superseded: {err}", older.display());
        }
        let bucket =
            crate::host_cell::bucket(&entry.bucket).map_err(|err| status("its bucket", err))?;
        let incarnation = host.incarnation();
        let referring =
            bucket_cell::lifecycle::refer_ephemeral(&*bucket, origin, incarnation, child);
        if tokio::time::timeout(BUCKET_BOUND, referring).await.is_err() {
            // Referred or not: given up, its reference retired, or, that timing out too, left to
            // the reaping of migrations idle too long.
            refuse(&host, &entry).await?;
            return Err(Status::unavailable(format!("referring to {origin:?} timed out")));
        }
        // Staged afresh but by this daemon: what a daemon before it staged, it may have lost part
        // of, unsynced, at its host's restart (board 5721).
        let staged = staging(&entry.path, migration);
        if !prepared(&migration) {
            drop_staged(&staged).map_err(|err| status("dropping a staging of before", err))?;
        }
        host_fs::create_dir_all(&staged).map_err(|err| status("staging", err))?;
        PREPARED.get().lock().unwrap().insert(migration);
        log::info!(
            "cladefsd: migration {migration:x} of {child} prepared, to {}",
            entry.path.display()
        );
        Ok((incarnation, token))
    }

    /// Gives up the older migration of `found`, never accepted, for one of `origin` in `bucket`:
    /// its cell, if taken into place, and its reference, unless the newer one's is the same; what
    /// it staged goes once the newer one is noted in its place.
    async fn supersede(
        host: &ephemeral_host::Host,
        found: &Entry,
        origin: Origin,
        bucket: &str,
    ) -> Result<(), Status> {
        host.drop_moved(found).map_err(journal)?;
        if (found.origin, found.bucket.as_str()) != (origin, bucket) {
            retire(host, found).await?;
        }
        Ok(())
    }

    /// Retires `entry`'s reference, then the entry, in [`BUCKET_BOUND`]: `UNAVAILABLE` past it,
    /// the entry kept, to be given up again.
    async fn retire(host: &ephemeral_host::Host, entry: &Entry) -> Result<(), Status> {
        match tokio::time::timeout(BUCKET_BOUND, host.retire(entry)).await {
            Ok(retired) => retired.map_err(|err| status("retiring its reference", err)),
            Err(_) => {
                Err(Status::unavailable(format!("retiring {}'s reference timed out", entry.child)))
            }
        }
    }

    /// Writes the piece of a file `request` carries into its migration's staging directory.
    pub(crate) async fn file(request: MigrateFileRequest) -> Result<(), Status> {
        let host = host()?;
        let migration = request.migration;
        let Some((_taking, entry)) = held(&host, migration).await? else {
            return Err(Status::not_found(format!("no migration {migration:x}")));
        };
        heard(migration);
        let staged = staging(&entry.path, migration);
        let written = super::super::run::off_the_runtime(move || write(&staged, request));
        written.await.map_err(|err| status("writing", err))?
    }

    /// Writes the piece of a file `request` carries into `staged`, its migration's staging
    /// directory.
    fn write(staged: &Path, request: MigrateFileRequest) -> Result<(), Status> {
        let MigrateFileRequest { migration, path, offset, bytes, len, holes } = request;
        // Dropped, as the migration was given up as this request came: not made again.
        if !host_fs::is_dir(staged) {
            return Err(Status::not_found(format!("migration {migration:x} given up")));
        }
        // A directory, empty or not, its path ending with `/`.
        let left = || Status::invalid_argument(format!("{path:?} leaves the cell"));
        if let Some(dir) = path.strip_suffix('/') {
            let dir = within(staged, dir).ok_or_else(left)?;
            return host_fs::create_dir_all(dir).map_err(|err| status(&path, err));
        }
        let file_path = within(staged, &path).ok_or_else(left)?;
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
        to_the_end(take_over(request)).await
    }

    async fn take_over(request: MigrateHandOverRequest) -> Result<bool, Status> {
        let host = host()?;
        let MigrateHandOverRequest { migration, incarnation, files, token } = request;
        // Asked of another incarnation of this host's, which alone knows the migration: this one,
        // its disk lost since, can say neither yes nor no (specs/ephemeral).
        if incarnation != host.incarnation() {
            return Err(Status::failed_precondition(format!(
                "migration {migration:x} asked of incarnation {incarnation:x}, this host's {:x}",
                host.incarnation()
            )));
        }
        if host.accepted(migration) {
            // Accepted before: what is left of noting it so, as asking again finishes it (board
            // 5721): its acceptance made durable again, and the cell noted taken. A failure past
            // the acceptance's writing left it there but not for good; answered yes at once, a
            // crash then lost it, and its entry, its cell's source having let the cell go.
            if let Some(child) = host.accepted_cell(migration) {
                host.accept(migration, child)
                    .map_err(|err| status("remembering it accepted", err))?;
            }
            noted_taken(&host, migration).await?;
            return Ok(true);
        }
        let Some((_taking, entry)) = held(&host, migration).await? else {
            log::warn!("cladefsd: refusing migration {migration:x}: not taking it");
            return Ok(false);
        };
        heard(migration);
        let staged = staging(&entry.path, migration);
        // Taken into place before, what followed failing: what is left of taking it, as asking
        // again finishes it (board 5721). A failure reading it says neither: answered so, asked
        // again later.
        let moved_before = !host_fs::exists(&staged)
            && host.holds(&entry).map_err(|err| status("reading the cell taken", err))?;
        let lacking = match entry.receiving() {
            Some((_, taken)) if taken != token => {
                Some(format!("it names token {token:x}, of another take of it"))
            }
            _ if moved_before => None,
            _ => lacking(&staged, &files),
        };
        if let Some(lacking) = lacking {
            log::warn!("cladefsd: refusing migration {migration:x}: {lacking}");
            refuse(&host, &entry).await?;
            return Ok(false);
        }
        let moved = || -> std::io::Result<()> {
            if !moved_before {
                sync_tree(&staged)?;
                host_fs::rename(&staged, &entry.path)?;
            }
            host_fs::File::open(entry.path.parent().unwrap_or(Path::new(".")))?.sync_all()
        };
        moved().map_err(|err| status("taking the cell", err))?;
        crate::faults::crashpoint!("migration::renamed");
        // Its reference written again before it is accepted (`targetRefersFirst`): a restart since
        // its prepare may have retired it, its entry's removal failing after (board 5721).
        let taken = Entry { migrating: None, ..entry };
        let bucket =
            crate::host_cell::bucket(&taken.bucket).map_err(|err| status("its bucket", err))?;
        let (origin, child) = (taken.origin, taken.child);
        let referring =
            bucket_cell::lifecycle::refer_ephemeral(&*bucket, origin, host.incarnation(), child);
        if tokio::time::timeout(BUCKET_BOUND, referring).await.is_err() {
            return Err(Status::unavailable(format!("referring to {origin:?} timed out")));
        }
        host.accept(migration, taken.child)
            .map_err(|err| status("remembering it accepted", err))?;
        host.note(&taken).map_err(|err| status("noting the cell", err))?;
        HEARD.get().lock().unwrap().remove(&migration);
        Ok(true)
    }

    /// Gives the migration `request` names up, if this host holds it not yet.
    pub(crate) async fn abort(request: MigrateAbortRequest) -> Result<(), Status> {
        to_the_end(give_up(request.migration)).await
    }

    async fn give_up(migration: u64) -> Result<(), Status> {
        let host = host()?;
        if host.accepted(migration) {
            return Ok(());
        }
        let Some((_taking, entry)) = held(&host, migration).await? else { return Ok(()) };
        refuse(&host, &entry).await
    }

    /// Gives up the migration taking `entry`'s cell: its cell, if taken into place, then its
    /// entry and reference, then its files. Dropped part way, a staging an entry names is one a
    /// retry of its migration may take for whole, a kept segment's file made again of holes alone
    /// (board 5721).
    async fn refuse(host: &ephemeral_host::Host, entry: &Entry) -> Result<(), Status> {
        host.drop_moved(entry).map_err(journal)?;
        retire(host, entry).await?;
        if let Some((migration, _)) = entry.receiving() {
            let staged = staging(&entry.path, migration);
            if let Err(err) = drop_staged(&staged) {
                log::warn!("cladefsd: dropping {}, refused: {err}", staged.display());
            }
            HEARD.get().lock().unwrap().remove(&migration);
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

    /// Notes migration `migration` goes on, its source there still: `NOT_FOUND` if this host
    /// takes it no more (board 5734).
    pub(crate) fn renew(migration: u64) -> Result<(), Status> {
        if host()?.receiving(migration).map_err(journal)?.is_none() {
            return Err(Status::not_found(format!("no migration {migration:x}")));
        }
        heard(migration);
        Ok(())
    }

    /// Forgets this host accepted migration `migration`: its source let the cell go, and asks no
    /// more (board 5734). The cell noted taken first, durably ([`noted_taken`]).
    pub(crate) async fn release(migration: u64) -> Result<(), Status> {
        let host = host()?;
        noted_taken(&host, migration).await?;
        host.release(migration).map_err(|err| status("forgetting it accepted", err))
    }

    /// Gives up, every so often, each migration being taken here that it heard nothing of for
    /// `idle` (board 5734; specs/ephemeral, `TargetAbort`): its source crashed, or the source's
    /// abort lost. Heard of for the first time, a migration's wait begins.
    pub(crate) async fn reap(idle: Duration) {
        loop {
            tokio::time::sleep((idle / 4).max(Duration::from_millis(250))).await;
            if let Err(err) = reap_idle(idle).await {
                log::warn!("cladefsd: giving up migrations idle too long: {err}");
            }
        }
    }

    async fn reap_idle(idle: Duration) -> Result<(), Status> {
        let host = host()?;
        let taken: Vec<u64> = host
            .entries()
            .map_err(journal)?
            .iter()
            .filter_map(Entry::receiving)
            .map(|(id, _)| id)
            .collect();
        let idle_ones: Vec<u64> = {
            let mut heard = HEARD.get().lock().unwrap();
            heard.retain(|migration, _| taken.contains(migration));
            let now = Instant::now();
            let quiet = |migration: &u64| now - *heard.entry(*migration).or_insert(now) >= idle;
            taken.into_iter().filter(quiet).collect()
        };
        for migration in idle_ones {
            let Some((_taking, entry)) = held(&host, migration).await? else { continue };
            // Heard of while this waited its turn.
            let last = HEARD.get().lock().unwrap().get(&migration).copied();
            if last.is_some_and(|last| last.elapsed() < idle) {
                continue;
            }
            log::warn!(
                "cladefsd: migration {migration:x} of {} idle {idle:?}: given up",
                entry.child
            );
            refuse(&host, &entry).await?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Board 5732: a migration of a cell reserves its path from before its prepare is asked, so
    /// that a second at once is refused rather than prepared beside it; one that never began lets
    /// the path go.
    #[test]
    fn migrations_of_a_cell_reserve_its_path_before_they_prepare() {
        let path = Path::new("/cells/a");
        let first = Reserved::new(path).unwrap();
        assert_eq!(Reserved::new(path).err(), Some(Errno::EBUSY));
        assert!(Reserved::new(Path::new("/cells/b")).is_ok(), "another cell's path is free");
        drop(first);
        drop(Reserved::new(path).unwrap());
        assert!(OUTGOING.get().lock().unwrap().is_empty());
    }
}
