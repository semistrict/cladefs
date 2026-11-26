// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: periodic stats-file reporting and bucket request logging (`CLADEFSD_STATS`), separated
//! from cell lifecycle and registry (`super::cells`).

use crate::Daemon;
use bucket_cell::object_bucket::{RoutedBucket, Stats};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// cladefs: how often the bucket's requests and bytes are looked at, and how often, at most, they
/// are logged while they change.
const STATS_POLL: Duration = Duration::from_millis(200);
const STATS_EVERY: Duration = Duration::from_secs(60);
/// cladefs: how often the stats file is written again for its process's line alone, its CPU time
/// and memory, which change each poll: what its file says of the process is no older than this.
const PROCESS_EVERY: Duration = Duration::from_secs(1);

/// cladefs: keeps what `bucket` did, its requests by kind and its bytes up and down
/// (`bucket_cell::object_bucket::Stats::line`), what serving the cells cost ([`daemon_line`]), the
/// requests served (`super::metrics::requests_line`), what its uploads hold ([`uploads_line`]) and
/// what its process holds and spent (`super::process::line_on`, sampled every poll, as the metrics
/// report it, file or not), while `daemon` and `bucket` live: in the file `CLADEFSD_STATS` names,
/// if it names one, a line each, rewritten whole within [`STATS_POLL`] of each change, which the
/// bench reports (`scripts/bench-report.py`); and the bucket's in the log every [`STATS_EVERY`]
/// that it changed.
pub(super) fn watch_stats(daemon: Weak<Daemon>, bucket: Weak<RoutedBucket>) {
    let file = node_local::env::var_os("CLADEFSD_STATS").filter(|file| !file.is_empty());
    let file = file.map(PathBuf::from);
    let mut logged = Stats::default();
    let mut written = Vec::new();
    let (mut at, mut at_process) = (Instant::now(), Instant::now());
    // cladefs: a thread of its own, or under madsim a task (`super::run`, doc-24); a turn every
    // STATS_POLL, the first one poll after it starts.
    let mut started = false;
    let watching = crate::run::every("cladefsd stats", STATS_POLL, std::thread::sleep, move || {
        if !std::mem::replace(&mut started, true) {
            return true;
        }
        {
            let (Some(daemon), Some(bucket)) = (daemon.upgrade(), bucket.upgrade()) else {
                return false;
            };
            let stats = bucket.stats();
            // cladefs: what the process holds, against its budget, whether or not a file keeps it
            // (TASK-330.2).
            crate::process::sample_on(&daemon);
            if let Some(file) = &file {
                let lines = [
                    stats.line(),
                    daemon_line(&daemon),
                    crate::metrics::requests_line(),
                    uploads_line(&daemon),
                    cache_line(&daemon),
                    buckets_line(&bucket),
                    crate::disk::line_on(&daemon),
                    crate::peers::line_on(&daemon).unwrap_or_else(|| "peers: none".to_owned()),
                    crate::process::line_on(&daemon),
                ];
                // cladefs: the process's line, its CPU time changing each poll, every
                // PROCESS_EVERY at most, unless the rest changed (TASK-330.2).
                let (rest, process) = lines.split_at(lines.len() - 1);
                let changed = written.get(..rest.len()) != Some(rest)
                    || (written.last() != process.last() && at_process.elapsed() >= PROCESS_EVERY);
                if changed {
                    if let Err(err) = write_stats(&daemon, file, &lines) {
                        log::warn!("cladefsd: writing {}: {err}", file.display());
                    }
                    written = lines.to_vec();
                    at_process = Instant::now();
                }
            }
            if stats != logged && at.elapsed() >= STATS_EVERY {
                log::info!("cladefsd: {}", stats.line());
                (logged, at) = (stats, Instant::now());
            }
        }
        true
    });
    if let Err(err) = watching {
        log::warn!("cladefsd: watching object storage's stats: {err}");
    }
}

/// cladefs: the stats file's line of what the daemon asked of other buckets than its own
/// (TASK-633.4): each one's GETs, downloaded and uploaded bytes, by number.
fn buckets_line(bucket: &RoutedBucket) -> String {
    use bucket_cell::object_bucket::{Request, Upload};
    let others: Vec<String> = (bucket.stats_by_bucket().into_iter().skip(1))
        .map(|(number, stats)| {
            let gets = stats.requests(Request::Get) + stats.requests(Request::GetRange);
            let uploaded: u64 = Upload::ALL.iter().map(|upload| stats.uploaded(*upload)).sum();
            format!("{number} gets={gets} downloaded={} uploaded={uploaded}", stats.downloaded)
        })
        .collect();
    match others.is_empty() {
        true => "buckets: its own alone".to_owned(),
        false => format!("buckets: {}", others.join("; ")),
    }
}

/// cladefs: what `daemon`'s uploads hold of the objects they upload, in bytes, now and at most,
/// and the most they may (`crate::bucket::upload_memory_on`, TASK-317).
fn uploads_line(daemon: &Daemon) -> String {
    let memory = cladefs_core::bucket::upload_memory_on(&daemon.host);
    format!("uploads: held={} max-held={} budget={}", memory.held, memory.max_held, memory.budget)
}

/// cladefs: what `daemon`'s part cache keeps, in bytes, and the most it may (TASK-406.1).
fn cache_line(daemon: &Daemon) -> String {
    match crate::storage::opened_cache(daemon) {
        Some(cache) => format!("cache: bytes={} capacity={}", cache.bytes(), cache.capacity()),
        None => "cache: none".to_owned(),
    }
}

/// cladefs: what serving the cells cost `daemon` (`crate::stats::Stats::named`), as a line of
/// `NAME=COUNT` after `daemon:`.
fn daemon_line(daemon: &Daemon) -> String {
    let named = cladefs_core::stats::stats().named().map(|(name, count)| format!("{name}={count}"));
    // cladefs: and how long its cells' commits waited to be made durable (TASK-165.9): each
    // bucket of `local_cell::Lags` a count of its own, within its bound, the last beyond.
    let lags = local_cell::durability_lags_on(daemon.host.local());
    let quantile = |fraction| lags.quantile_ms(fraction).unwrap_or(0);
    let last = local_cell::LAG_BUCKETS - 1;
    let bucket = |(bucket, count): (usize, &u64)| match bucket == last {
        false => format!("local_lag_le_{}ms={count}", 1u64 << bucket),
        true => format!("local_lag_gt_{}ms={count}", 1u64 << (bucket - 1)),
    };
    let buckets: Vec<String> = lags.buckets.iter().enumerate().map(bucket).collect();
    // And where it went: how long its flushes and framings took, phase by phase.
    let timings = local_cell::timings();
    let timed = [
        timed_fields("flush", &timings.flushes, &local_cell::FLUSH_PHASES),
        timed_fields("framing", &timings.framings, &local_cell::FRAMING_PHASES),
        // cladefs: and making the segments framed durable, so many at a time (TASK-310).
        timed_fields("durable", &timings.durable, &local_cell::DURABLE_PHASES),
        vec![
            format!("durable_segments={}", timings.durable_segments),
            format!("own_sync_max_us={}", timings.own_sync_max_us),
            format!("raw_sync_max_us={}", timings.raw_sync_max_us),
        ],
    ]
    .concat();
    // cladefs: the bytes its cells wrote, by kind, against what the process wrote in all, as the
    // kernel counts it, and of that, what went before it was written back (TASK-310).
    let all = local_cell::written();
    let mut written: Vec<String> =
        all.named().map(|(kind, bytes)| format!("written_{kind}_bytes={bytes}")).into();
    written.push(format!("written_writes={}", all.writes));
    written.push(format!("stats_written_bytes={}", daemon.stats_written.load(Ordering::Relaxed)));
    let (framing_read, seen_ahead) = local_cell::framing_reads();
    let once = [
        format!("segments_framed_in_place={}", local_cell::segments_framed_in_place()),
        format!("commits_spanning_segments={}", local_cell::commits_spanning_segments()),
        format!("framing_read_bytes={framing_read}"),
        format!("seen_ahead_bytes={seen_ahead}"),
    ];
    let parts = crate::memory::held_parts_on(daemon).counts();
    let held = [
        format!("held_parts_bytes={}", parts.bytes),
        format!("held_parts_peak_bytes={}", parts.peak),
        format!("held_parts_held_bytes={}", parts.held),
        format!("held_parts_taken_bytes={}", parts.taken),
        format!("held_parts_evicted_bytes={}", parts.evicted),
        format!("held_parts_evicted={}", parts.evicted_parts),
        format!("held_parts_late={}", parts.late_parts),
        format!("held_parts_skipped={}", parts.skipped_parts),
    ];
    let io = process_io().map(|(name, bytes)| format!("io_{name}={bytes}"));
    format!(
        "daemon: {} local_lag_count={} local_lag_max_us={} local_lag_p50_ms={} \
         local_lag_p99_ms={} local_lag_within_50ms={} {} {} {} {} {} {}",
        named.join(" "),
        lags.count,
        lags.max_us,
        quantile(0.5),
        quantile(0.99),
        lags.within_turn,
        buckets.join(" "),
        timed.join(" "),
        written.join(" "),
        once.join(" "),
        held.join(" "),
        io.join(" ")
    )
}

/// cladefs: what this process wrote, as the kernel counts it (/proc/self/io): the bytes it had
/// written to its disk, each as it dirtied the page, and of those, the bytes that went unwritten,
/// their file truncated or removed first; 0 where there is no /proc (TASK-310).
fn process_io() -> [(&'static str, u64); 2] {
    let io = host_fs::read_to_string("/proc/self/io").unwrap_or_default();
    let field = |name: &str| {
        // Not a number: none counted, as a kernel without the field counts.
        (io.lines())
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    };
    [
        ("write_bytes", field("write_bytes")),
        ("cancelled_write_bytes", field("cancelled_write_bytes")),
    ]
}

/// cladefs: `timed`, of `kind`, as `NAME=COUNT` fields: so many, so many slow, the slowest's
/// phases, by `names`, and each phase's longest (TASK-165.9).
fn timed_fields<const N: usize>(
    kind: &str,
    timed: &local_cell::Timed<N>,
    names: &[&str; N],
) -> Vec<String> {
    let mut fields =
        vec![format!("{kind}_count={}", timed.count), format!("{kind}_slow={}", timed.slow)];
    for (which, phases) in [("slowest", &timed.slowest), ("max", &timed.max)] {
        let named = names.iter().zip(phases.us);
        fields.extend(named.map(|(phase, us)| format!("{kind}_{which}_{phase}_us={us}")));
        // cladefs: and how long of it the threads waited to run (TASK-336).
        fields.push(format!("{kind}_{which}_runnable_us={}", phases.runnable_us));
    }
    // cladefs: and each phase summed over all of them, and their waits to run (TASK-336).
    let named = names.iter().zip(timed.sum.us);
    fields.extend(named.map(|(phase, us)| format!("{kind}_sum_{phase}_us={us}")));
    fields.push(format!("{kind}_sum_runnable_us={}", timed.sum.runnable_us));
    fields
}

/// cladefs: replaces `file` with `lines`, whole: a reader finds the old lines or the new. Counted,
/// in the pages it dirties (`daemon.stats_written`, TASK-330.2).
fn write_stats(daemon: &Daemon, file: &Path, lines: &[String]) -> std::io::Result<()> {
    let new = file.with_extension("new");
    let text = lines.join("\n") + "\n";
    host_fs::write(&new, &text)?;
    let pages = (text.len() as u64).div_ceil(cladefs_core::pages::page_size())
        * cladefs_core::pages::page_size();
    daemon.stats_written.fetch_add(pages, Ordering::Relaxed);
    host_fs::rename(&new, file)
}
