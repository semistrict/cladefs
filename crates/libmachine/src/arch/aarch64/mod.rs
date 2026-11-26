// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The arm64 board libmachine boots Linux on (Hypervisor.framework, board 13063).

pub(crate) mod boot;
pub(crate) mod fdt;
pub(crate) mod layout;
