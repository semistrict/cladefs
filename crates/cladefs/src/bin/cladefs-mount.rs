// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Mounts a store's filesystem on the host with FUSE (`cladefs::fuse`, TASK-222): an image, an
//! imported base, or a VM's store, which nothing else has open, read and written as any
//! filesystem is. Tenants' cells, many in one mount, are cladefsd's to mount (`CLADEFSD_MOUNT`,
//! TASK-475.2), which `cladefs-mount --control` did before.
//!
//! ```sh
//! cladefs-mount [--allow-other] [--new CAPACITY] STORE MOUNTPOINT
//! ```
//!
//! STORE is a store's directory, whose filesystem's `default` volume is mounted at MOUNTPOINT, a
//! directory. With `--new`, STORE, which must not exist, is made a new store of CAPACITY bytes, its
//! filesystem empty, as a scratch filesystem's or a test's (CI's pjdfstest, TASK-149.38): its
//! branch kept in a file bucket in its directory, or in the host's bucket on a host of cladefs's
//! system (`CLADEFSD_BUCKET`), as `cladefs::Storage::create` keeps it.
//!
//! Mounted, it prints MOUNTPOINT on standard output, and serves until it is unmounted: by `umount`
//! or `fusermount3 -u`, or lazily on SIGINT or SIGTERM, once nothing uses it. Then it closes the
//! store, writing back what is dirty, and exits 0.
//!
//! Root mounts with mount(2), any other user through libfuse's `fusermount3`. With
//! `--allow-other`, users other than the mounting one may use it, as its permissions say: for a
//! user other than root, `/etc/fuse.conf` must have `user_allow_other`.
//!
//! A store is the mount's alone while it is mounted: cladefsd serving it, or a second mount, fails
//! to open it, as the mount fails to open one in use. Mounting replays the filesystem's journal,
//! which writes to the store: to look at a VM's store and leave it be, mount a fork of it
//! (`cladefsctl fork`).

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
const USAGE: &str = "usage: cladefs-mount [--allow-other] [--new CAPACITY] STORE MOUNTPOINT";

/// Mounts a store as the arguments say, and serves it until it is unmounted.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::path::PathBuf;
    cladefs::renamed::refuse_old_variables().map_err(anyhow::Error::msg)?;
    cladefs::cladefsd::logging::to_stderr()?;
    // Before any thread is started, which would take the signals otherwise.
    cladefs::stacks::on_sigquit().context("handling SIGQUIT")?;
    let stop = cladefs::signals::Signals::block(&[libc::SIGINT, libc::SIGTERM])
        .context("handling SIGINT and SIGTERM")?;
    // cladefs: every store it serves is encrypted under the host's KMS: none chosen, it refuses to
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
                let capacity = args.next().and_then(|capacity| capacity.to_str()?.parse().ok());
                new = Some(capacity.with_context(|| format!("--new CAPACITY, in bytes? {USAGE}"))?);
            }
            Some(flag) if flag.starts_with("--") => anyhow::bail!("{flag}? {USAGE}"),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    let [store, at] = <[_; 2]>::try_from(paths).map_err(|_| anyhow::anyhow!(USAGE))?;
    let fs = match new {
        Some(capacity) => {
            let storage = cladefs::Storage::create(&store, capacity)
                .with_context(|| format!("making {}", store.display()))?;
            futures::executor::block_on(cladefs::Filesystem::format(storage))
                .with_context(|| format!("formatting {}", store.display()))?
        }
        None => {
            let storage = cladefs::Storage::open(&store)
                .with_context(|| format!("opening {}", store.display()))?;
            futures::executor::block_on(cladefs::Filesystem::mount(storage))
                .with_context(|| format!("mounting {}'s filesystem", store.display()))?
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

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cladefs-mount runs on Linux only: macOS has no open-source FUSE");
    std::process::exit(2);
}
