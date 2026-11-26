// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Every request's checks, once, where it enters (AGENTS.md, trust boundaries): what a request
//! asks for, decoded into values that mean something, or refused with INVALID_ARGUMENT (NOT_FOUND
//! for a kernel not there, UNIMPLEMENTED for what this build can't do yet). Past here, machined
//! relies on them.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use libmachine::agent::Command;
use libmachine::net::{EgressPolicy, HostPattern, NetworkConfig, Ports, Rule};
use libmachine::{CellId, ExtentCell, ExtentStore, LinuxBoot, MachineConfig};
use tonic::Status;

use crate::proto;

/// The longest name of a machine or snapshot.
const MAX_NAME: usize = 63;
/// The longest path, of the guest's or in the images directory.
const MAX_PATH: usize = 4096;
/// The longest kernel command line a request gives.
const MAX_CMDLINE: usize = 2048;
const MAX_LABELS: usize = 32;
const MAX_LABEL_KEY: usize = 63;
const MAX_LABEL_VALUE: usize = 256;
const MAX_VSOCK_PORTS: usize = 16;
const MAX_EGRESS_RULES: usize = 256;
/// The most arguments and environment entries of a command, together.
const MAX_COMMAND_ITEMS: usize = 4096;
/// The most bytes of a command's arguments, environment and working directory, together.
const MAX_COMMAND_BYTES: usize = 128 << 10;
/// The largest Linux signal number.
const MAX_SIGNAL: i32 = 64;
/// Guest RAM unless a spec says otherwise.
const DEFAULT_MEMORY: u64 = 256 << 20;
/// The guest's page size, which RAM is a multiple of.
const PAGE: u64 = 4096;

fn invalid(message: impl Into<String>) -> Status {
    Status::invalid_argument(message)
}

/// A machine's or snapshot's name: 1 to 63 bytes of a-z, 0-9, '.', '_' and '-', starting with a
/// letter or digit.
pub(crate) fn name(what: &str, name: &str) -> Result<(), Status> {
    let bytes = name.as_bytes();
    let first = bytes.first().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest =
        bytes.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c));
    if first && rest && bytes.len() <= MAX_NAME {
        Ok(())
    } else {
        Err(invalid(format!(
            "{what} {name:?}: 1 to {MAX_NAME} bytes of a-z, 0-9, '.', '_' and '-', starting with a \
             letter or digit"
        )))
    }
}

/// A path in the guest: absolute, at most [`MAX_PATH`] bytes, no NUL.
pub(crate) fn guest_path(path: &str) -> Result<(), Status> {
    if path.starts_with('/') && path.len() <= MAX_PATH && !path.contains('\0') {
        Ok(())
    } else {
        Err(invalid(format!("guest path {path:?}: absolute, at most {MAX_PATH} bytes, no NUL")))
    }
}

/// A Linux signal number.
pub(crate) fn signal(signal: i32) -> Result<i32, Status> {
    if (1..=MAX_SIGNAL).contains(&signal) {
        Ok(signal)
    } else {
        Err(invalid(format!("signal {signal}: from 1 to {MAX_SIGNAL}")))
    }
}

/// A file's permissions.
pub(crate) fn mode(mode: u32) -> Result<u32, Status> {
    if mode <= 0o7777 { Ok(mode) } else { Err(invalid(format!("mode {mode:#o}: at most 07777"))) }
}

/// A file in `images`, named relative to it, which it does not leave, symbolic links followed
/// included: its canonical path.
fn image(images: &Path, what: &str, name: &str) -> Result<PathBuf, Status> {
    let relative = Path::new(name);
    let plain = relative.components().all(|c| matches!(c, Component::Normal(_)));
    if name.is_empty() || name.len() > MAX_PATH || name.contains('\0') || !plain {
        return Err(invalid(format!(
            "{what} {name:?}: a path relative to the images directory, with no '..' and at most \
             {MAX_PATH} bytes"
        )));
    }
    let not_there = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(format!("{what} {name:?}: not there")),
        _ => Status::invalid_argument(format!("{what} {name:?}: {e}")),
    };
    let root = std::fs::canonicalize(images)
        .map_err(|e| Status::internal(format!("the images directory {}: {e}", images.display())))?;
    let path = std::fs::canonicalize(root.join(relative)).map_err(not_there)?;
    if !path.starts_with(&root) {
        return Err(invalid(format!("{what} {name:?}: leaves the images directory")));
    }
    if !path.is_file() {
        return Err(invalid(format!("{what} {name:?}: not a file")));
    }
    Ok(path)
}

/// A machine's labels.
pub(crate) fn labels(labels: &HashMap<String, String>) -> Result<(), Status> {
    if labels.len() > MAX_LABELS {
        return Err(invalid(format!("{} labels: at most {MAX_LABELS}", labels.len())));
    }
    for (key, value) in labels {
        if key.is_empty() || key.len() > MAX_LABEL_KEY || value.len() > MAX_LABEL_VALUE {
            return Err(invalid(format!(
                "label {key:?}: its key 1 to {MAX_LABEL_KEY} bytes, its value at most \
                 {MAX_LABEL_VALUE}"
            )));
        }
    }
    Ok(())
}

/// A machine's vsock listen ports: at most [`MAX_VSOCK_PORTS`], none twice.
pub(crate) fn vsock_ports(ports: &[u32]) -> Result<(), Status> {
    let distinct: BTreeSet<_> = ports.iter().collect();
    if ports.len() > MAX_VSOCK_PORTS || distinct.len() != ports.len() {
        return Err(invalid(format!(
            "vsock_listen_ports: at most {MAX_VSOCK_PORTS}, none twice; got {ports:?}"
        )));
    }
    Ok(())
}

/// What a spec makes: the machine, what it boots, and its vsock ports.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) config: MachineConfig,
    pub(crate) boot: LinuxBoot,
}

/// A spec, checked, made into what machined starts.
pub(crate) fn plan(images: &Path, spec: &proto::MachineSpec) -> Result<Plan, Status> {
    let vcpus = match spec.vcpus {
        0 => 1,
        n if n <= MachineConfig::MAX_VCPUS => n,
        n => return Err(invalid(format!("{n} vCPUs: at most {}", MachineConfig::MAX_VCPUS))),
    };
    let memory = match spec.memory_bytes {
        0 => DEFAULT_MEMORY,
        n if n.is_multiple_of(PAGE) => n,
        n => return Err(invalid(format!("memory_bytes {n}: a multiple of {PAGE}"))),
    };
    let mem_size = usize::try_from(memory)
        .map_err(|_| invalid(format!("memory_bytes {memory}: more than this host can address")))?;
    let boot = spec.boot.as_ref().ok_or_else(|| invalid("a spec boots something: boot"))?;
    if boot.command_line.len() > MAX_CMDLINE || boot.command_line.contains(['\0', '\n']) {
        return Err(invalid(format!(
            "command_line: at most {MAX_CMDLINE} bytes, no NUL or newline"
        )));
    }
    let kernel = image(images, "kernel", &boot.kernel)?;
    let initrd = match boot.initramfs.as_str() {
        "" => None,
        initramfs => Some(image(images, "initramfs", initramfs)?),
    };
    let network = spec.network.as_ref().map(network).transpose()?;
    let root = root(spec)?;
    vsock_ports(&spec.vsock_listen_ports)?;
    labels(&spec.labels)?;
    Ok(Plan {
        config: MachineConfig {
            mem_size,
            vcpus,
            network,
            // machined keeps the console itself (src/console.rs).
            serial_capture_limit: 0,
            // The daemon its helper connects to is the helper's to name (`crate::helper`).
            extent_stores: vec![root],
            balloon: spec.balloon,
            ..MachineConfig::default()
        },
        boot: LinuxBoot {
            kernel,
            initrd,
            cmdline: format!("root={ROOT_TAG} rootfstype=cladefs rw {}", boot.command_line),
        },
    })
}

/// The extent store device's tag a machine's root is mounted from (`root=`).
const ROOT_TAG: &str = "root";
/// A DAX window's size is a multiple of this.
const DAX_ALIGN: u64 = 2 << 20;

/// A machine's root: the cell its spec names, on an extent store device, which its kernel mounts
/// its Fxfs root from. Every machine boots from a cell: there is no machine without one.
fn root(spec: &proto::MachineSpec) -> Result<ExtentStore, Status> {
    let root = spec.root.as_ref().ok_or_else(|| {
        invalid("a machine boots from a cell: its spec names its root (CreateCell, ImportImage or ForkCell of the Cladefsd service make one)")
    })?;
    if root.cell == 0 {
        return Err(invalid("root: cell 0 names no cell"));
    }
    let dax_window_size = match spec.root_dax_window_bytes {
        0 => None,
        n if n.is_multiple_of(DAX_ALIGN) => Some(n),
        n => {
            return Err(invalid(format!(
                "root_dax_window_bytes {n}: a multiple of {DAX_ALIGN}, or 0 for none"
            )));
        }
    };
    let cell = ExtentCell::Existing(CellId { tenant: root.tenant, id: root.cell });
    Ok(ExtentStore { dax_window_size, ..ExtentStore::new(ROOT_TAG, cell, 0) })
}

/// A machine's network: its egress rules, as libmachine's policy.
fn network(network: &proto::Network) -> Result<NetworkConfig, Status> {
    if !network.port_forwards.is_empty() {
        return Err(Status::unimplemented(
            "port forwards wait on libmachine's network taking inbound connections (board 13486)",
        ));
    }
    if network.egress.len() > MAX_EGRESS_RULES {
        return Err(invalid(format!(
            "{} egress rules: at most {MAX_EGRESS_RULES}",
            network.egress.len()
        )));
    }
    let mut policy = EgressPolicy::deny_all();
    for rule in &network.egress {
        let action = match proto::EgressAction::try_from(rule.action) {
            Ok(proto::EgressAction::Allow) => libmachine::net::Action::Allow,
            Ok(proto::EgressAction::Deny) => libmachine::net::Action::Deny,
            Ok(proto::EgressAction::Unspecified) | Err(_) => {
                return Err(invalid(format!("egress rule {:?}: no action", rule.host)));
            }
        };
        let host: HostPattern = rule
            .host
            .parse()
            .map_err(|e| invalid(format!("egress rule host {:?}: {e}", rule.host)))?;
        let ports = match (rule.first_port, rule.last_port) {
            (0, 0) => Ports::ANY,
            (first, last) => {
                let port = |p: u32| {
                    u16::try_from(p).ok().filter(|&p| p != 0).ok_or_else(|| {
                        invalid(format!("egress rule port {p}: from 1 to 65535, or 0 and 0"))
                    })
                };
                Ports::range(port(first)?, port(last)?).map_err(|e| invalid(e.to_string()))?
            }
        };
        policy.push(Rule { action, host, ports });
    }
    let config = NetworkConfig::new(policy);
    config.validate().map_err(|e| invalid(e.to_string()))?;
    Ok(config)
}

/// The command an Exec starts.
pub(crate) fn command(start: &proto::ExecStart) -> Result<Command, Status> {
    let Some((program, args)) = start.argv.split_first() else {
        return Err(invalid("argv: at least the program"));
    };
    let items = start.argv.len().saturating_add(start.env.len());
    let bytes = start
        .argv
        .iter()
        .chain(start.env.keys())
        .chain(start.env.values())
        .chain([&start.working_directory])
        .map(String::len)
        .fold(0usize, usize::saturating_add);
    if items > MAX_COMMAND_ITEMS || bytes > MAX_COMMAND_BYTES {
        return Err(invalid(format!(
            "a command of {items} arguments and variables, {bytes} bytes: at most \
             {MAX_COMMAND_ITEMS}, {MAX_COMMAND_BYTES} bytes"
        )));
    }
    let nul = |s: &String| s.contains('\0');
    if start.argv.iter().any(nul) || start.env.values().any(nul) || nul(&start.working_directory) {
        return Err(invalid("a command's arguments, environment and directory hold no NUL"));
    }
    if start.env.keys().any(|k| k.is_empty() || k.contains(['=', '\0'])) {
        return Err(invalid("environment names: not empty, no '=' or NUL"));
    }
    if program.is_empty() {
        return Err(invalid("argv: the program is not empty"));
    }
    let mut command = Command::new(program);
    command.args(args);
    if !start.env.is_empty() {
        command.env_clear();
        for (key, value) in &start.env {
            command.env(key, value);
        }
    }
    if !start.working_directory.is_empty() {
        command.current_dir(&start.working_directory);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names at and past their edges.
    #[test]
    fn names() {
        for ok in ["a", "0", "ws-3f2a01", "a.b_c-d", &"a".repeat(MAX_NAME)] {
            assert!(name("machine", ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", ".a", "_a", "A", "a/b", "a b", "é", "a\0", &"a".repeat(MAX_NAME + 1)]
        {
            assert_eq!(
                name("machine", bad).unwrap_err().code(),
                tonic::Code::InvalidArgument,
                "{bad}"
            );
        }
    }

    /// No image path leaves the images directory: not by '..', not absolute, not by a symbolic
    /// link.
    #[test]
    fn images_stay_in_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        std::fs::create_dir(&images).unwrap();
        std::fs::write(images.join("vmlinux"), b"k").unwrap();
        std::fs::write(dir.path().join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), images.join("link")).unwrap();
        std::fs::create_dir(images.join("sub")).unwrap();

        assert!(image(&images, "kernel", "vmlinux").unwrap().ends_with("vmlinux"));
        for (bad, code) in [
            ("", tonic::Code::InvalidArgument),
            ("../secret", tonic::Code::InvalidArgument),
            ("sub/../vmlinux", tonic::Code::InvalidArgument),
            ("./vmlinux", tonic::Code::InvalidArgument),
            ("/etc/passwd", tonic::Code::InvalidArgument),
            ("link", tonic::Code::InvalidArgument),
            ("sub", tonic::Code::InvalidArgument),
            ("nope", tonic::Code::NotFound),
            ("a\0b", tonic::Code::InvalidArgument),
        ] {
            assert_eq!(image(&images, "kernel", bad).unwrap_err().code(), code, "{bad:?}");
        }
    }
}
