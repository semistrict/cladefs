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
    let bucket = crate::host_cell::bucket(&host.bucket).map_err(|err| {
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
    crate::host_cell::use_bucket(&host.bucket, bucket.clone());
    assert!(daemon.bucket.set(bucket.clone()).is_ok(), "the daemon has its bucket already");
    super::telemetry::watch_stats(Arc::downgrade(daemon), Arc::downgrade(&bucket));
}

/// cladefs: where the daemon keeps its part cache, and how much it may keep there, as the
/// environment says: the directory `CLADEFSD_PART_CACHE` names, if it names one (empty, it names
/// none); the bytes of objects `CLADEFSD_PART_CACHE_BYTES` gives, a budget, as a host whose cache
/// shares its disk with its cells needs (TASK-406.1), or, unset, none: as many as the disk governor
/// leaves it (doc-22). Read at the daemon's start, which opens the cache then ([`open_part_cache`],
/// board 8595), to refuse what it can't use.
pub fn cache_config() -> Result<Option<(PathBuf, u64)>, String> {
    let set = |var| node_local::env::var_os(var).filter(|value| !value.is_empty());
    let dir = set("CLADEFSD_PART_CACHE");
    let capacity = match set("CLADEFSD_PART_CACHE_BYTES") {
        None => u64::MAX,
        Some(_) if dir.is_none() => {
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
    Ok(dir.map(|dir| (dir.into(), capacity)))
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

/// cladefs: the part cache, as [`part_cache`] opens it, or why it can't be, naming the variable
/// that configures it and the cause (board 8595).
pub(super) fn open_part_cache(daemon: &Daemon) -> Result<Option<Arc<Cache>>, String> {
    let mut opened = daemon.part_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cache) = &*opened {
        return Ok(cache.clone());
    }
    let cache = match cache_config()? {
        Some((dir, capacity)) => match Cache::open(&dir, capacity) {
            Ok(cache) => Some(Arc::new(cache)),
            Err(err) => return Err(format!("CLADEFSD_PART_CACHE: {}: {err}", dir.display())),
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
        assert_eq!(cache_config(), Ok(Some((dir.path().to_owned(), u64::MAX))));
    }
}
