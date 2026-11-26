// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The caller's decider: a callback that allows or denies the connections
//! an [`Action::Ask`](super::Action::Ask) rule hands it.
//!
//! The contract, so that a slow or stuck callback can hang neither the
//! guest nor the network:
//! - It runs on a thread of its own (one per machine, started at the first
//!   question), never on the network thread: traffic, DNS and the other
//!   connections go on while it thinks.
//! - It is asked one connection at a time, in order. A question whose time
//!   ran out while an earlier one was being answered is not asked at all.
//! - The guest's SYN waits for the answer (its retransmissions are
//!   dropped), at most [`Decider::timeout`]; then the
//!   [default](Decider::default_verdict) applies and a late answer is
//!   ignored.
//! - A callback that panics, or answers `None`, gets the default too.
//! - A connection waiting for an answer counts against the machine's
//!   connection limit.

use std::fmt;
use std::net::Ipv4Addr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use super::policy::Verdict;

/// A connection the decider is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRequest {
    /// The guest's port.
    pub guest_port: u16,
    /// The host address the VMM would connect to (`127.0.0.1` when the
    /// guest connects to its gateway).
    pub addr: Ipv4Addr,
    pub port: u16,
    /// The names this address goes by for the guest: the caller's host
    /// entries, then the names the guest resolved to it through the VMM's
    /// DNS (unexpired).
    pub names: Vec<String>,
    /// The index of the ask rule that matched.
    pub rule: usize,
}

type Callback = dyn Fn(&ConnectionRequest) -> Option<Verdict> + Send + Sync;

/// The callback deciding connections, with how long it may take and what
/// applies if it does not answer.
///
/// ```
/// use std::time::Duration;
/// use libmachine::net::{Decider, Verdict};
/// let decider = Decider::new(|req| Some(if req.port == 443 { Verdict::Allow } else { Verdict::Deny }))
///     .with_timeout(Duration::from_secs(2))
///     .with_default(Verdict::Deny);
/// ```
#[derive(Clone)]
pub struct Decider {
    callback: Arc<Callback>,
    timeout: Duration,
    default: Verdict,
}

impl Decider {
    /// How long a decider may take, unless set otherwise.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

    /// A decider calling `callback`, with a timeout of
    /// [`DEFAULT_TIMEOUT`](Self::DEFAULT_TIMEOUT) and Deny by default.
    pub fn new(
        callback: impl Fn(&ConnectionRequest) -> Option<Verdict> + Send + Sync + 'static,
    ) -> Self {
        Self {
            callback: Arc::new(callback),
            timeout: Self::DEFAULT_TIMEOUT,
            default: Verdict::Deny,
        }
    }

    /// How long the guest's connection waits for an answer.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What applies when the callback does not answer in time, panics or
    /// answers `None`.
    pub fn with_default(mut self, default: Verdict) -> Self {
        self.default = default;
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn default_verdict(&self) -> Verdict {
        self.default
    }

    /// Asks the callback: its answer, or None if it gave none or panicked.
    pub(crate) fn call(&self, request: &ConnectionRequest) -> Option<Verdict> {
        catch_unwind(AssertUnwindSafe(|| (self.callback)(request))).unwrap_or(None)
    }
}

impl fmt::Debug for Decider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decider")
            .field("timeout", &self.timeout)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

/// The same callback (not merely an equal one), timeout and default.
impl PartialEq for Decider {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(
            Arc::as_ptr(&self.callback).cast::<()>(),
            Arc::as_ptr(&other.callback).cast::<()>(),
        ) && self.timeout == other.timeout
            && self.default == other.default
    }
}

impl Eq for Decider {}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ConnectionRequest {
        ConnectionRequest {
            guest_port: 1,
            addr: Ipv4Addr::LOCALHOST,
            port: 2,
            names: vec![],
            rule: 0,
        }
    }

    #[test]
    fn a_panicking_callback_answers_nothing() {
        let d = Decider::new(|_| panic!("the callback failed"));
        assert_eq!(d.call(&request()), None);
        let d = Decider::new(|r| Some(if r.port == 2 { Verdict::Allow } else { Verdict::Deny }));
        assert_eq!(d.call(&request()), Some(Verdict::Allow));
        assert_eq!(d.timeout(), Decider::DEFAULT_TIMEOUT);
        assert_eq!(d.default_verdict(), Verdict::Deny);
        assert_eq!(d, d.clone());
        assert_ne!(d, d.clone().with_default(Verdict::Allow));
        assert_ne!(d, Decider::new(|_| None));
    }
}
