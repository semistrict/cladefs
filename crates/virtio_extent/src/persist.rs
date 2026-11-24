// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device in a snapshot of its VM (TASK-39.1), serialized as a version-prefixed tuple
//! `(version, StateV2)` ([`FORMAT`], doc-28, board 10058), beside its transport's state and
//! [`VIRTIO_EXTENT_VERSION`], which a restore checks ([`check_version`]).
//!
//! Because a VMM's snapshot format (Firecracker's `bitcode`) is not self-describing, [`State`]
//! serializes in serde as the 2-tuple `(FORMAT.current, StateV2)` so `bitcode` keeps full
//! columnar packing of `StateV2`'s fields while [`Deserialize`] reads the leading `u32` version
//! and calls [`FORMAT::check`] before decoding any payload fields, refusing an unsupported version
//! by name rather than failing with a raw bitcode schema error.
//!
//! The protocol (doc-4, Snapshots): the guest's filesystem is frozen, nothing open on the device
//! (`VIRTIO_EXTENT_T_FREEZE`), and the VM paused, its requests in flight served; the device forks
//! its cell then, to the version the snapshot holds of it, and records the window's mappings
//! ([`Device::take_snapshot`](crate::Device::take_snapshot)). A VM restored from the snapshot opens
//! a cell of its own, forked from that version if it doesn't exist (each restore its own, given
//! its path as it loads), and maps the window as it was, where it was in guest memory
//! ([`Device::restore`](crate::Device::restore)); its guest learns its cell ID as its
//! filesystem thaws.
//!
//! [`VIRTIO_EXTENT_VERSION`]: crate::protocol::VIRTIO_EXTENT_VERSION
//! [`check_version`]: crate::protocol::check_version

use bincode::Options as _;
use fprint::TypeFingerprint;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{CacheType, Config};

/// The family of an extent-store device's saved state in a VM snapshot (doc-28, board 10058):
/// 1, unversioned bitcode fields with `dax_private_max` in pages; 2, frozen: version-prefixed
/// payload (`[version: u32 LE][bincode(StateV2)]`, trailing bytes refused), read by
/// [`FORMAT::check`] before decoding, with `dax_private_max_bytes` in bytes.
pub const FORMAT: formats::Format = formats::Format::new("snapshot extent state", 2, 2);

/// Maximum encoded device state accepted from a snapshot (64 MiB): bounds allocations from hostile
/// or corrupted snapshot bytes before bincode allocates `Vec<MappingStateV2>` (AGENTS.md, Trust
/// boundaries).
const MAX_STATE_BYTES: u64 = 64 << 20;

/// What a snapshot of its VM holds of an extent-store device, besides its transport's state and
/// its protocol version. Serializes as a version-prefixed byte payload ([`State::encode`],
/// [`State::decode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Its configuration: its cell's path is the one a restore opens, unless the snapshot's
    /// load overrides it.
    pub config: ConfigState,
    /// Where the window was in guest memory, if the device has one.
    pub window_addr: Option<u64>,
    /// The version of the cell the snapshot holds, which its restores fork, on any host.
    pub version: cladefs::CellVersion,
    /// The window's mappings.
    pub mappings: Vec<MappingState>,
    /// Whether the guest's filesystem was frozen: it is, as a snapshot is taken.
    pub frozen: bool,
}

/// A device's configuration ([`Config`]), as a snapshot holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigState {
    pub id: String,
    pub tag: String,
    pub path_on_host: String,
    pub capacity: u64,
    pub dax_window_size: Option<u64>,
    /// cladefs: the most bytes of private pages the window holds, as configured (doc-13, TASK-502,
    /// board 10058).
    pub dax_private_max_bytes: Option<u64>,
    pub cladefsd_socket: String,
    /// cladefs: what it does with the guest's asks for uploaded commits (TASK-363).
    pub cache_type: CacheType,
}

impl From<&Config> for ConfigState {
    fn from(config: &Config) -> Self {
        Self {
            id: config.id.clone(),
            tag: config.tag.clone(),
            path_on_host: config.path_on_host.clone(),
            capacity: config.capacity,
            dax_window_size: config.dax_window_size,
            dax_private_max_bytes: config.dax_private_max_bytes,
            cladefsd_socket: config.cladefsd_socket.clone(),
            cache_type: config.cache_type,
        }
    }
}

impl From<&ConfigState> for Config {
    fn from(config: &ConfigState) -> Self {
        Self {
            id: config.id.clone(),
            tag: config.tag.clone(),
            path_on_host: config.path_on_host.clone(),
            capacity: config.capacity,
            dax_window_size: config.dax_window_size,
            dax_private_max_bytes: config.dax_private_max_bytes,
            cladefsd_socket: config.cladefsd_socket.clone(),
            cache_type: config.cache_type,
        }
    }
}

/// A mapping of the window ([`cladefs::WindowMapping`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingState {
    pub window_offset: u64,
    pub len: u64,
    pub contents: MappingContentsState,
}

/// What a mapping of the window maps ([`cladefs::MappingContents`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingContentsState {
    Extent { cell: u64, sequence: u64, offset: u64 },
    Zeros,
}

impl From<&cladefs::WindowMapping> for MappingState {
    fn from(mapping: &cladefs::WindowMapping) -> Self {
        Self {
            window_offset: mapping.window_offset,
            len: mapping.len,
            contents: match mapping.contents {
                cladefs::MappingContents::Extent { extent, offset } => {
                    MappingContentsState::Extent {
                        cell: extent.cell,
                        sequence: extent.sequence,
                        offset,
                    }
                }
                cladefs::MappingContents::Zeros => MappingContentsState::Zeros,
            },
        }
    }
}

impl From<&MappingState> for cladefs::WindowMapping {
    fn from(mapping: &MappingState) -> Self {
        Self {
            window_offset: mapping.window_offset,
            len: mapping.len,
            contents: match mapping.contents {
                MappingContentsState::Extent { cell, sequence, offset } => {
                    cladefs::MappingContents::Extent {
                        extent: cladefs::ExtentId { cell, sequence },
                        offset,
                    }
                }
                MappingContentsState::Zeros => cladefs::MappingContents::Zeros,
            },
        }
    }
}

/// Errors decoding a versioned extent device [`State`].
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("snapshot extent state: truncated before version")]
    Truncated,
    #[error("snapshot extent state: {len} bytes exceeds maximum {max}")]
    TooLarge { len: usize, max: u64 },
    #[error("{0}")]
    Version(#[from] formats::Refused),
    #[error("snapshot extent state: {0}")]
    Decode(#[from] bincode::Error),
}

// Frozen wire types for version 2 (doc-28, board 10058). Never edit these types; a schema change
// adds `StateV3` and converts `StateV2 -> State`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
struct StateV2 {
    config: ConfigStateV2,
    window_addr: Option<u64>,
    version: CellVersionV2,
    mappings: Vec<MappingStateV2>,
    frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
struct ConfigStateV2 {
    id: String,
    tag: String,
    path_on_host: String,
    capacity: u64,
    dax_window_size: Option<u64>,
    dax_private_max_bytes: Option<u64>,
    cladefsd_socket: String,
    cache_type: CacheTypeV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
enum CacheTypeV2 {
    Unsafe,
    Writeback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
struct CellVersionV2 {
    tenant: u64,
    cell: u64,
    version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
struct MappingStateV2 {
    window_offset: u64,
    len: u64,
    contents: MappingContentsStateV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, fprint::TypeFingerprint)]
enum MappingContentsStateV2 {
    Extent { cell: u64, sequence: u64, offset: u64 },
    Zeros,
}

impl From<&State> for StateV2 {
    fn from(state: &State) -> Self {
        Self {
            config: ConfigStateV2 {
                id: state.config.id.clone(),
                tag: state.config.tag.clone(),
                path_on_host: state.config.path_on_host.clone(),
                capacity: state.config.capacity,
                dax_window_size: state.config.dax_window_size,
                dax_private_max_bytes: state.config.dax_private_max_bytes,
                cladefsd_socket: state.config.cladefsd_socket.clone(),
                cache_type: match state.config.cache_type {
                    CacheType::Unsafe => CacheTypeV2::Unsafe,
                    CacheType::Writeback => CacheTypeV2::Writeback,
                },
            },
            window_addr: state.window_addr,
            version: CellVersionV2 {
                tenant: state.version.tenant,
                cell: state.version.cell,
                version: state.version.version,
            },
            mappings: state
                .mappings
                .iter()
                .map(|m| MappingStateV2 {
                    window_offset: m.window_offset,
                    len: m.len,
                    contents: match m.contents {
                        MappingContentsState::Extent { cell, sequence, offset } => {
                            MappingContentsStateV2::Extent { cell, sequence, offset }
                        }
                        MappingContentsState::Zeros => MappingContentsStateV2::Zeros,
                    },
                })
                .collect(),
            frozen: state.frozen,
        }
    }
}

impl From<StateV2> for State {
    fn from(state: StateV2) -> Self {
        Self {
            config: ConfigState {
                id: state.config.id,
                tag: state.config.tag,
                path_on_host: state.config.path_on_host,
                capacity: state.config.capacity,
                dax_window_size: state.config.dax_window_size,
                dax_private_max_bytes: state.config.dax_private_max_bytes,
                cladefsd_socket: state.config.cladefsd_socket,
                cache_type: match state.config.cache_type {
                    CacheTypeV2::Unsafe => CacheType::Unsafe,
                    CacheTypeV2::Writeback => CacheType::Writeback,
                },
            },
            window_addr: state.window_addr,
            version: cladefs::CellVersion {
                tenant: state.version.tenant,
                cell: state.version.cell,
                version: state.version.version,
            },
            mappings: state
                .mappings
                .into_iter()
                .map(|m| MappingState {
                    window_offset: m.window_offset,
                    len: m.len,
                    contents: match m.contents {
                        MappingContentsStateV2::Extent { cell, sequence, offset } => {
                            MappingContentsState::Extent { cell, sequence, offset }
                        }
                        MappingContentsStateV2::Zeros => MappingContentsState::Zeros,
                    },
                })
                .collect(),
            frozen: state.frozen,
        }
    }
}

fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new().with_fixint_encoding().reject_trailing_bytes()
}

impl State {
    /// Encodes this device state as `[version: u32 LE][bincode(StateV2)]` at [`FORMAT::current`].
    /// Unbounded on encode so valid large DAX mapping tables never panic (hotbed review 90 finding
    /// 323, hotbed review 89 finding 326).
    pub fn encode(&self) -> Vec<u8> {
        bincode_options().serialize(self).expect("snapshot extent state serializes")
    }

    /// Decodes a version-prefixed device state, bounding the input size and checking [`FORMAT`]
    /// before decoding its payload and refusing trailing bytes (doc-28, board 10058; hotbed review
    /// 89 finding 326).
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(DecodeError::TooLarge { len: bytes.len(), max: MAX_STATE_BYTES });
        }
        let (version, payload) = bytes.split_first_chunk::<4>().ok_or(DecodeError::Truncated)?;
        let version = FORMAT.check(u32::from_le_bytes(*version))?;
        match version {
            2 => {
                let state: StateV2 = bincode_options().deserialize(payload)?;
                Ok(Self::from(state))
            }
            _ => unreachable!("FORMAT.check admitted unknown version {version}"),
        }
    }
}

impl Serialize for State {
    /// Serializes as `(FORMAT.current, StateV2)` so `bitcode` keeps columnar field packing and
    /// never blows Firecracker's 10 MB `Snapshot::load` limit (hotbed review 90 finding 324).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple as _;
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&FORMAT.current)?;
        tuple.serialize_element(&StateV2::from(self))?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for State {
    /// Deserializes `(version, StateV2)`, running [`FORMAT::check`] on `version` before asking the
    /// deserializer to read `StateV2`'s fields.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = State;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a versioned snapshot extent device state (version, state)")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let version: u32 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom(DecodeError::Truncated))?;
                let version = FORMAT.check(version).map_err(serde::de::Error::custom)?;
                match version {
                    2 => {
                        let state: StateV2 = seq
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
                        Ok(State::from(state))
                    }
                    _ => unreachable!("FORMAT.check admitted unknown version {version}"),
                }
            }
        }

        deserializer.deserialize_tuple(2, Visitor)
    }
}

/// What a snapshot of the device holds of its cell (TASK-39.1): the version of its cell it was
/// taken at, which each VM restored from it forks, and the window's mappings then, which the
/// restored VM's window maps again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: cladefs::CellVersion,
    pub mappings: Vec<cladefs::WindowMapping>,
}

impl From<&State> for Snapshot {
    fn from(state: &State) -> Self {
        Self {
            version: state.version,
            mappings: state.mappings.iter().map(cladefs::WindowMapping::from).collect(),
        }
    }
}

/// Errors taking a snapshot of a device (TASK-39.1).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(
        "Extent device {0}: its guest's filesystem is not frozen (fsfreeze it before the \
         snapshot, and thaw it after)"
    )]
    NotFrozen(String),
    #[error("Extent device {0}: cannot make a version of its cell: {1}")]
    Version(String, cladefs::Errno),
    #[error("Extent device {0}: cannot list its DAX window's mappings: {1}")]
    Mappings(String, cladefs::Errno),
    #[error(
        "Extent device {0}: its DAX window holds {1} private pages, whose bytes no extent holds \
         (freeze the guest's filesystem, which rebinds them)"
    )]
    Private(String, u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> State {
        State {
            config: ConfigState {
                id: "extents".into(),
                tag: "fxfs".into(),
                path_on_host: "/store".into(),
                capacity: 1 << 30,
                dax_window_size: Some(64 << 20),
                dax_private_max_bytes: Some(1 << 20),
                cladefsd_socket: "/cladefsd.sock".into(),
                cache_type: CacheType::Unsafe,
            },
            window_addr: Some(1 << 40),
            version: cladefs::CellVersion { tenant: 2, cell: 77, version: 5 },
            mappings: vec![
                MappingState {
                    window_offset: 4096,
                    len: 8192,
                    contents: MappingContentsState::Extent { cell: 3, sequence: 9, offset: 0 },
                },
                MappingState {
                    window_offset: 1 << 21,
                    len: 4096,
                    contents: MappingContentsState::Zeros,
                },
            ],
            frozen: true,
        }
    }

    fn minimal_state() -> State {
        State {
            config: ConfigState {
                id: "root".into(),
                tag: "data".into(),
                path_on_host: "/var/lib/cladefs/cell".into(),
                capacity: 16 << 20,
                dax_window_size: None,
                dax_private_max_bytes: None,
                cladefsd_socket: "/run/cladefsd.sock".into(),
                cache_type: CacheType::Writeback,
            },
            window_addr: None,
            version: cladefs::CellVersion { tenant: 0, cell: 1, version: 0 },
            mappings: Vec::new(),
            frozen: false,
        }
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<SnapshotState>(env!("CARGO_MANIFEST_DIR"));
    }

    struct SnapshotState;

    impl formats::golden::Family for SnapshotState {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            vec![("state", sample_state().encode()), ("state-minimal", minimal_state().encode())]
        }

        fn written(version: u32, _: &str, bytes: &[u8]) -> Result<String, String> {
            if version != 2 {
                return Err(format!("no decoder of version {version}"));
            }
            let (found, payload) = bytes
                .split_first_chunk::<4>()
                .ok_or_else(|| "truncated before version".to_owned())?;
            let found = u32::from_le_bytes(*found);
            if found != 2 {
                return Err(format!("expected version 2, found {found}"));
            }
            let state: StateV2 =
                bincode_options().deserialize(payload).map_err(|err| err.to_string())?;
            Ok(format!("{state:#?}"))
        }

        fn read(_: &str, bytes: &[u8]) -> Result<(), String> {
            State::decode(bytes).map(drop).map_err(|err| err.to_string())
        }

        fn fingerprint(version: u32) -> Option<String> {
            match version {
                2 => Some(format!("{}\n", StateV2::fingerprint())),
                _ => None,
            }
        }
    }

    #[test]
    fn versions_outside_the_range_and_malformed_bytes_are_refused() {
        // Truncated before version.
        for short in [&[][..], &[2], &[2, 0, 0]] {
            assert!(matches!(State::decode(short), Err(DecodeError::Truncated)));
        }
        // Unsupported versions are refused by FORMAT.check before bincode runs, even when the
        // payload after the 4-byte version is arbitrary bytes of a future schema.
        for bad_version in [0u32, 1, 3, u32::MAX] {
            let mut bytes = bad_version.to_le_bytes().to_vec();
            bytes.extend_from_slice(b"future schema bytes");
            let err = State::decode(&bytes).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "snapshot extent state version {bad_version}: this build reads version 2 alone"
                )
            );
        }
        // Trailing bytes after a valid v2 payload are refused.
        let mut trailing = sample_state().encode();
        trailing.push(0);
        assert!(matches!(State::decode(&trailing), Err(DecodeError::Decode(_))));

        // Serde round-trip via bincode directly, and unsupported version refused by serde
        // Deserialize before reading StateV2 fields.
        let encoded = bincode_options().serialize(&sample_state()).unwrap();
        assert_eq!(encoded, sample_state().encode());
        let decoded: State = bincode_options().deserialize(&encoded).unwrap();
        assert_eq!(decoded, sample_state());
        let future_encoded = bincode_options().serialize(&(3u32, "incompatible future")).unwrap();
        let err = bincode_options().deserialize::<State>(&future_encoded).unwrap_err();
        assert!(
            err.to_string()
                .contains("snapshot extent state version 3: this build reads version 2 alone"),
            "{err}"
        );

        // Hotbed review 90 finding 323 & review 89 finding 326: encoding a valid state larger than
        // MAX_STATE_BYTES does not panic, while decode enforces MAX_STATE_BYTES explicitly.
        let mut huge = sample_state();
        huge.mappings = (0..1_550_000u64)
            .map(|i| MappingState {
                window_offset: i * 4096,
                len: 4096,
                contents: MappingContentsState::Extent { cell: 3, sequence: i, offset: 0 },
            })
            .collect();
        let huge_bytes = huge.encode();
        assert!(huge_bytes.len() as u64 > MAX_STATE_BYTES);
        assert!(matches!(
            State::decode(&huge_bytes),
            Err(DecodeError::TooLarge { len, max }) if len == huge_bytes.len() && max == MAX_STATE_BYTES
        ));
    }

    #[test]
    fn config_state_preserves_dax_private_max_bytes_directly() {
        let config = Config {
            id: "extents".into(),
            tag: "fxfs".into(),
            path_on_host: "/store".into(),
            capacity: 1 << 30,
            dax_window_size: Some(64 << 20),
            dax_private_max_bytes: Some(3 * 4096),
            cladefsd_socket: "/cladefsd.sock".into(),
            cache_type: CacheType::Writeback,
        };
        let state = ConfigState::from(&config);
        assert_eq!(state.dax_private_max_bytes, Some(3 * 4096));
        assert_eq!(Config::from(&state), config);
    }
}
