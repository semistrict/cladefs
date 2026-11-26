// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! WorkspaceService's calls, by procedure (`/hotbed.v1.WorkspaceService/GetWorkspace`): a call's
//! request decoded, its handler run, its answers encoded. Which calls there are, and which stream,
//! is hotbed's proto's, read from its descriptors (build.rs): a call machined has no handler for
//! is refused as hotbed's worker refuses one it doesn't implement (Connect's `unimplemented`,
//! PLATFORM_NOT_SUPPORTED to the tunnel), and a procedure the service doesn't have as Go's HTTP
//! mux does, "404 page not found".

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt as _, StreamExt as _};
use prost::Message;

use super::error::{self, Error};

const SERVICE: &str = "hotbed.v1.WorkspaceService";

/// How a call's messages go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Unary,
    /// One request, a stream of answers.
    ServerStream,
    /// A stream of requests: no tunnel call can be one, as the tunnel sends one request.
    ClientStream,
}

/// WorkspaceService's calls, by name, from hotbed's proto.
static CALLS: LazyLock<HashMap<String, Shape>> = LazyLock::new(|| {
    let set = prost_types::FileDescriptorSet::decode(
        &include_bytes!(concat!(env!("OUT_DIR"), "/hotbed.bin"))[..],
    )
    .expect("build.rs wrote hotbed's descriptors");
    set.file
        .iter()
        .filter(|f| f.package() == "hotbed.v1")
        .flat_map(|f| &f.service)
        .filter(|s| s.name() == "WorkspaceService")
        .flat_map(|s| &s.method)
        .map(|m| {
            let shape = match (m.client_streaming(), m.server_streaming()) {
                (false, false) => Shape::Unary,
                (false, true) => Shape::ServerStream,
                (true, _) => Shape::ClientStream,
            };
            (m.name().to_owned(), shape)
        })
        .collect()
});

/// A call's answers, encoded.
pub(crate) type Answers = BoxStream<'static, Result<Vec<u8>, Error>>;

/// A typed stream of a handler's answers.
pub(crate) type Stream<T> = BoxStream<'static, Result<T, Error>>;

type Unary = Box<dyn Fn(Vec<u8>) -> BoxFuture<'static, Result<Vec<u8>, Error>> + Send + Sync>;
type Streaming = Box<dyn Fn(Vec<u8>) -> BoxFuture<'static, Result<Answers, Error>> + Send + Sync>;

enum Handler {
    Unary(Unary),
    Stream(Streaming),
}

/// What a call does: one answer, or a stream of them.
pub(crate) enum Call {
    Unary(BoxFuture<'static, Result<Vec<u8>, Error>>),
    Stream(BoxFuture<'static, Result<Answers, Error>>),
}

/// The handlers of WorkspaceService's calls.
#[derive(Default)]
pub(crate) struct Router {
    handlers: HashMap<&'static str, Handler>,
}

fn decode<Req: Message + Default>(body: &[u8]) -> Result<Req, Error> {
    Req::decode(body).map_err(|e| {
        let name = std::any::type_name::<Req>().rsplit("::").next().unwrap_or_default();
        Error::new(error::INVALID_ARGS, format!("unmarshal into hotbed.v1.{name}: {e}"))
    })
}

impl Router {
    /// `name` must be one of the service's calls, of `shape`: a router made otherwise is
    /// machined's mistake, which its tests find.
    fn add(&mut self, name: &'static str, shape: Shape, handler: Handler) {
        assert_eq!(CALLS.get(name), Some(&shape), "{SERVICE}/{name} is not a {shape:?} call");
        assert!(self.handlers.insert(name, handler).is_none(), "{name} has one handler");
    }

    /// Serves the unary call `name` with `f`, on `service`.
    pub(crate) fn unary<S, Req, Res, F, Fut>(
        mut self,
        service: &Arc<S>,
        name: &'static str,
        f: F,
    ) -> Self
    where
        S: Send + Sync + 'static,
        Req: Message + Default + 'static,
        Res: Message + 'static,
        F: Fn(Arc<S>, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Res, Error>> + Send + 'static,
    {
        let service = service.clone();
        let f = Arc::new(f);
        let handler: Unary = Box::new(move |body| {
            let (service, f) = (service.clone(), f.clone());
            async move { Ok(f(service, decode(&body)?).await?.encode_to_vec()) }.boxed()
        });
        self.add(name, Shape::Unary, Handler::Unary(handler));
        self
    }

    /// Serves the server-streaming call `name` with `f`, on `service`.
    pub(crate) fn stream<S, Req, Res, F, Fut>(
        mut self,
        service: &Arc<S>,
        name: &'static str,
        f: F,
    ) -> Self
    where
        S: Send + Sync + 'static,
        Req: Message + Default + 'static,
        Res: Message + 'static,
        F: Fn(Arc<S>, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Stream<Res>, Error>> + Send + 'static,
    {
        let service = service.clone();
        let f = Arc::new(f);
        let handler: Streaming = Box::new(move |body| {
            let (service, f) = (service.clone(), f.clone());
            async move {
                let answers = f(service, decode(&body)?).await?;
                Ok(answers.map(|answer| answer.map(|m| m.encode_to_vec())).boxed())
            }
            .boxed()
        });
        self.add(name, Shape::ServerStream, Handler::Stream(handler));
        self
    }

    /// The call `procedure` with request `body`, as a tunnel request says (`stream`: whether it
    /// asks for a stream of answers), or why there is none: as hotbed's worker answers, its
    /// Connect handler behind `net/http`'s mux (`internal/worker/tunnel.go`).
    pub(crate) fn call(&self, procedure: &str, stream: bool, body: Vec<u8>) -> Result<Call, Error> {
        let not_found = || Error::new(error::INTERNAL, "404 page not found");
        let name = procedure
            .strip_prefix('/')
            .and_then(|p| p.strip_prefix(SERVICE))
            .and_then(|p| p.strip_prefix('/'))
            .ok_or_else(not_found)?;
        let shape = *CALLS.get(name).ok_or_else(not_found)?;
        // Connect takes a stream's envelope (application/connect+proto) for streaming calls alone,
        // and a unary call's bare message (application/proto) for unary ones alone.
        if stream != (shape != Shape::Unary) {
            return Err(Error::new(error::INTERNAL, "415 Unsupported Media Type"));
        }
        match self.handlers.get(name) {
            Some(Handler::Unary(f)) => Ok(Call::Unary(f(body))),
            Some(Handler::Stream(f)) => Ok(Call::Stream(f(body))),
            None => Err(Error::new(
                error::PLATFORM_NOT_SUPPORTED,
                format!("{SERVICE}.{name} is not implemented"),
            )
            .suggest(
                "machined's hotbed node serves workspaces' lifecycles, commands and their logs: \
                 crates/machined/README.md says what it does not serve yet",
            )),
        }
    }
}
