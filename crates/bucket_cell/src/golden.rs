// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the bucket layout's golden corpus (doc-28, board 10038): every frozen version of every
//! bucket object kind and the key grammar is read by every later build. Framed envelopes are tested
//! in `formats/extent-frame/`; here each object's versioned payload (and the key grammar) is pinned
//! alongside `fprint::TypeFingerprint` of every serde type.

use crate::cell::Kind as CellKind;
use crate::checkpoint;
use crate::evidence::{self, Unit};
use crate::identity::{self, Details, Entry, Identity, Registry};
use crate::keys::{Dek, InheritedDek, Kek};
use crate::layout::{
    self, CellId, Configuration, Contents, Continued, FORMAT, Footer, Fsync, Head, Kind, Location,
    Member, Mirror, NewVersion, Origin, Reference, Replication, Retired, Version, Wish,
};
use crate::objectid::BlockTaker;
use crate::passed::{CollectorLease, Passed, Sum, Usage};
use crate::record::Made;
use crate::recovery::{Begun, State};
use extent_frame::spent::Spent;
use extent_frame::table::{self, Appendix, Block, TableFooter};
use extent_keys::Wrapped;
use formats::golden::Family as _;
use fprint::TypeFingerprint;
use std::collections::BTreeMap;
use storage_device::ExtentId;

const CELL: CellId = CellId { tenant: 0x2f, id: (3 << 48) | 7 };
const PARENT: CellId = CellId { tenant: 0x2f, id: (1 << 48) | 5 };
const CHILD: CellId = CellId { tenant: 0x3a, id: (2 << 48) | 9 };

#[test]
fn every_frozen_version_is_read() {
    formats::golden::check::<BucketLayout>(env!("CARGO_MANIFEST_DIR"));
}

/// Checks format 31 heads decode into format 32 heads, preserving `Ordered` and `Replicated` and
/// mapping the removed `v31::Fsync::Local` to `Fsync::Ordered` (doc-28).
#[test]
fn v31_heads_convert_to_current_and_map_local_to_ordered() {
    let cases = formats::golden::cases(FORMAT, env!("CARGO_MANIFEST_DIR"), 31);
    for (name, bytes) in cases {
        match name.as_str() {
            "head-root" => {
                assert_eq!(Head::decode(CELL, &bytes).unwrap(), head_root());
            }
            "head-forking" => {
                assert_eq!(Head::decode(CELL, &bytes).unwrap(), head_forking());
            }
            "head-clone" => {
                assert_eq!(Head::decode(CELL, &bytes).unwrap(), head_clone());
            }
            _ => {}
        }
    }

    for (v31_fsync, expected) in [
        (layout::v31::Fsync::Ordered, Fsync::Ordered),
        (layout::v31::Fsync::Local, Fsync::Ordered),
        (layout::v31::Fsync::Replicated, Fsync::Replicated),
    ] {
        let old = layout::v31::Head {
            format: 31,
            owner: 0x42,
            replication: Some(layout::v31::Replication {
                wish: layout::v31::Wish { f: 1, fsync: v31_fsync },
                configuration: None,
            }),
            ..layout::v31::Head::default()
        };
        let bytes = layout::encode(&old);
        let decoded = Head::decode(CELL, &bytes).unwrap();
        assert_eq!(decoded.format, FORMAT.current);
        assert_eq!(
            decoded.replication,
            Some(Replication { wish: Wish { f: 1, fsync: expected }, configuration: None })
        );
    }
}

/// Checks every versioned bucket object kind refuses versions outside [`FORMAT`] and trailing bytes
/// (doc-28, board 10038).
#[test]
fn every_object_kind_refuses_other_versions_and_trailing_bytes() {
    for (name, bytes) in BucketLayout::write() {
        if name == "keys" || name == "evidence-units" {
            continue;
        }
        // Trailing byte refused.
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(
            BucketLayout::read_as(name, &trailing).is_err(),
            "{name}: trailing byte was accepted"
        );
        // Older and newer versions refused before decoding the rest.
        for wrong in [FORMAT.oldest - 1, FORMAT.current + 1] {
            let mut changed = bytes.clone();
            changed[..4].copy_from_slice(&wrong.to_le_bytes());
            let err = BucketLayout::read_as(name, &changed)
                .expect_err(&format!("{name}: version {wrong} was accepted"));
            assert!(
                err.contains(&format!("bucket layout version {wrong}")),
                "{name}: unexpected refusal for version {wrong}: {err}"
            );
        }
    }
}

struct BucketLayout;

fn head_root() -> Head {
    Head {
        format: FORMAT.current,
        owner: 0x1234_5678,
        made: Made::Root,
        kind: Some(CellKind::Fxfs),
        keks: vec![Kek { generation: 1, wrapped: Wrapped(vec![1, 2, 3, 4]) }],
        shredded: false,
        deks: vec![Dek { generation: 1, kek: 1, wrapped: Wrapped(vec![5, 6, 7, 8]) }],
        inherited: vec![InheritedDek {
            cell: PARENT,
            dek: Dek { generation: 2, kek: 1, wrapped: Wrapped(vec![9, 10]) },
        }],
        epoch: 4,
        holder: 0x99,
        token: 12,
        end: 5,
        swaps: 7,
        runs: vec![(0, 1), (3, 4)],
        checkpoint: Some((3, 4)),
        versions: vec![Version { id: 0xab, at: 3, epoch: 4, shared: true }],
        replication: Some(Replication {
            wish: Wish { f: 1, fsync: Fsync::Replicated },
            configuration: Some(Configuration {
                number: 2,
                epoch: 4,
                f: 1,
                members: vec![
                    Member { address: "10.0.0.1:9000".into(), incarnation: 101 },
                    Member { address: "10.0.0.2:9000".into(), incarnation: 202 },
                ],
            }),
        }),
    }
}

fn head_forking() -> Head {
    let origin = Origin { parent: PARENT, at: 11, epoch: 2 };
    Head {
        format: FORMAT.current,
        owner: 0x55,
        made: Made::Forking { origin, version: 0xcd },
        kind: Some(CellKind::Sqlite),
        ..Head::default()
    }
}

fn head_clone() -> Head {
    let origin = Origin { parent: PARENT, at: 11, epoch: 2 };
    Head {
        format: FORMAT.current,
        owner: 0x66,
        made: Made::Clone(origin),
        kind: Some(CellKind::Memory),
        shredded: true,
        epoch: Head::FINAL,
        token: u64::MAX,
        ..Head::default()
    }
}

fn collector_lease() -> CollectorLease {
    let mut kinds = BTreeMap::new();
    kinds.insert("head".into(), Sum { objects: 1, bytes: 128 });
    kinds.insert("segment".into(), Sum { objects: 4, bytes: 65536 });
    CollectorLease {
        format: FORMAT.current,
        epoch: 9,
        holder: 0x42,
        until: 1_700_000_060_000,
        ended: 1_700_000_000_000,
        passed: Some(Passed {
            listing: Some([0xaa; 32]),
            usage: Usage { measured: 1_700_000_000_000, kinds },
        }),
    }
}

fn identity_obj() -> Identity {
    Identity { format: FORMAT.current, number: 3, id: 0xdead_beef_cafe_0003 }
}

fn registry_obj() -> Registry {
    Registry {
        format: FORMAT.current,
        version: 2,
        buckets: vec![
            Entry {
                number: 1,
                id: 0x1111,
                details: Details {
                    provider: Some("s3".into()),
                    region: Some("us-east-1".into()),
                    url: Some("s3://cladefs-root/prefix".into()),
                    endpoint: None,
                    kms: Some("aws-kms://us-east-1?key=alias/cladefs-{tenant}".into()),
                    master_key_id: None,
                },
            },
            Entry {
                number: 2,
                id: 0x2222,
                details: Details {
                    provider: Some("gcs".into()),
                    region: Some("us-central1".into()),
                    url: Some("gs://cladefs-two".into()),
                    endpoint: Some("https://storage.googleapis.com".into()),
                    kms: None,
                    master_key_id: Some("0123456789abcdef".into()),
                },
            },
        ],
    }
}

fn segment_footer() -> Footer {
    Footer {
        cell: CELL,
        segment: 9,
        epoch: 4,
        contents: Contents {
            commits: vec![layout::tag(4, 11), layout::tag(4, 12)],
            extents: vec![(ExtentId::new(CELL.id, 5), 0, 4096)],
            own: vec![(6, 8192)],
            frees: vec![ExtentId::new(PARENT.id, 1)],
            root: Some(b"root".to_vec()),
            next_sequence: 13,
            capacity: Some(1 << 30),
            continued: Some(Continued {
                commit: layout::tag(4, 13),
                extents: vec![(ExtentId::new(CELL.id, 14), 4096, 8192)],
            }),
            versions: vec![NewVersion { id: 0xbe, keep_last: Some(3) }],
        },
    }
}

fn checkpoint_table_footer() -> TableFooter {
    let mut spent = Spent::default();
    spent.insert(2);
    spent.insert(3);
    let mut state = State::new(CELL);
    state.next_sequence = 20;
    state.root = Some(b"ckpt-root".to_vec());
    state.last_commit = Some(layout::tag(4, 12));
    state.through = Some(9);
    state.spent = spent;
    state.capacity = Some(2 << 30);
    state.begun = Some(Begun {
        commit: layout::tag(4, 13),
        runs: vec![(
            ExtentId::new(CELL.id, 14),
            Location { key: layout::segment(CELL, 9, 4), offset: 4096, len: 8192 },
        )],
    });
    TableFooter {
        rest: layout::encode_versioned(&state),
        keys: vec![layout::segment(CELL, 8, 4), layout::segment(CELL, 9, 4)],
        blocks: vec![Block {
            first: ExtentId::new(CELL.id, 5),
            offset: 0,
            len: 36,
            crc: 0x1234_5678,
        }],
        appendix: Appendix { offset: 36, len: 10, crc: 0x8765_4321 },
    }
}

fn evidence_key() -> String {
    layout::evidence(PARENT, 7, 5, 0x2a)
}

fn evidence_footer() -> evidence::Footer {
    evidence::Footer { format: FORMAT.current, key: evidence_key() }
}

fn evidence_units() -> Vec<Unit> {
    vec![
        Unit { extent: ExtentId::new(PARENT.id, 1), mib: 0, first: 9 },
        Unit { extent: ExtentId::new(PARENT.id, 2), mib: 3, first: 14 },
    ]
}

/// One key for every [`Kind`] variant (including `Former` format 28 head slots), pinning the key
/// grammar including base-32 bucket numbers (doc-28, board 10038).
fn grammar_keys() -> Vec<String> {
    vec![
        layout::segment(CELL, 7, 2),
        layout::extent(CELL, 2, 4, ExtentId::new(CELL.id, 9)),
        layout::checkpoint(CELL, 7, 2),
        layout::head(CELL),
        layout::objectid_block(0x2a),
        layout::IDENTITY.to_owned(),
        layout::BUCKETS.to_owned(),
        layout::kek(0x2a),
        layout::collector_lease(CELL),
        format!("{}head/0000000000003", CELL.prefix()),
        layout::reference(PARENT, 7, CHILD),
        layout::retired(PARENT, 7, CHILD),
        layout::pin(PARENT, 7),
        layout::hold(PARENT, 7, 0x1234),
        layout::mirror(PARENT, 7),
        layout::ephemeral_reference(PARENT, 7, 0x2a, CHILD),
        layout::ephemeral_index(0x2a, CELL, 7, CHILD),
        layout::decommissioned(0x1234, 0x2a),
        evidence_key(),
        "unknown/key".to_owned(),
    ]
}

impl formats::golden::Family for BucketLayout {
    const FORMAT: formats::Format = FORMAT;

    fn write() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("head-root", layout::encode(&head_root())),
            ("head-forking", layout::encode(&head_forking())),
            ("head-clone", layout::encode(&head_clone())),
            ("collector-lease", layout::encode(&collector_lease())),
            ("identity", layout::encode(&identity_obj())),
            ("registry", layout::encode(&registry_obj())),
            ("reference", layout::encode(&Reference::of(0x77))),
            ("retired", layout::encode(&Retired::NOW)),
            ("mirror", layout::encode(&Mirror::NOW)),
            ("objectid", layout::encode(&BlockTaker::of(0x88))),
            ("segment-footer", layout::encode_segment_footer(&segment_footer())),
            ("own-extent-footer", layout::own_extent_footer(ExtentId::new(CELL.id, 42))),
            ("checkpoint-footer", table::encode_footer(&checkpoint_table_footer(), FORMAT.current)),
            ("checkpoint-appendix", checkpoint::encode_appendix(&[(8, 4, 4096), (9, 4, 8192)])),
            ("evidence-footer", layout::encode(&evidence_footer())),
            ("evidence-units", layout::encode(&evidence_units())),
            ("keys", grammar_keys().join("\n").into_bytes()),
        ]
    }

    fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
        match version {
            31 => match case {
                "head-root" | "head-forking" | "head-clone" => {
                    let head: layout::v31::Head =
                        layout::decode(bytes).map_err(|err| err.to_string())?;
                    if head.format != 31 {
                        return Err(format!("expected format 31 head, got {}", head.format));
                    }
                    Ok(format!("{head:#?}"))
                }
                _ => Self::read_as(case, bytes),
            },
            32 => Self::read_as(case, bytes),
            _ => Err(format!("no decoder of version {version}")),
        }
    }

    fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
        Self::read_as(case, bytes).map(drop)
    }

    fn fingerprint(version: u32) -> Option<String> {
        let head = match version {
            31 => layout::v31::Head::fingerprint(),
            32 => Head::fingerprint(),
            _ => return None,
        };
        Some(
            [
                format!("Head = {head}"),
                format!("CollectorLease = {}", CollectorLease::fingerprint()),
                format!("Identity = {}", Identity::fingerprint()),
                format!("Registry = {}", Registry::fingerprint()),
                format!("Reference = {}", Reference::fingerprint()),
                format!("Retired = {}", Retired::fingerprint()),
                format!("Mirror = {}", Mirror::fingerprint()),
                format!("BlockTaker = {}", BlockTaker::fingerprint()),
                format!("Footer = {}", Footer::fingerprint()),
                format!("OwnExtent = {}", ExtentId::fingerprint()),
                format!("TableFooter = {}", TableFooter::fingerprint()),
                format!("State = {}", State::fingerprint()),
                format!("EvidenceFooter = {}", evidence::Footer::fingerprint()),
                format!("EvidenceUnit = {}", Unit::fingerprint()),
            ]
            .join("\n")
                + "\n",
        )
    }
}

impl BucketLayout {
    fn read_as(case: &str, bytes: &[u8]) -> Result<String, String> {
        let fmt = |res: Result<String, crate::Error>| res.map_err(|err| err.to_string());
        match case {
            "head-root" | "head-forking" | "head-clone" => {
                fmt(Head::decode(CELL, bytes).map(|head| format!("{head:#?}")))
            }
            "collector-lease" => {
                fmt(CollectorLease::decode(CELL, bytes).map(|lease| format!("{lease:#?}")))
            }
            "identity" => fmt(identity::parse(bytes).map(|id| format!("{id:#?}"))),
            "registry" => fmt(identity::parse_registry(bytes).map(|reg| format!("{reg:#?}"))),
            "reference" => fmt(Reference::decode(bytes).map(|r| format!("{r:#?}"))),
            "retired" => fmt(Retired::decode(bytes).map(|r| format!("{r:#?}"))),
            "mirror" => fmt(Mirror::decode(bytes).map(|m| format!("{m:#?}"))),
            "objectid" => fmt(BlockTaker::decode(bytes).map(|b| format!("{b:#?}"))),
            "segment-footer" => {
                fmt(layout::decode_segment_footer(bytes).map(|f| format!("{f:#?}")))
            }
            "own-extent-footer" => {
                fmt(layout::decode_own_extent_footer(bytes).map(|e| format!("{e:#?}")))
            }
            "checkpoint-footer" => {
                fmt(checkpoint::Table::decode(bytes).map(|table| format!("{table:#?}")))
            }
            "checkpoint-appendix" => {
                fmt(checkpoint::decode_appendix(bytes).map(|lengths| format!("{lengths:#?}")))
            }
            "evidence-footer" => fmt(evidence::Footer::decode(bytes).map(|f| format!("{f:#?}"))),
            "evidence-units" => {
                fmt(layout::decode::<Vec<Unit>>(bytes).map(|units| format!("{units:#?}")))
            }
            "keys" => {
                let text = std::str::from_utf8(bytes).map_err(|err| err.to_string())?;
                let mut seen = Vec::new();
                let mut lines = Vec::new();
                for key in text.lines() {
                    let kind = layout::kind(key);
                    seen.push(kind.name());
                    let routed = layout::routed_to(key);
                    let owner = layout::owner(key);
                    lines.push(format!(
                        "{key} => {kind:?} (name={}, owner={owner:?}, routed={routed:?})",
                        kind.name()
                    ));
                }
                // Exhaustive check: every Kind variant is exercised by the corpus.
                for required in [
                    Kind::Segment { cell: CELL, segment: 0, epoch: 0 }.name(),
                    Kind::Extent { cell: CELL, epoch: 0, segment: 0, sequence: 0 }.name(),
                    Kind::Checkpoint { cell: CELL, segment: 0, epoch: 0 }.name(),
                    Kind::Head { cell: CELL }.name(),
                    Kind::ObjectIds { block: 0 }.name(),
                    Kind::Identity.name(),
                    Kind::Buckets.name(),
                    Kind::Kek { tenant: 0 }.name(),
                    Kind::CollectorLease { cell: CELL }.name(),
                    Kind::Former { cell: CELL, format: 28 }.name(),
                    Kind::Reference { parent: CELL, at: 0, child: CELL }.name(),
                    Kind::Retired { parent: CELL, at: 0, child: CELL }.name(),
                    Kind::Pin { parent: CELL, at: 0 }.name(),
                    Kind::Hold { parent: CELL, at: 0, bucket: 0 }.name(),
                    Kind::Mirror { cell: CELL, at: 0 }.name(),
                    Kind::Ephemeral { parent: CELL, at: 0, incarnation: 0, child: CELL }.name(),
                    Kind::EphemeralIndex { incarnation: 0 }.name(),
                    Kind::Decommissioned { incarnation: 0 }.name(),
                    Kind::Evidence { parent: CELL, at: 0, tenant: 0 }.name(),
                    Kind::Unknown.name(),
                ] {
                    if !seen.contains(&required) {
                        return Err(format!("missing key kind {required:?}"));
                    }
                }
                Ok(lines.join("\n") + "\n")
            }
            _ => Err(format!("no case {case}")),
        }
    }
}
