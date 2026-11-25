// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Mounts a cell's filesystem on the host with FUSE (`cladefs::fuse`, TASK-222): an image, an
//! imported base, or a VM's cell, which nothing else has open, read and written as any
//! filesystem is. Tenants' cells, many in one mount, are cladefsd's to mount (`CLADEFSD_MOUNT`,
//! TASK-475.2), which `cladefs-mount --control` did before.
//!
//! ```sh
//! cladefs-mount [--allow-other] [--new CAPACITY] CELL_DIR MOUNTPOINT
//! ```
//!
//! CELL_DIR is a cell directory, whose filesystem's `default` volume is mounted at MOUNTPOINT, a
//! directory. With `--new`, CELL_DIR, which must not exist, is made a new cell of CAPACITY bytes,
//! its filesystem empty, as a scratch filesystem's or a test's (CI's pjdfstest, TASK-149.38): its
//! cell kept in a file bucket in its directory, or in the host's bucket on a host of cladefs's
//! system (`CLADEFSD_BUCKET`), as `cladefs::Storage::create` keeps it.
//!
//! Mounted, it prints MOUNTPOINT on standard output, and serves until it is unmounted: by `umount`
//! or `fusermount3 -u`, or lazily on SIGINT or SIGTERM, once nothing uses it. Then it closes the
//! cell, writing back what is dirty, and exits 0.
//!
//! Root mounts with mount(2), any other user through libfuse's `fusermount3`. With
//! `--allow-other`, users other than the mounting one may use it, as its permissions say: for a
//! user other than root, `/etc/fuse.conf` must have `user_allow_other`.
//!
//! A cell is the mount's alone while it is mounted: cladefsd serving it, or a second mount, fails
//! to open it, as the mount fails to open one in use. Mounting replays the filesystem's journal,
//! which writes to the cell: to look at a VM's cell and leave it be, mount a fork of it
//! (`cladefsctl fork`).

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

// cladefs: Linux release binaries of cladefs-mount are static musl builds only (with mimalloc).
#[cfg(all(target_os = "linux", not(target_env = "musl"), not(debug_assertions)))]
compile_error!(
    "Linux release builds of cladefs-mount must target musl (use scripts/build-musl.sh or \
     --target <arch>-unknown-linux-musl)"
);

// cladefs: musl's default mallocng allocator contends heavily under multi-threaded FUSE serving;
// mimalloc uses lock-free per-thread heaps and detects OS page size at runtime.
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(target_os = "linux", not(madsim)))]
const USAGE: &str = "usage: cladefs-mount [--allow-other] [--new CAPACITY] CELL_DIR MOUNTPOINT";

/// Mounts a cell as the arguments say, and serves it until it is unmounted.
#[cfg(all(target_os = "linux", not(madsim)))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::path::PathBuf;
    cladefs::cladefsd::logging::to_stderr()?;
    // Give back freed 1 MiB part/segment buffers promptly and size standalone `cladefs-mount`'s
    // unpacked-part cache above the 64 MiB default when host RAM allows (board 12313).
    cladefs::cladefsd::memory::return_large_allocations();
    match cladefs::cladefsd::governor::host_memory() {
        Ok(host) => {
            let limit = (host.available / 4).clamp(bucket_cell::fetch::RECENT, 1 << 30);
            bucket_cell::fetch::shared_parts().size_to(limit);
        }
        Err(err) => {
            log::warn!("cladefs-mount: reading host memory for part cache sizing: {err}");
        }
    }
    // Before any thread is started, which would take the signals otherwise.
    cladefs::stacks::on_sigquit().context("handling SIGQUIT")?;
    let stop = cladefs::signals::Signals::block(&[libc::SIGINT, libc::SIGTERM])
        .context("handling SIGINT and SIGTERM")?;
    // cladefs: every cell it serves is encrypted under the host's KMS: none chosen, it refuses to
    // start, naming CLADEFS_KMS and CLADEFS_MASTER_KEY (TASK-411.28). After the signals are
    // blocked: choosing it may start threads (a KMS's runtime), which would take SIGTERM and
    // die of it rather than leave it to the thread waiting for it.
    cladefs::bucket::require_kms().map_err(anyhow::Error::msg)?;
    let mut options = cladefs::fuse::MountOptions::new();
    let (mut paths, mut new) = (Vec::new(), None);
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--allow-other") => {
                options.allow_other(true);
            }
            Some("--new") => {
                // Not a number, or none: the error below says what one is.
                let capacity = args.next().and_then(|capacity| capacity.to_str()?.parse().ok());
                new = Some(capacity.with_context(|| format!("--new CAPACITY, in bytes? {USAGE}"))?);
            }
            Some(flag) if flag.starts_with("--") => anyhow::bail!("{flag}? {USAGE}"),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    let [cell_dir, at] = <[_; 2]>::try_from(paths).map_err(|_| anyhow::anyhow!(USAGE))?;
    let fs = match new {
        Some(capacity) => {
            // The bin's outermost entry: waits on the bucket runtime (board 2948).
            let storage =
                local_cell::upload::wait_on(cladefs::Storage::create(&cell_dir, capacity))
                    .with_context(|| format!("making {}", cell_dir.display()))?;
            futures::executor::block_on(cladefs::Filesystem::format(storage))
                .with_context(|| format!("formatting {}", cell_dir.display()))?
        }
        None => {
            let storage = local_cell::upload::wait_on(cladefs::Storage::open(&cell_dir))
                .with_context(|| format!("opening {}", cell_dir.display()))?;
            futures::executor::block_on(cladefs::Filesystem::mount(storage))
                .with_context(|| format!("mounting {}'s filesystem", cell_dir.display()))?
        }
    };
    let mount =
        options.mount(fs, &at).with_context(|| format!("mounting it at {}", at.display()))?;
    let unmounter = mount.unmounter();
    cladefs::signals::spawn_taking_no_signals("cladefs-mount signals", move || {
        match stop.wait() {
            Ok(signal) => {
                log::info!("cladefs-mount: unmounting, on signal {signal}, once nothing uses it");
                if let Err(error) = unmounter.unmount(true) {
                    log::error!("cladefs-mount: {error:#}");
                }
            }
            Err(error) => log::error!("cladefs-mount: waiting for SIGINT or SIGTERM: {error}"),
        }
    })?;
    log::info!("cladefs-mount: mounted at {}", at.display());
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", at.display())?;
    stdout.flush()?;
    drop(stdout);

    mount.join().map(drop).context("closing the filesystem")?;
    log::info!("cladefs-mount: {} unmounted and closed", at.display());
    Ok(())
}

#[cfg(any(not(target_os = "linux"), madsim))]
fn main() {
    eprintln!("cladefs-mount runs on Linux only (not under madsim)");
    std::process::exit(2);
}
