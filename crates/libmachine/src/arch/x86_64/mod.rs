// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! x86_64: CPUID templates, the PC memory layout, ACPI tables and Linux boot.

pub(crate) mod acpi;
pub(crate) mod boot;
pub(crate) mod cpuid;
pub(crate) mod gdt;
pub(crate) mod layout;
