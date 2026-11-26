// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! How many SQLite cells one extent daemon holds open at once, and what each costs (board 13862):
//! a process embedding the daemon, as libcladefs's callers do, holding a SQLite cell per database
//! with no VM, as a broker keeping a cell per partition would. The cells-per-host cases
//! (guest/tests/test_cases/src/test_cells_per_host.rs) measure VMs; this measures cells alone.
//!
//! Cells open in steps of [`STEP`], each made embedded in the process's daemon
//! (`daemon::create_embedded`: no VM-process wire, board 13909) and filled with fake records in a
//! few transactions. After each step a line says what the cells cost the process
//! (anonymous memory, the daemon's shared memory it maps, threads, open files, each per cell, over
//! the process before any cell), the daemon's gauges, and probes' p99: a point read and a
//! one-record append, on cells spread over those open. What each cell after the first step cost is
//! held to budgets ([`Budget`]). Once all are open, every cell is synced to the bucket, then left
//! idle a while, which must cost no commit; then every database is checked, closed, and a sample
//! opened again by ID and checked.
//!
//! `CLADEFS_SQLITE_DENSITY_CELLS` raises the cells for a measurement past the test's [`CELLS`]:
//! the test fails at the first cell the daemon refuses, saying how many it held.
//!
//! Its own test binary, a process of its own: the measurements are the process's, and one daemon
//! a process (`Daemon::process`).

#![cfg(target_os = "linux")]

use cladefs::cladefsd::Daemon;
use cladefs::cladefsd::control::proto::{GetStatsRequest, GetStatsResponse, SyncCellRequest};
use rusqlite::{Connection, OpenFlags, params};
use sqlite_cladefs::{Database, Options, Registration, daemon, register};
use std::collections::BTreeMap;
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The cells the test opens, unless `CLADEFS_SQLITE_DENSITY_CELLS` says: some 8,000 open files at
/// a cell's ~7.8 (board 13914), and under a minute. 4,096 held on ubu1 (hotbed, 2026-11-26) in
/// 285 s; board 13907 is to 24,500.
const CELLS: usize = 1024;

/// The cells each step opens.
const STEP: usize = 64;

/// Each cell's capacity, and its window's size: sized to the data, as a window should be.
const CAPACITY: u64 = 64 << 20;

/// The records each cell is filled with, [`BATCH`] a transaction.
const RECORDS: i64 = 256;
const BATCH: i64 = 64;

/// The cells each step's probes read and append to, spread over those open.
const PROBES: usize = 32;

/// How long the cells are left idle, the CPU and commits of the process measured over it.
const IDLE: Duration = Duration::from_secs(5);

/// The cells opened again by ID at the end, spread over all.
const REOPENED: usize = 16;

/// What each cell opened after the first step may cost the process at most (board 13862), the
/// daemon's and SQLite's together. Measured on ubu1 (hotbed, 2026-11-26) at 1,024 and 4,096 cells,
/// with a margin: no thread, 7.6-7.8 open files (its directory's: board 13914), 380-466 KiB of
/// anonymous memory (board 13916), none of the daemon's shared memory, an embedded cell's reads
/// being the open cell's own. Its connection to the daemon cost 2 threads, 14 more files and 344
/// KiB of shared memory before cells were embedded (board 13909).
struct Budget;

impl Budget {
    const THREADS: f64 = 0.05;
    const FILES: f64 = 8.5;
    const ANON_KIB: f64 = 512.0;
    const SHMEM_KIB: f64 = 16.0;
}

/// A SQLite cell open on the daemon, its database filled with records `0..records`, each as
/// [`record`] makes it of the cell's `index`.
struct Cell {
    index: u64,
    id: cladefs::CellId,
    records: i64,
    connection: Connection,
    registration: Registration,
    database: Arc<Database>,
}

impl Cell {
    /// A new SQLite cell, the `index`th, filled.
    fn create(index: u64) -> Self {
        let (id, database) =
            daemon::create_embedded(Daemon::process(), 0, CAPACITY, Options::default())
                .unwrap_or_else(|error| panic!("cell {index}, after {index} held: {error:#}"));
        let mut cell = Self::mount(database, id, index);
        cell.connection
            .execute_batch(
                "CREATE TABLE records (offset INTEGER PRIMARY KEY, ts INTEGER NOT NULL, \
                 key TEXT NOT NULL, value BLOB NOT NULL)",
            )
            .unwrap();
        for _ in 0..RECORDS / BATCH {
            cell.append(BATCH);
        }
        cell
    }

    /// The cell `id` opened again, the `index`th, holding `records`.
    fn reopen(id: cladefs::CellId, index: u64, records: i64) -> Self {
        let database = daemon::open_embedded(Daemon::process(), id, Options::default())
            .unwrap_or_else(|error| panic!("opening cell {index} ({id}) again: {error:#}"));
        let mut cell = Self::mount(database, id, index);
        cell.records = records;
        cell
    }

    /// `database`, registered as a VFS of its own, and connected to.
    fn mount(database: Arc<Database>, id: cladefs::CellId, index: u64) -> Self {
        static MOUNTS: AtomicU64 = AtomicU64::new(0);
        let vfs = format!("density-{}", MOUNTS.fetch_add(1, Ordering::Relaxed));
        let registration = register(&vfs, database.clone()).unwrap();
        let connection =
            Connection::open_with_flags_and_vfs("records.db", OpenFlags::default(), vfs.as_str())
                .unwrap();
        Self { index, id, records: 0, connection, registration, database }
    }

    /// Appends `count` records, in one transaction.
    fn append(&mut self, count: i64) {
        let transaction = self.connection.unchecked_transaction().unwrap();
        for offset in self.records..self.records + count {
            let (ts, key, value) = record(self.index, offset);
            transaction
                .execute(
                    "INSERT INTO records VALUES (?1, ?2, ?3, ?4)",
                    params![offset, ts, key, value],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        self.records += count;
    }

    /// Checks the record at `offset` reads back as written.
    fn read(&self, offset: i64) {
        let read: (i64, String, Vec<u8>) = self
            .connection
            .query_row("SELECT ts, key, value FROM records WHERE offset = ?1", [offset], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert!(read == record(self.index, offset), "cell {}'s record {offset}", self.index);
    }

    /// Checks the database is whole and holds its records, each as written, and no other.
    fn check(&self) {
        let integrity: String =
            self.connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
        assert_eq!(integrity, "ok", "cell {}", self.index);
        let mut statement = self
            .connection
            .prepare("SELECT offset, ts, key, value FROM records ORDER BY offset")
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        let mut expected = 0;
        while let Some(row) = rows.next().unwrap() {
            let offset: i64 = row.get(0).unwrap();
            assert_eq!(offset, expected, "cell {}'s records", self.index);
            let read = (row.get(1).unwrap(), row.get(2).unwrap(), row.get(3).unwrap());
            assert!(read == record(self.index, offset), "cell {}'s record {offset}", self.index);
            expected += 1;
        }
        assert_eq!(expected, self.records, "cell {}'s records", self.index);
    }

    fn close(self) {
        let Self { connection, registration, database, .. } = self;
        drop(connection);
        drop(registration);
        database.close().unwrap();
    }
}

/// The fake record at `offset` of the `index`th cell: a timestamp, a key of a thousand, and a
/// value of 32 to 511 bytes, all of a seed of both.
fn record(index: u64, offset: i64) -> (i64, String, Vec<u8>) {
    let mut state = (index << 32) ^ offset as u64 ^ 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        // splitmix64.
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let ts = 1_767_225_600_000 + offset * 1000 + (next() % 1000) as i64;
    let key = format!("user-{:03}", next() % 1000);
    let len = 32 + (next() % 480) as usize;
    let value = (0..len.div_ceil(8)).flat_map(|_| next().to_le_bytes()).take(len).collect();
    (ts, key, value)
}

/// What the process holds, as Linux counts it.
#[derive(Clone, Copy, Debug)]
struct Process {
    /// Its anonymous memory resident, in KiB (`RssAnon`).
    anon_kib: u64,
    /// The shared memory it maps resident, in KiB (`RssShmem`): the daemon's arenas' pages.
    shmem_kib: u64,
    threads: u64,
    files: u64,
    /// Its memory mappings, which `vm.max_map_count` bounds.
    maps: u64,
    /// Of `anon_kib`, what SQLite holds (`sqlite3_memory_used`): its connections' page caches,
    /// schemas and statements.
    sqlite_kib: u64,
    /// Of `anon_kib`, what the cells' page cache holds (`local_cell::pages`), which its budget
    /// evicts: the pages of their directories' files, written and read.
    page_cache_kib: u64,
}

impl Process {
    fn now() -> Self {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |name: &str| -> u64 {
            let line = status.lines().find(|line| line.starts_with(name)).unwrap();
            line[name.len()..].trim().trim_end_matches(" kB").parse().unwrap()
        };
        let files = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap().lines().count() as u64;
        Self {
            anon_kib: field("RssAnon:"),
            shmem_kib: field("RssShmem:"),
            threads: field("Threads:"),
            files,
            maps,
            // SAFETY: a plain query of SQLite's count.
            sqlite_kib: unsafe { rusqlite::ffi::sqlite3_memory_used() } as u64 >> 10,
            page_cache_kib: local_cell::pages::shared().usage().held >> 10,
        }
    }
}

/// `name` with each part holding a digit, between dots, dashes, slashes or spaces, as `#`: so that
/// a kind's files, or threads, of every cell count as one.
fn kind_of(name: &str) -> String {
    let mut kind = String::new();
    for part in name.split_inclusive(['.', '-', '/', ' ', '_']) {
        let (word, end) = match part.char_indices().last() {
            Some((at, c)) if ".-/ _".contains(c) => (&part[..at], &part[at..]),
            _ => (part, ""),
        };
        kind.push_str(if word.chars().any(|c| c.is_ascii_digit()) { "#" } else { word });
        kind.push_str(end);
    }
    kind
}

/// The process's open files, how many of each kind: a socket, an anonymous inode's kind
/// (`userfaultfd`, `eventfd`), a memfd, or a file's name and its directories' ([`kind_of`]).
fn files_by_kind() -> BTreeMap<String, f64> {
    let mut kinds = BTreeMap::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let target = match std::fs::read_link(entry.unwrap().path()) {
            Ok(target) => target.to_string_lossy().into_owned(),
            // Closed since the listing: none of the process's then.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("reading an open file's name: {error}"),
        };
        let kind = match target.split_once(':') {
            Some(("anon_inode", inner)) => inner.trim_matches(['[', ']']).to_owned(),
            Some(("socket" | "pipe", _)) => target[..target.find(':').unwrap()].to_owned(),
            _ if target.starts_with("/memfd:") => "memfd".to_owned(),
            // A cell's file by the kind of directory it is in too, `logs/#/#`, a cell's files of a
            // kind being in a directory of the cell's (`local_cell::layout::cell_dir`).
            _ => {
                let parts: Vec<&str> = target.rsplitn(4, '/').take(3).collect();
                kind_of(&parts.into_iter().rev().collect::<Vec<_>>().join("/"))
            }
        };
        *kinds.entry(kind).or_default() += 1.0;
    }
    kinds
}

/// The process's threads by name ([`kind_of`]): how many, and their CPU time in milliseconds.
fn threads_by_name() -> BTreeMap<String, (f64, f64)> {
    // SAFETY: a plain query of a constant.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let mut threads = BTreeMap::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let task = task.unwrap().path();
        let (comm, stat) = match (
            std::fs::read_to_string(task.join("comm")),
            std::fs::read_to_string(task.join("stat")),
        ) {
            (Ok(comm), Ok(stat)) => (comm, stat),
            // Exited since the listing: none of the process's then.
            (Err(error), _) | (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            (Err(error), _) | (_, Err(error)) => panic!("reading a thread: {error}"),
        };
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
        let ticks = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
        let entry = threads.entry(kind_of(comm.trim_end())).or_insert((0.0, 0.0));
        entry.0 += 1.0;
        entry.1 += ticks * 1000.0 / per_second;
    }
    threads
}

/// What grew from `then` to `now`, divided by `by`, largest first, as `name=value` pairs: those
/// that moved by a hundredth at least.
fn grown(then: &BTreeMap<String, f64>, now: &BTreeMap<String, f64>, by: f64) -> String {
    let mut grown: Vec<(&String, f64)> = (now.iter())
        .map(|(name, value)| (name, (value - then.get(name).copied().unwrap_or(0.0)) / by))
        .filter(|(_, value)| value.abs() >= 0.01)
        .collect();
    grown.sort_by(|a, b| b.1.total_cmp(&a.1));
    let pairs: Vec<String> =
        grown.iter().map(|(name, value)| format!("{name}={value:.2}")).collect();
    pairs.join(" ")
}

/// The process's CPU time, user and system, in milliseconds.
fn cpu_ms() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // After the command's name, in parentheses, which may hold spaces: utime and stime are the
    // 12th and 13th fields.
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: a plain query of a constant.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    ticks * 1000 / per_second
}

/// The p99 of `samples`, in milliseconds.
fn p99_ms(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    let at = (samples.len() * 99).div_ceil(100).saturating_sub(1);
    samples[at].as_secs_f64() * 1000.0
}

/// `of` per cell, over `cells`.
fn per(of: u64, cells: usize) -> f64 {
    of as f64 / cells as f64
}

#[test]
fn sqlite_cells_held_open_at_once() {
    let cells_wanted = std::env::var("CLADEFS_SQLITE_DENSITY_CELLS")
        .map_or(CELLS, |cells| cells.parse().expect("CLADEFS_SQLITE_DENSITY_CELLS: a number"));
    // As cladefsd does before it boots: what the daemon serves at once is of this limit.
    cladefs::cladefsd::raise_open_files_limit().unwrap();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    cladefs::cladefsd::Daemon::process().configure_disks(&[dir.path().join("disk0")]).unwrap();
    // SAFETY: before this process starts any other thread.
    unsafe {
        std::env::set_var("CLADEFSD_BUCKET", "memory:///sqlite-density");
        std::env::set_var("CLADEFSD_TOKEN", "1");
    }
    let sockets = tempfile::tempdir().unwrap();
    let control_socket = sockets.path().join("control.sock");
    let listener = UnixListener::bind(&control_socket).unwrap();
    let tenants = sockets.path().join("tenants");
    std::fs::create_dir(&tenants).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::control::serve_control(listener, tenants));
    let control = cladefs::testing::Control::connect(&control_socket).unwrap();
    let stats = || -> GetStatsResponse {
        let request = GetStatsRequest { leave_out_vms: true, ..Default::default() };
        control.call(async |client| client.get_stats(request).await).unwrap()
    };
    let gauge = |stats: &GetStatsResponse, name: &str| stats.gauges[name];

    assert!(cells_wanted > STEP, "{cells_wanted} cells: a step's and more, for a cell's cost");
    let before = Process::now();
    let mut first_step = None;
    let mut cells: Vec<Cell> = Vec::with_capacity(cells_wanted);
    let mut step = 0;
    while cells.len() < cells_wanted {
        step += 1;
        let added = STEP.min(cells_wanted - cells.len());
        // The step's cells made at once, each on a thread of its own, as a process opening many
        // databases would (board 13918).
        let first = cells.len() as u64;
        let made: Vec<(Cell, Duration)> = std::thread::scope(|scope| {
            let making: Vec<_> = (first..first + added as u64)
                .map(|index| {
                    scope.spawn(move || {
                        let started = Instant::now();
                        (Cell::create(index), started.elapsed())
                    })
                })
                .collect();
            making.into_iter().map(|making| making.join().unwrap()).collect()
        });
        let opens = made.iter().map(|(_, took)| *took).collect();
        cells.extend(made.into_iter().map(|(cell, _)| cell));
        let (mut reads, mut appends) = (Vec::new(), Vec::new());
        let (open, probes) = (cells.len(), PROBES.min(cells.len()));
        for probe in 0..probes {
            let cell = &mut cells[probe * open / probes];
            let offset = (probe as i64 * 37) % cell.records;
            let started = Instant::now();
            cell.read(offset);
            reads.push(started.elapsed());
            let started = Instant::now();
            cell.append(1);
            appends.push(started.elapsed());
        }
        let (now, stats) = (Process::now(), stats());
        let n = cells.len();
        println!(
            "sqlite-density: step={step} cells={n} added={added} open_p99_ms={:.1} \
             anon_kib_per_cell={:.1} sqlite_kib_per_cell={:.1} page_cache_kib_per_cell={:.1} \
             shmem_kib_per_cell={:.1} threads_per_cell={:.2} \
             files_per_cell={:.2} daemon_memory_mib={} stores_open={} read_p99_ms={:.2} \
             append_p99_ms={:.2}",
            p99_ms(opens),
            per(now.anon_kib.saturating_sub(before.anon_kib), n),
            per(now.sqlite_kib.saturating_sub(before.sqlite_kib), n),
            per(now.page_cache_kib.saturating_sub(before.page_cache_kib), n),
            per(now.shmem_kib.saturating_sub(before.shmem_kib), n),
            per(now.threads.saturating_sub(before.threads), n),
            per(now.files.saturating_sub(before.files), n),
            gauge(&stats, "memory_bytes") >> 20,
            gauge(&stats, "stores_open"),
            p99_ms(reads),
            p99_ms(appends),
        );
        first_step.get_or_insert_with(|| (n, now, files_by_kind(), threads_by_name()));
    }
    assert_eq!(stats().gauges["stores_open"], cells.len() as u64);

    // What each cell after the first step cost: the process's fixed costs, its pools' threads and
    // the like, the first step's.
    let ((first, then, files_then, threads_then), now) = (first_step.unwrap(), Process::now());
    let added = cells.len() - first;
    let counts = |threads: &BTreeMap<String, (f64, f64)>| -> BTreeMap<String, f64> {
        threads.iter().map(|(name, (count, _))| (name.clone(), *count)).collect()
    };
    println!(
        "sqlite-density: a cell's files: {}",
        grown(&files_then, &files_by_kind(), added as f64)
    );
    println!(
        "sqlite-density: a cell's threads: {}",
        grown(&counts(&threads_then), &counts(&threads_by_name()), added as f64)
    );
    let cost = |now: u64, then: u64| per(now.saturating_sub(then), added);
    let (threads, files) = (cost(now.threads, then.threads), cost(now.files, then.files));
    let (anon_kib, shmem_kib) =
        (cost(now.anon_kib, then.anon_kib), cost(now.shmem_kib, then.shmem_kib));
    let maps = cost(now.maps, then.maps);
    let (sqlite_kib, page_cache_kib) =
        (cost(now.sqlite_kib, then.sqlite_kib), cost(now.page_cache_kib, then.page_cache_kib));
    println!(
        "sqlite-density: a cell costs threads={threads:.2} files={files:.2} maps={maps:.2} \
         anon_kib={anon_kib:.1} (sqlite_kib={sqlite_kib:.1} page_cache_kib={page_cache_kib:.1}) \
         shmem_kib={shmem_kib:.1}"
    );
    assert!(threads <= Budget::THREADS, "{threads:.2} threads a cell");
    assert!(files <= Budget::FILES, "{files:.2} open files a cell");
    assert!(anon_kib <= Budget::ANON_KIB, "{anon_kib:.1} KiB of anonymous memory a cell");
    assert!(shmem_kib <= Budget::SHMEM_KIB, "{shmem_kib:.1} KiB of shared memory a cell");

    // Every cell in the bucket, so that the idle cells have nothing left to upload.
    for cell in &cells {
        let request = SyncCellRequest { tenant: cell.id.tenant, cell: cell.id.id };
        control
            .call(async |client| client.sync_cell(request).await)
            .unwrap_or_else(|error| panic!("syncing cell {}: {error:#}", cell.index));
    }
    let (cpu_before, commits_before) = (cpu_ms(), stats().counters["commits"]);
    let threads_before = threads_by_name();
    let idle = Instant::now();
    std::thread::sleep(IDLE);
    let (cpu, commits) = (cpu_ms() - cpu_before, stats().counters["commits"] - commits_before);
    let idle_s = idle.elapsed().as_secs_f64();
    let cpu_ms_per_s = cpu as f64 / idle_s;
    let cpu_of = |threads: &BTreeMap<String, (f64, f64)>| -> BTreeMap<String, f64> {
        threads.iter().map(|(name, (_, ms))| (name.clone(), *ms)).collect()
    };
    println!(
        "sqlite-density: idle CPU ms/s by thread: {}",
        grown(&cpu_of(&threads_before), &cpu_of(&threads_by_name()), idle_s)
    );
    println!(
        "sqlite-density: idle cells={} cpu_ms_per_s={cpu_ms_per_s:.1} commits={commits}",
        cells.len()
    );
    assert_eq!(commits, 0, "idle cells committed");

    for cell in &cells {
        cell.check();
    }
    let sample: Vec<_> = (0..REOPENED.min(cells.len()))
        .map(|at| at * cells.len() / REOPENED.min(cells.len()))
        .map(|at| (cells[at].id, cells[at].index, cells[at].records))
        .collect();
    let held = cells.len();
    for cell in cells {
        cell.close();
    }
    for (id, index, records) in sample {
        let cell = Cell::reopen(id, index, records);
        cell.check();
        cell.close();
    }
    println!("sqlite-density: held={held} limit=none");
}
