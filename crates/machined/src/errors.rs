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
        Error::Boot(_) => Code::FailedPrecondition,
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => Code::NotFound,
        Error::MemoryAllocation(_) => Code::ResourceExhausted,
        // Its cell couldn't be opened or served: held by another machine, or the daemon gone.
        Error::ExtentStore(_) => Code::FailedPrecondition,
        Error::ExtentDaemonLost(_) => Code::Unavailable,
        _ => Code::Internal,
    };
    Status::new(code, e.to_string())
}
