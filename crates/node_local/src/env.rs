// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a daemon's environment, which its configuration is read from (doc-24, TASK-685.9):
//! the process's, as [`std::env`] reads it. Under `--cfg madsim`, a simulated node given an
//! environment of its own ([`set_vars`]) reads that instead, so that daemons in one simulation
//! are configured apart, each as its own process would be, by the same code: its socket, its
//! token, its peers' address, its directories. A node killed loses it ([`crate::kill`]): its
//! restart gives it again, as a host's boot does.
//!
//! cladefs: a process embedding the daemon (libcladefs, board 13471) configures it through
//! [`set_embedded_vars`] instead: variables read over the process's own, as setting the process's
//! environment is unsound once it runs threads, as an embedder's process does.

use std::env::VarError;
use std::ffi::{OsStr, OsString};

/// The variable `name` of the environment, as [`std::env::var_os`]: the simulated node's, or the
/// embedder's ([`set_embedded_vars`]) over the process's.
pub fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    let name = name.as_ref();
    #[cfg(madsim)]
    {
        let of = key();
        let all = VARS.lock().unwrap();
        if let Some((_, vars)) = all.iter().find(|(k, _)| *k == of) {
            return vars.iter().find(|(k, _)| k == name).map(|(_, value)| value.clone());
        }
    }
    process_var_os(name)
}

/// The variable `name` of the process's environment, with the embedder's ([`set_embedded_vars`])
/// over it: never a simulated node's, for what is read once a process (a compression level).
pub fn process_var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    let name = name.as_ref();
    let embedded = EMBEDDED.read().unwrap();
    match embedded.iter().find(|(k, _)| k == name) {
        Some((_, value)) => Some(value.clone()),
        None => std::env::var_os(name),
    }
}

/// Every variable of the environment, as [`std::env::vars_os`]: the simulated node's, if it was
/// given one, as [`var_os`] reads it, so that what reads them all (a KMS's variables and what
/// they trust, a bucket's options) is each simulated host's own, as each host's process's is
/// (board 14521); else the process's, with the embedder's ([`set_embedded_vars`]) over it, each
/// name once.
pub fn vars_os() -> Vec<(OsString, OsString)> {
    #[cfg(madsim)]
    {
        let of = key();
        let all = VARS.lock().unwrap();
        if let Some((_, vars)) = all.iter().find(|(k, _)| *k == of) {
            return vars.clone();
        }
    }
    let embedded = EMBEDDED.read().unwrap();
    let process = std::env::vars_os().filter(|(name, _)| !embedded.iter().any(|(k, _)| k == name));
    embedded.iter().cloned().chain(process).collect()
}

/// The embedder's variables, read over the process's ([`set_embedded_vars`]). The process's, not
/// a host's: one daemon is embedded in a process (libcladefs refuses a second), and the
/// simulation's nodes read their own ([`set_vars`]) before these.
static EMBEDDED: std::sync::RwLock<Vec<(OsString, OsString)>> = std::sync::RwLock::new(Vec::new());

/// Has [`var_os`] and [`vars_os`] read `vars` over the process's environment from now on, in place
/// of any given before: the configuration of a daemon embedded in this process (libcladefs, board
/// 13471), given before it boots, without `std::env::set_var`, unsound in a process running
/// threads.
pub fn set_embedded_vars<K: Into<OsString>, V: Into<OsString>>(
    vars: impl IntoIterator<Item = (K, V)>,
) {
    let vars = vars.into_iter().map(|(name, value)| (name.into(), value.into())).collect();
    *EMBEDDED.write().unwrap() = vars;
}

/// The variable `name` of the environment, as [`std::env::var`]: not there, or not Unicode.
pub fn var(name: impl AsRef<OsStr>) -> Result<String, VarError> {
    match var_os(name) {
        Some(value) => value.into_string().map_err(VarError::NotUnicode),
        None => Err(VarError::NotPresent),
    }
}

#[cfg(madsim)]
type Key = (std::thread::ThreadId, Option<madsim::task::NodeId>);

#[cfg(madsim)]
fn key() -> Key {
    let node = madsim::runtime::Handle::try_current().ok().map(|_| madsim::plugin::node());
    (std::thread::current().id(), node)
}

/// Each simulated node's environment, if it was given one ([`set_vars`]).
#[cfg(madsim)]
static VARS: std::sync::Mutex<Vec<(Key, Vec<(OsString, OsString)>)>> =
    std::sync::Mutex::new(Vec::new());

/// Has the simulated node this runs in read `vars` as its environment, in place of the
/// process's, until it is killed: a node's `init` gives it first, as each boot of a host does.
/// Outside a node, the process's own reads them, as a test's code does.
#[cfg(madsim)]
pub fn set_vars<K: Into<OsString>, V: Into<OsString>>(vars: impl IntoIterator<Item = (K, V)>) {
    let of = key();
    let vars: Vec<(OsString, OsString)> =
        vars.into_iter().map(|(name, value)| (name.into(), value.into())).collect();
    let mut all = VARS.lock().unwrap();
    match all.iter_mut().find(|(k, _)| *k == of) {
        Some((_, existing)) => *existing = vars,
        None => all.push((of, vars)),
    }
}

/// Clears `node`'s simulated environment ([`crate::reset`]).
#[cfg(madsim)]
pub(crate) fn clear(node: crate::Node) {
    VARS.lock().unwrap().retain(|(of, _)| *of != (node.0, Some(node.1)));
}
