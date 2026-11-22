// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a process's exclusive lock of a file, as `flock` takes it, which the process's death
//! lets go (doc-24, TASK-703). Under madsim, the simulated host's disk's: a host started again,
//! its disk crashed, holds none, as a process booting holds none of its predecessor's; and a lock
//! held, through another opening of the file, refuses another, as another open file description's
//! `flock` does.

use crate::File;
use std::io;

/// An exclusive lock of a file, held until it is dropped (see the module's docs).
#[derive(Debug)]
pub struct Flock {
    file: File,
}

impl Flock {
    /// Locks `file` exclusively, as `flock(LOCK_EX | LOCK_NB)` does: `WouldBlock` if another
    /// holds a lock of it.
    pub fn try_lock(file: File) -> io::Result<Self> {
        #[cfg(madsim)]
        file.lock_exclusive()?;
        #[cfg(not(madsim))]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: an flock of an open file.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { file })
    }

    /// The file it locks.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Unlocks the file, as closing its last descriptor would: explicitly, as another descriptor of
/// its open file description, a `dup` of it, may outlive this one.
impl Drop for Flock {
    fn drop(&mut self) {
        #[cfg(madsim)]
        self.file.unlock();
        #[cfg(not(madsim))]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: an flock of an open file.
            let unlocked = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
            // Of a descriptor open, which it is, it fails at nothing but a signal, which its
            // closing, next, unlocks through.
            debug_assert!(
                unlocked == 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            );
        }
    }
}
