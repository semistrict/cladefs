// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: cgroups of the cases' own (board 14529), as a production host gives each VM process
//! one, and each host's daemon its service's: each a sibling of the run's own cgroup, under its
//! parent, which scripts/test-e2e.sh gives the run's user (`sudo cladefsd --setup-host` beside it),
//! as systemd delegates one. A process goes into one as it starts, before it execs
//! ([`Cgroup::enter_on_exec`]), so that it is never counted in the run's.
//!
//! - Each VM process (`crate::vm`): its memory a cgroup's alone, which the extent daemon's governor
//!   reclaims as a production host's (`governor::reclaim_connected_vms`); in the daemon's own cgroup,
//!   as every VM process of a run was before, the governor reclaims none (board 14533).
//! - Each host's daemon of a case that stands several hosts on one machine ([`crate::common`]'s
//!   `spawn_host_daemon`): governed, as production's, within its host's memory, the cgroup's
//!   `memory.max`, rather than each taking the machine's free memory as its own.
//!
//! A cgroup is left as the case ends, empty, its processes gone: in a run's machine, which goes
//! with the run (a hotbed command's VM, a CI runner).

use anyhow::Context;
use std::path::{Path, PathBuf};

/// A cgroup of a case's.
pub struct Cgroup {
    path: PathBuf,
    procs: std::ffi::CString,
}

impl Cgroup {
    /// A new cgroup, `name`, beside this process's, of `memory_max` bytes of memory if given.
    pub fn new(name: &str, memory_max: Option<u64>) -> anyhow::Result<Self> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Cgroups made by this process, for names its others' don't take.
        static MADE: AtomicUsize = AtomicUsize::new(0);

        let parent = run_parent()?;
        let made = MADE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("cladefs-{}-{made}-{name}", std::process::id()));
        std::fs::create_dir(&path).with_context(|| {
            format!(
                "making the cgroup {} (scripts/test-e2e.sh gives the run's user its cgroup's \
                 parent)",
                path.display()
            )
        })?;
        if let Some(bytes) = memory_max {
            let max = path.join("memory.max");
            std::fs::write(&max, bytes.to_string())
                .with_context(|| format!("writing {}", max.display()))?;
        }
        let procs = path.join("cgroup.procs");
        let procs = std::ffi::CString::new(procs.into_os_string().into_encoded_bytes())?;
        Ok(Self { path, procs })
    }

    /// Its directory, under /sys/fs/cgroup.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Has the process `command` starts go into it before it execs, so that all it is, and all it
    /// starts, are counted in it from the first.
    pub fn enter_on_exec(&self, command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        let procs = self.procs.clone();
        // SAFETY: open, write and close are async-signal-safe; the path was made before.
        unsafe {
            command.pre_exec(move || {
                let fd =
                    nix::libc::open(procs.as_ptr(), nix::libc::O_WRONLY | nix::libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // "0": the process writing it.
                let written = nix::libc::write(fd, c"0".as_ptr().cast(), 1);
                let error = std::io::Error::last_os_error();
                nix::libc::close(fd);
                if written != 1 {
                    return Err(error);
                }
                Ok(())
            })
        };
    }
}

/// The parent of this process's cgroup (cgroup v2's, `0::PATH` of /proc/self/cgroup), under which
/// a case's cgroups go, its memory controller enabled for them.
fn run_parent() -> anyhow::Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    let own = (own.lines().find_map(|line| line.strip_prefix("0::")))
        .context("this process is in no cgroup v2")?;
    let own = own.trim_start_matches('/');
    anyhow::ensure!(!own.is_empty(), "this process's cgroup is the root, which has no parent");
    let own = Path::new("/sys/fs/cgroup").join(own);
    let parent = own.parent().context("this process's cgroup has no parent")?.to_owned();
    let control = parent.join("cgroup.subtree_control");
    let enabled = std::fs::read_to_string(&control)
        .with_context(|| format!("reading {}", control.display()))?;
    if !enabled.split_whitespace().any(|controller| controller == "memory") {
        std::fs::write(&control, "+memory")
            .with_context(|| format!("enabling memory in {}", control.display()))?;
    }
    Ok(parent)
}
