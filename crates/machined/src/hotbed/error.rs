// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! hotbed's errors: a code of hotbed's own (`INVALID_ARGS`, `NOT_FOUND`, ...: hotbed's
//! `internal/api/error.go`), a message, a suggestion, and whether calling again may work. They go
//! up the tunnel as they are (`rpc_err`), and come from the controller as Connect's JSON errors,
//! hotbed's code in their `hotbed.v1.ErrorInfo` detail (hotbed's `connectJSONToTunnelErr`).

use base64::Engine as _;
use prost::Message as _;
use tonic::{Code, Status};

use super::proto;

pub(crate) const INVALID_ARGS: &str = "INVALID_ARGS";
pub(crate) const CONTROLLER_UNREACHABLE: &str = "CONTROLLER_UNREACHABLE";
pub(crate) const NOT_AUTHORIZED: &str = "NOT_AUTHORIZED";
pub(crate) const NOT_JOINED: &str = "NOT_JOINED";
pub(crate) const CLUSTER_MISMATCH: &str = "CLUSTER_MISMATCH";
pub(crate) const PLATFORM_NOT_SUPPORTED: &str = "PLATFORM_NOT_SUPPORTED";
pub(crate) const NOT_FOUND: &str = "NOT_FOUND";
pub(crate) const CONFLICT: &str = "CONFLICT";
pub(crate) const WORKER_UNREACHABLE: &str = "WORKER_UNREACHABLE";
pub(crate) const COMMAND_STOPPED: &str = "COMMAND_STOPPED";
pub(crate) const INTERNAL: &str = "INTERNAL";

/// A hotbed error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// hotbed's code: one of `internal/api/error.go`'s, or another a newer controller sent.
    pub code: String,
    pub message: String,
    pub suggestion: String,
    pub retryable: bool,
}

impl Error {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            suggestion: String::new(),
            retryable: false,
        }
    }

    pub(crate) fn suggest(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = suggestion.into();
        self
    }

    pub(crate) fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// The error of a Connect call that failed with HTTP status `status` and `body`: hotbed's
    /// code from the body's ErrorInfo detail, else Connect's code made hotbed's; a body that is not
    /// Connect's JSON is INTERNAL, its text (or `status`) the message.
    pub(crate) fn of_connect(body: &[u8], status: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct Detail {
            #[serde(rename = "type", default)]
            kind: String,
            #[serde(default)]
            value: String,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            code: String,
            #[serde(default)]
            message: String,
            #[serde(default)]
            details: Vec<Detail>,
        }
        let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
            let text = String::from_utf8_lossy(body).trim().to_owned();
            return Self::new(INTERNAL, if text.is_empty() { status.to_owned() } else { text });
        };
        let mut error = Self::new(connect_code(&parsed.code), parsed.message);
        let info =
            parsed.details.iter().filter(|d| d.kind == "hotbed.v1.ErrorInfo").find_map(|d| {
                // Connect's JSON writes a detail's bytes unpadded; take them padded too.
                let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(d.value.trim_end_matches('='))
                    .ok()?;
                proto::ErrorInfo::decode(bytes.as_slice()).ok().filter(|info| !info.code.is_empty())
            });
        if let Some(info) = info {
            error.code = info.code;
            error.suggestion = info.suggestion;
            error.retryable = info.retryable;
        }
        if error.message.is_empty() {
            status.clone_into(&mut error.message);
        }
        error
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if !self.suggestion.is_empty() {
            write!(f, " ({})", self.suggestion)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// Connect's code, as a hotbed code (hotbed's `connectCodeStringToHotbed`).
fn connect_code(code: &str) -> &'static str {
    match code {
        "invalid_argument" => INVALID_ARGS,
        "permission_denied" | "unauthenticated" => NOT_AUTHORIZED,
        "not_found" => NOT_FOUND,
        "already_exists" => CONFLICT,
        "unavailable" | "deadline_exceeded" => WORKER_UNREACHABLE,
        "unimplemented" => PLATFORM_NOT_SUPPORTED,
        "aborted" => COMMAND_STOPPED,
        _ => INTERNAL,
    }
}

/// machined's statuses, as hotbed's errors: what a machine can't do now (machines full, its guest
/// not answering) is WORKER_UNREACHABLE and retryable, which sends the controller's placement to
/// another node.
impl From<Status> for Error {
    fn from(status: Status) -> Self {
        let code = match status.code() {
            Code::InvalidArgument | Code::OutOfRange => INVALID_ARGS,
            Code::NotFound => NOT_FOUND,
            Code::AlreadyExists | Code::FailedPrecondition => CONFLICT,
            Code::PermissionDenied | Code::Unauthenticated => NOT_AUTHORIZED,
            Code::ResourceExhausted | Code::Unavailable | Code::DeadlineExceeded => {
                return Self::new(WORKER_UNREACHABLE, status.message()).retryable();
            }
            Code::Unimplemented => PLATFORM_NOT_SUPPORTED,
            Code::Aborted => COMMAND_STOPPED,
            _ => INTERNAL,
        };
        Self::new(code, status.message())
    }
}
