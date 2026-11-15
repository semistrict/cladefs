// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax's own: OpenBao's Transit engine as a [`super::Kms`] (TASK-411.16): not built yet.

/// The KMS `url` names ([`super::from_url`]): refused until this backend is built.
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    Err(format!("{url:?}: OpenBao's Transit engine is not built yet (TASK-411.16)"))
}
