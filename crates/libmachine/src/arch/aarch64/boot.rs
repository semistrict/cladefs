// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Booting an arm64 Linux kernel, as its boot protocol says
//! (Documentation/arch/arm64/booting.rst): the `Image` at a 2 MiB-aligned address plus its
//! `text_offset`; the device tree in RAM, 8-byte aligned, not crossing a 2 MiB boundary; the
//! initramfs anywhere in RAM; the boot vCPU entered at the image's start with x0 the device
//! tree's address and x1 to x3 zero, at EL1 with its MMU off and every interrupt masked.

use std::path::Path;

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::layout::RAM_ADDR;
use crate::error::{Error, Result};

/// The arm64 Image header's magic, "ARM\x64", at byte 56.
const IMAGE_MAGIC: u32 = 0x644d_5241;
/// The `text_offset` a kernel whose header leaves `image_size` zero expects (before 3.17).
const OLD_TEXT_OFFSET: u64 = 0x8_0000;
/// The most a device tree may take: it must not cross a 2 MiB boundary, and gets the last
/// 2 MiB of RAM.
pub(crate) const FDT_MAX: u64 = 2 << 20;

/// A vCPU's state at a kernel's entry: its PC, and x0 to x3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootCpu {
    pub(crate) pc: u64,
    pub(crate) x: [u64; 4],
}

/// Where a kernel's pieces went in guest memory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loaded {
    pub(crate) entry: u64,
    /// The initramfs: (start, end).
    pub(crate) initrd: Option<(u64, u64)>,
    /// Where the device tree goes.
    pub(crate) fdt: u64,
}

/// A kernel or initramfs that cannot be booted: [`Error::Boot`] (an `InvalidConfig` here had
/// machined call a bad kernel a bad configuration).
fn boot_err(msg: String) -> Error {
    Error::Boot(msg)
}

/// Loads `kernel` (an arm64 `Image`) and `initrd` into the RAM of a machine with `mem_size`
/// bytes of it: the kernel at RAM's start plus its `text_offset`, the device tree's place in
/// the last 2 MiB, the initramfs below it.
pub(crate) fn load(
    mem: &GuestMemoryMmap,
    mem_size: u64,
    kernel: &Path,
    initrd: Option<&Path>,
) -> Result<Loaded> {
    let image = std::fs::read(kernel)
        .map_err(|e| boot_err(format!("{}: cannot read the kernel: {e}", kernel.display())))?;
    let field = |at: usize| u64::from_le_bytes(image[at..at + 8].try_into().expect("8 bytes"));
    if image.len() < 64
        || u32::from_le_bytes(image[56..60].try_into().expect("4 bytes")) != IMAGE_MAGIC
    {
        return Err(boot_err(format!(
            "{}: not an arm64 Image (no \"ARM\\x64\" magic in its header)",
            kernel.display()
        )));
    }
    let (text_offset, image_size) = match field(16) {
        0 => (OLD_TEXT_OFFSET, image.len() as u64),
        size => (field(8), size),
    };
    if mem_size < 2 * FDT_MAX {
        return Err(boot_err(format!("{mem_size} bytes of RAM is too little to boot")));
    }
    let entry = RAM_ADDR + text_offset;
    // The last 2 MiB of RAM, which RAM's size (a page multiple) may leave unaligned.
    let fdt = (RAM_ADDR + mem_size - FDT_MAX) & !(FDT_MAX - 1);
    // image_size covers the kernel's BSS too, which it needs free behind it.
    let kernel_end = entry + image_size;
    if kernel_end > fdt {
        return Err(boot_err(format!(
            "a {image_size}-byte kernel does not fit in {mem_size} bytes of RAM"
        )));
    }
    mem.write_slice(&image, GuestAddress(entry))?;

    let initrd = match initrd {
        None => None,
        Some(path) => {
            let data = std::fs::read(path).map_err(|e| {
                boot_err(format!("{}: cannot read the initrd: {e}", path.display()))
            })?;
            let size = data.len() as u64;
            // Below the device tree, 64 KiB-aligned (any host page), clear of the kernel.
            let start = fdt.checked_sub(size).map(|s| s & !0xffff).filter(|&s| s >= kernel_end);
            let Some(start) = start else {
                return Err(boot_err(format!(
                    "a {size}-byte initrd does not fit in RAM beside the kernel"
                )));
            };
            mem.write_slice(&data, GuestAddress(start))?;
            Some((start, start + size))
        }
    };
    Ok(Loaded { entry, initrd, fdt })
}

/// The boot vCPU's state for a kernel `loaded`.
pub(crate) fn boot_cpu(loaded: &Loaded) -> BootCpu {
    BootCpu { pc: loaded.entry, x: [loaded.fdt, 0, 0, 0] }
}
