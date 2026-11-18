// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The tenants granted VM sockets (TASK-43.4), which the control API's Grant and Revoke make and
//! take away: a socket of the tenant's own, in the daemon's directory of them, for the host's VM
//! manager to bind-mount into the tenant's jails. A connection's tenant is the socket it came in
//! on. Grants live as long as the daemon: after a restart, the VM manager grants again.

use super::server::{Tenant, key_in_dir, serve_tenant};
use crate::Errno;
use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The tenants granted, by ID, and the directory their sockets are in.
pub(super) struct Grants {
    sockets: PathBuf,
    granted: Mutex<HashMap<u64, Granted>>,
}

/// A tenant granted its socket.
struct Granted {
    tenant: Arc<Tenant>,
    socket: PathBuf,
    /// The socket's listener, which a revoke shuts down, so that its thread stops accepting.
    listener: UnixListener,
}

impl Grants {
    /// No tenants yet, whose sockets go in the directory `sockets`.
    pub fn new(sockets: PathBuf) -> Self {
        Self { sockets, granted: Mutex::new(HashMap::new()) }
    }

    /// Grants the tenant of the ID `id` in the bucket (TASK-364), not the default tenant's
    /// (`EINVAL`), whose stores are in `dir`, its socket, given to `uid` and `gid` (the daemon's
    /// own, where not given), and returns its path: a new tenant's, which a thread of its own
    /// serves; or, granted already, its socket, given again (`EEXIST` with another directory). A
    /// tenant revoked whose VM processes live on is the one granted again, its socket new: those it
    /// connects then share its arena with them, and the stores they have open (TASK-79.36), which,
    /// another tenant's, they could not.
    pub fn grant(
        &self,
        id: u64,
        dir: &Path,
        (uid, gid): (Option<u32>, Option<u32>),
    ) -> Result<PathBuf, Errno> {
        check_tenant(id)?;
        let dir = dir.canonicalize().map_err(errno)?;
        let mut granted = self.granted.lock().unwrap();
        if let Some(grant) = granted.get(&id) {
            if grant.tenant.dir() != Some(&dir) {
                return Err(Errno::EEXIST);
            }
            give(&grant.socket, uid, gid)?;
            return Ok(grant.socket.clone());
        }
        let tenant = match super::memory::alive(id, &dir) {
            Some(tenant) => tenant,
            None => Tenant::new(id, &dir).map_err(errno)?,
        };
        let socket = self.sockets.join(socket_name(id));
        remove_stale(&socket)?;
        // Its mode and owner from the moment it is there, not the umask's until given (TASK-159).
        let listener = super::bind_given(&socket, MODE, uid, gid).map_err(errno)?;
        let served = listener.try_clone().map_err(errno).and_then(|accepting| {
            let serving = tenant.clone();
            std::thread::Builder::new()
                .name("cladefsd tenant".into())
                .spawn(move || serve_tenant(accepting, serving))
                .map_err(errno)
        });
        if let Err(errno) = served {
            let _ = std::fs::remove_file(&socket);
            return Err(errno);
        }
        let grant = Granted { tenant, socket: socket.clone(), listener };
        granted.insert(id, grant);
        Ok(socket)
    }

    /// The ID in the bucket of the granted tenant whose directory the store at `path` is in, if
    /// any (TASK-364).
    pub fn tenant_at(&self, path: &Path) -> Option<u64> {
        let key = super::cells::store_key(path);
        let granted = self.granted.lock().unwrap();
        let mut grants = granted.values().map(|grant| &grant.tenant);
        grants.find(|tenant| tenant.dir().is_some_and(|dir| key_in_dir(&key, dir))).map(|t| t.id())
    }

    /// Whether the stores at `one` and `other` are of one tenant, or of none: their paths in one
    /// tenant's directory, as VM processes name them.
    pub fn same_tenant(&self, one: &Path, other: &Path) -> bool {
        let one = super::cells::store_key(one);
        let other = super::cells::store_key(other);
        let granted = self.granted.lock().unwrap();
        let tenant_of = |path: &Path| {
            granted
                .values()
                .filter_map(|grant| grant.tenant.dir())
                .find(|dir| key_in_dir(path, dir))
        };
        tenant_of(&one) == tenant_of(&other)
    }

    /// Revokes the grant of the tenant of the ID `id` (`ENOENT` if it has none): its socket goes,
    /// and takes no more connections. The tenant goes once its VM processes connected do.
    pub fn revoke(&self, id: u64) -> Result<(), Errno> {
        let mut granted = self.granted.lock().unwrap();
        let grant = granted.remove(&id).ok_or(Errno::ENOENT)?;
        if let Err(err) = std::fs::remove_file(&grant.socket) {
            log::warn!("cladefsd: removing {}: {err}", grant.socket.display());
        }
        drop(granted);
        // Its thread's accept fails with `EINVAL`, and it stops.
        super::shut_down(std::os::fd::AsFd::as_fd(&grant.listener));
        Ok(())
    }
}

/// `EINVAL` if `tenant` is the default tenant's ID, which no grant has, and so no limit, weight or
/// bytes in flight of its own: the default tenant is every tenant's with them.
pub(super) fn check_tenant(tenant: u64) -> Result<(), Errno> {
    match tenant {
        crate::host_cell::DEFAULT_TENANT => Err(Errno::EINVAL),
        _ => Ok(()),
    }
}

/// The name of the socket of the tenant `tenant`, in the daemon's directory of them: its ID, in
/// base 32, as cells name it.
pub(super) fn socket_name(tenant: u64) -> String {
    format!("{tenant}.sock", tenant = base32_id::Short(tenant))
}

/// A tenant's socket's mode: its owner and group alone connect, those it is given to.
const MODE: u32 = 0o660;

/// Gives the socket at `socket` to `uid` and `gid`, where given, connectable by them alone.
fn give(socket: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
    std::os::unix::fs::chown(socket, uid, gid).map_err(errno)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(MODE)).map_err(errno)
}

/// Removes the socket at `socket` that a daemon before this one left, whose grants went with it:
/// `EEXIST` if something else is there.
fn remove_stale(socket: &Path) -> Result<(), Errno> {
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(socket).map_err(errno)
        }
        Ok(_) => Err(Errno::EEXIST),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(errno(err)),
    }
}

/// The errno of `err`, or `EIO`.
fn errno(err: std::io::Error) -> Errno {
    err.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

#[cfg(test)]
mod tests {
    use super::{check_tenant, socket_name};
    use crate::Errno;

    /// Tenants' IDs name their sockets, in base 32; the default tenant's is refused.
    #[test]
    fn ids_name_sockets() {
        assert_eq!(socket_name(0xa), "a.sock");
        assert_eq!(socket_name(u64::MAX), "fzzzzzzzzzzzz.sock");
        assert_eq!(check_tenant(1), Ok(()));
        assert_eq!(check_tenant(crate::host_cell::DEFAULT_TENANT), Err(Errno::EINVAL));
    }
}
