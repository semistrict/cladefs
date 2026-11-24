// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a daemon's environment, which its configuration is read from (doc-24, TASK-685.9):
//! the process's, as [`std::env`] reads it. Under `--cfg madsim`, a simulated node given an
//! environment of its own ([`set_vars`]) reads that instead, so that daemons in one simulation
//! are configured apart, each as its own process would be, by the same code: its socket, its
//! token, its peers' address, its directories. A node killed loses it ([`crate::kill`]): its
//! restart gives it again, as a host's boot does.

use std::env::VarError;
use std::ffi::{OsStr, OsString};

/// The variable `name` of the environment, as [`std::env::var_os`]: the process's, or the
/// simulated node's.
pub fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    #[cfg(madsim)]
    {
        let of = key();
        let all = VARS.lock().unwrap();
        if let Some((_, vars)) = all.iter().find(|(k, _)| *k == of) {
            return vars.iter().find(|(k, _)| k == name.as_ref()).map(|(_, value)| value.clone());
        }
    }
    std::env::var_os(name)
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
