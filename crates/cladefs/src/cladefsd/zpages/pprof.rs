// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: CPU profiles of the daemon, taken as they are asked for (TASK-672.3), as Go serves
//! them at `/debug/pprof/profile`: `/z/pprof/profile?seconds=N`, the daemon's threads sampled for
//! N seconds, as pprof's protobuf, which `go tool pprof` reads; `/z/pprof/flamegraph?seconds=N`,
//! the same as an SVG flamegraph. N from 1 to [`MOST`], [`DEFAULT`] if not given. Sampled by
//! pprof-rs (Apache-2.0), with SIGPROF, [`FREQUENCY`] times a CPU second, walking frame pointers,
//! which every build here keeps (`.cargo/config.toml`). One at a time: a second asked for while
//! one is taken is refused, `409`, as SIGPROF's timer is the process's.
//!
//! What sampling costs the daemon: a signal on a thread running, about [`FREQUENCY`] times a
//! second for each, its stack walked; and SIGPROF interrupts system calls, which it restarts
//! (`SA_RESTART`) where the kernel can, the others failing `EINTR`, as Rust's standard library,
//! tokio and the daemon's own loops take.
//!
//! Untagged: samples say the stack alone, not the cell or the request they were for (Census's
//! tags), which pprof-rs keeps no place for.

use crate::cladefsd::http::Response;
use pprof::protos::Message as _;
use std::sync::Mutex;
use std::time::Duration;

/// How long a profile samples, unless `seconds` says.
pub const DEFAULT: Duration = Duration::from_secs(30);

/// The longest a profile samples.
pub const MOST: Duration = Duration::from_secs(300);

/// How many times a CPU second a thread is sampled: 99, not 100, so that samples don't fall in
/// step with what runs every 10 ms.
pub const FREQUENCY: i32 = 99;

/// Held while a profile is taken: one at a time.
static PROFILING: Mutex<()> = Mutex::new(());

/// What a profile is given as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// pprof's protobuf, uncompressed, which `go tool pprof` reads as it reads it gzipped.
    Pprof,
    /// An SVG flamegraph (inferno's).
    Flamegraph,
}

/// The page at `/z/pprof{page}`, asked with `query`.
pub(super) fn serve(page: &str, query: &str) -> Response {
    let format = match page {
        "/profile" => Format::Pprof,
        "/flamegraph" => Format::Flamegraph,
        _ => return Response::not_found(),
    };
    let duration = match seconds(query) {
        Ok(duration) => duration,
        Err(why) => return Response::plain("400 Bad Request", why),
    };
    let Ok(_profiling) = PROFILING.try_lock() else {
        return Response::plain("409 Conflict", "a profile is being taken: one at a time");
    };
    match profile(duration, format) {
        Ok(response) => response,
        Err(error) => {
            log::warn!("cladefsd: taking a CPU profile: {error}");
            Response::plain("500 Internal Server Error", format!("taking a profile: {error}"))
        }
    }
}

/// How long `query` asks a profile to sample: its `seconds`, from 1 to [`MOST`]'s, or [`DEFAULT`];
/// why not, if it asks for another.
fn seconds(query: &str) -> Result<Duration, String> {
    let asked = url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "seconds")
        .map(|(_, value)| value.into_owned());
    let Some(asked) = asked else { return Ok(DEFAULT) };
    asked
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .filter(|duration| (Duration::from_secs(1)..=MOST).contains(duration))
        .ok_or_else(|| format!("seconds must be 1 to {}, not {asked:?}", MOST.as_secs()))
}

/// The daemon's threads sampled for `duration`, given as `format`.
fn profile(duration: Duration, format: Format) -> Result<Response, pprof::Error> {
    let profiler = pprof::ProfilerGuardBuilder::default()
        .frequency(FREQUENCY)
        // Not sampled within them: a signal there would walk, or resolve, through code that may
        // hold the locks doing so takes.
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()?;
    std::thread::sleep(duration);
    let report = profiler.report().build()?;
    drop(profiler);
    let mut body = Vec::new();
    Ok(match format {
        Format::Pprof => {
            report.pprof()?.encode(&mut body).map_err(std::io::Error::other)?;
            Response::ok("application/octet-stream", body)
        }
        Format::Flamegraph => {
            report.flamegraph(&mut body)?;
            Response::ok("image/svg+xml", body)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn profiles_sample_for_the_seconds_asked() {
        assert_eq!(seconds(""), Ok(DEFAULT));
        assert_eq!(seconds("seconds=5"), Ok(Duration::from_secs(5)));
        assert_eq!(seconds("seconds=0"), Err("seconds must be 1 to 300, not \"0\"".to_owned()));
        assert_eq!(seconds("seconds=x"), Err("seconds must be 1 to 300, not \"x\"".to_owned()));
        assert_eq!(serve("/profile", "seconds=301").status, "400 Bad Request");
        assert_eq!(serve("/nothing", "").status, "404 Not Found");
    }

    /// A profile asked for while another is taken is refused, at once.
    #[test]
    fn profiles_are_taken_one_at_a_time() {
        let _taken = PROFILING.lock().unwrap();
        let refused = serve("/flamegraph", "seconds=300");
        assert_eq!(refused.status, "409 Conflict");
        assert_eq!(refused.body, b"a profile is being taken: one at a time\n");
    }

    /// A thread spinning for the profile's second, named, is what it sampled: pprof's protobuf,
    /// whose functions name it; and a flamegraph, SVG. Asked for again until a profile's second
    /// gave the thread a fifth of a CPU-second, about 20 samples, by its own CPU clock: a loaded
    /// host gave it next to none once, and the profile was empty (board 2924). A profile that
    /// misses a thread that ran fails.
    #[test]
    fn profiles_say_what_ran() {
        #[inline(never)]
        fn spins_for_the_profile(stop: &AtomicBool) {
            let mut x = 0u64;
            while !stop.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spinning = {
            let stop = stop.clone();
            std::thread::spawn(move || spins_for_the_profile(&stop))
        };
        // The spinning thread's CPU time, by its clock.
        let clock = {
            use std::os::unix::thread::JoinHandleExt as _;
            let mut clock: libc::clockid_t = 0;
            // SAFETY: the thread is alive until it is joined, below.
            let got = unsafe { libc::pthread_getcpuclockid(spinning.as_pthread_t(), &mut clock) };
            assert_eq!(got, 0, "the spinning thread's CPU clock");
            clock
        };
        let ran = || {
            let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: `now` is a timespec to write.
            assert_eq!(unsafe { libc::clock_gettime(clock, &mut now) }, 0);
            Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
        };
        // A page served while the thread had its fifth of a CPU-second: 30 tries at most.
        let while_spinning = |page: &str| {
            for _ in 0..30 {
                let before = ran();
                let response = serve(page, "seconds=1");
                if ran() - before >= Duration::from_millis(200) {
                    return response;
                }
            }
            panic!("the spinning thread never had a fifth of a CPU-second in a second");
        };

        let response = while_spinning("/profile");
        assert_eq!(
            (response.status, response.content_type),
            ("200 OK", "application/octet-stream")
        );
        let profile = pprof::protos::Profile::decode(response.body.as_slice()).unwrap();
        let names: Vec<&str> = (profile.function.iter())
            .map(|function| profile.string_table[function.name as usize].as_str())
            .collect();
        assert!(names.iter().any(|name| name.contains("spins_for_the_profile")), "{names:?}");
        assert!(!profile.sample.is_empty());

        let flamegraph = while_spinning("/flamegraph");
        assert_eq!((flamegraph.status, flamegraph.content_type), ("200 OK", "image/svg+xml"));
        let svg = String::from_utf8(flamegraph.body).unwrap();
        assert!(svg.starts_with("<?xml"), "{svg}");
        assert!(svg.contains("spins_for_the_profile"), "{svg}");
        stop.store(true, Ordering::Relaxed);
        spinning.join().unwrap();
    }
}
