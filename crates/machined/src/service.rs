// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Machined service: each call checked ([`checks`]), then done on the machines
//! ([`Machines`]).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use libmachine::agent::{Event, Process};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::checks;
use crate::errors;
use crate::guest;
use crate::machines::{AGENT_READY, Machines};
use crate::proto::machined_server::Machined;
use crate::proto::{self, exec_request, exec_response};
use crate::vmm;

/// The most bytes of a console answer.
const CONSOLE_CHUNK: usize = 64 << 10;
/// The most bytes of a ReadFile answer.
const FILE_CHUNK: usize = 1 << 20;
/// The most bytes of an Exec's stdin request, or a WriteFile's chunk.
const MAX_CHUNK: usize = 1 << 20;
/// Answers queued for a slow caller of a stream, each at most a chunk: past that, a command's
/// output waits in its guest's agent, and then the command.
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

    async fn stop_machine(
        &self,
        request: Request<proto::StopMachineRequest>,
    ) -> Result<Response<proto::StopMachineResponse>, Status> {
        let proto::StopMachineRequest { name, sync_filesystems, timeout_ms } = request.into_inner();
        let entry = self.machines.get(&name)?;
        let timeout = (timeout_ms != 0).then(|| Duration::from_millis(timeout_ms));
        let (machines, stopped) = (self.machines.clone(), entry.clone());
        detached(async move { machines.stop(&stopped, sync_filesystems, timeout).await }).await?;
        Ok(Response::new(proto::StopMachineResponse { machine: Some(entry.to_proto()) }))
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

    type WatchMachineStream = Answers<proto::WatchMachineResponse>;

    async fn watch_machine(
        &self,
        request: Request<proto::WatchMachineRequest>,
    ) -> Result<Response<Self::WatchMachineStream>, Status> {
        let entry = self.machines.get(&request.into_inner().name)?;
        let (mut status, mut deleted) = (entry.status(), entry.deleted());
        let (tx, rx) = mpsc::channel(QUEUED);
        tokio::spawn(async move {
            loop {
                let now = status.borrow_and_update().to_proto();
                let answer = proto::WatchMachineResponse { status: Some(now) };
                if tx.send(Ok(answer)).await.is_err() {
                    return;
                }
                tokio::select! {
                    changed = status.changed() => if changed.is_err() { return },
                    () = became_true(&mut deleted) => return,
                    () = tx.closed() => return,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
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

    type ExecStream = Answers<proto::ExecResponse>;

    async fn exec(
        &self,
        request: Request<Streaming<proto::ExecRequest>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let mut input = request.into_inner();
        let first = input.message().await?.and_then(|r| r.request);
        let Some(exec_request::Request::Start(start)) = first else {
            return Err(invalid("an Exec's first request starts its command"));
        };
        let command = checks::command(&start)?;
        let guest = self.machines.get(&start.name)?.guest()?;
        // Held until the command is finished, by its output thread.
        let place = guest.place()?;
        let process = guest
            .call(move |agent| {
                agent.wait_ready(AGENT_READY)?;
                agent.spawn(&command)
            })
            .await?;
        let process = Arc::new(process);
        if !start.stdin {
            let p = process.clone();
            guest.call(move |_| p.close_stdin()).await?;
        }
        let (tx, rx) = mpsc::channel(QUEUED);
        let started = exec_response::Response::Started(proto::ExecStarted { pid: process.pid() });
        tx.send(Ok(proto::ExecResponse { response: Some(started) }))
            .await
            .map_err(|_| Status::cancelled("the caller went"))?;
        let (finished_tx, finished) = watch::channel(false);
        let (writes, writing) = mpsc::channel(QUEUED);
        {
            let (process, tx) = (process.clone(), tx.clone());
            guest::spawn("machined-exec", move || {
                let _place = place;
                pump(&process, &tx, &finished_tx);
            })?;
        }
        {
            let (process, tx) = (process.clone(), tx.clone());
            guest::spawn("machined-stdin", move || write(&process, writing, &tx))?;
        }
        tokio::spawn(feed(input, process, tx, writes, finished, start.stdin));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type ReadFileStream = Answers<proto::ReadFileResponse>;

    async fn read_file(
        &self,
        request: Request<proto::ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        let proto::ReadFileRequest { name, path } = request.into_inner();
        checks::guest_path(&path)?;
        let guest = self.machines.get(&name)?.guest()?;
        let data = guest
            .file_call(move |agent| {
                agent.wait_ready(AGENT_READY)?;
                agent.read_file(&path)
            })
            .await?;
        let chunks: Vec<_> = if data.is_empty() {
            vec![Vec::new()]
        } else {
            data.chunks(FILE_CHUNK).map(<[u8]>::to_vec).collect()
        };
        let answers = chunks.into_iter().map(|data| Ok(proto::ReadFileResponse { data }));
        Ok(Response::new(Box::pin(tokio_stream::iter(answers))))
    }

    async fn write_file(
        &self,
        request: Request<Streaming<proto::WriteFileRequest>>,
    ) -> Result<Response<proto::WriteFileResponse>, Status> {
        let mut input = request.into_inner();
        let first = input.message().await?.ok_or_else(|| invalid("a WriteFile names its file"))?;
        let proto::WriteFileRequest { name, path, mode, data: mut contents } = first;
        checks::guest_path(&path)?;
        if contents.len() > MAX_CHUNK {
            return Err(invalid(format!("a WriteFile chunk is at most {MAX_CHUNK} bytes")));
        }
        let mode = checks::mode(mode)?;
        let guest = self.machines.get(&name)?.guest()?;
        while let Some(next) = input.message().await? {
            if !next.name.is_empty() || !next.path.is_empty() || next.mode != 0 {
                return Err(invalid("only a WriteFile's first request names its file"));
            }
            if next.data.len() > MAX_CHUNK {
                return Err(invalid(format!("a WriteFile chunk is at most {MAX_CHUNK} bytes")));
            }
            if next.data.len() > vmm::MAX_FILE - contents.len().min(vmm::MAX_FILE) {
                return Err(Status::resource_exhausted(format!(
                    "a file written is at most {} bytes",
                    vmm::MAX_FILE
                )));
            }
            contents.extend_from_slice(&next.data);
        }
        guest
            .file_call(move |agent| {
                agent.wait_ready(AGENT_READY)?;
                agent.write_file(&path, &contents, mode)
            })
            .await?;
        Ok(Response::new(proto::WriteFileResponse {}))
    }

    async fn snapshot_machine(
        &self,
        request: Request<proto::SnapshotMachineRequest>,
    ) -> Result<Response<proto::SnapshotMachineResponse>, Status> {
        let proto::SnapshotMachineRequest { name, snapshot } = request.into_inner();
        let (machines, entry) = (self.machines.clone(), self.machines.get(&name)?);
        detached(async move { machines.snapshot(&entry, snapshot).await }).await?;
        Ok(Response::new(proto::SnapshotMachineResponse {}))
    }

    async fn restore_machine(
        &self,
        request: Request<proto::RestoreMachineRequest>,
    ) -> Result<Response<proto::RestoreMachineResponse>, Status> {
        let proto::RestoreMachineRequest { snapshot, name, labels, vsock_listen_ports } =
            request.into_inner();
        let machines = self.machines.clone();
        let entry =
            detached(
                async move { machines.restore(&snapshot, name, labels, vsock_listen_ports).await },
            )
            .await?;
        Ok(Response::new(proto::RestoreMachineResponse { machine: Some(entry.to_proto()) }))
    }

    async fn delete_snapshot(
        &self,
        request: Request<proto::DeleteSnapshotRequest>,
    ) -> Result<Response<proto::DeleteSnapshotResponse>, Status> {
        self.machines.delete_snapshot(&request.into_inner().snapshot)?;
        Ok(Response::new(proto::DeleteSnapshotResponse {}))
    }

    async fn suspend_machine(
        &self,
        request: Request<proto::SuspendMachineRequest>,
    ) -> Result<Response<proto::SuspendMachineResponse>, Status> {
        let (machines, entry) =
            (self.machines.clone(), self.machines.get(&request.into_inner().name)?);
        detached(async move { machines.suspend(&entry).await }).await?;
        Ok(Response::new(proto::SuspendMachineResponse {}))
    }

    async fn balloon(
        &self,
        request: Request<proto::BalloonRequest>,
    ) -> Result<Response<proto::BalloonResponse>, Status> {
        let proto::BalloonRequest { name, target_bytes } = request.into_inner();
        let entry = self.machines.get(&name)?;
        let balloon = self.machines.balloon(&entry, target_bytes).await?;
        Ok(Response::new(proto::BalloonResponse { balloon: Some(balloon) }))
    }

    async fn resume_machine(
        &self,
        request: Request<proto::ResumeMachineRequest>,
    ) -> Result<Response<proto::ResumeMachineResponse>, Status> {
        let (machines, entry) =
            (self.machines.clone(), self.machines.get(&request.into_inner().name)?);
        detached(async move { machines.resume(&entry).await }).await?;
        Ok(Response::new(proto::ResumeMachineResponse {}))
    }
}

/// Waits until `flag` is true, or its sender is gone.
async fn became_true(flag: &mut watch::Receiver<bool>) {
    // Its guard dropped here, not held across the awaits of a select's other arms.
    let _ = flag.wait_for(|flag| *flag).await;
}

/// An exec's output thread: takes its command's events to the caller until its exit, a failure,
/// or the caller is gone (then kills it); says when it is finished.
fn pump(
    process: &Process,
    tx: &mpsc::Sender<Result<proto::ExecResponse, Status>>,
    finished: &watch::Sender<bool>,
) {
    use exec_response::Response as Answer;
    loop {
        let (answer, last) = match process.next_event() {
            Ok(Some(Event::Stdout(data))) => (Ok(Answer::Stdout(data)), false),
            Ok(Some(Event::Stderr(data))) => (Ok(Answer::Stderr(data)), false),
            Ok(Some(Event::Exit(status))) => {
                let status = match (status.code, status.signal) {
                    (_, Some(signal)) => proto::exec_exit::Status::Signal(signal),
                    (code, None) => proto::exec_exit::Status::Code(code.unwrap_or(-1)),
                };
                (Ok(Answer::Exit(proto::ExecExit { status: Some(status) })), true)
            }
            Ok(None) => (Err(Status::internal("the command's exit was already taken")), true),
            Err(e) => (Err(errors::status(e)), true),
        };
        if last {
            finished.send_replace(true);
        }
        let answer = answer.map(|answer| proto::ExecResponse { response: Some(answer) });
        if tx.blocking_send(answer).is_err() {
            // The caller went: what it ran goes too.
            if !last && let Err(e) = process.kill() {
                log::debug!("machined: killing an exec whose caller went: {e}");
            }
            finished.send_replace(true);
            return;
        }
        if last {
            return;
        }
    }
}

/// What an exec's caller sends its command, after its start, in order.
enum Write {
    Stdin(Vec<u8>),
    CloseStdin,
    Signal(i32),
}

/// An exec's writing thread: hands its command what its caller sent, in order, until the caller
/// sends no more or a write fails, whose status ends the caller's stream. A write the guest takes
/// no room for waits here, never where the caller's going is watched ([`feed`]).
fn write(
    process: &Process,
    mut writing: mpsc::Receiver<Write>,
    tx: &mpsc::Sender<Result<proto::ExecResponse, Status>>,
) {
    while let Some(write) = writing.blocking_recv() {
        let written = match write {
            Write::Stdin(data) => process.write_stdin(&data),
            Write::CloseStdin => process.close_stdin(),
            Write::Signal(signal) => process.signal(signal),
        };
        if let Err(e) = written {
            // The caller may have gone already.
            let _ = tx.blocking_send(Err(errors::status(e)));
            return;
        }
    }
}

/// An exec's requests after its first, checked and handed to its writing thread, until its
/// command is finished, or its caller goes, which kills it.
async fn feed(
    mut input: Streaming<proto::ExecRequest>,
    process: Arc<Process>,
    tx: mpsc::Sender<Result<proto::ExecResponse, Status>>,
    writes: mpsc::Sender<Write>,
    finished: watch::Receiver<bool>,
    stdin: bool,
) {
    // Its writing thread ends once `writes` goes, with this; the kill is sent after what it was
    // writing. A command fed more stdin than it reads takes the kill only once it reads: board
    // 13561.
    let kill = |process: &Arc<Process>, finished: &watch::Receiver<bool>| {
        if !*finished.borrow() {
            let p = process.clone();
            let killed = guest::spawn("machined-kill", move || {
                if let Err(e) = p.kill() {
                    log::debug!("machined: killing an exec whose caller went: {e}");
                }
            });
            if let Err(e) = killed {
                log::warn!("machined: killing an exec whose caller went: {e}");
            }
        }
    };
    let mut reading = true;
    let mut finishing = finished.clone();
    loop {
        let request = tokio::select! {
            () = became_true(&mut finishing) => return,
            () = tx.closed() => return kill(&process, &finished),
            request = input.message(), if reading => request,
        };
        let write = match request {
            Ok(Some(proto::ExecRequest { request: Some(request) })) => checked(request, stdin),
            Ok(Some(proto::ExecRequest { request: None })) => {
                Err(invalid("an Exec request says what it does"))
            }
            // The caller sends no more, and still reads.
            Ok(None) => {
                reading = false;
                continue;
            }
            Err(_) => return kill(&process, &finished),
        };
        let write = match write {
            Ok(write) => write,
            Err(status) => {
                // The caller is told why; the stream then ends with its status.
                let _ = tx.send(Err(status)).await;
                return kill(&process, &finished);
            }
        };
        tokio::select! {
            () = became_true(&mut finishing) => return,
            () = tx.closed() => return kill(&process, &finished),
            sent = writes.send(write) => if sent.is_err() {
                // The writing thread failed, and told the caller why.
                return kill(&process, &finished);
            },
        }
    }
}

/// An exec's request after its first, checked.
fn checked(request: exec_request::Request, stdin: bool) -> Result<Write, Status> {
    match request {
        exec_request::Request::Start(_) => Err(invalid("only an Exec's first request starts it")),
        exec_request::Request::Stdin(_) | exec_request::Request::CloseStdin(_) if !stdin => {
            Err(invalid("this command's stdin was closed: its start asked for none"))
        }
        exec_request::Request::Stdin(data) if data.len() > MAX_CHUNK => Err(invalid(format!(
            "{} bytes of stdin in one request: at most {MAX_CHUNK}",
            data.len()
        ))),
        exec_request::Request::Stdin(data) => Ok(Write::Stdin(data)),
        exec_request::Request::CloseStdin(_) => Ok(Write::CloseStdin),
        exec_request::Request::Signal(signal) => Ok(Write::Signal(checks::signal(signal)?)),
    }
}
