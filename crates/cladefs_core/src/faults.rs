// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the failpoints of a fork's protocol (TASK-44.10), here and in `local_cell` and
//! `bucket_cell`: a step failing, as what its function returns, partway through a fork or the
//! cleanup of one that failed. The `fail` crate's: nothing without the `failpoints` feature, which
//! this crate's tests have. Tests set them one by one, or as a seed says ([`plan`]); a daemon built
//! with them, as FAILPOINTS or CLADEFS_FAULT_SEED say ([`from_seed`]).

use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};

/// A failpoint's failure, as the function its step is in gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("failpoint {0}")]
pub struct Failpoint(pub &'static str);

/// A step of a protocol, which the failpoint of its name fails: the function it is in returns
/// [`Failpoint`] there, or whatever a test set the failpoint to do happens.
#[macro_export]
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err($crate::faults::Failpoint($name).into()))
    };
}
pub use crate::failpoint;

/// A crash at a step: the process aborting there, its disk as the step left it, as FAILPOINTS's
/// `NAME=return` asks of a daemon built with the failpoints feature (board 5728). A panic would
/// end only the request it is in.
#[macro_export]
macro_rules! crashpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| $crate::faults::crash($name))
    };
}
pub use crate::crashpoint;

/// Aborts the process at the crashpoint `name` ([`crashpoint`]).
pub fn crash(name: &str) -> ! {
    eprintln!("cladefs: crashpoint {name}: aborting");
    std::process::abort()
}

/// The steps of a fork of a cell with a writer, in the order it takes them, those that fail: a
/// version of its last commit made, then, on the uploader's side, its segment's upload and
/// acknowledgement, which names the version in the head; then the fork of the version, as a
/// restore's steps (TASK-508.3); then the version dropped, its child made, which fails the fork no
/// more (TASK-609).
pub const FORK_STEPS: [&str; 18] = [
    "version::committed",
    "upload::gather",
    "writer::upload",
    "writer::uploaded",
    "writer::acknowledge",
    "writer::acknowledged",
    "upload::acknowledged",
    "upload::released",
    "fork::uploaded",
    "upload::forked",
    "version::record",
    "version::recorded",
    "version::referred",
    "version::checked",
    "version::made",
    "fork::recorded",
    "fork::made",
    "upload::drop_versions",
];

/// The steps of a restore: a fork of a version, in the order it takes them, on any host holding a
/// copy of it (doc-7, Versions): its child's record, its reference under the version's origin's
/// cell (TASK-395.3, after the record since TASK-395.10), a read of the version's, and the record
/// made; then the child's cell. No upload, as it commits nothing; and no look for its child's ID
/// first, its record's create refusing one taken alike (TASK-449).
pub const RESTORE_STEPS: [&str; 8] = [
    "upload::forked",
    "version::record",
    "version::recorded",
    "version::referred",
    "version::checked",
    "version::made",
    "fork::recorded",
    "fork::made",
];

/// The steps of the daemon's around a fork: a guest's request, and a fork the control API asked
/// for once its cell's commits are uploaded; and the fork made, the answer to its guest lost.
pub const DAEMON_STEPS: [&str; 3] =
    ["cladefsd::fork", "cladefsd::fork_uploaded", "cladefsd::forked"];

/// The steps of a failed fork's cleanup, its child deleted in the bucket as the fork's alone: its
/// head ended, made so if it has none.
pub const CLEANUP_STEPS: [&str; 1] = ["lifecycle::head_ended"];

/// One in how many steps a seed fails.
const FAILING: u64 = 8;

/// The failpoints `seed` sets, with what `fail::cfg` sets them to: one step in [`FAILING`], of a
/// fork and its cleanup, failing once, at its first, second or third pass. The same seed sets the
/// same, which the `fail` crate's chances, drawn from no seed, do not.
pub fn plan(seed: u64) -> Vec<(&'static str, String)> {
    let mut rng = StdRng::seed_from_u64(seed);
    let steps = FORK_STEPS.into_iter().chain(DAEMON_STEPS).chain(CLEANUP_STEPS);
    let chosen = steps.filter(|_| rng.random_range(0..FAILING) == 0).collect::<Vec<_>>();
    let failing = |step| (step, format!("{}*off->1*return", rng.random_range(0..3)));
    chosen.into_iter().map(failing).collect()
}

/// Sets the failpoints the seed `CLADEFS_FAULT_SEED` names ([`plan`]), if it names one, saying
/// which: so a run that failed says how to fail again.
pub fn from_seed() -> Result<(), String> {
    let Ok(seed) = std::env::var("CLADEFS_FAULT_SEED") else { return Ok(()) };
    let seed = seed.parse().map_err(|err| format!("CLADEFS_FAULT_SEED={seed}: {err}"))?;
    for (step, actions) in plan(seed) {
        log::info!("cladefs: fault seed {seed}: {step}={actions}");
        fail::cfg(step, &actions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon's seed sets its plan's failpoints, as CLADEFS_FAULT_SEED names it.
    #[test]
    fn seeds_set_their_failpoints() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFS_FAULT_SEED", "7") };
        from_seed().unwrap();
        let mut set = fail::list();
        set.sort();
        let mut planned: Vec<_> =
            plan(7).into_iter().map(|(step, actions)| (step.to_owned(), actions)).collect();
        planned.sort();
        assert_eq!(set, planned);
    }

    /// A seed says the same failpoints each time, and each is a step, failing once.
    #[test]
    fn seeds_say_what_fails() {
        assert_eq!(plan(7), plan(7));
        let plans: Vec<_> = (0..64).map(plan).collect();
        assert_ne!(plans[0], plans[1]);
        for (step, actions) in plans.iter().flatten() {
            let known = [&FORK_STEPS[..], &DAEMON_STEPS, &CLEANUP_STEPS].concat();
            assert!(known.contains(step), "{step}");
            assert!(actions.ends_with("*off->1*return"), "{actions}");
        }
        assert!(plans.iter().map(Vec::len).sum::<usize>() > 64, "{plans:?}");
    }
}
