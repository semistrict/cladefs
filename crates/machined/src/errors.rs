// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libmachine's errors as the API's statuses (machined.proto's header comment).

use libmachine::Error;
use tonic::{Code, Status};

/// The status of libmachine's error `e`.
pub(crate) fn status(e: Error) -> Status {
    let code = match &e {
        Error::InvalidConfig(_) => Code::InvalidArgument,
        Error::Boot(_) | Error::CpuModel(_) | Error::Snapshot(_) | Error::Busy(_) => {
            Code::FailedPrecondition
        }
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => Code::NotFound,
        Error::MemoryAllocation(_) => Code::ResourceExhausted,
        // The guest stopped, or the machine is gone: nothing in it answers.
        Error::GuestGone(_) => Code::FailedPrecondition,
        Error::TimedOut(_) => Code::DeadlineExceeded,
        Error::ConnectionReset(_) => Code::Aborted,
        Error::Agent(_) => Code::Unavailable,
        // Its cell couldn't be opened or served: held by another machine, or the daemon gone.
        Error::ExtentStore(_) => Code::FailedPrecondition,
        Error::ExtentDaemonLost(_) => Code::Unavailable,
        Error::Guest { source, .. } => guest(source.raw_os_error()),
        _ => Code::Internal,
    };
    Status::new(code, e.to_string())
}

/// The status of an errno an operation in the guest failed with: Linux's numbering, whatever the
/// host's.
fn guest(errno: Option<i32>) -> Code {
    // Linux's errnos (asm-generic/errno-base.h, errno.h).
    const EPERM: i32 = 1;
    const ENOENT: i32 = 2;
    const ENOMEM: i32 = 12;
    const EACCES: i32 = 13;
    const EEXIST: i32 = 17;
    const EINVAL: i32 = 22;
    const EFBIG: i32 = 27;
    const ENOSPC: i32 = 28;
    const ENAMETOOLONG: i32 = 36;
    match errno {
        Some(ENOENT) => Code::NotFound,
        Some(EEXIST) => Code::AlreadyExists,
        Some(EACCES | EPERM) => Code::PermissionDenied,
        Some(EINVAL | ENAMETOOLONG) => Code::InvalidArgument,
        Some(EFBIG | ENOSPC | ENOMEM) => Code::ResourceExhausted,
        _ => Code::Unknown,
    }
}
