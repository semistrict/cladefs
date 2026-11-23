// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: this process's threads' stacks, in its log, when it is sent SIGQUIT (TASK-44.13):
//! what waits for what in a process that hangs, as `cladefsctl import` did (TASK-98), which its
//! user then needed gdb on the host to see. elfutils' eu-stack reads them, from a child of this
//! process: Yama's ptrace_scope lets a process trace its descendants alone, so this one names the
//! child its tracer (`PR_SET_PTRACER`) before the child, waiting for that, becomes eu-stack.
//!
//! cladefs: through the log, a record for each thread, so that a JSON log stays a JSON object a
//! line, and a closed standard error fails no write (board 8567); and eu-stack killed past
//! [`BOUND`], as tracing a thread in uninterruptible sleep may hang it, which would leave every
//! SIGQUIT after unanswered.
//!
//! Threads alone, not tokio's tasks: its task dump (`Handle::dump`) builds only with
//! `--cfg tokio_unstable` and its `taskdump` feature, which this workspace doesn't set. A task
//! that waits shows as its runtime's worker parked, or, blocked, as the thread it blocks.

use crate::signals::{Signals, spawn_taking_no_signals};
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// cladefs: how long eu-stack has to read the stacks, past which it is killed (board 8567): a
/// daemon's few hundred threads take it about a second.
const BOUND: Duration = Duration::from_secs(10);

/// What the log's records of the stacks begin with.
const HEADING: &str = "cladefs: the threads' stacks, on SIGQUIT:";

/// Blocks SIGQUIT in the calling thread, and those it starts from now on, and starts a thread that
/// logs the process's stacks each time it is sent: before the process starts
/// other threads, which would take the signal otherwise, and end it (`crate::signals`).
pub fn on_sigquit() -> std::io::Result<()> {
    let sigquit = Signals::block(&[libc::SIGQUIT])?;
    spawn_taking_no_signals("stacks on SIGQUIT", move || {
        loop {
            if let Err(error) = sigquit.wait() {
                log::error!("waiting for SIGQUIT: {error}");
                return;
            }
            log_stacks();
        }
    })
}

/// Logs every thread's stack, through eu-stack, a record each, or why it couldn't: as a warning,
/// which processes logging warnings alone, as cladefsctl, log too.
pub fn log_stacks() {
    let output = match stacks() {
        Ok(output) => output,
        Err(err) => {
            log::warn!("{HEADING} {err}");
            return;
        }
    };
    let stacks = String::from_utf8_lossy(&output.stdout);
    let (process, threads) = threads(&stacks);
    log::warn!("{HEADING} {} threads of {}", threads.len(), process.trim());
    for thread in threads {
        log::warn!("{HEADING} {}", thread.trim_end());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    if !said.trim().is_empty() || !output.status.success() {
        log::warn!("{HEADING} eu-stack {}: {}", output.status, said.trim_end());
    }
}

/// What eu-stack wrote, `stacks`, as what it says before the threads, its process, and each
/// thread's lines, from its `TID` line on.
fn threads(stacks: &str) -> (&str, Vec<&str>) {
    let mut starts = Vec::new();
    let mut at = 0;
    for line in stacks.split_inclusive('\n') {
        if line.starts_with("TID ") {
            starts.push(at);
        }
        at += line.len();
    }
    let process = &stacks[..starts.first().copied().unwrap_or(stacks.len())];
    let ends = starts.iter().skip(1).copied().chain([stacks.len()]);
    (process, starts.iter().zip(ends).map(|(&from, to)| &stacks[from..to]).collect())
}

/// What eu-stack says of this process's threads: what it wrote, and how it exited.
fn stacks() -> std::io::Result<Output> {
    let pid = std::process::id();
    // The child waits until it may trace this process, then is eu-stack, as the same process.
    let mut child = Command::new("sh")
        .args(["-c", &format!("read -r ready && exec eu-stack -p {pid}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // SAFETY: A plain prctl call naming this process's tracer.
    let named = unsafe { libc::prctl(libc::PR_SET_PTRACER, child.id() as libc::c_ulong, 0, 0, 0) };
    if named != 0 {
        log::warn!("naming eu-stack this process's tracer: {}", std::io::Error::last_os_error());
    }
    // Its failure, if the child is gone, said once it is waited for.
    let ready = child.stdin.take().expect("its standard input").write_all(b"ready\n");
    let output = output_within(child, BOUND);
    // SAFETY: As above, naming no tracer again.
    unsafe { libc::prctl(libc::PR_SET_PTRACER, 0, 0, 0, 0) };
    ready?;
    output
}

/// What `child`, its standard output and error piped, writes until it exits, and how it exited:
/// killed, an error, if it runs `bound` long.
fn output_within(mut child: Child, bound: Duration) -> std::io::Result<Output> {
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let mut pipe = pipe.expect("a pipe of the child's");
        // cladefs: not of the simulation: a process's own stacks (doc-24).
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    };
    let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let deadline = Instant::now() + bound;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("eu-stack ran {bound:?}, killed: no stacks"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let joined = |reading: std::thread::JoinHandle<std::io::Result<Vec<u8>>>| {
        reading.join().map_err(|_| std::io::Error::other("reading eu-stack's output panicked"))?
    };
    Ok(Output { status, stdout: joined(stdout)?, stderr: joined(stderr)? })
}

#[cfg(test)]
mod tests {
    /// A process's stacks name the functions its threads wait in: here, a thread parked in one of
    /// this test's.
    /// eu-stack is elfutils', which the hosts the tests run on have (scripts/lib/linux-host-setup.sh,
    /// scripts/lima-setup.sh).
    #[test]
    fn stacks_name_where_threads_wait() {
        #[inline(never)]
        fn waits_for_ever(parked: std::sync::mpsc::Sender<()>) {
            parked.send(()).unwrap();
            std::thread::park();
        }
        let (parked, parking) = std::sync::mpsc::channel();
        std::thread::spawn(move || waits_for_ever(parked));
        parking.recv().unwrap();
        let output = super::stacks().unwrap();
        let stacks = String::from_utf8_lossy(&output.stdout);
        let (_, threads) = super::threads(&stacks);
        let waiting = threads.iter().filter(|thread| thread.contains("waits_for_ever"));
        assert_eq!(waiting.count(), 1, "{stacks}");
        assert!(threads.iter().all(|thread| thread.starts_with("TID ")), "{threads:?}");
    }

    /// board 8567: an eu-stack that hangs, as tracing a thread in uninterruptible sleep may, is
    /// killed past its bound, and the dump fails, saying so, rather than wait for ever, every
    /// SIGQUIT after it unanswered.
    #[test]
    fn a_dump_that_hangs_is_killed_past_its_bound() {
        let child = std::process::Command::new("sleep")
            .arg("15")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let began = std::time::Instant::now();
        let output = super::output_within(child, std::time::Duration::from_millis(200));
        assert_eq!(output.map(drop).map_err(|err| err.kind()), Err(std::io::ErrorKind::TimedOut));
        assert!(began.elapsed() < std::time::Duration::from_secs(10), "{:?}", began.elapsed());
    }
}
