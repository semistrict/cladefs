// Copyright 2026 The cladefs Authors.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Tests of physical memory through its API, as its users allocate it, and of what they cannot
//! see, which the tests reach inside for: which chunk a page is of, which arena has a chunk.
//!
//! Physical memory, relief and quarantine are the process's: nextest runs each test in a process
//! of its own, which configures them as it needs.

#[cfg(all(not(feature = "shuttle"), target_os = "linux"))]
use crate::ArenaId;
use crate::CHUNK_SIZE;
#[cfg(not(feature = "shuttle"))]
use crate::{Page, page_bytes};

#[cfg(not(feature = "shuttle"))]
mod arenas;
#[cfg(not(feature = "shuttle"))]
mod config;
#[cfg(not(feature = "shuttle"))]
mod memory_out;
#[cfg(not(feature = "shuttle"))]
mod quarantine;
#[cfg(feature = "shuttle")]
mod shuttle;

/// The pages of a chunk.
#[cfg(not(feature = "shuttle"))]
fn chunk_pages() -> usize {
    CHUNK_SIZE / page_bytes()
}

/// The chunk `page` is of.
#[cfg(not(feature = "shuttle"))]
fn chunk_of(page: &Page) -> usize {
    crate::pmm::node().chunk_of(page.frame())
}

/// The arena that has chunk `chunk`, if any.
#[cfg(all(not(feature = "shuttle"), target_os = "linux"))]
fn chunk_arena(chunk: usize) -> Option<ArenaId> {
    crate::pmm::node().phys_memory().chunk_arena(chunk)
}

/// Physical memory of `chunks` chunks, for a test whose process has not used it yet.
#[cfg(feature = "shuttle")]
fn configure_chunks(chunks: usize) {
    crate::configure((chunks * CHUNK_SIZE) as u64).expect("physical memory is not in use yet");
}

/// Fills `page` with `byte`.
#[cfg(all(not(feature = "shuttle"), target_os = "linux"))]
fn fill(page: &Page, byte: u8) {
    // SAFETY: The page is the test's, a page long.
    unsafe { std::ptr::write_bytes(page.as_ptr(), byte, page_bytes()) };
}

/// The first byte of `page`.
#[cfg(all(not(feature = "shuttle"), target_os = "linux"))]
fn first_byte(page: &Page) -> u8 {
    // SAFETY: The page is the test's.
    unsafe { page.as_ptr().read() }
}
