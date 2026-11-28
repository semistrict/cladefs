// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the madsim world's workload families, ported from local_cell::sim's steps (AGENTS.md,
//! one simulator; boards 14565-14568), one module each. A family's module says, of a round of the
//! attrition, what it does beside the round's writer (`during`, its tasks aborted at the heal), and,
//! the buckets, disks and network whole again, what it checks (`healed`). Its rule searches
//! (`needs!`) are its own too.

use super::{Profile, Told};
use bucket_cell::object_bucket::RoutedBucket;
use std::sync::Arc;

pub(crate) mod collector;
pub(crate) mod forks;
pub(crate) mod stale;
pub(crate) mod zombies;

/// A round of the attrition, as its families see it.
pub(crate) struct Round<'a> {
    pub profile: &'a Profile,
    /// The VM manager's node, which calls the hosts' control APIs.
    pub manager: &'a madsim::runtime::NodeHandle,
    /// The hosts' nodes, host `h` at `h - 1`, as first made (a host started again is a node anew:
    /// `madsim::runtime::Handle::current().get_node(nodes[i].id())` is the one running).
    pub nodes: &'a [madsim::runtime::NodeHandle],
    pub bucket: &'a Arc<RoutedBucket>,
    /// The cold bucket's faults, which a family may set a while (restoring `crate::ODDS`).
    pub faults: &'a Arc<bucket_cell::faults::Seeded>,
    /// The zones' fast WAL buckets, of a run with fast WAL cells.
    pub fast: Option<&'a Arc<crate::FastBuckets>>,
    /// The cell the writers write.
    pub cell: u64,
    pub round: u64,
    /// The host (0-based) whose writer writes this round, having taken the cell.
    pub host: usize,
    pub told: &'a Arc<Told>,
}

/// The attrition healed, its last host (1) having taken the cell, as its families see it.
pub(crate) struct Healed<'a> {
    pub profile: &'a Profile,
    pub manager: &'a madsim::runtime::NodeHandle,
    pub nodes: &'a [madsim::runtime::NodeHandle],
    pub bucket: &'a Arc<RoutedBucket>,
    pub cell: u64,
    pub told: &'a Arc<Told>,
}

/// What the families `round.profile` has do beside its writer.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    let profile = round.profile;
    let mut tasks = Vec::new();
    if profile.zombies {
        tasks.extend(zombies::during(round));
    }
    if profile.stale {
        tasks.extend(stale::during(round));
    }
    if profile.forks {
        tasks.extend(forks::during(round));
    }
    if profile.collector_dies {
        tasks.extend(collector::during(round));
    }
    tasks
}

/// What the families `healed.profile` has check once healed: what one found lost, if any.
pub(crate) async fn healed(healed: &Healed<'_>) -> Result<(), String> {
    let profile = healed.profile;
    if profile.zombies {
        zombies::healed(healed).await?;
    }
    if profile.stale {
        stale::healed(healed).await?;
    }
    if profile.forks {
        forks::healed(healed).await?;
    }
    if profile.collector_dies {
        collector::healed(healed).await?;
    }
    Ok(())
}
