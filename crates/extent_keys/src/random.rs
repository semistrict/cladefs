// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the random bytes keys, salts and IDs are drawn from, aws-lc's, drawn no more once
//! the process exits (TASK-650).
//!
//! As a process exits, aws-lc clears every thread's random generator (`rand.c`'s
//! `rand_thread_local_state_clear_all`, an ELF destructor, as FIPS has it), and its seed generator
//! with it (`tree_drbg_jitter_entropy.c`'s `tree_jitter_free_global_drbg`), counting on the program
//! to have stopped its threads first. A thread drawing then aborts the process: an uploader still
//! writing a checkpoint as a test, or a daemon, returns from `main` died of SIGABRT after it
//! passed. So once the process exits, this draws nothing: an `atexit` handler, which runs before
//! the ELF destructors, stops new draws, and waits for those under way, each a few microseconds;
//! a thread asking for random bytes from then on waits for the process to end, as aws-lc's own
//! gate means to have it (`RAND_bytes_core`'s `state_clear_lock`), never handed bytes from a
//! cleared generator.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Whether the process is exiting: no draw begins once it is.
static EXITING: AtomicBool = AtomicBool::new(false);

/// The draws under way, which the exit waits for.
// The process's draws, which its exit waits for: every host's, as the exit is the process's.
static DRAWING: AtomicUsize = AtomicUsize::new(0);

/// The exit's handler, registered with the first draw.
// The process's exit handler, registered once.
static REGISTERED: Once = Once::new();

/// Fills `dest` with random bytes, from aws-lc's generator; once the process is exiting, never
/// returns, the calling thread parked until the process ends.
pub fn fill(dest: &mut [u8]) {
    REGISTERED.call_once(|| {
        // SAFETY: atexit(3) of a function that takes nothing, unwinds never (it neither panics
        // nor calls what may), and touches only statics, which outlive every handler.
        let registered = unsafe { libc::atexit(stop_drawing) };
        assert_eq!(registered, 0, "registering random draws' exit handler");
    });
    let drawing = Drawing::start();
    if EXITING.load(Ordering::SeqCst) {
        drop(drawing);
        loop {
            std::thread::park();
        }
    }
    #[cfg(test)]
    if tests::FAIL.swap(false, Ordering::SeqCst) {
        panic!("the system's random generator fails, as a test has it");
    }
    aws_lc_rs::rand::fill(dest).expect("the system's random generator fills its bytes");
    drop(drawing);
}

/// A draw under way, counted in [`DRAWING`] until dropped: however it ends, a panic's unwinding
/// too, which a caught panic (a task's, a worker thread's) would otherwise leave counted, the exit
/// waiting for it for ever (board 8591).
struct Drawing;

impl Drawing {
    fn start() -> Self {
        // SeqCst, as the exit's handler: either it sees this draw under way and waits, or this
        // sees the process exiting and draws nothing.
        DRAWING.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Drawing {
    fn drop(&mut self) {
        DRAWING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The process exits: draws stop, and those under way end, before aws-lc clears its generators.
extern "C" fn stop_drawing() {
    EXITING.store(true, Ordering::SeqCst);
    while DRAWING.load(Ordering::SeqCst) != 0 {
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// What makes this test's process a child exiting as it draws.
    const CHILD: &str = "EXTENT_KEYS_DRAWING_AT_EXIT";

    /// A failpoint: set, the next draw fails, panicking, as aws-lc's generator failing would.
    // The process's: each child process sets it once, before its one draw.
    pub(super) static FAIL: AtomicBool = AtomicBool::new(false);

    /// What makes this test's process a child whose draw failed before it exits.
    const FAILED_CHILD: &str = "EXTENT_KEYS_DRAW_FAILED";

    /// board 8591: a draw that panics, the panic caught (a task's, a worker thread's), leaves no
    /// draw counted: the process exits, within a minute, where the exit's handler waited for it
    /// for ever, the child never ending.
    #[test]
    fn processes_exit_after_a_draw_failed() {
        if std::env::var_os(FAILED_CHILD).is_some() {
            fill(&mut [0; 8]);
            FAIL.store(true, Ordering::SeqCst);
            let failed = std::thread::spawn(|| fill(&mut [0; 32])).join();
            assert!(failed.is_err(), "the draw failed not");
            std::process::exit(0);
        }
        let name = "random::tests::processes_exit_after_a_draw_failed";
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads", "1"])
            .env(FAILED_CHILD, "1")
            .spawn()
            .unwrap();
        // How long a child that exits may take: one that waits for a draw for ever takes longer.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("the child exits not: its exit waits for a draw that failed");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(status.success(), "the child {status}");
    }

    /// TASK-650: a process exiting as its threads draw random bytes exits as it says, aborting
    /// never: run 20 times, each a child whose 8 threads draw salts without end as it exits, and
    /// whose 8 more start threads without end that each draw once, their first, as the
    /// uploaders' workers aborted (a thread's generator set up after aws-lc cleared its seed's).
    #[test]
    fn processes_exit_cleanly_as_their_threads_draw() {
        if std::env::var_os(CHILD).is_some() {
            // Each thread drawing, or starting threads that draw, before the process exits, as
            // `main` does.
            let drawing = std::sync::Arc::new(std::sync::Barrier::new(17));
            for starts in [false, true].into_iter().flat_map(|starts| [starts; 8]) {
                let drawing = drawing.clone();
                std::thread::spawn(move || {
                    let draw = || fill(&mut [0; 32]);
                    let next = || match starts {
                        true => std::thread::spawn(draw).join().unwrap(),
                        false => draw(),
                    };
                    next();
                    drawing.wait();
                    loop {
                        next();
                    }
                });
            }
            drawing.wait();
            std::process::exit(0);
        }
        let exe = std::env::current_exe().unwrap();
        for round in 0..20 {
            let name = "random::tests::processes_exit_cleanly_as_their_threads_draw";
            let status = Command::new(&exe)
                .args(["--exact", name, "--nocapture", "--test-threads", "1"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "round {round}: the child {status}");
        }
    }
}
