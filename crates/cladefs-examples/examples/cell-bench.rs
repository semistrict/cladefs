// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what an extent store's commits, and the requests behind them, cost as its cell
//! grows, and while its segments close between commits: what the 2026-10-21 review's performance
//! issues were measured by, before and after their changes. Say:
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example cell-bench -- CASE [N]
//! ```
//!
//! cladefs's, not local_cell's, whose examples have the store's audit, from its dev-dependency on
//! itself, which a release build refuses (TASK-303).
//!
//! CASE is one of:
//!
//! - `lagging-view`: N commits (20,000), each writing an extent and freeing the one before, with a
//!   view that never advances, which keeps their frees: the time of each thousand commits, as the
//!   view lags more (TASK-92.29).
//! - `segments`: a cell of N closed segments (2,000), each holding an extent: the time of a
//!   thousand commits after them, and of opening the store, and the bytes opening it reads
//!   (TASK-92.16).
//! - `close-now`: a writer writing extents of 1 MiB and closing the open segment between commits
//!   each N MiB (32), as cladefs's uploader has the store do, while a guest writes a block and
//!   commits, again and again, in a directory of the host's: how long the guest's writes and
//!   commits take, the longest of them, and how long each closing takes (TASK-41.1).
//! - `burst`: a guest writing N MiB (256) of extents of 256 KiB after a commit, and committing,
//!   in a directory of the host's: more than a segment and an extent hold, so that the open
//!   segment fills between commits, again and again: how long the writes take, the longest, and
//!   the commit after them, ordered, durable and framed, what it seals in files of their own
//!   (TASK-79.2), which its flush syncs at once (TASK-92.42), and the bytes the store wrote for it
//!   by kind: cladefs: since the commit seals the extents in the segments filled where they are, a
//!   commit spanning them (TASK-92.41), none copied, none in files of their own.
//! - `durable`: a guest's durable commits, a hundred of them one after another, each sealing N
//!   extents (2) in files of their own, in a directory of the host's: how long each commit's flush
//!   takes, which syncs their files and their directory, as many at once as there are of each,
//!   up to sixteen; and with N 1, one at a time (TASK-92.42).
//! - `framing`: N segments (64) of 32 MiB, each written as extents of 1 MiB and closed by a commit,
//!   by `CLADEFS_BENCH_STORES` stores at once (4), each on a thread of its own, in a directory of the
//!   host's (`CLADEFS_BENCH_DIR`, or a temporary one), beside `CLADEFS_BENCH_HOGS` threads (0) that
//!   spin, as busy VMs' vCPUs do, at `CLADEFS_BENCH_HOG_NICE` (0), the process on the first
//!   `CLADEFS_BENCH_CPUS` of the CPUs it may run on (all): how long each phase of the framings took,
//!   on average, the slowest's and the longest of each, and how long their threads waited to run
//!   (TASK-336). The data is `CLADEFS_BENCH_DATA`'s: `text`, of sixteen letters at random, which
//!   compresses about by half, as a build's files do, slower than a filesystem's structures (the
//!   default); `noise`, which doesn't compress, framed in place; or `half`, one of each. cladefs: the
//!   stores share what holds the parts they pack as their segments fill, as a daemon's do
//!   (TASK-400), which `CLADEFS_BENCH_EVICT` (0) has let go of every part before each closing, as a
//!   governor short of memory would: what it held, took and let go of, the process's CPU time, and
//!   how long the writes and commits took, which the looks ahead run behind; and the bytes the
//!   stores wrote, raw and framed, and the segments framed in place. cladefs: encrypted stores look
//!   ahead, hold and frame in place (TASK-411.23, TASK-421).
//! - `writes`: N MiB (256) written to a store's open segments as extents of `CLADEFS_BENCH_EXTENT`
//!   KiB (1024), in a directory of the host's (`CLADEFS_BENCH_DIR`, or a temporary one), read back
//!   while open, then committed: MiB/s of the writes, of the reads, and how long the commit took
//!   (TASK-411.5).
//!
//! cladefs: every store is encrypted (TASK-411.28): their raw files encrypted, AES-256-XTS by sector,
//! their segments framed in place their raw files' sectors, tagged, or in a copy AES-256-GCM
//! (TASK-411.23). `CLADEFS_BENCH_ENCRYPTED` set to `1` has `framing` and `writes` open their stores as
//! a host with a KMS opens a cell's, through their uploaders, to a bucket in memory; unset, with no
//! uploader, kept in no bucket, encrypted under the tests' DEK (`local_cell::UNKEPT_DEK`,
//! TASK-411.5). `0`, a clear twin through uploaders before TASK-411.28, is refused: measure the
//! clear with an old pin's cell-bench.
//!
//! The stores of all but `close-now`, `burst` and `durable` are in memory ([`MemDir`]), so that
//! what is timed is the store's own work. On Linux, the stores' page cache is sized first as the
//! extent daemon's governor sizes it at its first look ([`govern`], TASK-433.8), so that those
//! three time a store as the daemon runs it.

use local_cell::{Description, Dir, HeldParts, HostDir, LocalCell, MemDir, Opening, Options};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use storage_device::ExtentId;

const BLOCK: usize = 4096;
/// The benchmark's cell: of a cell of the default tenant's (TASK-364).
const CELL: local_cell::CellId = local_cell::CellId { tenant: 0, id: 1 };

fn extent(sequence: u64) -> ExtentId {
    ExtentId::new(CELL.id, sequence)
}

fn create<D: Dir>(dir: D, options: Options) -> LocalCell<D> {
    let description = Description { block_size: BLOCK as u32, capacity: 64 << 30, cell: CELL };
    LocalCell::create(dir, description, options).unwrap()
}

/// Bytes that compress, as a filesystem's do, differing by `seed`.
fn data(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ seed as u8 ^ (i / BLOCK) as u8).collect()
}

/// Microseconds each of `count` operations took, which took `elapsed` together.
fn each(elapsed: Duration, count: u64) -> f64 {
    elapsed.as_secs_f64() * 1e6 / count as f64
}

/// Commits `store`, writing `extent` first, and freeing `freed` if any, and returns how long the
/// commit took: ordered, as its flush, each hundredth commit, is not timed, the store in memory
/// copying each file it syncs whole.
async fn commit(store: &LocalCell<MemDir>, extent: ExtentId, freed: Option<ExtentId>) -> Duration {
    store.write(extent, 0, &data(extent.sequence, BLOCK)).await.unwrap();
    if let Some(freed) = freed {
        store.free(freed).unwrap();
    }
    let started = Instant::now();
    store.commit_ordered(None).await.unwrap();
    let elapsed = started.elapsed();
    if extent.sequence.is_multiple_of(100) {
        store.flush().await.unwrap();
    }
    elapsed
}

/// `commits` commits, each writing an extent and freeing the one before, with a view that never
/// advances if `viewing`: the time each takes, by five thousands.
async fn commits_freeing(commits: u64, viewing: bool) {
    let store = create(MemDir::new(), Options::default());
    commit(&store, extent(0), None).await;
    let view = viewing.then(|| store.view());
    let mut elapsed = Duration::ZERO;
    for sequence in 1..=commits {
        elapsed += commit(&store, extent(sequence), Some(extent(sequence - 1))).await;
        if sequence.is_multiple_of(5000) {
            let first = sequence - 4999;
            println!("commits {first:>6} to {sequence:>6}: {:8.1} µs each", each(elapsed, 5000));
            elapsed = Duration::ZERO;
        }
    }
    let stats = store.view_stats();
    let kept = store.usage().unwrap().kept_for_views;
    println!("views: {}, {} commits behind, keeping {kept} bytes", stats.views, stats.behind);
    drop(view);
}

async fn lagging_view(commits: u64) {
    println!("with no view:");
    commits_freeing(commits, false).await;
    println!("with a view that never advances:");
    commits_freeing(commits, true).await;
}

async fn segments(count: u64) {
    let dir = MemDir::new();
    let options = Options::default();
    let store = create(dir.clone(), options);
    for sequence in 0..count {
        store.write(extent(sequence), 0, &data(sequence, BLOCK)).await.unwrap();
        store.close_segment().unwrap();
        store.commit(None).await.unwrap();
    }
    println!("{} segments", store.usage().unwrap().segments);
    const COMMITS: u64 = 1000;
    let mut elapsed = Duration::ZERO;
    for sequence in count..count + COMMITS {
        elapsed += commit(&store, extent(sequence), None).await;
    }
    store.flush().await.unwrap();
    println!("commits after them: {:8.1} µs each", each(elapsed, COMMITS));
    drop(store);
    let before = dir.io();
    let started = Instant::now();
    let store = LocalCell::open(dir.clone(), options).unwrap();
    let elapsed = started.elapsed();
    println!(
        "opening: {:8.1} ms, reading {} bytes",
        elapsed.as_secs_f64() * 1e3,
        dir.io().read - before.read
    );
    assert_eq!(store.extents().unwrap().len() as u64, count + COMMITS);
}

/// What the guest's requests took: how many, together, and the longest.
#[derive(Default)]
struct Waits {
    count: u64,
    total: Duration,
    longest: Duration,
}

impl Waits {
    fn add(&mut self, wait: Duration) {
        self.count += 1;
        self.total += wait;
        self.longest = self.longest.max(wait);
    }

    fn print(&self, name: &str) {
        println!(
            "{name}: {} of them, {:8.1} µs each, the longest {:8.1} ms",
            self.count,
            each(self.total, self.count.max(1)),
            self.longest.as_secs_f64() * 1e3
        );
    }
}

async fn close_now(mib: u64) {
    const SEGMENTS: u64 = 8;
    const MIB: usize = 1 << 20;
    let temp = tempfile::tempdir().unwrap();
    // Segments close between commits alone.
    let options = Options { segment_size: 1 << 30, ..Options::default() };
    let store = Arc::new(create(HostDir::new(temp.path()).unwrap(), options));
    let done = Arc::new(AtomicBool::new(false));
    let guest = {
        let (store, done) = (store.clone(), done.clone());
        tokio::spawn(async move {
            let (mut writes, mut commits) = (Waits::default(), Waits::default());
            let mut sequence = 1 << 32;
            while !done.load(Ordering::Relaxed) {
                let started = Instant::now();
                store.write(extent(sequence), 0, &[1; BLOCK]).await.unwrap();
                let written = Instant::now();
                store.commit_ordered(None).await.unwrap();
                writes.add(written - started);
                commits.add(written.elapsed());
                sequence += 1;
            }
            (writes, commits)
        })
    };
    let mut closings = Waits::default();
    let mut sequence = 0;
    for _ in 0..SEGMENTS {
        for _ in 0..mib {
            store.write(extent(sequence), 0, &data(sequence, MIB)).await.unwrap();
            store.commit_ordered(None).await.unwrap();
            sequence += 1;
        }
        store.flush().await.unwrap();
        let started = Instant::now();
        assert!(store.close_now().await.unwrap());
        closings.add(started.elapsed());
    }
    done.store(true, Ordering::Relaxed);
    let (writes, commits) = guest.await.unwrap();
    closings.print("closings between commits");
    writes.print("the guest's writes");
    commits.print("the guest's commits");
}

async fn burst(mib: u64) {
    const EXTENT: usize = 256 << 10;
    let temp = tempfile::tempdir().unwrap();
    let store = create(HostDir::new(temp.path()).unwrap(), Options::default());
    store.write(extent(0), 0, &data(0, BLOCK)).await.unwrap();
    store.commit(None).await.unwrap();
    let mut writes = Waits::default();
    for sequence in 1..=mib * 4 {
        // Made before it is timed: making 256 KiB took as long as the store's write of it (on a
        // Mac, 200 µs to 140), so that until TASK-433.9 the times said half as much of the store.
        let bytes = data(sequence, EXTENT);
        let started = Instant::now();
        store.write(extent(sequence), 0, &bytes).await.unwrap();
        writes.add(started.elapsed());
    }
    let (started, before) = (Instant::now(), local_cell::written());
    store.commit_ordered(None).await.unwrap();
    let ordered = started.elapsed();
    store.flush().await.unwrap();
    let durable = started.elapsed();
    // Its segments framed: what the uploader takes waits for that.
    let closed = store.closed(usize::MAX).await.unwrap().len();
    let framed = started.elapsed();
    let after = local_cell::written();
    let own =
        std::fs::read_dir(temp.path().join(format!("extents/{}", base32_id::Padded(CELL.id))))
            .unwrap();
    writes.print("writes");
    let ms = |took: Duration| took.as_secs_f64() * 1e3;
    println!(
        "the commit: ordered in {:.1} ms, durable in {:.1} ms, its {closed} segments framed in \
         {:.1} ms, sealing {} extents in files of their own",
        ms(ordered),
        ms(durable),
        ms(framed),
        own.count()
    );
    let mib = |bytes: u64| bytes as f64 / (1 << 20) as f64;
    println!(
        "written for it: copied {:.1} MiB, own {:.1} MiB, framed {:.1} MiB, log {:.2} MiB",
        mib(after.copied - before.copied),
        mib(after.own - before.own),
        mib(after.framed - before.framed),
        mib(after.log - before.log)
    );
}

async fn durable(files: u64) {
    const COMMITS: u64 = 100;
    // Each extent's one write ends where an extent takes a file of its own, the rest of it a hole,
    // so that its file is quick to write, and the flush's syncs are most of what it takes.
    const WRITE: usize = 64 << 10;
    let temp = tempfile::tempdir().unwrap();
    let options = Options::default();
    let store = create(HostDir::new(temp.path()).unwrap(), options);
    let mut flushes = Waits::default();
    let mut sequence = 0;
    for _ in 0..COMMITS {
        for _ in 0..files {
            sequence += 1;
            let offset = options.large_extent - WRITE as u64;
            store.write(extent(sequence), offset, &data(sequence, WRITE)).await.unwrap();
        }
        store.commit_ordered(None).await.unwrap();
        let started = Instant::now();
        store.flush().await.unwrap();
        flushes.add(started.elapsed());
    }
    let own =
        std::fs::read_dir(temp.path().join(format!("extents/{}", base32_id::Padded(CELL.id))))
            .unwrap();
    flushes.print(&format!("flushes of commits sealing {files} extents in files of their own"));
    println!("{} extents in files of their own", own.count());
}

/// `CLADEFS_BENCH_{name}`'s value, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(format!("CLADEFS_BENCH_{name}")) {
        Ok(value) => value.parse().unwrap_or_else(|_| panic!("CLADEFS_BENCH_{name} is {value:?}")),
        Err(_) => default,
    }
}

/// Bytes that don't compress, differing by `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// Text of sixteen letters at random, differing by `seed`: four bits a byte, which compresses
/// about by half.
fn text(seed: u64, len: usize) -> Vec<u8> {
    noise(seed, len).into_iter().map(|byte| b'a' + (byte & 0x0f)).collect()
}

/// Has the calling thread, and the threads it starts after, run on the first `cpus` of the CPUs
/// it may run on now; all of them if `cpus` is 0. Linux's alone.
fn run_on(cpus: usize) {
    #[cfg(target_os = "linux")]
    if cpus > 0 {
        // SAFETY: reads and narrows the calling thread's CPU affinity.
        unsafe {
            let size = std::mem::size_of::<libc::cpu_set_t>();
            let mut allowed: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(libc::sched_getaffinity(0, size, &mut allowed), 0);
            let mut first: libc::cpu_set_t = std::mem::zeroed();
            let all = (0..libc::CPU_SETSIZE as usize).filter(|&cpu| libc::CPU_ISSET(cpu, &allowed));
            for cpu in all.take(cpus) {
                libc::CPU_SET(cpu, &mut first);
            }
            assert_eq!(libc::sched_setaffinity(0, size, &first), 0);
        }
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(cpus, 0, "CLADEFS_BENCH_CPUS on Linux alone");
}

/// Sets the calling thread's nice value to `nice`. Linux's alone, whose nice values are each
/// thread's.
fn nice(nice: i32) {
    if nice == 0 {
        return;
    }
    // SAFETY: sets the calling thread's priority; Linux's are a thread's, by its ID.
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        assert_eq!(libc::setpriority(libc::PRIO_PROCESS, tid, nice), 0, "nice {nice}");
    }
    #[cfg(not(target_os = "linux"))]
    panic!("nice {nice} on Linux alone");
}

/// cladefs: a host with a KMS, whose stores' uploaders keep their cells in a bucket in memory, a
/// runtime of their own, to open stores as `CLADEFS_BENCH_ENCRYPTED` says ([`Host::from_env`],
/// TASK-411.5).
struct Host {
    bucket: Arc<bucket_cell::object_bucket::ObjectBucket>,
    upload: local_cell::upload::Runtime,
    _kms: tempfile::TempDir,
}

impl Host {
    /// The tenant whose KEK the host's KMS keeps.
    const ENCRYPTED: u64 = 1;

    /// A host if `CLADEFS_BENCH_ENCRYPTED` is `1`; `0`, a clear twin, refused (TASK-411.28).
    fn from_env() -> Option<Self> {
        let encrypted = std::env::var("CLADEFS_BENCH_ENCRYPTED").ok()?;
        assert_eq!(
            encrypted, "1",
            "CLADEFS_BENCH_ENCRYPTED: no store is in the clear since TASK-411.28; an old pin's \
             cell-bench measures one"
        );
        let objects = Arc::new(object_store::memory::InMemory::new());
        let bucket = Arc::new(bucket_cell::object_bucket::ObjectBucket::new(objects, ""));
        let kms = tempfile::tempdir().unwrap();
        let local = extent_keys::kms::LocalKms::open(kms.path().join("kms")).unwrap();
        local.create(Self::ENCRYPTED).unwrap();
        let keys = bucket_cell::keys::Host::new(
            bucket_cell::keys::CachingKms::new(Arc::new(local)),
            bucket_cell::fetch::Memory::new().keys().clone(),
        );
        let uploads = bucket_cell::fetch::Budget::new(bucket_cell::fetch::Limits::UNBOUNDED);
        let upload = local_cell::upload::Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads,
            keys,
        };
        Some(Self { bucket, upload, _kms: kms })
    }

    /// The store in `dir`, made, of cell `id` of the host's cell, encrypted, opened through its
    /// uploader, which closes no segment between commits, holding the parts it packs ahead in
    /// `held` (TASK-400).
    async fn open(
        &self,
        dir: HostDir,
        id: u64,
        options: Options,
        held: Arc<HeldParts>,
    ) -> (Arc<LocalCell<HostDir>>, local_cell::upload::Uploader<HostDir>) {
        let tenant = Self::ENCRYPTED;
        let cell = local_cell::CellId { tenant, id };
        let description = Description { block_size: BLOCK as u32, capacity: 64 << 30, cell };
        let kind = bucket_cell::cell::Kind::Fxfs;
        LocalCell::make(&dir, description, Some("memory:///bench"), Some(kind)).unwrap();
        let making = bucket_cell::lifecycle::create(&*self.bucket, cell, id);
        making.await.unwrap();
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let interval = Duration::from_secs(3600);
        let uploading =
            local_cell::upload::Options { interval, held: Some(held), ..Default::default() };
        let holding = (id, uploading);
        let bucket = self.bucket.clone();
        let (store, uploader) =
            local_cell::upload::open(recovering, options, bucket, holding, &self.upload)
                .await
                .unwrap();
        store.made().unwrap();
        assert!(store.keys().encrypts());
        (store, uploader)
    }
}

/// cladefs: a temporary directory in `CLADEFS_BENCH_DIR`, if set.
fn temp_dir() -> tempfile::TempDir {
    match std::env::var_os("CLADEFS_BENCH_DIR") {
        Some(dir) => tempfile::tempdir_in(dir),
        None => tempfile::tempdir(),
    }
    .unwrap()
}

/// cladefs: `mib` MiB written to a store's open segments as extents of `CLADEFS_BENCH_EXTENT` KiB, read
/// back while open, then committed (TASK-411.5).
async fn writes(mib: u64) {
    const MIB: u64 = 1 << 20;
    let extent_kib = env("EXTENT", 1024u64);
    let host = Host::from_env();
    let temp = temp_dir();
    let dir = HostDir::new(temp.path()).unwrap();
    // Segments close at commits alone.
    let options = Options { segment_size: 1 << 30, ..Options::default() };
    let (store, _uploader) = match &host {
        Some(host) => {
            let (store, uploader) = host.open(dir, 1, options, HeldParts::new()).await;
            (store, Some(uploader))
        }
        None => (Arc::new(create(dir, options)), None),
    };
    let len = (extent_kib << 10) as usize;
    let count = mib * MIB / len as u64;
    let bytes = noise(1, len);
    let started = Instant::now();
    for sequence in 0..count {
        store.write(extent(sequence), 0, &bytes).await.unwrap();
    }
    let written = started.elapsed();
    let mut buf = vec![0; len];
    let started = Instant::now();
    for sequence in 0..count {
        store.read(extent(sequence), 0, &mut buf).unwrap();
    }
    let read = started.elapsed();
    assert!(buf == bytes);
    let started = Instant::now();
    store.commit(None).await.unwrap();
    let committed = started.elapsed();
    let kind = match &host {
        Some(_) => "encrypted, through an uploader",
        None => "encrypted, kept nowhere",
    };
    let rate = |elapsed: Duration| (count * len as u64) as f64 / MIB as f64 / elapsed.as_secs_f64();
    println!(
        "{kind}: {count} extents of {extent_kib} KiB: writes {:.0} MiB/s, reads while open {:.0} \
         MiB/s; the commit {:.1} ms",
        rate(written),
        rate(read),
        committed.as_secs_f64() * 1e3
    );
}

fn framing(segments: u64, runtime: &tokio::runtime::Handle) {
    const MIB: usize = 1 << 20;
    const SEGMENT: u64 = 32;
    let (stores, hogs, cpus) = (env("STORES", 4u64), env("HOGS", 0usize), env("CPUS", 0usize));
    let (hog_nice, kind) = (env("HOG_NICE", 0i32), env("DATA", "text".to_owned()));
    let evict = env("EVICT", 0u8) != 0;
    let held = HeldParts::new();
    let cpu_before = cpu_seconds();
    let writing = std::sync::Mutex::new(Duration::ZERO);
    run_on(cpus);
    let temp = temp_dir();
    let host = {
        let _entered = runtime.enter();
        Host::from_env()
    };
    let done = Arc::new(AtomicBool::new(false));
    let hogging: Vec<_> = (0..hogs)
        .map(|_| {
            let done = done.clone();
            std::thread::spawn(move || {
                nice(hog_nice);
                while !done.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    let bytes = move |sequence: u64| match kind.as_str() {
        "text" => text(sequence, MIB),
        "noise" => noise(sequence, MIB),
        "half" if (sequence / SEGMENT).is_multiple_of(2) => text(sequence, MIB),
        "half" => noise(sequence, MIB),
        other => panic!("CLADEFS_BENCH_DATA is {other:?}"),
    };
    let before = local_cell::timings();
    let written = (local_cell::written(), local_cell::segments_framed_in_place());
    let started = Instant::now();
    std::thread::scope(|scope| {
        for store in 0..stores {
            let (temp, bytes, host) = (temp.path().join(format!("{store}")), &bytes, &host);
            let (held, writing) = (&held, &writing);
            // Each store's writer a thread of the bench's, its outermost entry, its awaits in the
            // bench's runtime.
            scope.spawn(move || {
                runtime.block_on(async move {
                    std::fs::create_dir(&temp).unwrap();
                    // Segments close between commits alone, a commit after each 32 MiB.
                    let options = Options { segment_size: 1 << 30, ..Options::default() };
                    let dir = HostDir::new(&temp).unwrap();
                    let (store, _uploader) = match host {
                        Some(host) => {
                            let (store, uploader) =
                                host.open(dir, store + 1, options, held.clone()).await;
                            (store, Some(uploader))
                        }
                        None => {
                            let description = Description {
                                block_size: BLOCK as u32,
                                capacity: 64 << 30,
                                cell: CELL,
                            };
                            LocalCell::make(&dir, description, None, None).unwrap();
                            let opening =
                                Opening { held: Some(held.clone()), ..Opening::new(CELL) };
                            let store = LocalCell::recover_unmade(dir)
                                .unwrap()
                                .open(options, opening)
                                .unwrap();
                            store.made().unwrap();
                            (Arc::new(store), None)
                        }
                    };
                    let (cell, mut sequence) = (store.description().cell.id, 0);
                    for _ in 0..segments / stores {
                        let started = Instant::now();
                        for _ in 0..SEGMENT {
                            let extent = ExtentId::new(cell, sequence);
                            store.write(extent, 0, &bytes(sequence)).await.unwrap();
                            sequence += 1;
                        }
                        *writing.lock().unwrap() += started.elapsed();
                        if evict {
                            held.evict_to(0);
                        }
                        store.close_segment().unwrap();
                        store.commit(None).await.unwrap();
                        store.flush().await.unwrap();
                        // Its framing written, its timings recorded.
                        store.closed(usize::MAX).await.unwrap();
                    }
                })
            });
        }
    });
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    hogging.into_iter().for_each(|hog| hog.join().unwrap());
    let after = local_cell::timings().framings;
    let count = after.count - before.framings.count;
    let ms = |us: u64| us as f64 / 1e3;
    let named = |phases: local_cell::Phases<4>, per: u64| {
        let names = local_cell::FRAMING_PHASES.iter().zip(phases.us);
        let named: Vec<String> =
            names.map(|(name, us)| format!("{name} {:.1}", ms(us / per))).collect();
        let total = ms(phases.total_us() / per);
        format!(
            "{total:.1} ms ({} ms; runnable {:.1} ms)",
            named.join(", "),
            ms(phases.runnable_us / per)
        )
    };
    let mut sum = after.sum;
    for (phase, before) in before.framings.sum.us.iter().enumerate() {
        sum.us[phase] -= before;
    }
    sum.runnable_us -= before.framings.sum.runnable_us;
    let encrypted = match &host {
        Some(_) => ", encrypted, through uploaders",
        None => ", encrypted, kept nowhere",
    };
    println!(
        "{count} framings of {SEGMENT} MiB{encrypted}, {stores} stores at once, {hogs} spinning \
         threads at nice {hog_nice}, on {} CPUs, in {:.1} s",
        if cpus == 0 { "all".to_owned() } else { cpus.to_string() },
        elapsed.as_secs_f64()
    );
    println!("  each, on average: {}", named(sum, count.max(1)));
    println!("  the slowest: {}", named(after.slowest, 1));
    println!("  each phase's longest: {}", named(after.max, 1));
    println!("  past 50 ms: {}", after.slow - before.framings.slow);
    let held = held.counts();
    let mib = |bytes: u64| bytes as f64 / MIB as f64;
    println!(
        "  parts held: {:.1} MiB in all, at most {:.1} MiB at once, {:.1} MiB taken, {:.1} MiB \
         let go of ({} parts)",
        mib(held.held),
        mib(held.peak),
        mib(held.taken),
        mib(held.evicted),
        held.evicted_parts
    );
    let (after_written, in_place) = (local_cell::written(), local_cell::segments_framed_in_place());
    let (raw, framed) =
        (after_written.raw - written.0.raw, after_written.framed - written.0.framed);
    println!(
        "  written: {:.1} MiB raw, {:.1} MiB framed, {:.2} times the data; {} of {count} framed in \
         place",
        mib(raw),
        mib(framed),
        (raw + framed) as f64 / raw.max(1) as f64,
        in_place - written.1,
    );
    let writing = writing.into_inner().unwrap();
    println!(
        "  CPU {:.2} s; writing each segment {:.1} ms on average",
        cpu_seconds() - cpu_before,
        writing.as_secs_f64() * 1e3 / count.max(1) as f64
    );
}

/// The process's CPU time so far, user and system, in seconds.
fn cpu_seconds() -> f64 {
    // SAFETY: getrusage fills `usage`, which it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// cladefs: on Linux, what the extent daemon's governor does at its first look (TASK-433.8): the
/// stores' page cache sized to what the host's memory, less the daemon's headroom, leaves it.
#[cfg(target_os = "linux")]
fn govern() {
    use cladefs::cladefsd::{governor, memory};
    let host = governor::host_memory().expect("reading the host's memory");
    let headroom = governor::headroom(host.total).expect("the daemon's headroom");
    // The bench's outermost entry, before its runtime: what the governor's turn awaits, here.
    let allowance = governor::allowance(memory::governed_bytes(), host, headroom);
    futures::executor::block_on(memory::govern(allowance));
}

/// cladefs: elsewhere, no governor: the stores' page cache goes by its defaults, as a process's
/// with none does.
#[cfg(not(target_os = "linux"))]
fn govern() {}

fn main() {
    govern();
    let args: Vec<String> = std::env::args().collect();
    let n = |default| args.get(2).map_or(default, |n| n.parse().expect("N is a number"));
    // The bench's runtime, as a daemon's: its stores' uploaders run on it, and its steps.
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    match args.get(1).map(String::as_str) {
        Some("lagging-view") => runtime.block_on(lagging_view(n(20_000))),
        Some("segments") => runtime.block_on(segments(n(2_000))),
        Some("close-now") => runtime.block_on(close_now(n(32))),
        Some("burst") => runtime.block_on(burst(n(256))),
        Some("durable") => runtime.block_on(durable(n(2))),
        Some("framing") => framing(n(64), runtime.handle()),
        Some("writes") => runtime.block_on(writes(n(256))),
        _ => {
            eprintln!(
                "usage: cell-bench lagging-view|segments|close-now|burst|durable|framing|writes \
                 [N]"
            );
            std::process::exit(2);
        }
    }
}
