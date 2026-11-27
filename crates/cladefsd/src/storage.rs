// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's bucket and part-cache handles, owned by [`Daemon`], separated from cell
//! lifecycle and registry (`super::cells`).

use crate::Daemon;
use bucket_cell::any_bucket::AnyBucket;
use bucket_cell::cache::Cache;
use bucket_cell::object_bucket::{ObjectBucket, RoutedBucket};
use cladefs_core::Errno;
use futures::future::{BoxFuture, FutureExt as _};
use local_cell::fast_wal::{DEFAULT_LARGE_EXTENT, FastWalConfig, FastWalOpener};
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
    let host = cladefs_core::bucket::Host::from_env().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EIO
    })?;
    let bucket = cladefs_core::host_cell::bucket_on(&daemon.host, &host.bucket).map_err(|err| {
        log::error!("cladefsd: CLADEFSD_BUCKET: {err:#}");
        Errno::EIO
    })?;
    Ok(daemon
        .bucket
        .get_or_init(|| {
            crate::telemetry::watch_stats(Arc::downgrade(daemon), Arc::downgrade(&bucket));
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

/// Whether closed segments wake cell uploaders immediately without waiting for `upload_interval`:
/// true in production, false when a test paused background uploads via [`uploads_when_asked`].
pub(super) fn upload_closed() -> bool {
    #[cfg(test)]
    if UPLOADS_WHEN_ASKED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    true
}

/// Makes `bucket` `daemon`'s, before any cell opens: for tests that put themselves between the
/// daemon's cells and their objects. The cells naming `CLADEFSD_BUCKET`'s bucket open it
/// (`host_cell::use_bucket`).
#[cfg(any(test, madsim))]
pub(super) fn use_bucket_on(daemon: &Arc<Daemon>, bucket: impl Into<Arc<RoutedBucket>>) {
    let bucket = bucket.into();
    let host = cladefs_core::bucket::Host::from_env().expect("the daemon's host");
    cladefs_core::host_cell::use_bucket_on(&daemon.host, &host.bucket, bucket.clone());
    assert!(daemon.bucket.set(bucket.clone()).is_ok(), "the daemon has its bucket already");
    crate::telemetry::watch_stats(Arc::downgrade(daemon), Arc::downgrade(&bucket));
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

/// Opener for cross-AZ fast WAL buckets named in a prior epoch's `head.fast_wal.active` (doc-29).
/// Validates every URL against the host's explicit allowlist (`CLADEFSD_FAST_WAL_BUCKET` plus
/// optional `CLADEFSD_FAST_WAL_ALLOWED_BUCKETS`) before opening or probing so a corrupted or
/// hostile cold head can never direct takeover to an untrusted endpoint or local path.
struct UrlFastWalOpener {
    allowed: Vec<String>,
}

impl FastWalOpener for UrlFastWalOpener {
    fn is_allowed(&self, url: &str) -> Result<(), String> {
        if self.allowed.iter().any(|allowed| allowed == url) {
            return Ok(());
        }
        Err(format!(
            "refusing untrusted fast WAL bucket {url:?} from cell head (add to \
             CLADEFSD_FAST_WAL_ALLOWED_BUCKETS for cross-AZ takeover)"
        ))
    }

    fn open(&self, url: &str) -> BoxFuture<'static, Result<Arc<AnyBucket>, String>> {
        if let Err(msg) = self.is_allowed(url) {
            return async move { Err(msg) }.boxed();
        }
        let url = url.to_owned();
        async move {
            let bucket = Arc::new(ObjectBucket::from_url(&url)?);
            bucket.probed_append(rand::random()).await?;
            Ok(Arc::new(AnyBucket::new(bucket)))
        }
        .boxed()
    }
}

/// Parses `CLADEFSD_FAST_WAL_BUCKET`, optional `CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES`, and
/// optional comma-separated `CLADEFSD_FAST_WAL_ALLOWED_BUCKETS` (doc-29).
pub fn fast_wal_env() -> Result<Option<(String, u64, Vec<String>)>, String> {
    let set = |var| node_local::env::var_os(var).filter(|value| !value.is_empty());
    let bucket = match set("CLADEFSD_FAST_WAL_BUCKET") {
        Some(url) => match url.into_string() {
            Ok(url) => Some(url),
            Err(url) => {
                return Err(format!("CLADEFSD_FAST_WAL_BUCKET must be valid UTF-8: {url:?}"));
            }
        },
        None => None,
    };
    let large_extent = match set("CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES") {
        None => DEFAULT_LARGE_EXTENT,
        Some(_) if bucket.is_none() => {
            return Err(
                "CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES is set, but CLADEFSD_FAST_WAL_BUCKET names \
                 no bucket"
                    .to_owned(),
            );
        }
        // Not UTF-8, or not a number: refused below.
        Some(bytes) => match bytes.to_str().and_then(|bytes| bytes.parse().ok()) {
            Some(0) | None => {
                return Err(format!(
                    "CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES must be a positive number of bytes: \
                     {bytes:?}"
                ));
            }
            Some(bytes) => bytes,
        },
    };
    let mut allowed: Vec<String> = bucket.iter().cloned().collect();
    if let Some(extra) = set("CLADEFSD_FAST_WAL_ALLOWED_BUCKETS") {
        if bucket.is_none() {
            return Err(
                "CLADEFSD_FAST_WAL_ALLOWED_BUCKETS is set, but CLADEFSD_FAST_WAL_BUCKET names no \
                 bucket"
                    .to_owned(),
            );
        }
        let extra = extra.into_string().map_err(|raw| {
            format!("CLADEFSD_FAST_WAL_ALLOWED_BUCKETS must be valid UTF-8: {raw:?}")
        })?;
        for entry in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !allowed.iter().any(|existing| existing == entry) {
                allowed.push(entry.to_owned());
            }
        }
    }
    Ok(bucket.map(|url| (url, large_extent, allowed)))
}

/// Opens and append-probes `daemon`'s fast WAL bucket at boot, failing hard if the backend does
/// not honor offset-checked appends (doc-29).
pub(super) async fn open_fast_wal(daemon: &Daemon) -> Result<Option<(FastWalConfig, u64)>, String> {
    if let Some(found) =
        daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    {
        return Ok(found);
    }
    let Some((url, large_extent, allowed)) = fast_wal_env()? else {
        *daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(None);
        return Ok(None);
    };
    let bucket = Arc::new(
        ObjectBucket::from_url(&url)
            .map_err(|err| format!("CLADEFSD_FAST_WAL_BUCKET: {url}: {err}"))?,
    );
    bucket
        .probed_append(rand::random())
        .await
        .map_err(|err| format!("CLADEFSD_FAST_WAL_BUCKET: {url}: {err}"))?;
    let bucket = Arc::new(AnyBucket::new(bucket));
    let config = FastWalConfig::new(url, bucket, Arc::new(UrlFastWalOpener { allowed }));
    let mut slot = daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(slot.get_or_insert(Some((config, large_extent))).clone())
}

/// Returns `daemon`'s fast WAL configuration, opening and probing lazily if `boot_daemon` did not
/// run (e.g. control API tests).
pub(super) fn fast_wal_config(daemon: &Daemon) -> Result<Option<(FastWalConfig, u64)>, Errno> {
    if let Some(found) =
        daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    {
        return Ok(found);
    }
    let Some((url, large_extent, allowed)) = fast_wal_env().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EINVAL
    })?
    else {
        *daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(None);
        return Ok(None);
    };
    let bucket = Arc::new(ObjectBucket::from_url(&url).map_err(|err| {
        log::error!("cladefsd: CLADEFSD_FAST_WAL_BUCKET: {url}: {err}");
        Errno::EIO
    })?);
    let probed = local_cell::upload::block_on_host(
        daemon.host.local(),
        bucket.probed_append(rand::random()),
    )
    .map_err(|err| {
        log::error!("cladefsd: CLADEFSD_FAST_WAL_BUCKET: {err}");
        Errno::EIO
    })?;
    if let Err(err) = probed {
        log::error!("cladefsd: CLADEFSD_FAST_WAL_BUCKET: {url}: {err}");
        return Err(Errno::EIO);
    }
    let bucket = Arc::new(AnyBucket::new(bucket));
    let config = FastWalConfig::new(url, bucket, Arc::new(UrlFastWalOpener { allowed }));
    let mut slot = daemon.fast_wal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(slot.get_or_insert(Some((config, large_extent))).clone())
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
    }

    /// doc-29: a persisted `head.fast_wal.active.bucket` URL is untrusted until matched against the
    /// daemon's configured allowlist (`CLADEFSD_FAST_WAL_BUCKET` + `CLADEFSD_FAST_WAL_ALLOWED_BUCKETS`).
    #[test]
    fn url_fast_wal_opener_refuses_untrusted_bucket_urls() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let opener = UrlFastWalOpener {
            allowed: vec![
                "s3://fast-az1--use1-az4--x-s3".to_owned(),
                "s3://fast-az2--use1-az5--x-s3".to_owned(),
            ],
        };
        assert!(
            opener
                .is_allowed("file:///tmp/untrusted-fast-wal")
                .unwrap_err()
                .contains("refusing untrusted fast WAL bucket")
        );
        assert!(opener.is_allowed("s3://fast-az2--use1-az5--x-s3").is_ok());
        let err = rt
            .block_on(opener.open("file:///tmp/untrusted-fast-wal"))
            .err()
            .expect("untrusted local file URL must be refused");
        assert!(err.contains("refusing untrusted fast WAL bucket"), "err={err}");
        let err = rt
            .block_on(opener.open("s3://other-zone--use1-az6--x-s3"))
            .err()
            .expect("unlisted zonal bucket URL must be refused");
        assert!(err.contains("CLADEFSD_FAST_WAL_ALLOWED_BUCKETS"), "err={err}");

        // Verify `fast_wal_env` parses the allowlist and deduplicates the primary bucket.
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("CLADEFSD_FAST_WAL_BUCKET", "s3://fast-az1--use1-az4--x-s3");
            std::env::set_var(
                "CLADEFSD_FAST_WAL_ALLOWED_BUCKETS",
                "s3://fast-az1--use1-az4--x-s3, s3://fast-az2--use1-az5--x-s3 ,",
            );
            std::env::remove_var("CLADEFSD_FAST_WAL_LARGE_EXTENT_BYTES");
        }
        let parsed = fast_wal_env().unwrap().unwrap();
        assert_eq!(parsed.0, "s3://fast-az1--use1-az4--x-s3");
        assert_eq!(parsed.1, DEFAULT_LARGE_EXTENT);
        assert_eq!(
            parsed.2,
            vec![
                "s3://fast-az1--use1-az4--x-s3".to_owned(),
                "s3://fast-az2--use1-az5--x-s3".to_owned(),
            ]
        );
    }
}
