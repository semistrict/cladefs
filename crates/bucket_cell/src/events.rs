// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: global control-plane event stream and durable state in a single root-bucket object
//! (`events`, [`layout::EVENTS`]).
//!
//! Optimized for cheap repeated conditional GETs (`If-None-Match` -> 304 Not Modified) by every
//! extent daemon every ~20 seconds and rare conditional PUTs (CAS create/update) by control-plane
//! operations:
//! - [`EventKind::TenantShredded`] + durable `shredded_tenants` set: halts open cells on every
//!   host and instructs `cladefs-gc` to reap shredded tenants' cells, read evidence, and orphaned
//!   ephemeral references without host fan-out.
//! - [`EventKind::CellShredded`]: halts open cells of a shredded cell on every host.
//! - [`EventKind::HostDecommissioned`] + durable `decommissioned_hosts` set: notifies hosts when
//!   an ephemeral incarnation is decommissioned.
//! - [`EventKind::PeersUpdated`] + durable `bucket_peers` map: propagates peer membership updates
//!   per bucket without fanning out `SetPeers` to every daemon.
//!
//! Wire format ([`FORMAT`], frozen): `b"fxev"` magic (4 bytes) || version (`u32` LE, 4 bytes) ||
//! Protobuf-encoded [`EventLog`]. New control-plane event kinds and durable state fields are added
//! as new Protobuf tags without bumping [`FORMAT`].

use crate::bucket::{self, Bucket, Created, IfChanged, Updated, Version};
use crate::layout::{self, CellId};
#[cfg(feature = "full")]
use crate::routed::RoutedBucket;
use prost::Message as _;
use std::collections::BTreeSet;
#[cfg(feature = "full")]
use std::sync::Arc;

/// Frozen format family of the `events` object (doc-28): every later build reads every version
/// from `oldest` through `current`.
pub const FORMAT: formats::Format = formats::Format::new("bucket events", 1, 1);

/// 4-byte magic prefix of an `events` object.
const MAGIC: [u8; 4] = *b"fxev";

/// Maximum encoded size accepted for an `events` object (4 MiB), bounding untrusted decode work.
pub const MAX_BYTES: usize = 4 << 20;

/// Maximum number of recent events retained in the `events` tail before trimming.
pub const MAX_EVENTS: usize = 1024;

/// How long recent events are retained in the `events` tail (24 hours in ms).
pub const RETAIN_MS: u64 = 24 * 60 * 60 * 1000;

/// Global control-plane event log and durable state stored at [`layout::EVENTS`].
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct EventLog {
    /// Highest sequence number assigned so far (0 if no events have been published).
    #[prost(uint64, tag = "1")]
    pub latest_seq: u64,
    /// Sequence number of the oldest event still present in `events` (0 if `events` is empty).
    #[prost(uint64, tag = "2")]
    pub oldest_seq: u64,
    /// Bounded tail of recent control-plane events in strictly increasing `seq` order.
    #[prost(message, repeated, tag = "3")]
    pub events: Vec<Event>,
    /// Durable sorted, deduplicated set of shredded tenant IDs. Survives event-tail trimming.
    #[prost(uint64, repeated, tag = "4")]
    pub shredded_tenants: Vec<u64>,
    /// Durable sorted, deduplicated set of decommissioned host incarnations.
    #[prost(uint64, repeated, tag = "5")]
    pub decommissioned_hosts: Vec<u64>,
    /// Durable peer membership per bucket number, sorted by `bucket`.
    #[prost(message, repeated, tag = "6")]
    pub bucket_peers: Vec<BucketPeers>,
}

/// Durable peer list for one bucket.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct BucketPeers {
    /// Bucket number (`0` for an unformatted single bucket, else its identity number).
    #[prost(uint32, tag = "1")]
    pub bucket: u32,
    /// Monotonically increasing generation bumped on each peer update for this bucket.
    #[prost(uint64, tag = "2")]
    pub generation: u64,
    /// Peer `HOST:PORT` addresses.
    #[prost(string, repeated, tag = "3")]
    pub addresses: Vec<String>,
}

/// One control-plane event in [`EventLog::events`].
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Event {
    #[prost(uint64, tag = "1")]
    pub seq: u64,
    #[prost(uint64, tag = "2")]
    pub unix_ms: u64,
    #[prost(oneof = "EventKind", tags = "10, 11, 12, 13")]
    pub kind: Option<EventKind>,
}

/// Payload of a control-plane [`Event`].
#[derive(Clone, PartialEq, Eq, prost::Oneof)]
pub enum EventKind {
    #[prost(message, tag = "10")]
    TenantShredded(TenantShredded),
    #[prost(message, tag = "11")]
    CellShredded(CellShredded),
    #[prost(message, tag = "12")]
    HostDecommissioned(HostDecommissioned),
    #[prost(message, tag = "13")]
    PeersUpdated(PeersUpdated),
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TenantShredded {
    #[prost(uint64, tag = "1")]
    pub tenant: u64,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct CellShredded {
    #[prost(uint64, tag = "1")]
    pub tenant: u64,
    #[prost(uint64, tag = "2")]
    pub cell: u64,
}

impl CellShredded {
    pub fn cell_id(&self) -> CellId {
        CellId { tenant: self.tenant, id: self.cell }
    }
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct HostDecommissioned {
    #[prost(uint64, tag = "1")]
    pub incarnation: u64,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct PeersUpdated {
    #[prost(uint32, tag = "1")]
    pub bucket: u32,
    #[prost(uint64, tag = "2")]
    pub generation: u64,
    #[prost(string, repeated, tag = "3")]
    pub addresses: Vec<String>,
}

/// What a caller asks [`publish`] to record in [`EventLog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publish {
    TenantShredded(u64),
    CellShredded(CellId),
    HostDecommissioned(u64),
    PeersUpdated { bucket: u32, addresses: Vec<String> },
}

/// What [`Poller::poll`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// Complete decoded log (durable state + event tail).
    pub log: EventLog,
    /// Events strictly newer than the poller's previous `last_seq`.
    pub new_events: Vec<Event>,
    /// True if some events between the poller's previous `last_seq` and `log.oldest_seq` were
    /// trimmed before this poll saw them; callers should reconcile against `log`'s durable state.
    pub skipped: bool,
}

impl EventLog {
    /// Shredded tenants as a set (excluding the default tenant 0 if ever malformed).
    pub fn shredded_set(&self) -> BTreeSet<u64> {
        self.shredded_tenants.iter().copied().filter(|&t| t != layout::DEFAULT_TENANT).collect()
    }

    /// Durable peer list for `bucket`, if recorded.
    pub fn peers_for(&self, bucket: u32) -> Option<&BucketPeers> {
        self.bucket_peers.iter().find(|p| p.bucket == bucket)
    }

    /// Applies `change` at `now_ms`, returning `true` if the log changed and must be written.
    fn apply(&mut self, change: &Publish, now_ms: u64) -> bool {
        let kind = match change {
            Publish::TenantShredded(tenant) => {
                if let Err(pos) = self.shredded_tenants.binary_search(tenant) {
                    self.shredded_tenants.insert(pos, *tenant);
                } else {
                    return false;
                }
                EventKind::TenantShredded(TenantShredded { tenant: *tenant })
            }
            Publish::CellShredded(cell) => {
                EventKind::CellShredded(CellShredded { tenant: cell.tenant, cell: cell.id })
            }
            Publish::HostDecommissioned(incarnation) => {
                if let Err(pos) = self.decommissioned_hosts.binary_search(incarnation) {
                    self.decommissioned_hosts.insert(pos, *incarnation);
                } else {
                    return false;
                }
                EventKind::HostDecommissioned(HostDecommissioned { incarnation: *incarnation })
            }
            Publish::PeersUpdated { bucket, addresses } => {
                let generation =
                    match self.bucket_peers.binary_search_by_key(bucket, |entry| entry.bucket) {
                        Ok(idx) => {
                            if self.bucket_peers[idx].addresses == *addresses {
                                return false;
                            }
                            let next = self.bucket_peers[idx].generation.saturating_add(1);
                            self.bucket_peers[idx].generation = next;
                            self.bucket_peers[idx].addresses = addresses.clone();
                            next
                        }
                        Err(idx) => {
                            self.bucket_peers.insert(
                                idx,
                                BucketPeers {
                                    bucket: *bucket,
                                    generation: 1,
                                    addresses: addresses.clone(),
                                },
                            );
                            1
                        }
                    };
                EventKind::PeersUpdated(PeersUpdated {
                    bucket: *bucket,
                    generation,
                    addresses: addresses.clone(),
                })
            }
        };

        let next_seq = self.latest_seq.saturating_add(1);
        self.latest_seq = next_seq;
        self.events.push(Event { seq: next_seq, unix_ms: now_ms, kind: Some(kind) });
        self.trim(now_ms);
        true
    }

    /// Trims old events past [`RETAIN_MS`] or [`MAX_EVENTS`], updating `oldest_seq`.
    fn trim(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(RETAIN_MS);
        // Always keep at least the latest event when non-empty.
        if self.events.len() > 1 {
            let keep_from = self
                .events
                .iter()
                .position(|ev| ev.unix_ms >= cutoff)
                .unwrap_or(self.events.len() - 1);
            if keep_from > 0 {
                self.events.drain(..keep_from);
            }
        }
        if self.events.len() > MAX_EVENTS {
            let drop_count = self.events.len() - MAX_EVENTS;
            self.events.drain(..drop_count);
        }
        self.oldest_seq = self.events.first().map_or(0, |ev| ev.seq);
    }
}

/// Encodes `log` into the frozen wire format (`b"fxev" || version_le || protobuf`).
pub fn encode(log: &EventLog) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + log.encoded_len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT.current.to_le_bytes());
    log.encode(&mut out).expect("Vec<u8> has unbounded capacity");
    out
}

/// Decodes untrusted bytes from [`layout::EVENTS`] into an [`EventLog`].
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn decode(bytes: &[u8]) -> Result<EventLog, crate::Error> {
    if bytes.len() > MAX_BYTES {
        return Err(crate::Error::Corrupt(format!(
            "{}: {} bytes exceeds limit of {MAX_BYTES}",
            layout::EVENTS,
            bytes.len()
        )));
    }
    let Some((magic, rest)) = bytes.split_at_checked(MAGIC.len()) else {
        return Err(crate::Error::Corrupt(format!("{}: truncated header", layout::EVENTS)));
    };
    if magic != MAGIC {
        return Err(crate::Error::Corrupt(format!("{}: bad magic", layout::EVENTS)));
    }
    let Some((ver_bytes, payload)) = rest.split_at_checked(4) else {
        return Err(crate::Error::Corrupt(format!("{}: truncated version", layout::EVENTS)));
    };
    let ver_arr: [u8; 4] = ver_bytes
        .try_into()
        .map_err(|_| crate::Error::Corrupt(format!("{}: truncated version", layout::EVENTS)))?;
    let version = u32::from_le_bytes(ver_arr);
    FORMAT.check(version).map_err(|err| crate::Error::Corrupt(err.to_string()))?;

    let log = EventLog::decode(payload).map_err(|err| {
        crate::Error::Corrupt(format!("{}: protobuf decode: {err}", layout::EVENTS))
    })?;

    if log.oldest_seq > log.latest_seq {
        return Err(crate::Error::Corrupt(format!(
            "{}: oldest_seq {} > latest_seq {}",
            layout::EVENTS,
            log.oldest_seq,
            log.latest_seq
        )));
    }
    let mut prev_seq = 0u64;
    for ev in &log.events {
        if ev.seq == 0 || ev.seq <= prev_seq || ev.seq > log.latest_seq {
            return Err(crate::Error::Corrupt(format!(
                "{}: out-of-order or invalid event seq {}",
                layout::EVENTS,
                ev.seq
            )));
        }
        if log.oldest_seq != 0 && ev.seq < log.oldest_seq {
            return Err(crate::Error::Corrupt(format!(
                "{}: event seq {} < oldest_seq {}",
                layout::EVENTS,
                ev.seq,
                log.oldest_seq
            )));
        }
        prev_seq = ev.seq;
    }
    Ok(log)
}

/// The authoritative bucket holding [`layout::EVENTS`]: root bucket #1 when `home` is formatted
/// and routed, else `home.own()`.
#[cfg(feature = "full")]
pub async fn authority<B: Bucket>(home: &RoutedBucket<B>) -> Result<Arc<B>, crate::Error> {
    if !home.is_routed() {
        return Ok(home.own());
    }
    let own = crate::identity::get(&*home.own()).await?;
    match own {
        Some(id) if id.number != crate::identity::ROOT => {
            home.numbered(crate::identity::ROOT).await
        }
        _ => Ok(home.own()),
    }
}

/// The bucket number of `home` for [`PeersUpdated`] / [`BucketPeers`] (`0` if unformatted).
#[cfg(feature = "full")]
pub async fn bucket_number<B: Bucket>(home: &RoutedBucket<B>) -> Result<u32, crate::Error> {
    Ok(crate::identity::get(&*home.own()).await?.map_or(0, |id| u32::from(id.number)))
}

/// Reads and decodes [`layout::EVENTS`] from `bucket`, or `Ok(None)` if not yet created.
pub async fn get(bucket: &impl Bucket) -> Result<Option<EventLog>, crate::Error> {
    let Some(object) = bucket::get(bucket, layout::EVENTS).await? else {
        return Ok(None);
    };
    decode(&object.bytes).map(Some)
}

/// Publishes `change` to [`layout::EVENTS`] in `bucket` using CAS create/update.
/// Returns the updated [`EventLog`].
pub async fn publish(bucket: &impl Bucket, change: Publish) -> Result<EventLog, crate::Error> {
    loop {
        let now = bucket.now();
        match bucket::get(bucket, layout::EVENTS).await? {
            None => {
                let mut log = EventLog::default();
                if !log.apply(&change, now) {
                    return Ok(log);
                }
                let bytes = encode(&log);
                match bucket::create(bucket, layout::EVENTS, bytes).await? {
                    Created::Created(_) => return Ok(log),
                    Created::Exists(_) => continue,
                }
            }
            Some(object) => {
                let mut log = decode(&object.bytes)?;
                if !log.apply(&change, now) {
                    return Ok(log);
                }
                let bytes = encode(&log);
                match bucket::update(bucket, layout::EVENTS, bytes, &object.meta.version).await? {
                    Updated::Updated(_) => return Ok(log),
                    Updated::Changed => continue,
                }
            }
        }
    }
}

/// Stateful poller for [`layout::EVENTS`] using `If-None-Match` conditional GETs.
#[derive(Clone, Debug, Default)]
pub struct Poller {
    last_version: Option<Version>,
    last_seq: u64,
}

impl Poller {
    pub fn new() -> Self {
        Self::default()
    }

    /// Last sequence number processed by this poller.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Polls `bucket` for changes to [`layout::EVENTS`].
    /// Returns `Ok(None)` when the object does not exist or is unchanged (304 Not Modified).
    pub async fn poll(&mut self, bucket: &impl Bucket) -> Result<Option<Update>, crate::Error> {
        let object =
            match bucket::get_if_changed(bucket, layout::EVENTS, self.last_version.as_ref()).await?
            {
                IfChanged::Missing | IfChanged::Unchanged => return Ok(None),
                IfChanged::Changed(object) => object,
            };
        let log = decode(&object.bytes)?;
        let skipped = self.last_seq > 0 && log.oldest_seq > self.last_seq.saturating_add(1);
        let new_events: Vec<Event> =
            log.events.iter().filter(|ev| ev.seq > self.last_seq).cloned().collect();
        self.last_version = Some(object.meta.version);
        self.last_seq = log.latest_seq;
        Ok(Some(Update { log, new_events, skipped }))
    }
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use crate::object_bucket::{ObjectBucket, Request};
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;
    use object_store::memory::InMemory;

    struct BucketEvents;

    fn sample_populated() -> EventLog {
        EventLog {
            latest_seq: 4,
            oldest_seq: 1,
            events: vec![
                Event {
                    seq: 1,
                    unix_ms: 1_700_000_000_000,
                    kind: Some(EventKind::TenantShredded(TenantShredded { tenant: 7 })),
                },
                Event {
                    seq: 2,
                    unix_ms: 1_700_000_001_000,
                    kind: Some(EventKind::CellShredded(CellShredded {
                        tenant: 9,
                        cell: (1 << 48) | 0x2a,
                    })),
                },
                Event {
                    seq: 3,
                    unix_ms: 1_700_000_002_000,
                    kind: Some(EventKind::HostDecommissioned(HostDecommissioned {
                        incarnation: 0x55,
                    })),
                },
                Event {
                    seq: 4,
                    unix_ms: 1_700_000_003_000,
                    kind: Some(EventKind::PeersUpdated(PeersUpdated {
                        bucket: 1,
                        generation: 2,
                        addresses: vec!["10.0.0.1:9000".into(), "10.0.0.2:9000".into()],
                    })),
                },
            ],
            shredded_tenants: vec![7],
            decommissioned_hosts: vec![0x55],
            bucket_peers: vec![BucketPeers {
                bucket: 1,
                generation: 2,
                addresses: vec!["10.0.0.1:9000".into(), "10.0.0.2:9000".into()],
            }],
        }
    }

    impl formats::golden::Family for BucketEvents {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            vec![
                ("empty", encode(&EventLog::default())),
                ("populated", encode(&sample_populated())),
            ]
        }

        fn written(version: u32, _case: &str, bytes: &[u8]) -> Result<String, String> {
            assert_eq!(version, 1);
            let decoded = decode(bytes).map_err(|err| err.to_string())?;
            Ok(format!("{decoded:#?}"))
        }

        fn read(_case: &str, bytes: &[u8]) -> Result<(), String> {
            decode(bytes).map(drop).map_err(|err| err.to_string())
        }
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<BucketEvents>(env!("CARGO_MANIFEST_DIR"));
    }

    #[test]
    fn decode_refuses_malformed_and_unsupported_versions() {
        let valid = encode(&sample_populated());
        assert_eq!(decode(&valid).unwrap(), sample_populated());

        // Truncated inputs.
        assert!(decode(&[]).is_err());
        assert!(decode(b"fxe").is_err());
        assert!(decode(b"fxev\x01\x00").is_err());

        // Wrong magic.
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert!(decode(&bad_magic).is_err());

        // Unsupported versions.
        for wrong in [FORMAT.oldest - 1, FORMAT.current + 1] {
            let mut bad_ver = valid.clone();
            bad_ver[4..8].copy_from_slice(&wrong.to_le_bytes());
            let err = decode(&bad_ver).unwrap_err().to_string();
            assert!(err.contains(&format!("bucket events version {wrong}")), "{err}");
        }

        // Oversized input.
        let oversized = vec![0u8; MAX_BYTES + 1];
        assert!(decode(&oversized).is_err());

        // Out-of-order / invalid sequence numbers.
        let mut bad_seq = sample_populated();
        bad_seq.oldest_seq = 99;
        assert!(decode(&encode(&bad_seq)).is_err());

        let mut dup_seq = sample_populated();
        dup_seq.events[1].seq = 1;
        assert!(decode(&encode(&dup_seq)).is_err());
    }

    #[test]
    fn publish_is_idempotent_and_poller_uses_if_none_match() {
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        block_on(async {
            let mut poller = Poller::new();
            assert_eq!(poller.poll(&*bucket).await.unwrap(), None);

            // Publish tenant shred.
            let log = publish(&*bucket, Publish::TenantShredded(42)).await.unwrap();
            assert_eq!(log.latest_seq, 1);
            assert_eq!(log.shredded_tenants, vec![42]);

            // Repeated shred of same tenant performs zero PUTs.
            let updates_before = bucket.stats().requests(Request::Update);
            let creates_before = bucket.stats().requests(Request::Create);
            let again = publish(&*bucket, Publish::TenantShredded(42)).await.unwrap();
            assert_eq!(again.latest_seq, 1);
            assert_eq!(bucket.stats().requests(Request::Update), updates_before);
            assert_eq!(bucket.stats().requests(Request::Create), creates_before);

            // First poll receives the update.
            let update = poller.poll(&*bucket).await.unwrap().expect("update");
            assert!(!update.skipped);
            assert_eq!(update.new_events.len(), 1);
            assert_eq!(update.log.shredded_tenants, vec![42]);

            // Second poll gets 304 Not Modified (0 bytes downloaded).
            let bytes_before = bucket.stats().downloaded;
            assert_eq!(poller.poll(&*bucket).await.unwrap(), None);
            assert_eq!(bucket.stats().downloaded, bytes_before);

            // Publish peers update and verify generation bumps.
            let addrs = vec!["10.0.0.1:9000".to_owned()];
            let log =
                publish(&*bucket, Publish::PeersUpdated { bucket: 1, addresses: addrs.clone() })
                    .await
                    .unwrap();
            assert_eq!(log.peers_for(1).unwrap().generation, 1);
            assert_eq!(log.peers_for(1).unwrap().addresses, addrs);

            let update = poller.poll(&*bucket).await.unwrap().expect("peers update");
            assert_eq!(update.new_events.len(), 1);
            assert_eq!(poller.last_seq(), 2);
        });
    }

    #[test]
    fn event_tail_trims_and_poller_reports_gap_while_durable_state_remains() {
        let sim = SimBucket::new();
        let bucket = Direct(sim.clone());
        block_on(async {
            let mut poller = Poller::new();
            publish(&bucket, Publish::TenantShredded(5)).await.unwrap();
            let first = poller.poll(&bucket).await.unwrap().unwrap();
            assert!(!first.skipped);

            // Advance past retention window and publish a new event so seq 1 is trimmed.
            sim.advance(RETAIN_MS + 1000);
            publish(&bucket, Publish::HostDecommissioned(0x11)).await.unwrap();
            publish(&bucket, Publish::CellShredded(CellId { tenant: 6, id: 10 })).await.unwrap();

            let update = poller.poll(&bucket).await.unwrap().unwrap();
            // Durable state keeps tenant 5 even though its event was trimmed.
            assert_eq!(update.log.shredded_tenants, vec![5]);
            assert_eq!(update.log.decommissioned_hosts, vec![0x11]);
            assert_eq!(update.log.oldest_seq, 2);
            assert!(!update.skipped);

            // A poller that saw seq 1 while seq 2..=3 were also trimmed sees `skipped = true`.
            let mut lagging = Poller::new();
            lagging.last_seq = 1;
            sim.advance(RETAIN_MS + 1000);
            publish(&bucket, Publish::CellShredded(CellId { tenant: 6, id: 11 })).await.unwrap();
            let gap = lagging.poll(&bucket).await.unwrap().unwrap();
            assert!(gap.skipped);
            assert_eq!(gap.log.shredded_tenants, vec![5]);
        });
    }
}
