// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Architecture-specific logic that does not depend on a hypervisor backend.

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

/// The machine's guest physical address map: what code shared by both boards (PCI, memory)
/// finds its windows by.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::layout;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::layout;
