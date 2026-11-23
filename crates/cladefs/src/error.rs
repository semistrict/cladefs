// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Errors: the errno values a guest is given, and the errors of setting a filesystem up.

use std::fmt;

/// A Linux errno value, as a guest's Linux kernel understands it: Fxfs's VFSs' own, which gives
/// the errnos of Fxfs's errors (`fxfs_posix::errno`).
pub use fxfs_posix::Errno;

/// The result of a filesystem operation, or with `E`, of setting one up.
pub type Result<T, E = Errno> = std::result::Result<T, E>;

/// An error setting up, checking or tearing down a filesystem.
pub struct Error(anyhow::Error);

impl Error {
    pub(crate) fn new(error: impl Into<anyhow::Error>) -> Self {
        Self(error.into())
    }

    pub(crate) fn msg(message: &'static str) -> Self {
        Self(anyhow::Error::msg(message))
    }

    /// The error, saying what failed with it: its errno stays its own.
    pub(crate) fn context(self, context: impl fmt::Display + Send + Sync + 'static) -> Self {
        Self(self.0.context(context))
    }

    /// The errno that caused the error, if one did: `ENOTSUP` for a cell of another format, an
    /// older one included, which is refused (`local_cell::Format`, TASK-165.6); `ENOENT` for one
    /// its maker never made whole, which is no cell (`local_cell::Unmade`, TASK-196); `ENOSPC` for
    /// one whose making its disk or its tenant's limits refused (`local_cell::footprint::DiskFull`,
    /// doc-22).
    pub fn errno(&self) -> Option<Errno> {
        // An errno added as context is found by downcasting the error, not its causes.
        let errno = self.0.downcast_ref::<Errno>().copied();
        errno
            .or_else(|| self.0.chain().find_map(|cause| cause.downcast_ref::<Errno>()).copied())
            .or_else(|| {
                self.0
                    .chain()
                    .any(|cause| cause.is::<local_cell::Format>())
                    .then_some(Errno::ENOTSUP)
            })
            .or_else(|| {
                (self.0.chain())
                    .any(|cause| cause.is::<local_cell::Unmade>())
                    .then_some(Errno::ENOENT)
            })
            .or_else(|| {
                (self.0.chain())
                    .any(|cause| cause.is::<local_cell::footprint::DiskFull>())
                    .then_some(Errno::ENOSPC)
            })
    }

    /// Whether what failed waited for something busy and gave up: another process's lock of a
    /// cell (`local_cell`'s, which an extent daemon serving it holds).
    #[cfg(any(target_os = "linux", madsim))]
    pub(crate) fn is_busy(&self) -> bool {
        self.0.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::WouldBlock)
        })
    }
}

/// cladefs: an uploader's failure, or an opening's (`local_cell::upload`): its errno, as cladefs's
/// `bucket` maps it, as context; this host's own as the error it is, whose causes say its errno.
impl From<local_cell::upload::Failure> for Error {
    fn from(failure: local_cell::upload::Failure) -> Self {
        use local_cell::upload::Failure;
        match failure {
            Failure::Local(error) => Self(
                std::sync::Arc::try_unwrap(error)
                    .unwrap_or_else(|error| anyhow::anyhow!("{error:#}")),
            ),
            failure => {
                let errno = crate::bucket::errno(&failure);
                let error = anyhow::Error::new(failure);
                Self(match errno {
                    Some(errno) => error.context(errno),
                    None => error,
                })
            }
        }
    }
}

impl From<crate::faults::Failpoint> for Error {
    fn from(failpoint: crate::faults::Failpoint) -> Self {
        Self::new(anyhow::Error::new(failpoint))
    }
}

impl From<Errno> for Error {
    fn from(errno: Errno) -> Self {
        Self::new(errno)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Error {
    /// The error, without its sources; `{:#}` includes them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}
