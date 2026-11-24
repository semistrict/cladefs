// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/object_record.rs`. cladefs: an extent's value locates
// its data in an extent of the extent store (backlog doc-4), so ObjectValue holds an ExtentValue,
// which nothing older converts to. ObjectDescriptorV63 says which special file a file is, so that
// a directory's entries say what each node is (`ObjectDescriptor::Special`), and ChildValueV63
// holds it. Version 65 drops what Fxfs's own encryption, fscrypt and fsverity kept (cladefs
// encrypts below Fxfs, in its extents and buckets): keys, encrypted children and symlinks, and
// verified attributes; ObjectKeyDataV65, ObjectKeyV65, ObjectKindV65, ObjectValueV65 and
// ObjectItemV65 are what is left.

mod legacy;

// TODO(https://fxbug.dev/42178223): need validation after deserialization. cladefs: what a layer or
// the journal holds is validated as it's read (`ObjectValue::is_valid_for`).
use crate::checksum::Checksums;
use crate::log::error;
use crate::lsm_tree::types::{
    FuzzyHash, Item, ItemRef, LayerKey, MergeType, OrdLowerBound, OrdUpperBound, SortByU64, Value,
};
use crate::object_store::ProjectId;
use crate::object_store::extent::{Extent, ExtentPartitionIterator};
use crate::object_store::extent_record::{ExtentValue, ExtentValueV65};
use crate::serialized_types::Versioned;
use fprint::TypeFingerprint;
use fxfs_macros::SerializeKey;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::default::Default;
use std::hash::Hash;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use storage_device::ExtentId;
use storage_units::BlockSize;

/// ObjectDescriptor is the set of possible records in the object store.
pub type ObjectDescriptor = ObjectDescriptorV63;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectDescriptorV63 {
    /// A file (in the generic sense; i.e. an object with some attributes).
    File,
    /// A directory (in the generic sense; i.e. an object with children).
    Directory,
    /// A volume, which is the root of a distinct object store containing Files and Directories.
    Volume,
    /// A symbolic link.
    Symlink,
    /// cladefs: a special file: a file object, whose POSIX mode says it is this one
    /// (`SpecialFile::of_mode`), and fsck checks that it does. Its entries say so, that a listing
    /// of a directory need not look up each file's record to say what it is (TASK-236).
    Special(SpecialFileV63),
}

impl ObjectDescriptor {
    /// cladefs: whether the entry is a file's, which is a file object: a regular file's, or a
    /// special file's.
    pub fn is_file(&self) -> bool {
        matches!(self, ObjectDescriptor::File | ObjectDescriptor::Special(_))
    }

    /// cladefs: the descriptor of a file object whose POSIX mode is `mode`, if it has one: a special
    /// file's, or a regular file's.
    pub fn of_file(mode: Option<u32>) -> ObjectDescriptor {
        match mode.and_then(SpecialFile::of_mode) {
            Some(special) => ObjectDescriptor::Special(special),
            None => ObjectDescriptor::File,
        }
    }
}

/// cladefs: the kinds of special file, as POSIX has them.
pub type SpecialFile = SpecialFileV63;

#[derive(Copy, Clone, Debug, Eq, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum SpecialFileV63 {
    /// A FIFO (`S_IFIFO`).
    Fifo,
    /// A character device (`S_IFCHR`).
    CharDevice,
    /// A block device (`S_IFBLK`).
    BlockDevice,
    /// A socket (`S_IFSOCK`).
    Socket,
}

impl SpecialFile {
    /// The `S_IFMT` bits of a mode.
    pub const MODE_TYPE: u32 = 0o170000;

    /// The special file a POSIX mode's `S_IFMT` bits say, if they say one.
    pub fn of_mode(mode: u32) -> Option<SpecialFile> {
        match mode & Self::MODE_TYPE {
            0o010000 => Some(SpecialFile::Fifo),
            0o020000 => Some(SpecialFile::CharDevice),
            0o060000 => Some(SpecialFile::BlockDevice),
            0o140000 => Some(SpecialFile::Socket),
            _ => None,
        }
    }

    /// Its `S_IFMT` bits.
    pub fn mode(self) -> u32 {
        match self {
            SpecialFile::Fifo => 0o010000,
            SpecialFile::CharDevice => 0o020000,
            SpecialFile::BlockDevice => 0o060000,
            SpecialFile::Socket => 0o140000,
        }
    }
}

/// For specifying what property of the project is being addressed.
pub type ProjectProperty = ProjectPropertyV32;

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    TypeFingerprint,
    SerializeKey,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ProjectPropertyV32 {
    /// The configured limit for the project.
    Limit,
    /// The currently tracked usage for the project.
    Usage,
}

pub type ObjectKeyData = ObjectKeyDataV66;

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    TypeFingerprint,
    SerializeKey,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectKeyDataV66 {
    /// A generic, untyped object.  This must come first and sort before all other keys for a given
    /// object because it's also used as a tombstone and it needs to merge with all following keys.
    Object,
    /// An attribute associated with an object.  It has a 64-bit ID.
    Attribute(AttributeId, AttributeKeyV32),
    /// A child of a directory.
    Child { name: String },
    /// A graveyard entry for an entire object.
    GraveyardEntry { object_id: u64 },
    /// Project ID info. This should only be attached to the volume's root node. Used to address the
    /// configured limit and the usage tracking which are ordered after the `project_id` to provide
    /// locality of the two related values.
    Project { project_id: ProjectId, property: ProjectPropertyV32 },
    /// An extended attribute associated with an object. It stores the name used for the extended
    /// attribute, which has a maximum size of 255 bytes enforced by fuchsia.io.
    ExtendedAttribute {
        #[serde(with = "crate::zerocopy_serialization")]
        name: Vec<u8>,
    },
    /// A graveyard entry for an attribute.
    GraveyardAttributeEntry { object_id: u64, attribute_id: AttributeId },
}

pub type AttributeKey = AttributeKeyV32;

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    TypeFingerprint,
    SerializeKey,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum AttributeKeyV32 {
    // Order here is important: code expects Attribute to precede Extent.
    Attribute,
    Extent(Extent),
}

/// ObjectKey is a key in the object store.
pub type ObjectKey = ObjectKeyV66;

#[derive(
    Clone,
    Debug,
    Eq,
    Ord,
    Hash,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    SerializeKey,
    TypeFingerprint,
    Versioned,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct ObjectKeyV66 {
    /// The ID of the object referred to.
    pub object_id: u64,
    /// The type and data of the key.
    pub data: ObjectKeyDataV66,
}

impl SortByU64 for ObjectKey {
    fn get_leading_u64(&self) -> u64 {
        self.object_id
    }
}

impl ObjectKey {
    /// Creates a generic ObjectKey.
    pub fn object(object_id: u64) -> Self {
        Self { object_id: object_id, data: ObjectKeyData::Object }
    }

    /// Creates an ObjectKey for an attribute.
    pub fn attribute(object_id: u64, attribute_id: AttributeId, key: AttributeKey) -> Self {
        Self { object_id, data: ObjectKeyData::Attribute(attribute_id, key) }
    }

    /// Creates an ObjectKey for an extent.
    pub fn extent(object_id: u64, attribute_id: AttributeId, range: std::ops::Range<u64>) -> Self {
        Self {
            object_id,
            data: ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(Extent(range))),
        }
    }

    /// Creates an ObjectKey from an extent.
    pub fn from_extent(object_id: u64, attribute_id: AttributeId, extent: Extent) -> Self {
        Self {
            object_id,
            data: ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(extent)),
        }
    }

    /// Creates an ObjectKey for a child.
    pub fn child(object_id: u64, name: &str) -> Self {
        Self { object_id, data: ObjectKeyData::Child { name: name.into() } }
    }

    /// Creates a graveyard entry for an object.
    pub fn graveyard_entry(graveyard_object_id: u64, object_id: u64) -> Self {
        Self { object_id: graveyard_object_id, data: ObjectKeyData::GraveyardEntry { object_id } }
    }

    /// Creates a graveyard entry for an attribute.
    pub fn graveyard_attribute_entry(
        graveyard_object_id: u64,
        object_id: u64,
        attribute_id: AttributeId,
    ) -> Self {
        Self {
            object_id: graveyard_object_id,
            data: ObjectKeyData::GraveyardAttributeEntry { object_id, attribute_id },
        }
    }

    /// Creates an ObjectKey for a ProjectLimit entry.
    pub fn project_limit(object_id: u64, project_id: ProjectId) -> Self {
        Self {
            object_id,
            data: ObjectKeyData::Project { project_id, property: ProjectProperty::Limit },
        }
    }

    /// Creates an ObjectKey for a ProjectUsage entry.
    pub fn project_usage(object_id: u64, project_id: ProjectId) -> Self {
        Self {
            object_id,
            data: ObjectKeyData::Project { project_id, property: ProjectProperty::Usage },
        }
    }

    pub fn extended_attribute(object_id: u64, name: Vec<u8>) -> Self {
        Self { object_id, data: ObjectKeyData::ExtendedAttribute { name } }
    }

    /// Returns the merge key for this key; that is, a key which is <= this key and any
    /// other possibly overlapping key, under Ord. This would be used for the hint in |merge_into|.
    pub fn key_for_merge_into(&self) -> Self {
        if let Self {
            object_id,
            data: ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(e)),
        } = self
        {
            Self::attribute(*object_id, *attribute_id, AttributeKey::Extent(e.key_for_merge_into()))
        } else {
            self.clone()
        }
    }
}

impl OrdUpperBound for ObjectKey {
    fn cmp_upper_bound(&self, other: &ObjectKey) -> std::cmp::Ordering {
        self.object_id.cmp(&other.object_id).then_with(|| match (&self.data, &other.data) {
            (
                ObjectKeyData::Attribute(left_attr_id, AttributeKey::Extent(left_extent)),
                ObjectKeyData::Attribute(right_attr_id, AttributeKey::Extent(right_extent)),
            ) => left_attr_id.cmp(right_attr_id).then(left_extent.cmp_upper_bound(right_extent)),
            _ => self.data.cmp(&other.data),
        })
    }
}

impl OrdLowerBound for ObjectKey {
    fn cmp_lower_bound(&self, other: &ObjectKey) -> std::cmp::Ordering {
        self.object_id.cmp(&other.object_id).then_with(|| match (&self.data, &other.data) {
            (
                ObjectKeyData::Attribute(left_attr_id, AttributeKey::Extent(left_extent)),
                ObjectKeyData::Attribute(right_attr_id, AttributeKey::Extent(right_extent)),
            ) => left_attr_id.cmp(right_attr_id).then(left_extent.cmp_lower_bound(right_extent)),
            _ => self.data.cmp(&other.data),
        })
    }
}

impl LayerKey for ObjectKey {
    fn merge_type(&self) -> MergeType {
        // This listing is intentionally exhaustive to force folks to think about how certain
        // subsets of the keyspace are merged.
        match self.data {
            ObjectKeyData::Object
            | ObjectKeyData::Attribute(..)
            | ObjectKeyData::Child { .. }
            | ObjectKeyData::GraveyardEntry { .. }
            | ObjectKeyData::GraveyardAttributeEntry { .. }
            | ObjectKeyData::Project { property: ProjectProperty::Limit, .. }
            | ObjectKeyData::ExtendedAttribute { .. } => MergeType::OptimizedMerge,
            ObjectKeyData::Project { property: ProjectProperty::Usage, .. } => MergeType::FullMerge,
        }
    }

    fn next_key(&self) -> Option<Self> {
        match &self.data {
            ObjectKeyData::Attribute(attr_id, AttributeKey::Extent(extent)) => {
                // This key comes before (or is equal to) any extent starting at or after the
                // end of `self`. Searching for its `next_key` finds extents that end after
                // the end of `self`.
                Some(ObjectKey {
                    object_id: self.object_id,
                    data: ObjectKeyData::Attribute(
                        *attr_id,
                        AttributeKey::Extent(Extent::search_key_from_offset(extent.end)),
                    ),
                })
            }
            _ => None,
        }
    }

    fn search_key(&self) -> Option<Self> {
        if let Self {
            object_id,
            data: ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(e)),
        } = self
        {
            Some(Self::attribute(*object_id, *attribute_id, AttributeKey::Extent(e.search_key())))
        } else {
            None
        }
    }

    fn is_search_key(&self) -> bool {
        match self {
            Self { data: ObjectKeyData::Attribute(_, AttributeKey::Extent(e)), .. } => {
                e.is_search_key()
            }
            _ => true,
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        if self.object_id != other.object_id {
            return false;
        }
        match (&self.data, &other.data) {
            (
                ObjectKeyData::Attribute(left_attr_id, AttributeKey::Extent(left_key)),
                ObjectKeyData::Attribute(right_attr_id, AttributeKey::Extent(right_key)),
            ) if *left_attr_id == *right_attr_id => left_key.overlaps(right_key),
            (a, b) => a == b,
        }
    }

    /// cladefs: an object store's item is what its value says it may be (`ObjectValue::is_valid_for`).
    fn is_valid_item<V: 'static>(&self, value: &V, block_size: BlockSize) -> bool {
        let value: &dyn Any = value;
        value.downcast_ref::<ObjectValue>().is_none_or(|value| value.is_valid_for(self, block_size))
    }
}

pub enum ObjectKeyFuzzyHashIterator {
    Extent(/* object_id */ u64, AttributeId, ExtentPartitionIterator),
    NotExtent(/* hash */ Option<u64>),
}

impl Iterator for ObjectKeyFuzzyHashIterator {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Extent(oid, attr_id, extent_keys) => extent_keys.next().map(|range| {
                let key = ObjectKey::extent(*oid, *attr_id, range);
                crate::stable_hash::stable_hash(key)
            }),
            Self::NotExtent(hash) => hash.take(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Extent(_, _, extent_keys) => extent_keys.size_hint(),
            Self::NotExtent(hash) => {
                let len = if hash.is_some() { 1 } else { 0 };
                (len, Some(len))
            }
        }
    }
}

impl ExactSizeIterator for ObjectKeyFuzzyHashIterator {}

impl FuzzyHash for ObjectKey {
    fn fuzzy_hash(&self) -> impl ExactSizeIterator<Item = u64> {
        match &self.data {
            ObjectKeyData::Attribute(attr_id, AttributeKey::Extent(extent)) => {
                ObjectKeyFuzzyHashIterator::Extent(
                    self.object_id,
                    *attr_id,
                    extent.fuzzy_hash_partition(),
                )
            }
            _ => {
                let hash = crate::stable_hash::stable_hash(self);
                ObjectKeyFuzzyHashIterator::NotExtent(Some(hash))
            }
        }
    }

    fn is_range_key(&self) -> bool {
        match &self.data {
            ObjectKeyData::Attribute(_, AttributeKey::Extent(_)) => true,
            _ => false,
        }
    }
}

/// UNIX epoch based timestamp in the UTC timezone.
pub type Timestamp = TimestampV49;

#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize,
    TypeFingerprint,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct TimestampV49 {
    nanos: u64,
}

impl Timestamp {
    const NSEC_PER_SEC: u64 = 1_000_000_000;

    pub fn now() -> Self {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).into()
    }

    pub const fn zero() -> Self {
        Self { nanos: 0 }
    }

    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    pub fn from_secs_and_nanos(secs: u64, nanos: u32) -> Self {
        let Some(secs_in_nanos) = secs.checked_mul(Self::NSEC_PER_SEC) else {
            error!("Fxfs doesn't support dates past 2554-07-21");
            return Self { nanos: u64::MAX };
        };
        let Some(nanos) = secs_in_nanos.checked_add(nanos as u64) else {
            error!("Fxfs doesn't support dates past 2554-07-21");
            return Self { nanos: u64::MAX };
        };
        Self { nanos }
    }

    /// Returns the total number of nanoseconds represented by this `Timestamp` since the Unix
    /// epoch.
    pub fn as_nanos(&self) -> u64 {
        self.nanos
    }

    /// Returns the fractional nanoseconds represented by this `Timestamp`.
    pub fn subsec_nanos(&self) -> u32 {
        (self.nanos % Self::NSEC_PER_SEC) as u32
    }

    /// Returns the total number of whole seconds represented by this `Timestamp` since the Unix
    /// epoch.
    pub fn as_secs(&self) -> u64 {
        self.nanos / Self::NSEC_PER_SEC
    }
}

impl From<std::time::Duration> for Timestamp {
    fn from(duration: std::time::Duration) -> Self {
        Self::from_secs_and_nanos(duration.as_secs(), duration.subsec_nanos())
    }
}

impl From<Timestamp> for std::time::Duration {
    fn from(timestamp: Timestamp) -> std::time::Duration {
        Duration::from_nanos(timestamp.nanos)
    }
}

pub type ObjectKind = ObjectKindV66;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectKindV66 {
    File {
        /// The number of references to this file.
        refs: u64,
    },
    Directory {
        /// The number of sub-directories in this directory.
        sub_dirs: u64,
    },
    Graveyard,
    Symlink {
        /// The number of references to this symbolic link.
        refs: u64,
        /// `link` is the target of the link and has no meaning within Fxfs; clients are free to
        /// interpret it however they like.
        #[serde(with = "crate::zerocopy_serialization")]
        link: Box<[u8]>,
    },
}

/// This consists of POSIX attributes that are not used in Fxfs but it may be meaningful to some
/// clients to have the ability to to set and retrieve these values.
pub type PosixAttributes = PosixAttributesV32;

#[derive(Clone, Debug, Copy, Default, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct PosixAttributesV32 {
    /// The mode bits associated with this object
    pub mode: u32,
    /// User ID of owner
    pub uid: u32,
    /// Group ID of owner
    pub gid: u32,
    /// Device ID
    pub rdev: u64,
}

/// Object-level attributes.  Note that these are not the same as "attributes" in the
/// ObjectValue::Attribute sense, which refers to an arbitrary data payload associated with an
/// object.  This naming collision is unfortunate.
pub type ObjectAttributes = ObjectAttributesV49;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct ObjectAttributesV49 {
    /// The timestamp at which the object was created (i.e. crtime).
    pub creation_time: TimestampV49,
    /// The timestamp at which the object's data was last modified (i.e. mtime).
    pub modification_time: TimestampV49,
    /// The project id to associate this object's resource usage with.
    #[serde(with = "crate::object_store::project_id::optional_project_id")]
    pub project_id: Option<ProjectId>,
    /// Mode, uid, gid, and rdev
    pub posix_attributes: Option<PosixAttributesV32>,
    /// The number of bytes allocated to all extents across all attributes for this object.
    pub allocated_size: u64,
    /// The timestamp at which the object was last read (i.e. atime).
    pub access_time: TimestampV49,
    /// The timestamp at which the object's status was last modified (i.e. ctime).
    pub change_time: TimestampV49,
}

pub type ExtendedAttributeValue = ExtendedAttributeValueV32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ExtendedAttributeValueV32 {
    /// The extended attribute value is stored directly in this object. If the value is above a
    /// certain size, it should be stored as an attribute with extents instead.
    Inline(#[serde(with = "crate::zerocopy_serialization")] Vec<u8>),
    /// The extended attribute value is stored as an attribute with extents. The attribute id
    /// should be chosen to be within the range of 64-512.
    AttributeId(AttributeId),
}

/// Id and descriptor for a child entry.
pub type ChildValue = ChildValueV63;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint, Versioned)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct ChildValueV63 {
    /// The ID of the child object.
    pub object_id: u64,
    /// Describes the type of the child.
    pub object_descriptor: ObjectDescriptorV63,
}

/// ObjectValue is the value of an item in the object store.
/// Note that the tree stores deltas on objects, so these values describe deltas. Unless specified
/// otherwise, a value indicates an insert/replace mutation.
pub type ObjectValue = ObjectValueV66;
impl Value for ObjectValue {
    const DELETED_MARKER: Self = Self::None;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint, Versioned)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectValueV66 {
    /// Some keys have no value (this often indicates a tombstone of some sort).  Records with this
    /// value are always filtered when a major compaction is performed, so the meaning must be the
    /// same as if the item was not present.
    None,
    /// Some keys have no value but need to differentiate between a present value and no value
    /// (None) i.e. their value is really a boolean: None => false, Some => true.
    Some,
    /// The value for an ObjectKey::Object record.
    Object { kind: ObjectKindV66, attributes: ObjectAttributesV49 },
    /// An attribute associated with a file object. |size| is the size of the attribute in bytes.
    Attribute { size: u64, has_overwrite_extents: bool },
    /// An extent associated with an object.
    Extent(ExtentValueV65),
    /// A child of an object.
    Child(ChildValue),
    /// Graveyard entries can contain these entries which will cause a file that has extents beyond
    /// EOF to be trimmed at mount time.  This is used in cases where shrinking a file can exceed
    /// the bounds of a single transaction.
    Trim,
    /// Added to support tracking Project ID usage and limits.
    BytesAndNodes { bytes: i64, nodes: i64 },
    /// A value for an extended attribute. Either inline or a redirection to an attribute with
    /// extents.
    ExtendedAttribute(ExtendedAttributeValueV32),
}

/// Storage for a pair of related byte and node values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BytesAndNodes {
    pub bytes: i64,
    pub nodes: i64,
}

impl BytesAndNodes {
    pub fn is_zero(&self) -> bool {
        self.bytes == 0 && self.nodes == 0
    }
}

impl std::ops::Add for BytesAndNodes {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self { bytes: self.bytes + rhs.bytes, nodes: self.nodes + rhs.nodes }
    }
}

impl std::ops::AddAssign for BytesAndNodes {
    fn add_assign(&mut self, rhs: Self) {
        self.bytes += rhs.bytes;
        self.nodes += rhs.nodes;
    }
}

impl From<BytesAndNodes> for ObjectValue {
    fn from(value: BytesAndNodes) -> Self {
        ObjectValue::BytesAndNodes { bytes: value.bytes, nodes: value.nodes }
    }
}

impl From<(i64, i64)> for BytesAndNodes {
    fn from((bytes, nodes): (i64, i64)) -> Self {
        Self { bytes, nodes }
    }
}

impl From<BytesAndNodes> for (i64, i64) {
    fn from(value: BytesAndNodes) -> Self {
        (value.bytes, value.nodes)
    }
}

impl ObjectValue {
    /// cladefs: whether this is what an object store's item of `key` may hold, in a filesystem of
    /// `block_size` blocks: the one check of what's read, of the journal's mutations
    /// (`Journal::validate_mutation`), a super-block's records, and a layer's items
    /// (`LayerKey::is_valid_item`), after which what reads them trusts them. Any extent's key is
    /// on blocks, not only those of extents that have a value, and what its value says fits it.
    pub fn is_valid_for(&self, key: &ObjectKey, block_size: BlockSize) -> bool {
        let ObjectKeyData::Attribute(_, AttributeKey::Extent(extent)) = &key.data else {
            return true;
        };
        if extent.is_empty() || !block_size.is_aligned(extent) {
            return false;
        }
        match self {
            ObjectValue::Extent(value) => value.fits(extent.end - extent.start, block_size),
            _ => true,
        }
    }

    /// Creates an ObjectValue for a file object.
    pub fn file(
        refs: u64,
        allocated_size: u64,
        creation_time: Timestamp,
        modification_time: Timestamp,
        access_time: Timestamp,
        change_time: Timestamp,
        project_id: Option<ProjectId>,
        posix_attributes: Option<PosixAttributes>,
    ) -> ObjectValue {
        ObjectValue::Object {
            kind: ObjectKind::File { refs },
            attributes: ObjectAttributes {
                creation_time,
                modification_time,
                project_id,
                posix_attributes,
                allocated_size,
                access_time,
                change_time,
            },
        }
    }
    /// Creates an ObjectValue for an object attribute.
    pub fn attribute(size: u64, has_overwrite_extents: bool) -> ObjectValue {
        ObjectValue::Attribute { size, has_overwrite_extents }
    }
    /// Creates an ObjectValue for an insertion/replacement of an object extent.
    pub fn extent(extent: ExtentId, extent_offset: u64) -> ObjectValue {
        ObjectValue::Extent(ExtentValue::new_raw(extent, extent_offset))
    }
    /// Creates an ObjectValue for an insertion/replacement of an object extent.
    pub fn extent_with_checksum(
        extent: ExtentId,
        extent_offset: u64,
        checksum: Checksums,
    ) -> ObjectValue {
        ObjectValue::Extent(ExtentValue::with_checksum(extent, extent_offset, checksum))
    }
    /// Creates an ObjectValue for a deletion of an object extent.
    pub fn deleted_extent() -> ObjectValue {
        ObjectValue::Extent(ExtentValue::deleted_extent())
    }
    /// Creates an ObjectValue for an object child.
    pub fn child(object_id: u64, object_descriptor: ObjectDescriptor) -> ObjectValue {
        ObjectValue::Child(ChildValue { object_id, object_descriptor })
    }
    /// Creates an ObjectValue for an object symlink.
    pub fn symlink(
        link: impl Into<Box<[u8]>>,
        creation_time: Timestamp,
        modification_time: Timestamp,
        project_id: Option<ProjectId>,
    ) -> ObjectValue {
        ObjectValue::Object {
            kind: ObjectKind::Symlink { refs: 1, link: link.into() },
            attributes: ObjectAttributes {
                creation_time,
                modification_time,
                project_id,
                ..Default::default()
            },
        }
    }
    pub fn inline_extended_attribute(value: impl Into<Vec<u8>>) -> ObjectValue {
        ObjectValue::ExtendedAttribute(ExtendedAttributeValue::Inline(value.into()))
    }
    pub fn extended_attribute(attribute_id: AttributeId) -> ObjectValue {
        ObjectValue::ExtendedAttribute(ExtendedAttributeValue::AttributeId(attribute_id))
    }
}

pub type ObjectItem = ObjectItemV66;

pub type ObjectItemV66 = Item<ObjectKeyV66, ObjectValueV66>;

impl ObjectItem {
    pub fn is_tombstone(&self) -> bool {
        matches!(
            self,
            Item {
                key: ObjectKey { data: ObjectKeyData::Object, .. },
                value: ObjectValue::None,
                ..
            }
        )
    }
}

// If the given item describes an extent, unwraps it and returns the extent key/value.
impl<'a> From<ItemRef<'a, ObjectKey, ObjectValue>>
    for Option<(/*object-id*/ u64, AttributeId, &'a Extent, &'a ExtentValue)>
{
    fn from(item: ItemRef<'a, ObjectKey, ObjectValue>) -> Self {
        match item {
            ItemRef {
                key:
                    ObjectKey {
                        object_id,
                        data:
                            ObjectKeyData::Attribute(
                                attribute_id, //
                                AttributeKey::Extent(extent_key),
                            ),
                    },
                value: ObjectValue::Extent(extent_value),
                ..
            } => Some((*object_id, *attribute_id, extent_key, extent_value)),
            _ => None,
        }
    }
}

#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    Serialize,
    Deserialize,
    Hash,
    SerializeKey,
    TypeFingerprint,
)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
#[repr(transparent)]
pub struct AttributeId(pub u64);

impl AttributeId {
    /// The common case for extents which cover the data payload of an object.
    pub const DATA: Self = Self(0);

    /// The range of fxfs attribute IDs which are reserved for extended attribute values. Whenever a
    /// new attribute is needed, the first unused ID will be chosen from this range. It's
    /// technically safe to change these values, but it has potential consequences - they are only
    /// used during ID selection, so any existing extended attributes keep their IDs, which means
    /// any past or present selected range here could potentially have used attributes unless they
    /// are explicitly migrated, which isn't currently done.
    pub const XATTR_RANGE_START: Self = Self(64);
    pub const XATTR_RANGE_END: Self = Self(512);

    /// A semantic alias for the `0` attribute ID, indicating that it is being used as a starting
    /// point to iterate over all attributes rather than specifically looking up the primary data
    /// attribute [`AttributeId::DATA`].
    pub const SORTED_START: Self = Self(0);

    /// An attribute ID to use in tests when no particular ID is necessary.
    #[cfg(test)]
    pub const TEST_ID: Self = Self(u64::MAX - 1000);

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns the current id + 1.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Returns true if the attribute ID is within the range of extended attributes.
    pub const fn is_xattr(self) -> bool {
        self.0 >= Self::XATTR_RANGE_START.0 && self.0 < Self::XATTR_RANGE_END.0
    }
}

impl log::kv::ToValue for AttributeId {
    fn to_value(&self) -> log::kv::Value<'_> {
        log::kv::Value::from(self.0)
    }
}

impl std::fmt::Display for AttributeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::{AttributeId, ObjectKey, ObjectKeyV66, TimestampV49};
    use crate::lsm_tree::types::{FuzzyHash as _, LayerKey};
    use crate::object_store::extent::MIN_BLOCK_SIZE;
    use std::ops::Add;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // Smoke test to ensure hash stability for Fxfs objects.
    // If this test fails, the hash algorithm changed, and that won't do -- Fxfs relies on stable
    // hash values, and existing images will appear to be corrupt if they change (see
    // https://fxbug.dev/419133532).
    #[test]
    fn test_hash_stability() {
        // Target a specific version of ObjectKey.  If you want to delete ObjectKeyV66, simply
        // update this test with a later key version, which will also require re-generating the
        // hashes.
        assert_eq!(
            &ObjectKeyV66::object(100).fuzzy_hash().collect::<Vec<_>>()[..],
            &[11885326717398844384]
        );
        assert_eq!(
            &ObjectKeyV66::extent(1, AttributeId::DATA, 0..2 * 1024 * 1024)
                .fuzzy_hash()
                .collect::<Vec<_>>()[..],
            &[240613681451893062, 14933272422807517849]
        );
    }

    #[test]
    fn test_fuzzy_hash_len() {
        let key = ObjectKey::object(100);
        let mut iter = key.fuzzy_hash();
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert!(iter.next().is_some());
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.next(), None);

        let key = ObjectKey::extent(1, AttributeId::DATA, 0..2 * 1024 * 1024);
        let mut iter = key.fuzzy_hash();
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.size_hint(), (2, Some(2)));
        assert!(iter.next().is_some());
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert!(iter.next().is_some());
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_next_key() {
        assert_eq!(
            ObjectKey::extent(1, AttributeId::TEST_ID, 25 * MIN_BLOCK_SIZE..100 * MIN_BLOCK_SIZE)
                .next_key()
                .unwrap(),
            ObjectKey::extent(1, AttributeId::TEST_ID, 100 * MIN_BLOCK_SIZE..101 * MIN_BLOCK_SIZE)
        );
        assert_eq!(ObjectKey::object(100).next_key(), None);
    }

    #[test]
    fn test_range_key() {
        const ATTR_ID: AttributeId = AttributeId::TEST_ID;
        // Make sure we disallow using extent keys with point queries. Other object keys should
        // still be allowed with point queries.
        assert!(ObjectKey::extent(1, ATTR_ID, 0..2 * 1024 * 1024).is_range_key());
        assert!(!ObjectKey::object(100).is_range_key());

        assert_eq!(ObjectKey::object(1).overlaps(&ObjectKey::object(1)), true);
        assert_eq!(ObjectKey::object(1).overlaps(&ObjectKey::object(2)), false);
        assert_eq!(ObjectKey::extent(1, ATTR_ID, 0..100).overlaps(&ObjectKey::object(1)), false);
        assert_eq!(ObjectKey::object(1).overlaps(&ObjectKey::extent(1, ATTR_ID, 0..100)), false);
        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 0..100).overlaps(&ObjectKey::extent(2, ATTR_ID, 0..100)),
            false
        );
        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 0..100).overlaps(&ObjectKey::extent(
                1,
                ATTR_ID.next(),
                0..100
            )),
            false
        );
        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 0..100).overlaps(&ObjectKey::extent(1, ATTR_ID, 0..100)),
            true
        );

        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 0..50).overlaps(&ObjectKey::extent(1, ATTR_ID, 49..100)),
            true
        );
        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 49..100).overlaps(&ObjectKey::extent(1, ATTR_ID, 0..50)),
            true
        );

        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 0..50).overlaps(&ObjectKey::extent(1, ATTR_ID, 50..100)),
            false
        );
        assert_eq!(
            ObjectKey::extent(1, ATTR_ID, 50..100).overlaps(&ObjectKey::extent(1, ATTR_ID, 0..50)),
            false
        );
    }

    #[test]
    fn test_timestamp() {
        fn compare_time(std_time: Duration) {
            let ts_time: TimestampV49 = std_time.into();
            assert_eq!(<TimestampV49 as Into<Duration>>::into(ts_time), std_time);
            assert_eq!(ts_time.subsec_nanos(), std_time.subsec_nanos());
            assert_eq!(ts_time.as_secs(), std_time.as_secs());
            assert_eq!(ts_time.as_nanos() as u128, std_time.as_nanos());
        }
        compare_time(Duration::from_nanos(0));
        compare_time(Duration::from_nanos(u64::MAX));
        compare_time(SystemTime::now().duration_since(UNIX_EPOCH).unwrap());

        let ts: TimestampV49 = Duration::from_secs(u64::MAX - 1).into();
        assert_eq!(ts.nanos, u64::MAX);

        let ts: TimestampV49 = (Duration::from_nanos(u64::MAX).add(Duration::from_nanos(1))).into();
        assert_eq!(ts.nanos, u64::MAX);
    }
}
