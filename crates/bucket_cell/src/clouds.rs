// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the simulation's clouds (doc-24, board 14521): under madsim, the object stores the
//! buckets' URLs name, which every simulated host reaches as it reaches a cloud's, by the URL.
//!
//! A `memory:///NAME` URL names a store of the process that opens it, a new one each time it is
//! opened: two hosts in one simulation, each opening its bucket by the URL as production does
//! (`CLADEFSD_BUCKET`, or an entry of the registry of buckets, [`crate::object_bucket::Registered`]),
//! would reach two stores, sharing nothing. So a simulation says what store a URL names
//! ([`serve`]) before its hosts boot, as a cloud serves a bucket before its hosts do: every
//! opening of that URL in the simulation then reaches that one store, through every layer a
//! bucket's requests go through otherwise. What a simulation serves is its to degrade
//! ([`crate::faults::Seeded`]), a cloud region's link each. Its appends, which a bucket of a
//! process's memory takes one at a time, are one at a time across its openings too, as a cloud's
//! are across its hosts.
//!
//! A URL no simulation serves names a new store, as outside madsim.

use object_store::ObjectStore;
use std::sync::{Arc, Mutex};

/// What each simulation serves, by its URL: the simulation's, by its thread, as madsim runs each
/// on one, so that simulations sharing a process share none; never a simulated node's, as a
/// cloud is no host's.
static SERVED: Mutex<Vec<(std::thread::ThreadId, String, Cloud)>> = Mutex::new(Vec::new());

/// A store served, and what its appends take turns by.
pub(crate) type Cloud = (Arc<dyn ObjectStore>, Arc<tokio::sync::Mutex<()>>);

/// `url`, as [`url::Url`] spells it: what [`serve`] and [`at`] know it by.
fn spelled(url: &str) -> String {
    url::Url::parse(url).unwrap_or_else(|err| panic!("{url}: {err}")).as_str().to_owned()
}

/// Has every opening of `url` in this simulation, from now on, reach `objects`, in place of what
/// it reached before.
pub fn serve(url: &str, objects: Arc<dyn ObjectStore>) {
    let (simulation, url) = (std::thread::current().id(), spelled(url));
    let mut served = SERVED.lock().unwrap();
    served.retain(|(of, at, _)| (*of, at) != (simulation, &url));
    served.push((simulation, url, (objects, Arc::default())));
}

/// What this simulation serves at `url`, if anything.
pub(crate) fn at(url: &url::Url) -> Option<Cloud> {
    let simulation = std::thread::current().id();
    let served = SERVED.lock().unwrap();
    let found = served.iter().find(|(of, at, _)| *of == simulation && at == url.as_str());
    found.map(|(_, _, cloud)| cloud.clone())
}
