// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::control;
use crate::control::proto::cladefsd_client::CladefsdClient;
use std::path::Path;
use tonic::transport::Channel;

/// The runtime and channel owned by a synchronous test of a daemon's control API.
pub struct Control {
    runtime: tokio::runtime::Runtime,
    client: CladefsdClient<Channel>,
}

impl Control {
    /// Connects the test to the daemon's control socket.
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self, cladefs_core::Error> {
        let socket = socket.as_ref();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(cladefs_core::Error::new)?;
        let client =
            runtime.block_on(control::connect(socket)).map_err(cladefs_core::Error::new)?;
        Ok(Self { runtime, client })
    }

    /// Drives the test's asynchronous tonic call and returns its response or its daemon errno.
    pub fn call<T>(
        &self,
        call: impl AsyncFnOnce(
            &mut CladefsdClient<Channel>,
        ) -> Result<tonic::Response<T>, tonic::Status>,
    ) -> Result<T, cladefs_core::Error> {
        let mut client = self.client.clone();
        self.runtime
            .block_on(call(&mut client))
            .map(tonic::Response::into_inner)
            .map_err(|status| control::failed("calling the extent daemon".to_owned(), status))
    }
}

/// A fixture's path as a control request names it.
pub fn control_path(path: &Path) -> String {
    path.to_str().expect("a test control path is UTF-8").to_owned()
}
