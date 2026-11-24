// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/serialized_types/types.rs`. cladefs deviates from upstream here:
// it never migrates (AGENTS.md), so every format change is breaking, and the only version a mount
// reads is the latest (`EARLIEST_SUPPORTED_VERSION` is `LATEST_VERSION`); the table lists no type a
// version before the latest used. Version 60 locates data in extents of an extent store rather
// than at device offsets (backlog doc-4): the types that hold locations are V60. AllocatorInfoV60
// adds snapshots' owners, the sequence number the allocator mints extents from, and the extents
// dying when the allocator's tree was compacted, which a mount frees again. Version 61 adds
// StoreInfoV61: layers of a base, and the cells of its extents (backlog doc-10). Version 62's
// MutationV62, and JournalRecordV62 that holds it, record what replay needs to find the metadata
// space borrowed as the filesystem had it (`Mutation::UpdateBorrowed`, TASK-218). Version 63's
// ObjectDescriptorV63 says which special file a file is, in the entries of its directories
// (TASK-236): ObjectValueV63 holds it, and the item, mutation, journal, encrypted transaction and
// super-block records that hold an ObjectValue are V63. Version 64 replaces the journal file and
// A/B super-block objects with position-derived journal spans and a single atomic super-block in
// Root (boards 6923, 6926): SuperBlockHeaderV64 drops journal_object_id, SuperBlockRecordV64 drops
// Extent, and JournalRecordV64 adds JournalSpan. Version 65 drops Fxfs's own encryption, fscrypt,
// fsverity and blobs (cladefs encrypts below Fxfs): no keys, encrypted children, encrypted symlinks,
// verified attributes or encrypted mutations; an extent names no key (ExtentValueV65); a store's
// information no mutations key, and its object IDs are sequential alone, its last a plain u64
// (StoreInfoV65). Version 66 drops casefold directories (board 10598), overwrite/preallocation
// extents (board 10600), project quotas (board 10602), and volume byte limits (board 10604):
// ObjectKey, ObjectValue, Mutation, JournalRecord and SuperBlockRecord are V66.

use crate::lsm_tree::{
    PersistentLayerHeader, PersistentLayerHeaderV39, PersistentLayerInfo, PersistentLayerInfoV39,
};
use crate::object_store::allocator::{
    AllocatorInfo, AllocatorInfoV66, AllocatorKey, AllocatorKeyV60, AllocatorValue,
    AllocatorValueV32,
};
use crate::object_store::journal::super_block::{
    SuperBlockHeader, SuperBlockHeaderV64, SuperBlockRecord, SuperBlockRecordV66,
};
use crate::object_store::journal::{JournalRecord, JournalRecordV66};
use crate::object_store::object_record::{ObjectKey, ObjectKeyV66, ObjectValue, ObjectValueV66};
use crate::object_store::transaction::{Mutation, MutationV66};
use crate::object_store::{StoreInfo, StoreInfoV65};
use crate::serialized_types::{Version, Versioned, VersionedLatest, versioned_type};
use std::collections::BTreeMap;

/// The latest version of on-disk filesystem format.
///
/// If all layer files are compacted the the journal flushed, and super-block
/// both rewritten, all versions should match this value.
///
/// If making a breaking change, please see EARLIEST_SUPPORTED_VERSION (below).
///
/// IMPORTANT: When changing this (major or minor), update the list of possible versions at
/// https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/cobalt_config/fuchsia/local_storage/versions.txt.
pub const LATEST_VERSION: Version = Version { major: 66, minor: 0 };

/// The last version of the filesystem where keys in layer files used the old serialization format
/// (prior to SerializeKey and delta encoding).
pub const OLD_KEY_SERIALIZATION_VERSION: Version = Version { major: 57, minor: 0 };

/// The earliest supported version of the on-disk filesystem format.
///
/// When a breaking change is made:
/// 1) LATEST_VERSION should have it's major component increased (see above).
/// 2) EARLIEST_SUPPORTED_VERSION should be set to the new LATEST_VERSION.
/// 3) The SuperBlockHeader version (below) should also be set to the new LATEST_VERSION.
///
/// Also check the constant version numbers above for any code cleanup that can happen.
///
/// cladefs: the latest, whatever it is: every change is breaking, as no format is migrated.
pub const EARLIEST_SUPPORTED_VERSION: Version = LATEST_VERSION;

/// This trait prevents types from showing up in `versioned_types` multiple times. `versioned_types`
/// implements this trait for every type passed to it. If a type is listed multiple times then this
/// trait will be implemented for the type multiple times which will fail to compile.
#[allow(dead_code)]
trait UniqueVersionForType {}

macro_rules! versioned_types {
    ( $( $name:ident { $latest:literal.. => $latest_type:ty $(, $major:literal.. => $type:ty )* $(,)? } )+ ) => {
        $(
            static_assertions::assert_type_eq_all!($name, $latest_type);

            versioned_type! {
                $latest.. => $latest_type,
                $( $major.. => $type ),*
            }

            impl UniqueVersionForType for $latest_type {}
            $( impl UniqueVersionForType for $type {} )*

        )+

        pub fn get_type_fingerprints(version: Version) -> BTreeMap<String, String> {
            let mut map = BTreeMap::new();
            $(
                let fingerprint = {
                    let mut fp = None;
                    const FINGERPRINTS: &[(u32, fn() -> String)] = &[
                        ($latest, <$latest_type as fprint::TypeFingerprint>::fingerprint),
                        $( ($major, <$type as fprint::TypeFingerprint>::fingerprint) ),*
                    ];
                    for (major, type_fp) in FINGERPRINTS {
                        if version.major >= *major {
                            fp = Some(type_fp());
                            break;
                        }
                    }
                    fp
                };
                if let Some(fp) = fingerprint {
                    map.insert(stringify!($name).to_string(), fp.to_string());
                }
            )+
            map
        }
    };
}

versioned_types! {
    AllocatorInfo {
        66.. => AllocatorInfoV66,
    }
    AllocatorKey {
        60.. => AllocatorKeyV60,
    }
    AllocatorValue {
        32.. => AllocatorValueV32,
    }
    JournalRecord {
        66.. => JournalRecordV66,
    }
    Mutation {
        66.. => MutationV66,
    }
    ObjectKey {
        66.. => ObjectKeyV66,
    }
    ObjectValue {
        66.. => ObjectValueV66,
    }
    PersistentLayerHeader {
        39.. => PersistentLayerHeaderV39,
    }
    PersistentLayerInfo {
        39.. => PersistentLayerInfoV39,
    }
    StoreInfo {
        65.. => StoreInfoV65,
    }
    SuperBlockHeader {
        64.. => SuperBlockHeaderV64,
    }
    SuperBlockRecord {
        66.. => SuperBlockRecordV66,
    }
}
