// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Rust client of a machined: over its Unix socket ([`connect_unix`]), or any tonic channel
//! (`MachinedClient::new`), TCP's with TLS included.

use std::path::Path;

pub use crate::proto::machined_client::MachinedClient;

/// A channel to the machined serving on the Unix socket `socket`: for its Machined service
/// ([`MachinedClient::new`]) and the Cladefsd service of its cladefs daemon alike.
pub async fn channel_unix(
    socket: impl AsRef<Path>,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    libcladefs::unix_channel(socket, "http://machined").await
}

/// A client of the machined serving on the Unix socket `socket`.
pub async fn connect_unix(
    socket: impl AsRef<Path>,
) -> Result<MachinedClient<tonic::transport::Channel>, tonic::transport::Error> {
    Ok(MachinedClient::new(channel_unix(socket).await?))
}
