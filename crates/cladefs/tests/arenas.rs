// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-79.36: an arena of physical memory of its own (TASK-43), a tenant's, held by
//! what allocates in it: its tenant, the cells opened in it, and what reads into the extent cache
//! for them. It goes once none of them holds it, and nothing allocates in it after.

use cladefs::pages::Arena;
use cladefs::{Access, ExtentId, OpenCell, Opening, VolatileSlice};
use futures::executor::block_on;

/// A cell opened in an arena pages its extents in, through its window, once what made the arena
/// for it lets go of it, as a tenant whose VM processes are gone does; and the arena goes with the
/// cell: it gives no page then.
#[tokio::test(flavor = "multi_thread")]
async fn cells_hold_their_arenas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let ps = cladefs::pages::page_size();
    let open = OpenCell::create(&path, 1 << 24).await.unwrap();
    let extent = ExtentId { cell: open.cell().id, sequence: 1 };
    let bytes = vec![0xa5; ps as usize];
    open.write(extent, 0, &[VolatileSlice::from(&bytes[..])]).await.unwrap();
    open.commit(None).await.unwrap();
    drop(open);

    let held = Arena::new().unwrap();
    let arena = held.id();
    let open = Opening::here().unwrap().arena(Some(held.clone())).open(&path).await.unwrap();
    // What made it lets go of it, as a tenant's going does.
    drop(held);
    let window = open.create_dax_window(ps, None).unwrap();
    open.map(extent, 0, ps, 0).await.unwrap();
    block_on(window.fault(0, Access::Read)).expect("the extent paged in");
    assert!(cladefs::slabs::arena_bytes(arena) > 0, "the page is another arena's");
    let page = cladefs::slabs::alloc(arena, 1, cladefs::slabs::Wait::No);
    drop(page.expect("a page of the cell's arena"));

    drop((window, open));
    let page = cladefs::slabs::alloc(arena, 1, cladefs::slabs::Wait::No);
    assert_eq!(page.err(), Some(cladefs::slabs::NoMemory));
}
