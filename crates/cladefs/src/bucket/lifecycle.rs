// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-443.5, TASK-443.14: what a sandbox's life asks of its bucket, by phase: a restore (a fork
//! of a point and the fork's first open) on a host that never saw the point and on one that has it
//! open, a small write and its commit, a clean close, a reopen, a snapshot (a point) of the written
//! fork, and its deletion, on a base of as many extents as asked for. A measurement, not a check.
//!
//! The bucket is one in memory behind [`Recording`], which notes every request that reaches the
//! object store, the ones `ObjectBucket::stats` leaves out too (the collector's marks), with its
//! key, its bytes and when it began and ended, and behind a fixed latency
//! (`extent_bucket::faults::Faulty`). So each phase says, beside its requests by kind and its
//! bytes up and down: how many requests were in flight at once at most; how long at least one was
//! in flight, which over the latency is how many round trips it made one after another; and the
//! keys it read more than once.
//!
//! Its requests, bytes and round trips are the code's, whatever the build. Its times beside the
//! bucket's (`wall-ms` less `bucket-ms`: decoding and encoding checkpoints, opening stores) are a
//! debug build's: cladefs's tests don't build in release (TASK-320).

use super::tests::{idle, kept_in, pattern, reopened, url_of};
use crate::extents::Opening;
use crate::host_store::{self, DEFAULT_TENANT, new_cell};
use crate::{Branch, ExtentId, ExtentStore, VolatileSlice};
use extent_bucket::faults::{Fault, Faults, Faulty};
use extent_bucket::object_bucket::ObjectBucket;
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

const BLOCK: usize = ExtentStore::BLOCK_SIZE as usize;

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

    fn add(&mut self, other: &Self) {
        self.wall += other.wall;
        for (kind, count) in &other.kinds {
            *self.kinds.entry(kind).or_default() += count;
        }
        (self.up, self.down) = (self.up + other.up, self.down + other.down);
        self.busy += other.busy;
        self.at_once = self.at_once.max(other.at_once);
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
            "requests={} in-series={series} at-once={} wall-ms={:.0} bucket-ms={:.0} up={} down={} \
             [{}] again=[{}]",
            self.requests(),
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

/// The names of the branches and cells a run made, by their IDs as keys have them.
#[derive(Default)]
struct Names(BTreeMap<String, String>);

impl Names {
    fn name(&mut self, branch: Branch, name: &str) {
        self.0.insert(format!("{:016x}", branch.id), name.to_owned());
        self.0.insert(format!("{:016x}", branch.cell.id), format!("{name}'s"));
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
    /// The phases run, what each asked, and its requests: said once every branch is named.
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
fn write_and_commit(store: &ExtentStore) {
    let info = store.info().unwrap();
    let sequence = info.next_sequence.max(info.sequence_floor);
    let extent = ExtentId { branch: store.branch().id, sequence };
    let data = pattern(sequence as u8, BLOCK);
    store.write(extent, 0, &[VolatileSlice::from(&data[..])]).unwrap();
    store.commit(None).unwrap();
}

/// The store at `path`, opened, uploading only as a commit waits.
fn open(path: &Path) -> ExtentStore {
    reopened(path, idle()).unwrap()
}

/// The phases on a base of `extents` extents, each request `latency` late.
fn lifecycle(extents: u64, latency: Duration) {
    let dir = tempfile::tempdir().unwrap();
    let faults = Faults::new(Duration::ZERO, None);
    let memory: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let log = Arc::new(Log { origin: Instant::now(), events: Mutex::default() });
    let objects: Arc<dyn ObjectStore> = Arc::new(Recording {
        objects: Arc::new(Faulty::new(memory, faults.clone())),
        log: log.clone(),
    });
    let mut run =
        Run { extents, latency, log, faults, names: Names::default(), phases: Vec::new() };

    // The host the base is made on, and its point: another's than the one measured.
    let elsewhere = Arc::new(ObjectBucket::new(objects.clone(), ""));
    let capacity = (8 * extents * BLOCK as u64).max(1 << 30);
    let made = dir.path().join("made");
    std::fs::create_dir(&made).unwrap();
    let base = kept_in(made.join("base"), (capacity, None), &elsewhere, idle()).unwrap();
    run.names.name(base.branch(), "base");
    // Extents of one to four blocks, so that their lengths and places in their segments vary, as
    // an image's do: a checkpoint of extents all alike packs smaller than one of a real base.
    let blocks = pattern(7, 4 * BLOCK);
    for sequence in 1..=extents {
        let extent = ExtentId { branch: base.branch().id, sequence };
        let len = BLOCK * (1 + (sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 62) as usize);
        base.write(extent, 0, &[VolatileSlice::from(&blocks[..len])]).unwrap();
        if sequence % COMMIT_EVERY == 0 {
            base.commit(None).unwrap();
        }
    }
    base.commit(None).unwrap();
    drop(base);
    let base = open(&made.join("base"));
    let (point, asked, _) =
        run.asked(|| base.point(made.join("point"), new_cell(DEFAULT_TENANT)).unwrap());
    run.names.name(point, "point");
    let line = asked.line(Duration::ZERO, &run.names);
    println!("SETUP extents={extents} point of the base: {line}");
    drop(base);

    // The host measured: a bucket of its own over the same objects, which decoded nothing yet, and
    // directories of its own. What it does once, not for each restore, done first: its
    // conditional writes probed, a block of object IDs taken.
    let bucket = Arc::new(ObjectBucket::new(objects, ""));
    // The bucket the daemon's stores are made in, as a point taken from it is.
    unsafe { std::env::set_var("CLADEFSD_BUCKET", url_of(&bucket)) };
    extent_store::upload::block_on(bucket.probed(rand::random())).unwrap().unwrap();
    host_store::new_branch(&bucket, new_cell(DEFAULT_TENANT)).unwrap();
    let host = dir.path().join("host");
    std::fs::create_dir(&host).unwrap();
    let at = |name: &str| host.join(name);
    let opening = || Opening { keeping: idle(), ..Opening::here().unwrap() };
    run.late(true);

    // A restore on a host that never saw the point: its store taken from the bucket, as the
    // daemon's `take_store` does, forked, the fork opened.
    let taken = run.phase("cold point-open", || {
        ExtentStore::take_with(at("point"), (capacity, point, None), opening()).unwrap()
    });
    let unwritten =
        run.phase("cold fork", || taken.fork(at("cold"), new_cell(DEFAULT_TENANT)).unwrap());
    run.names.name(unwritten, "cold-fork");
    let cold = run.phase("cold fork-open", || open(&at("cold")));
    // And on one that has it open, as the daemon keeps a point open for the restores after.
    let fork = run.phase("warm fork", || taken.fork(at("vm"), new_cell(DEFAULT_TENANT)).unwrap());
    run.names.name(fork, "fork");
    let vm = run.phase("warm fork-open", || open(&at("vm")));

    run.phase("write-commit", || write_and_commit(&vm));
    run.phase("close", || drop(vm));
    let vm = run.phase("reopen", || open(&at("vm")));
    run.phase("write-commit again", || write_and_commit(&vm));
    let snapshot =
        run.phase("snapshot", || vm.point(at("snapshot"), new_cell(DEFAULT_TENANT)).unwrap());
    run.names.name(snapshot, "snapshot");
    run.phase("close after snapshot", || drop(vm));
    let deleting = |name: &str| host_store::delete(&at(name), Some(&bucket), || Ok(1)).unwrap();
    run.phase("delete", || deleting("vm"));
    run.phase("delete snapshot", || deleting("snapshot"));
    // A sandbox that lived minutes: restored, written, closed, deleted. Its deletion.
    let short = taken.fork(at("short"), new_cell(DEFAULT_TENANT)).unwrap();
    run.names.name(short, "short");
    let lived = open(&at("short"));
    write_and_commit(&lived);
    drop(lived);
    run.phase("delete after close", || deleting("short"));
    // A fork that wrote nothing.
    run.phase("close unwritten", || drop(cold));
    run.phase("delete unwritten", || deleting("cold"));

    run.report();
    run.total("restore, cold", &["cold point-open", "cold fork", "cold fork-open"]);
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
        "close after snapshot",
        "delete",
        "delete snapshot",
    ];
    run.total("life with a resume and a snapshot", &resumed);

    // The states the bucket keeps decoded: after how many resumes of other stores a restore reads
    // the base's checkpoint again. Not late: its requests and bytes alone.
    run.late(false);
    let restore = |run: &Run, name: &str| {
        let (_, asked, events) = run.asked(|| {
            taken.fork(at(name), new_cell(DEFAULT_TENANT)).unwrap();
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
        taken.fork(at(&name), new_cell(DEFAULT_TENANT)).unwrap();
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
