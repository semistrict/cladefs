// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libmachine's guest agent: listens on a vsock port and serves each
//! connection's one exchange (the protocol is the library's, `lib.rs`):
//! running a command (or taking one up again after a reset), reading a
//! file, writing one. Commands live apart from their connections
//! (`jobs.rs` keeps their output for a host that attaches again).
//!
//!   libmachine-agent [--port PORT]     serve (default port 100)
//!   libmachine-agent connect PORT      copy stdin to the host's PORT
//!                                      (CID 2) and what comes back to
//!                                      stdout, for testing guest-initiated
//!                                      connections

#[cfg(target_os = "linux")]
mod agent;
#[cfg(target_os = "linux")]
mod jobs;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(agent::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("libmachine-agent runs in Linux guests only");
    std::process::exit(1);
}
