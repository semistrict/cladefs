// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Rust client of a machined: over its Unix socket ([`connect_unix`]), or any tonic channel
//! (`MachinedClient::new`), TCP's with TLS included.

use std::path::Path;

pub use crate::proto::machined_client::MachinedClient;

/// A client of the machined serving on the Unix socket `socket`.
pub async fn connect_unix(
    socket: impl AsRef<Path>,
) -> Result<MachinedClient<tonic::transport::Channel>, tonic::transport::Error> {
    let socket = socket.as_ref().to_owned();
    // The URI names no host: the connector connects to the socket, whatever it asks for.
    let channel = tonic::transport::Endpoint::from_static("http://machined")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(MachinedClient::new(channel))
}
