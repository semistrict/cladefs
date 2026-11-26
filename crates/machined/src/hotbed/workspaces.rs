// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! hotbed's WorkspaceService on machined's machines, what the controller calls down the tunnel:
//!
//! - **A workspace is a machine**, of the node's template (`Hotbed::workspace`), named
//!   `hotbed-<n>` (hotbed's IDs are not machined's names) and labelled with its workspace and
//!   session. CreateWorkspace boots it: ACTIVE once it runs, FAILED if its guest stopped;
//!   ReleaseWorkspace deletes it, which ends what runs in it. A throwaway workspace is released
//!   once its command ends, as hotbed's worker does.
//! - **A command is an exec in it**, through libmachine's agent: its arguments, its environment
//!   on libmachine's PATH, in `/work/<repo>` if it names a repo. Its output, stdout and stderr in
//!   the order they came, is kept as a machine's console is, its last 1 MiB at offsets in all it
//!   wrote, which FollowLog streams and GetLogLines tails; StopCommand kills it, CANCELLED.
//!
//! Calls and errors are hotbed's worker's (`internal/worker/server.go`): its IDs checked, NOT_FOUND
//! "no workspace X on this node", ListCommands by session alone. What machined's machines can't
//! do yet is in crates/machined/README.md, each with its task.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use libmachine::agent::{Event, Process};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use super::error::{self, Error};
use super::proto as pb;
use super::router::{Router, Stream};
use crate::console::{self, Console};
use crate::guest;
use crate::machines::{AGENT_READY, Entry, Machines};
use crate::proto::{self as m, MachineState};
use crate::{checks, errors};

/// The runtime a node's workspaces and commands say they run on.
pub(crate) const RUNTIME: &str = "machined";
/// libmachine's PATH, which a command given an environment keeps unless it sets its own.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Ended commands kept, with their logs, for calls after their end: the oldest go first.
const KEPT_ENDED: usize = 64;
/// The most commands under way on the node.
const MAX_RUNNING: usize = 256;
/// The most bytes of output a FollowLog chunk carries.
const LOG_CHUNK: usize = 64 << 10;
/// The most bytes of lines GetLogLines answers (hotbed's).
const MAX_LOG_LINES: usize = 1 << 20;
/// Lines GetLogLines answers unless asked (hotbed's).
const DEFAULT_TAIL: usize = 2000;
/// How long StopCommand waits for its command to end.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// Answers queued for a stream's caller.
const QUEUED: usize = 8;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Unix milliseconds, hotbed's times.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A workspace's or command's ID, as hotbed's worker takes them (`internal/worker/id.go`): an
/// ASCII letter or digit, then up to 127 of letters, digits, '-' and '_'.
fn check_id(id: &str) -> Result<(), Error> {
    let bytes = id.as_bytes();
    let ok = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.len() <= 128
        && bytes.iter().all(|c| c.is_ascii_alphanumeric() || b"-_".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            error::INVALID_ARGS,
            format!(
                "invalid ID {id:?}: must start with an ASCII letter or digit and contain only \
                 letters, digits, '-' or '_'"
            ),
        ))
    }
}

fn not_found(kind: &str, id: &str) -> Error {
    Error::new(error::NOT_FOUND, format!("no {kind} {id} on this node"))
}

/// `args` as a shell would take them, for display.
fn quoted(args: &[String]) -> String {
    let quote = |arg: &String| {
        let plain = !arg.is_empty()
            && arg.bytes().all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c));
        if plain { arg.clone() } else { format!("'{}'", arg.replace('\'', r"'\''")) }
    };
    args.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// A workspace: its machine, and what hotbed said of it.
struct Space {
    id: String,
    session_id: String,
    agent: String,
    created_at: i64,
    fork_millis: i64,
    throwaway: bool,
    machine: Arc<Entry>,
    /// Set as it is released: RELEASED while its machine goes, and after.
    released: AtomicBool,
}

impl Space {
    fn to_proto(&self, node_id: &str) -> pb::Workspace {
        let status = self.machine.status().borrow().clone();
        let (state, reason) = match status.state() {
            _ if self.released.load(Ordering::Acquire) => {
                (pb::WorkspaceState::Released, String::new())
            }
            MachineState::Running | MachineState::Suspended => {
                (pb::WorkspaceState::Active, String::new())
            }
            MachineState::Starting | MachineState::Created => {
                (pb::WorkspaceState::Preparing, String::new())
            }
            MachineState::Stopping | MachineState::Stopped | MachineState::Unspecified => {
                let why = status.last_exit().map_or_else(
                    || "its machine stopped".to_owned(),
                    |exit| format!("its machine stopped: {:?} {}", exit.reason(), exit.detail),
                );
                (pb::WorkspaceState::Failed, why.trim_end().to_owned())
            }
        };
        pb::Workspace {
            id: self.id.clone(),
            node_id: node_id.to_owned(),
            agent: self.agent.clone(),
            state: state.into(),
            created_at: self.created_at,
            fork_millis: self.fork_millis,
            session_id: self.session_id.clone(),
            reason,
            runtime: RUNTIME.into(),
            throwaway: self.throwaway,
            ..pb::Workspace::default()
        }
    }
}

/// Where a command is.
#[derive(Debug, Clone)]
struct Progress {
    state: pb::CommandState,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    exit_code: Option<i32>,
    reason: String,
}

fn ended(state: pb::CommandState) -> bool {
    matches!(
        state,
        pb::CommandState::Succeeded | pb::CommandState::Failed | pb::CommandState::Cancelled
    )
}

/// A command.
struct Run {
    id: String,
    workspace_id: String,
    session_id: String,
    args: Vec<String>,
    kind: String,
    attached: bool,
    repo: String,
    queued_at: i64,
    log: Arc<Console>,
    progress: watch::Sender<Progress>,
    process: Arc<Process>,
    /// Why it is being stopped, once StopCommand or a release stops it: it ends CANCELLED.
    stopping: Mutex<Option<String>>,
}

impl Run {
    fn to_proto(&self, node_id: &str) -> pb::Command {
        let progress = self.progress.borrow().clone();
        let kept_from = self.log.read(0, 0).0;
        pb::Command {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            node_id: node_id.to_owned(),
            argv: quoted(&self.args),
            args: self.args.clone(),
            kind: self.kind.clone(),
            state: progress.state.into(),
            queued_at: self.queued_at,
            started_at: progress.started_at,
            ended_at: progress.ended_at,
            exit_code: progress.exit_code,
            reason: progress.reason,
            attached: self.attached,
            session_id: self.session_id.clone(),
            repo: self.repo.clone(),
            dropped_output_bytes: i64::try_from(kept_from).unwrap_or(i64::MAX),
            runtime: RUNTIME.into(),
            ..pb::Command::default()
        }
    }

    /// Stops it for `why`: kills it, on a thread of its own, as a guest's calls go.
    fn stop(self: &Arc<Self>, why: String) {
        lock(&self.stopping).get_or_insert(why);
        let process = self.process.clone();
        let killed = guest::spawn("machined-hotbed-kill", move || {
            if let Err(e) = process.kill() {
                log::debug!("machined: killing a hotbed command: {e}");
            }
        });
        if let Err(e) = killed {
            log::warn!("machined: killing a hotbed command: {e}");
        }
    }
}

#[derive(Default)]
struct State {
    spaces: BTreeMap<String, Arc<Space>>,
    /// Workspaces being made: their IDs, taken.
    creating: BTreeSet<String>,
    runs: HashMap<String, Arc<Run>>,
    /// Ended commands, oldest first.
    ended: VecDeque<String>,
}

/// The node's workspaces and commands.
pub(crate) struct Workspaces {
    machines: Arc<Machines>,
    node_id: String,
    /// What each workspace's machine is.
    template: m::MachineSpec,
    state: Mutex<State>,
    next: AtomicU64,
    /// Where commands' output threads, apart from tokio, start what ends their workspaces.
    runtime: tokio::runtime::Handle,
}

impl Workspaces {
    /// The node's workspaces, none yet; on the current tokio runtime.
    pub(crate) fn new(machines: Arc<Machines>, node_id: String, template: m::MachineSpec) -> Self {
        Self {
            machines,
            node_id,
            template,
            state: Mutex::default(),
            next: AtomicU64::new(1),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// WorkspaceService's calls the node serves.
    pub(crate) fn router(self: &Arc<Self>) -> Router {
        Router::default()
            .unary(self, "CreateWorkspace", Self::create_workspace)
            .unary(self, "GetWorkspace", Self::get_workspace)
            .stream(self, "WatchWorkspace", Self::watch_workspace)
            .unary(self, "ListWorkspaces", Self::list_workspaces)
            .unary(self, "ReleaseWorkspace", Self::release_workspace)
            .unary(self, "RunCommand", Self::run_command)
            .unary(self, "GetCommand", Self::get_command)
            .unary(self, "ListCommands", Self::list_commands)
            .unary(self, "StopCommand", Self::stop_command)
            .stream(self, "FollowLog", Self::follow_log)
            .unary(self, "GetLogLines", Self::get_log_lines)
    }

    /// The memory its workspaces' machines hold.
    pub(crate) fn reserved_memory(&self) -> i64 {
        let spaces = lock(&self.state).spaces.len();
        let each = i64::try_from(self.template.memory_bytes).unwrap_or(i64::MAX);
        each.saturating_mul(i64::try_from(spaces).unwrap_or(i64::MAX))
    }

    fn space(&self, id: &str) -> Result<Arc<Space>, Error> {
        check_id(id)?;
        lock(&self.state).spaces.get(id).cloned().ok_or_else(|| not_found("workspace", id))
    }

    fn run(&self, id: &str) -> Result<Arc<Run>, Error> {
        check_id(id)?;
        lock(&self.state).runs.get(id).cloned().ok_or_else(|| not_found("command", id))
    }

    /// A new ID of the node's.
    fn new_id(&self, prefix: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}{:x}{n:x}", now())
    }

    async fn create_workspace(
        self: Arc<Self>,
        req: pb::CreateWorkspaceRequest,
    ) -> Result<pb::Workspace, Error> {
        let id = if req.id.is_empty() { self.new_id("w") } else { req.id.clone() };
        check_id(&id)?;
        if !req.session_id.is_empty() {
            check_id(&req.session_id)?;
        }
        {
            let mut state = lock(&self.state);
            if let Some(space) = state.spaces.get(&id) {
                return Ok(space.to_proto(&self.node_id));
            }
            if !state.creating.insert(id.clone()) {
                return Err(Error::new(
                    error::CONFLICT,
                    format!("workspace {id} is being created"),
                ));
            }
        }
        // To its end even if its caller goes: dropped half-way, it would leave a machine made
        // and lost.
        let made = tokio::spawn({
            let this = self.clone();
            let id = id.clone();
            async move { this.make(id, req).await }
        })
        .await
        .unwrap_or_else(|e| Err(Error::new(error::INTERNAL, format!("making a workspace: {e}"))));
        lock(&self.state).creating.remove(&id);
        made
    }

    /// Makes workspace `id`'s machine and boots it.
    async fn make(
        &self,
        id: String,
        req: pb::CreateWorkspaceRequest,
    ) -> Result<pb::Workspace, Error> {
        let mut spec = self.template.clone();
        spec.labels.insert("hotbed.workspace".into(), id.clone());
        if !req.session_id.is_empty() {
            spec.labels.insert("hotbed.session".into(), req.session_id.clone());
        }
        let name = format!("hotbed-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let entry = self.machines.create(name, spec).await?;
        let booting = Instant::now();
        if let Err(status) = self.machines.start(&entry).await {
            if let Err(e) = self.machines.delete(&entry, true).await {
                log::warn!("machined: deleting workspace {id}'s machine that failed to boot: {e}");
            }
            let full = status.code() == tonic::Code::ResourceExhausted;
            let e = Error::from(status);
            return Err(if full {
                e.suggest("this node takes no more workspaces now: the controller places elsewhere")
            } else {
                e
            });
        }
        let space = Arc::new(Space {
            id: id.clone(),
            session_id: req.session_id,
            agent: req.agent,
            created_at: now(),
            fork_millis: i64::try_from(booting.elapsed().as_millis()).unwrap_or(i64::MAX),
            throwaway: req.throwaway,
            machine: entry,
            released: AtomicBool::new(false),
        });
        lock(&self.state).spaces.insert(id, space.clone());
        Ok(space.to_proto(&self.node_id))
    }

    async fn get_workspace(
        self: Arc<Self>,
        req: pb::GetWorkspaceRequest,
    ) -> Result<pb::Workspace, Error> {
        Ok(self.space(&req.id)?.to_proto(&self.node_id))
    }

    async fn list_workspaces(
        self: Arc<Self>,
        _: pb::ListWorkspacesRequest,
    ) -> Result<pb::ListWorkspacesResponse, Error> {
        let mut spaces: Vec<_> = lock(&self.state).spaces.values().cloned().collect();
        spaces.sort_by_key(|s| s.created_at);
        let workspaces = spaces.iter().map(|s| s.to_proto(&self.node_id)).collect();
        Ok(pb::ListWorkspacesResponse { workspaces })
    }

    async fn watch_workspace(
        self: Arc<Self>,
        req: pb::WatchWorkspaceRequest,
    ) -> Result<Stream<pb::Workspace>, Error> {
        let space = self.space(&req.id)?;
        let (mut status, mut deleted) = (space.machine.status(), space.machine.deleted());
        let (tx, rx) = mpsc::channel(QUEUED);
        let node_id = self.node_id.clone();
        tokio::spawn(async move {
            let mut sent = None;
            loop {
                status.borrow_and_update();
                let gone = *deleted.borrow_and_update();
                if gone {
                    // Its machine deleted another way (machined stopping): it is gone too.
                    space.released.store(true, Ordering::Release);
                }
                let now = space.to_proto(&node_id);
                if sent.as_ref() != Some(&now) {
                    if tx.send(Ok(now.clone())).await.is_err() {
                        return;
                    }
                    sent = Some(now);
                }
                if gone {
                    return;
                }
                tokio::select! {
                    changed = status.changed() => if changed.is_err() { return },
                    _ = deleted.changed() => {}
                    () = tx.closed() => return,
                }
            }
        });
        Ok(ReceiverStream::new(rx).boxed())
    }

    async fn release_workspace(
        self: Arc<Self>,
        req: pb::ReleaseWorkspaceRequest,
    ) -> Result<pb::ReleaseWorkspaceResponse, Error> {
        check_id(&req.id)?;
        let this = self.clone();
        tokio::spawn(async move { this.release(&req.id).await }).await.unwrap_or_else(|e| {
            Err(Error::new(error::INTERNAL, format!("releasing a workspace: {e}")))
        })?;
        Ok(pb::ReleaseWorkspaceResponse {})
    }

    /// Releases workspace `id`: its commands stopped, its machine deleted.
    pub(crate) async fn release(&self, id: &str) -> Result<(), Error> {
        let (space, runs) = {
            let mut state = lock(&self.state);
            let space = state.spaces.remove(id).ok_or_else(|| not_found("workspace", id))?;
            let runs: Vec<_> =
                state.runs.values().filter(|r| r.workspace_id == id).cloned().collect();
            (space, runs)
        };
        space.released.store(true, Ordering::Release);
        for run in runs.iter().filter(|r| !ended(r.progress.borrow().state)) {
            lock(&run.stopping).get_or_insert_with(|| "its workspace was released".into());
        }
        self.machines.delete(&space.machine, true).await?;
        Ok(())
    }

    async fn run_command(
        self: Arc<Self>,
        req: pb::RunCommandRequest,
    ) -> Result<pb::Command, Error> {
        let space = self.space(&req.workspace_id)?;
        let id = if req.id.is_empty() { self.new_id("c") } else { req.id.clone() };
        check_id(&id)?;
        if req.args.is_empty() {
            return Err(Error::new(error::INVALID_ARGS, "a command has at least its program"));
        }
        let mut env = HashMap::new();
        for entry in &req.env {
            let (key, value) = entry.split_once('=').ok_or_else(|| {
                Error::new(error::INVALID_ARGS, format!("environment entry {entry:?}: KEY=VALUE"))
            })?;
            env.insert(key.to_owned(), value.to_owned());
        }
        if !env.is_empty() {
            env.entry("PATH".to_owned()).or_insert_with(|| PATH.to_owned());
        }
        if !req.repo.is_empty() {
            check_id(&req.repo)?;
        }
        let start = m::ExecStart {
            name: space.machine.name().to_owned(),
            argv: req.args.clone(),
            env,
            working_directory: if req.repo.is_empty() {
                String::new()
            } else {
                format!("/work/{}", req.repo)
            },
            stdin: false,
        };
        let command = checks::command(&start)?;
        {
            let state = lock(&self.state);
            if state.runs.contains_key(&id) {
                return Err(Error::new(error::CONFLICT, format!("command {id} exists")));
            }
            let running = state.runs.len().saturating_sub(state.ended.len());
            if running >= MAX_RUNNING {
                return Err(Error::new(
                    error::WORKER_UNREACHABLE,
                    format!("{MAX_RUNNING} commands run on this node, its most"),
                )
                .retryable());
            }
        }
        // To its end even if its caller goes: dropped half-way, a command would run unknown.
        let this = self.clone();
        tokio::spawn(async move { this.start(space, id, req, command).await }).await.unwrap_or_else(
            |e| Err(Error::new(error::INTERNAL, format!("starting a command: {e}"))),
        )
    }

    /// Starts command `id` in `space`'s guest, and its output's thread.
    async fn start(
        self: Arc<Self>,
        space: Arc<Space>,
        id: String,
        req: pb::RunCommandRequest,
        command: libmachine::agent::Command,
    ) -> Result<pb::Command, Error> {
        let queued_at = now();
        let guest = space.machine.guest()?;
        // Held until it ends, by its output thread.
        let place = guest.place()?;
        let process = guest
            .call(move |agent| {
                agent.wait_ready(AGENT_READY)?;
                agent.spawn(&command)
            })
            .await?;
        let process = Arc::new(process);
        let p = process.clone();
        guest.call(move |_| p.close_stdin()).await?;
        let progress = Progress {
            state: pb::CommandState::Running,
            started_at: Some(now()),
            ended_at: None,
            exit_code: None,
            reason: String::new(),
        };
        let run = Arc::new(Run {
            id: id.clone(),
            workspace_id: space.id.clone(),
            // A command's session is its workspace's.
            session_id: space.session_id.clone(),
            args: req.args,
            kind: req.kind,
            attached: req.attached,
            repo: req.repo,
            queued_at,
            log: Console::new(),
            progress: watch::channel(progress).0,
            process,
            stopping: Mutex::new(None),
        });
        lock(&self.state).runs.insert(id, run.clone());
        let (this, pumped) = (self.clone(), run.clone());
        let pumping = guest::spawn("machined-hotbed-run", move || {
            let _place = place;
            let outcome = pump(&pumped);
            this.finish(&pumped, outcome);
        });
        if let Err(status) = pumping {
            run.stop("no thread for its output".into());
            self.finish(&run, Err(status.message().to_owned()));
            return Err(status.into());
        }
        Ok(run.to_proto(&self.node_id))
    }

    /// `run` ended as `outcome` (its exit, or why it has none).
    fn finish(&self, run: &Arc<Run>, outcome: Result<libmachine::agent::ExitStatus, String>) {
        let stopped = lock(&run.stopping).clone();
        run.progress.send_modify(|p| {
            p.ended_at = Some(now());
            match (&stopped, outcome) {
                (Some(why), outcome) => {
                    p.state = pb::CommandState::Cancelled;
                    p.reason.clone_from(why);
                    p.exit_code = outcome.ok().and_then(|status| status.code);
                }
                (None, Ok(status)) => {
                    p.exit_code = status.code;
                    match (status.code, status.signal) {
                        (Some(0), _) => p.state = pb::CommandState::Succeeded,
                        (_, Some(signal)) => {
                            p.state = pb::CommandState::Failed;
                            p.reason = format!("killed by signal {signal}");
                        }
                        _ => p.state = pb::CommandState::Failed,
                    }
                }
                (None, Err(why)) => {
                    p.state = pb::CommandState::Failed;
                    p.reason = why;
                }
            }
        });
        let throwaway = {
            let mut state = lock(&self.state);
            state.ended.push_back(run.id.clone());
            while state.ended.len() > KEPT_ENDED {
                if let Some(old) = state.ended.pop_front() {
                    state.runs.remove(&old);
                }
            }
            let throwaway = state.spaces.get(&run.workspace_id).is_some_and(|s| s.throwaway);
            throwaway.then(|| state.spaces.remove(&run.workspace_id)).flatten()
        };
        // A throwaway workspace goes with its command (hotbed's Workspace.throwaway).
        if let Some(space) = throwaway {
            let machines = self.machines.clone();
            self.runtime.spawn(async move {
                if let Err(e) = machines.delete(&space.machine, true).await {
                    log::warn!("machined: releasing throwaway workspace {}: {e}", space.id);
                }
            });
        }
    }

    async fn get_command(
        self: Arc<Self>,
        req: pb::GetCommandRequest,
    ) -> Result<pb::Command, Error> {
        Ok(self.run(&req.id)?.to_proto(&self.node_id))
    }

    async fn list_commands(
        self: Arc<Self>,
        req: pb::ListCommandsRequest,
    ) -> Result<pb::ListCommandsResponse, Error> {
        if req.ended || !req.r#match.is_empty() || req.limit.is_some() || !req.project.is_empty() {
            return Err(Error::new(
                error::INVALID_ARGS,
                "a worker lists its commands by session alone: ask the controller for history, \
                 projects, matches or limits",
            ));
        }
        let mut runs: Vec<_> = lock(&self.state)
            .runs
            .values()
            .filter(|r| req.session_id.is_empty() || r.session_id == req.session_id)
            .cloned()
            .collect();
        runs.sort_by(|a, b| (a.queued_at, &a.id).cmp(&(b.queued_at, &b.id)));
        let commands = runs.iter().map(|r| r.to_proto(&self.node_id)).collect();
        Ok(pb::ListCommandsResponse { commands, more: 0 })
    }

    async fn stop_command(
        self: Arc<Self>,
        req: pb::StopCommandRequest,
    ) -> Result<pb::Command, Error> {
        let run = self.run(&req.id)?;
        if !req.session_id.is_empty() && run.session_id != req.session_id {
            return Err(Error::new(
                error::NOT_FOUND,
                format!("session {} has no command {}", req.session_id, req.id),
            ));
        }
        if !ended(run.progress.borrow().state) {
            let why = [&req.reason, &req.comment]
                .into_iter()
                .find(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "stopped by hand".into());
            run.stop(why);
            let mut progress = run.progress.subscribe();
            let ends = progress.wait_for(|p| ended(p.state));
            if tokio::time::timeout(STOP_WAIT, ends).await.is_err() {
                log::warn!(
                    "machined: hotbed command {} still runs {STOP_WAIT:?} after its kill",
                    run.id
                );
            }
        }
        Ok(run.to_proto(&self.node_id))
    }

    async fn follow_log(
        self: Arc<Self>,
        req: pb::FollowLogRequest,
    ) -> Result<Stream<pb::LogChunk>, Error> {
        let run = self.run(&req.id)?;
        let (tx, rx) = mpsc::channel(QUEUED);
        tokio::spawn(async move {
            let (mut end, mut progress) = (run.log.subscribe(), run.progress.subscribe());
            let mut offset = u64::try_from(req.offset).unwrap_or(0);
            let mut following = req.follow;
            let mut ended_as = None;
            loop {
                // Seen before the reads, so that a write after them wakes the wait below.
                end.borrow_and_update();
                let state = progress.borrow_and_update().state;
                loop {
                    let (from, mut data) = run.log.read(offset, LOG_CHUNK);
                    if data.is_empty() {
                        break;
                    }
                    let next = from.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                    if from > offset {
                        // What the log no longer keeps, said in its place, as hotbed's log does.
                        let dropped =
                            format!("[machined: {} bytes of output dropped]\n", from - offset);
                        data.splice(0..0, dropped.into_bytes());
                    }
                    offset = next;
                    let next_offset = i64::try_from(offset).unwrap_or(i64::MAX);
                    let chunk = pb::LogChunk { data, next_offset, ..pb::LogChunk::default() };
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                if !following {
                    if let Some(state) = ended_as {
                        let next_offset = i64::try_from(offset).unwrap_or(i64::MAX);
                        let last = pb::LogChunk { next_offset, state, ..pb::LogChunk::default() };
                        // The caller may have gone: nothing follows either way.
                        let _ = tx.send(Ok(last)).await;
                    }
                    return;
                }
                if ended(state) {
                    // One more read, of what it wrote before it ended, then its end.
                    ended_as = Some(state.into());
                    following = false;
                    continue;
                }
                tokio::select! {
                    _ = end.changed() => {}
                    _ = progress.changed() => {}
                    () = tx.closed() => return,
                }
            }
        });
        Ok(ReceiverStream::new(rx).boxed())
    }

    async fn get_log_lines(
        self: Arc<Self>,
        req: pb::GetLogLinesRequest,
    ) -> Result<pb::CommandLog, Error> {
        let run = self.run(&req.id)?;
        let tail = usize::try_from(req.tail).ok().filter(|&t| t > 0).unwrap_or(DEFAULT_TAIL);
        let (from, log) = run.log.read(0, console::KEPT);
        let text = String::from_utf8_lossy(&log);
        let mut lines: Vec<&str> = text.split('\n').collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        let mut kept = Vec::new();
        let mut bytes = 0usize;
        for line in lines.iter().rev().take(tail) {
            bytes = bytes.saturating_add(line.len());
            if bytes > MAX_LOG_LINES {
                break;
            }
            kept.push((*line).to_owned());
        }
        kept.reverse();
        // Fewer lines than asked though the output had more: its front dropped, or past 1 MiB.
        let cut = kept.len() < tail && (from > 0 || kept.len() < lines.len());
        Ok(pb::CommandLog { command_id: run.id.clone(), lines: kept, cut })
    }
}

/// A command's output thread: its output into its log until its exit, or why it has none.
fn pump(run: &Run) -> Result<libmachine::agent::ExitStatus, String> {
    use std::io::Write as _;
    let mut log = run.log.writer();
    loop {
        match run.process.next_event() {
            Ok(Some(Event::Stdout(data) | Event::Stderr(data))) => {
                // The log takes everything: a write never fails.
                let _ = log.write_all(&data);
            }
            Ok(Some(Event::Exit(status))) => return Ok(status),
            Ok(None) => return Err("its exit was already taken".into()),
            Err(e) => return Err(errors::status(e).message().to_owned()),
        }
    }
}
