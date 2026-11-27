// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A deterministic simulation of cells in object storage (TASK-28.1): doc-7's protocol run by
//! hosts' cells, this crate's, and their uploaders (`upload`), writing, committing, forking, taken
//! over and crashing, and by deletions and collectors, over a bucket that delays, reorders and
//! fails requests (TASK-88: bucket_cell's until then, whose writers made their segments with
//! `SegmentBuilder`, so that the cell's choice of what a segment seals and frees never met a
//! takeover or a collector), checked after every step against what each cell's guests did and were
//! told, every object a cell's acknowledged segments name there, and what its live writer and its
//! cell name of its guest's read back (TASK-149.32, TASK-195); and, once everything stops, for
//! garbage left behind, for tasks still waiting, a checkpoint, a deletion or anything else nothing
//! will wake, and for uploaders that upload no more what they are asked to. A host's writers upload
//! within one budget, a few PUTs at once, so that uploads wait for each other.
//! `crates/bucket_cell/mutants.sh` puts back a bug of each kind, and sees a test fail.
//!
//! Each case doc-7 lists is a profile that makes it likely, run over many seeds; each rule of the
//! protocol has a test that turns it off and expects some seed to find the violation it prevents.
//!
//! cladefs: and replicated cells (doc-21, TASK-680), their members on the other hosts, over a
//! network that breaks, delays, duplicates and drops calls, whose disks crash corrupting what
//! they never synced: every commit told replicated must survive every take.
//!
//! cladefs: and fast WAL cells (doc-29, board 14433), their commits appended to their zones'
//! appendable buckets: every commit told durable must survive every take, its host's disk lost or
//! not; each rule of `specs/fast-wal` the code keeps has a test that turns it off.

mod world;

use bucket_cell::{Rules, set_rules};
use std::ops::Range;
use world::{Profile, Weights, World};

/// The violations a run of `seed` finds, and, if any, what the bucket held.
fn violations(seed: u64, profile: Profile) -> (Vec<String>, String) {
    let mut world = World::new(seed, profile);
    world.run();
    let description = match world.violations.is_empty() {
        true => String::new(),
        // cladefs: and the buggify sites the run asked about, on or off (doc-24).
        false => format!("{}\nbuggify: {:#?}", world.describe(), bucket_cell::buggify::sites()),
    };
    (world.violations, description)
}

/// `seeds`, as many again as `SIM_SOAK` says (1 by default) from their start.
fn soaked(seeds: Range<u64>) -> Range<u64> {
    let soak: u64 = std::env::var("SIM_SOAK").map_or(1, |soak| soak.parse().unwrap());
    seeds.start..seeds.start + (seeds.end - seeds.start) * soak
}

/// What a run of `seed` asked of the bucket, and the violations it found.
fn replay(seed: u64, profile: Profile) -> (Vec<String>, Vec<String>) {
    let mut world = World::new(seed, profile);
    world.run();
    (world.journal(), world.violations)
}

/// Runs each of `seeds` twice, expecting the same requests of the bucket, in the same order, each
/// giving the same, and the same violations: a seed replays as it ran, so that one that fails can
/// be run again to debug it (TASK-380). `SIM_SOAK` multiplies how many.
fn replays(profile: Profile, seeds: Range<u64>) {
    for seed in soaked(seeds) {
        let (first, second) = (replay(seed, profile), replay(seed, profile));
        let at = first.0.iter().zip(&second.0).position(|(first, second)| first != second);
        let at = at.unwrap_or(first.0.len().min(second.0.len()));
        assert!(
            first == second,
            "seed {seed} ran two ways: request {at} of {} and {}: {:?} and {:?}, after {:#?}",
            first.0.len(),
            second.0.len(),
            first.0.get(at),
            second.0.get(at),
            &first.0[at.saturating_sub(8)..at],
        );
    }
}

/// Runs `seeds`, failing at the first that finds a violation. `SIM_SOAK` multiplies how many.
fn holds(profile: Profile, seeds: Range<u64>) {
    holds_with(profile, seeds, &[]);
}

/// Runs `seeds` as [`holds`] does, and `found` first, seeds past them that found a bug once, which
/// every run replays; then [`fresh`] seeds, which no run before ran.
fn holds_with(profile: Profile, seeds: Range<u64>, found: &[u64]) {
    for seed in found.iter().copied().chain(soaked(seeds)) {
        let (found, description) = violations(seed, profile);
        assert!(found.is_empty(), "seed {seed}: {found:#?}\n{description}");
    }
    for seed in fresh() {
        let (found, description) = violations(seed, profile);
        assert!(
            found.is_empty(),
            "fresh seed {seed}, found by this run alone: keep it with the test's seeds that found a \
             bug once, and replay it with SIM_SEED={seed}: {found:#?}\n{description}"
        );
    }
}

/// cladefs: seeds drawn anew by each run, past the fixed ones, so that each run of the simulation
/// reaches states none before it did, as FoundationDB's harness draws its seeds (doc-24,
/// TASK-685.12): a run of fixed seeds replays the same states every time. `SIM_FRESH` of them (16
/// by default, 0 for none), from `SIM_FRESH_BASE` if set, to run a run's again, or else from a base
/// drawn at random, said on standard output (which a failing test's output shows), at 2^40 or past
/// it, away from the fixed seeds.
fn fresh() -> Range<u64> {
    let count: u64 = std::env::var("SIM_FRESH").map_or(16, |count| count.parse().unwrap());
    let base = match std::env::var("SIM_FRESH_BASE") {
        Ok(base) => base.parse().unwrap(),
        Err(_) => (1 << 40) | (rand::random::<u64>() >> 2),
    };
    println!("fresh seeds: {count} from {base} (SIM_FRESH_BASE={base} runs them again)");
    base..base + count
}

/// Runs `seeds` with `rules`, expecting one of them to find a violation other than garbage left:
/// the rules keep cells whole; leaving garbage is a lesser failure. It stops at the first, so
/// a wide range costs time only when no seed finds one. Changing the simulation moves the seeds
/// that do; a test that stops finding one needs a profile likelier to make its case.
fn needed(rules: Rules, profile: Profile, seeds: Range<u64>) {
    set_rules(rules);
    let found = seeds.clone().find_map(|seed| {
        let (found, _) = violations(seed, profile);
        let unsafe_ = found.iter().any(|violation| !violation.starts_with("garbage left"));
        unsafe_.then_some((seed, found))
    });
    set_rules(Rules::ALL);
    let (seed, found) = found.unwrap_or_else(|| panic!("no seed of {seeds:?} needed the rule"));
    println!("seed {seed}: {found:#?}");
}

/// A test that 200 seeds of `$profile` find no violation, and the seeds past them `$found`, each of
/// which found a bug once.
macro_rules! test_holds {
    ($name:ident, $profile:expr) => {
        test_holds!($name, $profile, []);
    };
    ($name:ident, $profile:expr, [$($found:expr),*]) => {
        #[test]
        fn $name() {
            holds_with($profile, 0..200, &[$($found),*]);
        }
    };
}

/// A test that `$rule` is needed: that one of 20,000 seeds of `$profile` without it finds a
/// violation.
macro_rules! test_needs {
    ($name:ident, $rule:ident, $profile:expr) => {
        test_needs!($name, $rule, $profile, 0..20_000);
    };
    ($name:ident, $rule:ident, $profile:expr, $seeds:expr) => {
        #[test]
        fn $name() {
            needed(Rules { $rule: false, ..Rules::ALL }, $profile, $seeds);
        }
    };
}

/// cladefs: a test that `$rule` keeps the bucket from holding for ever what no pass collects: that
/// one of 20,000 seeds of `$profile` without it leaves a reference no record names
/// ([`leaked`], TASK-395.10), a lesser failure than those [`test_needs`] finds.
macro_rules! test_leaks {
    ($name:ident, $rule:ident, $profile:expr) => {
        test_leaks!($name, $rule, $profile, 0..20_000);
    };
    ($name:ident, $rule:ident, $profile:expr, $seeds:expr) => {
        #[test]
        fn $name() {
            leaked(Rules { $rule: false, ..Rules::ALL }, $profile, $seeds);
        }
    };
}

/// Runs `seeds` with `rules`, expecting one of them to leave a reference behind, its child's
/// record naming another origin or none, garbage no pass retires (TASK-395.10).
fn leaked(rules: Rules, profile: Profile, seeds: Range<u64>) {
    set_rules(rules);
    let found = seeds.clone().find_map(|seed| {
        let (found, _) = violations(seed, profile);
        let leaked = found
            .iter()
            .any(|violation| violation.starts_with("garbage left") && violation.contains("/refs/"));
        leaked.then_some((seed, found))
    });
    set_rules(Rules::ALL);
    let (seed, found) = found.unwrap_or_else(|| panic!("no seed of {seeds:?} leaked a reference"));
    println!("seed {seed}: {found:#?}");
}

fn weights(change: impl FnOnce(&mut Weights)) -> Profile {
    let mut profile = Profile::default();
    change(&mut profile.weights);
    profile
}

/// `SIM_SEEDS` seeds (1 by default) from `SIM_SEED` of one profile, as `SIM_PROFILE` says, with
/// the rules `SIM_OFF` names (separated by commas) off, to debug them; with `SIM_JOURNAL` set, each
/// seed's requests of the bucket printed first, in order, as they took effect.
#[test]
#[ignore = "debugs one seed (SIM_SEED, SIM_PROFILE, SIM_OFF): the sim part runs the soaks"]
fn one() {
    let seed = std::env::var("SIM_SEED").map_or(0, |seed| seed.parse().unwrap());
    let mut rules = Rules::ALL;
    for name in
        std::env::var("SIM_OFF").unwrap_or_default().split(',').filter(|name| !name.is_empty())
    {
        match name {
            "head_swap" => rules.head_swap = false,
            "epoch_keys" => rules.epoch_keys = false,
            "collector_epochs" => rules.collector_epochs = false,
            "epoch_sequences" => rules.epoch_sequences = false,
            "candidates_first" => rules.candidates_first = false,
            "tokens" => rules.tokens = false,
            "delete_tokens" => rules.delete_tokens = false,
            "forks_check_versions" => rules.forks_check_versions = false,
            "forks_pending" => rules.forks_pending = false,
            "collector_abandons" => rules.collector_abandons = false,
            "wal_follows" => rules.wal_follows = false,
            "references" => rules.references = false,
            "clones_refer_first" => rules.clones_refer_first = false,
            "clones_record_first" => rules.clones_record_first = false,
            "retirements_marked" => rules.retirements_marked = false,
            "references_held" => rules.references_held = false,
            "references_relisted" => rules.references_relisted = false,
            "origins_kept" => rules.origins_kept = false,
            "versions_kept" => rules.versions_kept = false,
            "versions_carried" => rules.versions_carried = false,
            "versions_acked" => rules.versions_acked = false,
            "installs_own" => rules.installs_own = false,
            "protect_wal_epoch" => rules.protect_wal_epoch = false,
            "wal_fences" => rules.wal_fences = false,
            "wal_offset_checked" => rules.wal_offset_checked = false,
            "wal_rolls_before_next" => rules.wal_rolls_before_next = false,
            "wal_fence_follows_roll" => rules.wal_fence_follows_roll = false,
            "wal_rolls_before_limit" => rules.wal_rolls_before_limit = false,
            "wal_kept_on_take" => rules.wal_kept_on_take = false,
            "wal_activates_after_flush" => rules.wal_activates_after_flush = false,
            "wal_after_large" => rules.wal_after_large = false,
            "wal_reclaims_after_ack" => rules.wal_reclaims_after_ack = false,
            "wal_sweep_tombstones" => rules.wal_sweep_tombstones = false,
            "wal_rolled_tombstones" => rules.wal_rolled_tombstones = false,
            "wal_contiguous" => rules.wal_contiguous = false,
            other => panic!("no rule {other}"),
        }
    }
    set_rules(rules);
    let profile = match std::env::var("SIM_PROFILE").as_deref().unwrap_or("everything") {
        "everything" => Profile::default(),
        "forks" => forks(),
        "bases" => deleted_bases(),
        "stale" => stale_placements(),
        "collected" => collected(),
        "takeovers" => takeovers(),
        "recreated" => recreated(),
        "crashes" => crashes(),
        "collectors" => collectors(),
        "taken" => taken(),
        "children" => children(),
        "fresh_cells" => fresh_cells(),
        "recovering" => recovering(),
        "twins" => twin_restores(),
        "restores" => restores(),
        "forgotten" => forgotten_restores(),
        "whole_restores" => whole_restores(),
        "wals" => wals(),
        "versioned_wals" => versioned_wals(),
        "late_checkpoints" => late_checkpoints(),
        "versions" => versions(),
        "regions" => regions(),
        "mirrors" => mirrors(),
        "replicated" => replicated(),
        "retakes" => retakes(),
        "relocations" => relocations(),
        "fast_wals" => fast_wals(),
        "fast_wal_takeovers" => fast_wal_takeovers(),
        "fast_wal_zombies" => fast_wal_zombies(),
        other => panic!("no profile {other}"),
    };
    let count = std::env::var("SIM_SEEDS").map_or(1, |count| count.parse().unwrap());
    if std::env::var_os("SIM_JOURNAL").is_some() {
        for seed in seed..seed + count {
            for (at, asked) in replay(seed, profile).0.iter().enumerate() {
                println!("seed {seed} request {at}: {asked}");
            }
        }
    }
    holds(profile, seed..seed + count);
}

/// Seeds of every profile replay as they ran.
#[test]
fn seeds_replay() {
    let profiles = [
        Profile::default(),
        deleted_bases(),
        restores(),
        versioned_wals(),
        late_checkpoints(),
        recreated(),
        twin_restores(),
        regions(),
        mirrors(),
        replicated(),
        fast_wals(),
    ];
    for profile in profiles {
        replays(profile, 0..20);
    }
}

/// cladefs: every buggify site the code has is asked about by the simulation (doc-24): a site no
/// run reaches makes no run do its unusual thing, as FoundationDB reports a probe no run hits.
#[test]
fn buggify_sites_are_reached() {
    let sites = ["segments.rs", "upload.rs", "writer.rs"];
    let mut asked = std::collections::BTreeSet::<&str>::new();
    for seed in 0..20 {
        let mut world = World::new(seed, Profile::default());
        world.run();
        for site in bucket_cell::buggify::sites() {
            asked.extend(sites.iter().copied().filter(|file| site.contains(file)));
        }
    }
    bucket_cell::buggify::disable();
    assert_eq!(asked.len(), sites.len(), "reached only {asked:?} of {sites:?}");
}

/// cladefs: FoundationDB's swarm of faults (doc-24): a profile the seed draws, so that each seed
/// runs its own mix of what happens, as each of FoundationDB's runs adds fault workloads at random
/// with their parameters. One of the named profiles as its base; each of its families of steps
/// kept, dropped, halved or doubled, but those that drive the world (requests completing, calls
/// delivered); its rates of failures, holds, restarts, forgetting, passes over one cell and
/// corrupting crashes drawn again. Replication and regions only as the profiles made for them
/// have them, whose worlds hold what they need.
fn swarm(seed: u64) -> Profile {
    use rand::{RngExt as _, SeedableRng as _};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed ^ 0x5a5a_f00d);
    let bases: [fn() -> Profile; 26] = [
        Profile::default,
        forks,
        deleted_bases,
        regions,
        stale_placements,
        collected,
        children,
        fresh_cells,
        takeovers,
        recreated,
        late_checkpoints,
        taken,
        crashes,
        wals,
        versioned_wals,
        spanning_wals,
        restores,
        forgotten_restores,
        twin_restores,
        whole_restores,
        collectors,
        recovering,
        versions,
        replicated,
        retakes,
        fast_wals,
    ];
    let mut profile = bases[rng.random_range(0..bases.len())]();
    profile.weights.scale(|name, weight| match name {
        "complete" | "deliver" => weight,
        _ => match rng.random_range(0..6) {
            0 => 0,
            1 => weight / 2,
            2 => weight.saturating_mul(2),
            _ => weight,
        },
    });
    let mut either = |kept: f64, drawn: f64| match rng.random_range(0..3) {
        0 => kept,
        1 => 0.0,
        _ => drawn,
    };
    profile.fail_before = either(profile.fail_before, 0.15);
    profile.fail_after = either(profile.fail_after, 0.15);
    profile.hold = either(profile.hold, 0.5);
    profile.forget = either(profile.forget, 0.5);
    if profile.replicated > 0.0 {
        profile.net_faults = either(profile.net_faults, 0.2);
    }
    profile.one_cell = rng.random_range(0.0..=1.0);
    profile.restarts = rng.random_bool(0.5);
    profile.corrupting = rng.random_bool(0.5);
    profile
}

/// cladefs: seeds of [`swarm`] profiles, each its own, find no violation; and replay as they ran.
#[test]
fn swarms() {
    for seed in soaked(0..200).chain(fresh()) {
        let profile = swarm(seed);
        let (found, description) = violations(seed, profile);
        assert!(found.is_empty(), "seed {seed} of {profile:#?}: {found:#?}\n{description}");
    }
    for seed in 0..10 {
        replays(swarm(seed), seed..seed + 1);
    }
}

/// Everything, at the default rates.
#[test]
fn everything() {
    holds(Profile::default(), 0..300);
}

/// Case 1: frees before and after versions, by parents and the children restored from them. Case
/// 2: chains of generations, some deleted in the middle.
fn forks() -> Profile {
    weights(|weights| {
        weights.version = 4;
        weights.restore_version = 4;
        weights.open = 6;
        weights.delete = 2;
    })
}

test_holds!(forks_and_frees, forks());

/// cladefs: doc-27 (board 6214): writers relocate out of their cells' sparse segments as guests
/// free and write, versions are cut, restored and dropped, checkpoints land, and cells are taken
/// over and crash: what recoveries need stays (`NothingNeededGone`), and every commit reads as
/// committed.
fn relocations() -> Profile {
    weights(|weights| {
        weights.relocate = 4;
        weights.version = 2;
        weights.drop_version = 1;
        weights.restore_version = 2;
        weights.checkpoint = 4;
        weights.collect = 3;
        weights.open = 4;
    })
}

/// cladefs: [`relocations`] holds, its seeds relocating: not a property of rounds that never
/// copied anything.
#[test]
fn relocations_keep_what_recoveries_need() {
    let before = crate::written::written().relocated;
    holds(relocations(), 0..200);
    assert!(crate::written::written().relocated > before, "no seed relocated anything");
}

/// Bases deleted while the cells restored from their versions live (TASK-39.4): those write, cut
/// versions, are restored and taken over, and collectors run, keeping what they need of the bases.
fn deleted_bases() -> Profile {
    weights(|weights| {
        weights.version = 3;
        weights.restore_version = 4;
        weights.open = 6;
        weights.delete_base = 3;
        weights.collect = 6;
        weights.checkpoint = 4;
        weights.takeover = 2;
    })
}

test_holds!(bases_deleted_under_live_forks, deleted_bases(), [6496]);

/// cladefs: cells across the buckets of three cloud regions (TASK-633.10): each cell in the bucket
/// of the host that made it, versions restored by hosts of other regions into their own buckets,
/// bases deleted and collected while their children elsewhere live, a region's bucket down for
/// stretches.
fn regions() -> Profile {
    Profile { regions: 3, ..deleted_bases() }
}

test_holds!(cells_across_regions, regions(), [2325]);

/// cladefs: versions mirrored across the buckets of three regions (TASK-633.10, TASK-633.8):
/// restores as [`restores`] makes them, by hosts of three regions, two each, so that most land in
/// another bucket than their versions' cells', whose hosts begin their versions' mirrors there, as
/// their children's openings do, a host's mirrors of a version taking turns, two hosts' racing;
/// hosts crashing midway, their pins left to go stale; versions dropped and bases deleted under
/// them; collectors of each region passing over copies; and at its end every cell deleted, every
/// copy collected with them.
fn mirrors() -> Profile {
    let mut profile = Profile { regions: 3, hosts: 6, steps: 1000, teardown: true, ..restores() };
    profile.weights.complete = 200;
    profile.weights.version = 8;
    profile.weights.restore_version = 4;
    profile.weights.drop_version = 2;
    profile.weights.crash = 1;
    profile.weights.write = 2;
    profile.weights.commit = 4;
    profile.weights.checkpoint = 1;
    profile.weights.collect = 3;
    profile.weights.open = 3;
    profile
}

test_holds!(versions_mirrored_across_regions, mirrors());

/// cladefs: the mirrors' runs reach mirrors (TASK-633.10): versions forked into other regions'
/// buckets mirrored there, some by a second host as one is under way, others dropped first, their
/// mirrors finding them unnamed, and copies collected once nothing needs them, as a site no run
/// reaches makes no run test it.
#[test]
fn mirrors_are_reached_across_regions() {
    let mut reached = std::collections::BTreeMap::<&str, usize>::new();
    for seed in 0..20 {
        let mut world = World::new(seed, mirrors());
        world.run();
        for (outcome, count) in world.mirrored {
            *reached.entry(outcome).or_default() += count;
        }
    }
    println!("mirrors of 20 seeds: {reached:?}");
    for outcome in ["done", "unnamed", "under way", "collected"] {
        assert!(reached.get(outcome).is_some_and(|&n| n > 0), "no mirror {outcome}: {reached:?}");
    }
}
// A child in another region's bucket than its parent's keeps what it reads of the parent by its
// reference, in the parent's bucket, and the parent's collector keeps its origins.
test_needs!(references_hold_across_regions, references, regions());
test_needs!(origins_are_kept_across_regions, origins_kept, regions());

/// Placements cut off taking cells back, and deleting them, with their earlier fencing tokens,
/// from the ones after them, while those write and are taken over in turn (TASK-39.7, TASK-45.1).
fn stale_placements() -> Profile {
    weights(|weights| {
        weights.takeover = 4;
        weights.open = 6;
        weights.reclaim = 4;
        weights.stale_delete = 2;
        weights.delete = 1;
        weights.close = 4;
        weights.crash = 2;
    })
}

test_holds!(stale_placements_take_nothing_back, stale_placements());

test_needs!(fencing_needs_tokens, tokens, stale_placements());

test_needs!(deletes_need_tokens, delete_tokens, stale_placements());

/// Case 3: restores racing collection.
fn collected() -> Profile {
    weights(|weights| {
        weights.version = 2;
        weights.restore_version = 3;
        weights.open = 6;
        weights.collect = 8;
        weights.checkpoint = 6;
        weights.tick = 6;
    })
}

test_holds!(forks_racing_collection, collected());

/// Restores whose children open soon after, while collectors run: a child's record and head come
/// while a pass reads cells.
fn children() -> Profile {
    weights(|weights| {
        weights.version = 3;
        weights.restore_version = 6;
        weights.open = 10;
        weights.collect = 10;
        weights.crash = 2;
    })
}

/// cladefs: restores whose children open at once, passes over one cell alone, the child's
/// among them, which may have no record yet as the pass starts, and requests slow to complete, so
/// that a pass spans the child's opening and first upload (TASK-395.4).
fn fresh_cells() -> Profile {
    let mut profile = weights(|weights| {
        weights.complete = 15;
        weights.version = 4;
        weights.restore_version = 8;
        weights.open = 12;
        weights.collect = 12;
        weights.crash = 0;
        weights.delete = 0;
        weights.stop_collector = 0;
    });
    profile.one_cell = 1.0;
    profile
}

test_holds!(forks_with_children_opening, children());

// cladefs: a pass over a child's cell begun before the child's head (TASK-395.4): every restore is
// to a cell of its own here. A cell's one head says whether it lives (TASK-515), so a pass reading
// none finds no writer's objects to lose; what the rule `candidates_first` keeps, a child's head
// made after the pass read none, the simulation finds no seed for, nor does a cell's own writer
// lose anything without it, the head's epoch keeping what it may yet name: specs/bucket's
// NoCandidatesFirst shows it needed.
test_holds!(passes_over_cells_being_made, fresh_cells());

/// Case 4: takeovers while the writer before is still uploading. Case 9: a stale writer
/// recreating what the collector deleted.
fn takeovers() -> Profile {
    let mut profile = weights(|weights| {
        weights.takeover = 6;
        weights.open = 6;
        weights.close = 10;
        weights.collect = 4;
        weights.checkpoint = 4;
    });
    profile.fail_after = 0.15;
    profile
}

test_holds!(takeovers_while_uploading, takeovers());

// A writer acknowledging what it uploaded by swapping the head, with the condition that it is its
// own taking still (TASK-407): written whatever is there, a writer taken over writes it over its
// successor's, whose acknowledged commits the cell then holds not.
test_needs!(takeovers_need_head_swaps, head_swap, takeovers());

// Keys naming their writers' epochs (TASK-407): two writers' segments of one number at one key.
test_needs!(takeovers_need_epoch_keys, epoch_keys, takeovers());

/// Case 9: a stale writer's create of a segment in flight while a later writer's takes its place,
/// which the collector deletes, landing then: it recreates the segment, holding other extents than
/// readers whose indexes are older look for there (TASK-79.18).
/// Takeovers hold the creates of the writers they take cells from ([`Profile::hold`]), and
/// requests complete often, so that passes of the collector finish while a cell's writer lives.
fn recreated() -> Profile {
    let mut profile = weights(|weights| {
        weights.complete = 200;
        weights.takeover = 6;
        weights.open = 6;
        weights.close = 10;
        weights.checkpoint = 6;
        weights.collect = 8;
        weights.tick = 6;
    });
    profile.hold = 1.0;
    // cladefs: sweeps alone (TASK-395.4): the searches of this profile's rules find their cases
    // with passes over every cell, none in 20,000 seeds with half the passes over one cell.
    profile.one_cell = 0.0;
    profile.ticks = 1;
    profile
}

// Seed 453: a writer taken over creating where a void segment was, which a pass deleted below its
// successor's fence (TASK-383), before keys were written once (TASK-407).
test_holds!(stale_creates_landing_past_deletions, recreated(), [453]);

// The collector keeping what a live cell's head may yet name (TASK-407): of its epoch, segments
// and extents of their own at or past its end, checkpoints newer than its own; deleted, a writer's
// swap names what is gone.
test_needs!(collection_needs_epochs, collector_epochs, recreated());

/// Writers checkpointing often, taken over with their checkpoints in flight, which land late, past
/// where the next writer recovered from, while collectors delete what they cover (TASK-175).
fn late_checkpoints() -> Profile {
    let mut profile = weights(|weights| {
        weights.complete = 80;
        weights.write = 2;
        weights.commit = 6;
        weights.close = 6;
        weights.checkpoint = 12;
        weights.takeover = 12;
        weights.open = 2;
        weights.crash = 0;
        weights.delete = 0;
        weights.version = 0;
        weights.restore_version = 0;
        weights.collect = 12;
        weights.stale_checkpoint = 6;
        weights.tick = 8;
    });
    profile.ticks = 1;
    profile
}

test_holds!(late_checkpoints_landing_past_takeovers, late_checkpoints());

/// Takeovers while collectors run: an extent of the writer taken over, uploaded, its segment not
/// yet.
fn taken() -> Profile {
    let mut profile = weights(|weights| {
        weights.takeover = 8;
        weights.open = 8;
        weights.close = 4;
        weights.collect = 10;
        weights.tick = 4;
    });
    profile.fail_after = 0.1;
    profile
}

test_holds!(takeovers_racing_collection, taken());

// Case 10: extents minted after a takeover.
test_needs!(takeovers_need_epoch_sequences, epoch_sequences, takeovers());

/// Case 5: crashes between a segment's upload and the next, and amid restores. Case 6: frees in
/// commits never made durable.
fn crashes() -> Profile {
    let mut profile = weights(|weights| {
        weights.crash = 4;
        weights.version = 2;
        weights.restore_version = 3;
        weights.unfork = 2;
        weights.open = 8;
    });
    profile.fail_before = 0.1;
    profile
}

test_holds!(crashes_anywhere, crashes());

/// Hosts crashing, keeping their write-ahead logs or losing them, and taking their cells back,
/// at once as they start again or later, while other hosts take them over and write, and the
/// segments of writers gone land late (doc-14): every open on a host with a log puts it after the
/// cell's end ([`crate::Wal`]). Nothing else: the PUTs of writers gone land among few
/// requests, between a recovery and its writer's first swap of the head.
fn wals() -> Profile {
    let mut profile = weights(|weights| {
        weights.crash = 4;
        weights.open = 8;
        weights.takeover = 3;
        weights.close = 6;
        weights.version = 0;
        weights.restore_version = 0;
        weights.delete = 0;
        weights.checkpoint = 0;
        weights.collect = 0;
        weights.stop_collector = 0;
    });
    profile.restarts = true;
    profile
}

/// Logs as [`wals`] makes them, holding versions, which their next writers name again where they
/// are put, dropped and restored, while checkpoints come and collectors run.
fn versioned_wals() -> Profile {
    let mut profile = wals();
    profile.weights.version = 3;
    profile.weights.drop_version = 1;
    profile.weights.restore_version = 2;
    profile.weights.delete = 1;
    profile.weights.checkpoint = 2;
    profile.weights.collect = 4;
    profile.weights.stop_collector = 1;
    profile
}

test_holds!(logs_put_after_takeovers, wals());

test_holds!(logs_of_versions_put_after_takeovers, versioned_wals());

// A host's log put after another host's acknowledged commit, which it loses.
test_needs!(logs_need_to_follow, wal_follows, wals());

/// cladefs: logs as [`wals`] makes them, their guests writing a segment's worth or more between
/// commits, most of them: commits spanning segments, the first of which may end the commit before,
/// landing alone as a host crashes or a writer takes the cell over (TASK-92.41, TASK-345).
fn spanning_wals() -> Profile {
    let mut profile = wals();
    profile.weights.write = 18;
    profile.weights.commit = 4;
    profile
}

test_holds!(spanning_logs_put_after_takeovers, spanning_wals());

/// Case 11: versions restored on several hosts at once (doc-20): forks of one racing each other,
/// passes of the collector, and the version's drop, whose hosts die at each step, what they leave
/// opened or deleted; while the cell it names goes on, frees what it held, and is deleted in
/// turn. Requests land late, after their hosts die.
fn restores() -> Profile {
    let mut profile = weights(|weights| {
        weights.version = 4;
        weights.drop_version = 1;
        weights.restore_version = 10;
        weights.open = 8;
        weights.delete = 3;
        weights.delete_base = 2;
        weights.unfork = 3;
        weights.collect = 8;
        weights.stop_collector = 1;
        weights.crash = 3;
        weights.checkpoint = 3;
        weights.tick = 4;
    });
    profile.fail_after = 0.1;
    profile.ticks = 1;
    profile
}

test_holds!(versions_restored_on_many_hosts_at_once, restores());

test_needs!(restores_need_versions_read_after_their_records, forks_check_versions, restores());

test_needs!(restores_need_records_saying_they_are_being_made, forks_pending, restores());

test_needs!(collection_needs_abandoning_restores, collector_abandons, restores());

/// cladefs: restores as [`restores`] makes them, half of those a crash stopped forgotten, as a host
/// deleting the directory it left unmade knows nothing of the child (TASK-395.10): what they wrote
/// is the collector's alone, a record being made given up on once its version goes, and the
/// reference written after it retired by it.
fn forgotten_restores() -> Profile {
    let mut profile = restores();
    profile.forget = 0.5;
    profile
}

test_holds!(restores_forgotten_leave_nothing_behind, forgotten_restores());

// Written first, a reference whose record never landed stays for ever.
test_leaks!(forgotten_restores_need_clones_record_first, clones_record_first, forgotten_restores());

// A slow host's reference, written after its retirement, stays for ever unmarked: no pass of its
// child's cell, skipping it, looks at it again (TASK-395.13, TASK-395.14).
test_leaks!(restores_need_retirements_marked, retirements_marked, restores());

/// Restores as [`restores`] makes them, and pairs of them to one child, as guests may name any ID:
/// whichever's record comes first takes it, the other refused (TASK-79.82, TASK-173).
fn twin_restores() -> Profile {
    let mut profile = restores();
    profile.weights.twins = 4;
    profile.weights.restore_version = 4;
    profile
}

test_holds!(restores_racing_for_one_child, twin_restores());

// cladefs: the per-cell collector's rules (TASK-395.3, TASK-395.4), every restore of the
// simulation's to a cell of its own. A base's cell's pass, the base deleted, keeps what its forks
// inherited only by their references...
test_needs!(collection_needs_references, references, deleted_bases());

// ...and its state at their origins whole, what recovering it reads too: a fork with no
// checkpoint of its own recovers through it...
test_needs!(collection_needs_origins_kept, origins_kept, deleted_bases());

// ...and a reference stays while its child's cell holds references of its own, a generation's
// forks inheriting through it: no seed of the simulation's shows it since a cell's one head
// (TASK-515); specs/bucket's NoReferencesHeld does.

// A clone's reference written before it reads its version has no search here: collector::tests'
// direct one shows it needed
// (`clones_keeping_what_they_inherit_as_their_versions_go_need_clones_refer_first`), as does
// specs/bucket's NoForksReferFirst; references listed after the states, NoReferencesRelisted alone.

/// Restores as [`restores`] makes them, but no host dying, no collector stopped and no request
/// failing once it took effect: a pass that misses a restored cell then reclaims what it inherited,
/// rather than stopping first. With `restores` itself, the trajectories TASK-175 moved put the
/// first seed missing it at 64216, past the search (it was 5310).
fn whole_restores() -> Profile {
    let mut profile = restores();
    profile.weights.crash = 0;
    profile.weights.stop_collector = 0;
    profile.fail_after = 0.0;
    profile
}

/// Case 7: collectors stopping midway, two at once, one fenced by another.
fn collectors() -> Profile {
    weights(|weights| {
        weights.collect = 10;
        weights.stop_collector = 4;
        weights.checkpoint = 4;
        weights.tick = 8;
    })
}

test_holds!(collectors_stopping_and_racing, collectors());

/// Writers taking cells over, recovering them, while checkpoints come and collectors delete
/// what they cover.
fn recovering() -> Profile {
    let mut profile = weights(|weights| {
        weights.commit = 20;
        weights.close = 15;
        weights.checkpoint = 3;
        weights.takeover = 8;
        weights.open = 8;
        weights.collect = 10;
        weights.tick = 6;
    });
    profile.steps = 800;
    profile.ticks = 1;
    profile
}

test_holds!(recoveries_racing_collection, recovering());

/// cladefs: versions (doc-18, TASK-476): cut by cells' writers, their cells quiescent,
/// acknowledged with their segments, carried by takeovers, dropped, uploaded or local, restored on
/// any host, while their cells go on, free what they held, checkpoint past them,
/// crash and are deleted, and collectors run.
fn versions() -> Profile {
    let mut profile = weights(|weights| {
        weights.version = 6;
        weights.drop_version = 2;
        weights.restore_version = 4;
        weights.open = 8;
        weights.takeover = 2;
        weights.crash = 2;
        weights.checkpoint = 4;
        weights.collect = 8;
        weights.stop_collector = 1;
        weights.delete = 1;
        weights.unfork = 2;
        weights.tick = 4;
    });
    profile.fail_after = 0.1;
    profile.ticks = 1;
    profile
}

test_holds!(versions_cut_dropped_and_restored, versions());

test_needs!(versions_need_their_states_kept, versions_kept, versions());

test_needs!(versions_need_takeovers_to_carry_them, versions_carried, versions());

test_needs!(versions_need_naming_with_their_segments, versions_acked, versions());

test_needs!(restores_of_versions_need_their_heads_read, forks_check_versions, versions());

/// cladefs: replicated cells (doc-21, TASK-680), of f 1 and 2 on five hosts, beside cells that
/// are not: their writers' commits sent to their members a turn at a time, over a network that
/// breaks between hosts, one way or both, and delays, duplicates and drops calls; hosts crashing,
/// keeping their disks, corrupting what they never synced, or losing them, their replica
/// directories with them, and taking their cells back at once; other hosts taking them over.
/// Every commit told replicated survives every take.
fn replicated() -> Profile {
    let mut profile = weights(|weights| {
        weights.deliver = 40;
        weights.late = 4;
        weights.partition = 3;
        weights.crash = 3;
        weights.open = 8;
        weights.takeover = 3;
        weights.close = 8;
        weights.version = 0;
        weights.restore_version = 0;
        weights.delete = 0;
        weights.collect = 1;
    });
    profile.hosts = 5;
    profile.replicated = 0.8;
    profile.net_faults = 0.1;
    profile.corrupting = true;
    profile.restarts = true;
    profile
}

test_holds!(replicated_cells_survive_takes_crashes_and_the_network, replicated());

/// cladefs: TASK-678's case, likelier: cells of f 1 on four hosts whose writers' hosts crash and
/// take them back at once, on their own disks, while the network between hosts breaks, so that a
/// take may hold, of its own log, commits told replicated that the members it fences lack.
fn retakes() -> Profile {
    let mut profile = replicated();
    profile.hosts = 4;
    profile.replicated = 1.0;
    profile.net_faults = 0.0;
    profile.corrupting = false;
    profile.weights.crash = 6;
    profile.weights.partition = 8;
    profile.weights.takeover = 4;
    profile.weights.late = 0;
    profile
}

test_holds!(replicated_cells_retaken_on_their_writers_disks, retakes());

test_needs!(retakes_need_to_install_their_own_logs, installs_own, retakes(), 0..2_000);

/// cladefs: fast WAL cells (doc-29, board 14433), most of a world's, beside plain ones, on four
/// hosts in two zones: their writers appending to their zones' fast buckets, which fail appends
/// before or after they land, hold an object to a few appends and go down at times, while the cold
/// bucket's uploads trail; hosts crashing, keeping their disks or losing them, and taking their
/// cells back at once; hosts of either zone taking them over, fencing and recovering the fast WAL
/// the head names; collectors running. Every commit told durable survives every take.
fn fast_wals() -> Profile {
    let mut profile = weights(|weights| {
        weights.crash = 4;
        weights.open = 8;
        weights.takeover = 4;
        weights.close = 4;
        weights.collect = 2;
        weights.version = 0;
        weights.restore_version = 0;
        weights.delete = 0;
    });
    profile.hosts = 4;
    profile.fast_wal = 0.8;
    profile.fail_after = 0.1;
    profile.restarts = true;
    profile
}

test_holds!(fast_wal_commits_survive_takes_crashes_and_lost_hosts, fast_wals());

/// cladefs: [`fast_wals`] with writers taken over while their appends are in flight, and their
/// hosts lost: what doc-29's fencing is for. Half the takeovers hold the creates of segments the
/// writer taken over has in flight, its fast WAL's among them, each until something deletes what
/// is at its key ([`Profile::hold`]): a zombie's create landing late.
fn fast_wal_takeovers() -> Profile {
    let mut profile = fast_wals();
    profile.weights.takeover = 10;
    profile.weights.crash = 6;
    profile.weights.close = 2;
    profile.fast_wal = 1.0;
    profile.hold = 0.5;
    profile.restarts = false;
    profile
}

test_holds!(fast_wal_takeovers_fence_writers_in_flight, fast_wal_takeovers());

/// cladefs: [`fast_wal_takeovers`] whose every takeover holds the creates of the writer taken
/// over, many of its appends landing with their answers lost: its create of a segment that landed,
/// tried again, waits until a sweep deletes the segment the take fenced, then lands again, and
/// the writer appends on, past the fence that was there.
fn fast_wal_zombies() -> Profile {
    let mut profile = fast_wal_takeovers();
    profile.hold = 1.0;
    profile.fail_after = 0.3;
    profile
}

test_holds!(fast_wal_zombies_land_on_tombstones, fast_wal_zombies());

// specs/fast-wal's rules, each turned off: some seed loses a commit told durable, finds a large
// extent missing, or a take that never ends.
test_needs!(fast_wal_takes_need_fences, wal_fences, fast_wal_takeovers(), 0..2_000);
test_needs!(fast_wal_appends_need_offsets, wal_offset_checked, fast_wal_takeovers(), 0..2_000);
test_needs!(
    fast_wal_rolls_need_their_markers,
    wal_rolls_before_next,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(
    fast_wal_takes_need_to_follow_rolls,
    wal_fence_follows_roll,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(fast_wal_segments_need_room_for_fences, wal_rolls_before_limit, fast_wals(), 0..2_000);
test_needs!(
    fast_wal_takes_need_the_old_wal_named,
    wal_kept_on_take,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(
    fast_wal_takes_need_to_flush_first,
    wal_activates_after_flush,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(
    fast_wal_batches_need_their_large_extents,
    wal_after_large,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(
    fast_wal_collectors_need_the_wal_epoch,
    protect_wal_epoch,
    fast_wal_takeovers(),
    0..2_000
);
test_needs!(fast_wal_reclaims_need_acknowledgements, wal_reclaims_after_ack, fast_wals(), 0..2_000);
// `wal_sweep_tombstones` (SweepDeletesFence) is `fast_wal::tests::sweeps_need_tombstones`'s: its case
// needs a retried create in flight across a whole take and its sweep, which no seed of 5,000 of
// fast_wal_zombies() reached.
test_needs!(fast_wal_reclaims_need_tombstones, wal_rolled_tombstones, fast_wals(), 0..2_000);
test_needs!(
    fast_wal_degraded_writers_need_contiguous_batches,
    wal_contiguous,
    fast_wals(),
    0..2_000
);
