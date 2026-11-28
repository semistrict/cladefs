// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM's memory served as Firecracker's `Mappings` memory backend (board 14630,
//! docs/research/guest-ram-mapping-list.md): the daemon tells the VM process which mappings make
//! its guest memory, and serves the faults on them itself.
//!
//! Its memory cell's image is guest memory: each run of data its extent's pages, of the daemon's
//! arena, which the VM process maps private, so that every VM mapping the same pages shares them
//! until it stores to one, which the kernel copies; each hole anonymous memory. Guest memory is
//! listed lazy at first: the VM's first touch of an image node's range faults here, which reads
//! that node of the image's tree, places its extents' parts (`CachedExtent::place`: paged in at
//! one place while a list names them, kept through evictions, board 14629), maps the node's range
//! anew, and installs its pages ahead of the guest's touches. A page installed is the arena's,
//! read-only (`UFFDIO_CONTINUE`), if its part is where its entry maps it, else a copy of its bytes
//! (`UFFDIO_COPY`), decided and made under the part's lock.
//!
//! A VM's memory is served on threads of its own, which block on its faults and its socket: one
//! reading its faults and serving them, one reading its socket. The VM process going ends them,
//! and its placements.

mod handler;
mod protocol;
mod uffd;

pub use handler::serve;

#[cfg(test)]
mod tests;
