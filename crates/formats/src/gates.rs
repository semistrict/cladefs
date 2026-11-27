// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Storage-version upgrade ratchets stored in each bucket (`formats`, doc-28).
//!
//! Separate from per-format golden-corpus pinning (`formats/<family>/`), the upgrade ratchet
//! collapses cluster-wide write gating to **two linear monotonic numbers** stored at bucket
//! format time:
//!
//! - **`storage`** (`1..=CURRENT_STORAGE`): maps via [`PROFILES`] to the exact per-format write
//!   versions for the host release train (`cladefsd`, `cladefs-gc`, `cladefsctl`, VMM).
//! - **`fxfs`** (`OLDEST_FXFS..=CURRENT_FXFS`): the guest Linux kernel's Fxfs on-disk format
//!   version, kept separate so a slow guest-kernel rollout never blocks ratcheting a host-only
//!   storage change.
//!
//! Wire format ([`FORMAT`], frozen at v1):
//! ```text
//! cladefs format 1
//! storage 1
//! fxfs 66
//! ```
//!
//! Purpose: during partial/rolling upgrades and rollbacks, nodes running the new build read all
//! old versions (`oldest..=current`) while continuing to write the bucket's gated profile until an
//! operator explicitly ratchets forward once all nodes run the new build.

use crate::{Format, Refused};
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// Frozen format family of the bucket's `formats` ratchet object.
pub const FORMAT: Format = Format::new("format gates", 1, 1);

/// First line prefix of the `formats` ratchet object.
const HEADER: &str = "cladefs format ";

/// Maximum encoded size accepted from untrusted bucket bytes (4 KiB).
pub const MAX_BYTES: usize = 4 << 10;

/// Oldest host `storage` profile version supported by this build.
pub const OLDEST_STORAGE: u32 = 1;

/// Newest host `storage` profile version supported by this build.
/// Bump by 1 and append a new [`StorageProfile`] to [`PROFILES`] whenever any host-side storage
/// format version increases.
pub const CURRENT_STORAGE: u32 = 1;

/// Oldest guest `fxfs` version supported by this build (`fxfs::EARLIEST_SUPPORTED_VERSION`).
pub const OLDEST_FXFS: u32 = 66;

/// Newest guest `fxfs` version supported by this build (`fxfs::LATEST_VERSION`).
pub const CURRENT_FXFS: u32 = 66;

/// Per-format write versions selected by a single linear `storage` profile number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageProfile {
    pub storage: u32,
    pub block_device: u32,
    pub bucket_events: u32,
    pub bucket_layout: u32,
    pub cell_directory: u32,
    pub cell_records: u32,
    pub ephemeral_host: u32,
    pub extent_frame: u32,
    pub fast_wal: u32,
    pub replica_directory: u32,
    pub snapshot_extent_state: u32,
}

/// Linear host storage-version profiles (`storage = 1` is today's baseline).
/// Only append a new profile (`storage = 2`, ...) when a future host storage format bump ships.
pub const PROFILES: &[StorageProfile] = &[StorageProfile {
    storage: 1,
    block_device: 2,
    bucket_events: 1,
    bucket_layout: 35,
    cell_directory: 19,
    cell_records: 1,
    ephemeral_host: 2,
    extent_frame: 3,
    fast_wal: 1,
    replica_directory: 4,
    snapshot_extent_state: 3,
}];

const _: () = {
    assert!(!PROFILES.is_empty());
    assert!(OLDEST_STORAGE <= CURRENT_STORAGE);
    assert!(PROFILES.len() == (CURRENT_STORAGE - OLDEST_STORAGE + 1) as usize);
    let mut i = 0;
    while i < PROFILES.len() {
        assert!(PROFILES[i].storage == OLDEST_STORAGE + i as u32);
        if i > 0 {
            let prev = &PROFILES[i - 1];
            let cur = &PROFILES[i];
            assert!(cur.block_device >= prev.block_device);
            assert!(cur.bucket_events >= prev.bucket_events);
            assert!(cur.bucket_layout >= prev.bucket_layout);
            assert!(cur.cell_directory >= prev.cell_directory);
            assert!(cur.cell_records >= prev.cell_records);
            assert!(cur.ephemeral_host >= prev.ephemeral_host);
            assert!(cur.extent_frame >= prev.extent_frame);
            assert!(cur.fast_wal >= prev.fast_wal);
            assert!(cur.replica_directory >= prev.replica_directory);
            assert!(cur.snapshot_extent_state >= prev.snapshot_extent_state);
        }
        i += 1;
    }
    assert!(OLDEST_FXFS <= CURRENT_FXFS);
};

/// Returns the [`StorageProfile`] for `storage`, if supported by this build.
pub const fn profile_for(storage: u32) -> Option<&'static StorageProfile> {
    if storage < OLDEST_STORAGE || storage > CURRENT_STORAGE {
        return None;
    }
    Some(&PROFILES[(storage - OLDEST_STORAGE) as usize])
}

/// Errors decoding, validating, or advancing a bucket's format gates.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("format gates: {len} bytes exceeds limit of {MAX_BYTES}")]
    TooLarge { len: usize },
    #[error("format gates: {0}")]
    Corrupt(String),
    #[error("format gates: {0}")]
    UnsupportedHeader(#[from] Refused),
    #[error(
        "{knob} is gated at version {gated}, which this build cannot use ({supported}); \
         upgrade this host before joining a ratcheted bucket"
    )]
    IncompatibleGate { knob: &'static str, gated: u32, supported: String },
    #[error(
        "cannot ratchet {knob} backward from version {current} to {requested}: \
         format gates are monotonic"
    )]
    CannotDowngrade { knob: &'static str, current: u32, requested: u32 },
    #[error(
        "cannot ratchet {knob} to version {requested}: \
         this build only supports up to version {max}"
    )]
    BeyondBuild { knob: &'static str, requested: u32, max: u32 },
}

/// Serializable ratchet state stored in a bucket's `formats` object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gates {
    /// Host storage-profile version (`OLDEST_STORAGE..=CURRENT_STORAGE`).
    pub storage: u32,
    /// Guest Fxfs format version (`OLDEST_FXFS..=CURRENT_FXFS`).
    pub fxfs: u32,
}

impl Gates {
    /// Initial ratchet for a freshly formatted bucket: this build's latest supported versions.
    pub const fn current() -> Self {
        Self { storage: CURRENT_STORAGE, fxfs: CURRENT_FXFS }
    }

    /// Ratchet pinning both knobs to this build's oldest supported versions.
    pub const fn oldest() -> Self {
        Self { storage: OLDEST_STORAGE, fxfs: OLDEST_FXFS }
    }

    /// Componentwise monotonic join of two valid ratchets (never regresses either knob).
    pub const fn merge_monotonic(self, other: Self) -> Self {
        Self {
            storage: if self.storage >= other.storage { self.storage } else { other.storage },
            fxfs: if self.fxfs >= other.fxfs { self.fxfs } else { other.fxfs },
        }
    }

    /// Validates that both `storage` and `fxfs` fall within this build's supported ranges.
    pub fn validate(&self) -> Result<(), GateError> {
        let _ = self.profile()?;
        if !(OLDEST_FXFS..=CURRENT_FXFS).contains(&self.fxfs) {
            return Err(incompatible("fxfs", self.fxfs, OLDEST_FXFS, CURRENT_FXFS));
        }
        Ok(())
    }

    /// Returns the [`StorageProfile`] selected by `self.storage`, or an error if unsupported.
    pub fn profile(&self) -> Result<&'static StorageProfile, GateError> {
        profile_for(self.storage)
            .ok_or_else(|| incompatible("storage", self.storage, OLDEST_STORAGE, CURRENT_STORAGE))
    }

    /// Monotonically advances `storage` and/or `fxfs`. Returns `(next, changed)`.
    /// Refuses downgrades and versions past this build's maximum.
    pub fn ratchet(
        &self,
        storage: Option<u32>,
        fxfs: Option<u32>,
    ) -> Result<(Self, bool), GateError> {
        self.validate()?;
        let mut next = *self;
        if let Some(target) = storage {
            if target > CURRENT_STORAGE {
                return Err(GateError::BeyondBuild {
                    knob: "storage",
                    requested: target,
                    max: CURRENT_STORAGE,
                });
            }
            if target < next.storage {
                return Err(GateError::CannotDowngrade {
                    knob: "storage",
                    current: next.storage,
                    requested: target,
                });
            }
            next.storage = target;
        }
        if let Some(target) = fxfs {
            if target > CURRENT_FXFS {
                return Err(GateError::BeyondBuild {
                    knob: "fxfs",
                    requested: target,
                    max: CURRENT_FXFS,
                });
            }
            if target < next.fxfs {
                return Err(GateError::CannotDowngrade {
                    knob: "fxfs",
                    current: next.fxfs,
                    requested: target,
                });
            }
            next.fxfs = target;
        }
        let changed = next != *self;
        Ok((next, changed))
    }

    /// Advances both `storage` and `fxfs` to this build's current versions.
    pub fn ratchet_all(&self) -> Result<(Self, bool), GateError> {
        self.ratchet(Some(CURRENT_STORAGE), Some(CURRENT_FXFS))
    }

    /// Encodes `self` into the canonical frozen `format-gates` v1 text format.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::new();
        let _ = writeln!(out, "{HEADER}{}", FORMAT.current);
        let _ = writeln!(out, "storage {}", self.storage);
        let _ = writeln!(out, "fxfs {}", self.fxfs);
        out.into_bytes()
    }

    /// Decodes untrusted bytes from a bucket's `formats` object.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(bytes: &[u8]) -> Result<Self, GateError> {
        if bytes.len() > MAX_BYTES {
            return Err(GateError::TooLarge { len: bytes.len() });
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|err| GateError::Corrupt(format!("not utf-8: {err}")))?;
        let mut lines = text.lines();
        let first = lines.next().ok_or_else(|| GateError::Corrupt("missing header".to_owned()))?;
        let raw_version = first
            .strip_prefix(HEADER)
            .ok_or_else(|| GateError::Corrupt(format!("bad header {first:?}")))?;
        let header_version = parse_u32(raw_version)
            .ok_or_else(|| GateError::Corrupt(format!("bad header version {raw_version:?}")))?;
        FORMAT.check(header_version)?;

        let mut storage: Option<u32> = None;
        let mut fxfs: Option<u32> = None;
        for line in lines {
            if line.is_empty() {
                return Err(GateError::Corrupt("blank line".to_owned()));
            }
            let (key, raw_val) = line
                .split_once(' ')
                .ok_or_else(|| GateError::Corrupt(format!("bad entry {line:?}")))?;
            let value = parse_u32(raw_val)
                .filter(|&v| v > 0)
                .ok_or_else(|| GateError::Corrupt(format!("bad version for {key}: {raw_val:?}")))?;
            match key {
                "storage" => {
                    if storage.replace(value).is_some() {
                        return Err(GateError::Corrupt("duplicate storage entry".to_owned()));
                    }
                }
                "fxfs" => {
                    if fxfs.replace(value).is_some() {
                        return Err(GateError::Corrupt("duplicate fxfs entry".to_owned()));
                    }
                }
                other => {
                    return Err(GateError::Corrupt(format!("unknown key {other:?}")));
                }
            }
        }
        let storage = storage.ok_or_else(|| GateError::Corrupt("missing storage".to_owned()))?;
        let fxfs = fxfs.ok_or_else(|| GateError::Corrupt("missing fxfs".to_owned()))?;
        Ok(Self { storage, fxfs })
    }
}

/// Lock-free per-host runtime ratchet holding the active `storage` and `fxfs` versions.
///
/// Owned explicitly by `bucket_cell::Host` (and shared by `local_cell::Host`, `cladefs_core::Host`,
/// and `ObjectBucket`) so simulated hosts under `madsim` have independent ratchets with zero
/// cross-node static leakage.
#[derive(Debug)]
pub struct Ratchet {
    storage: AtomicU32,
    fxfs: AtomicU32,
}

impl Ratchet {
    /// Initializes both knobs to this build's current versions (default for fresh buckets and
    /// standalone unit tests).
    pub const fn current() -> Self {
        Self { storage: AtomicU32::new(CURRENT_STORAGE), fxfs: AtomicU32::new(CURRENT_FXFS) }
    }

    /// Initializes both knobs to this build's oldest supported versions.
    pub const fn oldest() -> Self {
        Self { storage: AtomicU32::new(OLDEST_STORAGE), fxfs: AtomicU32::new(OLDEST_FXFS) }
    }

    /// Builds a runtime ratchet from decoded [`Gates`], failing if outside this build's range.
    pub fn from_gates(gates: Gates) -> Result<Self, GateError> {
        gates.validate()?;
        Ok(Self { storage: AtomicU32::new(gates.storage), fxfs: AtomicU32::new(gates.fxfs) })
    }

    /// Shared read-only ratchet pinned at current versions for default `Bucket::ratchet()` impls
    /// (immutable: never mutated at runtime, safe across all hosts and `madsim`).
    pub fn default_current() -> &'static Self {
        static DEFAULT: OnceLock<Ratchet> = OnceLock::new();
        DEFAULT.get_or_init(Self::current)
    }

    /// Active gated host `storage` version (one atomic load).
    #[inline]
    pub fn storage(&self) -> u32 {
        self.storage.load(Ordering::Acquire)
    }

    /// Active gated guest `fxfs` version (one atomic load).
    #[inline]
    pub fn fxfs(&self) -> u32 {
        self.fxfs.load(Ordering::Acquire)
    }

    /// Active [`StorageProfile`] for this host's current `storage` gate.
    #[inline]
    pub fn profile(&self) -> &'static StorageProfile {
        profile_for(self.storage()).expect("ratchet storage version is validated on load/advance")
    }

    /// Validates `gates` and installs them on boot (replacing initial defaults, allowing lower
    /// gated versions when joining an older bucket).
    pub fn load(&self, gates: Gates) -> Result<(), GateError> {
        gates.validate()?;
        self.storage.store(gates.storage, Ordering::Release);
        self.fxfs.store(gates.fxfs, Ordering::Release);
        Ok(())
    }

    /// Validates `gates` and monotonically advances either knob if it increased. Never lowers an
    /// active version; returns `Ok(true)` if either knob advanced.
    pub fn advance(&self, gates: Gates) -> Result<bool, GateError> {
        gates.validate()?;
        let prev_storage = self.storage.fetch_max(gates.storage, Ordering::AcqRel);
        let prev_fxfs = self.fxfs.fetch_max(gates.fxfs, Ordering::AcqRel);
        Ok(gates.storage > prev_storage || gates.fxfs > prev_fxfs)
    }

    /// Snapshot of the active gated versions as [`Gates`].
    pub fn snapshot(&self) -> Gates {
        Gates { storage: self.storage(), fxfs: self.fxfs() }
    }
}

fn incompatible(knob: &'static str, gated: u32, oldest: u32, current: u32) -> GateError {
    let supported = if oldest == current {
        format!("version {current} alone")
    } else {
        format!("versions {oldest} to {current}")
    };
    GateError::IncompatibleGate { knob, gated, supported }
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_u32(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FormatGatesCorpus;

    impl crate::golden::Family for FormatGatesCorpus {
        const FORMAT: Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            vec![("gates", Gates { storage: 1, fxfs: 66 }.encode())]
        }

        fn written(version: u32, _case: &str, bytes: &[u8]) -> Result<String, String> {
            let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            let first = text.lines().next().unwrap_or_default();
            let expected = format!("{HEADER}{version}");
            if first != expected {
                return Err(format!("expected header {expected:?}, found {first:?}"));
            }
            let decoded = Gates::decode(bytes).map_err(|e| e.to_string())?;
            Ok(format!("{decoded:#?}"))
        }

        fn read(_case: &str, bytes: &[u8]) -> Result<(), String> {
            Gates::decode(bytes).map(drop).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn every_frozen_version_is_read() {
        crate::golden::check::<FormatGatesCorpus>(env!("CARGO_MANIFEST_DIR"));
    }

    #[test]
    fn gates_validate_ratchet_and_refuse_downgrades_or_future_versions() {
        let current = Gates::current();
        current.validate().unwrap();
        assert_eq!(current.profile().unwrap().storage, CURRENT_STORAGE);
        assert_eq!(Gates::decode(&current.encode()).unwrap(), current);

        // Idempotent ratchet reports unchanged.
        let (same, changed) = current.ratchet(Some(CURRENT_STORAGE), Some(CURRENT_FXFS)).unwrap();
        assert!(!changed);
        assert_eq!(same, current);

        // Beyond build's maximum is refused for both knobs.
        assert!(matches!(
            current.ratchet(Some(CURRENT_STORAGE + 1), None),
            Err(GateError::BeyondBuild { knob: "storage", .. })
        ));
        assert!(matches!(
            current.ratchet(None, Some(CURRENT_FXFS + 1)),
            Err(GateError::BeyondBuild { knob: "fxfs", .. })
        ));

        // Downgrade below current gate is refused.
        assert!(matches!(
            current.ratchet(Some(0), None),
            Err(GateError::CannotDowngrade { knob: "storage", .. })
        ));
        assert!(matches!(
            current.ratchet(None, Some(OLDEST_FXFS - 1)),
            Err(GateError::CannotDowngrade { knob: "fxfs", .. })
        ));

        // Future bucket ratcheted past this build is refused by validate() and Ratchet::load().
        let future_storage = Gates { storage: CURRENT_STORAGE + 1, fxfs: CURRENT_FXFS };
        assert!(matches!(
            future_storage.validate(),
            Err(GateError::IncompatibleGate { knob: "storage", .. })
        ));
        let future_fxfs = Gates { storage: CURRENT_STORAGE, fxfs: CURRENT_FXFS + 1 };
        assert!(matches!(
            future_fxfs.validate(),
            Err(GateError::IncompatibleGate { knob: "fxfs", .. })
        ));
        let ratchet = Ratchet::current();
        assert!(ratchet.load(future_storage).is_err());
        assert!(ratchet.advance(future_fxfs).is_err());
        assert_eq!(ratchet.snapshot(), current);
    }

    #[test]
    fn malformed_and_hostile_gates_are_refused() {
        for bad in [
            &[][..],
            b"wrong header\nstorage 1\nfxfs 66\n",
            b"cladefs format 2\nstorage 1\nfxfs 66\n",
            b"cladefs format 0\nstorage 1\nfxfs 66\n",
            b"cladefs format 1\nstorage 0\nfxfs 66\n",
            b"cladefs format 1\nstorage 1\n",
            b"cladefs format 1\nfxfs 66\n",
            b"cladefs format 1\nstorage 1\nstorage 1\nfxfs 66\n",
            b"cladefs format 1\nstorage 1\nfxfs 66\nfxfs 66\n",
            b"cladefs format 1\nstorage 1\nfxfs 66\nextra 1\n",
            b"cladefs format 1\nstorage +1\nfxfs 66\n",
            b"cladefs format 1\n\nstorage 1\nfxfs 66\n",
        ] {
            assert!(
                Gates::decode(bad).is_err(),
                "should refuse {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let oversized = vec![b'a'; MAX_BYTES + 1];
        assert!(matches!(Gates::decode(&oversized), Err(GateError::TooLarge { .. })));
    }
}
