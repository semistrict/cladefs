// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/extent_record.rs`. cladefs: an extent is located by
// an extent in the extent store and an offset within it, not a device offset (backlog doc-4),
// names no key, and has no overwrite modes (`ExtentModeV66`, `ExtentValueV66`, board 10600).

// TODO(https://fxbug.dev/42178223): need validation after deserialization. cladefs: what a layer or
// the journal holds is validated as it's read (`ExtentValue::fits`, `ObjectValue::is_valid_for`).

use crate::checksum::{Checksums, ChecksumsV38};
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use storage_device::ExtentId;
use storage_units::BlockSize;

/// The mode the extent is operating in. This changes how writes work to this region of the file.
pub type ExtentMode = ExtentModeV66;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TypeFingerprint)]
pub enum ExtentModeV66 {
    /// This extent doesn't have defined write semantics. The writer chooses how to handle the data
    /// here. Notable uses of this are things which have their own separate checksum mechanism,
    /// like the journal file and blobs.
    Raw,
    /// This extent uses copy-on-write semantics. We store the checksums for data validation. New writes to this logical range are written to new extents.
    Cow(ChecksumsV38),
}

#[cfg(fuzz)]
impl<'a> arbitrary::Arbitrary<'a> for ExtentMode {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=1)? {
            0 => ExtentMode::Raw,
            1 => ExtentMode::Cow(u.arbitrary()?),
            _ => unreachable!(),
        })
    }
}

/// ExtentValue is the payload for an extent in the object store, which describes where the extent
/// is physically located.
pub type ExtentValue = ExtentValueV66;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ExtentValueV66 {
    /// Indicates a deleted extent; that is, the logical range described by the extent key is
    /// considered to be deleted.
    None,
    /// The location of the extent, `extent_offset` bytes into `extent` in the extent store.
    /// `mode` describes the write pattern for this extent.
    Some { extent: ExtentId, extent_offset: u64, mode: ExtentModeV66 },
}

impl ExtentValue {
    pub fn new(extent: ExtentId, extent_offset: u64, mode: ExtentMode) -> ExtentValue {
        ExtentValue::Some { extent, extent_offset, mode }
    }

    pub fn new_raw(extent: ExtentId, extent_offset: u64) -> ExtentValue {
        Self::new(extent, extent_offset, ExtentMode::Raw)
    }

    /// Creates an ExtentValue with a checksum
    pub fn with_checksum(
        extent: ExtentId,
        extent_offset: u64,
        checksums: Checksums,
    ) -> ExtentValue {
        Self::new(extent, extent_offset, ExtentMode::Cow(checksums))
    }

    /// Creates an ObjectValue for a deletion of an object extent.
    pub fn deleted_extent() -> ExtentValue {
        ExtentValue::None
    }

    pub fn is_deleted(&self) -> bool {
        if let ExtentValue::None = self { true } else { false }
    }

    /// cladefs: whether what this says fits an extent `len` bytes long, of `block_size` blocks:
    /// checksums each of a whole number of blocks, and an offset on blocks that
    /// doesn't overflow, as an object store's records must (`ObjectValue::is_valid_for`).
    pub fn fits(&self, len: u64, block_size: BlockSize) -> bool {
        let ExtentValue::Some { extent_offset, mode, .. } = self else {
            return true;
        };
        let entries_fit = match mode {
            ExtentMode::Cow(checksums) if checksums.len() > 0 => {
                let sums = checksums.len() as u64;
                len % sums == 0 && block_size.is_aligned(len / sums)
            }
            _ => true,
        };
        entries_fit
            && block_size.is_aligned(*extent_offset)
            && extent_offset.checked_add(len).is_some()
    }

    /// Returns a new ExtentValue offset by `amount`.  Both `amount` and `extent_len` must be
    /// multiples of the underlying block size.
    pub fn offset_by(&self, amount: u64, extent_len: u64) -> Self {
        match self {
            ExtentValue::None => Self::deleted_extent(),
            ExtentValue::Some { extent, extent_offset, mode } => {
                let mode = match mode {
                    ExtentMode::Raw => ExtentMode::Raw,
                    ExtentMode::Cow(checksums) => {
                        if checksums.len() > 0 {
                            let index = (amount / (extent_len / checksums.len() as u64)) as usize;
                            ExtentMode::Cow(checksums.offset_by(index))
                        } else {
                            ExtentMode::Cow(Checksums::fletcher(Vec::new()))
                        }
                    }
                };
                ExtentValue::Some { extent: *extent, extent_offset: extent_offset + amount, mode }
            }
        }
    }

    /// Returns a new ExtentValue after shrinking the extent from |original_len| to |new_len|.
    pub fn shrunk(&self, original_len: u64, new_len: u64) -> Self {
        match self {
            ExtentValue::None => Self::deleted_extent(),
            ExtentValue::Some { extent, extent_offset, mode } => {
                let mode = match mode {
                    ExtentMode::Raw => ExtentMode::Raw,
                    ExtentMode::Cow(checksums) => {
                        if checksums.len() > 0 {
                            let len = (new_len / (original_len / checksums.len() as u64)) as usize;
                            ExtentMode::Cow(checksums.shrunk(len))
                        } else {
                            ExtentMode::Cow(Checksums::fletcher(Vec::new()))
                        }
                    }
                };
                ExtentValue::Some { extent: *extent, extent_offset: *extent_offset, mode }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExtentValue;
    use crate::checksum::Checksums;
    use storage_device::ExtentId;

    /// The extent the tests' extents are in.
    const E: ExtentId = ExtentId::new(0, 0);

    #[test]
    fn extent_value_offset_by() {
        assert_eq!(ExtentValue::None.offset_by(1024, 2048), ExtentValue::None);
        assert_eq!(ExtentValue::new_raw(E, 1024).offset_by(0, 2048), ExtentValue::new_raw(E, 1024));
        assert_eq!(
            ExtentValue::new_raw(E, 1024).offset_by(1024, 2048),
            ExtentValue::new_raw(E, 2048)
        );
        assert_eq!(
            ExtentValue::new_raw(E, 1024).offset_by(2048, 2048),
            ExtentValue::new_raw(E, 3072)
        );

        let make_checksums = |range: std::ops::Range<u64>| Checksums::fletcher(range.collect());

        // In these tests we are making block size 256.
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).offset_by(0, 2048),
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8))
        );
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).offset_by(1024, 2048),
            ExtentValue::with_checksum(E, 2048, make_checksums(4..8))
        );
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).offset_by(2048, 2048),
            ExtentValue::with_checksum(E, 3072, Checksums::fletcher(Vec::new()))
        );
    }

    #[test]
    fn extent_value_shrunk() {
        assert_eq!(ExtentValue::None.shrunk(2048, 1024), ExtentValue::None);
        assert_eq!(ExtentValue::new_raw(E, 1024).shrunk(2048, 2048), ExtentValue::new_raw(E, 1024));
        assert_eq!(ExtentValue::new_raw(E, 1024).shrunk(2048, 1024), ExtentValue::new_raw(E, 1024));
        assert_eq!(ExtentValue::new_raw(E, 1024).shrunk(2048, 0), ExtentValue::new_raw(E, 1024));

        let make_checksums = |range: std::ops::Range<u64>| Checksums::fletcher(range.collect());

        // In these tests we are making block size 256.
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).shrunk(2048, 2048),
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8))
        );
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).shrunk(2048, 1024),
            ExtentValue::with_checksum(E, 1024, make_checksums(0..4))
        );
        assert_eq!(
            ExtentValue::with_checksum(E, 1024, make_checksums(0..8)).shrunk(2048, 0),
            ExtentValue::with_checksum(E, 1024, Checksums::fletcher(Vec::new()))
        );
    }
}
