// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: writers compact (doc-27, Writers compact; board 6214). At each checkpoint round the
//! uploader relocates the live extents of its cell's sparse segments, under half live
//! ([`LocalCell::sparse`]), into the cell's open segment, in a commit of the open cell's own
//! ([`LocalCell::relocate`]), so that the collector, which compacts nothing, deletes them whole
//! once no head, version or origin names them. Within a budget ([`Credit`]): no more bytes
//! copied than the cell uploaded of its own since, so that relocation at most doubles what the
//! host writes; at most [`MOST`] a round, which is what the round holds in memory; and none while
//! the cell's disk is under pressure. Not the segments a kept state reads ([`Pins`]): the state
//! at each version the head names and at each fork's origin under the cell's references, which
//! keep them whole, so that relocating out of them would copy what stays anyway.

use crate::{Dir, LocalCell, PartlyRelocated, Relocated, Relocating};
use bucket_cell::fetch::{Budget, Fetcher, Limits, Memory, Priority};
use bucket_cell::layout::CellId;
use bucket_cell::{Bucket, Writer, lifecycle, recovery};
use futures::{StreamExt as _, TryStreamExt as _};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// The most bytes a round relocates: what it holds read meanwhile (doc-27: `UPLOAD_BYTES / 4`).
pub(crate) const MOST: u64 = crate::upload::UPLOAD_BYTES / 4;

/// How many extents a round reads at once, and, with no budget of the host's fetchers, how many
/// GETs it has in flight, of a part each ([`Budgets`]).
const AT_ONCE: usize = 8;

/// What a round's reads go within: a place in the budget of the host's uploads for what it holds
/// read until relocated, behind the uploads; and its fetcher's, the host's fetchers' budget,
/// memory and cache, if it has them (doc-27; Codex's reviews of fa1912379 and 10149e8e3), so that
/// what it fetches is governed and shared; else its own, of [`AT_ONCE`] parts, keeping none: the
/// tests' and tools', which read each extent's parts anew.
pub(crate) struct Budgets<'a> {
    pub uploads: &'a Arc<Budget>,
    pub fetching: Option<crate::upload::Fetching>,
    pub cache: Option<Arc<bucket_cell::cache::Cache>>,
}

/// What a cell's rounds may copy: the bytes it uploaded of its own since, less those its rounds
/// copied. What a round relocated, uploaded in the segments after, counts as none of its own.
#[derive(Debug, Default)]
pub(crate) struct Credit {
    credit: u64,
    /// Relocated bytes not yet counted off the uploads that carry them.
    owed: u64,
}

impl Credit {
    /// A segment of `len` bytes of data is uploaded.
    pub(crate) fn uploaded(&mut self, len: u64) {
        let relocated = self.owed.min(len);
        self.owed -= relocated;
        self.credit = self.credit.saturating_add(len - relocated);
    }

    /// What the next round may copy, taken from the credit.
    pub(crate) fn take(&mut self) -> u64 {
        let allowance = self.credit.min(MOST);
        self.credit -= allowance;
        allowance
    }

    /// A round given `allowance` relocated `relocated` of it, failing or not: the rest goes back.
    pub(crate) fn spent(&mut self, allowance: u64, relocated: u64) {
        let relocated = relocated.min(allowance);
        self.credit = self.credit.saturating_add(allowance - relocated);
        self.owed = self.owed.saturating_add(relocated);
    }
}

/// The keys the states a cell keeps read, by the segment each is at, each read once while the
/// uploader runs: a version's state, or a fork's origin's, never changes.
#[derive(Default)]
pub(crate) struct Pins {
    at: Mutex<HashMap<u64, Arc<HashSet<String>>>>,
}

impl Pins {
    /// The cell's own segments the states at `ats` read: each state read once, holding `writer`'s
    /// keys; those at other segments let go.
    async fn of<B: Bucket>(
        &self,
        bucket: &B,
        writer: &Writer<B>,
        ats: &BTreeSet<u64>,
    ) -> Result<HashSet<String>, bucket_cell::Error> {
        let cell = writer.cell();
        self.at.lock().unwrap().retain(|at, _| ats.contains(at));
        let mut pinned = HashSet::new();
        for &at in ats {
            let known = self.at.lock().unwrap().get(&at).cloned();
            let keys = match known {
                Some(keys) => keys,
                None => match recovery::recover_version(bucket, cell, at, writer.keys()).await {
                    Ok(read) => {
                        let keys = Arc::new(own_segments(cell, &read.state));
                        self.at.lock().unwrap().insert(at, keys.clone());
                        keys
                    }
                    // A pin spares copies alone: one unread, as a given-up fork's origin may be
                    // whose reference stands yet, pins nothing this round, and fails none.
                    Err(error) => {
                        log::warn!("cladefs: cell {cell}'s state at {at:x}, kept: {error}");
                        continue;
                    }
                },
            };
            pinned.extend(keys.iter().cloned());
        }
        Ok(pinned)
    }
}

/// What a round that ended as `ended` relocated: what it says it did, or, if it failed, what its
/// relocation committed before ([`PartlyRelocated`]); nothing else.
pub(crate) fn relocated(ended: &anyhow::Result<Relocated>) -> Relocated {
    match ended {
        Ok(relocated) => *relocated,
        Err(error) => error
            .downcast_ref::<PartlyRelocated>()
            .map_or_else(Relocated::default, |partly| partly.0),
    }
}

/// The keys of `cell`'s own segments `state` reads.
fn own_segments(cell: CellId, state: &bucket_cell::State) -> HashSet<String> {
    let prefix = bucket_cell::layout::segments(cell);
    let keys = state.extents.values().map(|at| &at.key);
    keys.filter(|key| key.starts_with(&prefix)).cloned().collect()
}

/// A round over `cell`, `writer`'s, in `bucket`: what it relocates of `allowance` bytes at most,
/// and the commit that names it there,
/// out of the segments sparsest first; none pinned ([`Pins`]). An extent read gone, freed or
/// relocated since it was listed, is passed over; any other failure ends the round, failing
/// nothing but it (a relocation is optional, doc-27). What it reads it holds a place for in
/// `uploads`, the budget of the host's uploads, which its uploads go before.
pub(crate) async fn round<D: Dir, B: Bucket>(
    cell: &LocalCell<D>,
    writer: &Writer<B>,
    (bucket, Budgets { uploads, fetching, cache }): (&Arc<B>, Budgets<'_>),
    pins: &Pins,
    allowance: u64,
) -> anyhow::Result<Relocated> {
    // No more than a relocation would take now, none while it would take nothing; and no more than
    // a quarter of the uploads' budget, so that a round's hold of it keeps no upload waiting long
    // (the verifying review of fa1912379).
    let allowance = allowance.min(uploads.limits().bytes / 4).min(cell.relocation_room()?);
    if allowance == 0 {
        return Ok(Relocated::default());
    }
    let mut ats: BTreeSet<u64> = writer.versions().await.iter().map(|version| version.at).collect();
    ats.extend(lifecycle::referenced_at(&**bucket, writer.cell()).await?);
    let pinned = pins.of(&**bucket, writer, &ats).await?;
    let sparse = cell.sparse(|key| pinned.contains(key))?;
    if sparse.is_empty() {
        return Ok(Relocated::default());
    }
    let keys: HashSet<String> = sparse.iter().map(|sparse| sparse.key.clone()).collect();
    let mut held = cell.held_in(&keys)?;
    // Sparsest first, as many whole segments as the allowance takes, and of the next as much.
    let mut chosen = Vec::new();
    let mut left = allowance;
    'segments: for sparse in &sparse {
        for (extent, at) in held.remove(&sparse.key).unwrap_or_default() {
            if at.len > left {
                break 'segments;
            }
            left -= at.len;
            chosen.push((extent, at));
        }
    }
    // What it reads it holds until relocated: in the budget of the host's uploads, behind them.
    let bytes = chosen.iter().map(|(_, at)| at.len).sum();
    let _held = uploads.hold(Priority::Ahead, bytes).await;
    let (budgets, memory, cache) = match fetching {
        Some(crate::upload::Fetching { budgets, memory }) => (budgets, memory, cache),
        None => {
            let parts = Limits { requests: AT_ONCE, bytes: AT_ONCE as u64 * extent_frame::PART };
            (Budget::new(parts).into(), Memory::new(), None)
        }
    };
    // Its cell's keys, held among its memory's while it reads.
    let _keys = memory.keys().hold(writer.keys().clone());
    let fetcher = Fetcher::new(bucket.clone(), budgets, memory, cache);
    let read = futures::stream::iter(chosen)
        .map(|(extent, from)| {
            let fetcher = &fetcher;
            async move {
                match fetcher.read(extent, &from, 0, from.len, Priority::Ahead).await {
                    Ok(data) => Ok(Some(Relocating { extent, from, data })),
                    // Freed, or relocated, since: the collector may have deleted its object.
                    Err(bucket_cell::Error::Lost(_)) => Ok(None),
                    Err(err) => Err(err),
                }
            }
        })
        .buffered(AT_ONCE)
        .try_filter_map(|read| async move { Ok(read) })
        .try_collect::<Vec<_>>()
        .await?;
    Ok(cell.relocate(read).await?)
}
