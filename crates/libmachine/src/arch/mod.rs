// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Architecture-specific logic that does not depend on a hypervisor backend.

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;
