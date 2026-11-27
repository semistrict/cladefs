// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/fsck/store_scanner.rs`. cladefs snapshots (volume snapshots that
// share their source's layers and extents) deviate from upstream here: a store's extents are its
// data owner's except where frozen owners hold them or a base does (backlog doc-10), and a root
// object (a shared layer) may be referenced by several stores' information.

use crate::fsck::errors::{FsckError, FsckFatal, FsckWarning};
use crate::fsck::{FragmentationStats, Fsck, FsckResult};
use crate::lsm_tree::Query;
use crate::lsm_tree::types::{Item, ItemRef, LayerIterator};
use crate::object_handle::INVALID_OBJECT_ID;
use crate::object_store::graveyard::Graveyard;
use crate::object_store::{
    AttributeId, AttributeKey, ChildValue, ExtendedAttributeValue, ExtentValue, ObjectAttributes,
    ObjectDescriptor, ObjectKey, ObjectKeyData, ObjectKind, ObjectStore, ObjectValue,
};
use crate::range::RangeExt;
use anyhow::Error;
use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::ops::Range;
use storage_device::ExtentId;
use storage_units::BlockSize;

/// cladefs: how far past a file's end, to the next boundary of this many bytes, appends through a
/// DAX window reserve blocks ahead of their writes (the guest's FXFS_DAX_APPEND_SIZE, TASK-92.58),
/// which a clean unmount leaves as a crash does (TASK-133).
const APPEND_RESERVATION: u64 = 64 << 10;

/// cladefs: where a file `size` bytes long may have blocks up to.
fn max_extent_end(bs: BlockSize, size: u64) -> u64 {
    // What a corrupt file's size may be.
    let reserved = size.checked_next_multiple_of(APPEND_RESERVATION).unwrap_or(u64::MAX);
    std::cmp::max(bs.align_up(size).unwrap_or(u64::MAX), reserved)
}

// Information about a specific attribute.
#[derive(Debug)]
struct ScannedAttribute {
    // ID of the attribute. Most commonly zero, which is the attribute value regular data is stored
    // in, but others are used for extended attributes and merkle trees.
    attribute_id: AttributeId,
    // The logical size of this attribute according to its metadata.
    size: u64,
}

// Information for scanned objects about their allocated attributes.
#[derive(Debug)]
struct ScannedAttributes {
    attributes: Vec<ScannedAttribute>,
    // A list of attributes that have been tombstoned.
    tombstoned_attributes: Vec<AttributeId>,
    // The object's allocated size, according to its metadata.
    stored_allocated_size: u64,
    // The allocated size of the object (computed by summing up the extents for the file).
    observed_allocated_size: u64,
    // The object is in the graveyard which means extents beyond the end of the file are allowed.
    in_graveyard: bool,
    // Any extended attributes which point at an fxfs attribute for this object.
    extended_attributes: Vec<AttributeId>,
}

#[derive(Debug)]
struct ScannedFile {
    // A list of parent object IDs for the file.  INVALID_OBJECT_ID indicates a reference from
    // outside the object store (either the graveyard, or because the object is a root object of the
    // store and probably has a reference to it in e.g. the StoreInfo or superblock).
    parents: Vec<u64>,
    // The number of references to the file, according to its metadata.
    stored_refs: u64,
    // Attributes for this file.
    attributes: ScannedAttributes,
    // cladefs: what its entries say it is, as its POSIX mode says: a regular file, or a special file.
    descriptor: ObjectDescriptor,
}

#[derive(Debug)]
struct ScannedDir {
    // The number of sub-directories of the dir, according to its metadata.
    stored_sub_dirs: u64,
    // The number of sub-directories we found for the dir.
    observed_sub_dirs: u64,
    // The parent object of the directory.  See ScannedFile::parents.  Note that directories can
    // only have one parent, hence this is just an Option (not a Vec).
    parent: Option<u64>,
    // Used to detect directory cycles.
    visited: UnsafeCell<bool>,
    // Attributes for this directory.
    attributes: ScannedAttributes,
}

unsafe impl Sync for ScannedDir {}

#[derive(Debug)]
struct ScannedSymlink {
    // A list of parent object IDs for the symlink.
    parents: Vec<u64>,
    // The number of references to the file, according to its metadata.
    stored_refs: u64,
    // Attributes for this symlink
    attributes: ScannedAttributes,
}

#[derive(Debug)]
enum ScannedObject {
    Directory(ScannedDir),
    File(ScannedFile),
    Graveyard,
    Symlink(ScannedSymlink),
    // A tombstoned object, which should have no other records associated with it.
    Tombstone,
}

struct ScannedStore<'a> {
    fsck: &'a Fsck<'a>,
    objects: BTreeMap<u64, ScannedObject>,
    root_objects: Vec<u64>,
    store_id: u64,
    is_root_store: bool,
}

impl<'a> ScannedStore<'a> {
    fn new(
        fsck: &'a Fsck<'a>,
        root_objects: impl AsRef<[u64]>,
        store_id: u64,
        is_root_store: bool,
    ) -> Self {
        Self {
            fsck,
            objects: BTreeMap::new(),
            root_objects: root_objects.as_ref().into(),
            store_id,
            is_root_store,
        }
    }

    // Process an object store record, adding or updating any objects known to the ScannedStore.
    fn process(&mut self, key: &ObjectKey, value: &ObjectValue) -> Result<(), Error> {
        match key.data {
            ObjectKeyData::Object => {
                match value {
                    ObjectValue::None => {
                        if self.objects.insert(key.object_id, ScannedObject::Tombstone).is_some() {
                            self.fsck.error(FsckError::TombstonedObjectHasRecords(
                                self.store_id,
                                key.object_id,
                            ))?;
                        }
                    }
                    ObjectValue::Some => {
                        self.fsck.error(FsckError::UnexpectedRecordInObjectStore(
                            self.store_id,
                            key.into(),
                            value.into(),
                        ))?;
                    }
                    ObjectValue::Object {
                        kind: ObjectKind::File { refs },
                        attributes: ObjectAttributes { allocated_size, posix_attributes, .. },
                    } => {
                        // cladefs: a root object is referenced once by each of the root objects
                        // listing it, as a layer file is by each store sharing it (snapshots).
                        let roots =
                            self.root_objects.iter().filter(|id| **id == key.object_id).count();
                        let parents = vec![INVALID_OBJECT_ID; roots];
                        self.objects.insert(
                            key.object_id,
                            ScannedObject::File(ScannedFile {
                                parents,
                                stored_refs: *refs,
                                attributes: ScannedAttributes {
                                    attributes: Vec::new(),
                                    tombstoned_attributes: Vec::new(),
                                    stored_allocated_size: *allocated_size,
                                    observed_allocated_size: 0,
                                    in_graveyard: false,
                                    extended_attributes: Vec::new(),
                                },
                                descriptor: ObjectDescriptor::of_file(
                                    posix_attributes.as_ref().map(|posix| posix.mode),
                                ),
                            }),
                        );
                    }
                    ObjectValue::Object {
                        kind: ObjectKind::Directory { sub_dirs },
                        attributes: ObjectAttributes { allocated_size, .. },
                    } => {
                        let parent = if self.root_objects.contains(&key.object_id) {
                            Some(INVALID_OBJECT_ID)
                        } else {
                            None
                        };
                        // We've verified no duplicate keys, and Object records come first,
                        // so this should always be the first time we encounter this object.
                        self.objects.insert(
                            key.object_id,
                            ScannedObject::Directory(ScannedDir {
                                stored_sub_dirs: *sub_dirs,
                                observed_sub_dirs: 0,
                                parent,
                                visited: UnsafeCell::new(false),
                                attributes: ScannedAttributes {
                                    attributes: Vec::new(),
                                    tombstoned_attributes: Vec::new(),
                                    stored_allocated_size: *allocated_size,
                                    observed_allocated_size: 0,
                                    in_graveyard: false,
                                    extended_attributes: Vec::new(),
                                },
                            }),
                        );
                    }
                    ObjectValue::Object { kind: ObjectKind::Graveyard, .. } => {
                        self.objects.insert(key.object_id, ScannedObject::Graveyard);
                    }
                    ObjectValue::Object {
                        kind: ObjectKind::Symlink { refs, .. },
                        attributes: ObjectAttributes { allocated_size, .. },
                    } => {
                        self.objects.insert(
                            key.object_id,
                            ScannedObject::Symlink(ScannedSymlink {
                                parents: vec![],
                                stored_refs: *refs,
                                attributes: ScannedAttributes {
                                    attributes: Vec::new(),
                                    tombstoned_attributes: Vec::new(),
                                    stored_allocated_size: *allocated_size,
                                    observed_allocated_size: 0,
                                    in_graveyard: false,
                                    extended_attributes: Vec::new(),
                                },
                            }),
                        );
                    }
                    _ => {
                        self.fsck.error(FsckError::MalformedObjectRecord(
                            self.store_id,
                            key.into(),
                            value.into(),
                        ))?;
                    }
                }
            }
            ObjectKeyData::Attribute(attribute_id, AttributeKey::Attribute) => {
                match value {
                    ObjectValue::Attribute { size } => {
                        match self.objects.get_mut(&key.object_id) {
                            Some(
                                ScannedObject::File(ScannedFile { attributes, .. })
                                | ScannedObject::Directory(ScannedDir { attributes, .. })
                                | ScannedObject::Symlink(ScannedSymlink { attributes, .. }),
                            ) => {
                                attributes
                                    .attributes
                                    .push(ScannedAttribute { attribute_id, size: *size });
                            }
                            Some(ScannedObject::Graveyard) => { /* NOP */ }
                            Some(ScannedObject::Tombstone) => {
                                self.fsck.error(FsckError::TombstonedObjectHasRecords(
                                    self.store_id,
                                    key.object_id,
                                ))?;
                            }
                            None => {
                                // We verify key ordering elsewhere, and Object records come before
                                // Attribute records, so we should never find an attribute without
                                // its object already encountered.  Thus, this is an orphaned
                                // attribute.
                                self.fsck.warning(FsckWarning::OrphanedAttribute(
                                    self.store_id,
                                    key.object_id,
                                    attribute_id,
                                ))?;
                            }
                        }
                    }
                    // Deleted attribute.
                    ObjectValue::None => (),
                    _ => {
                        self.fsck.error(FsckError::MalformedObjectRecord(
                            self.store_id,
                            key.into(),
                            value.into(),
                        ))?;
                    }
                }
            }
            // Mostly ignore extents on this pass. We'll process them later.
            ObjectKeyData::Attribute(_, AttributeKey::Extent(_)) => {
                match value {
                    // Regular and deleted extent records.
                    ObjectValue::Extent(_) => {}
                    _ => {
                        self.fsck.error(FsckError::MalformedObjectRecord(
                            self.store_id,
                            key.into(),
                            value.into(),
                        ))?;
                    }
                }
            }
            ObjectKeyData::Child { .. } => match value {
                ObjectValue::None => {}
                ObjectValue::Child(ChildValue { object_id: child_id, object_descriptor }) => {
                    if *child_id == INVALID_OBJECT_ID {
                        self.fsck.warning(FsckWarning::InvalidObjectIdInStore(
                            self.store_id,
                            key.into(),
                            value.into(),
                        ))?;
                    }
                    if self.root_objects.contains(child_id) {
                        self.fsck.error(FsckError::RootObjectHasParent(
                            self.store_id,
                            *child_id,
                            key.object_id,
                        ))?;
                    }
                    if object_descriptor == &ObjectDescriptor::Volume && !self.is_root_store {
                        self.fsck.error(FsckError::VolumeInChildStore(self.store_id, *child_id))?;
                    }
                }
                _ => {
                    self.fsck.error(FsckError::MalformedObjectRecord(
                        self.store_id,
                        key.into(),
                        value.into(),
                    ))?;
                }
            },
            ObjectKeyData::ExtendedAttribute { .. } => match value {
                ObjectValue::None => {}
                ObjectValue::ExtendedAttribute(ExtendedAttributeValue::Inline(_)) => {
                    if self.objects.get(&key.object_id).is_none() {
                        self.fsck.warning(FsckWarning::OrphanedExtendedAttributeRecord(
                            self.store_id,
                            key.object_id,
                        ))?;
                    }
                }
                ObjectValue::ExtendedAttribute(ExtendedAttributeValue::AttributeId(id)) => {
                    match self.objects.get_mut(&key.object_id) {
                        Some(
                            ScannedObject::File(ScannedFile { attributes, .. })
                            | ScannedObject::Directory(ScannedDir { attributes, .. })
                            | ScannedObject::Symlink(ScannedSymlink { attributes, .. }),
                        ) => {
                            attributes.extended_attributes.push(*id);
                        }
                        Some(ScannedObject::Graveyard) => { /* NOP */ }
                        Some(ScannedObject::Tombstone) => {
                            self.fsck.error(FsckError::TombstonedObjectHasRecords(
                                self.store_id,
                                key.object_id,
                            ))?;
                        }
                        None => {
                            self.fsck.warning(FsckWarning::OrphanedExtendedAttributeRecord(
                                self.store_id,
                                key.object_id,
                            ))?;
                        }
                    }
                }
                _ => {
                    self.fsck.error(FsckError::MalformedObjectRecord(
                        self.store_id,
                        key.into(),
                        value.into(),
                    ))?;
                }
            },
            ObjectKeyData::GraveyardEntry { .. } => {}
            ObjectKeyData::GraveyardAttributeEntry { .. } => {}
        }
        Ok(())
    }

    /// Performs some checks on the child link and records information such as the sub-directory
    /// count that get verified later.
    fn process_child(
        &mut self,
        parent_id: u64,
        child_id: u64,
        object_descriptor: &ObjectDescriptor,
    ) -> Result<(), Error> {
        match (self.objects.get_mut(&child_id), object_descriptor) {
            (Some(ScannedObject::File(ScannedFile { parents, descriptor, .. })), linked)
                if linked == descriptor || linked == &ObjectDescriptor::Volume =>
            {
                parents.push(parent_id);
            }
            (
                Some(ScannedObject::Directory(ScannedDir { parent, .. })),
                ObjectDescriptor::Directory,
            ) => {
                if parent.is_some() {
                    // TODO(https://fxbug.dev/42168496): Accumulating and reporting all parents
                    // might be useful.
                    self.fsck
                        .error(FsckError::MultipleLinksToDirectory(self.store_id, child_id))?;
                }
                *parent = Some(parent_id);
            }
            (Some(ScannedObject::Tombstone), _) => {
                self.fsck.error(FsckError::TombstonedObjectHasRecords(self.store_id, parent_id))?;
                return Ok(());
            }
            (None, _) => {
                self.fsck.error(FsckError::MissingObjectInfo(self.store_id, child_id))?;
                return Ok(());
            }
            (Some(s), _) => {
                let expected = match s {
                    ScannedObject::Directory(_) => ObjectDescriptor::Directory,
                    ScannedObject::File(ScannedFile { descriptor, .. }) => *descriptor,
                    ScannedObject::Graveyard => ObjectDescriptor::File,
                    ScannedObject::Symlink(ScannedSymlink { parents, .. }) => {
                        parents.push(parent_id);
                        ObjectDescriptor::Symlink
                    }
                    ScannedObject::Tombstone => unreachable!(),
                };
                if &expected != object_descriptor {
                    self.fsck.error(FsckError::ConflictingTypeForLink(
                        self.store_id,
                        child_id,
                        expected.into(),
                        object_descriptor.into(),
                    ))?;
                }
            }
        }
        match self.objects.get_mut(&parent_id) {
            Some(
                ScannedObject::File(..) | ScannedObject::Graveyard | ScannedObject::Symlink(_),
            ) => {
                self.fsck.error(FsckError::ObjectHasChildren(self.store_id, parent_id))?;
            }
            Some(ScannedObject::Directory(ScannedDir { observed_sub_dirs, .. })) => {
                if *object_descriptor == ObjectDescriptor::Directory {
                    *observed_sub_dirs += 1;
                }
            }
            Some(ScannedObject::Tombstone) => {
                self.fsck.error(FsckError::TombstonedObjectHasRecords(self.store_id, parent_id))?;
            }
            None => self.fsck.error(FsckError::MissingObjectInfo(self.store_id, parent_id))?,
        }
        Ok(())
    }

    // Process an extent, performing some checks and building fsck.allocations.
    fn process_extent(
        &mut self,
        object_id: u64,
        attribute_id: AttributeId,
        range: &Range<u64>,
        store_extent: ExtentId,
        device_offset: u64,
        bs: BlockSize,
    ) -> Result<(), Error> {
        if !bs.is_aligned(range) {
            self.fsck.error(FsckError::MisalignedExtent(
                self.store_id,
                object_id,
                range.clone(),
                0,
            ))?;
        }
        if range.start >= range.end {
            self.fsck.error(FsckError::MalformedExtent(
                self.store_id,
                object_id,
                range.clone(),
                0,
            ))?;
            return Ok(());
        }
        let len = range.end - range.start;
        match self.objects.get_mut(&object_id) {
            Some(
                ScannedObject::File(ScannedFile { attributes, .. })
                | ScannedObject::Directory(ScannedDir { attributes, .. })
                | ScannedObject::Symlink(ScannedSymlink { attributes, .. }),
            ) => {
                let ScannedAttributes {
                    attributes,
                    tombstoned_attributes,
                    observed_allocated_size: allocated_size,
                    in_graveyard,
                    ..
                } = attributes;
                match attributes.iter_mut().find(|attribute| attribute.attribute_id == attribute_id)
                {
                    Some(attribute) => {
                        if !*in_graveyard
                            && !tombstoned_attributes.contains(&attribute_id)
                            && range.end > max_extent_end(bs, attribute.size)
                        {
                            self.fsck.error(FsckError::ExtentExceedsLength(
                                self.store_id,
                                object_id,
                                attribute_id,
                                attribute.size,
                                range.into(),
                            ))?;
                        }
                    }
                    None => {
                        self.fsck.warning(FsckWarning::ExtentForMissingAttribute(
                            self.store_id,
                            object_id,
                            attribute_id,
                        ))?;
                    }
                }
                *allocated_size += len;
            }
            Some(ScannedObject::Graveyard | ScannedObject::Tombstone) => { /* NOP */ }
            None => {
                self.fsck
                    .warning(FsckWarning::ExtentForNonexistentObject(self.store_id, object_id))?;
            }
        }
        if !bs.is_aligned(device_offset) {
            self.fsck.error(FsckError::MisalignedExtent(
                self.store_id,
                object_id,
                range.clone(),
                device_offset,
            ))?;
        }
        self.fsck.reference(store_extent, device_offset..device_offset + len, self.store_id);
        Ok(())
    }

    // A graveyard entry can either be for tombstoning a file/attribute, or for trimming a file.
    fn handle_graveyard_entry(
        &mut self,
        object_id: u64,
        attribute_id: Option<AttributeId>,
        tombstone: bool,
    ) -> Result<(), Error> {
        match self.objects.get_mut(&object_id) {
            Some(ScannedObject::File(ScannedFile {
                parents,
                attributes: ScannedAttributes { in_graveyard, tombstoned_attributes, .. },
                ..
            })) => {
                if attribute_id.is_none() {
                    *in_graveyard = true;
                }
                if tombstone {
                    if let Some(attribute_id) = attribute_id {
                        tombstoned_attributes.push(attribute_id);
                    } else {
                        parents.push(INVALID_OBJECT_ID)
                    }
                }
            }
            Some(
                ScannedObject::Directory(ScannedDir { attributes, .. })
                | ScannedObject::Symlink(ScannedSymlink { attributes, .. }),
            ) => {
                if tombstone {
                    if let Some(attribute_id) = attribute_id {
                        attributes.tombstoned_attributes.push(attribute_id);
                    } else {
                        attributes.in_graveyard = true;
                    }
                } else {
                    self.fsck.error(FsckError::UnexpectedObjectInGraveyard(object_id))?;
                }
            }
            Some(ScannedObject::Graveyard | ScannedObject::Tombstone) => {
                self.fsck.error(FsckError::UnexpectedObjectInGraveyard(object_id))?;
            }
            None => {
                self.fsck.warning(FsckWarning::GraveyardRecordForAbsentObject(
                    self.store_id,
                    object_id,
                ))?;
            }
        }
        Ok(())
    }
}

// Scans extents and directory child entries in the store, emitting synthesized allocations into
// |fsck.allocations|, updating the sizes for files in |scanned| and performing checks on directory
// children.
// TODO(https://fxbug.dev/42177485): Roll the extent scanning back into main function.
async fn scan_extents_and_directory_children<'a>(
    store: &ObjectStore,
    scanned: &mut ScannedStore<'a>,
    result: &mut FsckResult,
) -> Result<(), Error> {
    let bs = store.block_size();
    let layer_set = store.tree().layer_set();
    let mut merger = layer_set.merger();
    let mut iter = merger.query(Query::FullScan).await?;
    let mut allocated_bytes = 0;
    let mut extent_count = 0;
    let mut previous_object_id = INVALID_OBJECT_ID;
    while let Some(itemref) = iter.get() {
        match itemref {
            ItemRef {
                key:
                    ObjectKey {
                        object_id,
                        data:
                            ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(extent_key)),
                    },
                value: ObjectValue::Extent(extent),
                ..
            } => {
                // Ignore deleted extents.
                if let ExtentValue::Some { extent: store_extent, extent_offset, .. } = extent {
                    let size = extent_key.length().unwrap_or(0);
                    allocated_bytes += size;

                    if previous_object_id != *object_id {
                        if previous_object_id != INVALID_OBJECT_ID {
                            result.fragmentation.extent_count
                                [FragmentationStats::get_histogram_bucket_for_count(
                                    extent_count,
                                )] += 1;
                        }
                        extent_count = 0;
                        previous_object_id = *object_id;
                    }
                    result.fragmentation.extent_size
                        [FragmentationStats::get_histogram_bucket_for_size(size)] += 1;
                    extent_count += 1;

                    scanned.process_extent(
                        *object_id,
                        *attribute_id,
                        extent_key,
                        *store_extent,
                        *extent_offset,
                        bs,
                    )?;
                };
            }
            ItemRef {
                key: ObjectKey { object_id, data: ObjectKeyData::Child { .. } },
                value: ObjectValue::Child(ChildValue { object_id: child_id, object_descriptor }),
                ..
            } => scanned.process_child(*object_id, *child_id, object_descriptor)?,
            _ => {}
        }
        iter.advance().await?;
    }
    if extent_count != 0 && previous_object_id != INVALID_OBJECT_ID {
        result.fragmentation.extent_count
            [FragmentationStats::get_histogram_bucket_for_count(extent_count)] += 1;
    }
    scanned.fsck.verbose(format!(
        "Store {} has {} bytes allocated",
        store.store_object_id(),
        allocated_bytes
    ));
    Ok(())
}

fn validate_attributes(
    fsck: &Fsck<'_>,
    store_id: u64,
    object_id: u64,
    attributes: &ScannedAttributes,
    is_file: bool,
) -> Result<(), Error> {
    let ScannedAttributes {
        attributes,
        tombstoned_attributes,
        observed_allocated_size,
        stored_allocated_size,
        extended_attributes,
        ..
    } = attributes;
    if observed_allocated_size != stored_allocated_size {
        fsck.error(FsckError::AllocatedSizeMismatch(
            store_id,
            object_id,
            *observed_allocated_size,
            *stored_allocated_size,
        ))?;
    }

    if is_file && !attributes.iter().any(|attribute| attribute.attribute_id == AttributeId::DATA) {
        fsck.error(FsckError::MissingDataAttribute(store_id, object_id))?;
    }

    // Attributes queued for tombstoning must exist.
    for attr in tombstoned_attributes {
        if attributes.iter().find(|attribute| attribute.attribute_id == *attr).is_none() {
            fsck.error(FsckError::TombstonedAttributeDoesNotExist(store_id, object_id, *attr))?
        }
    }

    for expected_attribute_id in extended_attributes {
        if !expected_attribute_id.is_xattr() {
            fsck.error(FsckError::InvalidExtendedAttributeId(
                store_id,
                object_id,
                *expected_attribute_id,
            ))?;
        }
        if attributes
            .iter()
            .find(|attribute| attribute.attribute_id == *expected_attribute_id)
            .is_none()
        {
            fsck.error(FsckError::MissingAttributeForExtendedAttribute(
                store_id,
                object_id,
                *expected_attribute_id,
            ))?;
        }
    }

    for attribute in attributes {
        if attribute.attribute_id.is_xattr() {
            // For all the attributes in the extended attribute range, make sure there is an
            // extended attribute record for them.
            if extended_attributes
                .iter()
                .find(|xattr_id| attribute.attribute_id == **xattr_id)
                .is_none()
            {
                fsck.warning(FsckWarning::OrphanedExtendedAttribute(
                    store_id,
                    object_id,
                    attribute.attribute_id,
                ))?;
            }
        }
    }

    Ok(())
}

/// Scans an object store, accumulating all of its allocations into |fsck.allocations| and
/// validating various object properties.
pub(super) async fn scan_store(
    fsck: &Fsck<'_>,
    store: &ObjectStore,
    root_objects: impl AsRef<[u64]>,
    result: &mut FsckResult,
) -> Result<(), Error> {
    let store_id = store.store_object_id();
    let next_object_id = store.query_next_object_id();

    let mut scanned = ScannedStore::new(fsck, root_objects, store_id, store.is_root());

    // Scan the store for objects, attributes, and parent/child relationships.
    let layer_set = store.tree().layer_set();
    let mut merger = layer_set.merger();
    let mut iter = merger.query(Query::FullScan).await?;
    let mut last_item: Option<Item<ObjectKey, ObjectValue>> = None;

    let mut last_object_id = INVALID_OBJECT_ID;
    let mut highest_object_id = INVALID_OBJECT_ID;

    while let Some(item) = iter.get() {
        if let Some(last_item) = last_item {
            if last_item.key >= *item.key {
                fsck.fatal(FsckFatal::MisOrderedObjectStore(store_id))?;
            }
        }
        if item.key.object_id == INVALID_OBJECT_ID {
            fsck.warning(FsckWarning::InvalidObjectIdInStore(
                store_id,
                item.key.into(),
                item.value.into(),
            ))?;
        }
        if item.key.object_id != last_object_id {
            if item.key.object_id == next_object_id {
                fsck.error(FsckError::NextObjectIdInUse(store_id, next_object_id))?;
            }
            highest_object_id = highest_object_id.max(item.key.object_id);
            last_object_id = item.key.object_id;
        }
        scanned.process(item.key, item.value)?;
        last_item = Some(item.cloned());
        iter.advance().await?;
    }

    // Add a reference for files in the graveyard (which acts as the file's parent until it is
    // purged, leaving only the Object record in the original store and no links to the file).
    // This must be done after scanning the object store.
    let layer_set = store.tree().layer_set();
    let mut merger = layer_set.merger();
    let mut iter = fsck.assert(
        Graveyard::iter(store.graveyard_directory_object_id(), &mut merger).await,
        FsckFatal::MalformedGraveyard,
    )?;
    while let Some(info) = iter.get() {
        match info.value() {
            ObjectValue::Some => {
                scanned.handle_graveyard_entry(info.object_id(), info.attribute_id(), true)?
            }
            ObjectValue::Trim => {
                if let Some(attribute_id) = info.attribute_id() {
                    fsck.error(FsckError::TrimValueForGraveyardAttributeEntry(
                        store_id,
                        info.object_id(),
                        attribute_id,
                    ))?
                } else {
                    scanned.handle_graveyard_entry(info.object_id(), None, false)?
                }
            }
            _ => fsck.error(FsckError::BadGraveyardValue(store_id, info.object_id()))?,
        }
        fsck.assert(iter.advance().await, FsckFatal::MalformedGraveyard)?;
    }

    scan_extents_and_directory_children(store, &mut scanned, result).await?;

    // At this point, we've provided all of the inputs to |scanned|.

    // Mark all the root directories as visited so that cycle detection below works.
    for oid in scanned.root_objects {
        if let Some(ScannedObject::Directory(ScannedDir { visited, .. })) =
            scanned.objects.get_mut(&oid)
        {
            *visited.get_mut() = true;
        }
    }

    // Iterate over all objects performing checks we were unable to perform earlier.
    let mut num_objects = 0;
    let mut files = 0;
    let mut directories = 0;
    let mut symlinks = 0;
    let mut tombstones = 0;
    let mut other = 0;
    let mut stack = Vec::new();
    for (object_id, object) in &scanned.objects {
        num_objects += 1;
        match object {
            ScannedObject::File(ScannedFile { parents, stored_refs, attributes, .. }) => {
                files += 1;
                let observed_refs = parents.len().try_into().unwrap();
                // observed_refs == 0 is handled separately to distinguish orphaned objects
                if observed_refs != *stored_refs && observed_refs > 0 {
                    fsck.error(FsckError::RefCountMismatch(
                        *object_id,
                        observed_refs,
                        *stored_refs,
                    ))?;
                }
                validate_attributes(fsck, store_id, *object_id, attributes, true)?;
                if parents.is_empty() {
                    fsck.warning(FsckWarning::OrphanedObject(store_id, *object_id))?;
                }
                // cladefs: a root object may be one several times (a layer shared by snapshots),
                // but must have no other parents.
                if parents.contains(&INVALID_OBJECT_ID)
                    && parents.iter().any(|oid| *oid != INVALID_OBJECT_ID)
                {
                    let parents = parents
                        .iter()
                        .filter(|oid| **oid != INVALID_OBJECT_ID)
                        .cloned()
                        .collect::<Vec<u64>>();
                    fsck.error(FsckError::ZombieFile(store_id, *object_id, parents))?;
                }
            }
            ScannedObject::Directory(ScannedDir {
                stored_sub_dirs,
                observed_sub_dirs,
                parent,
                visited,
                attributes,
                ..
            }) => {
                directories += 1;
                if *observed_sub_dirs != *stored_sub_dirs {
                    fsck.error(FsckError::SubDirCountMismatch(
                        store_id,
                        *object_id,
                        *observed_sub_dirs,
                        *stored_sub_dirs,
                    ))?;
                }
                validate_attributes(fsck, store_id, *object_id, attributes, false)?;
                if let &Some(mut oid) = parent {
                    if attributes.in_graveyard {
                        fsck.error(FsckError::ZombieDir(store_id, *object_id, oid))?;
                    }
                    // Check this directory is attached to a root object.
                    // SAFETY: This is safe because here and below are the only places that we
                    // manipulate `visited`.
                    if !std::mem::replace(unsafe { &mut *visited.get() }, true) {
                        stack.push(*object_id);
                        loop {
                            if let Some(ScannedObject::Directory(ScannedDir {
                                parent: Some(parent),
                                visited,
                                ..
                            })) = scanned.objects.get(&oid)
                            {
                                stack.push(oid);
                                oid = *parent;
                                // SAFETY: See above.
                                if std::mem::replace(unsafe { &mut *visited.get() }, true) {
                                    break;
                                }
                            } else {
                                // This indicates an error (e.g. missing parent), but they should be
                                // reported elsewhere.
                                break;
                            }
                        }
                        // Check that the object we got to isn't one in our stack which would
                        // indicate a cycle.
                        for s in stack.drain(..) {
                            if s == oid {
                                fsck.error(FsckError::LinkCycle(store_id, oid))?;
                                break;
                            }
                        }
                    }
                } else if !attributes.in_graveyard {
                    fsck.warning(FsckWarning::OrphanedObject(store_id, *object_id))?;
                }
            }
            ScannedObject::Graveyard => other += 1,
            ScannedObject::Symlink(ScannedSymlink { parents, stored_refs, attributes, .. }) => {
                symlinks += 1;
                let observed_refs = parents.len().try_into().unwrap();
                // observed_refs == 0 is handled separately to distinguish orphaned objects
                if observed_refs != *stored_refs && observed_refs > 0 {
                    fsck.error(FsckError::RefCountMismatch(
                        *object_id,
                        observed_refs,
                        *stored_refs,
                    ))?;
                }
                validate_attributes(fsck, store_id, *object_id, attributes, false)?;
                if attributes.in_graveyard {
                    if !parents.is_empty() {
                        fsck.error(FsckError::ZombieSymlink(
                            store_id,
                            *object_id,
                            parents.clone(),
                        ))?;
                    }
                } else if parents.is_empty() {
                    fsck.warning(FsckWarning::OrphanedObject(store_id, *object_id))?;
                }
            }
            ScannedObject::Tombstone => {
                tombstones += 1;
                num_objects -= 1;
            }
        }
    }
    if num_objects != store.object_count() {
        fsck.error(FsckError::ObjectCountMismatch(store_id, num_objects, store.object_count()))?;
    }
    let last_object_id = store.last_object_id();
    if last_object_id < highest_object_id {
        fsck.error(FsckError::BadLastObjectId(highest_object_id, last_object_id))?;
    }
    fsck.verbose(format!(
        "Store {store_id} has {files} files, {directories} dirs, {symlinks} symlinks, \
         {tombstones} tombstones, {other} other objects",
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::max_extent_end;
    use storage_units::BlockSize;

    /// cladefs: a file's size is what an image says.
    #[test]
    fn test_max_extent_end_of_any_size() {
        assert_eq!(max_extent_end(BlockSize::SIZE_4KIB, u64::MAX), u64::MAX);
        assert_eq!(max_extent_end(BlockSize::SIZE_4KIB, 1), 64 << 10);
    }
}
