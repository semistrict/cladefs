// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the mount of a daemon that mounts no cells ([`super`]): built without its `mount`
//! feature, under madsim, or not on Linux. Asked for a mount, it refuses, saying which.

use super::Config;
use crate::Daemon;
use cladefs_core::error::Error;

/// Why this daemon mounts no cells.
const WHY: &str = if cfg!(madsim) {
    "FUSE is outside the simulation"
} else if cfg!(not(target_os = "linux")) {
    "the daemon's FUSE mount is Linux's"
} else {
    "this daemon is built without its FUSE mount (cladefsd's `mount` feature)"
};

/// Per-daemon FUSE mount state, owned by [`Daemon`]: none.
#[derive(Default)]
pub(crate) struct State;

/// What a mount's server did (`cladefs_fuse::Counts`), for the daemon's metrics: nothing.
#[derive(Default)]
pub(crate) struct Counts {
    pub(crate) requests: u64,
    pub(crate) errors: u64,
    pub(crate) attaches: u64,
    pub(crate) detaches: u64,
}

/// Refused, saying [`WHY`]: a mount asked for (`CLADEFSD_MOUNT`, the configuration's `[mount]`) is
/// never left out silently.
pub(crate) fn start_on(_daemon: &Daemon, config: &Config, _: String) -> Result<(), Error> {
    Err(Error::new(anyhow::anyhow!("mounting no cells at {}: {WHY}", config.mountpoint.display())))
}

/// The Mount service of a daemon that mounts no cells, on its control socket as a mount's is:
/// every call refused, `UNIMPLEMENTED`, saying [`WHY`] (hotbed question 603). cladefs: outside the
/// simulation, whose control API serves no Mount service.
#[cfg(not(madsim))]
pub(crate) fn service_on(
    _daemon: &std::sync::Arc<Daemon>,
) -> Option<
    cladefs_proto::mount_v1::mount_server::MountServer<
        impl cladefs_proto::mount_v1::mount_server::Mount + use<>,
    >,
> {
    use cladefs_proto::mount_v1::*;
    use tonic::{Request, Response, Status};

    struct Refusing;

    fn refused<T>() -> Result<Response<T>, Status> {
        Err(Status::unimplemented(format!("mounting no cells: {WHY}")))
    }

    #[tonic::async_trait]
    impl mount_server::Mount for Refusing {
        async fn attach(
            &self,
            _: Request<AttachRequest>,
        ) -> Result<Response<AttachResponse>, Status> {
            refused()
        }

        async fn detach(
            &self,
            _: Request<DetachRequest>,
        ) -> Result<Response<DetachResponse>, Status> {
            refused()
        }

        async fn list(&self, _: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
            refused()
        }
    }

    Some(mount_server::MountServer::new(Refusing))
}

/// What `daemon`'s mount of cells holds and did, for its metrics: nothing, all 0.
pub(crate) fn counts_on(_daemon: &Daemon) -> (usize, usize, Counts) {
    (0, 0, Counts::default())
}

/// Bytes of FUSE requests queued on `daemon`: none.
pub(crate) fn queued_bytes_on(_daemon: &Daemon) -> u64 {
    0
}

/// Whether a drain past its bound left `daemon`'s mount: it has none.
pub(crate) fn left_on(_daemon: &Daemon) -> bool {
    false
}

/// Nothing to close: `daemon` mounts no cells ([`start_on`]).
pub(crate) async fn close_on(_daemon: &Daemon) -> Result<(), Error> {
    Ok(())
}

/// The cells mounted in this process: none.
pub(crate) fn live_cells() -> Vec<cladefs_core::CellId> {
    Vec::new()
}
