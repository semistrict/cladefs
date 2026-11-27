// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Architecture-specific logic that does not depend on the hypervisor.

pub(crate) mod aarch64;

/// The machine's guest physical address map: what code shared by the board's devices (PCI,
/// memory) finds its windows by.
pub(crate) use aarch64::layout;
