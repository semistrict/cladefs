// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Generated gRPC and protobuf stubs for cladefs's control, peer, and mount services.

#![allow(clippy::all)]

/// Control API (`cladefs.cladefsd.v1`).
pub mod cladefsd_v1 {
    tonic::include_proto!("cladefs.cladefsd.v1");
}

/// Daemon-to-daemon peer API (`cladefs.peer.v1`).
pub mod peer_v1 {
    tonic::include_proto!("cladefs.peer.v1");
}

/// Multi-cell FUSE mount control API (`cladefs.mount.v1`).
pub mod mount_v1 {
    tonic::include_proto!("cladefs.mount.v1");
}

/// Static descriptors of every gRPC service and method defined in cladefs's `.proto` files.
pub mod services {
    /// A gRPC service of the daemon's `.protos`: its full name (`PACKAGE.SERVICE`) and its
    /// methods, each by path (`/PACKAGE.SERVICE/METHOD`).
    #[derive(Clone, Copy, Debug)]
    pub struct Service {
        pub name: &'static str,
        pub methods: &'static [&'static str],
    }

    include!(concat!(env!("OUT_DIR"), "/grpc_services.rs"));
}
