// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: an extent store on local disk, as backlog doc-4's host keeps extents
//! (TASK-26.10), behind the device Fxfs writes its filesystem to: cladefs's images, and the
//! extent-store device of a VM's host.
//!
//! Extents are written once: from their first write until the next commit, which seals them. They
//! are freed whole. A cell holds a filesystem, which mints its extents' IDs under its cell's ID; it
//! may hold extents of other cells' too, whose data is in the bucket, as a fork's
//! inherited ones are (doc-7). cladefs: forks and deletion are the bucket's (doc-14,
//! TASK-165.7): the cell makes nothing of them but the commit that names a version
//! ([`LocalCell::commit_version`]). A cell directory holds:
//!
//! - `store`, its description: its format, block size, cell, and the bucket its cell is kept
//!   in;
//! - `segments/<cell>/<n>`: the cell's extents' data, in segments (doc-7, Segments): open extents'
//!   pieces, as they were written, and sealed extents in one 4 KiB-aligned run each. A segment is
//!   written raw while it is open, in `open-segments/<cell>/<n>`. It closes at a commit once no
//!   write is in progress in it, when it holds about `Options::segment_size`, when it is
//!   `Options::segment_age` old, when asked to ([`LocalCell::close_segment`]), and at a fork; then
//!   it is framed as objects in object storage are (`extent_frame`), its data in parts compressed
//!   with zstd, with its footer: what the commits since it opened changed. cladefs: framed on a
//!   thread of its own, which the commit's flush waits for, not the commit, nor the writes behind
//!   it; read from its raw file until then. Its raw file goes once that commit is local. cladefs:
//!   one compressing would save less than an eighth of is framed in place, its parts stored raw,
//!   its raw file, its tail appended, linked as its framed file: its data written once (TASK-310).
//!   Between commits, full, holding a segment and an extent (`Options::max_extent`), it takes no
//!   more writes, which go to the next, so that no segment holds more, however much is written
//!   between commits (TASK-79.2); cladefs: and it waits for the next commit, which seals in place
//!   each extent one run in it, as in the open segment, and closes it, its footer naming those runs
//!   as the commit's, which the segments after it go on with, the open one ending it: a commit
//!   spanning segments (TASK-92.41). The commit copies what is not one run where it is into the
//!   open segment, filling it in turn past its room. Before, a segment filled was closed, or set
//!   aside, holding no commit, and the commit copied every extent written since out of it, into
//!   files of their own past the open segment's room;
//! - `extents/<cell>/<sequence>`: the cell's extents sealed at `Options::large_extent` or more,
//!   in files of their own, framed so; written raw in `open-extents/<cell>/<sequence>` until
//!   sealed, if large from their first write. cladefs: one none of whose parts compresses is framed
//!   in place, its raw file renamed its framed file, its tail appended: its data written once
//!   (TASK-310);
//! - `logs/<cell>/<n>`: each segment's log of the commits made in it, a record each, appended as
//!   they are made local, until it is framed and they are; and `wal`, its write-ahead log's
//!   start, the cell's commit its first segment goes on from (TASK-165.9: the `wal` module has
//!   the write-ahead log).
//!
//! A cell is opened by one process at a time ([`Dir::take`]), and once in it. Cells in
//! directories of the host's are made, opened and deleted with [`create_host`], [`open_host`] and
//! [`delete_host`], which removes the directory whole, once what else keeps the cell, as a
//! bucket does, deleted it.
//!
//! **Commits.** A commit is a barrier: it seals the extents written before it, copying each whose
//! pieces are not one run in the open segment, cladefs: or in a segment filled since the last
//! commit, into one there, or into a file of its own if it is large, and orders what came before it
//! before what comes after. Its level follows ([`LocalCell::flush`], or [`LocalCell::commit`],
//! which waits for it): commits become local in order, several at once, which syncs the data
//! written since the last flush, and the directories whose entries changed, then appends the
//! commits' records to the logs of the segments they were made in and syncs those. Only then do the
//! files they made unreferenced go.
//!
//! **Recovery.** Opening a cell recovers its write-ahead log from its directory (the `wal`
//! module): the segments framed whole, and those a crash left open or torn closed again from their
//! raw data and the records of their logs that are whole, which are the commits made local, at
//! least. Its cell is the bucket's, recovered by its uploader, after whose end the log is put
//! by its commits ([`Recovering::open`], `bucket_cell::Wal::rebase`); the cell's state is the
//! cell's and its log's segments' after it. cladefs: a closed segment's raw file goes only once
//! the commits its footer names are local, so that one closed by a commit not local has it
//! still, holding what the local commits wrote, whatever its framed file holds (TASK-79.33). So
//! a crash at any point leaves each cell as of a prefix of its commits, one at least as late as
//! the last made local.
//!
//! **Footers.** A closed segment's footer is the one segments in object storage have
//! (`extent_frame::segment`), so that a closed segment is the object its cell keeps there, byte for
//! byte (doc-7, decision 1). Given a writer's chain by an uploader ([`Recovering::open`]), the
//! cell's footers are the writer's (`extent_frame::segment::Chain`): numbered on from where the
//! chain is, each following the one before, their commits tagged with the writer's epoch, their
//! extents of their own at the writer's keys; and it keeps each closed segment for the uploader
//! until it is acknowledged in the bucket ([`LocalCell::closed`], [`LocalCell::acknowledged`]). The
//! uploader may have it close its open segment between commits ([`LocalCell::close_now`]): cladefs:
//! framed holding none of the cell's locks, as a commit's closing is, which the cell's closing and
//! forks wait for.
//!
//! **The bucket.** cladefs: once its writer acknowledges a segment in the bucket
//! ([`LocalCell::acknowledged`], TASK-165.8), the extents sealed in it, and those stored on their
//! own its footer names, held or kept, are the bucket's: read there ([`BucketData`]), and their
//! files leave the cell, into the host's cache of the bucket's objects if the uploader gave one,
//! the segment's once no write is in progress in it. Locally: the write-ahead log's start records
//! it, after the commits local so far. So the cell's files are what is not acknowledged yet, and
//! the open segment: what the upload lag bound bounds, with the segment's room, and the cache's
//! capacity. Nothing cleans the cell's segments here: what no extent is in leaves with them, the
//! bucket's collector's to take back. What writes since the last commit wasted, writing over what
//! they wrote, or writing extents freed before the commit, counts as used once it is past the
//! capacity's worth, which the commit takes back: a writer's that never commits. What the commits
//! leave freed takes none of the cell's room, however much, which the writer could neither make
//! room for nor know of.
//!
//! **Reading.** Reads of open extents read their raw files, which no CRC covers; reads of sealed
//! ones read the parts their bytes are in, whole, checked against their CRCs, and decompress those
//! stored compressed. The parts unpacked lately are kept, as reads are mostly of their
//! neighbours'. [`LocalCell::locate`] says where bytes are for a reader that reads files itself
//! ([`Located`]).
//!
//! The cell works in a [`Dir`]: a directory of the host's ([`HostDir`]), or of memory
//! ([`MemDir`]), which simulates crashes for tests.
//!
//! **Concurrency tests.** With the `shuttle` feature the cell's locks are Shuttle's, whose
//! scheduler chooses every interleaving of its writers, closer, flusher and uploader; and the
//! rules its concurrency relies on ([`Rules`]) can be turned off to see the tests fail
//! (tests/shuttle/).

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

/// cladefs: a step of a protocol, which the failpoint of its name fails (the `fail` crate's,
/// TASK-44.10): the function it is in returns an error there, or whatever a test set the failpoint
/// to do happens. Nothing without the crate's `failpoints` feature, which `cladefs/failpoints`
/// turns on. cladefs: marks a wait on another task's or thread's work, which the daemon's paths
/// must not reach under madsim, whose one thread it would stop for good (doc-24, TASK-685.3): under
/// `--cfg madsim`, fails at once, naming `$what`, so that a path not async yet is found the moment
/// a simulation reaches it, and the rewrite's work left is what the simulation finds; built
/// normally, nothing.
#[macro_export]
macro_rules! blocks {
    ($what:expr) => {
        if cfg!(madsim) {
            panic!("would block the simulation's one thread: {} (doc-24, TASK-685.3)", $what);
        }
    };
}

macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(anyhow::anyhow!("failpoint {}", $name)))
    };
}

// cladefs: the audit, a test's, recounts the cell's state after each of its critical sections:
// ~8% of a one-row SQLite commit's CPU (TASK-301). A build without debug assertions is the release
// profile's, an optimized build's, which measurements are made with, and tests are not (the dev
// and sim profiles have them): so none has it. Built across the workspace, a binary or an example
// has it from this crate's own dev-dependency on itself, which cargo unifies with every member's:
// build its package alone (`cargo build --release -p PACKAGE`), as example runs on GCE had not, and
// measured every example with it (TASK-303).
#[cfg(all(feature = "audit", not(debug_assertions)))]
compile_error!(
    "local_cell's `audit` in a release build, which measures with it: build the package alone \
     (`-p PACKAGE`), not across the workspace, whose dev-dependencies turn it on (TASK-303)"
);
// cladefs: and its `testing`, tests' panics holding a lock shared beyond one cell: each such lock,
// as it is taken, then takes one process-wide lock to see whether a test asked
// (shared::testing::panic_if_asked). cladefs's tests' dev-dependency turns it on, and so, in its
// package, cladefs's examples had it, until they were cladefs-examples' (TASK-320).
#[cfg(all(feature = "testing", not(debug_assertions)))]
compile_error!(
    "local_cell's `testing` in a release build, which measures with it: build cladefs's examples \
     as cladefs-examples' (`-p cladefs-examples`), a package alone (`-p PACKAGE`), not across the \
     workspace, whose dev-dependencies turn it on (TASK-320)"
);

mod cached;
mod dir;
mod durability;
mod extents;
pub mod fast_wal;
#[cfg(not(madsim))]
pub use durability::durability_lags;
pub use durability::{LAG_BUCKETS, Lags, durability_lags_on};
pub use extents::{OWN_ENTRY_BYTES, Own, Sparse, own_held};
mod framed;
mod frames;
mod framing;
#[cfg(test)]
mod golden;
mod held;
pub use held::{HeldCounts, HeldParts};
mod cell;
mod deferral;
mod host;
mod kept;
#[cfg(not(madsim))]
pub use deferral::defer_durability;
pub use deferral::{Deferring, defer_durability_on};
mod making;
pub use making::Making;
mod layout;
mod mem;
mod metered;
pub use metered::{Metered, MeteredFile};
pub mod footprint;
pub mod hot_budget;
mod on_disk;
pub mod pages;
pub mod pool;
mod raw;
mod relocations;
mod seals;
mod segments;
pub mod shared;
#[cfg(test)]
mod sim;
mod state;
mod sync;
pub use sync::wait_here;
mod timings;
pub use timings::{DURABLE_PHASES, FLUSH_PHASES, FRAMING_PHASES, Phases, Timed, Timings, timings};
mod untrusted;
pub mod upload;
mod uploads;
mod wal;
mod written;
pub use written::{Written, written};

pub use bucket_cell::cell::Kind;
pub use cached::{CachedDir, CachedFile};
#[cfg(any(test, feature = "unkept"))]
pub use cell::UNKEPT_DEK;
pub use cell::{
    BucketData, DirHold, EphemeralOpening, Info, LastSequence, LocalCell, LocalCheckpoint, NoSpace,
    NotQuiescent, Opening, Options, Parent, PartlyRelocated, Recovering, Relocated, Relocating,
    SpentExtent, TooLong, TooSmall, Unaligned, Unmade, Usage, Write,
};
#[cfg(not(madsim))]
pub use dir::closed;
pub use dir::{
    Dir, DirFile, DiskDir, DiskFile, File, HostDir, HostFile, MAX_DESCRIPTION_BYTES,
    MAX_LOCAL_CHECKPOINT_BYTES, MAX_LOG_BYTES, MAX_RAW_SEGMENT_BYTES, MAX_START_BYTES, closed_on,
    unclaimed_on,
};
/// cladefs: where a cell is: its tenant's cell, and its ID there (TASK-364).
pub use extent_frame::segment::CellId;
#[cfg(not(madsim))]
pub use framed::unpacked;
pub use framed::{Aligned, Located, Packed, Unpacked, read_object, read_whole, unpacks};
pub use framing::{
    buffers_bytes as framing_buffers_bytes, evict_buffers_to as evict_framing_buffers_to,
};
pub use host::{
    Host, create_host_at_version_on, create_host_ephemeral_on, create_host_on, create_host_with_on,
    delete_host_on, deleting_host, host_bucket, host_cell, host_ephemeral, host_kind,
    host_local_checkpoint_on, host_token, host_version, left_unmade_on, made_host_on, place,
    place_host_on, remove_cell_dir_on, unmade_host_on,
};
#[cfg(not(madsim))]
pub use host::{
    create_host, create_host_at_version, create_host_ephemeral, create_host_with, delete_host,
    host_local_checkpoint, left_unmade, made_host, place_host, remove_cell_dir, unmade_host,
};
pub use layout::{DESCRIPTION, DESCRIPTION_HEADER, Description, FORMAT, Format, Place};
pub use mem::{Io, MemDir, MemDisk, MemDiskFile, MemFile};
pub use on_disk::{CLEAR_LOG_MAGIC, ClearFile, OnDisk, on_disk};
pub use raw::{RawFile, SECTOR};
pub use segments::{
    Unsectored, commits_spanning_segments, framing_reads, segment_closes, segments_framed_in_place,
};
#[cfg(feature = "audit")]
pub use state::AUDIT_FAILED;
pub use sync::Rules;
#[cfg(feature = "shuttle")]
pub use sync::{rules, set_rules};
pub use uploads::Closed;
