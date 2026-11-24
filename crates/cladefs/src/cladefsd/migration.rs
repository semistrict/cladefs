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
//!    checks what the prepare names, untrusted, once, before anything ([`target::Prepare`]): the
//!    source's formats its own (board 8455), the bucket its own (board 8443), and the cell's path a
//!    new name in its directory of migrated cells (board 8447). It notes the cell in its journal
//!    and writes its own reference to the cell's origin before it takes anything
//!    (`targetRefersFirst`), and answers its incarnation and a token new to this
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
//!    all so (`openAtNamed`), and the cell they make opening as the one prepared, of its own
//!    format (board 8455), remembering the migration accepted (`remembersAccepted`); refusing,
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
//! 5. **Abort** (`AbortMigration`, board 8577): a migration whose hand-over has not begun ends at
//!    the source's word, as one whose prepare went unanswered does (`SourceLoses`): its rounds
//!    stopped, its target told to give it up by its ID, and the cell free to migrate again, to
//!    another target too, its guest running on. Its hand-over begun, the cell given up, its end is
//!    the target's to say: told to give the migration up, then asked to take the cell over as
//!    `HandOverCell` asks, a target that gave it up refuses, and the cell is served here again;
//!    one that accepted before says so again, and the cell is let go of here.
//!
//! The target serves one request of a cell's migrations at a time, each run to its end however
//! its asker goes, its bucket calls bounded (board 5732). The source bounds each of its requests
//! (board 8577): a target whose handler is wedged (a stalled disk, a lock) answers the HTTP/2
//! layer's pings, its connection alive, and a request with no deadline waited on it for ever.

use super::Daemon;
use super::peers::proto::{
    FileDigest, MigrateAbortRequest, MigrateFileRequest, MigrateHandOverRequest,
    MigratePrepareRequest, MigratePrepareResponse, MigrateReleaseRequest, MigrateRenewRequest,
    Range,
};
use crate::cell_dir::Marker;
use crate::ephemeral_host::{self, Entry, Migrating, staging};
use crate::{CellId, Errno};
use bucket_cell::layout::Origin;
use host_fs::FileExt as _;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::Status;

/// The marker of a cell given up to a migration, in its directory ([`Marker`]).
pub(crate) const MARKER: &str = "migrating";

/// Where the marker is staged as it is written ([`write_durably`]), which a failure or a crash may
/// leave, as no part of the cell.
const MARKER_STAGED: &str = "migrating.new";

/// How much of a file a request carries at most.
pub(super) const PIECE: usize = 4 << 20;

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

/// How long a source waits for its target to take each piece of a file (`MigrateFile`), at most
/// [`PIECE`] bytes, and a directory or a file's holes alone (board 8577): the target admits what
/// the piece adds to its disk and writes it bypassing its page cache, a loaded disk's second or
/// two at worst. Past it the round fails, its file sent again whole at the next; a hand-over's
/// last files failing so, it answers `ENOTCONN`, to be asked again. The source's bounds are
/// hotbed question 520.
const PIECE_BOUND: Duration = Duration::from_secs(30);

/// How long a source waits for its target's answer to a hand-over (`MigrateHandOver`, board 8577):
/// the target reads every file but the kept segments whole for its CRC (the raw segments past the
/// local checkpoint, their logs, a few hundred MiB at most: seconds of a disk's reading), opens the
/// cell staged, syncs every file and directory of it (thousands of fsyncs at a few milliseconds
/// each on a loaded disk: tens of seconds), renames it into place, and writes its reference in its
/// bucket within [`BUCKET_BOUND`]. Past it, the hand-over answers `ENOTCONN`, the cell given up
/// still: the target runs the request to its end however its asker goes, and asked again it
/// answers as it did, so a bound too short costs a retry, never the cell.
const HAND_OVER_BOUND: Duration = Duration::from_secs(120);

/// How long a source waits for its target's answer to a renewal (`MigrateRenew`), which reads its
/// journal and writes nothing: past it, the renewal is said and sent again at the next round, well
/// within any `CLADEFSD_MIGRATION_IDLE`.
const RENEW_BOUND: Duration = Duration::from_secs(5);

/// How long a source waits for its target to give a migration up (`MigrateAbort`): as long as a
/// prepare, the target retiring its reference within [`BUCKET_BOUND`].
const ABORT_BOUND: Duration = PREPARE_BOUND;

/// A migration from this host under way: its target, its prepare's answer, what of each kept
/// segment it sent, its length and the bytes it took of the disk then, and its rounds.
///
/// One sender at a time: its rounds, until [`stop`](Self::stop) ends them, then its hand-over
/// alone, which holds the cell. `sent` is locked only between requests, never across one (board
/// 8577): a round waiting on its target held it across its sends, and the hand-over, the cell held
/// alone and its guest paused, waited on the round for as long.
struct Outgoing {
    migration: u64,
    peer: String,
    incarnation: u64,
    token: u64,
    client: super::peers::Client,
    sent: Mutex<HashMap<String, (u64, u64)>>,
    rounds: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Outgoing {
    fn new(
        Marker { migration, peer, incarnation, token }: Marker,
        client: super::peers::Client,
    ) -> Self {
        let (sent, rounds) = (Mutex::default(), Mutex::default());
        Outgoing { migration, peer, incarnation, token, client, sent, rounds }
    }

    /// The migration a cell's marker names, asked again with nothing known sent and no rounds, as
    /// after a restart.
    fn resumed(daemon: &Daemon, marker: Marker) -> Result<Self, Errno> {
        let client = client(daemon, &marker.peer)?;
        Ok(Self::new(marker, client))
    }

    fn marker(&self) -> Marker {
        let (migration, peer) = (self.migration, self.peer.clone());
        Marker { migration, peer, incarnation: self.incarnation, token: self.token }
    }

    /// Ends its rounds, if they run, once their task is gone: a request of theirs cancelled, its
    /// file unnoted in `sent`, which the next sender sends again whole.
    async fn stop(&self) {
        let rounds = self.rounds.lock().unwrap().take();
        let Some(rounds) = rounds else { return };
        rounds.abort();
        match rounds.await {
            Ok(()) => {}
            Err(ended) if ended.is_cancelled() => {}
            Err(ended) => log::error!("cladefsd: migration {:x}'s rounds: {ended}", self.migration),
        }
    }

    /// Tells its target to give it up (`MigrateAbort`), within [`ABORT_BOUND`].
    async fn abort(&self) -> Result<(), Status> {
        let abort = MigrateAbortRequest { migration: self.migration };
        within(ABORT_BOUND, self.client.clone().migrate_abort(abort)).await.map(drop)
    }
}

/// A migration from this host of a cell, in [`OUTGOING`].
enum Underway {
    /// Being prepared ([`Reserved`]).
    Preparing,
    /// Under way, its rounds sending kept segments while the guest runs: [`abort`] ends it.
    Moving(Arc<Outgoing>),
    /// Its hand-over begun ([`hand_over`]): its rounds stopped, its cell given up or about to be,
    /// its end its target's answer.
    Handing(Arc<Outgoing>),
}

/// Per-daemon migration state: outgoing migrations from this host and incoming migrations being
/// taken here (Wave 4C.5, board 10774).
pub(super) struct State {
    /// The migrations from this host, by their cells' paths.
    outgoing: Mutex<BTreeMap<PathBuf, Underway>>,
    /// The cells migrations are taking here, each held by the request of its migrations being
    /// served (`target::Taking`).
    taking: Mutex<HashMap<CellId, Arc<tokio::sync::Mutex<()>>>>,
    /// When each migration being taken here was last heard of, by its ID (board 5734).
    heard: Mutex<HashMap<u64, tokio::time::Instant>>,
    /// The migrations this daemon, since it started, prepared to take, the only ones it takes on
    /// with (board 5721): what a migration staged before is written but not synced, which its
    /// host's restart may have lost part of, so that a migration being taken does not outlive its
    /// daemon (specs/ephemeral: `Crash` makes a cell being received none), whether or not its
    /// start, giving it up, could retire its entry.
    prepared: Mutex<BTreeSet<u64>>,
    /// cladefs: the staging directories of the migrations being taken here, each open, its files
    /// counted in its footprint as they are written, so that the disk governor admits what a peer
    /// stages (board 8451). Like `prepared` and `heard`, of migrations the journal names being
    /// taken alone: each goes as its migration does.
    staging: Mutex<HashMap<u64, local_cell::HostDir>>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            outgoing: Mutex::new(BTreeMap::new()),
            taking: Mutex::new(HashMap::new()),
            heard: Mutex::new(HashMap::new()),
            prepared: Mutex::new(BTreeSet::new()),
            staging: Mutex::new(HashMap::new()),
        }
    }
}

/// cladefs: what migrations did since the daemon started, for its metrics (board 8565): as a source,
/// those prepared at their targets, those whose prepare failed, aborted, their rounds, the bytes they
/// sent, their hand-overs accepted and refused, and those their targets gave up; as a target, the
/// bytes taken, and the migrations given up, aborted by their sources or idle.
#[derive(Default)]
pub(super) struct Counts {
    pub prepared: AtomicU64,
    pub aborted: AtomicU64,
    pub rounds: AtomicU64,
    pub sent_bytes: AtomicU64,
    pub accepted: AtomicU64,
    pub refused: AtomicU64,
    pub given_up: AtomicU64,
    pub taken_bytes: AtomicU64,
    pub taken_given_up: AtomicU64,
}

/// cladefs: [`Counts`]' of the daemon.
// The process's counts, every simulated host's together, as a process's metrics are.
pub(super) static COUNTS: Counts = Counts {
    prepared: AtomicU64::new(0),
    aborted: AtomicU64::new(0),
    rounds: AtomicU64::new(0),
    sent_bytes: AtomicU64::new(0),
    accepted: AtomicU64::new(0),
    refused: AtomicU64::new(0),
    given_up: AtomicU64::new(0),
    taken_bytes: AtomicU64::new(0),
    taken_given_up: AtomicU64::new(0),
};

/// cladefs: counts `n` more in `count` ([`COUNTS`]).
fn count(count: &AtomicU64, n: u64) {
    count.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// cladefs: the migrations under way on `daemon`, for the daemon's metrics (board 8565): from this
/// host, being prepared and moving (rounds, then their hand-overs); and to it, being taken.
pub(super) fn under_way_on(daemon: &Daemon) -> (usize, usize, usize) {
    let outgoing = daemon.migrations.outgoing.lock().unwrap();
    let preparing = outgoing.values().filter(|under| matches!(under, Underway::Preparing)).count();
    (preparing, outgoing.len() - preparing, target::being_taken(daemon))
}

/// The path of a cell whose migration is being prepared, held in `daemon.migrations.outgoing` from
/// before the prepare is asked, so that two migrations of one cell at once never both prepare
/// (board 5732): let go unless the migration begins.
struct Reserved {
    daemon: Arc<Daemon>,
    path: PathBuf,
}

impl Reserved {
    /// The path of the cell at `path` reserved: `EBUSY` if a migration of it is under way.
    fn new(daemon: &Arc<Daemon>, path: &Path) -> Result<Self, Errno> {
        let mut outgoing = daemon.migrations.outgoing.lock().unwrap();
        if outgoing.contains_key(path) {
            return Err(Errno::EBUSY);
        }
        outgoing.insert(path.to_owned(), Underway::Preparing);
        Ok(Reserved { daemon: Arc::clone(daemon), path: path.to_owned() })
    }

    /// The migration under way, `outgoing`: the cell's from now on.
    fn begin(self, outgoing: Arc<Outgoing>) {
        self.daemon
            .migrations
            .outgoing
            .lock()
            .unwrap()
            .insert(self.path.clone(), Underway::Moving(outgoing));
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let mut outgoing = self.daemon.migrations.outgoing.lock().unwrap();
        if let Some(Underway::Preparing) = outgoing.get(&self.path) {
            outgoing.remove(&self.path);
        }
    }
}

/// The status of `errno`, for a peer.
fn status(what: impl std::fmt::Display, err: impl std::fmt::Display) -> Status {
    Status::internal(format!("{what}: {err:#}"))
}

/// What `call`, a request of a peer's, answers within `bound`: `DEADLINE_EXCEEDED` past it, the
/// request dropped (board 8577).
async fn within<T>(
    bound: Duration,
    call: impl std::future::Future<Output = Result<tonic::Response<T>, Status>>,
) -> Result<T, Status> {
    match tokio::time::timeout(bound, call).await {
        Ok(answered) => answered.map(tonic::Response::into_inner),
        // Unanswered within its bound: a failure, as the request's own are.
        Err(_) => Err(Status::deadline_exceeded(format!("no answer within {bound:?}"))),
    }
}

/// A client of the peer at `address`.
fn client(daemon: &Daemon, address: &str) -> Result<super::peers::Client, Errno> {
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    super::peers::member_on(daemon, &runtime, address).map_err(|err| {
        log::warn!("cladefsd: migrating to {address}: {err}");
        Errno::EINVAL
    })
}

/// How long this host, a target, keeps a migration it hears nothing of: `CLADEFSD_MIGRATION_IDLE`
/// seconds, five minutes by default (board 5734).
pub(super) fn idle() -> Result<Duration, String> {
    let Some(set) = node_local::env::var_os("CLADEFSD_MIGRATION_IDLE") else { return Ok(IDLE) };
    // Not UTF-8, or not a number: refused below.
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
pub(super) async fn migrate_on(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    peer: String,
    to: String,
) -> Result<u64, Errno> {
    let host = ephemeral_host::host()?;
    let reserved = Reserved::new(daemon, &path)?;
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
    let entry = local_cell::upload::blocking(|| host.entry(cell)).map_err(|err| {
        log::warn!("cladefsd: the journal: {err:#}");
        Errno::EIO
    })?;
    let entry = entry.ok_or(Errno::ENOENT)?;
    // Taken here by a migration not yet accepted, or handed over and being let go of.
    if entry.migrating.is_some() {
        return Err(Errno::EBUSY);
    }
    // The cell's next epoch, noted before any target hears of it: newer than every migration of
    // the cell before, this host's restarted too.
    let entry = Entry { epoch: entry.epoch + 1, ..entry };
    local_cell::upload::blocking(|| host.note(&entry)).map_err(|err| {
        log::warn!("cladefsd: noting {}'s epoch: {err:#}", entry.child);
        Errno::EIO
    })?;
    let parent = checkpoint.parent();
    let migration = rand::random::<u64>() | 1;
    let mut client = client(daemon, &peer)?;
    let prepare = MigratePrepareRequest {
        migration,
        tenant: cell.tenant,
        cell: cell.id,
        parent_tenant: parent.cell.tenant,
        parent_cell: parent.cell.id,
        at: parent.at,
        parent_epoch: parent.epoch,
        bucket: entry.bucket.clone(),
        to,
        epoch: entry.epoch,
        #[allow(deprecated)]
        formats: String::new(),
        versions: Some(super::peers::current_formats()),
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
                if let Err(err) = within(ABORT_BOUND, client.migrate_abort(abort)).await {
                    log::info!("cladefsd: aborting migration {migration:x} at {peer}: {err}");
                }
                count(&COUNTS.aborted, 1);
                return Err(refusal(&refused));
            }
        };
    count(&COUNTS.prepared, 1);
    let marker = Marker { migration, peer, incarnation, token };
    let outgoing = Arc::new(Outgoing::new(marker, client));
    // Its rounds' task noted before the migration is the cell's, with nothing awaited between: a
    // hand-over or an abort finding it stops them.
    start_rounds(&outgoing, path)?;
    reserved.begin(outgoing);
    Ok(migration)
}

/// Starts the rounds of `outgoing`, the migration of the cell at `path`, noting their task in it.
fn start_rounds(outgoing: &Arc<Outgoing>, path: PathBuf) -> Result<(), Errno> {
    let runtime = crate::bucket::bucket_runtime().map_err(|_| Errno::EIO)?;
    let moving = outgoing.clone();
    let rounds = runtime.spawn(async move {
        let mut renewed = tokio::time::Instant::now();
        loop {
            // Failed, a request of it unanswered within its bound among them: what it did not
            // send, sent at the next (board 8577).
            if let Err(err) = round(&moving, &path).await {
                log::warn!("cladefsd: migration {:x}'s round: {err}", moving.migration);
            }
            count(&COUNTS.rounds, 1);
            if renewed.elapsed() >= RENEW {
                if !renew(&moving).await {
                    break;
                }
                renewed = tokio::time::Instant::now();
            }
            tokio::time::sleep(ROUND).await;
        }
    });
    *outgoing.rounds.lock().unwrap() = Some(rounds);
    Ok(())
}

/// Asks a target to prepare a migration, `prepare`, again with the same migration while its answer
/// is lost, [`PREPARE_TRIES`] times at most (board 5734): the target's answer, or the last
/// failure.
async fn prepare_retrying(
    client: &mut super::peers::Client,
    prepare: MigratePrepareRequest,
) -> Result<MigratePrepareResponse, Status> {
    let mut tries = 0;
    loop {
        tries += 1;
        // Unanswered within its bound: a failure, tried again below as the others are.
        match within(PREPARE_BOUND, client.migrate_prepare(prepare.clone())).await {
            Ok(prepared) => return Ok(prepared),
            Err(lost) if tries < PREPARE_TRIES && unanswered(&lost) => {
                log::info!("cladefsd: migration {:x}'s prepare, again: {lost}", prepare.migration);
                tokio::time::sleep(ROUND * tries).await;
            }
            Err(failed) => return Err(failed),
        }
    }
}

/// The errno `MigrateCell` answers for its prepare's failure, `status`: `ENOTSUP` if the target
/// takes no such migration (`FAILED_PRECONDITION`: its formats, its bucket, or no directory of
/// migrated cells, board 8455), `EINVAL` if the prepare named what means nothing there
/// (`INVALID_ARGUMENT`: a path not in that directory, board 8447); `ENOTCONN` else, unanswered or
/// refused for now, worth asking again.
fn refusal(status: &Status) -> Errno {
    match status.code() {
        tonic::Code::FailedPrecondition => Errno::ENOTSUP,
        tonic::Code::InvalidArgument => Errno::EINVAL,
        _ => Errno::ENOTCONN,
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

/// Whether `relative` is a finalized segment file under `segments/` (immutable except for punched
/// holes, pre-copied in migration rounds; board 6905).
fn is_segment(relative: &str) -> bool {
    relative.starts_with("segments/")
        && !relative.rsplit('/').next().unwrap_or_default().starts_with("framing-")
}

/// A round of `outgoing`: the finalized segments of the cell at `path` not sent yet, each whole.
async fn round(outgoing: &Outgoing, path: &Path) -> Result<(), Status> {
    let listed = path.to_owned();
    let listed = super::run::off_the_runtime(move || files(&listed))
        .await
        .map_err(|err| status("listing", err))?
        .map_err(|err| status("listing", err))?;
    for (relative, len) in listed {
        if !is_segment(&relative) || outgoing.sent.lock().unwrap().contains_key(&relative) {
            continue;
        }
        let allocated = send(outgoing, path, &relative, len).await?;
        outgoing.sent.lock().unwrap().insert(relative, (len, allocated));
    }
    Ok(())
}

/// Tells `outgoing`'s target the migration goes on (board 5734): whether its rounds go on. Given
/// up there, they end: its hand-over will be refused, the cell served here again, or
/// `AbortMigration` ends it.
async fn renew(outgoing: &Outgoing) -> bool {
    let renewal = MigrateRenewRequest { migration: outgoing.migration };
    match within(RENEW_BOUND, outgoing.client.clone().migrate_renew(renewal)).await {
        Ok(_) => true,
        Err(gone) if gone.code() == tonic::Code::NotFound => {
            log::warn!("cladefsd: migration {:x} given up by its target", outgoing.migration);
            count(&COUNTS.given_up, 1);
            false
        }
        Err(err) => {
            log::info!("cladefsd: renewing migration {:x}: {err}", outgoing.migration);
            true
        }
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
            if entry.metadata()?.is_dir() {
                found.push(relative.clone());
                pending.push(relative);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The files of the cell at `path`, by their paths in it, with their lengths: all but its marker,
/// staged or not, and what deleting leaves.
fn files(path: &Path) -> std::io::Result<Vec<(String, u64)>> {
    let mut found = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in host_fs::read_dir(path.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if [MARKER, MARKER_STAGED, "store.deleted"].contains(&relative.as_str()) {
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
/// holes, streaming one [`PIECE`] at a time bypassing the host OS page cache (board 5997), the
/// last piece carrying the holes, so that it lands as it is here over any copy sent before:
/// returns the bytes it takes of the disk.
async fn send(outgoing: &Outgoing, path: &Path, relative: &str, len: u64) -> Result<u64, Status> {
    let full = path.join(relative);
    let label = relative.to_owned();
    let (file, data, allocated) = super::run::off_the_runtime(move || {
        let file = host_fs::File::open(&full)?;
        bucket_cell::direct::bypass(&file, &full)?;
        let (data, allocated) = layout_of(&file, len)?;
        Ok::<_, std::io::Error>((Arc::new(file), data, allocated))
    })
    .await
    .map_err(|err| status(&label, err))?
    .map_err(|err| status(&label, err))?;
    let holes = gaps(&data, len);
    let mut pieces: Vec<(u64, usize)> = Vec::new();
    for &(start, end) in &data {
        let mut at = start;
        while at < end {
            let take = (end - at).min(PIECE as u64) as usize;
            pieces.push((at, take));
            at += take as u64;
        }
    }
    if pieces.is_empty() {
        pieces.push((0, 0));
    }
    let mut client = outgoing.client.clone();
    let last = pieces.len() - 1;
    for (piece, (offset, take)) in pieces.into_iter().enumerate() {
        let bytes = if take == 0 {
            Vec::new()
        } else {
            let file = file.clone();
            super::run::off_the_runtime(move || {
                let mut buf = vec![0u8; take];
                bucket_cell::direct::read_at(&file, &mut buf, offset)?;
                Ok::<_, std::io::Error>(buf)
            })
            .await
            .map_err(|err| status(relative, err))?
            .map_err(|err| status(relative, err))?
        };
        let sent = bytes.len() as u64;
        let request = MigrateFileRequest {
            migration: outgoing.migration,
            path: relative.to_owned(),
            offset,
            bytes,
            len,
            holes: if piece == last { holes.clone() } else { Vec::new() },
        };
        within(PIECE_BOUND, client.migrate_file(request)).await?;
        count(&COUNTS.sent_bytes, sent);
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

/// Fixed buffer size for streaming [`crc_of`] bypassing the host OS page cache (board 5997).
const CRC_CHUNK: usize = 64 << 10;

/// The CRC32C of the file at `path`, streamed in a fixed buffer bypassing the host OS page cache
/// (board 5997).
fn crc_of(path: &Path) -> std::io::Result<u32> {
    let file = host_fs::File::open(path)?;
    bucket_cell::direct::bypass(&file, path)?;
    let len = file.metadata()?.len();
    let mut buf = bucket_cell::direct::Buffer::new(CRC_CHUNK);
    let mut crc = extent_frame::Crc::new();
    let mut offset = 0u64;
    while offset < len {
        let take = (len - offset).min(CRC_CHUNK as u64) as usize;
        let slice = &mut buf.bytes_mut()[..take];
        bucket_cell::direct::read_at(&file, slice, offset)?;
        crc.update(slice);
        offset += take as u64;
    }
    Ok(crc.finish())
}

/// Hands the ephemeral cell at `path` over to the target of its migration
/// (`HandOverCell`), once nothing has it open here (`EBUSY` otherwise): see the module's docs.
/// `EAGAIN` if the target refused, the cell served here again; `ENOTCONN` if it did not answer,
/// the cell given up still, to be asked again; `EIO` if letting the cell go failed once the target
/// took it, which asking again finishes.
pub(super) async fn hand_over_on(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    let host = ephemeral_host::host()?;
    let _held = super::cells::hold_cell_alone(daemon, &path).await?;
    // Handed over before, and asked again: what is left of letting it go (board 5730).
    if let_go_handed_over_at(daemon, &host, &path).await? {
        return Ok(());
    }
    let marked = local_cell::upload::blocking(|| marker_at(&path))?;
    let Handing { outgoing, under_way, unmarked } = handing(daemon, &path, marked)?;
    // Held alone, the cell changes no more: rounds are over.
    if let Some(under_way) = under_way {
        under_way.stop().await;
    }
    if unmarked {
        local_cell::upload::blocking(|| give_up_to(daemon, &path, &outgoing))?;
    }
    ask(daemon, &host, &path, &outgoing).await
}

/// Gives the cell at `path`, held alone, up to `outgoing`, its migration, its marker written
/// durably. Failing so, the cell not given up (no marker there, its write failing before its
/// rename), the migration is under way again ([`Underway::Moving`]), its rounds going on, so that
/// it may be aborted, or handed over again, its guest running meanwhile (board 8577's review: left
/// [`Underway::Handing`], every abort, migration and hand-over answered `EBUSY` while the guest
/// ran). A marker there, renamed into place, its directory's sync failing, gives the cell up still.
fn give_up_to(daemon: &Daemon, path: &Path, outgoing: &Arc<Outgoing>) -> Result<(), Errno> {
    let bytes = outgoing.marker().bytes();
    let Err(err) = write_marker(&path.join(MARKER), &bytes) else { return Ok(()) };
    log::warn!("cladefsd: giving {} up: {err}", path.display());
    if marker_at(path)?.is_none() {
        start_rounds(outgoing, path.to_owned())?;
        let moving = Underway::Moving(outgoing.clone());
        daemon.migrations.outgoing.lock().unwrap().insert(path.to_owned(), moving);
    }
    Err(Errno::EIO)
}

/// Writes a cell's marker, `bytes`, at `path`, durably ([`write_durably`]); failpoint
/// `migration::marking` fails it before anything is written.
fn write_marker(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fail::fail_point!("migration::marking", |_| {
        Err(std::io::Error::other("failpoint migration::marking"))
    });
    write_durably(path, bytes)
}

/// The hand-over of the migration of the cell at `path`, begun ([`handing`]).
struct Handing {
    /// The migration to hand over.
    outgoing: Arc<Outgoing>,
    /// The migration under way, whose rounds to stop: `outgoing`, or one its marker's replaces.
    under_way: Option<Arc<Outgoing>>,
    /// Whether the cell is to be given up: no marker says so yet.
    unmarked: bool,
}

/// Begins the hand-over of the migration of the cell at `path`, held alone, its marker `marked`
/// if it has one: the migration under way, or the one its marker names, under way again, after a
/// restart too, until its marker goes durably (board 5721), noted [`Underway::Handing`], which an
/// abort leaves to the target's answer. `EBUSY` while its prepare is unanswered, `ENOENT` if no
/// migration of it is under way.
fn handing(daemon: &Daemon, path: &Path, marked: Option<Marker>) -> Result<Handing, Errno> {
    let mut outgoing = daemon.migrations.outgoing.lock().unwrap();
    let under_way = match outgoing.get(path) {
        // Its prepare not answered yet: nothing to hand over.
        Some(Underway::Preparing) => return Err(Errno::EBUSY),
        Some(Underway::Moving(under_way) | Underway::Handing(under_way)) => Some(under_way.clone()),
        None => None,
    };
    let (handed, unmarked) = match (marked, &under_way) {
        (None, Some(under_way)) => (under_way.clone(), true),
        // Given up before: asked again, the marker naming the migration.
        (Some(marker), Some(under_way)) if under_way.migration == marker.migration => {
            (under_way.clone(), false)
        }
        (Some(marker), _) => (Arc::new(Outgoing::resumed(daemon, marker)?), false),
        (None, None) => return Err(Errno::ENOENT),
    };
    outgoing.insert(path.to_owned(), Underway::Handing(handed.clone()));
    Ok(Handing { outgoing: handed, under_way, unmarked })
}

/// The marker of the cell at `path`, if it has one. cladefs: a marker there but unread, its disk
/// failing, or not whole, which its durable write never leaves, fails it (`EIO`): taken for none,
/// a migration under way after a restart would be answered as none (board 6908).
fn marker_at(path: &Path) -> Result<Option<Marker>, Errno> {
    let marker_path = path.join(MARKER);
    let read = match host_fs::read(&marker_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read,
    };
    let marker = read.map_err(anyhow::Error::from).and_then(|bytes| Marker::read(&bytes));
    let marker = marker.map_err(|err| {
        log::error!("cladefsd: {}: {err:#}", marker_path.display());
        Errno::EIO
    })?;
    Ok(Some(marker))
}

/// Lets go of the cell at `path`, held, if this host's journal notes it handed over: whether it
/// did, asked again after a failure or a crash letting it go (board 5730).
async fn let_go_handed_over_at(
    daemon: &Daemon,
    host: &ephemeral_host::Host,
    path: &Path,
) -> Result<bool, Errno> {
    let handed = local_cell::upload::blocking(|| host.handed_over(path)).map_err(|err| {
        log::error!("cladefsd: the journal: {err:#}");
        Errno::EIO
    })?;
    for entry in &handed {
        log::info!("cladefsd: {} handed over: letting it go", entry.child);
        let_go(daemon, host, entry).await?;
    }
    Ok(!handed.is_empty())
}

/// Asks the target of `outgoing`, the migration of the cell at `path`, held alone and given up, to
/// take the cell over: what it lacks sent first, then the hand-over. Accepted, the cell is let go
/// of here; refused, served here again (`EAGAIN`); unanswered, given up still (`ENOTCONN`).
async fn ask(
    daemon: &Daemon,
    host: &ephemeral_host::Host,
    path: &Path,
    outgoing: &Outgoing,
) -> Result<(), Errno> {
    let migration = outgoing.migration;
    // What the target lacks, sent again however often asked; staged there no more, the target
    // accepted the migration or gave it up, as its answer says.
    match finish(outgoing, path).await {
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
    let naming = path.to_owned();
    let files = super::cells::blocking(move || {
        named(&naming).map_err(|err| {
            log::warn!("cladefsd: naming {}'s files: {err}", naming.display());
            Errno::EIO
        })
    })
    .await?;
    let (incarnation, token) = (outgoing.incarnation, outgoing.token);
    let request = MigrateHandOverRequest { migration, incarnation, files, token };
    let mut client = outgoing.client.clone();
    let accepted = match within(HAND_OVER_BOUND, client.migrate_hand_over(request)).await {
        Ok(answer) => answer.accepted,
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
        let marker = path.join(MARKER);
        let display = path.to_owned();
        super::cells::blocking(move || {
            remove_durably(&marker).map_err(|err| {
                log::warn!("cladefsd: serving {} again: {err}", display.display());
                Errno::EIO
            })
        })
        .await?;
        daemon.migrations.outgoing.lock().unwrap().remove(path);
        count(&COUNTS.refused, 1);
        return Err(Errno::EAGAIN);
    }
    daemon.migrations.outgoing.lock().unwrap().remove(path);
    count(&COUNTS.accepted, 1);
    // Accepted: noted so in the cell's entry, durably, then let go of, as asking again, or the
    // daemon starting again, finds it noted (board 5730).
    let handed = local_cell::upload::blocking(|| -> Result<Entry, Errno> {
        let cell = local_cell::host_cell(path).map_err(|_| Errno::EIO)?;
        let entry = host.entry(cell).map_err(|err| {
            log::warn!("cladefsd: the journal: {err:#}");
            Errno::EIO
        })?;
        let entry = entry.ok_or(Errno::EIO)?;
        let peer = outgoing.peer.clone();
        let handed =
            Entry { migrating: Some(Migrating::HandedOver { id: migration, peer }), ..entry };
        host.note(&handed).map_err(|err| {
            log::warn!("cladefsd: noting {} handed over: {err:#}", path.display());
            Errno::EIO
        })?;
        Ok(handed)
    })?;
    let_go(daemon, host, &handed).await
}

/// Ends the migration of the ephemeral cell at `path` (`AbortMigration`, board 8577), as the
/// module's docs say. Its hand-over not begun: its rounds stopped, its target told to give it up,
/// and the cell free to migrate again, its guest running on; told not, the target gives it up idle,
/// or at the cell's next prepare there (`SourceLoses`, `TargetAbort`, `supersedes`). Its hand-over
/// begun, the cell held alone as a hand-over holds it (`EBUSY` while open), its marker's migration
/// is told to give up, then asked to take the cell over, as `HandOverCell` asks: refused, the cell
/// served here again (`EAGAIN`, as `HandOverCell` answers); unanswered, given up still
/// (`ENOTCONN`); accepted before the target heard the abort, or handed over before, the cell let
/// go of here, the target's (`EEXIST`). `EBUSY` while its prepare is unanswered, `ENOENT` if no
/// migration of the cell is under way.
pub(super) async fn abort_on(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    // While its guest runs, the cell open: no hold, which an open cell refuses.
    if let Some(moving) = take_moving(daemon, &path, false)? {
        return abort_moving(&moving).await;
    }
    let host = ephemeral_host::host()?;
    // Nothing of it under way, given up or handed over: nothing to end, its guest running or not.
    // A hand-over gives it up only under way, which a migration begun since is.
    let under_way = daemon.migrations.outgoing.lock().unwrap().contains_key(&path);
    let (handed, has_marker) = local_cell::upload::blocking(|| -> Result<_, Errno> {
        let handed = host.handed_over(&path).map_err(|err| {
            log::error!("cladefsd: the journal: {err:#}");
            Errno::EIO
        })?;
        let has_marker = !under_way && handed.is_empty() && marker_at(&path)?.is_some();
        Ok((handed, has_marker))
    })?;
    if !under_way && handed.is_empty() && !has_marker {
        return Err(Errno::ENOENT);
    }
    let _held = super::cells::hold_cell_alone(daemon, &path).await?;
    if let_go_handed_over_at(daemon, &host, &path).await? {
        return Err(Errno::EEXIST);
    }
    let Some(marker) = local_cell::upload::blocking(|| marker_at(&path))? else {
        // Held alone, no hand-over goes on: its hand-over failed before giving the cell up, if
        // it began, which leaves it not given up, a migration under way still.
        return match take_moving(daemon, &path, true)? {
            Some(moving) => abort_moving(&moving).await,
            None => Err(Errno::ENOENT),
        };
    };
    let Handing { outgoing, under_way, .. } = handing(daemon, &path, Some(marker))?;
    if let Some(under_way) = under_way {
        under_way.stop().await;
    }
    // Unheard, the target may yet accept: the cell given up still, asked again.
    if let Err(err) = outgoing.abort().await {
        log::warn!("cladefsd: aborting migration {:x}: {err}", outgoing.migration);
        return Err(Errno::ENOTCONN);
    }
    match ask(daemon, &host, &path, &outgoing).await {
        // Accepted before the abort: handed over, the cell the target's, which an abort answering
        // Ok would hide (hotbed question 519).
        Ok(()) => Err(Errno::EEXIST),
        Err(errno) => Err(errno),
    }
}

/// The migration of the cell at `path` taken out of `daemon.migrations.outgoing`, if its hand-over
/// has not begun, or, `handing`, the cell held alone, whose hand-over then went no further than
/// stopping its rounds: `EBUSY` while its prepare is unanswered.
fn take_moving(
    daemon: &Daemon,
    path: &Path,
    handing: bool,
) -> Result<Option<Arc<Outgoing>>, Errno> {
    let mut outgoing = daemon.migrations.outgoing.lock().unwrap();
    let taken = match outgoing.get(path) {
        Some(Underway::Preparing) => return Err(Errno::EBUSY),
        Some(Underway::Moving(moving)) => moving.clone(),
        Some(Underway::Handing(begun)) if handing => begun.clone(),
        Some(Underway::Handing(_)) | None => return Ok(None),
    };
    outgoing.remove(path);
    Ok(Some(taken))
}

/// Ends `moving`, a migration out of `daemon.migrations.outgoing` whose cell is not given up: its
/// rounds stopped, its target told to give it up. Told not, the target gives it up idle, or at the
/// cell's next prepare there: this host pursues it no more either way.
async fn abort_moving(moving: &Outgoing) -> Result<(), Errno> {
    moving.stop().await;
    let migration = moving.migration;
    match moving.abort().await {
        Ok(()) => log::info!("cladefsd: migration {migration:x} aborted"),
        Err(err) => log::warn!("cladefsd: aborting migration {migration:x}, unheard: {err}"),
    }
    Ok(())
}

/// Lets go of the cell `entry` notes handed over: its cell directory goes, if it is the cell's
/// still, then its target is told it may forget it accepted the migration, then this host's
/// reference and the entry go (`sourceRetiresLast`). `EIO` if the directory does not go, the entry
/// kept, from which asking again goes on.
async fn let_go(daemon: &Daemon, host: &ephemeral_host::Host, entry: &Entry) -> Result<(), Errno> {
    let Some(Migrating::HandedOver { id: migration, peer }) = &entry.migrating else {
        return Err(Errno::EINVAL);
    };
    let (path, child) = (entry.path.clone(), entry.child);
    super::cells::blocking(move || {
        // The cell, or its removal stopped part way: gone. Another cell put there since, its own.
        // Its description there but unread fails it, asked again: taken for another's, the cell
        // handed over would stay here, its entry forgotten (board 6908).
        let its = match local_cell::host_cell(&path) {
            Ok(cell) => cell == child,
            // Its removal stopped part way: its own still.
            Err(_) if local_cell::deleting_host(&path) => true,
            Err(err)
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
            {
                false
            }
            Err(err) => {
                log::warn!("cladefsd: {}, handed over: {err:#}", path.display());
                return Err(Errno::EIO);
            }
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
    release(daemon, peer, *migration).await;
    match host.retire(entry).await {
        Ok(()) => Ok(()),
        // The entry gone, its directory's sync failing: let go of all the same (board 5721), as
        // asking again would find nothing to go on from (`ENOENT`). A crash may bring the entry
        // back, which the daemon starting lets go of again, each step again as before.
        Err(err) if matches!(local_cell::upload::blocking(|| host.entry(child)), Ok(None)) => {
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
async fn release(daemon: &Daemon, peer: &str, migration: u64) {
    let Ok(mut client) = client(daemon, peer) else { return };
    let released = MigrateReleaseRequest { migration };
    // Unanswered: said; the target forgets the migration when it is idle as long.
    if let Err(err) = within(BUCKET_BOUND, client.migrate_release(released)).await {
        log::info!("cladefsd: releasing migration {migration:x} at {peer}: {err}");
    }
}

/// Lets go of every cell this host's journal notes handed over, as `daemon` starts, before it
/// serves (board 5730): a crash part way through letting one go. Those it fails to it leaves to
/// asking again.
pub(super) async fn let_go_handed_over_on(
    daemon: &Daemon,
    host: &ephemeral_host::Host,
) -> anyhow::Result<usize> {
    let mut let_go_of = 0;
    for entry in local_cell::upload::blocking(|| host.entries())? {
        if !matches!(entry.migrating, Some(Migrating::HandedOver { .. })) {
            continue;
        }
        match let_go(daemon, host, &entry).await {
            Ok(()) => let_go_of += 1,
            Err(err) => log::warn!("cladefsd: letting {} go, handed over: {err}", entry.child),
        }
    }
    Ok(let_go_of)
}

/// Sends what `outgoing` has not sent of the cell at `path`, given up: its directories; the kept
/// segments not sent, and the holes of those sent that gave back more since; and every other file,
/// whole. Its rounds stopped, it is the one sender ([`Outgoing`]).
async fn finish(outgoing: &Outgoing, path: &Path) -> Result<(), Status> {
    // Its directories first, the empty ones among them: a directory's path ends with `/`.
    let listing = path.to_owned();
    let (listed_dirs, listed_files) = super::run::off_the_runtime(move || {
        Ok::<_, std::io::Error>((dirs(&listing)?, files(&listing)?))
    })
    .await
    .map_err(|err| status("listing", err))?
    .map_err(|err| status("listing", err))?;
    for dir in listed_dirs {
        let request = MigrateFileRequest {
            migration: outgoing.migration,
            path: format!("{dir}/"),
            offset: 0,
            bytes: Vec::new(),
            len: 0,
            holes: Vec::new(),
        };
        within(PIECE_BOUND, outgoing.client.clone().migrate_file(request)).await?;
    }
    for (relative, len) in listed_files {
        let sent = outgoing.sent.lock().unwrap().get(&relative).copied();
        match sent {
            Some((was, allocated)) if is_segment(&relative) && was == len => {
                let full = path.join(&relative);
                let label = relative.clone();
                let (data, now) = super::run::off_the_runtime(move || {
                    let file = host_fs::File::open(&full)?;
                    layout_of(&file, len)
                })
                .await
                .map_err(|err| status(&label, err))?
                .map_err(|err| status(&label, err))?;
                if now != allocated {
                    let request = MigrateFileRequest {
                        migration: outgoing.migration,
                        path: relative.clone(),
                        offset: 0,
                        bytes: Vec::new(),
                        len,
                        holes: gaps(&data, len),
                    };
                    within(PIECE_BOUND, outgoing.client.clone().migrate_file(request)).await?;
                    outgoing.sent.lock().unwrap().insert(relative, (len, now));
                }
            }
            _ => {
                let allocated = send(outgoing, path, &relative, len).await?;
                outgoing.sent.lock().unwrap().insert(relative, (len, allocated));
            }
        }
    }
    Ok(())
}

/// Every file of the cell at `path`, given up, as its hand-over names them: its length, and the
/// CRC of each but the finalized segments'.
fn named(path: &Path) -> std::io::Result<Vec<FileDigest>> {
    let mut named = Vec::new();
    for (relative, len) in files(path)? {
        let crc = match is_segment(&relative) {
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
        // Failing already: what is left of the staged file, gone if it can go; the error is the
        // caller's.
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

    /// cladefs: how many migrations `daemon` takes, prepared and not yet gone (board 8565).
    pub(super) fn being_taken(daemon: &Daemon) -> usize {
        daemon.migrations.prepared.lock().unwrap().len()
    }

    /// cladefs: forgets migration `migration`, being taken here no more (board 8451): what
    /// `prepared`, `heard` and `staging` keep of it.
    fn forget(daemon: &Daemon, migration: u64) {
        daemon.migrations.prepared.lock().unwrap().remove(&migration);
        daemon.migrations.heard.lock().unwrap().remove(&migration);
        daemon.migrations.staging.lock().unwrap().remove(&migration);
    }

    /// cladefs: migration `migration`'s staging directory at `staged`, open: the one open, or
    /// opened now, counted as its files are written.
    fn staging_dir(
        daemon: &Daemon,
        migration: u64,
        staged: &Path,
    ) -> std::io::Result<local_cell::HostDir> {
        let mut open = daemon.migrations.staging.lock().unwrap();
        if let Some(dir) = open.get(&migration) {
            return Ok(dir.clone());
        }
        let dir = local_cell::HostDir::new(staged)?;
        open.insert(migration, dir.clone());
        Ok(dir)
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
        daemon: Arc<Daemon>,
        child: CellId,
        held: Option<tokio::sync::OwnedMutexGuard<()>>,
    }

    /// Holds `child` ([`Taking`]), once no other request of its migrations is served.
    async fn taking(daemon: &Arc<Daemon>, child: CellId) -> Taking {
        let lock = daemon.migrations.taking.lock().unwrap().entry(child).or_default().clone();
        Taking { daemon: Arc::clone(daemon), child, held: Some(lock.lock_owned().await) }
    }

    impl Drop for Taking {
        fn drop(&mut self) {
            self.held = None;
            // Its lock gone unless another request holds it, or waits for it.
            let mut taking = self.daemon.migrations.taking.lock().unwrap();
            if taking.get(&self.child).is_some_and(|lock| Arc::strong_count(lock) == 1) {
                taking.remove(&self.child);
            }
        }
    }

    /// The entry of the cell migration `migration` is taking here, held ([`Taking`]), if one is.
    /// One a daemon before this one prepared it gives up (`prepared`), its start having failed
    /// to, its disk failing.
    async fn held(
        daemon: &Arc<Daemon>,
        host: &ephemeral_host::Host,
        migration: u64,
    ) -> Result<Option<(Taking, Entry)>, Status> {
        let Some(entry) =
            local_cell::upload::blocking(|| host.receiving(migration)).map_err(journal)?
        else {
            return Ok(None);
        };
        let taking = taking(daemon, entry.child).await;
        // As the request served before this one left it.
        let Some(entry) =
            local_cell::upload::blocking(|| host.receiving(migration)).map_err(journal)?
        else {
            return Ok(None);
        };
        if !daemon.migrations.prepared.lock().unwrap().contains(&migration) {
            log::warn!("cladefsd: migration {migration:x} prepared by a daemon before: given up");
            refuse(daemon, host, &entry).await?;
            return Ok(None);
        }
        Ok(Some((taking, entry)))
    }

    /// Notes, durably, the cell migration `migration`, accepted, took as taken: its entry written
    /// again, unless the cell went on since. A noting that failed, or whose sync did, is made good
    /// so before the acceptance is answered again or forgotten (boards 5721, 5734): forgotten
    /// first, a crash bringing back an entry being taken had this host's start take it for a
    /// migration never accepted, removing the cell, the one copy its source let go of.
    async fn noted_taken(
        daemon: &Arc<Daemon>,
        host: &ephemeral_host::Host,
        migration: u64,
    ) -> Result<(), Status> {
        let Some(child) =
            local_cell::upload::blocking(|| host.accepted_cell(migration)).map_err(journal)?
        else {
            return Ok(());
        };
        let _taking = taking(daemon, child).await;
        local_cell::upload::blocking(|| -> Result<(), Status> {
            let Some(entry) = host.entry(child).map_err(journal)? else { return Ok(()) };
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
        })
    }

    /// Notes migration `migration` heard of now (board 5734).
    fn heard(daemon: &Daemon, migration: u64) {
        daemon.migrations.heard.lock().unwrap().insert(migration, Instant::now());
    }

    /// Runs `work`, a request's, to its end however its asker goes: a request cancelled leaves
    /// none of its steps half taken, each its bucket calls bounded.
    async fn to_the_end<T: Send + 'static>(
        work: impl std::future::Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Status> {
        let runtime = crate::bucket::bucket_runtime().map_err(|err| status("its runtime", err))?;
        // cladefs: in the call's trace context, the parent of its spans (board 8569).
        let done = runtime.spawn(bucket_cell::trace::Parent::current().around(work)).await;
        done.unwrap_or_else(|err| Err(status("serving it", err)))
    }

    /// The directory ephemeral cells migrated to this host go in (`CLADEFSD_EPHEMERAL_CELLS`),
    /// as its path resolves, each a new name in it (board 8447): the only place a peer's prepare
    /// makes anything. `Err` saying why if it names none, or no directory: a host of no such
    /// directory takes no migrations, and its daemon refuses to start naming one not there.
    pub(crate) fn cells_root() -> Result<PathBuf, String> {
        let set = node_local::env::var_os("CLADEFSD_EPHEMERAL_CELLS");
        let Some(set) = set.filter(|set| !set.is_empty()) else {
            return Err("CLADEFSD_EPHEMERAL_CELLS names no directory for migrated cells".into());
        };
        let root = host_fs::canonicalize(&set);
        let root = root.map_err(|err| format!("CLADEFSD_EPHEMERAL_CELLS {set:?}: {err}"))?;
        if !host_fs::is_dir(&root) {
            return Err(format!("CLADEFSD_EPHEMERAL_CELLS {set:?} is no directory"));
        }
        Ok(root)
    }

    /// The longest name of a migrated cell's directory, `NAME_MAX` less what its staging adds:
    /// `.`, `.migrating-` and a migration's ID in hexadecimal.
    const NAME_MOST: usize = 255 - 1 - ".migrating-".len() - 16;

    /// The path in `root`, the directory of migrated cells, a peer's `to` names, if it names one:
    /// a name directly in `root`, `root` as it resolves, then `/` and a name neither empty, nor
    /// beginning with `.` (as `.`, `..` and every staging do), nor holding a `/` or a NUL, nor
    /// longer than [`NAME_MOST`] (board 8447). A peer's paths are untrusted (AGENTS.md, Trust
    /// boundaries): one relative, through `..`, deeper, of `root` by another path (a link to it),
    /// or `root` itself, names none, which every use of `to` checks here, never by itself.
    fn cell_path(root: &Path, to: &str) -> Option<PathBuf> {
        let name = to.strip_prefix(root.to_str()?)?.strip_prefix('/')?;
        let normal = !name.is_empty()
            && !name.starts_with('.')
            && !name.contains(['/', '\0'])
            && name.len() <= NAME_MOST;
        normal.then(|| root.join(name))
    }

    /// A migration's prepare, decoded, its request checked once, where it enters (AGENTS.md, Trust
    /// boundaries), before anything is noted, made or opened: the source's formats this host's
    /// (board 8455), its bucket this host's (board 8443), and where its cell goes a new name in the
    /// directory of migrated cells (board 8447).
    pub(crate) struct Prepare {
        migration: u64,
        child: CellId,
        origin: Origin,
        bucket: String,
        path: PathBuf,
        epoch: u64,
    }

    impl Prepare {
        /// `request` decoded: `FAILED_PRECONDITION` if its formats are not this host's, an older
        /// source's naming none among them, or its bucket is not this host's, or this host takes
        /// no migrations; `INVALID_ARGUMENT` if its `to` names no new name in the directory of
        /// migrated cells ([`cell_path`]).
        pub(crate) fn decode(request: MigratePrepareRequest) -> Result<Self, Status> {
            #[allow(deprecated)]
            let MigratePrepareRequest {
                migration,
                tenant,
                cell,
                parent_tenant,
                parent_cell,
                at,
                parent_epoch,
                bucket,
                to,
                epoch,
                formats: _,
                versions,
            } = request;
            crate::cladefsd::peers::check_migration_formats(migration, versions)?;
            // Never echoed: the host's bucket's URL is no peer's to learn (board 8437).
            if crate::host_cell::host_bucket_url().as_deref() != Some(bucket.as_str()) {
                return Err(Status::failed_precondition(format!(
                    "migration {migration:x}: its bucket is not this host's"
                )));
            }
            let root = cells_root().map_err(Status::failed_precondition)?;
            let path = cell_path(&root, &to).ok_or_else(|| {
                Status::invalid_argument(format!(
                    "migration {migration:x}: {to:?} is no new name in {}",
                    root.display()
                ))
            })?;
            let child = CellId { tenant, id: cell };
            let parent = CellId { tenant: parent_tenant, id: parent_cell };
            let origin = Origin { parent, at, epoch: parent_epoch };
            Ok(Prepare { migration, child, origin, bucket, path, epoch })
        }
    }

    /// Prepares the migration `request` names: the cell noted, then its reference written here
    /// before anything is taken, then its staging directory made. Returns this host's incarnation
    /// and the token of its take of it. Asked again, it prepares again, each step as it did,
    /// answering the same. An older migration of the cell it is taking, never accepted, it gives
    /// up first: its source, the cell's writer, preparing this one, pursues that one no more, its
    /// answer lost or its daemon restarted since (specs/ephemeral, `supersedes`); a migration older
    /// than the one it takes, by the cell's epoch, it refuses (`ABORTED`, board 5732).
    pub(crate) async fn prepare(
        daemon: &Arc<Daemon>,
        request: MigratePrepareRequest,
    ) -> Result<(u64, u64), Status> {
        let prepare = local_cell::upload::blocking(|| Prepare::decode(request))?;
        to_the_end(take(Arc::clone(daemon), prepare)).await
    }

    async fn take(daemon: Arc<Daemon>, prepare: Prepare) -> Result<(u64, u64), Status> {
        let host = host()?;
        let Prepare { migration, child, origin, bucket, path, epoch } = prepare;
        // The bucket its reference goes in, this host's, opened before anything is noted: a
        // failure after the note left an entry (board 8443).
        let opened = crate::host_cell::bucket(&bucket).map_err(|err| status("its bucket", err))?;
        let _taking = taking(&daemon, child).await;
        // Its source has its answer: asked again, late.
        if local_cell::upload::blocking(|| host.accepted(migration)) {
            return Err(Status::already_exists(format!("migration {migration:x} accepted")));
        }
        // Another cell's migration going to the same path: the later rename would fail, or find
        // the other cell there.
        let entries = local_cell::upload::blocking(|| host.entries()).map_err(journal)?;
        if let Some(other) = entries
            .iter()
            .find(|other| other.path == path && other.child != child && other.receiving().is_some())
        {
            return Err(Status::already_exists(format!(
                "{} is where a migration of cell {} goes",
                path.display(),
                other.child
            )));
        }
        let found = entries.into_iter().find(|found| found.child == child);
        let prepared = |id: &u64| daemon.migrations.prepared.lock().unwrap().contains(id);
        let mut superseded = None;
        let token = match found.as_ref().map(|found| (found, &found.migrating)) {
            None => None,
            // Asked again, its path another: no retry of it, which names what it named.
            Some((found, Some(Migrating::Receiving { id, .. })))
                if *id == migration && found.path != path =>
            {
                return Err(Status::failed_precondition(format!(
                    "migration {migration:x} goes to {}",
                    found.path.display()
                )));
            }
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
                superseded = Some((*older, staging(&found.path, *older)));
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
                super::let_go(&daemon, &host, found).await.map_err(|err| {
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
        // Anything there, a link to nowhere among them, which the rename would replace.
        if token.is_none()
            && local_cell::upload::blocking(|| host_fs::symlink_metadata(&path).is_ok())
        {
            return Err(Status::already_exists(format!("{} is there", path.display())));
        }
        let token = token.unwrap_or_else(|| rand::random::<u64>() | 1);
        let migrating = Some(Migrating::Receiving { id: migration, token });
        let entry = Entry { child, origin, path, bucket, epoch, migrating };
        local_cell::upload::blocking(|| host.note(&entry))
            .map_err(|err| status("noting the cell", err))?;
        heard(&daemon, migration);
        // The older migration's files dropped once no entry names it: dropped part way, a staging
        // an entry names is one a retry of its migration may take for whole (board 5721).
        if let Some((older_id, older)) = superseded {
            forget(&daemon, older_id);
            if let Err(err) = local_cell::upload::blocking(|| drop_staged(&older)) {
                log::warn!("cladefsd: dropping {}, superseded: {err}", older.display());
            }
        }
        let incarnation = host.incarnation();
        let referring =
            bucket_cell::lifecycle::refer_ephemeral(&*opened, origin, incarnation, child);
        let failed = match tokio::time::timeout(BUCKET_BOUND, referring).await {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(status("its reference", err)),
            // Timed out: the failure said, as the others are.
            Err(_) => Some(Status::unavailable(format!("referring to {origin:?} timed out"))),
        };
        if let Some(failed) = failed {
            // Referred or not: given up, its reference retired, or, that timing out too, left to
            // the reaping of migrations idle too long.
            refuse(&daemon, &host, &entry).await?;
            return Err(failed);
        }
        // Staged afresh but by this daemon: what a daemon before it staged, it may have lost part
        // of, unsynced, at its host's restart (board 5721).
        // Failing, given up, its entry and reference with it: kept, the migration waited for its
        // reaping (board 8443).
        let staged = staging(&entry.path, migration);
        let already_prepared = prepared(&migration);
        let staging = local_cell::upload::blocking(|| {
            if !already_prepared {
                drop_staged(&staged)?;
            }
            // In the directory of migrated cells, there already: no directory above it made.
            host_fs::create_dir(&staged).or_else(|err| match err.kind() {
                std::io::ErrorKind::AlreadyExists if host_fs::is_dir(&staged) => Ok(()),
                _ => Err(err),
            })
        });
        if let Err(err) = staging {
            refuse(&daemon, &host, &entry).await?;
            return Err(status("staging", err));
        }
        daemon.migrations.prepared.lock().unwrap().insert(migration);
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

    /// Writes the piece of a file `request` carries into its migration's staging directory. It
    /// waits its turn for the cell as its asker does, a request given up on before its turn holding
    /// nothing, its piece let go of (Codex's review, hotbed review 86: waiting detached, a peer's
    /// requests given up on piled up behind a stalled write, their pieces with them); its turn
    /// come, it runs to its end however its asker goes, holding the cell until written: its source
    /// giving up on it at its bound (board 8577) dropped the request, and the cell's hold, while
    /// its write went on off the runtime, the next request, a hand-over's last copy of the file
    /// among them, overtaking it (hotbed review 83).
    pub(crate) async fn file(
        daemon: &Arc<Daemon>,
        request: MigrateFileRequest,
    ) -> Result<(), Status> {
        if wedged() {
            std::future::pending::<()>().await;
        }
        let host = host()?;
        let migration = request.migration;
        let Some((taking, entry)) = held(daemon, &host, migration).await? else {
            return Err(Status::not_found(format!("no migration {migration:x}")));
        };
        to_the_end(write_held(Arc::clone(daemon), taking, entry, request)).await
    }

    /// Writes the piece of a file `request` carries into the staging directory of `entry`'s
    /// migration, its cell held, `_taking`, until written.
    async fn write_held(
        daemon: Arc<Daemon>,
        _taking: Taking,
        entry: Entry,
        request: MigrateFileRequest,
    ) -> Result<(), Status> {
        let migration = request.migration;
        heard(&daemon, migration);
        let staged = staging(&entry.path, migration);
        let written = super::super::run::off_the_runtime(move || {
            // Dropped, as the migration was given up as this request came: not made again.
            if !host_fs::is_dir(&staged) {
                return Err(Status::not_found(format!("migration {migration:x} given up")));
            }
            let dir =
                staging_dir(&daemon, migration, &staged).map_err(|err| status("staging", err))?;
            write(&dir, request)
        });
        written.await.map_err(|err| status("writing", err))?
    }

    /// Whether this target is wedged, taking a file's piece and never answering, as a stalled disk
    /// or a lock leaves its handler (failpoint `migration::file_unanswered`): its source's bound
    /// ends the wait (board 8577's tests).
    fn wedged() -> bool {
        fail::fail_point!("migration::file_unanswered", |_| true);
        false
    }

    /// Writes the piece of a file `request` carries into `staging`, its migration's staging
    /// directory: what it adds to the disk admitted by the disk governor first (board 8451),
    /// `RESOURCE_EXHAUSTED` if it has no room, and counted in the directory's footprint as it is
    /// written: its bytes, and the length it gives the file, holes and all, as a peer's length may
    /// be any.
    fn write(staging: &local_cell::HostDir, request: MigrateFileRequest) -> Result<(), Status> {
        use local_cell::{Dir as _, File as _};
        fail::fail_point!("migration::writing");
        let MigrateFileRequest { migration, path, offset, bytes, len, holes } = request;
        let staged = staging.path();
        // A directory, empty or not, its path ending with `/`.
        let left = || Status::invalid_argument(format!("{path:?} leaves the cell"));
        if let Some(dir) = path.strip_suffix('/') {
            within(staged, dir).ok_or_else(left)?;
            return made_dirs(staging, dir).map_err(|err| status(&path, err));
        }
        within(staged, &path).ok_or_else(left)?;
        // Validate untrusted peer offset and length before `direct::write_at` rounds end up to
        // `DIRECT_BLOCK` (4096) and writes at `offset` (board 5997 / AGENTS.md trust boundary).
        // cladefs: and its piece within the file's length (board 8451).
        let end = offset.checked_add(bytes.len() as u64);
        let valid_end = end
            .and_then(|end| end.checked_next_multiple_of(bucket_cell::direct::DIRECT_BLOCK))
            .is_some_and(|end| end <= i64::MAX as u64);
        if !valid_end || len > i64::MAX as u64 || end.is_none_or(|end| end > len) {
            return Err(Status::invalid_argument(format!(
                "{path:?}: offset {offset} or len {len} out of range"
            )));
        }
        let failed = |err: std::io::Error| status(&path, err);
        if let Some((dir, _)) = path.rsplit_once('/') {
            made_dirs(staging, dir).map_err(failed)?;
        }
        // The file staged so far, if any: a file not there yet is made once admitted.
        let file = match staging.open(&path, false) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            opened => Some(opened.map_err(failed)?),
        };
        let size = file.as_ref().map_or(Ok(0), |file| file.size()).map_err(failed)?;
        // Sent again shorter than before, from its start: made anew, as no file shrinks in place.
        let (file, size) = match size > len {
            false => (file, size),
            true if offset == 0 => {
                drop(file);
                staging.remove(&path).map_err(failed)?;
                (None, 0)
            }
            true => {
                return Err(Status::invalid_argument(format!(
                    "{path:?}: {len} bytes long, {size} staged, sent at {offset}"
                )));
            }
        };
        let grows = len.saturating_sub(size);
        let admitted = staging.footprint().admit(bytes.len() as u64, grows);
        let _pledge = admitted.map_err(|full| {
            Status::resource_exhausted(format!(
                "migration {migration:x}: {path:?}: no room on the disk: {full}"
            ))
        })?;
        let file = match file {
            Some(file) => file,
            None => staging.create(&path, false).map_err(failed)?,
        };
        if !bytes.is_empty() {
            file.write_at(&bytes, offset).map_err(failed)?;
            count(&COUNTS.taken_bytes, bytes.len() as u64);
        }
        // Its length, past its last data a hole, which its holes' punching gives back.
        if len > file.size().map_err(failed)? {
            file.write_at(&[0], len - 1).map_err(failed)?;
        }
        for Range { offset, len } in holes {
            file.punch(offset, len).map_err(failed)?;
        }
        Ok(())
    }

    /// cladefs: makes the directory `dir` of `staging` and those it is in, those there already
    /// left as they are, each counted in its footprint as it is made.
    fn made_dirs(staging: &local_cell::HostDir, dir: &str) -> std::io::Result<()> {
        use local_cell::Dir as _;
        let mut at = String::new();
        for name in dir.split('/') {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(name);
            match staging.create_dir(&at) {
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                made => made?,
            }
        }
        Ok(())
    }

    /// Takes the migration `request` names over, or refuses it, as the module's docs say.
    pub(crate) async fn hand_over(
        daemon: &Arc<Daemon>,
        request: MigrateHandOverRequest,
    ) -> Result<bool, Status> {
        to_the_end(take_over(Arc::clone(daemon), request)).await
    }

    async fn take_over(
        daemon: Arc<Daemon>,
        request: MigrateHandOverRequest,
    ) -> Result<bool, Status> {
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
        if local_cell::upload::blocking(|| host.accepted(migration)) {
            // Accepted before: what is left of noting it so, as asking again finishes it (board
            // 5721): its acceptance made durable again, and the cell noted taken. A failure past
            // the acceptance's writing left it there but not for good; answered yes at once, a
            // crash then lost it, and its entry, its cell's source having let the cell go.
            local_cell::upload::blocking(|| -> Result<(), Status> {
                if let Some(child) = host.accepted_cell(migration).map_err(journal)? {
                    host.accept(migration, child)
                        .map_err(|err| status("remembering it accepted", err))?;
                }
                Ok(())
            })?;
            noted_taken(&daemon, &host, migration).await?;
            return Ok(true);
        }
        let Some((_taking, entry)) = held(&daemon, &host, migration).await? else {
            log::warn!("cladefsd: refusing migration {migration:x}: not taking it");
            return Ok(false);
        };
        heard(&daemon, migration);
        let staged = staging(&entry.path, migration);
        // Taken into place before, what followed failing: what is left of taking it, as asking
        // again finishes it (board 5721). A failure reading it says neither: answered so, asked
        // again later. Its files read whole for their CRCs off the runtime's workers, as all of
        // the hand-over's disk work is (board 5378).
        let checked = {
            let (host, entry, staged) = (host.clone(), entry.clone(), staged.clone());
            off(move || {
                let moved_before = !host_fs::exists(&staged)
                    && host.holds(&entry).map_err(|err| status("reading the cell taken", err))?;
                let lacking = match entry.receiving() {
                    Some((_, taken)) if taken != token => {
                        Some(format!("it names token {token:x}, of another take of it"))
                    }
                    _ if moved_before => None,
                    _ => match lacking(&staged, &files) {
                        Some(lacking) => Some(lacking),
                        // Whole as named, it must open as the cell prepared, as its opening here
                        // will, before it goes into place (board 8455): of another format
                        // (`ENOTSUP` at its first opening), or not opening at all, it is refused,
                        // its source serving the cell again, whose only copy it is.
                        None => ephemeral_host::not_its(&staged, &entry)
                            .unwrap_or_else(|err| Some(format!("it does not open: {err:#}"))),
                    },
                };
                Ok((moved_before, lacking))
            })
        };
        let (moved_before, lacking) = checked.await?;
        if let Some(lacking) = lacking {
            log::warn!("cladefsd: refusing migration {migration:x}: {lacking}");
            refuse(&daemon, &host, &entry).await?;
            return Ok(false);
        }
        let into = entry.path.clone();
        // Its staging directory closed before it goes into place, its footprint the cell's.
        daemon.migrations.staging.lock().unwrap().remove(&migration);
        let moved = move || -> std::io::Result<()> {
            if !moved_before {
                sync_tree(&staged)?;
                host_fs::rename(&staged, &into)?;
            }
            host_fs::File::open(into.parent().unwrap_or(Path::new(".")))?.sync_all()
        };
        off(move || moved().map_err(|err| status("taking the cell", err))).await?;
        crate::faults::crashpoint!("migration::renamed");
        // Its reference written again before it is accepted (`targetRefersFirst`): a restart since
        // its prepare may have retired it, its entry's removal failing after (board 5721).
        let taken = Entry { migrating: None, ..entry };
        let bucket =
            crate::host_cell::bucket(&taken.bucket).map_err(|err| status("its bucket", err))?;
        let (origin, child) = (taken.origin, taken.child);
        let referring =
            bucket_cell::lifecycle::refer_ephemeral(&*bucket, origin, host.incarnation(), child);
        match tokio::time::timeout(BUCKET_BOUND, referring).await {
            Ok(referred) => referred.map_err(|err| status("its reference", err))?,
            Err(_) => {
                return Err(Status::unavailable(format!("referring to {origin:?} timed out")));
            }
        }
        off(move || {
            fail::fail_point!("migration::accepting");
            host.accept(migration, taken.child)
                .map_err(|err| status("remembering it accepted", err))?;
            fail::fail_point!("migration::noting_taken", |_| {
                Err(status("noting the cell", "failpoint migration::noting_taken"))
            });
            host.note(&taken).map_err(|err| status("noting the cell", err))
        })
        .await?;
        forget(&daemon, migration);
        Ok(true)
    }

    /// Runs `work`, of the host's disk, off the runtime's workers ([`run::off_the_runtime`]): a
    /// hand-over's reads and syncs held a bucket runtime's worker for as long as a cell's files
    /// took to read whole and sync (board 5378).
    ///
    /// [`run::off_the_runtime`]: super::super::run::off_the_runtime
    async fn off<T: Send + 'static>(
        work: impl FnOnce() -> Result<T, Status> + Send + 'static,
    ) -> Result<T, Status> {
        super::super::run::off_the_runtime(work)
            .await
            .map_err(|err| status("its disk work", err))?
    }

    /// Gives the migration `request` names up, if this host holds it not yet.
    pub(crate) async fn abort(
        daemon: &Arc<Daemon>,
        request: MigrateAbortRequest,
    ) -> Result<(), Status> {
        to_the_end(give_up(Arc::clone(daemon), request.migration)).await
    }

    async fn give_up(daemon: Arc<Daemon>, migration: u64) -> Result<(), Status> {
        let host = host()?;
        if local_cell::upload::blocking(|| host.accepted(migration)) {
            return Ok(());
        }
        // Accepted while this waited its turn: refused not ([`refuse`]).
        let Some((_taking, entry)) = held(&daemon, &host, migration).await? else { return Ok(()) };
        refuse(&daemon, &host, &entry).await?;
        count(&COUNTS.taken_given_up, 1);
        Ok(())
    }

    /// Gives up the migration taking `entry`'s cell: its cell, if taken into place, then its
    /// entry and reference, then its files. Dropped part way, a staging an entry names is one a
    /// retry of its migration may take for whole, a kept segment's file made again of holes alone
    /// (board 5721).
    ///
    /// A migration this host accepted is never given up, its entry a migration's still, its noting
    /// of the cell taken having failed after it was accepted: the cell taken into place is the one
    /// copy once its source hears so. It is noted taken instead. An abort, or the reaper, waiting
    /// behind that hand-over dropped it (Codex's review, hotbed review 83; board 5372).
    async fn refuse(
        daemon: &Daemon,
        host: &ephemeral_host::Host,
        entry: &Entry,
    ) -> Result<(), Status> {
        if let Some((migration, _)) = entry.receiving()
            && local_cell::upload::blocking(|| host.accepted(migration))
        {
            log::warn!("cladefsd: migration {migration:x} accepted: noted taken, not given up");
            // Its acceptance made durable again first, as a hand-over asked again makes it: its
            // writing may have failed past its rename, before its directory's sync, which a crash
            // then undid, the cell noted taken here while its source, refused, served it again
            // (Codex's review, hotbed review 86).
            local_cell::upload::blocking(|| -> Result<(), Status> {
                host.accept(migration, entry.child)
                    .map_err(|err| status("remembering it accepted", err))?;
                let taken = Entry { migrating: None, ..entry.clone() };
                host.note(&taken).map_err(|err| status("noting the cell", err))
            })?;
            forget(daemon, migration);
            return Ok(());
        }
        host.drop_moved(entry).map_err(journal)?;
        retire(host, entry).await?;
        if let Some((migration, _)) = entry.receiving() {
            let staged = staging(&entry.path, migration);
            forget(daemon, migration);
            if let Err(err) = local_cell::upload::blocking(|| drop_staged(&staged)) {
                log::warn!("cladefsd: dropping {}, refused: {err}", staged.display());
            }
        }
        Ok(())
    }

    /// Removes the directory `staged`, if there, durably: no crash brings it back. cladefs: as a
    /// cell directory goes (`local_cell::remove_cell_dir`), so that what its files took of the
    /// disk, counted as they were staged, goes with them (board 8451).
    fn drop_staged(staged: &Path) -> std::io::Result<()> {
        if !host_fs::exists(staged) {
            return Ok(());
        }
        local_cell::remove_cell_dir(staged).map_err(std::io::Error::other)
    }

    /// Notes migration `migration` goes on, its source there still: `NOT_FOUND` if this host
    /// takes it no more (board 5734).
    pub(crate) fn renew(daemon: &Daemon, migration: u64) -> Result<(), Status> {
        let host = host()?;
        if local_cell::upload::blocking(|| host.receiving(migration)).map_err(journal)?.is_none() {
            return Err(Status::not_found(format!("no migration {migration:x}")));
        }
        heard(daemon, migration);
        Ok(())
    }

    /// Forgets this host accepted migration `migration`: its source let the cell go, and asks no
    /// more (board 5734). The cell noted taken first, durably ([`noted_taken`]).
    pub(crate) async fn release(daemon: &Arc<Daemon>, migration: u64) -> Result<(), Status> {
        let host = host()?;
        noted_taken(daemon, &host, migration).await?;
        local_cell::upload::blocking(|| host.release(migration))
            .map_err(|err| status("forgetting it accepted", err))
    }

    /// Gives up, every so often, each migration being taken on `daemon` that it heard nothing of
    /// for `idle` (board 5734; specs/ephemeral, `TargetAbort`): its source crashed, or the source's
    /// abort lost. Heard of for the first time, a migration's wait begins.
    pub(crate) async fn reap(daemon: Arc<Daemon>, idle: Duration) {
        loop {
            tokio::time::sleep((idle / 4).max(Duration::from_millis(250))).await;
            if let Err(err) = reap_idle(&daemon, idle).await {
                log::warn!("cladefsd: giving up migrations idle too long: {err}");
            }
        }
    }

    async fn reap_idle(daemon: &Arc<Daemon>, idle: Duration) -> Result<(), Status> {
        let host = host()?;
        let taken: Vec<u64> = local_cell::upload::blocking(|| host.entries())
            .map_err(journal)?
            .iter()
            .filter_map(Entry::receiving)
            .map(|(id, _)| id)
            .collect();
        // cladefs: what is kept of migrations the journal names being taken no more (board 8451).
        daemon.migrations.prepared.lock().unwrap().retain(|migration| taken.contains(migration));
        daemon.migrations.staging.lock().unwrap().retain(|migration, _| taken.contains(migration));
        let idle_ones: Vec<u64> = {
            let mut heard = daemon.migrations.heard.lock().unwrap();
            heard.retain(|migration, _| taken.contains(migration));
            let now = Instant::now();
            let quiet = |migration: &u64| now - *heard.entry(*migration).or_insert(now) >= idle;
            taken.into_iter().filter(quiet).collect()
        };
        for migration in idle_ones {
            let Some((_taking, entry)) = held(daemon, &host, migration).await? else { continue };
            // Heard of while this waited its turn.
            let last = daemon.migrations.heard.lock().unwrap().get(&migration).copied();
            if last.is_some_and(|last| last.elapsed() < idle) {
                continue;
            }
            log::warn!(
                "cladefsd: migration {migration:x} of {} idle {idle:?}: given up",
                entry.child
            );
            refuse(daemon, &host, &entry).await?;
            count(&COUNTS.taken_given_up, 1);
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
            // Unread: said below, with the error.
            if found.as_ref().ok() != Some(len) {
                return Some(format!("{path}: {found:?} bytes, not {len}"));
            }
            // Unread: no CRC, which differs from the one named, said below.
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

        /// Board 8447: a peer's `to` names a cell's path only as a name directly in the directory
        /// of migrated cells, as its path resolves: an absolute path elsewhere, a relative one, one
        /// through `.` or `..`, deeper, of a name a staging's, or too long, the directory itself, a
        /// path of another directory sharing its name's start, none.
        #[test]
        fn peers_cell_paths_are_new_names_in_the_directory_of_migrated_cells() {
            let root = Path::new("/cells/migrated");
            let longest = "n".repeat(NAME_MOST);
            for name in ["vm1", "a.b", "-x", longest.as_str()] {
                let to = format!("/cells/migrated/{name}");
                assert_eq!(cell_path(root, &to), Some(root.join(name)), "{to:?}");
            }
            let too_long = format!("/cells/migrated/{longest}n");
            for to in [
                "",
                "vm1",
                "cells/migrated/vm1",
                "/cells/migrated",
                "/cells/migrated/",
                "/cells/migrated/.",
                "/cells/migrated/..",
                "/cells/migrated/../x",
                "/cells/migrated/./x",
                "/cells/migrated/a/b",
                "/cells/migrated/a/",
                "/cells/migrated//a",
                "/cells/migrated/.a.migrating-1",
                "/cells/migrated/a\0b",
                "/cells/migratedx",
                "/cells/migrated-other/a",
                "/etc/systemd/system/cladefsd.service.d",
                "//cells/migrated/a",
                too_long.as_str(),
            ] {
                assert_eq!(cell_path(root, to), None, "{to:?}");
            }
        }

        /// Board 8451: what a peer stages is counted in its staging directory's footprint as it is
        /// written, 10,000 files of it, and admitted by the disk governor first: a file whose
        /// length the disk has no room for, a peer's terabyte of holes, refused
        /// (`RESOURCE_EXHAUSTED`) before it is made; dropping the staging gives its count back.
        #[test]
        fn what_peers_stage_is_counted_and_admitted() {
            use bucket_cell::host_disk::GRANULE;
            use tonic::Code;
            let dir = tempfile::tempdir().unwrap();
            let staged = dir.path().join("cell.migrating-1");
            host_fs::create_dir(&staged).unwrap();
            let staging = local_cell::HostDir::new(&staged).unwrap();
            let footprint = staging.footprint().clone();
            let disk = footprint.disk().unwrap().clone();
            let before = disk.cells.bytes();
            let piece = |path: String, len: u64| MigrateFileRequest {
                migration: 1,
                path,
                offset: 0,
                bytes: vec![7; 100],
                len,
                holes: Vec::new(),
            };
            const FILES: u64 = 10_000;
            for file in 0..FILES {
                write(&staging, piece(format!("extents/{file}"), 100)).unwrap();
            }
            // Each file a granule, and its directory.
            assert_eq!(footprint.bytes(), (FILES + 1) * GRANULE);
            assert_eq!(disk.cells.bytes() - before, (FILES + 1) * GRANULE);
            disk.govern(disk.cells.bytes() + (64 << 20), 0, false);
            let refused = write(&staging, piece("huge".into(), 1 << 40)).unwrap_err();
            assert_eq!(refused.code(), Code::ResourceExhausted, "{refused:?}");
            assert!(!staged.join("huge").exists(), "a file refused made");
            write(&staging, piece("small".into(), 1 << 20)).unwrap();
            assert_eq!(std::fs::metadata(staged.join("small")).unwrap().len(), 1 << 20);
            disk.govern(u64::MAX, 0, false);
            drop(staging);
            drop_staged(&staged).unwrap();
            assert!(!staged.exists());
            assert_eq!(disk.cells.bytes(), before);
        }

        /// The daemon's environment of a test of a target's prepares, in `dir`: its bucket in
        /// memory, its ephemeral cells' directory, and `dir`, as it resolves, its directory of
        /// migrated cells, which it answers.
        fn target_host(dir: &Path) -> (Arc<ephemeral_host::Host>, PathBuf) {
            let root = host_fs::canonicalize(dir).unwrap();
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment yet.
            unsafe {
                std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
                std::env::set_var("CLADEFSD_TOKEN", "1");
                std::env::set_var("CLADEFSD_EPHEMERAL_CELLS", &root);
            }
            (ephemeral_host::open(&root.join("ephemeral")).unwrap(), root)
        }

        /// A prepare of migration `migration` of cell 1/7, of 1/2's segment 3, to `to`, as a
        /// source of this build in this host's bucket asks it.
        fn prepare_of(migration: u64, to: &Path) -> MigratePrepareRequest {
            MigratePrepareRequest {
                migration,
                tenant: 1,
                cell: 7,
                parent_tenant: 1,
                parent_cell: 2,
                at: 3,
                parent_epoch: 1,
                bucket: "memory:///cells".into(),
                to: to.display().to_string(),
                epoch: 1,
                #[allow(deprecated)]
                formats: String::new(),
                versions: Some(crate::cladefsd::peers::current_formats()),
            }
        }

        /// Boards 8443, 8447 and 8455: a prepare naming what this host is not is refused before
        /// anything is noted, made or opened: formats not its own, an older build's naming none
        /// among them (`FAILED_PRECONDITION`); a bucket not its own, a `file://` directory not
        /// made by it, nor any of 10,000 others opened (`FAILED_PRECONDITION`); a `to` not a new
        /// name in its directory of migrated cells, a link in it to elsewhere and a path through
        /// a link to it among them (`INVALID_ARGUMENT`), or a name there already, the link to
        /// nowhere among them (`ALREADY_EXISTS`).
        #[test]
        fn prepares_naming_what_this_host_is_not_are_refused_first() {
            use tonic::Code;
            let dir = tempfile::tempdir().unwrap();
            let (host, root) = target_host(dir.path());
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
            std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();
            let alias = outside.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            let listed = |dir: &Path| -> Vec<String> {
                let mut names: Vec<String> = host_fs::read_dir(dir)
                    .unwrap()
                    .map(|found| found.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                names
            };
            let daemon = Daemon::new();
            let (before, opened) = (listed(&root), crate::host_cell::buckets_opened());
            let refused = |request: MigratePrepareRequest| {
                let prepared = futures::executor::block_on(prepare(&daemon, request));
                prepared.map_err(|status| (status.code(), status.message().to_owned()))
            };
            let cell = root.join("vm");
            let ours = crate::cladefsd::peers::current_formats();
            let other = |versions: Option<crate::cladefsd::peers::proto::Formats>| {
                MigratePrepareRequest { versions, ..prepare_of(0x11, &cell) }
            };
            assert_eq!(
                refused(other(None)),
                Err((
                    Code::FailedPrecondition,
                    "migration 11: the source named no format versions (older build): refused"
                        .to_owned()
                ))
            );
            for (versions, family) in [
                (
                    crate::cladefsd::peers::proto::Formats { cell_directory: 15, ..ours },
                    "cell directory version 15",
                ),
                (
                    crate::cladefsd::peers::proto::Formats { cell_records: 99, ..ours },
                    "cell records version 99",
                ),
                (
                    crate::cladefsd::peers::proto::Formats { extent_frame: 2, ..ours },
                    "extent frame version 2",
                ),
                (
                    crate::cladefsd::peers::proto::Formats { bucket_layout: 1, ..ours },
                    "bucket layout version 1",
                ),
            ] {
                let (code, message) = refused(other(Some(versions))).unwrap_err();
                assert_eq!(code, Code::FailedPrecondition, "{family}: {message}");
                assert!(message.contains(family), "{family} not in {message:?}");
            }
            let made = outside.path().join("made");
            let file = format!("file://{}/", made.display());
            let buckets = (0..10_000).map(|bucket| format!("memory:///other{bucket}"));
            for bucket in [file, "memory:///cells/".to_owned()].into_iter().chain(buckets) {
                let request = MigratePrepareRequest { bucket, ..prepare_of(0x13, &cell) };
                assert_eq!(
                    refused(request),
                    Err((
                        Code::FailedPrecondition,
                        "migration 13: its bucket is not this host's".to_owned()
                    ))
                );
            }
            assert!(!made.exists(), "the file bucket's directory made");
            assert_eq!(crate::host_cell::buckets_opened(), opened, "buckets opened");
            let out = outside.path().join("x");
            for to in [
                out.clone(),
                root.clone(),
                root.join("link").join("x"),
                alias.join("x"),
                root.join("a/b"),
                root.join(".vm.migrating-1"),
                root.join("ephemeral").join("cells"),
            ] {
                let message = format!(
                    "migration 15: {:?} is no new name in {}",
                    to.display().to_string(),
                    root.display()
                );
                assert_eq!(
                    refused(prepare_of(0x15, &to)),
                    Err((Code::InvalidArgument, message)),
                    "{}",
                    to.display()
                );
            }
            for name in ["link", "dangling", "ephemeral"] {
                let to = root.join(name);
                assert_eq!(
                    refused(prepare_of(0x17, &to)),
                    Err((Code::AlreadyExists, format!("{} is there", to.display()))),
                    "{name}"
                );
            }
            assert_eq!(host.entries().unwrap(), vec![], "nothing noted");
            assert_eq!(listed(&root), before, "nothing made");
            assert_eq!(listed(outside.path()), vec!["alias".to_owned()], "nothing made");
        }

        /// Board 8455: a hand-over naming every file it staged whole is refused if the cell they
        /// make does not open as the cell prepared: a cell directory of another build's format,
        /// its description's first line another, which its first opening would refuse (`ENOTSUP`)
        /// once its source let go of it; and one whose description, of the cell prepared and this
        /// format, is longer than a description is, which the check reads not (board 8455's
        /// review: a staged file's length is the peer's to name). Nothing goes into place, and
        /// what it took goes, its reference and entry with it.
        #[test]
        fn hand_overs_of_cells_that_do_not_open_are_refused() {
            let daemon = Daemon::new();
            let dir = tempfile::tempdir().unwrap();
            let (host, root) = target_host(dir.path());
            // Its writes of a file's pieces run off a runtime's workers.
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let runtime = runtime.unwrap();
            let other = b"cladefs extent store 15\nblock_size 4096\ncell 1/7\n".to_vec();
            let child = CellId { tenant: 1, id: 7 };
            let long = format!(
                "{}{}\nblock_size 4096\ncell {child}\ncapacity 4096\nbucket memory:///cells\n\
                 ephemeral yes\npadding {}\n",
                local_cell::DESCRIPTION_HEADER,
                local_cell::FORMAT.current,
                "x".repeat(64 << 10)
            );
            for (migration, bytes) in [(0x21, other), (0x23, long.into_bytes())] {
                let len = bytes.len() as u64;
                let cell = root.join("vm");
                let prepared = runtime.block_on(prepare(&daemon, prepare_of(migration, &cell)));
                let (incarnation, token) = prepared.unwrap();
                let staged = staging(&cell, migration);
                assert!(staged.is_dir());
                let crc = Some(extent_frame::crc(&bytes));
                let path = local_cell::DESCRIPTION.to_owned();
                let holes = Vec::new();
                let piece = MigrateFileRequest { migration, path, offset: 0, bytes, len, holes };
                runtime.block_on(file(&daemon, piece)).unwrap();
                let path = local_cell::DESCRIPTION.to_owned();
                let files = vec![FileDigest { path, len, crc }];
                let request = MigrateHandOverRequest { migration, incarnation, files, token };
                let answered = runtime.block_on(hand_over(&daemon, request)).map_err(|s| s.code());
                assert_eq!(answered, Ok(false), "{migration:x}");
                assert!(!cell.exists(), "taken into place");
                assert!(!staged.exists(), "what it took kept");
                assert_eq!(host.entries().unwrap(), vec![], "its entry kept");
                assert!(!host.accepted(migration));
            }
        }

        /// The cell 1/7's locks held, by requests of its migrations and those waiting for it.
        fn holders(daemon: &Daemon) -> usize {
            let child = CellId { tenant: 1, id: 7 };
            daemon.migrations.taking.lock().unwrap().get(&child).map_or(0, Arc::strong_count)
        }

        /// Waits until `done` holds, 30 s at most.
        fn until(what: &str, done: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "{what}");
                std::thread::yield_now();
            }
        }

        /// Codex's reviews of board 8577 (hotbed reviews 83, 86): a request of a migration's file
        /// its source gave up on at its bound, dropped as its write runs off the runtime, holds the
        /// cell until the write ends: the next request of the migration waits its turn rather than
        /// overtaking it, as a hand-over's last copy of the file would, the late write then landing
        /// in the cell taken over. Requests given up on while they wait their turn hold nothing.
        #[test]
        fn file_requests_given_up_on_hold_the_cell_until_written() {
            use std::sync::Barrier;
            use std::sync::atomic::{AtomicUsize, Ordering};
            let daemon = Daemon::new();
            let dir = tempfile::tempdir().unwrap();
            let (_host, root) = target_host(dir.path());
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let (migration, cell) = (0x41, root.join("vm"));
            runtime.block_on(prepare(&daemon, prepare_of(migration, &cell))).unwrap();
            let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
            let writes = Arc::new(AtomicUsize::new(0));
            let (entering, releasing) = (entered.clone(), release.clone());
            fail::cfg_callback("migration::writing", move || {
                // The first write held until let go; the others as they come.
                if writes.fetch_add(1, Ordering::SeqCst) == 0 {
                    entering.wait();
                    releasing.wait();
                }
            })
            .unwrap();
            let piece = |path: &str| MigrateFileRequest {
                migration,
                path: path.to_owned(),
                offset: 0,
                bytes: vec![7; 100],
                len: 100,
                holes: Vec::new(),
            };
            let d = Arc::clone(&daemon);
            let req = piece("first");
            let first = runtime.spawn(async move { file(&d, req).await });
            entered.wait();
            first.abort();
            until("the first request, given up on, never ended", || first.is_finished());
            // Requests given up on before their turn hold nothing, their pieces let go of (hotbed
            // review 86): the cell's lock is the first write's alone again.
            let mut waiting = Vec::new();
            for queued in 0..3 {
                let d = Arc::clone(&daemon);
                let req = piece(&format!("waiting{queued}"));
                waiting.push(runtime.spawn(async move { file(&d, req).await }));
                until("a request never waited its turn", || holders(&daemon) == 3 + queued);
            }
            for given_up in &waiting {
                given_up.abort();
            }
            until("requests given up on before their turn wait still", || holders(&daemon) == 2);
            let d = Arc::clone(&daemon);
            let req = piece("second");
            let second = runtime.spawn(async move { file(&d, req).await });
            until("the second neither waited nor ended", || {
                holders(&daemon) == 3 || second.is_finished()
            });
            let overtook = second.is_finished();
            release.wait();
            let second = runtime.block_on(second).unwrap().map_err(|status| status.code());
            fail::remove("migration::writing");
            assert!(!overtook, "the second request overtook the first's write");
            assert_eq!(second, Ok(()));
            let staged = staging(&cell, migration);
            assert_eq!(std::fs::read(staged.join("first")).unwrap(), vec![7; 100]);
        }

        /// Codex's review of board 8577 (hotbed review 83; board 5372): a migration accepted is
        /// never given up. An abort waiting behind a hand-over that accepts the migration, whose
        /// noting of the cell taken then fails (failpoint `migration::noting_taken`), its entry a
        /// migration's still, gave up the cell taken into place, the one copy once its source heard
        /// the hand-over accepted again. It is noted taken instead, and accepted again.
        #[test]
        fn migrations_accepted_are_never_given_up() {
            use std::sync::Barrier;
            use tonic::Code;
            let daemon = Daemon::new();
            let dir = tempfile::tempdir().unwrap();
            let (host, root) = target_host(dir.path());
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let (migration, cell) = (0x43, root.join("vm"));
            let prepared = runtime.block_on(prepare(&daemon, prepare_of(migration, &cell)));
            let (incarnation, token) = prepared.unwrap();
            let child = CellId { tenant: 1, id: 7 };
            let description = format!(
                "{}{}\nblock_size 4096\ncell {child}\ncapacity 4096\nbucket memory:///cells\n\
                 ephemeral yes\n",
                local_cell::DESCRIPTION_HEADER,
                local_cell::FORMAT.current,
            )
            .into_bytes();
            let (len, crc) = (description.len() as u64, Some(extent_frame::crc(&description)));
            let path = local_cell::DESCRIPTION.to_owned();
            let holes = Vec::new();
            let bytes = description;
            let piece = MigrateFileRequest { migration, path, offset: 0, bytes, len, holes };
            runtime.block_on(file(&daemon, piece)).unwrap();
            let files = vec![FileDigest { path: local_cell::DESCRIPTION.to_owned(), len, crc }];
            let request = MigrateHandOverRequest { migration, incarnation, files, token };
            let (entered, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
            let (entering, releasing) = (entered.clone(), release.clone());
            fail::cfg_callback("migration::accepting", move || {
                entering.wait();
                releasing.wait();
            })
            .unwrap();
            fail::cfg("migration::noting_taken", "1*return").unwrap();
            let d = Arc::clone(&daemon);
            let req = request.clone();
            let handing = runtime.spawn(async move { hand_over(&d, req).await });
            entered.wait();
            let d = Arc::clone(&daemon);
            let aborting =
                runtime.spawn(async move { abort(&d, MigrateAbortRequest { migration }).await });
            until("the abort never waited behind the hand-over", || holders(&daemon) == 3);
            fail::remove("migration::accepting");
            release.wait();
            let handed = runtime.block_on(handing).unwrap().map_err(|status| status.code());
            let aborted = runtime.block_on(aborting).unwrap().map_err(|status| status.code());
            fail::remove("migration::noting_taken");
            assert_eq!(handed, Err(Code::Internal), "its noting of the cell taken failing");
            assert_eq!(aborted, Ok(()));
            assert!(host.accepted(migration));
            assert!(cell.exists(), "the cell accepted, given up");
            let noted = host.entry(child).unwrap();
            assert!(matches!(noted, Some(Entry { migrating: None, .. })), "{noted:?}");
            let again =
                runtime.block_on(hand_over(&daemon, request)).map_err(|status| status.code());
            assert_eq!(again, Ok(true));
            assert!(cell.exists());
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

        /// Board 5997: streaming `crc_of` matches full-buffer `extent_frame::crc` across empty,
        /// unaligned, and multi-chunk files, and `target::write` writes bypassing the host OS page
        /// cache (failing hard when `direct::bypass` is refused).
        #[test]
        fn streaming_crc_and_direct_target_write() {
            let dir = tempfile::tempdir().unwrap();
            let staging = local_cell::HostDir::new(dir.path()).unwrap();
            for len in [0usize, 13, 4096, 4097, CRC_CHUNK, CRC_CHUNK * 2 + 123] {
                let data: Vec<u8> =
                    (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 5)) as u8).collect();
                let name = format!("f-{len}");
                let request = MigrateFileRequest {
                    migration: 1,
                    path: name.clone(),
                    offset: 0,
                    bytes: data.clone(),
                    len: len as u64,
                    holes: Vec::new(),
                };
                write(&staging, request).unwrap();
                let path = dir.path().join(&name);
                assert_eq!(std::fs::read(&path).unwrap(), data, "len {len}");
                assert_eq!(crc_of(&path).unwrap(), extent_frame::crc(&data), "crc len {len}");
            }
            // Refused when the filesystem cannot bypass the host OS page cache.
            fail::cfg("direct::bypass", "return").unwrap();
            let err_crc = crc_of(&dir.path().join("f-13"));
            let err_write = write(
                &staging,
                MigrateFileRequest {
                    migration: 1,
                    path: "refused".to_owned(),
                    offset: 0,
                    bytes: b"hello".to_vec(),
                    len: 5,
                    holes: Vec::new(),
                },
            );
            fail::cfg("direct::bypass", "off").unwrap();
            assert!(err_crc.is_err(), "crc_of must fail hard without page-cache bypass");
            assert!(err_write.is_err(), "target::write must fail hard without page-cache bypass");
            // Hostile peer offsets that would overflow `direct::write_at`'s block rounding are
            // rejected cleanly with InvalidArgument rather than panicking.
            for (bad_offset, bad_len) in
                [(u64::MAX, 1), (u64::MAX - 10, 1), (i64::MAX as u64, 1), (0, u64::MAX)]
            {
                let status = write(
                    &staging,
                    MigrateFileRequest {
                        migration: 1,
                        path: "bad-offset".to_owned(),
                        offset: bad_offset,
                        bytes: vec![1],
                        len: bad_len,
                        holes: Vec::new(),
                    },
                )
                .unwrap_err();
                assert_eq!(status.code(), tonic::Code::InvalidArgument, "{bad_offset} {bad_len}");
            }
        }
    }
}
