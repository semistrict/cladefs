// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Machined service: each call checked (`checks`), then done on the machines ([`Machines`]).

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::machines::Machines;
use crate::proto;
use crate::proto::machined_server::Machined;

/// The most bytes of a console answer.
const CONSOLE_CHUNK: usize = 64 << 10;
/// Answers queued for a slow caller of a stream, each at most a chunk.
const QUEUED: usize = 8;

type Answers<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// machined's API over its machines.
#[derive(Clone)]
pub(crate) struct Service {
    machines: Arc<Machines>,
}

impl Service {
    pub(crate) fn new(machines: Arc<Machines>) -> Self {
        Self { machines }
    }
}

fn invalid(message: impl Into<String>) -> Status {
    Status::invalid_argument(message)
}

/// Runs a lifecycle call to its end even if its caller goes (a deadline, a cancel): dropped
/// half-way, it would leave its machine STARTING or STOPPING, or a VM made and lost.
async fn detached<T: Send + 'static>(
    call: impl Future<Output = Result<T, Status>> + Send + 'static,
) -> Result<T, Status> {
    tokio::spawn(call)
        .await
        .map_err(|e| Status::internal(format!("a lifecycle call failed: {e}")))?
}

#[tonic::async_trait]
impl Machined for Service {
    async fn create_machine(
        &self,
        request: Request<proto::CreateMachineRequest>,
    ) -> Result<Response<proto::CreateMachineResponse>, Status> {
        let proto::CreateMachineRequest { name, spec } = request.into_inner();
        let spec = spec.ok_or_else(|| invalid("a machine has a spec"))?;
        let entry = self.machines.create(name, spec).await?;
        Ok(Response::new(proto::CreateMachineResponse { machine: Some(entry.to_proto()) }))
    }

    async fn start_machine(
        &self,
        request: Request<proto::StartMachineRequest>,
    ) -> Result<Response<proto::StartMachineResponse>, Status> {
        let entry = self.machines.get(&request.into_inner().name)?;
        let (machines, started) = (self.machines.clone(), entry.clone());
        detached(async move { machines.start(&started).await }).await?;
        Ok(Response::new(proto::StartMachineResponse { machine: Some(entry.to_proto()) }))
    }

    async fn delete_machine(
        &self,
        request: Request<proto::DeleteMachineRequest>,
    ) -> Result<Response<proto::DeleteMachineResponse>, Status> {
        let proto::DeleteMachineRequest { name, force } = request.into_inner();
        let (machines, entry) = (self.machines.clone(), self.machines.get(&name)?);
        detached(async move { machines.delete(&entry, force).await }).await?;
        Ok(Response::new(proto::DeleteMachineResponse {}))
    }

    async fn get_machine(
        &self,
        request: Request<proto::GetMachineRequest>,
    ) -> Result<Response<proto::GetMachineResponse>, Status> {
        let entry = self.machines.get(&request.into_inner().name)?;
        Ok(Response::new(proto::GetMachineResponse { machine: Some(entry.to_proto()) }))
    }

    async fn list_machines(
        &self,
        _: Request<proto::ListMachinesRequest>,
    ) -> Result<Response<proto::ListMachinesResponse>, Status> {
        let machines = self.machines.list().iter().map(|e| e.to_proto()).collect();
        Ok(Response::new(proto::ListMachinesResponse { machines }))
    }

    type StreamConsoleStream = Answers<proto::StreamConsoleResponse>;

    async fn stream_console(
        &self,
        request: Request<proto::StreamConsoleRequest>,
    ) -> Result<Response<Self::StreamConsoleStream>, Status> {
        let proto::StreamConsoleRequest { name, offset, follow } = request.into_inner();
        let entry = self.machines.get(&name)?;
        let console = entry.console().clone();
        let (mut end, mut deleted) = (console.subscribe(), entry.deleted());
        let (tx, rx) = mpsc::channel(QUEUED);
        tokio::spawn(async move {
            let mut next = offset;
            loop {
                // Seen before the read, so that a write after it wakes the wait below.
                end.borrow_and_update();
                let (from, data) = console.read(next, CONSOLE_CHUNK);
                if !data.is_empty() {
                    next = from.saturating_add(data.len() as u64);
                    let answer = proto::StreamConsoleResponse { offset: from, data };
                    if tx.send(Ok(answer)).await.is_err() {
                        return;
                    }
                    continue;
                }
                if !follow {
                    return;
                }
                tokio::select! {
                    changed = end.changed() => if changed.is_err() { return },
                    () = became_true(&mut deleted) => return,
                    () = tx.closed() => return,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// Waits until `flag` is true, or its sender is gone.
async fn became_true(flag: &mut watch::Receiver<bool>) {
    // Its guard dropped here, not held across the awaits of a select's other arms.
    let _ = flag.wait_for(|flag| *flag).await;
}
