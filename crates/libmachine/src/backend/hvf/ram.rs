// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest RAM whose host memory its caller fills (board 13994): a memory cell's window, cladefs's
//! `MemoryCell::window_in_ram`, maps its pages there before a machine is restored onto it
//! ([`Machine::restore_onto`](crate::Machine::restore_onto)), and maps them anew as the cell
//! changes while the machine runs, each change followed by a [`rebind`](GuestRam::rebind):
//! Hypervisor.framework binds a guest page to the host's page as it maps it, and follows no later
//! change of the host's mapping (`coherence_tests`). While a range is mapped anew, the guest's
//! accesses there exit, and wait for it before they are retried ([`GuestRam::wait`]).

use std::ptr::NonNull;
use std::sync::{OnceLock, RwLock};

use super::check;
use super::ffi::*;
use crate::error::{Error, Result};

/// What the RAM's owner does with pages the guest gives back (the balloon's): `len` bytes at
/// `offset` in the RAM, whole host pages.
type GivenBack = Box<dyn Fn(u64, u64) -> std::result::Result<(), String> + Send + Sync>;

/// Guest RAM of host memory its caller fills (see the module docs): a reservation of the RAM's
/// size, anonymous zeros until something maps over it.
pub struct GuestRam {
    host: NonNull<u8>,
    size: usize,
    /// The guest address the VM maps the RAM at, while it does: what a rebind maps anew, held
    /// for writing meanwhile, which the guest's accesses wait for.
    bound: RwLock<Option<u64>>,
    /// What takes pages the guest gives back, if anything does: the RAM's own memory is freed
    /// otherwise, its pages the anonymous zeros again.
    given_back: OnceLock<GivenBack>,
}

// SAFETY: A reservation of this process's memory, which the guest and its owner's window access
// as the machine does; its own state is behind locks.
unsafe impl Send for GuestRam {}
// SAFETY: As above.
unsafe impl Sync for GuestRam {}

impl std::fmt::Debug for GuestRam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestRam").field("host", &self.host).field("size", &self.size).finish()
    }
}

impl GuestRam {
    /// A reservation of `size` bytes, whole host pages, for a machine's RAM.
    pub fn reserve(size: usize) -> Result<std::sync::Arc<Self>> {
        let page = crate::sys::page_size();
        if size == 0 || !size.is_multiple_of(page) {
            return Err(Error::MemoryAllocation(format!(
                "RAM of {size} bytes, not whole {page}-byte host pages"
            )));
        }
        // SAFETY: New anonymous memory, reserving none, which the RAM owns; checked.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if host == libc::MAP_FAILED {
            return Err(Error::MemoryAllocation(format!(
                "reserving {size} bytes of RAM: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(std::sync::Arc::new(Self {
            host: NonNull::new(host.cast()).expect("mmap's address"),
            size,
            bound: RwLock::new(None),
            given_back: OnceLock::new(),
        }))
    }

    /// The RAM's host memory.
    pub fn host(&self) -> NonNull<u8> {
        self.host
    }

    /// The RAM's bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Has `taken` take the pages the guest gives back (its balloon's), rather than libmachine
    /// freeing their memory: a memory cell's window maps the cell's pages there again. Once; a
    /// later call is ignored. A failure is the guest's memory unlike what it gave: the machine
    /// stops.
    pub fn on_given_back(
        &self,
        taken: impl Fn(u64, u64) -> std::result::Result<(), String> + Send + Sync + 'static,
    ) {
        let _ = self.given_back.set(Box::new(taken));
    }

    /// What takes the pages the guest gives back, if anything does.
    pub(crate) fn given_back(&self) -> Option<&GivenBack> {
        self.given_back.get()
    }

    /// Maps the RAM into the VM at `guest_addr`, which it follows from then on: rebinds map it
    /// anew there.
    pub(crate) fn bind(&self, guest_addr: u64) -> Result<()> {
        let mut bound = self.bound.write().unwrap_or_else(|e| e.into_inner());
        // SAFETY: The RAM's reservation, mapped as long as the RAM, which the VM holds.
        check("map guest RAM", unsafe {
            hv_vm_map(
                self.host.as_ptr().cast(),
                guest_addr,
                self.size,
                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
            )
        })?;
        *bound = Some(guest_addr);
        Ok(())
    }

    /// Lets go of the VM, which goes, taking the RAM's mapping with it.
    pub(crate) fn unbind(&self) {
        *self.bound.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether `guest_addr` is the RAM's, in the VM it is bound to.
    pub(crate) fn holds(&self, guest_addr: u64) -> bool {
        let bound = self.bound.read().unwrap_or_else(|e| e.into_inner());
        bound.is_some_and(|start| (start..start + self.size as u64).contains(&guest_addr))
    }

    /// Waits for a rebind under way, if any: a guest's access that exited as its range was being
    /// mapped anew may be retried after.
    pub(crate) fn wait(&self) {
        drop(self.bound.read().unwrap_or_else(|e| e.into_inner()));
    }
}

impl cladefs::GuestRam for GuestRam {
    fn host(&self) -> NonNull<u8> {
        self.host
    }

    fn size(&self) -> u64 {
        self.size as u64
    }

    fn rebind(&self, offset: u64, len: u64) {
        let bound = self.bound.write().unwrap_or_else(|e| e.into_inner());
        let Some(start) = *bound else {
            return;
        };
        assert!(offset + len <= self.size as u64, "{len:#x} bytes at {offset:#x} past the RAM");
        // SAFETY: Within the RAM's reservation, which the VM maps at `start` while bound: its
        // pages mapped anew from what is there now, the guest's accesses waiting meanwhile.
        let ret = unsafe {
            let unmapped = hv_vm_unmap(start + offset, len as usize);
            match unmapped {
                HV_SUCCESS => hv_vm_map(
                    self.host.as_ptr().add(offset as usize).cast(),
                    start + offset,
                    len as usize,
                    HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
                ),
                refused => refused,
            }
        };
        if let Err(err) = check("map guest RAM anew", ret) {
            panic!("{len:#x} bytes of RAM at {offset:#x}: {err}");
        }
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: The reservation's own mapping, which no VM maps any more: a VM holds its RAM.
        unsafe { libc::munmap(self.host.as_ptr().cast(), self.size) };
    }
}
