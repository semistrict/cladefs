// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// cladefs's own: the layers a volume had of a base, a volume another filesystem wrote, which shared
// volumes made (removed, board 14527). No code reads them: these types are what `StoreInfo` still
// stores, a frozen format (doc-28, board 10050), so that every store's information keeps decoding;
// `ObjectStore::open` refuses a store that names any.

use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use storage_device::ExtentId;

/// A layer file of a base: its size, and where its bytes are.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TypeFingerprint)]
pub struct BaseLayerV61 {
    pub size: u64,
    /// In order, leaving out what reads as zeros.
    pub extents: Vec<BaseExtentV61>,
}

/// The bytes `offset..offset + length` of a base layer, `extent_offset` bytes into `extent`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TypeFingerprint)]
pub struct BaseExtentV61 {
    pub offset: u64,
    pub length: u64,
    pub extent: ExtentId,
    pub extent_offset: u64,
}
