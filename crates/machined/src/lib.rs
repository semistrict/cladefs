// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined: the server libmachine is for (board 13467). One process runs many machines, each a
//! Linux guest on libmachine (`crates/libmachine`, doc-30), and serves a gRPC API to make, start
//! and delete them, and stream their consoles (`proto/machined/v1/machined.proto`; the README
//! says how to run it).
//!
//! - [`start`] serves it, as [`Config`] says, on a Unix socket, TCP with mutual TLS, or both
//!   (`listen`); its binary (`main.rs`) is a thin one over it.
//! - Every request is checked once, where it enters (`checks`), then done on the machines
//!   (`machines`), each of which runs through machined's one seam over libmachine (`vmm`): a
//!   helper process per machine (board 13460).
//! - A machine's guest's vsock is reached through a Unix socket speaking Firecracker's protocol
//!   (`vsock`); its console is kept and streamed with offsets (`console`).
//! - [`client`] connects a Rust client to a machined's Unix socket.
//!
//! Machines' roots are cells of the extent daemon machined embeds (libcladefs, board 13471), whose
//! Cladefsd service machined serves on its endpoint.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

/// The API's messages and service, generated from its .proto.
pub mod proto {
    tonic::include_proto!("machined.v1");
}

mod checks;
pub mod client;
mod config;
mod console;
mod errors;
#[doc(hidden)]
pub mod helper;
mod listen;
mod machines;
mod service;
mod vmm;
mod vsock;

pub use config::{CladefsConfig, Config, Tcp};
pub use listen::{Server, start};
