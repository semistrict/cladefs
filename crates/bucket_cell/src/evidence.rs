// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: read evidence (TASK-702.2): what forks of a version read of it as they opened, from
//! which a host predicts what the next fork will read, and fetches it first (TASK-702.3).
//!
//! A version's data never changes, but what its forks read of it does: workloads drift, and one
//! version serves different commands. So the prediction is made from recent evidence, never fixed:
//! a fork's host records what its guest read of the version in its first seconds, a **recording**,
//! and writes it as an object of its own beside the version ([`layout::evidence`]), created once at
//! a key it drew, which needs no writer of the version's cell and no merge in place, as segments
//! need none (TASK-407). The prediction is the units any of the tenant's newest [`KEPT`] recordings
//! read, each first where it was read first in any of them ([`Prediction`]: TASK-702.1 chose it,
//! docs/measurements/ubu1-python-sandbox-2026-11-20T2043Z-dc6e5dff); the recordings older than
//! those, which the collector deletes ([`doomed`]), are the decay. So are those older than
//! [`KEPT_FOR`]: of a tenant that forks the version no more, which would otherwise stay as long as
//! the version, a public base's for good; and of a version dropped since, which a fork made before
//! it went may still write, as its cell's passes skip it.
//!
//! Each tenant's forks write and read their own tenant's evidence alone, a public base's too
//! ([`layout::evidence_of`]): what one tenant's guests read tells no other tenant anything. A
//! recording is encrypted under the version's cell's newest DEK the fork's keys hold
//! ([`crate::keys::Ring::encrypting_as`]), which every fork of the version inherits, and names its
//! own key in its footer, so that one copied to another key is refused. Its format is its own
//! ([`FORMAT`]), refused if it is another.
//!
//! Evidence is advice: losing it costs a fork's opening speed alone, and a reader skips what it
//! cannot read. So a recording is written once, with no retries, and the collector deletes
//! evidence without fencing anyone ([`doomed`]).

use crate::Error;
use crate::bucket::{self, Bucket, Meta, PutMode};
use crate::layout::{self, CellId, EncryptionKey, Origin};
use fprint::TypeFingerprint;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use storage_device::ExtentId;

/// How many of a tenant's newest recordings of a version predict its next fork's reads, and are
/// kept (TASK-702.1's window).
pub const KEPT: usize = 8;

/// How long a recording is kept and read, by the bucket's time, in milliseconds: a week.
pub const KEPT_FOR: u64 = 7 * 24 * 60 * 60 * 1000;

/// The most units a recording holds: 64 GiB of extents' MiBs, far past any boot's.
pub const MOST_UNITS: usize = 1 << 16;

/// The bytes of extents a unit covers: an Fxfs extent is a MiB at most.
pub const UNIT: u64 = 1 << 20;

/// A unit of a version a fork read: a MiB of one of its extents, and when the fork first read it,
/// in milliseconds after it opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TypeFingerprint)]
pub struct Unit {
    pub extent: ExtentId,
    /// Which MiB of the extent: its offset over [`UNIT`].
    pub mib: u64,
    pub first: u32,
}

/// The version a fork's evidence is of, and the tenant whose forks' it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Of {
    pub origin: Origin,
    pub tenant: u64,
}

impl Of {
    /// The prefix its recordings are under.
    pub fn prefix(&self) -> String {
        layout::evidence_of(self.origin.parent, self.origin.at, self.tenant)
    }

    /// The version's cell, under whose keys its recordings are encrypted.
    pub fn cell(&self) -> CellId {
        self.origin.parent
    }
}

/// A recording's footer: its bucket layout version first ([`layout::FORMAT`], doc-28, board
/// 10038), then the key it was written at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TypeFingerprint)]
pub(crate) struct Footer {
    pub(crate) format: u32,
    pub(crate) key: String,
}

impl Footer {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let format: u32 = layout::decode(bytes.get(..4).unwrap_or(bytes))?;
        let format = layout::FORMAT.check(format).map_err(|_| Error::EvidenceFormat(format))?;
        match format {
            31.. => layout::decode(bytes),
            _ => unreachable!("FORMAT.check enforces oldest..=current"),
        }
    }
}

/// The object of the recording of `units` at `key`, encrypted as `encrypting` says.
pub fn encode(key: &str, units: &[Unit], encrypting: EncryptionKey<'_>) -> Vec<u8> {
    let footer = Footer { format: layout::FORMAT.current, key: key.to_owned() };
    layout::with_footer(&layout::encode(&units), &footer, encrypting)
}

/// The units of the recording at `key` whose object is `bytes`, by a reader holding `keys`: refused
/// if of an unsupported format ([`Error::EvidenceFormat`]), written at another key, or under a key
/// it lacks.
pub fn decode(key: &str, bytes: &[u8], keys: &dyn extent_frame::Keys) -> Result<Vec<Unit>, Error> {
    let footer = Footer::decode(extent_frame::footer(bytes, keys)?)?;
    if footer.key != key {
        return Err(Error::Corrupt(format!("{key}: read evidence written at {}", footer.key)));
    }
    layout::decode(&layout::data_under(bytes, keys)?)
}

/// Writes a recording of `units`, of `of`, encrypted as `encrypting` says, at a new key: once, by a
/// conditional create, retrying nothing. Returns its key.
pub async fn write(
    bucket: &impl Bucket,
    of: Of,
    units: &[Unit],
    encrypting: EncryptionKey<'_>,
) -> Result<String, bucket::Error> {
    let key = layout::evidence(of.origin.parent, of.origin.at, of.tenant, rand::random());
    let bytes = encode(&key, &units[..units.len().min(MOST_UNITS)], encrypting);
    bucket.put(&key, bytes.into(), PutMode::Create).await?;
    Ok(key)
}

/// What a read of a version's evidence found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// The tenant's newest recordings read, newest first.
    pub recordings: Vec<Vec<Unit>>,
    /// How many of those newest it could not read or open, which it says why of.
    pub refused: usize,
}

/// The newest [`KEPT`] recordings of `of` of the last [`KEPT_FOR`], by the bucket's time, read by
/// a reader holding `keys`, many at once: each asked once, what fails skipped and counted. None if
/// the listing failed, which it says why of.
pub async fn read(
    bucket: &impl Bucket,
    of: Of,
    keys: &(dyn extent_frame::Keys + Sync),
) -> Option<Read> {
    let listed = match bucket.list(&of.prefix()).await {
        Ok(listed) => listed,
        Err(err) => {
            log::warn!("cladefs: listing the read evidence at {}: {err}", of.prefix());
            return None;
        }
    };
    let now = bucket.now();
    let kept = listed.iter().filter(|meta| meta.modified.saturating_add(KEPT_FOR) > now);
    let newest: Vec<String> = newest(kept).into_iter().map(|m| m.key.clone()).collect();
    let reads = futures::stream::iter(newest).map(|key| async move {
        let found = bucket.get(&key).await.map_err(|err| err.to_string());
        found.and_then(|object| decode(&key, &object.bytes, keys).map_err(|e| e.to_string()))
    });
    let mut read = Read::default();
    let mut found = reads.buffered(KEPT);
    while let Some(units) = found.next().await {
        match units {
            Ok(units) => read.recordings.push(units),
            Err(why) => {
                log::warn!("cladefs: reading the read evidence at {}: {why}", of.prefix());
                read.refused += 1;
            }
        }
    }
    Some(read)
}

/// Of `listed`, one tenant's recordings of one version, the newest [`KEPT`], by the bucket's time,
/// newest first: of two written at once, the greater key.
fn newest<'a>(listed: impl Iterator<Item = &'a Meta>) -> Vec<&'a Meta> {
    let mut listed: Vec<&Meta> = listed.collect();
    listed.sort_by(|a, b| (b.modified, &b.key).cmp(&(a.modified, &a.key)));
    listed.truncate(KEPT);
    listed
}

/// What a version's next fork will read, as its tenant's recordings predict it: every unit any of
/// them read, each first where any of them read it first, in that order (TASK-702.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prediction {
    pub units: Vec<Unit>,
    /// How many recordings it was made from.
    pub recordings: usize,
}

impl Prediction {
    /// The prediction `recordings` make.
    pub fn of(recordings: &[Vec<Unit>]) -> Self {
        let mut first: HashMap<(ExtentId, u64), u32> = HashMap::new();
        for unit in recordings.iter().flatten() {
            let at = first.entry((unit.extent, unit.mib)).or_insert(unit.first);
            *at = (*at).min(unit.first);
        }
        let mut units: Vec<Unit> =
            first.into_iter().map(|((extent, mib), first)| Unit { extent, mib, first }).collect();
        units.sort_by_key(|unit| (unit.first, unit.extent, unit.mib));
        Self { units, recordings: recordings.len() }
    }
}

/// The evidence the pass over `cell` deletes, as of `now` by the bucket's time, of `listed`, its
/// listing: all but each tenant's newest [`KEPT`] of each version, and all older than
/// [`KEPT_FOR`]; and, if the pass read `named`, the versions the cell's head names by the segments
/// they are at, read after the listing, all of those of every other version, dropped, as all of a
/// dead cell's, which names none. Evidence needs no lease: nothing relies on it.
pub(crate) fn doomed<'a>(
    cell: CellId,
    listed: impl IntoIterator<Item = &'a Meta>,
    named: Option<&BTreeSet<u64>>,
    now: u64,
) -> Vec<&'a Meta> {
    let mut by: BTreeMap<(u64, u64), Vec<&Meta>> = BTreeMap::new();
    let mut doomed = Vec::new();
    for meta in listed {
        if let layout::Kind::Evidence { parent, at, tenant } = layout::kind(&meta.key)
            && parent == cell
        {
            match meta.modified.saturating_add(KEPT_FOR) > now {
                true => by.entry((at, tenant)).or_default().push(meta),
                false => doomed.push(meta),
            }
        }
    }
    for ((at, _), metas) in by {
        if named.is_some_and(|named| !named.contains(&at)) {
            doomed.extend(metas);
            continue;
        }
        let kept: BTreeSet<&str> = newest(metas.iter().copied()).iter().map(|m| &*m.key).collect();
        doomed.extend(metas.into_iter().filter(|meta| !kept.contains(meta.key.as_str())));
    }
    doomed.sort_by(|a, b| a.key.cmp(&b.key));
    doomed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use crate::testing::{self, Direct, SimBucket};
    use futures::executor::block_on;

    const VERSION: Of = Of { origin: Origin { parent: test_cell(3), at: 7, epoch: 1 }, tenant: 5 };

    fn units(of: u64) -> Vec<Unit> {
        let extent = ExtentId { cell: 3, sequence: of };
        vec![Unit { extent, mib: 0, first: 9 }, Unit { extent, mib: 2, first: 4 }]
    }

    /// A recording is written once at a key of its own, under its tenant's prefix of its version,
    /// and read back by a reader holding the version's cell's keys; one copied to another key, of
    /// another format, or read under other keys, is refused.
    #[test]
    fn recordings_are_read_back_under_the_versions_keys_alone() {
        let bucket = Direct(SimBucket::new());
        let cell = VERSION.cell();
        let key = block_on(write(&bucket, VERSION, &units(1), testing::encrypting(cell))).unwrap();
        assert!(key.starts_with(&VERSION.prefix()), "{key}");
        let ring = testing::ring([cell]);
        let read = block_on(read(&bucket, VERSION, &ring.of(cell.id)));
        assert_eq!(read, Some(Read { recordings: vec![units(1)], refused: 0 }));

        let bytes = block_on(bucket::get(&bucket, &key)).unwrap().unwrap().bytes;
        let elsewhere = layout::evidence(cell, 7, 5, 1);
        let copied = decode(&elsewhere, &bytes, &ring.of(cell.id)).map(drop);
        assert!(matches!(copied, Err(Error::Corrupt(ref why)) if why.contains("written at")));
        let other = testing::ring([test_cell(4)]);
        assert!(decode(&key, &bytes, &other.of(cell.id)).is_err());
        let footer = Footer { format: layout::FORMAT.current + 1, key: key.clone() };
        let newer =
            layout::with_footer(&layout::encode(&units(1)), &footer, testing::encrypting(cell));
        assert_eq!(
            decode(&key, &newer, &ring.of(cell.id)),
            Err(Error::EvidenceFormat(layout::FORMAT.current + 1))
        );
    }

    /// A tenant reads its own evidence of a version alone, the newest [`KEPT`] of it, newest first.
    #[test]
    fn a_tenant_reads_its_own_newest_evidence_alone() {
        let sim = SimBucket::new();
        let bucket = Direct(sim.clone());
        let cell = VERSION.cell();
        for of in 0..KEPT as u64 + 2 {
            block_on(write(&bucket, VERSION, &units(of), testing::encrypting(cell))).unwrap();
            sim.advance(1);
        }
        let another = Of { tenant: 6, ..VERSION };
        block_on(write(&bucket, another, &units(99), testing::encrypting(cell))).unwrap();
        let ring = testing::ring([cell]);
        let read = block_on(read(&bucket, VERSION, &ring.of(cell.id)));
        let newest: Vec<Vec<Unit>> = (2..KEPT as u64 + 2).rev().map(units).collect();
        assert_eq!(read, Some(Read { recordings: newest, refused: 0 }));
        let theirs = block_on(super::read(&bucket, another, &ring.of(cell.id))).unwrap();
        assert_eq!(theirs.recordings, vec![units(99)]);
        // A week on, none is read.
        sim.advance(KEPT_FOR);
        let read = block_on(super::read(&bucket, VERSION, &ring.of(cell.id)));
        assert_eq!(read, Some(Read::default()));
    }

    /// A prediction is every unit any recording read, each at its earliest first read, in that
    /// order.
    #[test]
    fn predictions_take_each_unit_at_its_earliest_read() {
        let extent = ExtentId { cell: 3, sequence: 1 };
        let other = ExtentId { cell: 3, sequence: 2 };
        let unit = |extent, mib, first| Unit { extent, mib, first };
        let recordings = vec![
            vec![unit(extent, 0, 30), unit(other, 1, 10)],
            vec![unit(extent, 0, 5), unit(extent, 1, 40)],
        ];
        let predicted = Prediction::of(&recordings);
        let units = vec![unit(extent, 0, 5), unit(other, 1, 10), unit(extent, 1, 40)];
        assert_eq!(predicted, Prediction { units, recordings: 2 });
    }

    /// The collector deletes all but each tenant's newest [`KEPT`] recordings of each version, every
    /// recording of a version the head names no more, and every one [`KEPT_FOR`] old.
    #[test]
    fn all_but_each_tenants_newest_evidence_goes() {
        let cell = test_cell(3);
        let meta = |at, tenant, id, modified| Meta {
            key: layout::evidence(cell, at, tenant, id),
            version: Default::default(),
            size: 1,
            modified,
        };
        let mut listed: Vec<Meta> = (0..KEPT as u64 + 2).map(|id| meta(7, 5, id, id)).collect();
        listed.push(meta(7, 6, 1, 0));
        listed.push(meta(9, 5, 1, 0));
        let keys = |doomed: Vec<&Meta>| doomed.iter().map(|m| m.key.clone()).collect::<Vec<_>>();
        let oldest = vec![meta(7, 5, 0, 0).key, meta(7, 5, 1, 1).key];
        assert_eq!(keys(doomed(cell, &listed, None, 0)), oldest);
        let named = BTreeSet::from([7]);
        let mut dropped = oldest.clone();
        dropped.push(meta(9, 5, 1, 0).key);
        assert_eq!(keys(doomed(cell, &listed, Some(&named), 0)), dropped);
        let all: Vec<String> = listed.iter().map(|m| m.key.clone()).collect();
        let mut all_sorted = all.clone();
        all_sorted.sort();
        assert_eq!(keys(doomed(cell, &listed, Some(&BTreeSet::new()), 0)), all_sorted);
        assert!(doomed(test_cell(4), &listed, None, 0).is_empty());
        // A week on, what was written at 0 goes, tenant 6's one recording among it.
        let aged = vec![meta(7, 6, 1, 0).key, meta(9, 5, 1, 0).key];
        assert_eq!(keys(doomed(cell, &listed[KEPT..], None, KEPT_FOR)), aged);
    }
}
