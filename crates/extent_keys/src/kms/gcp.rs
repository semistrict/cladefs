// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: Google Cloud KMS as a [`super::Kms`] (TASK-411.18): not built yet.

/// The KMS `url` names ([`super::from_url`]): refused until this backend is built.
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    Err(format!("{url:?}: Google Cloud KMS is not built yet (TASK-411.18)"))
}
