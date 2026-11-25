// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's bucket and part-cache handles, owned by [`Daemon`], separated from cell
//! lifecycle and registry (`super::cells`).

use super::Daemon;
use crate::Errno;
use bucket_cell::cache::Cache;
use bucket_cell::object_bucket::RoutedBucket;
use std::path::PathBuf;
use std::sync::Arc;

/// What `daemon` did with its bucket so far, if it made it (`metrics`).
pub(super) fn bucket_stats(daemon: &Daemon) -> Option<bucket_cell::object_bucket::Stats> {
    daemon.bucket.get().map(|bucket| bucket.stats())
}

/// cladefs: what `daemon` asked of each bucket, its own and those of other buckets' cells
/// (TASK-633.4), by number (`metrics`).
pub(super) fn bucket_stats_by_bucket(
    daemon: &Daemon,
) -> Vec<(u16, bucket_cell::object_bucket::Stats)> {
    daemon.bucket.get().map(|bucket| bucket.stats_by_bucket()).unwrap_or_default()
}

/// cladefs: the bucket `CLADEFSD_BUCKET` names, which `daemon`'s cells are kept in:
/// `EIO` if it names none, as the daemon's host must (`crate::bucket::Host`, TASK-45).
pub(crate) fn daemon_bucket_on(daemon: &Arc<Daemon>) -> Result<Arc<RoutedBucket>, Errno> {
    if let Some(bucket) = daemon.bucket.get() {
        return Ok(bucket.clone());
    }
    let host = crate::bucket::Host::from_env().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EIO
    })?;
    let bucket = crate::host_cell::bucket_on(&daemon.host, &host.bucket).map_err(|err| {
        log::error!("cladefsd: CLADEFSD_BUCKET: {err:#}");
        Errno::EIO
    })?;
    Ok(daemon
        .bucket
        .get_or_init(|| {
            super::telemetry::watch_stats(Arc::downgrade(daemon), Arc::downgrade(&bucket));
            bucket
        })
        .clone())
}

/// Whether the cells opened upload only when something waits for an upload
/// ([`uploads_when_asked`]).
#[cfg(test)]
static UPLOADS_WHEN_ASKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Has the cells opened from now on upload only when something waits for an upload (a fork, a
/// sync, a commit stalled at the lag's bound, the cell closing), never as their interval runs
/// out, as `bucket::fork_faults`' cells do. For tests that set a failpoint to fail once for one
/// cell's work: failpoints are the process's, and another cell's uploader, woken by its
/// interval, would pass it first (TASK-157).
#[cfg(test)]
pub(super) fn uploads_when_asked() {
    UPLOADS_WHEN_ASKED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// How long the cells' durable commits wait at most before they upload: the uploader's default;
/// in a test that asked for it, until something waits for an upload ([`uploads_when_asked`]).
pub(super) fn upload_interval() -> std::time::Duration {
    #[cfg(test)]
    if UPLOADS_WHEN_ASKED.load(std::sync::atomic::Ordering::Relaxed) {
        return std::time::Duration::MAX;
    }
    local_cell::upload::Options::default().interval
}

/// Makes `bucket` `daemon`'s, before any cell opens: for tests that put themselves between the
/// daemon's cells and their objects. The cells naming `CLADEFSD_BUCKET`'s bucket open it
/// (`host_cell::use_bucket`).
#[cfg(any(test, madsim))]
pub(super) fn use_bucket_on(daemon: &Arc<Daemon>, bucket: impl Into<Arc<RoutedBucket>>) {
    let bucket = bucket.into();
    let host = crate::bucket::Host::from_env().expect("the daemon's host");
    crate::host_cell::use_bucket_on(&daemon.host, &host.bucket, bucket.clone());
    assert!(daemon.bucket.set(bucket.clone()).is_ok(), "the daemon has its bucket already");
    super::telemetry::watch_stats(Arc::downgrade(daemon), Arc::downgrade(&bucket));
}

/// Parses `CLADEFSD_PART_CACHE` into one or more JBOD shard directories: colon-separated paths
/// (`std::env::split_paths`), with shell-style glob patterns (`*`, `?`, `[...]`) expanded per
/// component. A glob that matches nothing fails hard.
pub(super) fn parse_part_cache_dirs(raw: &std::ffi::OsStr) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    for entry in std::env::split_paths(raw) {
        if entry.as_os_str().is_empty() {
            continue;
        }
        let has_glob = entry.to_str().is_some_and(|s| s.contains(['*', '?', '[']));
        if has_glob {
            let mut matched = expand_glob(&entry)?;
            if matched.is_empty() {
                return Err(format!(
                    "CLADEFSD_PART_CACHE: glob {} matched no directories",
                    entry.display()
                ));
            }
            matched.sort();
            dirs.extend(matched);
        } else {
            dirs.push(entry);
        }
    }
    Ok(dirs)
}

/// Best-effort per-entry expansion of `CLADEFSD_PART_CACHE` for internal-directory exclusion:
/// unlike [`parse_part_cache_dirs`], a single glob that stopped matching after startup does not
/// discard the surviving globs' or literal paths' matches.
pub(super) fn expand_part_cache_dirs_best_effort(raw: &std::ffi::OsStr) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for entry in std::env::split_paths(raw) {
        if entry.as_os_str().is_empty() {
            continue;
        }
        let has_glob = entry.to_str().is_some_and(|s| s.contains(['*', '?', '[']));
        if has_glob {
            // An unmatching or unreadable glob contributes nothing rather than discarding other
            // surviving entries; opened shards are also excluded directly from the live `Cache`.
            if let Ok(matched) = expand_glob(&entry) {
                dirs.extend(matched);
            }
        } else {
            dirs.push(entry);
        }
    }
    dirs
}

/// Expands a path containing glob wildcards (`*`, `?`, `[...]`) across directory components.
fn expand_glob(pattern: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let mut current = vec![PathBuf::new()];
    for comp in pattern.components() {
        let mut next = Vec::new();
        match comp {
            std::path::Component::RootDir
            | std::path::Component::Prefix(_)
            | std::path::Component::CurDir
            | std::path::Component::ParentDir => {
                for base in current {
                    next.push(base.join(comp));
                }
            }
            std::path::Component::Normal(seg) => {
                let pat = seg.to_str();
                if let Some(pat) = pat
                    && pat.contains(['*', '?', '['])
                {
                    for base in current {
                        let scan = if base.as_os_str().is_empty() {
                            std::path::Path::new(".")
                        } else {
                            base.as_path()
                        };
                        let entries = match host_fs::read_dir(scan) {
                            Ok(entries) => entries,
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(err) => {
                                return Err(format!(
                                    "CLADEFSD_PART_CACHE: reading {} for {}: {err}",
                                    scan.display(),
                                    pattern.display()
                                ));
                            }
                        };
                        for entry in entries {
                            let entry = entry.map_err(|err| {
                                format!(
                                    "CLADEFSD_PART_CACHE: reading {} for {}: {err}",
                                    scan.display(),
                                    pattern.display()
                                )
                            })?;
                            let name = entry.file_name();
                            if let Some(name) = name.to_str()
                                && glob_match(pat, name)
                            {
                                next.push(base.join(name));
                            }
                        }
                    }
                } else {
                    for base in current {
                        next.push(base.join(seg));
                    }
                }
            }
        }
        current = next;
    }
    Ok(current.into_iter().filter(|p| host_fs::is_dir(p)).collect())
}

/// Minimal glob matcher supporting `*`, `?`, and `[...]` character classes (no path separators).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == '?' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '[' {
            if let Some(end) = p[pi + 1..].iter().position(|&c| c == ']') {
                let class = &p[pi + 1..pi + 1 + end];
                let negate = class.first() == Some(&'!') || class.first() == Some(&'^');
                let chars = if negate { &class[1..] } else { class };
                let mut matched = false;
                let mut i = 0;
                while i < chars.len() {
                    if i + 2 < chars.len() && chars[i + 1] == '-' {
                        if t[ti] >= chars[i] && t[ti] <= chars[i + 2] {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if t[ti] == chars[i] {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if matched != negate {
                    pi = pi + 2 + end;
                    ti += 1;
                } else if let Some(sp) = star_p {
                    star_t += 1;
                    ti = star_t;
                    pi = sp + 1;
                } else {
                    return false;
                }
            } else if p[pi] == t[ti] {
                pi += 1;
                ti += 1;
            } else if let Some(sp) = star_p {
                star_t += 1;
                ti = star_t;
                pi = sp + 1;
            } else {
                return false;
            }
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_p {
            star_t += 1;
            ti = star_t;
            pi = sp + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// cladefs: where the daemon keeps its part cache, and how much it may keep there, as the
/// environment says: one or more colon-separated directories (or glob patterns) in
/// `CLADEFSD_PART_CACHE`, if set (empty, it names none); the aggregate bytes of objects
/// `CLADEFSD_PART_CACHE_BYTES` gives, a budget, as a host whose cache shares its disk with its
/// cells needs (TASK-406.1), or, unset, none: as many as the disk governor leaves each disk
/// (doc-22). Read at the daemon's start, which opens the cache then ([`open_part_cache`],
/// board 8595), to refuse what it can't use.
pub fn cache_config() -> Result<Option<(Vec<PathBuf>, u64)>, String> {
    let set = |var| node_local::env::var_os(var).filter(|value| !value.is_empty());
    let dirs = match set("CLADEFSD_PART_CACHE") {
        Some(raw) => {
            let dirs = parse_part_cache_dirs(&raw)?;
            if dirs.is_empty() { None } else { Some(dirs) }
        }
        None => None,
    };
    let capacity = match set("CLADEFSD_PART_CACHE_BYTES") {
        None => u64::MAX,
        Some(_) if dirs.is_none() => {
            return Err("CLADEFSD_PART_CACHE_BYTES is set, but CLADEFSD_PART_CACHE names no \
                        directory for the cache"
                .to_owned());
        }
        // Not UTF-8, or not a number: refused below.
        Some(bytes) => match bytes.to_str().and_then(|bytes| bytes.parse().ok()) {
            Some(0) | None => {
                return Err(format!(
                    "CLADEFSD_PART_CACHE_BYTES must be a positive number of bytes: {bytes:?}"
                ));
            }
            Some(bytes) => bytes,
        },
    };
    Ok(dirs.map(|dirs| (dirs, capacity)))
}

/// cladefs: `daemon`'s part cache, if it opened one ([`part_cache`]): opening none.
pub(super) fn opened_cache(daemon: &Daemon) -> Option<Arc<Cache>> {
    daemon.part_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().flatten()
}

/// cladefs: the cache the environment configures ([`cache_config`]), if it does, where what
/// `daemon`'s cells read from their bucket is kept. Opened once, by the daemon's boot
/// ([`open_part_cache`]), or by the first cell to open in a daemon served without one booted, those
/// opening meanwhile waiting for it (TASK-67). A failure is the caller's, and the next tries again.
pub(super) fn part_cache(daemon: &Daemon) -> Result<Option<Arc<Cache>>, Errno> {
    open_part_cache(daemon).map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EIO
    })
}

/// Opens `daemon`'s part cache explicitly over `dirs` with aggregate `capacity` (used by TOML
/// `boot_with_config` via `DiskPool`).
pub(super) fn open_part_cache_with(
    daemon: &Daemon,
    dirs: &[PathBuf],
    capacity: u64,
) -> Result<Option<Arc<Cache>>, String> {
    let mut opened = daemon.part_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cache) = &*opened {
        return Ok(cache.clone());
    }
    if dirs.is_empty() {
        return Ok(opened.insert(None).clone());
    }
    let cache = local_cell::upload::blocking(|| {
        Cache::open_all_on(daemon.host.bucket_host(), dirs, capacity)
    })
    .map_err(|err| {
        let shown = dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(":");
        format!("part cache ({shown}): {err}")
    })?;
    Ok(opened.insert(Some(Arc::new(cache))).clone())
}

/// cladefs: the part cache, as [`part_cache`] opens it, or why it can't be, naming the variable
/// that configures it and the cause (board 8595).
pub(super) fn open_part_cache(daemon: &Daemon) -> Result<Option<Arc<Cache>>, String> {
    let mut opened = daemon.part_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cache) = &*opened {
        return Ok(cache.clone());
    }
    let config = local_cell::upload::blocking(cache_config)?;
    let cache = match config {
        Some((dirs, capacity)) => match local_cell::upload::blocking(|| {
            Cache::open_all_on(daemon.host.bucket_host(), &dirs, capacity)
        }) {
            Ok(cache) => Some(Arc::new(cache)),
            Err(err) => {
                let shown = if dirs.len() == 1 {
                    dirs[0].display().to_string()
                } else {
                    dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(":")
                };
                return Err(format!("CLADEFSD_PART_CACHE: {shown}: {err}"));
            }
        },
        None => None,
    };
    Ok(opened.insert(cache).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cladefs: the daemon's first cells, opened at once, share the one cache, which holds its
    /// directory locked: each found none yet and opened its own, and the second's lock failed, its
    /// cell with EIO, as a daemon started again served its VMs booted again all at once
    /// (fxfs-guest-soak, TASK-67: a VM of five exited 1 at boot).
    #[test]
    fn cells_opened_at_once_share_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path()) };
        let daemon = Daemon::new();
        let at_once = std::sync::Barrier::new(16);
        let caches: Vec<_> = std::thread::scope(|scope| {
            let opening: Vec<_> = (0..16)
                .map(|_| {
                    let daemon = &daemon;
                    let at_once = &at_once;
                    scope.spawn(move || {
                        at_once.wait();
                        part_cache(daemon)
                    })
                })
                .collect();
            opening.into_iter().map(|opening| opening.join().unwrap()).collect()
        });
        let first = caches[0].clone().expect("a cache").expect("CLADEFSD_PART_CACHE's");
        for cache in caches {
            let cache = cache.expect("a cache").expect("CLADEFSD_PART_CACHE's");
            assert!(Arc::ptr_eq(&cache, &first), "two caches of one directory");
        }
        assert_eq!(first.capacity(), u64::MAX, "a cache with no budget");
    }

    /// TASK-406.1: the daemon's cache keeps no more than `CLADEFSD_PART_CACHE_BYTES`, as a host
    /// whose cache shares its disk with its cells needs.
    #[test]
    fn the_cache_takes_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_PART_CACHE", dir.path());
            std::env::set_var("CLADEFSD_PART_CACHE_BYTES", "268435456");
        }
        let daemon = Daemon::new();
        let cache = part_cache(&daemon).expect("a cache").expect("CLADEFSD_PART_CACHE's");
        assert_eq!(cache.capacity(), 256 << 20);
    }

    /// TASK-406.1: a budget the cache can't take stops the daemon at its start, naming the
    /// variable: none, not a number, or with no cache to take it.
    #[test]
    fn budgets_the_cache_cant_take_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("CLADEFSD_PART_CACHE") };
        // SAFETY: as above: no other thread of this test's process runs.
        let set = |bytes: &str| unsafe { std::env::set_var("CLADEFSD_PART_CACHE_BYTES", bytes) };
        set("1");
        assert_eq!(
            cache_config(),
            Err("CLADEFSD_PART_CACHE_BYTES is set, but CLADEFSD_PART_CACHE names no directory \
                 for the cache"
                .to_owned())
        );
        // SAFETY: as above.
        unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path()) };
        for bytes in ["0", "-1", "1G", "x"] {
            set(bytes);
            assert_eq!(
                cache_config(),
                Err(format!(
                    "CLADEFSD_PART_CACHE_BYTES must be a positive number of bytes: {bytes:?}"
                ))
            );
        }
        set("");
        assert_eq!(cache_config(), Ok(Some((vec![dir.path().to_owned()], u64::MAX))));
    }

    #[test]
    fn jbod_cache_config_splits_and_expands_globs() {
        let dir = tempfile::tempdir().unwrap();
        let d0 = dir.path().join("nvme0").join("part-cache");
        let d1 = dir.path().join("nvme1").join("part-cache");
        std::fs::create_dir_all(&d0).unwrap();
        std::fs::create_dir_all(&d1).unwrap();

        let joined = std::env::join_paths([&d0, &d1]).unwrap();
        assert_eq!(parse_part_cache_dirs(&joined).unwrap(), vec![d0.clone(), d1.clone()]);

        let glob = dir.path().join("nvme*").join("part-cache");
        assert_eq!(parse_part_cache_dirs(glob.as_os_str()).unwrap(), vec![d0.clone(), d1.clone()]);

        let missing = dir.path().join("missing*").join("part-cache");
        assert!(parse_part_cache_dirs(missing.as_os_str()).is_err());

        // Best-effort expansion preserves surviving globs even when one glob stops matching.
        let mixed = std::env::join_paths([&missing, &glob]).unwrap();
        let mut expanded = expand_part_cache_dirs_best_effort(&mixed);
        expanded.sort();
        assert_eq!(expanded, vec![d0, d1]);
    }
}
