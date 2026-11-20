// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-443.5, TASK-443.14: what a sandbox's life asks of its bucket, by phase: a restore (a fork
//! of a version and the fork's first open) on a host that never saw the version and on one that has it
//! open, a small write and its commit, a clean close, a reopen, a snapshot (a version) of the written
//! fork, and its deletion, on a base of as many extents as asked for. A measurement, not a check.
//!
//! The bucket is one in memory behind [`Recording`], which notes every request that reaches the
//! object store, the ones `ObjectBucket::stats` leaves out too (the collector's marks), with its
//! key, its bytes and when it began and ended, and behind a fixed latency
//! (`bucket_cell::faults::Faulty`). So each phase says, beside its requests by kind and its
//! bytes up and down: how many requests were in flight at once at most; how long at least one was
//! in flight, which over the latency is how many round trips it made one after another; and the
//! keys it read more than once.
//!
//! Its requests, bytes and round trips are the code's, whatever the build. Its times beside the
//! bucket's (`wall-ms` less `bucket-ms`: decoding and encoding checkpoints, opening stores) are a
//! debug build's: cladefs's tests don't build in release (TASK-320).

use super::tests::{idle, kept_in, pattern, reopened, url_of};
use crate::host_cell::{self, DEFAULT_TENANT};
use crate::open_cell::Opening;
use crate::{CellId, ExtentId, OpenCell, VolatileSlice};
use base32_id::Padded;
use bucket_cell::faults::{Fault, Faults, Faulty};
use bucket_cell::object_bucket::ObjectBucket;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use object_store::path::Path as Key;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

/// How many stores are closed and opened again, a restore after each, to see which of them reads
/// the base's checkpoint anew: twice what the bucket keeps decoded, and two.
const RESUMES: u32 = 34;

/// How many extents the base's commits hold each: a segment each.
const COMMIT_EVERY: u64 = 1024;

/// A request that reached the object store.
#[derive(Clone, Debug)]
struct Event {
    /// Its kind: `get`, `get-range`, `head`, `list`, `list-level`, `create`, `update`,
    /// `overwrite`, `delete`.
    kind: &'static str,
    /// Its object's key, or the prefix it listed.
    key: String,
    up: u64,
    down: u64,
    /// When it began and was answered, since the store was made.
    began: Duration,
    ended: Duration,
}

/// The requests so far, in the order they were answered.
#[derive(Debug)]
struct Log {
    origin: Instant,
    events: Mutex<Vec<Event>>,
}

/// A request in flight, noted as it drops, answered.
struct Flight {
    log: Arc<Log>,
    kind: &'static str,
    key: String,
    up: u64,
    down: u64,
    began: Duration,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let event = Event {
            kind: self.kind,
            key: std::mem::take(&mut self.key),
            up: self.up,
            down: self.down,
            began: self.began,
            ended: self.log.origin.elapsed(),
        };
        self.log.events.lock().unwrap().push(event);
    }
}

/// An object store that notes every request made of `objects` ([`Log`]).
#[derive(Debug)]
struct Recording {
    objects: Arc<dyn ObjectStore>,
    log: Arc<Log>,
}

impl Recording {
    fn flight(&self, kind: &'static str, key: String) -> Flight {
        let began = self.log.origin.elapsed();
        Flight { log: self.log.clone(), kind, key, up: 0, down: 0, began }
    }
}

impl std::fmt::Display for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Recording({})", self.objects)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Recording {
    async fn put_opts(
        &self,
        location: &Key,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let kind = match opts.mode {
            PutMode::Create => "create",
            PutMode::Update(_) => "update",
            PutMode::Overwrite => "overwrite",
        };
        let mut flight = self.flight(kind, location.to_string());
        flight.up = payload.content_length() as u64;
        self.objects.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Key,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let _flight = self.flight("multipart", location.to_string());
        self.objects.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Key,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let kind = match (options.head, options.range.is_some()) {
            (true, _) => "head",
            (false, true) => "get-range",
            (false, false) => "get",
        };
        let head = options.head;
        let mut flight = self.flight(kind, location.to_string());
        let got = self.objects.get_opts(location, options).await;
        if let (Ok(got), false) = (&got, head) {
            flight.down = got.range.end - got.range.start;
        }
        got
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Key>>,
    ) -> BoxStream<'static, object_store::Result<Key>> {
        let flight = self.flight("delete", String::new());
        let flight = Mutex::new(flight);
        let deleted = self.objects.delete_stream(locations);
        deleted
            .inspect(move |deleted| {
                if let Ok(key) = deleted {
                    flight.lock().unwrap().key = key.to_string();
                }
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Key>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let flight = self.flight("list", prefix.map(Key::to_string).unwrap_or_default());
        // Noted as the listing, read to its end, drops.
        self.objects.list(prefix).inspect(move |_| _ = &flight).boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Key>) -> object_store::Result<ListResult> {
        let _flight = self.flight("list-level", prefix.map(Key::to_string).unwrap_or_default());
        self.objects.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Key,
        to: &Key,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        let _flight = self.flight("copy", to.to_string());
        self.objects.copy_opts(from, to, options).await
    }
}

/// What a phase asked of the bucket.
#[derive(Clone, Debug, Default)]
struct Asked {
    /// How long it took, bucket and all.
    wall: Duration,
    /// Its requests, by kind.
    kinds: BTreeMap<&'static str, u64>,
    up: u64,
    down: u64,
    /// How long at least one request was in flight.
    busy: Duration,
    /// The most in flight at once.
    at_once: usize,
    /// The longest chain of its requests each begun once the one before it was answered: its
    /// round trips one after another, counted, as the latency over its busy time estimates them
    /// (TASK-449).
    chain: usize,
    /// The keys it read (`get`, `get-range`, `head`) more than once, and how often.
    again: BTreeMap<String, u64>,
}

impl Asked {
    fn of(events: &[Event], wall: Duration) -> Self {
        let mut asked = Self { wall, ..Self::default() };
        let mut read: BTreeMap<String, u64> = BTreeMap::new();
        for event in events {
            *asked.kinds.entry(event.kind).or_default() += 1;
            asked.up += event.up;
            asked.down += event.down;
            if matches!(event.kind, "get" | "get-range" | "head") {
                *read.entry(format!("{} {}", event.kind, event.key)).or_default() += 1;
            }
        }
        asked.again = read.into_iter().filter(|(_, count)| *count > 1).collect();
        asked.chain = chain(events);
        // The time a request was in flight, and the most at once, by their beginnings and ends.
        let mut edges: Vec<(Duration, i32)> =
            (events.iter()).flat_map(|event| [(event.began, 1), (event.ended, -1)]).collect();
        edges.sort();
        let (mut flying, mut since) = (0, Duration::ZERO);
        for (at, edge) in edges {
            if flying == 0 {
                since = at;
            }
            flying += edge;
            asked.at_once = asked.at_once.max(flying as usize);
            if flying == 0 {
                asked.busy += at - since;
            }
        }
        asked
    }

    fn requests(&self) -> u64 {
        self.kinds.values().sum()
    }

    /// Its requests and its round trips one after another ([`Asked::chain`]).
    fn cost(&self) -> (u64, usize) {
        (self.requests(), self.chain)
    }

    fn add(&mut self, other: &Self) {
        self.wall += other.wall;
        for (kind, count) in &other.kinds {
            *self.kinds.entry(kind).or_default() += count;
        }
        (self.up, self.down) = (self.up + other.up, self.down + other.down);
        self.busy += other.busy;
        self.at_once = self.at_once.max(other.at_once);
        self.chain += other.chain;
    }

    /// Its line: `latency` is what each request waited, which its busy time over is the round
    /// trips it made in series.
    fn line(&self, latency: Duration, names: &Names) -> String {
        let ms = |duration: Duration| duration.as_secs_f64() * 1e3;
        let series = match latency.is_zero() {
            true => "-".to_owned(),
            false => format!("{:.1}", self.busy.as_secs_f64() / latency.as_secs_f64()),
        };
        let kinds: Vec<String> =
            self.kinds.iter().map(|(kind, count)| format!("{kind}={count}")).collect();
        let again: Vec<String> = self
            .again
            .iter()
            .map(|(key, count)| format!("{} x{count}", names.named(key)))
            .collect();
        format!(
            "requests={} chain={} in-series={series} at-once={} wall-ms={:.0} bucket-ms={:.0} \
             up={} down={} [{}] again=[{}]",
            self.requests(),
            self.chain,
            self.at_once,
            ms(self.wall),
            ms(self.busy),
            self.up,
            self.down,
            kinds.join(" "),
            again.join("; ")
        )
    }
}

/// The longest chain of `events` each begun once the one before it in the chain was answered.
fn chain(events: &[Event]) -> usize {
    let mut events: Vec<&Event> = events.iter().collect();
    events.sort_by_key(|event| event.began);
    let mut longest: Vec<usize> = Vec::with_capacity(events.len());
    for (i, event) in events.iter().enumerate() {
        let before = (0..i).filter(|&j| events[j].ended <= event.began).map(|j| longest[j]);
        longest.push(1 + before.max().unwrap_or(0));
    }
    longest.into_iter().max().unwrap_or(0)
}

/// The names of the cells a run made, by their IDs as keys have them.
#[derive(Default)]
struct Names(BTreeMap<String, String>);

impl Names {
    fn name(&mut self, cell: CellId, name: &str) {
        self.0.insert(Padded(cell.id).to_string(), name.to_owned());
        self.0.insert(Padded(cell.id).to_string(), format!("{name}'s"));
    }

    /// `key`, each of its segments that is a named ID as its name.
    fn named(&self, key: &str) -> String {
        let parts = key.split('/').map(|part| self.0.get(part).map_or(part, String::as_str));
        parts.collect::<Vec<_>>().join("/")
    }
}

/// A run of the phases on a base of so many extents.
struct Run {
    extents: u64,
    latency: Duration,
    log: Arc<Log>,
    faults: Arc<Faults>,
    names: Names,
    /// The phases run, what each asked, and its requests: said once every cell is named.
    phases: Vec<(&'static str, Asked, Vec<Event>)>,
}

impl Run {
    /// Has every request from now on wait the run's latency, or none.
    fn late(&self, late: bool) {
        let latency = if late { self.latency } else { Duration::ZERO };
        self.faults.set(Fault { latency, ..Fault::default() });
    }

    /// Runs `operation` and returns what it asked of the bucket.
    fn asked<T>(&self, operation: impl FnOnce() -> T) -> (T, Asked, Vec<Event>) {
        let from = self.log.events.lock().unwrap().len();
        let started = Instant::now();
        let done = operation();
        let wall = started.elapsed();
        let events = self.log.events.lock().unwrap()[from..].to_vec();
        (done, Asked::of(&events, wall), events)
    }

    /// Runs `operation`, a `phase`, and keeps what it asked of the bucket.
    fn phase<T>(&mut self, phase: &'static str, operation: impl FnOnce() -> T) -> T {
        let (done, asked, mut events) = self.asked(operation);
        events.sort_by_key(|event| event.began);
        self.phases.push((phase, asked, events));
        done
    }

    /// Says what each phase asked of the bucket: a line, then each of its requests, when it began
    /// and ended since the phase's first did.
    fn report(&self) {
        for (phase, asked, events) in &self.phases {
            let line = asked.line(self.latency, &self.names);
            println!("PHASE extents={} {phase}: {line}", self.extents);
            let start = events.first().map_or(Duration::ZERO, |event| event.began);
            for event in events {
                println!(
                    "  REQUEST +{:.0}..{:.0}ms {} {} up={} down={}",
                    (event.began - start).as_secs_f64() * 1e3,
                    (event.ended - start).as_secs_f64() * 1e3,
                    event.kind,
                    self.names.named(&event.key),
                    event.up,
                    event.down
                );
            }
        }
    }

    /// Says what the phases named cost together.
    fn total(&self, name: &str, phases: &[&str]) {
        let mut total = Asked::default();
        for phase in phases {
            let (_, asked, _) = (self.phases.iter().find(|(name, ..)| name == phase))
                .unwrap_or_else(|| panic!("no phase {phase}"));
            total.add(asked);
        }
        let line = total.line(self.latency, &self.names);
        println!("TOTAL extents={} {name}: {line}", self.extents);
    }
}

/// Writes a block to a new extent of `store`'s and commits it, durable.
fn write_and_commit(store: &OpenCell) {
    let info = store.info().unwrap();
    let sequence = info.next_sequence.max(info.sequence_floor);
    let extent = ExtentId { cell: store.cell().id, sequence };
    let data = pattern(sequence as u8, BLOCK);
    store.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    futures::executor::block_on(store.commit(None)).unwrap();
}

/// The store at `path`, opened, uploading only as a commit waits.
fn open(path: &Path) -> OpenCell {
    reopened(path, idle()).unwrap()
}

/// A base of `extents` extents and a version of it, made on one host, kept in `objects`, and the
/// directory of another, measured, which saw neither: what the phases run on.
struct Made {
    dir: tempfile::TempDir,
    /// The base's cell, and the version.
    version: (CellId, u64),
    capacity: u64,
}

impl Made {
    /// Makes them, their requests noted in `run`.
    fn new(extents: u64, objects: &Arc<dyn ObjectStore>, run: &mut Run) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The host the base is made on, and its version: another's than the one measured.
        let elsewhere = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let capacity = (8 * extents * BLOCK as u64).max(1 << 30);
        let made = dir.path().join("made");
        std::fs::create_dir(&made).unwrap();
        let version = based(extents, &made, (capacity, &elsewhere), run);
        std::fs::create_dir(dir.path().join("host")).unwrap();
        Self { dir, version, capacity }
    }

    /// A copy of the version, made at `name` on the host measured, kept in `bucket`, and opened:
    /// as the daemon's `CreateCell` with a version, then the store's opening, make it.
    fn copied(&self, bucket: &Arc<ObjectBucket>, name: &str) -> OpenCell {
        let ((cell, version), capacity) = (self.version, self.capacity);
        let url = super::tests::url_of(bucket);
        let description = local_cell::Description { block_size: BLOCK as u32, capacity, cell };
        let kind = bucket_cell::cell::Kind::Fxfs;
        OpenCell::create_at_version(&self.at(name), (&url, kind), description, version).unwrap();
        OpenCell::take_with(self.at(name), (capacity, cell, None), opening()).unwrap()
    }

    /// The path of the store `name` on the host measured.
    fn at(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join("host").join(name)
    }
}

/// Makes a base of `extents` extents, of `capacity` bytes, in `made` and kept in `bucket`, and a
/// version of it, its requests noted in `run`, and returns the base's cell and the version.
fn based(
    extents: u64,
    made: &Path,
    (capacity, bucket): (u64, &Arc<ObjectBucket>),
    run: &mut Run,
) -> (CellId, u64) {
    let base = kept_in(made.join("base"), (capacity, None), bucket, idle()).unwrap();
    run.names.name(base.cell(), "base");
    // Extents of one to four blocks, so that their lengths and places in their segments vary, as
    // an image's do: a checkpoint of extents all alike packs smaller than one of a real base.
    let blocks = pattern(7, 4 * BLOCK);
    for sequence in 1..=extents {
        let extent = ExtentId { cell: base.cell().id, sequence };
        let len = BLOCK * (1 + (sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 62) as usize);
        base.write(extent, 0, &[VolatileSlice::from(&blocks[..len])]).unwrap();
        if sequence % COMMIT_EVERY == 0 {
            futures::executor::block_on(base.commit(None)).unwrap();
        }
    }
    futures::executor::block_on(base.commit(None)).unwrap();
    drop(base);
    let base = open(&made.join("base"));
    let (version, asked, _) = run.asked(|| base.uploaded_version_at(made.join("copy")).unwrap());
    let line = asked.line(Duration::ZERO, &run.names);
    println!("SETUP extents={extents} version of the base: {line}");
    (base.cell(), version)
}

/// Has `bucket` be the measured host's, as the daemon's stores are made in it, and does what a
/// host does once, not for each restore: its conditional writes probed, a block of object IDs
/// taken. Made so without the environment, which the base's stores' threads, and the S3 server's,
/// may read meanwhile (TASK-444.16).
fn host(bucket: &Arc<ObjectBucket>) {
    crate::host_cell::use_host_bucket(url_of(bucket));
    local_cell::upload::block_on(bucket.probed(rand::random())).unwrap().unwrap();
    host_cell::new_cell(bucket, DEFAULT_TENANT).unwrap();
}

/// A bucket's objects in memory.
fn memory() -> Arc<dyn ObjectStore> {
    Arc::new(object_store::memory::InMemory::new())
}

/// How the host measured opens stores: uploading only as a commit waits.
fn opening() -> Opening {
    Opening { keeping: idle(), ..Opening::here().unwrap() }
}

/// `objects`, its requests noted, each as late as the run says, of a base of `extents` extents:
/// and the run.
fn recorded(
    objects: Arc<dyn ObjectStore>,
    extents: u64,
    latency: Duration,
) -> (Arc<dyn ObjectStore>, Run) {
    let faults = Faults::new(Duration::ZERO, None);
    let log = Arc::new(Log { origin: Instant::now(), events: Mutex::default() });
    let objects: Arc<dyn ObjectStore> = Arc::new(Recording {
        objects: Arc::new(Faulty::new(objects, faults.clone())),
        log: log.clone(),
    });
    let run = Run { extents, latency, log, faults, names: Names::default(), phases: Vec::new() };
    (objects, run)
}

/// The phases on a base of `extents` extents, each request `latency` late.
fn lifecycle(extents: u64, latency: Duration) {
    let (objects, mut run) = recorded(memory(), extents, latency);
    let made = Made::new(extents, &objects, &mut run);
    // The host measured: a bucket of its own over the same objects, which decoded nothing yet, and
    // directories of its own.
    let bucket = Arc::new(ObjectBucket::new(objects, ""));
    host(&bucket);
    let at = |name: &str| made.at(name);
    run.late(true);

    // A restore on a host that never saw the version: a copy of it made, as the daemon's
    // `CreateCell` with a version does, opened, forked, the fork opened.
    let taken = run.phase("cold copy-open", || made.copied(&bucket, "copy"));
    let unwritten = run.phase("cold fork", || taken.fork(at("cold"), DEFAULT_TENANT).unwrap());
    run.names.name(unwritten, "cold-fork");
    let cold = run.phase("cold fork-open", || open(&at("cold")));
    // And on one that has it open, as the daemon keeps a copy open for the restores after.
    let fork = run.phase("warm fork", || taken.fork(at("vm"), DEFAULT_TENANT).unwrap());
    run.names.name(fork, "fork");
    let vm = run.phase("warm fork-open", || open(&at("vm")));

    run.phase("write-commit", || write_and_commit(&vm));
    run.phase("close", || drop(vm));
    let vm = run.phase("reopen", || open(&at("vm")));
    run.phase("write-commit again", || write_and_commit(&vm));
    let snapshot = run.phase("snapshot", || vm.make_version(None).unwrap());
    run.phase("drop snapshot", || vm.drop_versions(&[snapshot].into()).unwrap());
    run.phase("close after snapshot", || drop(vm));
    let deleting = |name: &str| host_cell::delete(&at(name), Some(&bucket), || Ok(1)).unwrap();
    run.phase("delete", || deleting("vm"));
    // A sandbox that lived minutes: restored, written, closed, deleted. Its deletion.
    let short = taken.fork(at("short"), DEFAULT_TENANT).unwrap();
    run.names.name(short, "short");
    let lived = open(&at("short"));
    write_and_commit(&lived);
    drop(lived);
    run.phase("delete after close", || deleting("short"));
    // A fork that wrote nothing.
    run.phase("close unwritten", || drop(cold));
    run.phase("delete unwritten", || deleting("cold"));

    run.report();
    run.total("restore, cold", &["cold copy-open", "cold fork", "cold fork-open"]);
    run.total("restore, warm", &["warm fork", "warm fork-open"]);
    let life = ["warm fork", "warm fork-open", "write-commit", "close", "delete after close"];
    run.total("life (restore warm, a write, close, delete)", &life);
    let resumed = [
        "warm fork",
        "warm fork-open",
        "write-commit",
        "close",
        "reopen",
        "write-commit again",
        "snapshot",
        "drop snapshot",
        "close after snapshot",
        "delete",
    ];
    run.total("life with a resume and a snapshot", &resumed);

    // The states the bucket keeps decoded: after how many resumes of other stores a restore reads
    // the base's checkpoint again. Not late: its requests and bytes alone.
    run.late(false);
    let restore = |run: &Run, name: &str| {
        let (_, asked, events) = run.asked(|| {
            taken.fork(at(name), DEFAULT_TENANT).unwrap();
            drop(open(&at(name)));
        });
        let checkpoints = events.iter().filter(|event| {
            event.kind == "get" && event.key.contains("/checkpoints/") && event.down > 0
        });
        (asked, checkpoints.count())
    };
    let said = |resumes: u32, (asked, checkpoints): (Asked, usize)| {
        println!(
            "DECODED extents={extents} restore after {resumes} resumes: requests={} down={} \
             checkpoints-read={checkpoints}",
            asked.requests(),
            asked.down
        );
    };
    said(0, restore(&run, "restore-0"));
    for resumes in 1..=RESUMES {
        let name = format!("resumed-{resumes}");
        taken.fork(at(&name), DEFAULT_TENANT).unwrap();
        let resumed = open(&at(&name));
        write_and_commit(&resumed);
        drop(resumed);
        drop(open(&at(&name)));
        said(resumes, restore(&run, &format!("restore-{resumes}")));
    }
    drop(taken);
}

/// TASK-443.5, TASK-443.14: what a restore, a write, a close, a reopen, a snapshot and a deletion
/// ask of the bucket, by phase (the module's docs), on bases of `CLADEFS_LIFECYCLE_EXTENTS` extents
/// each (`1024,32768`), each request `CLADEFS_LIFECYCLE_LATENCY_MS` late (30). Lines `PHASE`, each
/// followed by its requests, `TOTAL` and `DECODED`; recorded in docs/measurements.
#[test]
#[ignore = "a benchmark (TASK-443.5): --run-ignored only, alone"]
fn bucket_lifecycle_by_phase() {
    let sizes = std::env::var("CLADEFS_LIFECYCLE_EXTENTS").unwrap_or_else(|_| "1024,32768".into());
    let latency = std::env::var("CLADEFS_LIFECYCLE_LATENCY_MS").unwrap_or_else(|_| "30".into());
    let latency = Duration::from_millis(latency.parse().expect("CLADEFS_LIFECYCLE_LATENCY_MS"));
    for extents in sizes.split(',') {
        lifecycle(extents.trim().parse().expect("CLADEFS_LIFECYCLE_EXTENTS"), latency);
    }
}

/// TASK-449: a restore, a fork of a version and its child's first open, asks the bucket what the
/// protocol needs, at once where it can: a fork, its child's record created, then its reference and
/// its cell held at once, the head naming the version read, the record made from the version its create left
/// (4 round trips, one after another); the child's open, its head taken from none, then its record
/// and its cell read again at once (2), all else known from the fork (`bucket_cell::known`; the
/// cell read once the head is taken, as its holding from what the fork left writes nothing:
/// `restores_refused_to_placements_superseded_since_their_forks`). 8 requests in 6 round trips,
/// where they were 14 in 13. On a host that never saw the version, its store taken first, opened
/// once, its record read as the take finds it, at once with the collector's marking looked at, then
/// the base's head and the checkpoint the version names (3): 12 in 9, where they were 25 in 21.
/// Requests are counted as they reach the object store, round trips as the longest chain of them
/// each begun once the one before it was answered.
///
/// cladefs: those counts were a clear restore's. Every restore is encrypted since TASK-411.28; with
/// one object a cell, its head (TASK-515), the fork makes its child's head with its keys, a KEK of
/// the host's pool, its DEK and those it inherits, in the head's create (6 requests in 6); and the
/// child's open takes the head from what the fork left, reading the base's head for its origin (3
/// in 3). A restore 9 in 9, where it was 14 in 13; the version's open 5 in 4, where it was 8 in 5.
///
/// cladefs: since TASK-633.9 the host's first ID, in [`host`], reads its bucket's identity, which
/// looks at the collector's marking for the minute after, so the version's open, at once with its
/// record before, asks it no more: 4 in 4. And since TASK-633.5 a child's open, its head given its
/// keys by the fork, reads no ancestor's head: its taking alone, 1 in 1, where it was 3 in 3.
#[test]
fn restores_ask_the_bucket_what_the_protocol_needs() {
    let (objects, mut run) = recorded(memory(), 64, Duration::from_millis(5));
    let made = Made::new(64, &objects, &mut run);
    let bucket = Arc::new(ObjectBucket::new(objects, ""));
    host(&bucket);
    let at = |name: &str| made.at(name);
    run.late(true);
    let taken = run.phase("cold copy-open", || made.copied(&bucket, "copy"));
    run.phase("cold fork", || taken.fork(at("cold"), DEFAULT_TENANT).unwrap());
    let cold = run.phase("cold fork-open", || open(&at("cold")));
    run.phase("warm fork", || taken.fork(at("vm"), DEFAULT_TENANT).unwrap());
    let vm = run.phase("warm fork-open", || open(&at("vm")));
    let costs: Vec<_> =
        (run.phases.iter()).map(|(phase, asked, _)| (*phase, asked.cost())).collect();
    run.report();
    assert_eq!(
        costs,
        [
            ("cold copy-open", (4, 4)),
            ("cold fork", (6, 6)),
            ("cold fork-open", (1, 1)),
            ("warm fork", (6, 6)),
            ("warm fork-open", (1, 1)),
        ]
    );
    drop((vm, cold, taken));
}

/// TASK-449's review: a host's fork remembers the clone's cell as it held it, which the clone's
/// first open here starts its holding from, a read less. Another host placed later, its token
/// higher, holding the cell meanwhile (for another of its cells, the clone's head not taken),
/// the open is refused (`ESTALE`), as it was when the open read the cell: the cell's holding found
/// from memory, not from a read, unchanged, writes nothing, so a read once the clone's head is
/// taken finds it still held, or the open fails. Opened anyway, this host would write the clone
/// until the later placement's writer fenced it, what its guest wrote meanwhile lost.
#[test]
fn restores_refused_to_placements_superseded_since_their_forks() {
    let (objects, mut run) = recorded(memory(), 16, Duration::ZERO);
    let made = Made::new(16, &objects, &mut run);
    let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
    host(&bucket);
    let taken = made.copied(&bucket, "copy");
    let clone = taken.fork(made.at("vm"), DEFAULT_TENANT).unwrap();
    // Another host, placed later, holds the clone's cell, through a bucket handle of its own.
    let elsewhere = ObjectBucket::new(objects, "");
    let later = host_cell::keeping().unwrap().token + 1;
    let holding = bucket_cell::testing::take(&elsewhere, clone, later, later);
    local_cell::upload::block_on(holding).unwrap().unwrap();
    let opened = super::tests::reopened(made.at("vm"), super::tests::idle());
    assert_eq!(opened.err().and_then(|err| err.errno()), Some(crate::Errno::ESTALE));
    drop(taken);
}

/// TASK-449's review: how fast a store's uploads go after `CLADEFS_UPLOAD_RESTORES` restores (8),
/// against the S3 server in memory, over HTTP: the connections the restores' calls from no
/// runtime's task opened stay in the bucket's pool, which the uploads take first; their sockets
/// are served by the runtime they opened on. `CLADEFS_UPLOAD_MIB` MiB (256) written, in extents of
/// 1 MiB, then committed, three times. Lines `UPLOADS`.
#[test]
#[ignore = "a benchmark (TASK-449's review): --run-ignored only, alone"]
fn uploads_after_restores() {
    let var = |name: &str, default: u64| {
        std::env::var(name).map_or(default, |value| value.parse().expect(name))
    };
    let (restores, mib) = (var("CLADEFS_UPLOAD_RESTORES", 8), var("CLADEFS_UPLOAD_MIB", 256));
    let serving = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let server = serving.block_on(async { bucket_cell::s3_server::S3Server::start(None) });
    let (objects, mut run) = recorded(server.objects(), 16, Duration::ZERO);
    let made = Made::new(16, &objects, &mut run);
    let bucket = Arc::new(ObjectBucket::new(server.objects(), ""));
    host(&bucket);
    let taken = made.copied(&bucket, "copy");
    for restore in 0..restores {
        let at = made.at(&format!("restore-{restore}"));
        taken.fork(&at, DEFAULT_TENANT).unwrap();
        drop(open(&at));
    }
    let connected = server.connections();
    let extent = pattern(9, 1 << 20);
    for round in 0..3 {
        let at = made.at(&format!("writer-{round}"));
        taken.fork(&at, DEFAULT_TENANT).unwrap();
        let writer = open(&at);
        let info = writer.info().unwrap();
        let first = info.next_sequence.max(info.sequence_floor);
        for sequence in first..first + mib {
            let id = ExtentId { cell: writer.cell().id, sequence };
            writer.write(id, 0, &[VolatileSlice::from(&extent[..])]).unwrap();
        }
        let began = Instant::now();
        futures::executor::block_on(writer.commit(None)).unwrap();
        let took = began.elapsed();
        println!(
            "UPLOADS restores={restores} round={round} mib={mib} commit-ms={} mib-per-s={:.0} \
             connections-before={connected} connections-after={}",
            took.as_millis(),
            mib as f64 / took.as_secs_f64(),
            server.connections()
        );
        drop(writer);
    }
    drop(taken);
}

/// TASK-449: a restore on a host that has the version open opens no connection to the bucket: those
/// its requests went on before stay in the bucket's pool, the fork's too, on the bucket
/// runtime, which the calls from no runtime's task run in, and which lives on
/// (`local_cell::upload::block_on`). On a runtime of
/// each call's own, as before, every fork connected anew for what it asked at once beyond the
/// connections the host's runtime had left idle in the pool (here one: its reference's write and
/// its cell's holding), a TCP handshake, and on a bucket elsewhere a TLS one too, before the
/// request, the connection closed as the fork returned. Against the S3 server in memory, which
/// counts the connections it accepts, the host a client of its own, as another host is.
#[test]
fn restores_connect_to_the_bucket_no_more() {
    let serving = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let server = serving.block_on(async { bucket_cell::s3_server::S3Server::start(None) });
    let (objects, mut run) = recorded(server.objects(), 16, Duration::ZERO);
    let made = Made::new(16, &objects, &mut run);
    // The host measured: a client of its own, its own pool of connections, as another host's.
    let bucket = Arc::new(ObjectBucket::new(server.objects(), ""));
    host(&bucket);
    let taken = made.copied(&bucket, "copy");
    let restore = |name: &str| {
        let before = server.connections();
        taken.fork(made.at(name), DEFAULT_TENANT).unwrap();
        let forked = server.connections();
        drop(open(&made.at(name)));
        (forked - before, server.connections() - forked)
    };
    let connected: Vec<_> = ["first", "second", "third"].map(restore).into();
    assert_eq!(connected, [(0, 0); 3]);
}

/// TASK-633.7: a version of a cell of one bucket, forked on a host of another: its child is a cell
/// of the host's bucket, its ID that bucket's, its head there; it reads what it inherited. A host
/// forks into its own region's bucket, as a host of that region (doc-23).
///
/// TASK-633.8: the fork starts the version's mirror in the host's bucket, in the background, which
/// marks it there and moves the child's reference there from the parent's bucket.
#[test]
fn versions_forked_on_a_host_of_another_bucket_make_children_there() {
    use bucket_cell::identity::Details;
    let dir = tempfile::tempdir().unwrap();
    let url = |name: &str| {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        format!("file://{}", dir.path().join(name).display())
    };
    let (url_a, url_b) = (url("a"), url("b"));
    crate::bucket::format_bucket(&url_a, Details::default()).unwrap();
    assert_eq!(crate::bucket::add_bucket(&url_a, &url_b, Details::default()).unwrap().number, 2);
    let (bucket_a, bucket_b) =
        (host_cell::bucket(&url_a).unwrap(), host_cell::bucket(&url_b).unwrap());
    let capacity = 1 << 30;

    // The base, a cell of bucket A, and its version, made on a host of A.
    let cell = host_cell::new_cell(&bucket_a, DEFAULT_TENANT).unwrap();
    assert_eq!(cell.id >> 48, 1);
    let base_path = dir.path().join("base");
    let base = kept_in(&base_path, (capacity, Some(cell)), &bucket_a, idle()).unwrap();
    let extent = ExtentId { cell: cell.id, sequence: base.info().unwrap().sequence_floor };
    let data = pattern(3, BLOCK);
    base.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    futures::executor::block_on(base.commit(None)).unwrap();
    drop(base);
    let base = open(&base_path);
    let version = base.uploaded_version_at(dir.path().join("version-a")).unwrap();
    drop(base);

    // On a host of B: the version opened there, then forked.
    let at = dir.path().join("version-b");
    let description = local_cell::Description { block_size: BLOCK as u32, capacity, cell };
    let kind = bucket_cell::cell::Kind::Fxfs;
    OpenCell::create_at_version(&at, (&url_of(&bucket_b), kind), description, version).unwrap();
    let opened = OpenCell::take_with(&at, (capacity, cell, None), opening()).unwrap();
    let child_path = dir.path().join("child");
    let child = opened.fork(&child_path, DEFAULT_TENANT).unwrap();
    assert_eq!(child.id >> 48, 2, "the child is not a cell of the host's bucket");

    let raw = |url: &str| ObjectBucket::from_url(url).unwrap();
    let head = |bucket: &ObjectBucket, cell| {
        local_cell::upload::block_on(bucket_cell::head::get(bucket, cell)).unwrap().unwrap()
    };
    let childs = head(&raw(&url_b), child).expect("the child's head is not in B");
    assert!(head(&raw(&url_a), child).is_none(), "the child's head is in A");
    let origin = childs.origin().expect("the child names no origin");
    let reference = bucket_cell::layout::reference(cell, origin.at, child);
    let got = |url: &str, key: &str| {
        local_cell::upload::block_on(bucket_cell::bucket::get(&raw(url), key)).unwrap().is_some()
    };
    let mirrored = bucket_b.mirror_wanted(cell, origin.at);
    assert_eq!(local_cell::upload::block_on(mirrored).unwrap().unwrap(), None, "not mirrored");
    let marker = bucket_cell::layout::mirror(cell, origin.at);
    assert_eq!((got(&url_a, &marker), got(&url_b, &marker)), (false, true));
    assert_eq!((got(&url_a, &reference), got(&url_b, &reference)), (false, true));
    drop(opened);

    let child = open(&child_path);
    let mut read = vec![0; BLOCK];
    child.read(extent, 0, &mut [crate::VolatileSliceMut::from(&mut read[..])]).unwrap();
    assert!(read == data, "the child reads another than its parent's data");
}
