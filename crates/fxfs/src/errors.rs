// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/errors.rs`, without its conversions to `zx::Status` (`mod
// fuchsia`): cladefs maps these to errnos (fxfs-posix's `errno`). Nor are there errors of Fxfs's own
// encryption, which cladefs leaves to its extents and buckets.

use thiserror::Error;

#[derive(Eq, Error, Clone, Debug, PartialEq)]
pub enum FxfsError {
    #[error("Already exists")]
    AlreadyExists,
    #[error("Filesystem inconsistency")]
    Inconsistent,
    #[error("Internal error")]
    Internal,
    #[error("Expected directory")]
    NotDir,
    #[error("Expected file")]
    NotFile,
    #[error("Not found")]
    NotFound,
    #[error("Not empty")]
    NotEmpty,
    #[error("Read only filesystem")]
    ReadOnlyFilesystem,
    #[error("No space")]
    NoSpace,
    #[error("Deleted")]
    Deleted,
    #[error("Invalid arguments")]
    InvalidArgs,
    #[error("Too big")]
    TooBig,
    #[error("Invalid version")]
    InvalidVersion,
    #[error("Journal flush error")]
    JournalFlushError,
    #[error("Not supported")]
    NotSupported,
    #[error("Access denied")]
    AccessDenied,
    #[error("Out of range")]
    OutOfRange,
    #[error("Already bound")]
    AlreadyBound,
    #[error("Bad path")]
    BadPath,
    #[error("Wrong type")]
    WrongType,
    #[error("Data integrity error")]
    IntegrityError,
    #[error("Unavailable")]
    Unavailable,
}

impl FxfsError {
    /// A helper to match against this FxfsError against the root cause of an anyhow::Error.
    ///
    /// The main application of this helper is to allow us to match an anyhow::Error against a
    /// specific case of FxfsError in a boolean expression, such as:
    ///
    /// let result: Result<(), anyhow:Error> = foo();
    /// match result {
    ///   Ok(foo) => Ok(foo),
    ///   Err(e) if &FxfsError::NotFound.matches(e) => { ... }
    ///   Err(e) => Err(e)
    /// }
    pub fn matches(&self, error: &anyhow::Error) -> bool {
        if let Some(root_cause) = error.root_cause().downcast_ref::<FxfsError>() {
            self == root_cause
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FxfsError;
    use anyhow::{Context, anyhow};

    #[test]
    fn test_matches() {
        // We make heavy use of Context, so make sure that works.
        let err: anyhow::Error = FxfsError::AlreadyBound.into();
        let result: Result<(), anyhow::Error> = Err(err);
        let result = result.context("Foo");
        let err = result.err().unwrap();
        assert!(FxfsError::AlreadyBound.matches(&err));

        // `anyhow!` will plumb through source, so this should work just fine.
        let err = anyhow!(FxfsError::AlreadyBound).context("Foo");
        assert!(FxfsError::AlreadyBound.matches(&err));

        // `bail!(anyhow!(...).context("blah"))` is quite common and boils down to
        // `anyhow!(anyhow!(..))`, so check that too.
        let err = anyhow!(anyhow!(FxfsError::AlreadyBound).context("Foo"));
        assert!(FxfsError::AlreadyBound.matches(&err));
    }
}
