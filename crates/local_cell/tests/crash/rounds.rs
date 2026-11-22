// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The crash tests' rounds: a cell's random workloads, crashing at random versions, and what it
//! recovers checked; each a function of its seed and of the directory it is given, as long as the
//! directory's file system fails, and its crashes keep, what the cell's calls decide alone. The
//! crash tests run them in a serial directory ([`MemDir::serial`]); the Shuttle tests with the
//! cell's threads, on schedules of Shuttle's seeded by the rounds (tests/shuttle/crash.rs).

use local_cell::{Description, Dir as _, File as _, LocalCell, MemDir, Options};
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::collections::{BTreeMap, BTreeSet};
use storage_device::{ExtentId, SealedExtent};

pub const BLOCK: usize = 4096;
pub const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(3);
/// How many recovering rounds the tests run.
pub const ROUNDS: u64 = 300;
const STEPS: usize = 200;

/// Small segments, large extents and index files, so that workloads make many of each. cladefs:
/// and extents small enough that a segment fills between commits, holding a segment and an extent
/// at most, so that commits span segments, sealing extents in those filled (TASK-92.41): with
/// extents of 64 MiB, none ever filled, and no round made such a commit.
pub const OPTIONS: Options = Options {
    max_extent: 48 << 10,
    segment_size: 16 << 10,
    segment_age: std::time::Duration::from_secs(3600),
    large_extent: 24 << 10,
    open_files: 3,
    view_lag: None,
    view_kept_bytes: None,
    // cladefs: flushes of either kind, holding their runs or checking them (TASK-301).
    held_most: 8 << 10,
};

/// What a cell holds: its extents' contents, and its root.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub extents: BTreeMap<ExtentId, Vec<u8>>,
    pub root: Vec<u8>,
}

impl std::fmt::Debug for Model {
    /// Each extent's sequence number, length and hash, and the root.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let extents: Vec<_> = self
            .extents
            .iter()
            .map(|(extent, data)| {
                (extent.sequence, data.len() / BLOCK, rapidhash::rapidhash(data) as u16)
            })
            .collect();
        write!(f, "{extents:?} root {:?}", String::from_utf8_lossy(&self.root))
    }
}

/// One to `most` blocks of data, which compress, or, half the time, don't: cladefs: segments of
/// parts stored compressed and of parts stored raw, which the kept rounds' passes compact alike.
fn random_data(rng: &mut StdRng, most: usize) -> Vec<u8> {
    let blocks = rng.random_range(1..=most);
    random_blocks(rng, blocks)
}

/// `blocks` blocks of data, which compress, or, half the time, don't.
fn random_blocks(rng: &mut StdRng, blocks: usize) -> Vec<u8> {
    if rng.random_bool(0.5) {
        return (0..blocks * BLOCK).map(|_| rng.random()).collect();
    }
    let byte: u8 = rng.random();
    (0..blocks * BLOCK).map(|i| byte ^ (i / BLOCK) as u8).collect()
}

/// cladefs: the blocks of each of a burst's extents: under `large_extent`, so that the open segment
/// holds it.
const BURST_BLOCKS: usize = OPTIONS.large_extent as usize / BLOCK - 1;

/// cladefs: how many extents a burst writes between two commits: more than two segments hold at
/// most (`Options::segment_limit`), so that one fills, and the commit after spans segments
/// (TASK-92.41), however full the open segment was, and though an uploader closes it once among
/// them, as it may between two commits. Without bursts, a commit spanned segments only after a
/// long run of writes, which an uploader's closing, when it came, cut short: the kept rounds made
/// 26 to 33 such commits in 100 on the Mac, and 20 once in CI, at their bound (TASK-431).
const BURST: usize =
    2 * (OPTIONS.segment_size + OPTIONS.max_extent) as usize / (BURST_BLOCKS * BLOCK) + 1;

/// A workload under way: the cell, what it holds, and what its commits began with.
struct Run<'a> {
    cell: &'a LocalCell<MemDir>,
    epochs: &'a mut Vec<Model>,
    model: Model,
    /// The last commit made durable.
    durable: usize,
    /// The extents written since the last commit.
    open: Vec<ExtentId>,
    next_sequence: u64,
}

impl Run<'_> {
    /// A new extent of `data`.
    async fn write_new(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let extent = ExtentId::new(self.cell.description().cell.id, self.next_sequence);
        self.next_sequence += 1;
        let result = self.cell.write(extent, 0, &data).await;
        if result.is_ok() {
            self.model.extents.insert(extent, data);
            self.open.push(extent);
        }
        result
    }

    /// More of an open extent, over what it holds or past it.
    async fn write_more(&mut self, rng: &mut StdRng) -> anyhow::Result<()> {
        let extent = self.open[rng.random_range(0..self.open.len())];
        let held = self.model.extents[&extent].len() / BLOCK;
        let offset = rng.random_range(0..=held);
        let data = random_data(rng, 2);
        let result = self.cell.write(extent, (offset * BLOCK) as u64, &data).await;
        if result.is_ok() {
            let contents = self.model.extents.get_mut(&extent).unwrap();
            if contents.len() < offset * BLOCK + data.len() {
                contents.resize(offset * BLOCK + data.len(), 0);
            }
            contents[offset * BLOCK..offset * BLOCK + data.len()].copy_from_slice(&data);
        }
        result
    }

    /// Frees an extent the cell holds.
    fn free(&mut self, rng: &mut StdRng) -> anyhow::Result<()> {
        let extents: Vec<_> = self.model.extents.keys().copied().collect();
        let extent = extents[rng.random_range(0..extents.len())];
        let result = self.cell.free(extent);
        if result.is_ok() {
            self.model.extents.remove(&extent);
            self.open.retain(|open| *open != extent);
        }
        result
    }

    /// A commit, durable or only ordered, setting the root or not.
    async fn commit(&mut self, rng: &mut StdRng) -> anyhow::Result<()> {
        let ordered = rng.random_bool(0.5);
        let root = rng.random_bool(0.5).then(|| format!("root {}", self.epochs.len()));
        if let Some(root) = &root {
            self.model.root = root.clone().into_bytes();
        }
        self.epochs.push(self.model.clone());
        self.open.clear();
        let root = root.as_deref().map(str::as_bytes);
        let cell = self.cell;
        let result = match ordered {
            true => cell.commit_ordered(root).await,
            false => cell.commit(root).await,
        };
        if result.is_ok() && !ordered {
            self.durable = self.epochs.len() - 1;
        }
        result
    }

    /// The commits only ordered so far made durable.
    async fn flush(&mut self) -> anyhow::Result<()> {
        let result = self.cell.flush().await;
        if result.is_ok() {
            self.durable = self.epochs.len() - 1;
        }
        result
    }

    /// cladefs: a burst, [`BURST`] new extents, then the commit that spans the segments they
    /// filled.
    async fn burst(&mut self, rng: &mut StdRng) -> anyhow::Result<()> {
        for _ in 0..BURST {
            self.write_new(random_blocks(rng, BURST_BLOCKS)).await?;
        }
        self.commit(rng).await
    }
}

/// Runs a random workload on `cell`, which holds `model`, committed or not, and whose commits up
/// to `durable` are durable, until it ends or the cell fails, recording in `epochs` what each
/// commit began with, and returns the epoch of the last commit made durable, and what the cell
/// holds then, committed or not. The commits after `durable` may be ordered only, as a workload
/// before this one may have ended on such commits.
pub async fn workload(
    rng: &mut StdRng,
    cell: &LocalCell<MemDir>,
    epochs: &mut Vec<Model>,
    model: Model,
    durable: usize,
) -> (usize, Model) {
    workload_uploading(rng, cell, epochs, (model, durable), None).await
}

/// cladefs: what a workload waits for at a step of its: a burst at step `.0`, whose commit spans
/// segments, then `.1` of that commit's epoch, which waits for it uploaded and acknowledged.
pub type Uploading<'a> =
    (usize, &'a dyn Fn(u64) -> futures::future::BoxFuture<'static, anyhow::Result<()>>);

/// cladefs: runs a random workload as [`workload`] does, but at the step `uploading` names, if the
/// cell has not failed by then, a burst, then waiting as it says: so that the kept rounds' commits
/// span segments and are acknowledged by the uploader in most rounds by what the round does, not
/// by how fast a workload ran against the uploader's interval of real time (board 1738, board
/// 1852): an optimized build's workloads ended before the uploader's first tick.
pub async fn workload_uploading(
    rng: &mut StdRng,
    cell: &LocalCell<MemDir>,
    epochs: &mut Vec<Model>,
    (model, durable): (Model, usize),
    uploading: Option<Uploading<'_>>,
) -> (usize, Model) {
    let next_sequence = cell.info().unwrap().next_sequence;
    let mut run = Run { cell, epochs, model, durable, open: Vec::new(), next_sequence };
    for step in 0..STEPS {
        if let Some((_, wait)) = uploading.filter(|(at, _)| *at == step) {
            let burst = run.burst(rng).await;
            let epoch = burst.and_then(|()| Ok(cell.last_commit()?));
            let waited = match epoch {
                Ok(epoch) => wait(epoch).await,
                Err(err) => Err(err),
            };
            if waited.is_err() {
                break;
            }
        }
        let result = match rng.random_range(0..15) {
            // A new extent, small or large.
            0..=3 => run.write_new(random_data(rng, 8)).await,
            4..=5 if !run.open.is_empty() => run.write_more(rng).await,
            6..=7 if !run.model.extents.is_empty() => run.free(rng),
            // Cleaning, which commits durably, or a commit, durable or only ordered, setting the
            // root or not.
            8..=11 => run.commit(rng).await,
            12 => run.flush().await,
            // cladefs: the open segment closed between commits, as an uploader has it closed: its
            // footer names commits only ordered, maybe (TASK-92.16).
            13 => cell.close_now().await.map(drop),
            // cladefs: a burst, a quarter of the time: each writes more than two segments.
            14 if rng.random_ratio(1, 4) => run.burst(rng).await,
            _ => Ok(()),
        };
        if result.is_err() {
            break;
        }
    }
    (run.durable, run.model)
}

/// What `cell` holds.
fn contents(cell: &LocalCell<MemDir>) -> Model {
    let mut extents = BTreeMap::new();
    for (extent, len) in cell.extents().unwrap() {
        let mut data = vec![0; len as usize];
        cell.read(extent, 0, &mut data).unwrap();
        extents.insert(extent, data);
    }
    Model { extents, root: cell.root().unwrap() }
}

/// Round `round`'s first crash: a cell made in `dir`, a random workload run on it until its file
/// system fails, from a random version on, as the host crashes, and the directory that crash
/// leaves. Returns that, what each commit began with, the last made durable, and the round's RNG,
/// to carry on.
async fn crashed_round(round: u64, dir: MemDir) -> (MemDir, Vec<Model>, usize, StdRng) {
    let mut rng = StdRng::seed_from_u64(round);
    let description = Description { block_size: BLOCK as u32, capacity: 4 << 20, cell: CELL };
    let cell = LocalCell::create(dir.clone(), description, OPTIONS).unwrap();
    let mut epochs = vec![Model::default()];
    dir.fail_after(Some(rng.random_range(0..600)));
    let (durable, _) = workload(&mut rng, &cell, &mut epochs, Model::default(), 0).await;
    drop(cell);
    (dir.crash(round), epochs, durable, rng)
}

/// What a directory holds: each file's path, with its length and a hash of its contents; and each
/// directory's, with neither. cladefs: of a framed file's data, opened under the cell's keys
/// (`UNKEPT_DEK`'s); of no other file's, its length alone: every raw file, log and framed object is
/// encrypted under salts drawn at random (TASK-411.28), so their bytes differ from run to run, but
/// not their lengths, nor what they hold.
type Digest = BTreeMap<String, Option<(u64, u64)>>;

/// What `dir` holds.
fn digest(dir: &MemDir) -> Digest {
    let mut digest = BTreeMap::new();
    let mut paths = vec![String::new()];
    while let Some(path) = paths.pop() {
        // A directory lists, a file opens.
        let held = match dir.list(&path) {
            Ok(names) => {
                let child = |name| if path.is_empty() { name } else { format!("{path}/{name}") };
                paths.extend(names.into_iter().map(child));
                None
            }
            Err(_) => {
                let file = dir.open(&path, false).unwrap();
                let mut data = vec![0; file.size().unwrap() as usize];
                file.read_at(&mut data, 0).unwrap();
                let keys = bucket_cell::testing::ring([CELL]);
                let opened = match path.starts_with("segments/") || path.starts_with("extents/") {
                    true => extent_frame::data(&data, &keys.of(CELL.id)).ok(),
                    false => None,
                };
                Some((data.len() as u64, opened.map_or(0, |data| rapidhash::rapidhash(&data))))
            }
        };
        digest.insert(path, held);
    }
    digest
}

/// The directories a round left, each named, in order: those its crashes left, and the one it
/// ended with.
pub type Left = Vec<(&'static str, Digest)>;

/// What `run` returned, run twice at once, two futures of the test's task: a round's two runs, in
/// the time of one, whose directories [`same`] compares.
pub async fn twice<T>(run: impl AsyncFn() -> T) -> (T, T) {
    futures::join!(run(), run())
}

/// Checks that round `round`, run twice, left the same directories, `first` and `second`, of which
/// each run leaves as many: a round that stops early panics rather than return.
pub fn same(round: u64, first: &Left, second: &Left) {
    for ((name, first), (_, second)) in first.iter().zip(second) {
        let paths: BTreeSet<_> = first.keys().chain(second.keys()).collect();
        let differing: Vec<_> =
            paths.into_iter().filter(|path| first.get(*path) != second.get(*path)).collect();
        assert_eq!(differing, Vec::<&String>::new(), "round {round}: {name} differs");
    }
}

/// Round `round` of the recovering rounds, its cell in `dir`: it crashes in a random workload,
/// recovers a prefix of its commits, carries on, and crashes again after a flush, keeping what it
/// committed. Returns the directories it left, and whether its first crash was in a commit.
pub async fn recovering(round: u64, dir: MemDir) -> (Left, bool) {
    let (crashed, epochs, durable, mut rng) = crashed_round(round, dir).await;
    let mut left = vec![("its first crash", digest(&crashed))];
    let mid_commit = durable + 1 < epochs.len();

    let cell = LocalCell::open(crashed.clone(), OPTIONS)
        .unwrap_or_else(|e| panic!("round {round}: the cell doesn't open: {e:?}"));
    let epoch = cell.last_commit().unwrap() as usize;
    assert!(
        (durable..epochs.len()).contains(&epoch),
        "round {round}: recovered epoch {epoch}, of {durable}..{}",
        epochs.len()
    );
    assert_eq!(contents(&cell), epochs[epoch], "round {round}: epoch {epoch} differs");
    // What it recovered is sealed, and minting resumes past it.
    let info = cell.info().unwrap();
    for (extent, _) in cell.extents().unwrap() {
        assert!(extent.sequence < info.next_sequence, "round {round}");
        let error = cell.write(extent, 0, &[0; BLOCK]).await.expect_err("rewrote a sealed extent");
        assert_eq!(error.downcast_ref::<SealedExtent>(), Some(&SealedExtent(extent)));
    }

    // It carries on, and crashing again after a flush keeps what it committed. Its write-ahead
    // log's footers, the open segment closed by a last commit of what it wrote since, say what
    // it holds, and spent: the frees of extents freed while open among them (TASK-160.15), which
    // the cell's audit replays at each closing (TASK-165.9).
    let recovered = epochs[epoch].clone();
    let mut epochs = vec![recovered.clone()];
    let (_, model) = workload(&mut rng, &cell, &mut epochs, recovered, 0).await;
    cell.flush().await.unwrap_or_else(|e| panic!("round {round}: a commit failed: {e:?}"));
    epochs.push(model);
    rebuilds(round, &cell).await;
    let durable = epochs.len() - 1;
    drop(cell);
    let crashed = crashed.crash(round + ROUNDS);
    left.push(("its second crash", digest(&crashed)));
    let cell = LocalCell::open(crashed.clone(), OPTIONS).unwrap();
    assert_eq!(contents(&cell), epochs[durable], "round {round}: carrying on");
    // And closing its open segment, its footers say what it holds.
    rebuilds(round, &cell).await;
    drop(cell);
    left.push(("its last directory", digest(&crashed)));
    (left, mid_commit)
}

/// Checks that `cell`'s footers, its open segment closed by a commit, say what it holds as of that
/// commit: its extents, its root, and what it spent, which its audit asserts as it closes it
/// (TASK-165.9's AC #5).
async fn rebuilds(round: u64, cell: &LocalCell<MemDir>) {
    cell.close_segment().unwrap();
    cell.commit(None).await.unwrap_or_else(|e| panic!("round {round}: a commit failed: {e:?}"));
}
