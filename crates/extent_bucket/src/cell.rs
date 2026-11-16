// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: cells (TASK-364). A tenant's cell is its unit of placement: a host at a time holds it,
//! and with it every one of its vdisks, each a branch of its own, kept under the cell's prefix
//! ([`Cell::prefix`]). The cell's object ([`layout::cell`]) says who holds it, at what epoch, by
//! which placement's fencing token, and what vdisks it has.
//!
//! Taking a cell is one compare-and-swap of its object, to the next epoch, refused to a placement
//! of a lower token than the one holding it ([`Error::Stale`]), as a branch's head is. The taker
//! then takes each vdisk's branch's head with the cell's epoch as its token: a later taking of
//! the cell, of a higher epoch, takes them from it, and its vdisks' writers learn of it as any
//! writer taken over does, at their next upload; it can take none back, its epoch lower. So a cell
//! moves whole, fenced by the branches' own heads, which nothing about cells changes.
//!
//! The vdisks a cell has change only by its holder, each change conditional on the object being
//! as its holder last took or changed it: another's taking since fails it ([`Error::Fenced`]).
//!
//! cladefs: a cell's object keeps its KEK, every generation of it, each wrapped by its tenant's KEK
//! in the KMS ([`Object::keks`], doc-16, TASK-411.3), given it by its maker ([`hold`], [`create`]);
//! none, it is in the clear, and so are its branches ([`crate::keys`]).
//!
//! cladefs: a cell is shredded by taking every wrapped KEK out of its object, which is marked so
//! ([`shred`], doc-16, Shredding; TASK-411.7): its branches' DEKs unwrap no more, and no one takes
//! the cell again, nor writes a branch of it in the clear. Only as strong as the bucket keeps no
//! older version of the cell's object, whose wrapped KEKs unwrap while its tenant's KEK lives.

use crate::branch::{Start, Step, transition, transition_leaving};
use crate::bucket::Bucket;
use crate::keys::Kek;
use crate::layout::{self, Branch, Cell, FORMAT};
use crate::{Error, bucket, rules};
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a vdisk of a cell is: what its branch holds, which says what opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// An Fxfs filesystem.
    Fxfs,
    /// A database of `sqlite_extents`.
    Sqlite,
    /// A VM's memory: an `extent_files` file set of one file, its image (TASK-456).
    Memory,
}

impl Kind {
    /// Its name, as a store's description says it: `fxfs`, `sqlite` or `memory`.
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

/// The longest name a vdisk has.
pub const MAX_NAME: usize = 64;

/// Whether `name` may name a vdisk: 1 to [`MAX_NAME`] bytes of ASCII letters, digits, `.`, `_`
/// and `-`, not starting with `.`: a name a host's directories and mounts show as it is.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && !name.starts_with('.')
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// A vdisk of a cell: its branch, of the cell's, and what it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vdisk {
    pub branch: u64,
    pub kind: Kind,
}

/// A cell's object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    /// The format of the cell's objects, its branches' among them ([`FORMAT`]), first, so that a
    /// reader of any format reads it before the rest.
    pub format: u32,
    /// The epoch of its holder's taking, the token its vdisks' heads are taken with.
    pub epoch: u64,
    /// Who took it: unique to each taking, so that a write of it is recognised.
    pub holder: u64,
    /// The fencing token of the placement that took it, which no lower one takes it from.
    pub token: u64,
    /// Its vdisks, by their names.
    pub vdisks: BTreeMap<String, Vdisk>,
    /// Whether it was deleted: then no one takes it again.
    pub deleted: bool,
    /// cladefs: its KEK, each generation of it, wrapped by its tenant's KEK in the KMS, oldest first:
    /// none, in the clear (TASK-411.3).
    pub keks: Vec<Kek>,
    /// cladefs: whether it was shredded ([`shred`], TASK-411.7): its KEKs gone, so that its
    /// branches' data is unreadable, and no one takes it again.
    pub shredded: bool,
}

impl Object {
    /// cladefs: a cell's object as its maker makes it, held by `holder` of the placement of `token`
    /// at epoch 1, of no vdisks, its KEK `kek`, if any.
    fn made(holder: u64, token: u64, kek: Option<Kek>) -> Self {
        Self {
            format: FORMAT,
            epoch: 1,
            holder,
            token,
            vdisks: BTreeMap::new(),
            deleted: false,
            keks: kek.into_iter().collect(),
            shredded: false,
        }
    }

    fn decode(cell: Cell, bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = layout::decode(bytes.get(..4).unwrap_or(bytes))?;
        if format != FORMAT {
            let branch = Branch { cell, id: 0 };
            return Err(Error::Format { branch, format });
        }
        layout::decode(bytes)
    }

    /// The branch of its vdisk `name`, if it has one.
    pub fn branch(&self, cell: Cell, name: &str) -> Option<Branch> {
        self.vdisks.get(name).map(|vdisk| Branch { cell, id: vdisk.branch })
    }
}

/// A taking of a cell: what changing it, and taking its vdisks' heads, goes by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub cell: Cell,
    /// The epoch of the taking: the token its vdisks' heads are taken with.
    pub epoch: u64,
    pub holder: u64,
}

/// Changes `cell`'s object as `step` says ([`transition`]).
async fn change<R>(
    bucket: &impl Bucket,
    cell: Cell,
    start: Start,
    step: impl FnMut(Option<Object>) -> Result<Step<Object, R>, Error>,
) -> Result<R, Error> {
    let decode = |bytes: &[u8]| Object::decode(cell, bytes);
    transition(bucket, &layout::cell(cell), start, decode, step).await
}

/// Creates `cell`, of no vdisks, held by `holder` of the placement of `token` at epoch 1: a
/// create of another's finds its object there. cladefs: its KEK `kek`, if given (TASK-411.3).
pub async fn create(
    bucket: &impl Bucket,
    cell: Cell,
    (holder, token): (u64, u64),
    kek: Option<Kek>,
) -> Result<Held, Error> {
    let object = Object::made(holder, token, kek);
    change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(object.clone(), ())),
        Some(found) if found.holder == holder && found.epoch == 1 => Ok(Step::Stop(())),
        Some(_) => Err(Error::Corrupt(format!("cell {:x}/{:x} exists", cell.tenant, cell.id))),
    })
    .await?;
    Ok(Held { cell, epoch: 1, holder })
}

/// cladefs: makes `cell`, of no vdisks, its KEK `kek`, as `holder` makes it, held by no placement
/// (token 0), so that any placement takes it (TASK-411.4): what a fork into a new cell makes as it
/// gives its child its keys, before the child's store opens, wherever it does. Returns the cell's
/// object, and whether this made it: one there already, another's, is left as it is.
pub async fn make(
    bucket: &impl Bucket,
    cell: Cell,
    holder: u64,
    kek: &Kek,
) -> Result<(Object, bool), Error> {
    let object = Object::made(holder, 0, Some(kek.clone()));
    change(bucket, cell, Start::Create, |found| match found {
        None => Ok(Step::Write(object.clone(), (object.clone(), true))),
        // Ours, landed though its answer was lost, its bytes this making's alone.
        Some(found) => {
            let ours = found == object;
            Ok(Step::Stop((found, ours)))
        }
    })
    .await
}

/// Takes `cell` for `holder`, of the placement of `token`, at the epoch after its last taking's:
/// refused if it has no object ([`Error::NoCell`]), if it was deleted ([`Error::Deleted`]), or if
/// a placement of a higher token holds it ([`Error::Stale`]). cladefs: and if it was shredded
/// ([`shredded`]).
pub async fn take(
    bucket: &impl Bucket,
    cell: Cell,
    holder: u64,
    token: u64,
) -> Result<Held, Error> {
    let epoch = change(bucket, cell, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if found.shredded => Err(shredded(cell)),
        Some(found) if rules().tokens && found.token > token => Err(Error::Stale),
        Some(found) if found.holder == holder => Ok(Step::Stop(found.epoch)),
        Some(found) => {
            let epoch = found.epoch + 1;
            Ok(Step::Write(Object { epoch, holder, token, ..found }, epoch))
        }
    })
    .await?;
    Ok(Held { cell, epoch, holder })
}

/// `cell`'s object, if it has one.
pub async fn read(bucket: &impl Bucket, cell: Cell) -> Result<Option<Object>, Error> {
    match bucket::get(bucket, &layout::cell(cell)).await {
        Some(object) => Ok(Some(Object::decode(cell, &object.bytes)?)),
        None => Ok(None),
    }
}

/// cladefs: whether `held` holds its cell still, read: [`Error::Stale`] if another took it since,
/// [`Error::Deleted`] if it was deleted, refused if it was shredded. What a writer's taking of a
/// branch reads once its head is taken, of a holding its host remembers rather than read before,
/// whose epoch the head was taken with (`Taker::remembered`, TASK-449's review): found so, it
/// held the cell as the head was taken, as epochs only grow, holding by holding.
pub async fn still_held(bucket: &impl Bucket, held: Held) -> Result<(), Error> {
    match read(bucket, held.cell).await? {
        None => Err(Error::NoCell),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if found.shredded => Err(shredded(held.cell)),
        Some(found) if (found.epoch, found.holder) != (held.epoch, held.holder) => {
            Err(Error::Stale)
        }
        Some(_) => Ok(()),
    }
}

/// Changes the object of the cell `held` holds as `step` says, if `held` holds it still:
/// [`Error::Fenced`] if another took it since, [`Error::Deleted`] if it was deleted.
async fn change_held<R>(
    bucket: &impl Bucket,
    held: Held,
    mut step: impl FnMut(Object) -> Result<Step<Object, R>, Error>,
) -> Result<R, Error> {
    change(bucket, held.cell, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.deleted => Err(Error::Deleted),
        Some(found) if (found.epoch, found.holder) != (held.epoch, held.holder) => {
            Err(Error::Fenced)
        }
        Some(found) => step(found),
    })
    .await
}

/// Holds `cell` for `holder`, of the placement of `token`, in one compare-and-swap of its object:
/// takes it as [`take`] does, a shredded one refused; or, if it has no object and `make` says so, makes it, held at epoch 1,
/// cladefs: its KEK `kek`, if given (TASK-411.3);
/// and, given `vdisk`, has it hold that vdisk, under that name, unless it names the vdisk's
/// branch already, under whatever name it gave it: refused, as [`add_vdisk`] refuses, if another
/// branch has the name, or the name names no vdisk. A vdisk's name is its branch's from its
/// first: a store of the branch taken into a directory of another name names it no other way.
/// Returns the taking, and the object as it left it.
pub async fn hold(
    bucket: &impl Bucket,
    cell: Cell,
    holding: (u64, u64),
    making: (bool, Option<&Kek>),
    vdisk: Option<(&str, Vdisk)>,
) -> Result<(Held, Object), Error> {
    let (held, object, _) = hold_from(bucket, cell, Start::Read, holding, making, vdisk).await?;
    Ok((held, object))
}

/// cladefs: [`hold`], starting from `start`: what a read found, or what a holding before left, or
/// nothing, for a cell just drawn, whose object its maker creates; its write conditional on what it
/// starts from, as on what a read found, so that a write of another's since is read and gone on
/// from (TASK-449). Returns too the cell's object as it left it, raw, what a later holding starts
/// from: a fork's host holds its child's cell so, and the child's store, opening, from that.
pub async fn hold_from(
    bucket: &impl Bucket,
    cell: Cell,
    start: Start,
    (holder, token): (u64, u64),
    (make, kek): (bool, Option<&Kek>),
    vdisk: Option<(&str, Vdisk)>,
) -> Result<(Held, Object, Option<crate::bucket::Object>), Error> {
    if let Some((name, _)) = vdisk
        && !valid_name(name)
    {
        return Err(Error::VdiskName(name.into()));
    }
    let decode = |bytes: &[u8]| Object::decode(cell, bytes);
    let ((held, object), left) =
        transition_leaving(bucket, &layout::cell(cell), start, decode, |found| {
            let (mut object, mut changed) = match found {
                None if !make => return Err(Error::NoCell),
                None => (Object::made(holder, token, kek.cloned()), true),
                Some(found) if found.deleted => return Err(Error::Deleted),
                Some(found) if found.shredded => return Err(shredded(cell)),
                Some(found) if rules().tokens && found.token > token => return Err(Error::Stale),
                Some(found) if found.holder == holder => (found, false),
                Some(found) => (Object { epoch: found.epoch + 1, holder, token, ..found }, true),
            };
            if let Some((name, vdisk)) = vdisk
                && !object.vdisks.values().any(|had| had.branch == vdisk.branch)
            {
                if object.vdisks.contains_key(name) {
                    return Err(Error::VdiskExists(name.into()));
                }
                object.vdisks.insert(name.into(), vdisk);
                changed = true;
            }
            let held = Held { cell, epoch: object.epoch, holder };
            Ok(match changed {
                true => Step::Write(object.clone(), (held, object)),
                false => Step::Stop((held, object)),
            })
        })
        .await?;
    Ok((held, object, left))
}

/// Adds to the cell `held` holds the vdisk `name`, `vdisk`: [`Error::VdiskExists`] if it has
/// one of that name, of another branch; [`Error::VdiskName`] if `name` names no vdisk
/// ([`valid_name`]).
pub async fn add_vdisk(
    bucket: &impl Bucket,
    held: Held,
    name: &str,
    vdisk: Vdisk,
) -> Result<(), Error> {
    if !valid_name(name) {
        return Err(Error::VdiskName(name.into()));
    }
    change_held(bucket, held, |mut found| match found.vdisks.get(name) {
        Some(&had) if had == vdisk => Ok(Step::Stop(())),
        Some(_) => Err(Error::VdiskExists(name.into())),
        None => {
            found.vdisks.insert(name.into(), vdisk);
            Ok(Step::Write(found, ()))
        }
    })
    .await
}

/// Removes from the cell `held` holds its vdisk `name`, returning it: [`Error::NoVdisk`] if it
/// has none of that name. Its branch stays, to be deleted.
pub async fn remove_vdisk(bucket: &impl Bucket, held: Held, name: &str) -> Result<Vdisk, Error> {
    change_held(bucket, held, |mut found| match found.vdisks.remove(name) {
        Some(vdisk) => Ok(Step::Write(found, vdisk)),
        None => Err(Error::NoVdisk(name.into())),
    })
    .await
}

/// cladefs: adds `kek`, a new generation of its KEK, to the cell `held` holds, if `held` holds it
/// still: the cell's KEK rotated ([`crate::keys::rotate`], TASK-411.3). Returns its object as left.
pub async fn add_kek(bucket: &impl Bucket, held: Held, kek: Kek) -> Result<Object, Error> {
    change_held(bucket, held, |mut found| {
        if found.shredded {
            return Err(shredded(held.cell));
        }
        if found.keks.iter().any(|had| had.generation >= kek.generation) {
            return Err(Error::Corrupt(format!(
                "cell {} has a KEK of generation {} or later",
                held.cell, kek.generation
            )));
        }
        found.keks.push(kek.clone());
        Ok(Step::Write(found.clone(), found))
    })
    .await
}

/// cladefs: shreds `cell` (doc-16, Shredding; TASK-411.7): takes every generation of its KEK out of
/// its object and marks it shredded, by one conditional update, whoever holds it: its branches'
/// DEKs, wrapped by those KEKs, unwrap no more, and no one takes it again ([`take`], [`hold`]).
/// Its holder, if any, goes on changing its vdisks until told (the mark stays, as every change
/// keeps the rest of the object); a host that unwrapped its KEK before keeps it until told too
/// (`crate::keys::CachingKms::forget`). Shredded already, nothing changes. Returns how many
/// generations of its KEK it took out: [`Error::NoCell`] if it has no object.
///
/// Only as strong as the bucket keeps no older version of the cell's object: with versioning on,
/// a lifecycle rule must expire non-current versions, or theirs unwrap while the tenant's KEK
/// lives.
pub async fn shred(bucket: &impl Bucket, cell: Cell) -> Result<usize, Error> {
    change(bucket, cell, Start::Read, |found| match found {
        None => Err(Error::NoCell),
        Some(found) if found.shredded => Ok(Step::Stop(0)),
        Some(found) => {
            let removed = found.keks.len();
            Ok(Step::Write(Object { keks: Vec::new(), shredded: true, ..found }, removed))
        }
    })
    .await
}

/// cladefs: the refusal of a shredded cell, `cell`: as a KMS's refusal, naming its tenant, its
/// stores opening no more (`EACCES`), and the collector leaving its encrypted objects uncompacted.
pub fn shredded(cell: Cell) -> Error {
    Error::Kms(format!("tenant {}: cell {cell} is shredded, its KEK gone", cell.tenant))
}

/// Takes out of `cell` the vdisk naming `branch`, deleted, if it names it: what any placement
/// deleting the branch does, whoever holds the cell, as a deleted branch's name holds nothing, and
/// changing the cell's vdisks so changes nothing of who holds it, nor its epoch.
pub async fn forget(bucket: &impl Bucket, cell: Cell, branch: u64) -> Result<(), Error> {
    change(bucket, cell, Start::Read, |found| match found {
        Some(mut found) if found.vdisks.values().any(|vdisk| vdisk.branch == branch) => {
            found.vdisks.retain(|_, vdisk| vdisk.branch != branch);
            Ok(Step::Write(found, ()))
        }
        _ => Ok(Step::Stop(())),
    })
    .await
}

/// Marks the cell `held` holds deleted, so that no one takes it again, returning its vdisks, whose
/// branches stay, to be deleted.
pub async fn delete(bucket: &impl Bucket, held: Held) -> Result<BTreeMap<String, Vdisk>, Error> {
    change_held(bucket, held, |found| {
        let vdisks = found.vdisks.clone();
        Ok(Step::Write(Object { deleted: true, ..found }, vdisks))
    })
    .await
}

/// Whether `held` holds its cell still.
pub async fn holds(bucket: &impl Bucket, held: Held) -> Result<bool, Error> {
    Ok(read(bucket, held.cell).await?.is_some_and(|found| {
        !found.deleted && (found.epoch, found.holder) == (held.epoch, held.holder)
    }))
}

/// cladefs: every cell of `bucket` with an object under its prefix, in the order of their keys: found
/// by delimiter listings of `tenants/` then of each tenant's `cells/`, listing none of their objects
/// (TASK-395.4, TASK-395.5): what a sweep of the collector over every cell visits. A page at a
/// time, of tenants and of a tenant's cells, each listed as the cells before it are taken: so it
/// holds no more than a page of each, a tenant of millions of cells as one of a few. A request for
/// every [`LIST_PAGE`](bucket::LIST_PAGE) tenants, and one for every
/// [`LIST_PAGE`](bucket::LIST_PAGE) cells of each.
pub fn listed<B: Bucket>(bucket: &B) -> impl Stream<Item = Cell> + Send + '_ {
    tenants(bucket).flat_map(move |tenant| of_tenant(bucket, tenant))
}

/// cladefs: every tenant of `bucket` with a cell, in order: by a delimiter listing of `tenants/`, a
/// page at a time, listing none of their cells' objects.
pub fn tenants<B: Bucket>(bucket: &B) -> impl Stream<Item = u64> + Send + '_ {
    let tenants = bucket::prefixes(bucket, layout::TENANTS.to_owned());
    tenants.filter_map(|prefix| std::future::ready(layout::tenant_of_prefix(&prefix)))
}

/// cladefs: every cell of `tenant` with an object under its prefix, in order: by a delimiter listing
/// of the tenant's `cells/`, a page at a time, listing none of their objects (`cladefs-gc --usage`,
/// TASK-395.8).
pub fn of_tenant<B: Bucket>(bucket: &B, tenant: u64) -> impl Stream<Item = Cell> + Send + '_ {
    let cells = bucket::prefixes(bucket, layout::cells_of(tenant));
    cells.filter_map(|prefix| std::future::ready(layout::cell_of_prefix(&prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::LIST_PAGE;
    use crate::layout::TEST_CELL;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;

    const FXFS: Vdisk = Vdisk { branch: 7, kind: Kind::Fxfs };
    const SQLITE: Vdisk = Vdisk { branch: 8, kind: Kind::Sqlite };

    /// A cell made is held by its maker at epoch 1, of no vdisks; made again by its maker, whose
    /// answer was lost, it stands; by another, it exists. One that was never made has no object,
    /// and no one takes it.
    #[test]
    fn cells_are_made_once() {
        let bucket = Direct(SimBucket::new());
        let held = block_on(create(&bucket, TEST_CELL, (0x11, 5), None)).unwrap();
        assert_eq!(held, Held { cell: TEST_CELL, epoch: 1, holder: 0x11 });
        assert_eq!(block_on(create(&bucket, TEST_CELL, (0x11, 5), None)), Ok(held));
        let exists = Error::Corrupt("cell 1/1 exists".into());
        assert_eq!(block_on(create(&bucket, TEST_CELL, (0x12, 5), None)), Err(exists));
        let object = block_on(read(&bucket, TEST_CELL)).unwrap().unwrap();
        let expected = Object {
            format: FORMAT,
            epoch: 1,
            holder: 0x11,
            token: 5,
            vdisks: BTreeMap::new(),
            deleted: false,
            keks: Vec::new(),
            shredded: false,
        };
        assert_eq!(object, expected);
        let never = Cell { tenant: 1, id: 2 };
        assert_eq!(block_on(read(&bucket, never)), Ok(None));
        assert_eq!(block_on(take(&bucket, never, 0x13, 5)), Err(Error::NoCell));
    }

    /// TASK-395.4: every cell with an object under its prefix is listed, in the order of its key,
    /// whatever its objects: a cell's object, a record alone, or what the collector keeps there;
    /// keys of no cell's are not.
    #[test]
    fn cells_are_listed_by_their_prefixes() {
        use crate::bucket::PutMode;
        let bucket = Direct(SimBucket::new());
        let cells =
            [Cell { tenant: 1, id: 2 }, Cell { tenant: 1, id: 0x10 }, Cell { tenant: 3, id: 1 }];
        let branch = |cell, id| Branch { cell, id };
        let keys = [
            layout::cell(cells[2]),
            layout::record(branch(cells[0], 4)),
            layout::segment(branch(cells[0], 4), 0, 1),
            layout::collector_lease(cells[1]),
            layout::objectid_block(1),
        ];
        for key in keys {
            block_on(bucket.put(&key, vec![1].into(), PutMode::Create)).unwrap();
        }
        assert_eq!(block_on(listed(&bucket).collect::<Vec<_>>()), cells);
        assert_eq!(block_on(listed(&Direct(SimBucket::new())).collect::<Vec<_>>()), []);
    }

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
        let mut cells: Vec<Cell> = (0..many).map(|id| Cell { tenant: 2, id }).collect();
        cells.extend([Cell { tenant: 1, id: 7 }, Cell { tenant: 3, id: 1 }]);
        for &cell in &cells {
            let key = layout::record(Branch { cell, id: 1 });
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
                "list-delimited tenants/0000000000000001/cells/".to_owned()
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
        let tenant = "tenants/0000000000000002/cells/";
        let after = |id: u64| format!("list-delimited {tenant} after {tenant}{id:016x}/");
        assert_eq!(
            asked,
            [
                format!("list-delimited {tenant}"),
                after(LIST_PAGE as u64 - 1),
                after(LIST_PAGE as u64 * 2 - 1),
                "list-delimited tenants/0000000000000003/cells/".to_owned(),
            ]
        );
    }

    /// A cell taken goes to the next epoch, its taker's; a placement of a lower token than the one
    /// holding it takes it not. Its holder before holds it no more, and changes none of its vdisks.
    #[test]
    fn takings_fence_the_holder_before() {
        let bucket = Direct(SimBucket::new());
        let first = block_on(create(&bucket, TEST_CELL, (0x11, 5), None)).unwrap();
        block_on(add_vdisk(&bucket, first, "root", FXFS)).unwrap();
        let second = block_on(take(&bucket, TEST_CELL, 0x12, 5)).unwrap();
        assert_eq!(second, Held { cell: TEST_CELL, epoch: 2, holder: 0x12 });
        assert_eq!(block_on(take(&bucket, TEST_CELL, 0x13, 4)), Err(Error::Stale));
        assert_eq!(block_on(holds(&bucket, first)), Ok(false));
        assert_eq!(block_on(holds(&bucket, second)), Ok(true));
        assert_eq!(block_on(add_vdisk(&bucket, first, "db", SQLITE)), Err(Error::Fenced));
        assert_eq!(block_on(remove_vdisk(&bucket, first, "root")), Err(Error::Fenced));
        assert_eq!(block_on(delete(&bucket, first)), Err(Error::Fenced));
        // Taken again by its taker, whose answer was lost, it stays at its epoch.
        assert_eq!(block_on(take(&bucket, TEST_CELL, 0x12, 5)), Ok(second));
        let third = block_on(take(&bucket, TEST_CELL, 0x14, 6)).unwrap();
        assert_eq!(third.epoch, 3);
        assert_eq!(block_on(take(&bucket, TEST_CELL, 0x15, 5)), Err(Error::Stale));
        let object = block_on(read(&bucket, TEST_CELL)).unwrap().unwrap();
        assert_eq!((object.epoch, object.holder, object.token), (3, 0x14, 6));
        assert_eq!(object.vdisks, BTreeMap::from([("root".to_owned(), FXFS)]));
    }

    /// Its holder adds vdisks by name, each once: a name taken by another branch is refused; and
    /// removes them, a name it has none of refused. A cell deleted keeps its vdisks' names, which
    /// its deletion returns, and no one takes it or changes it again.
    #[test]
    fn holders_add_and_remove_vdisks() {
        let bucket = Direct(SimBucket::new());
        let held = block_on(create(&bucket, TEST_CELL, (0x11, 5), None)).unwrap();
        block_on(add_vdisk(&bucket, held, "root", FXFS)).unwrap();
        block_on(add_vdisk(&bucket, held, "db", SQLITE)).unwrap();
        assert_eq!(block_on(add_vdisk(&bucket, held, "db", SQLITE)), Ok(()));
        let taken = block_on(add_vdisk(&bucket, held, "db", FXFS));
        assert_eq!(taken, Err(Error::VdiskExists("db".into())));
        let object = block_on(read(&bucket, TEST_CELL)).unwrap().unwrap();
        assert_eq!(object.branch(TEST_CELL, "db"), Some(Branch { cell: TEST_CELL, id: 8 }));
        assert_eq!(object.branch(TEST_CELL, "logs"), None);
        assert_eq!(block_on(remove_vdisk(&bucket, held, "db")), Ok(SQLITE));
        let none = block_on(remove_vdisk(&bucket, held, "db"));
        assert_eq!(none, Err(Error::NoVdisk("db".into())));
        let vdisks = block_on(delete(&bucket, held)).unwrap();
        assert_eq!(vdisks, BTreeMap::from([("root".to_owned(), FXFS)]));
        assert_eq!(block_on(holds(&bucket, held)), Ok(false));
        assert_eq!(block_on(take(&bucket, TEST_CELL, 0x12, 9)), Err(Error::Deleted));
        let added = block_on(add_vdisk(&bucket, held, "db", SQLITE));
        assert_eq!(added, Err(Error::Deleted));
    }

    /// Holding a cell takes it, or makes it if it has no object and its holder may: a holder
    /// holding it already goes on at its epoch, another placement takes it at the next; one that
    /// may not make it finds none.
    #[test]
    fn holding_takes_or_makes() {
        let bucket = Direct(SimBucket::new());
        assert_eq!(
            block_on(hold(&bucket, TEST_CELL, (5, 5), (false, None), None)).map(|(held, _)| held),
            Err(Error::NoCell)
        );
        let (made, object) =
            block_on(hold(&bucket, TEST_CELL, (5, 5), (true, None), None)).unwrap();
        assert_eq!((made.epoch, object.epoch, object.holder), (1, 1, 5));
        let (again, _) = block_on(hold(&bucket, TEST_CELL, (5, 5), (true, None), None)).unwrap();
        assert_eq!(again, made);
        block_on(add_vdisk(&bucket, made, "root", FXFS)).unwrap();
        let (taken, object) =
            block_on(hold(&bucket, TEST_CELL, (6, 6), (true, None), None)).unwrap();
        assert_eq!((taken.epoch, object.holder), (2, 6));
        assert_eq!(object.vdisks, BTreeMap::from([("root".to_owned(), FXFS)]));
    }

    /// Holding a cell with a vdisk makes it hold the vdisk, in the one write of its taking, or
    /// none; a vdisk whose branch the cell names already, under whatever name, is left as named:
    /// its name is its branch's. A name another branch has is refused, writing nothing.
    #[test]
    fn holding_with_a_vdisk_names_its_branch_once() {
        let bucket = Direct(SimBucket::new());
        let made = block_on(hold(&bucket, TEST_CELL, (5, 5), (true, None), Some(("root", FXFS))));
        let (made, object) = made.unwrap();
        assert_eq!((made.epoch, object.vdisks.len()), (1, 1));
        let again =
            block_on(hold(&bucket, TEST_CELL, (5, 5), (true, None), Some(("elsewhere", FXFS))));
        let (_, object) = again.unwrap();
        assert_eq!(object.vdisks, BTreeMap::from([("root".to_owned(), FXFS)]));
        let taken = block_on(hold(&bucket, TEST_CELL, (6, 6), (true, None), Some(("db", SQLITE))));
        let (taken, object) = taken.unwrap();
        let both = BTreeMap::from([("db".to_owned(), SQLITE), ("root".to_owned(), FXFS)]);
        assert_eq!((taken.epoch, &object.vdisks), (2, &both));
        let other = Vdisk { branch: 9, kind: Kind::Fxfs };
        let refused = block_on(hold(&bucket, TEST_CELL, (7, 7), (true, None), Some(("db", other))));
        assert_eq!(refused.map(|(held, _)| held), Err(Error::VdiskExists("db".into())));
        let object = block_on(read(&bucket, TEST_CELL)).unwrap().unwrap();
        assert_eq!((object.epoch, object.holder, object.vdisks), (2, 6, both));
        let bad = block_on(hold(&bucket, TEST_CELL, (7, 7), (true, None), Some(("a/b", other))));
        assert_eq!(bad.map(|(held, _)| held), Err(Error::VdiskName("a/b".into())));
    }

    /// Vdisks are named as a host's directories show them: letters, digits, `.`, `_` and `-`, up
    /// to 64 of them, not starting with `.`; any other name is refused. Kinds go by their names.
    #[test]
    fn vdisks_are_named_plainly() {
        for name in ["root", "db.sqlite", "a-b_c", "0", &"x".repeat(MAX_NAME)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", ".hidden", "a/b", "a b", "é", &"x".repeat(MAX_NAME + 1)] {
            assert!(!valid_name(name), "{name}");
        }
        let bucket = Direct(SimBucket::new());
        let held = block_on(create(&bucket, TEST_CELL, (0x11, 5), None)).unwrap();
        let refused = block_on(add_vdisk(&bucket, held, "a/b", FXFS));
        assert_eq!(refused, Err(Error::VdiskName("a/b".into())));
        for kind in [Kind::Fxfs, Kind::Sqlite, Kind::Memory] {
            assert_eq!(Kind::named(kind.name()), Some(kind));
        }
        assert_eq!(Kind::named("ext4"), None);
    }

    /// A cell's object of another format is refused, naming it, before the rest of it is read: a
    /// newer one's, and format 8's, the format before the cells' collection under their prefixes
    /// (TASK-395.2).
    #[test]
    fn cells_of_other_formats_are_refused() {
        for format in [FORMAT + 1, 8, 9, 10] {
            let bucket = Direct(SimBucket::new());
            let object = Object {
                format,
                epoch: 1,
                holder: 0x11,
                token: 5,
                vdisks: BTreeMap::new(),
                deleted: false,
                keks: Vec::new(),
                shredded: false,
            };
            block_on(bucket::create(&bucket, &layout::cell(TEST_CELL), layout::encode(&object)));
            let refused = Error::Format { branch: Branch { cell: TEST_CELL, id: 0 }, format };
            assert_eq!(block_on(read(&bucket, TEST_CELL)), Err(refused.clone()));
            assert_eq!(block_on(take(&bucket, TEST_CELL, 0x12, 5)), Err(refused));
        }
    }
}
