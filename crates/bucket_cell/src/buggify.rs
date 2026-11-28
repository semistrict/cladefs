// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: FoundationDB's BUGGIFY (doc-24, TASK-685.12): code that may do something legal but
//! unusual, which the simulations have it do, to reach the states it leads to: a segment closed
//! early, an upload made before its interval, an upload of one part at a time. [`buggify!`] says
//! whether to, here.
//!
//! As FoundationDB has it, at two levels: a run of a simulation switches each site on or off once,
//! a quarter of them on (as `hash(seed, file, line)` says, so that a change elsewhere moves no
//! seed's sites), and an active site fires as often as its probability says, each time it is
//! asked, so that a run has a few sites misbehaving often rather than every one a little. A run
//! switches buggify on at all for four seeds in five ([`enable`]).
//!
//! Built normally, and in no simulation, it says no, a constant the compiler takes the site's
//! branch away with: production does what it always did.

/// Whether this site is to do its unusual thing now: in a simulation's run that switched buggify
/// on, if the run switched this site on, as often as `p` says (a quarter by default); never
/// otherwise.
#[macro_export]
macro_rules! buggify {
    () => {
        $crate::buggify!(0.25)
    };
    ($p:expr) => {
        $crate::buggify::fires(file!(), line!(), $p)
    };
}

/// Whether the site at `file:line` fires, as [`buggify!`] says: never, built normally.
#[cfg(not(any(test, feature = "testing")))]
#[inline(always)]
pub fn fires(_file: &'static str, _line: u32, _p: f64) -> bool {
    false
}

/// Whether the site at `file:line` fires, as [`buggify!`] says.
#[cfg(any(test, feature = "testing"))]
pub fn fires(file: &'static str, line: u32, p: f64) -> bool {
    RUN.with(|run| {
        let mut run = run.borrow_mut();
        let Some(run) = run.as_mut() else { return false };
        let on = *run.sites.entry((file, line)).or_insert_with(|| {
            // One in four, as `hash(seed, file, line)` says.
            mix(run.seed ^ mix(fnv(file) ^ u64::from(line))).is_multiple_of(4)
        });
        on && (run.next() as f64 / u64::MAX as f64) < p
    })
}

/// cladefs: a number below `bound` (at least 1), drawn from the run's own draws, as the seed says,
/// for a site that fired to say how much (how long to wait, board 14565): 0 outside a run that
/// switched buggify on. Built normally, 0, a constant.
#[cfg(not(any(test, feature = "testing")))]
#[inline(always)]
pub fn below(_bound: u64) -> u64 {
    0
}

/// cladefs: a number below `bound` (at least 1), drawn from the run's own draws, as the seed says,
/// for a site that fired to say how much (how long to wait, board 14565): 0 outside a run that
/// switched buggify on.
#[cfg(any(test, feature = "testing"))]
pub fn below(bound: u64) -> u64 {
    RUN.with(|run| run.borrow_mut().as_mut().map_or(0, |run| run.next() % bound.max(1)))
}

/// cladefs: waits a while, up to `most` of the clock's time, as the run's draws say ([`below`]), for
/// a site that fired: a request held back, as a slow network or a paused process holds it, so that
/// what races it lands first. Nothing is sent, nor any lock held, while it waits, so a request its
/// caller drops meanwhile never lands. Built normally no site fires, and none waits; nor does one
/// on an executor with no clock, as local_cell::sim's, whose scheduler holds requests itself.
pub async fn hold(most: std::time::Duration) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let millis = u64::try_from(most.as_millis()).unwrap_or(u64::MAX);
    tokio::time::sleep(std::time::Duration::from_millis(below(millis))).await;
}

/// cladefs: notes that the code did what none may, `what`, as FoundationDB's simulation fails a run
/// on any event of severity error (board 14565): a writer stopped finding what its cell needs gone
/// or corrupt, which no fault the simulations make may cause, only a rule of the protocol broken.
/// The simulation's checker reads them ([`violations`]). Built normally, nothing: production logs
/// what it found, as it always did.
#[cfg(not(any(test, feature = "testing")))]
#[inline(always)]
pub fn violated(_what: impl FnOnce() -> String) {}

/// cladefs: notes that the code did what none may, `what`, as FoundationDB's simulation fails a run
/// on any event of severity error (board 14565), for the simulation's checker ([`violations`]).
#[cfg(any(test, feature = "testing"))]
pub fn violated(what: impl FnOnce() -> String) {
    VIOLATIONS.with(|violations| violations.borrow_mut().push(what()));
}

/// cladefs: what this thread's code did that none may since it was last asked ([`violated`]), in
/// the order it did them, taken.
#[cfg(any(test, feature = "testing"))]
pub fn violations() -> Vec<String> {
    VIOLATIONS.with(|violations| std::mem::take(&mut *violations.borrow_mut()))
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static VIOLATIONS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(any(test, feature = "testing"))]
struct Run {
    seed: u64,
    /// The sites asked about, and whether this run switched each on.
    sites: std::collections::BTreeMap<(&'static str, u32), bool>,
    state: u64,
}

#[cfg(any(test, feature = "testing"))]
impl Run {
    /// The next of the run's draws: SplitMix64, from its seed.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.state)
    }
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static RUN: std::cell::RefCell<Option<Run>> = const { std::cell::RefCell::new(None) };
}

/// Has this thread's simulation run of `seed` buggify, if the seed says so (four in five): its
/// sites switched on and fired as the seed says, whatever ran on the thread before. Whether it
/// did.
#[cfg(any(test, feature = "testing"))]
pub fn enable(seed: u64) -> bool {
    let on = !mix(seed ^ 0xb066_1f1e).is_multiple_of(5);
    RUN.with(|run| {
        let state = mix(seed ^ 0x5eed);
        *run.borrow_mut() = on.then(|| Run { seed, sites: Default::default(), state });
    });
    on
}

/// Has this thread's code buggify nothing, as built normally.
#[cfg(any(test, feature = "testing"))]
pub fn disable() {
    RUN.with(|run| *run.borrow_mut() = None);
}

/// The sites this thread's run has asked about so far, and whether it switched each on: what a
/// failing seed's description says.
#[cfg(any(test, feature = "testing"))]
pub fn sites() -> Vec<String> {
    RUN.with(|run| match &*run.borrow() {
        None => vec!["buggify off".to_owned()],
        Some(run) => (run.sites.iter())
            .map(|((file, line), on)| format!("{file}:{line} {}", if *on { "on" } else { "off" }))
            .collect(),
    })
}

/// SplitMix64's finalizer: a seed's bits spread over the whole word.
#[cfg(any(test, feature = "testing"))]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// FNV-1a of `text`: a file's name as a number, the same on every host and build.
#[cfg(any(test, feature = "testing"))]
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    /// Off, no site fires; a run's sites and draws are its seed's alone; about a quarter of the
    /// sites a run asks about are on, and an active site fires about as often as it says.
    #[test]
    fn runs_fire_as_their_seeds_say() {
        super::disable();
        assert!(!crate::buggify!(1.0));
        let draws = |seed| {
            super::enable(seed);
            let fired: Vec<bool> = (0..64).map(|_| super::fires("a.rs", 7, 0.5)).collect();
            (fired, super::sites())
        };
        let seed = (0..).find(|&seed| super::enable(seed)).unwrap();
        assert_eq!(draws(seed), draws(seed));
        // Of 4000 sites, about a quarter on; of 4000 asks of an on site, about half fire at 0.5.
        super::enable(seed);
        let on = (0..4000).filter(|&line| super::fires("b.rs", line, 1.0)).count();
        assert!((800..1200).contains(&on), "{on} of 4000 sites on");
        let line = (0..).find(|&line| super::fires("c.rs", line, 1.0)).unwrap();
        let fired = (0..4000).filter(|_| super::fires("c.rs", line, 0.5)).count();
        assert!((1800..2200).contains(&fired), "{fired} of 4000 asks fired");
        super::disable();
    }
}
