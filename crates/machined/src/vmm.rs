// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! machined's one seam over its machines' VMs: a running machine ([`Vm`]), its vsock socket,
//! stopped. Nothing else of machined touches a VM.
//!
//! Each machine's VM runs in a helper process of its own (`crate::helper`, board 13460), which
//! machined starts from the helper binary beside it and speaks to over the helper's standard
//! input (`helper::wire`): machined keeps no VM, so it runs as many as the host takes
//! (Hypervisor.framework gives a process one), and a helper that dies ends its machine, with what
//! killed it said, while machined lives on. A helper is machined's: it ends with machined's end
//! of its socket.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{oneshot, watch};
use tonic::Status;

use crate::helper::wire::{self, Event, How, event};

/// How a machine ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// [`Vm::stop`] stopped it.
    Stopped,
    Poweroff,
    Reset,
    Shutdown,
    /// Running it failed (the hypervisor's error, an exit libmachine doesn't handle, its helper
    /// gone): why.
    Failed(String),
}

/// A running machine, in its helper. Dropping it stops it, and kills its helper.
pub(crate) struct Vm {
    /// Its helper's process ID.
    pid: u32,
    vsock_socket: PathBuf,
    shared: Arc<Shared>,
    ended: watch::Receiver<Option<Ended>>,
}

/// What a [`Vm`] and the task reading its helper share.
struct Shared {
    /// machined's end of the helper's socket; None once the helper is gone.
    to: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
}

/// The helper binary of this machined: `machined-vm`, beside machined's own.
pub(crate) fn default_helper() -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name("machined-vm"))
}

impl Vm {
    /// Starts `helper` and has it make the machine of `spec` (its paths in `images`), serving its
    /// vsock at `vsock_socket`, its console written to `console`: once the machine runs, or why
    /// it didn't (BOOT_FAILED's detail).
    pub(crate) async fn start(
        helper: &Path,
        spec: crate::proto::MachineSpec,
        images: &Path,
        cladefs_socket: &Path,
        vsock_socket: PathBuf,
        console: impl Write + Send + 'static,
    ) -> Result<Self, Status> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| Status::resource_exhausted(format!("a helper's socket: {e}")))?;
        let mut child = tokio::process::Command::new(helper)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                Status::internal(format!("starting the helper {}: {e}", helper.display()))
            })?;
        let pid = child.id().unwrap_or(0);
        ours.set_nonblocking(true)
            .map_err(|e| Status::internal(format!("a helper's socket: {e}")))?;
        let (mut from, to) = tokio::net::UnixStream::from_std(ours)
            .map_err(|e| Status::internal(format!("a helper's socket: {e}")))?
            .into_split();
        let start = wire::Start {
            protocol: wire::PROTOCOL,
            spec: Some(spec),
            images: images.to_string_lossy().into_owned(),
            vsock_socket: vsock_socket.to_string_lossy().into_owned(),
            cladefs_socket: cladefs_socket.to_string_lossy().into_owned(),
        };
        let shared = Arc::new(Shared { to: tokio::sync::Mutex::new(Some(to)) });
        let sent = async {
            let mut to = shared.to.lock().await;
            wire::write(to.as_mut().expect("just made"), &start).await
        };
        sent.await.map_err(|e| Status::internal(format!("starting the helper: {e}")))?;
        let (started_tx, started) = oneshot::channel();
        let (ended_tx, ended) = watch::channel(None);
        let reading = shared.clone();
        tokio::spawn(async move {
            let how = read_events(&mut from, started_tx, console).await;
            // A helper that said nothing of its end: its exit says why.
            let how = match how {
                Some(how) => how,
                None => {
                    let status = child.wait().await;
                    Ended::Failed(format!(
                        "its helper process ended with no word of the machine ({})",
                        status.map_or_else(|e| e.to_string(), |s| s.to_string())
                    ))
                }
            };
            reading.to.lock().await.take();
            // Its process gone too, before anyone hears its machine ended.
            if let Err(e) = child.wait().await {
                log::warn!("machined: waiting for a helper: {e}");
            }
            ended_tx.send_replace(Some(how));
        });
        match started.await {
            Ok(Ok(())) => {}
            Ok(Err(status)) => return Err(status),
            Err(_) => {
                let mut ended = ended.clone();
                let how = ended.wait_for(Option::is_some).await.ok().and_then(|h| h.clone());
                return Err(Status::failed_precondition(match how {
                    Some(Ended::Failed(why)) => why,
                    other => format!("the machine ended as it started: {other:?}"),
                }));
            }
        };
        Ok(Self { pid, vsock_socket, shared, ended })
    }

    /// Its helper's process ID.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// The Unix socket its helper serves its guest's vsock at (Firecracker's protocol).
    pub(crate) fn vsock_socket(&self) -> &Path {
        &self.vsock_socket
    }

    /// Stops the machine (its helper then frees it and ends): [`wait`](Self::wait) says when.
    pub(crate) fn stop(&self) {
        let shared = self.shared.clone();
        tokio::spawn(async move { send_stop(&shared).await });
    }

    /// Waits until the machine has ended and its helper is gone; says how.
    pub(crate) async fn wait(&self) -> Ended {
        let mut ended = self.ended.clone();
        let how = ended.wait_for(Option::is_some).await.ok().and_then(|h| h.clone());
        how.unwrap_or_else(|| Ended::Failed("its helper's reader went".into()))
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Tells the helper to stop the machine, if it is still there.
async fn send_stop(shared: &Shared) {
    if let Some(to) = shared.to.lock().await.as_mut()
        && let Err(e) = wire::write(to, &wire::Empty {}).await
    {
        log::debug!("machined: a helper's socket: {e}");
    }
}

/// Reads the helper's events until its end: how the machine ended, or None if the helper went
/// without saying.
async fn read_events(
    from: &mut tokio::net::unix::OwnedReadHalf,
    started: oneshot::Sender<Result<(), Status>>,
    mut console: impl Write,
) -> Option<Ended> {
    let mut started = Some(started);
    loop {
        let event = match wire::read::<_, Event>(from).await {
            Ok(Some(Event { kind: Some(event) })) => event,
            Ok(Some(Event { kind: None })) => continue,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("machined: a helper's socket: {e}");
                return None;
            }
        };
        match event {
            event::Kind::Started(_) => {
                if let Some(started) = started.take() {
                    // The starter may have gone; the machine runs, and is stopped as its Vm goes.
                    let _ = started.send(Ok(()));
                }
            }
            event::Kind::Console(data) => {
                // The console keeps what it can: a write never fails.
                let _ = console.write_all(&data);
            }
            event::Kind::Ended(ended) => {
                let how = match How::try_from(ended.how) {
                    Ok(How::Stopped) => Ended::Stopped,
                    Ok(How::Poweroff) => Ended::Poweroff,
                    Ok(How::Reset) => Ended::Reset,
                    Ok(How::Shutdown) => Ended::Shutdown,
                    Ok(How::BootFailed) => {
                        if let Some(started) = started.take() {
                            let status = Status::new(ended.code.into(), ended.detail.clone());
                            let _ = started.send(Err(status));
                        }
                        Ended::Failed(ended.detail)
                    }
                    Ok(How::Failed | How::Unspecified) | Err(_) => Ended::Failed(ended.detail),
                };
                return Some(how);
            }
        }
    }
}
