// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: cells (TASK-364, doc-20): a tenant's cell, one log, its objects under its prefix
//! ([`CellId::prefix`]), its one swapped object its head ([`crate::head`]), which says who holds
//! it, by which placement's fencing token, and its kind ([`Kind`]): one, given by its first
//! writer, never another ([`Error::OtherKind`](crate::Error::OtherKind)). Here: kinds, and the
//! listing of a bucket's cells.

use crate::bucket;
use crate::bucket::Bucket;
use crate::layout::{self, CellId};
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};

/// What a cell holds, which says what opens it (doc-20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// An Fxfs filesystem.
    Fxfs,
    /// A database of `sqlite_cladefs`.
    Sqlite,
    /// A VM's memory: a `block_device` whose contents are its image (TASK-456).
    Memory,
}

impl Kind {
    /// Its name, as a cell's description says it: `fxfs`, `sqlite` or `memory`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fxfs => "fxfs",
            Self::Sqlite => "sqlite",
            Self::Memory => "memory",
        }
    }

    /// The kind `name` names ([`Kind::name`]).
    pub fn named(name: &str) -> Option<Self> {
        [Self::Fxfs, Self::Sqlite, Self::Memory].into_iter().find(|kind| kind.name() == name)
    }
}

/// cladefs: every cell of `bucket` with an object under its prefix, in the order of their keys:
/// found by delimiter listings of `tenants/` then of each tenant's `cells/`, listing none of their
/// objects (TASK-395.4, TASK-395.5): what a sweep of the collector over every cell visits. A page
/// at a time, of tenants and of a tenant's cells, each listed as the cells before it are taken: so
/// it holds no more than a page of each, a tenant of millions of cells as one of a few. A request
/// for every [`LIST_PAGE`](bucket::LIST_PAGE) tenants, and one for every
/// [`LIST_PAGE`](bucket::LIST_PAGE) cells of each.
pub fn listed<B: Bucket>(bucket: &B) -> impl Stream<Item = CellId> + Send + '_ {
    tenants(bucket).flat_map(move |tenant| of_tenant(bucket, tenant))
}

/// cladefs: every tenant of `bucket` with a cell, in order: by a delimiter listing of `tenants/`, a
/// page at a time, listing none of their cells' objects.
pub fn tenants<B: Bucket>(bucket: &B) -> impl Stream<Item = u64> + Send + '_ {
    let tenants = bucket::prefixes(bucket, layout::TENANTS.to_owned());
    tenants.filter_map(|prefix| std::future::ready(layout::tenant_of_prefix(&prefix)))
}

/// cladefs: every cell of `tenant` with an object under its prefix, in order: by a delimiter
/// listing of the tenant's `cells/`, a page at a time, listing none of their objects (`cladefs-gc
/// --usage`, TASK-395.8).
pub fn of_tenant<B: Bucket>(bucket: &B, tenant: u64) -> impl Stream<Item = CellId> + Send + '_ {
    let cells = bucket::prefixes(bucket, layout::cells_of(tenant));
    cells.filter_map(|prefix| std::future::ready(layout::cell_of_prefix(&prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::LIST_PAGE;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    /// TASK-395.5: a bucket's cells are listed a page at a time, of tenants and of each tenant's
    /// cells, each page asked for once the cells before it are taken: a tenant of more cells than
    /// a page, among others, all in order, each once, and no request made but for the pages of
    /// cells taken so far.
    #[test]
    fn cells_are_listed_a_page_at_a_time() {
        use crate::bucket::PutMode;
        let sim = SimBucket::new();
        let bucket = Direct(sim.clone());
        let many = LIST_PAGE as u64 * 2 + 5;
        let mut cells: Vec<CellId> = (0..many).map(|id| CellId { tenant: 2, id }).collect();
        cells.extend([CellId { tenant: 1, id: 7 }, CellId { tenant: 3, id: 1 }]);
        for &cell in &cells {
            let key = layout::head(cell, 0);
            block_on(bucket.put(&key, vec![1].into(), PutMode::Create)).unwrap();
        }
        cells.sort();
        // Each page as it is asked for: its requests, as the bucket served them, completed as
        // they come.
        let mut stream = std::pin::pin!(listed(&*sim));
        let mut taken = Vec::new();
        let (first, asked) = sim.served(stream.next());
        taken.extend(first);
        assert_eq!(
            asked,
            [
                "list-delimited tenants/".to_owned(),
                "list-delimited tenants/0000000000001/cells/".to_owned()
            ]
        );
        let (rest, asked) = sim.served(async {
            let mut rest = Vec::new();
            while let Some(cell) = stream.next().await {
                rest.push(cell);
            }
            rest
        });
        taken.extend(rest);
        assert_eq!(taken, cells);
        let tenant = "tenants/0000000000002/cells/";
        let after = |id: u64| {
            format!("list-delimited {tenant} after {tenant}{id}/", id = base32_id::Padded(id))
        };
        assert_eq!(
            asked,
            [
                format!("list-delimited {tenant}"),
                after(LIST_PAGE as u64 - 1),
                after(LIST_PAGE as u64 * 2 - 1),
                "list-delimited tenants/0000000000003/cells/".to_owned(),
            ]
        );
    }
}
