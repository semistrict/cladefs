// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-149.17: a compromised VM process, of one tenant, checked against what must stay true of the
//! tenants beside it, as each of its sequences ends: not that the daemon only panics nothing and
//! serves on ([`super::whatever_a_vm_process_says_harms_no_other`], TASK-77), but what it serves
//! the others, and what it holds.
//!
//! Three tenants share one daemon: mallory, whose VM processes are hostile; alice, granted too,
//! whose one VM process uses the protocol as it allows, beside mallory's, its every result checked
//! against a model of its cell and window; and the default tenant's bob, whose cell waits. A case
//! is one to three of mallory's connections at once, each saying anything, of any kind, flags and
//! words, naming alice's and bob's cells, extents and cells, paths that lead out of its
//! directory, forks to their cells, transfer areas of any make, storms of
//! `UNMAP`s, acknowledgements forged, early or never sent, and hanging up as it likes, mid-request
//! or with its framing broken; or flooding the daemon with reads whose replies it leaves unread;
//! or filling the host's disk, its tenant's limited a few MiB past what it takes (TASK-94);
//! and doing what it likes to the memory it is given: shrinking, growing, punching holes in and
//! writing it, and handing it back as its transfer area.
//! Alice meanwhile writes, commits, reads, frees and maps extents, and writes to and reads from her
//! window, which copies on write, in this process. Then:
//!
//! - **Alice is served as her model says**, her commits among it, which go on succeeding
//!   (TASK-150.7); and her cell closed, which mallory names, holds what she wrote.
//! - **Mallory gets nothing of theirs**: no reply, no stage operation, names their cells or
//!   holds their bytes; it maps no arena but its tenant's and the public one, gets no other
//!   tenant's memory, and finds none of their bytes in the memory it gets, read whole.
//! - **The daemon holds no more of mallory's than the protocol's bounds say**
//!   (`server::Peer::credit` each connection), however it floods it (TASK-150.3): the process's
//!   heap, counted as it is allocated (`crate::heap::Bounded`), and a VM process past its credit
//!   let go. Nor more of the host's disk than its tenant's limit (TASK-94): its writes past it
//!   refused with `ENOSPC`, as alice's go on.
//! - **Nothing of mallory's stays, nor goes**: its every connection ends; every cell it named opens
//!   again (TASK-150.5), every one it wrote commits; no version is taken of a cell it named
//!   (TASK-150.4); nothing comes into the others' directories; nothing panics, nor crashes; bob's
//!   cell reads, writes and commits.
//!
//! What it found is pinned beside it: VM processes shrinking the memory they are given crashed the
//! daemon ([`vm_processes_shrinking_their_memory_crash_nothing`]); requests held copies of
//! their whole lengths past the connection's credit ([`what_requests_hold_is_within_the_credit`]);
//! a connection's faults paged in as many parts at once as it kept faults in flight
//! ([`page_ins_in_flight_are_bounded`], TASK-149.34), which mallory's floods of page-ins do here;
//! a tenant's connections held as many times their own rooms as it had connections
//! ([`a_tenants_in_flight_data_is_bounded`], TASK-149.35); and a tenant's VM processes filled the
//! host's disk as far as their cells' capacities let them, in as many cells as they made
//! ([`a_tenant_filling_its_disk_leaves_others_committing`], TASK-94).
//! `crates/cladefs/mutants.sh` puts back a bug of each kind it exists to find, and sees it fail.

use super::{BLOCK, LastWords, control, host, memory, read, within, write};
use crate::cells::table;
use crate::client::CellClient;
use crate::disk;
use crate::server::{QUEUED_CREDIT, Tenant, connections, serve, serve_tenant};
use crate::wire::*;
use cladefs_core::heap::{CASE, LIMIT, held, peak_while};
use cladefs_core::pages::{ArenaId, page_size};
use cladefs_core::{Errno, ExtentId};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use zerocopy::IntoBytes as _;

/// The bytes of mallory's usual window, and of its cells.
const WINDOW_BYTES: u64 = 1 << 20;
const CELL_BYTES: u64 = 1 << 24;
/// Alice's window, of pages, and the most of them private at once.
const ALICE_PAGES: usize = 32;
const ALICE_PRIVATE: usize = 6;
/// What every 16 bytes alice and bob write begin with, which no byte mallory gets may hold.
const ALICE_MARK: [u8; 8] = *b"ALICE\xa1\x1c\xe5";
const BOB_MARK: [u8; 8] = *b"BOB\xb0\xb0\xb0\xb0\x0b";
/// The ID of the request that ends what a connection of mallory's says, which it waits for.
const LAST: u64 = u64::MAX - 1;
/// The extents of the version mallory's VM manager made for it, a MiB each.
const GIVEN_EXTENTS: u64 = 12;
/// TASK-343: the bytes of the filesystem's blocks each file under `dir` takes, by path, as
/// `disk::count` counts them.
fn files_taking(dir: &Path) -> BTreeMap<PathBuf, u64> {
    use std::os::unix::fs::MetadataExt as _;
    let mut files = BTreeMap::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs.push(entry.path());
            } else {
                files.insert(entry.path(), meta.blocks() * 512);
            }
        }
    }
    files
}

/// TASK-343: the files of `after` that grew past `before`, or are new, largest growth first, with
/// their bytes: what a case's disk check names as taking what it took.
fn grown_since(before: &BTreeMap<PathBuf, u64>, after: &BTreeMap<PathBuf, u64>) -> String {
    let mut grown: Vec<(u64, &PathBuf, u64)> = (after.iter())
        .filter_map(|(path, &bytes)| {
            let was = before.get(path).copied().unwrap_or(0);
            (bytes > was).then(|| (bytes - was, path, bytes))
        })
        .collect();
    grown.sort_by(|a, b| b.0.cmp(&a.0));
    let gone: u64 = (before.iter())
        .filter(|(path, _)| !after.contains_key(*path))
        .map(|(_, bytes)| bytes)
        .sum();
    let listed: Vec<String> = (grown.iter().take(24))
        .map(|(by, path, bytes)| format!("{} +{by} ({bytes})", path.display()))
        .collect();
    format!("{} grown, {gone} bytes gone: {}", grown.len(), listed.join(", "))
}

/// The extent a flood reads, its own, and how much of it each read asks for.
const FLOOD_EXTENT: u64 = 0xf100d;
/// The first of the extents mallory fills its disk with (TASK-94).
const FILL_EXTENT: u64 = 0xf111_0000;
/// What mallory's disk may take, each case, past what it took as the case began (TASK-94): less
/// than a cell's capacity, so that its fills find its disk's limit before their cell's.
const FILL_ROOM: u64 = 4 << 20;

/// Mallory's tenant's ID, and alice's.
const MALLORY: u64 = 1;
const ALICE: u64 = 2;
const FLOOD_READ: u64 = 1 << 20;
/// What a case's heap may hold past what mallory's connections' rooms do: alice's, mallory's cells
/// as they open, and the bucket in memory taking what they upload.
const SLACK: u64 = 256 << 20;
/// How long anything may take before the case fails: what would otherwise hang for good.
const WAIT: Duration = Duration::from_secs(60);

/// What the daemon holds at most of what a VM process with a window of `window` bytes keeps in
/// flight, as the protocol bounds it: its requests' stream data, what it queued for it besides,
/// and as many mappings as a reply to `MAPPINGS` holds of its window (TASK-79.16, TASK-79.43).
fn credit(window: u64) -> u64 {
    MAX_STREAM_BYTES + QUEUED_CREDIT + window / cladefs_core::dax::map_unit() * MAPPING_BYTES
}

/// A word of a message mallory says: as it is, or naming a cell, as the case has it.
#[derive(Clone, Copy, Debug)]
enum Word {
    Is(u64),
    /// Its own cell, as of what it was answered so far.
    Own,
    Alice,
    Bob,
    /// The cell of the version it may restore, shared.
    Given,
    /// That version, and the cell's tenant.
    GivenVersion,
    GivenTenant,
}

/// A path mallory names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// A cell of its own, of the case's: one of four, which its connections share.
    Own(u8),
    /// Alice's cell she closed, out of its directory by `..`.
    Escaped,
    /// Alice's cell she closed, through a link in its directory to hers.
    Linked,
    /// Alice's cells by their own paths: the one she closed and the one she has open.
    Alices,
    AlicesOpen,
    /// A name in alice's directory that nothing has, of the case's: by its own path, and through
    /// the link.
    AlicesNew,
    LinkedNew,
    /// Bob's cell.
    Bobs,
    Relative,
    /// Its directory itself.
    Directory,
}

/// A message's payload.
#[derive(Clone, Debug)]
enum Payload {
    Bytes(Vec<u8>),
    /// So many bytes of one.
    Filled(u8, usize),
    Path(Place),
    /// An `OPEN`'s `<path>\0<build>` payload (board 11697).
    Open(Place),
    /// A `FREE`'s extents, after the first.
    Extents(Vec<(Word, u64)>),
    /// A `MAP`'s encoded `MappingWire` payload (board 11697).
    Mapping {
        cell: Word,
        sequence: Word,
        offset: Word,
        len: Word,
        to: Word,
        zeros: bool,
    },
}

/// A transfer area mallory gives, of the bytes it says.
#[derive(Clone, Copy, Debug)]
enum Area {
    Sealed(u64),
    /// Sealed against shrinking, but pages never written (hole-checked at `TRANSFER`, board 11707).
    Sparse(u64),
    /// Not sealed against shrinking.
    Unsealed(u64),
    /// Of half the bytes it says.
    Short(u64),
    /// A pipe, no shared memory at all.
    Pipe,
    /// The memory it was given as its cell opened, its tenant's (0) or the public one (1), of
    /// the bytes it says.
    Given(u8, u64),
}

/// What mallory does to the memory it was given as its cell opened ([`Shared::memories`]), its
/// tenant's arena (0), which it may write, or the public one (1), which it may only read: what
/// the daemon maps of them it must go on reaching, whatever the VM process does.
#[derive(Clone, Copy, Debug)]
enum Abuse {
    /// Shrinks it to so many bytes.
    Shrink { which: u8, to: u64 },
    /// Grows it by a tebibyte.
    Grow { which: u8 },
    /// Punches a hole of `len` bytes at `at` in it.
    Punch { which: u8, at: u64, len: u64 },
    /// Writes its name at `at` in it.
    Scribble { which: u8, at: u64 },
}

/// What mallory does next: says something, or abuses its memory; or floods the daemon with
/// page-ins (TASK-149.34): the given version's extents, a MiB each, cold, a page of each mapped and
/// faulted at once, `times` over, as many faults in flight as it says; or fills its disk
/// (TASK-94): writes so many extents of its cell's, a MiB each, uncommitted, each once the last is
/// answered.
#[derive(Clone, Debug)]
enum Act {
    Say(Said),
    Abuse(Abuse),
    PageIns { times: u8 },
    Fill { extents: u8 },
}

/// How mallory acknowledges the stage operations it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acking {
    /// Each that asks for it, once made, as a VM process does.
    Honest,
    Never,
    /// All it will ever be sent, at the first it is.
    Early,
    /// Each as it comes, past it, and before it again.
    Forged,
}

/// The stage operation an `ACK` of mallory's acknowledges (board 8463): as it is, or the last it
/// was sent as it says it, or the one after that, which the daemon never sent yet.
#[derive(Clone, Copy, Debug)]
enum AckId {
    Is(u64),
    Last,
    PastLast,
}

impl AckId {
    /// Its ID, the last stage operation its connection was sent being `last`.
    fn id(self, last: u64) -> u64 {
        match self {
            AckId::Is(id) => id,
            AckId::Last => last,
            AckId::PastLast => last.wrapping_add(1),
        }
    }
}

/// A message mallory says, `repeat` times, each of a window's `PRIVATE`s, `UNMAP`s and `FAULT`s
/// 7 pages on from the one before: a storm.
#[derive(Clone, Debug)]
struct Said {
    kind: u32,
    flags: u32,
    /// Its ID, if it has one of its own: an `ACK`'s, of the stage operation it acknowledges.
    id: Option<AckId>,
    args: [Word; 5],
    payload: Payload,
    /// A descriptor it carries.
    area: Option<Area>,
    repeat: u16,
}

fn said(kind: u32, flags: u32, args: [Word; 5], payload: Payload) -> Said {
    Said { kind, flags, id: None, args, payload, area: None, repeat: 1 }
}

/// The cell a connection of mallory's opens first, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opens {
    Nothing,
    /// One of the case's four, which another connection may have.
    Shared(u8),
    /// Its own, of no other connection's.
    Own,
    /// One somewhere else, out of its directory, or a name there it may not open.
    Elsewhere(Place),
    /// A restore of its own, of the version its VM manager made for it, as a restored VM opens its
    /// cell, naming the version's cell.
    Restores,
    /// Two cells of its own at once, two OPENs in flight, of which a connection has one
    /// (TASK-150.5).
    Twice,
}

/// A connection of mallory's, all it does.
#[derive(Clone, Debug)]
struct Hostile {
    opens: Opens,
    /// The window it asks for first, and whether it says it copies on write
    /// ([`WINDOW_COPYING`], doc-26).
    window: (u64, bool),
    area: Option<Area>,
    acking: Acking,
    acts: Vec<Act>,
    /// Whether it waits for what it said to be answered before it goes: or hangs up mid-request.
    waits: bool,
    /// Whether it then floods the daemon with reads past its credit, their replies unread.
    flood: bool,
    last_words: LastWords,
}

/// What alice does, through her one connection, as the protocol allows: each result as her model
/// says. What it picks among her extents, and where in her window, is reduced to what she has.
#[derive(Clone, Copy, Debug)]
enum AliceOp {
    /// Writes a new extent of so many blocks.
    Write {
        blocks: u8,
    },
    Commit {
        durable: bool,
        root: bool,
    },
    /// Reads an extent she wrote, freed or not.
    Read {
        pick: usize,
        block: u8,
        blocks: u8,
    },
    /// Frees an extent sealed.
    Free {
        pick: usize,
    },
    /// Maps pages of an extent not freed, sealed or not, into her window.
    Map {
        pick: usize,
        page: u8,
        pages: u8,
        at: u8,
    },
    Zeros {
        at: u8,
        pages: u8,
    },
    Unmap {
        at: u8,
        pages: u8,
    },
    /// A page of her window mapped, as her guest reads it.
    GuestRead {
        at: u8,
    },
    /// A byte, as her guest writes it, to a page mapped, within her private pages.
    GuestWrite {
        at: u8,
        byte: u8,
    },
    Private {
        at: u8,
        pages: u8,
    },
    Root,
}

fn alice_op() -> impl Strategy<Value = AliceOp> {
    use AliceOp::*;
    prop_oneof![
        4 => (1u8..=8).prop_map(|blocks| Write { blocks }),
        2 => (any::<bool>(), any::<bool>()).prop_map(|(durable, root)| Commit { durable, root }),
        2 => (any::<usize>(), any::<u8>(), any::<u8>())
            .prop_map(|(pick, block, blocks)| Read { pick, block, blocks }),
        1 => any::<usize>().prop_map(|pick| Free { pick }),
        4 => (any::<usize>(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(pick, page, pages, at)| Map { pick, page, pages, at }),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(at, pages)| Zeros { at, pages }),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(at, pages)| Unmap { at, pages }),
        3 => any::<u8>().prop_map(|at| GuestRead { at }),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(at, byte)| GuestWrite { at, byte }),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(at, pages)| Private { at, pages }),
        1 => Just(Root),
    ]
}

fn word() -> BoxedStrategy<Word> {
    let ps = page_size();
    prop_oneof![
        Just(Word::Is(0)),
        Just(Word::Is(1)),
        Just(Word::Is(ps)),
        Just(Word::Is(WINDOW_BYTES)),
        Just(Word::Is(WINDOW_BYTES - ps)),
        Just(Word::Is(MAX_WINDOW)),
        Just(Word::Is(MAX_WINDOW + ps)),
        Just(Word::Is(MAX_PAYLOAD)),
        Just(Word::Is(MAX_TRANSFER_AREA)),
        Just(Word::Is(1 << 60)),
        Just(Word::Own),
        Just(Word::Alice),
        Just(Word::Bob),
        Just(Word::Given),
        (1u64..16).prop_map(Word::Is),
        (0u64..64).prop_map(move |page| Word::Is(page * ps)),
        // Negative, as a signed word has it.
        (0u64..4096).prop_map(|back| Word::Is(u64::MAX - back)),
        any::<u64>().prop_map(Word::Is),
    ]
    .boxed()
}

fn place() -> BoxedStrategy<Place> {
    prop_oneof![
        4 => (0u8..4).prop_map(Place::Own),
        1 => Just(Place::Escaped),
        1 => Just(Place::Linked),
        1 => Just(Place::Alices),
        1 => Just(Place::AlicesOpen),
        1 => Just(Place::AlicesNew),
        1 => Just(Place::LinkedNew),
        1 => Just(Place::Bobs),
        1 => Just(Place::Relative),
        1 => Just(Place::Directory),
    ]
    .boxed()
}

fn area() -> BoxedStrategy<Area> {
    prop_oneof![
        1 => (0u8..2, prop_oneof![Just(1u64 << 20), Just(MAX_TRANSFER_AREA)]).prop_map(|(which, len)| Area::Given(which, len)),
        4 => Just(Area::Sealed(1 << 20)),
        2 => Just(Area::Sealed(MAX_TRANSFER_AREA)),
        1 => Just(Area::Sealed(MAX_TRANSFER_AREA + page_size())),
        2 => Just(Area::Sparse(1 << 20)),
        1 => Just(Area::Unsealed(1 << 20)),
        1 => Just(Area::Short(1 << 20)),
        1 => Just(Area::Pipe),
    ]
    .boxed()
}

/// Anything mallory says.
fn message() -> BoxedStrategy<Said> {
    use Word::*;
    let ps = page_size();
    const KINDS: [u32; 34] = [
        OPEN,
        READ,
        WRITE,
        FREE,
        MAP,
        UNMAP,
        COMMIT,
        ROOT,
        INFO,
        WINDOW,
        FAULT,
        ACK,
        FORK,
        RESIZED,
        MAPPINGS,
        // A follower's, gone (board 8237).
        21,
        22,
        23,
        24,
        TRANSFER,
        // 26 (FREE_MANY), 28 (BUILD), 32 (MAP_MANY), 33 (STAGE_PRIVATE_COUNTS) retired (board 11697).
        26,
        STAGE_COUNTS,
        28,
        // `PRIVATE`'s, which its VM process serves itself now (doc-26).
        30,
        32,
        33,
        REPLY,
        STAGE_MAP,
        STAGE_UNMAP,
        // `STAGE_PRIVATE`'s.
        108,
        CAPACITY,
        // A follower's `COMMITTED`, gone (board 8237).
        105,
        29,
        0xffff_ffff,
    ];
    let page = move || (0u64..WINDOW_BYTES / ps).prop_map(move |page| Is(page * ps));
    let pages = move || (1u64..16).prop_map(move |pages| Is(pages * ps));
    // cladefs: and the last sequences, the last of which has no successor (board 2762): drawn
    // from 1 to 16 alone, no request ever named it.
    let sequence = || {
        prop_oneof![
            8 => (1u64..16).prop_map(Is),
            1 => Just(Is(u64::MAX)),
            1 => Just(Is(u64::MAX - 1)),
        ]
    };
    let theirs = || prop_oneof![Just(Alice), Just(Bob)];
    let transferred =
        || prop_oneof![Just(Is(0)), Just(Is(ps)), Just(Is(1 << 20)), Just(Is(MAX_TRANSFER_AREA))];
    // Requests of its own cell and window, as far as their extents and pages go.
    let plausible = prop_oneof![
        (sequence(), page(), 1usize..8).prop_map(move |(extent, at, count)| {
            said(
                WRITE,
                0,
                [Own, extent, at, Is(0), Is(0)],
                Payload::Filled(0x5a, count * ps as usize),
            )
        }),
        (sequence(), page(), pages()).prop_map(|(extent, at, len)| {
            said(READ, 0, [Own, extent, at, len, Is(0)], Payload::Bytes(Vec::new()))
        }),
        // Of any length, anywhere, as a commit can't write the extent left of it (TASK-150.7).
        (sequence(), prop_oneof![Just(0), Just(1), Just(511), Just(ps - 1)], 1usize..200).prop_map(
            |(extent, at, len)| {
                said(WRITE, 0, [Own, extent, Is(at), Is(0), Is(0)], Payload::Filled(0x77, len))
            }
        ),
        (sequence(), page(), pages(), page(), any::<bool>()).prop_map(
            |(extent, at, len, to, zeros)| {
                said(
                    MAP,
                    0,
                    [Is(0); 5],
                    Payload::Mapping { cell: Own, sequence: extent, offset: at, len, to, zeros },
                )
            },
        ),
        (page(), pages()).prop_map(|(at, len)| said(
            UNMAP,
            0,
            [at, len, Is(0), Is(0), Is(0)],
            Payload::Bytes(Vec::new())
        )),
        (page(), 0u32..8).prop_map(|(at, flags)| said(
            FAULT,
            flags,
            [at, Is(0), Is(0), Is(0), Is(0)],
            Payload::Bytes(Vec::new())
        )),
        sequence().prop_map(|extent| said(
            FREE,
            0,
            [Own, extent, Is(0), Is(0), Is(0)],
            Payload::Bytes(Vec::new())
        )),
        (0u32..4, any::<bool>()).prop_map(|(flags, root)| {
            let payload =
                if root && flags & COMMIT_ROOT != 0 { b"mallory".to_vec() } else { Vec::new() };
            said(COMMIT, flags, [Is(0); 5], Payload::Bytes(payload))
        }),
        Just(said(MAPPINGS, 0, [Is(0); 5], Payload::Bytes(Vec::new()))),
        Just(said(INFO, 0, [Is(0); 5], Payload::Bytes(Vec::new()))),
    ];
    // Storms of the window's own requests, and of transfers as large as its area takes. cladefs:
    // `PRIVATE`s are its VM process's own now, which bounds them (doc-26): a storm of them reaches
    // no daemon.
    let storms = prop_oneof![
        (prop_oneof![Just(UNMAP), Just(FAULT)], page(), pages(), 16u16..128).prop_map(
            |(kind, at, len, repeat)| {
                let (flags, second) = if kind == FAULT { (0, Is(0)) } else { (0, len) };
                Said {
                    repeat,
                    ..said(
                        kind,
                        flags,
                        [at, second, Is(0), Is(0), Is(0)],
                        Payload::Bytes(Vec::new()),
                    )
                }
            }
        ),
        (sequence(), transferred(), transferred(), 2u16..24).prop_map(
            |(extent, at, len, repeat)| {
                Said {
                    repeat,
                    ..said(
                        READ,
                        READ_TRANSFER,
                        [Own, extent, Is(0), len, at],
                        Payload::Bytes(Vec::new()),
                    )
                }
            }
        ),
        (sequence(), transferred(), transferred(), 2u16..24).prop_map(
            |(extent, at, len, repeat)| {
                Said {
                    repeat,
                    ..said(
                        WRITE,
                        WRITE_TRANSFER,
                        [Own, extent, Is(0), at, len],
                        Payload::Bytes(Vec::new()),
                    )
                }
            }
        ),
    ];
    // What names another tenant's: their extents, cell directories and cells.
    let reaching = prop_oneof![
        1 => (theirs(), sequence(), pages()).prop_map(|(whose, extent, len)| {
            said(READ, 0, [whose, extent, Is(0), len, Is(0)], Payload::Bytes(Vec::new()))
        }),
        1 => (theirs(), sequence(), page()).prop_map(move |(whose, extent, to)| {
            said(
                MAP,
                0,
                [Is(0); 5],
                Payload::Mapping {
                    cell: whose,
                    sequence: extent,
                    offset: Is(0),
                    len: Is(ps),
                    to,
                    zeros: false,
                },
            )
        }),
        1 => (theirs(), sequence()).prop_map(|(whose, extent)| {
            said(FREE, 0, [whose, extent, Is(0), Is(0), Is(0)], Payload::Bytes(Vec::new()))
        }),
        1 => (theirs(), sequence()).prop_map(move |(whose, extent)| {
            said(WRITE, 0, [whose, extent, Is(0), Is(0), Is(0)], Payload::Filled(0x66, ps as usize))
        }),
        1 => (theirs(), proptest::collection::vec((theirs(), 1u64..16), 0..8)).prop_map(|(whose, rest)| {
            said(FREE, 0, [whose, Is(1), Is(0), Is(0), Is(0)], Payload::Extents(rest))
        }),
        // Forks of its own cell to another's path or its own, and drops of another's version.
        1 => (0u8..4, 0u32..2)
            .prop_map(|(own, flags)| {
                said(FORK, flags, [Is(0); 5], Payload::Path(Place::Own(own)))
            }),
        1 => prop_oneof![Just(Alice), Just(Bob), Just(Given), Just(Own), Just(GivenVersion), any::<u64>().prop_map(Is)]
            .prop_map(|version| {
                said(FORK, FORK_DROP, [version, Is(0), Is(0), Is(0), Is(0)], Payload::Bytes(Vec::new()))
            }),
        // cladefs: and a follower's flag, 1 << 2, gone (board 8237).
        3 => (place(), prop_oneof![Just(0), Just(OPEN_CREATE), Just(1 << 2)]).prop_map(|(at, flags)| {
            said(OPEN, flags, [Is(CELL_BYTES), Is(0), Is(0), Is(0), Is(VERSION)], Payload::Open(at))
        }),
        // Forks of its own cell to anywhere it names.
        1 => (place(), 0u32..2).prop_map(|(to, flags)| {
            said(FORK, flags, [Is(0); 5], Payload::Path(to))
        }),
        // Restores, of versions there are or not, of any cell it names, of any tenant's.
        1 => ((0u8..4).prop_map(Place::Own), prop_oneof![Just(Alice), Just(Bob), Just(Given)], prop_oneof![Just(GivenVersion), any::<u64>().prop_map(Is)], prop_oneof![Just(GivenTenant), (0u64..4).prop_map(Is)])
            .prop_map(|(to, cell, version, tenant)| {
                said(OPEN, OPEN_FORK, [Is(CELL_BYTES), cell, version, tenant, Is(VERSION)], Payload::Open(to))
            }),
    ];
    // The protocol's own: acknowledgements forged, what its stage operations cost, its build, a
    // second window and transfer area, answers to asks never made.
    // cladefs: at the edges of what the daemon sent as often as anywhere else (board 8463): 0, 1,
    // the last sent and the one after it, past every one it will send, and any.
    let ack_id = prop_oneof![
        Just(AckId::Is(0)),
        Just(AckId::Is(1)),
        Just(AckId::Last),
        Just(AckId::PastLast),
        Just(AckId::Is(u64::MAX)),
        (1u64..1024).prop_map(AckId::Is),
        any::<u64>().prop_map(AckId::Is),
    ];
    let abusing_payload =
        prop_oneof![Just(Vec::new()), proptest::collection::vec(any::<u8>(), 1..64)];
    let abusing = prop_oneof![
        (ack_id, [word(), word(), word(), word(), word()], abusing_payload.clone()).prop_map(
            |(id, args, payload)| Said {
                id: Some(id),
                ..said(ACK, 0, args, Payload::Bytes(payload))
            }
        ),
        ([word(), word(), word(), word(), word()], abusing_payload.clone())
            .prop_map(|(args, payload)| said(STAGE_COUNTS, 0, args, Payload::Bytes(payload))),
        ([word(), word(), word(), word(), word()], abusing_payload)
            .prop_map(|(args, payload)| { said(33, 0, args, Payload::Bytes(payload)) }),
        prop_oneof![Just(b"0123abcd".to_vec()), proptest::collection::vec(any::<u8>(), 0..80)]
            .prop_map(|commit| said(28, 0, [Is(0); 5], Payload::Bytes(commit))),
        (area(), word()).prop_map(|(area, len)| Said {
            area: Some(area),
            ..said(TRANSFER, 0, [len, Is(0), Is(0), Is(0), Is(0)], Payload::Bytes(Vec::new()))
        }),
        (word(), word(), 0u32..4).prop_map(|(size, private, flags)| said(
            WINDOW,
            flags,
            [size, private, Is(0), Is(0), Is(0)],
            Payload::Bytes(Vec::new())
        )),
        (word(), 0u64..3).prop_map(|(capacity, took)| said(
            RESIZED,
            0,
            [capacity, Is(took), Is(0), Is(0), Is(0)],
            Payload::Bytes(Vec::new())
        )),
    ];
    let anything = (
        proptest::sample::select(&KINDS[..]),
        prop_oneof![0u32..32, any::<u32>()],
        [word(), word(), word(), word(), word()],
        prop_oneof![
            4 => proptest::collection::vec(any::<u8>(), 0..64).prop_map(Payload::Bytes),
            1 => place().prop_map(Payload::Path),
        ],
        proptest::option::weighted(0.05, area()),
    )
        .prop_map(|(kind, flags, args, payload, area)| Said {
            area,
            ..said(kind, flags, args, payload)
        });
    prop_oneof![10 => plausible, 2 => storms, 4 => reaching, 2 => abusing, 3 => anything].boxed()
}

/// Anything mallory does: mostly what it says.
fn act() -> impl Strategy<Value = Act> {
    let ps = page_size();
    let which = || 0u8..2;
    let at =
        move || prop_oneof![Just(0u64), (0u64..1024).prop_map(move |page| page * ps), any::<u64>()];
    let abuse = prop_oneof![
        (which(), prop_oneof![Just(0u64), Just(ps), Just(2u64 << 20)])
            .prop_map(|(which, to)| Abuse::Shrink { which, to }),
        which().prop_map(|which| Abuse::Grow { which }),
        (which(), at(), prop_oneof![Just(ps), Just(2u64 << 20), Just(1u64 << 40)])
            .prop_map(|(which, at, len)| Abuse::Punch { which, at, len }),
        (which(), at()).prop_map(|(which, at)| Abuse::Scribble { which, at }),
    ];
    prop_oneof![
        30 => message().prop_map(Act::Say),
        1 => abuse.prop_map(Act::Abuse),
        1 => (1u8..8).prop_map(|times| Act::PageIns { times }),
        1 => (1u8..=8).prop_map(|extents| Act::Fill { extents }),
    ]
}

fn hostile() -> impl Strategy<Value = Hostile> {
    let ps = page_size();
    let window = prop_oneof![4 => Just(WINDOW_BYTES), 1 => Just(MAX_WINDOW), 1 => Just(MAX_WINDOW + ps), 1 => Just(1 << 60)];
    let copying = prop_oneof![1 => Just(false), 6 => Just(true)];
    // One the daemon grants: no larger than it allows, a writer's, which copies on write.
    let granted = (prop_oneof![4 => Just(WINDOW_BYTES), 1 => Just(MAX_WINDOW)], Just(true));
    let elsewhere = prop_oneof![
        Just(Place::Escaped),
        Just(Place::Linked),
        Just(Place::Alices),
        Just(Place::AlicesOpen),
        Just(Place::AlicesNew),
        Just(Place::LinkedNew),
        Just(Place::Bobs),
        Just(Place::Relative),
        Just(Place::Directory),
    ];
    let opens = prop_oneof![
        2 => Just(Opens::Nothing),
        2 => (0u8..4).prop_map(Opens::Shared),
        2 => Just(Opens::Own),
        1 => elsewhere.prop_map(Opens::Elsewhere),
        1 => Just(Opens::Restores),
        1 => Just(Opens::Twice),
    ];
    let acking = prop_oneof![
        8 => Just(Acking::Honest),
        1 => Just(Acking::Never),
        1 => Just(Acking::Early),
        2 => Just(Acking::Forged),
    ];
    let last_words = prop_oneof![
        Just(LastWords::Nothing),
        Just(LastWords::PartOfAHeader),
        any::<u64>().prop_map(LastWords::MissingPayload),
        // cladefs: and lengths the daemon admits, as any u64 almost never is (board 2492): at and
        // around the most a payload holds, small, and any it admits (board 8459).
        prop_oneof![
            Just(MAX_PAYLOAD),
            Just(MAX_PAYLOAD + 64),
            Just(MAX_PAYLOAD + 65),
            1u64..=4096,
            1u64..=MAX_PAYLOAD,
        ]
        .prop_map(LastWords::MissingPayload),
        (1u32..4).prop_map(LastWords::MissingDescriptors),
    ];
    (
        opens,
        ((window, copying), granted),
        proptest::option::of(area()),
        acking,
        proptest::collection::vec(act(), 1..64),
        prop_oneof![3 => Just(true), 1 => Just(false)],
        proptest::bool::weighted(0.15),
        last_words,
    )
        .prop_map(
            |(opens, (window, granted), area, acking, mut acts, waits, flood, last_words)| {
                // A restore floods its version with page-ins, as a restored VM's first faults
                // do, through the window such a VM has: one granted. Refused, its MAPs would be
                // refused ENOSYS before they paged in, as those of connections opening other
                // cells are; and a run whose few restores all drew refused windows flooded
                // nothing (TASK-717).
                let window = if opens == Opens::Restores { granted } else { window };
                // A flood is of a connection that has a cell of its own and waits for its
                // answers, whose credit, of the window it asked for, is no more than a few hundred
                // mebibytes: one it doesn't change, as another window, asked for later, may; nor
                // its cell's capacity, which its fills would reach, refusing its flood's write.
                let flood = flood && opens == Opens::Own && waits && window.0 != MAX_WINDOW;
                if flood {
                    acts.retain(|act| match act {
                        Act::Say(said) => said.kind != WINDOW,
                        Act::Fill { .. } => false,
                        _ => true,
                    });
                }
                Hostile { opens, window, area, acking, acts, waits, flood, last_words }
            },
        )
}

/// The bytes alice or bob write of `tag`, `len` of them: each 16 `mark` and a word of `tag` and
/// where they are.
fn marked(mark: [u8; 8], tag: u64, len: usize) -> Vec<u8> {
    (0..len.div_ceil(16) as u64)
        .flat_map(|unit| mark.into_iter().chain((tag << 24 | unit).to_le_bytes()))
        .take(len)
        .collect()
}

/// The `len` bytes at `addr`, as the kernel reads them, faulting as a vCPU's read does: `Err` with
/// the errno it failed with, as KVM_RUN does once a fault is refused.
fn guest_read(addr: usize, len: usize) -> Result<Vec<u8>, i32> {
    let mut pipe = [0; 2];
    let mut bytes = vec![0u8; len];
    // SAFETY: Makes a pipe, writes the bytes at `addr` to it, reads them back, and closes it.
    unsafe {
        assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
        let written = libc::write(pipe[1], addr as *const libc::c_void, len);
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        let loaded = match written {
            -1 => Err(errno),
            written if written as usize == len => {
                let read = libc::read(pipe[0], bytes.as_mut_ptr().cast(), len);
                assert_eq!(read, len as isize);
                Ok(bytes)
            }
            // Part of it: never, of a page alone.
            written => Err(-(written as i32)),
        };
        libc::close(pipe[0]);
        libc::close(pipe[1]);
        loaded
    }
}

/// `byte` written at `addr`, as the kernel writes it, faulting for writing as KVM does for a vCPU:
/// `Err` with the errno it failed with.
fn guest_write(addr: usize, byte: u8) -> Result<(), i32> {
    let mut pipe = [0; 2];
    // SAFETY: Makes a pipe, writes a byte to it, and reads it into `addr`, closing it after.
    unsafe {
        assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
        assert_eq!(libc::write(pipe[1], [byte].as_ptr().cast(), 1), 1);
        let read = libc::read(pipe[0], addr as *mut libc::c_void, 1);
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap();
        libc::close(pipe[0]);
        libc::close(pipe[1]);
        if read == 1 { Ok(()) } else { Err(errno) }
    }
}

/// An extent alice wrote and hasn't freed.
struct LiveExtent {
    bytes: Vec<u8>,
    sealed: bool,
}

/// What a page of alice's window holds, as her model has it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Page {
    Unmapped,
    /// Page `page` of extent `sequence`, zeros past its end.
    Extent {
        sequence: u64,
        page: u64,
    },
    Zeros,
    Private(Vec<u8>),
}

/// Alice: a correct VM process, of a granted tenant, beside mallory's; her cell, her window in
/// this process's memory, which copies on write, and her model of both.
struct Alice {
    client: CellClient,
    window: NonNull<u8>,
    cell: u64,
    /// The sequence of her next extent.
    next: u64,
    extents: BTreeMap<u64, LiveExtent>,
    freed: BTreeSet<u64>,
    pages: Vec<Page>,
    root: Vec<u8>,
}

/// Alice's result of `op` unless it is `expected`, saying both.
fn expect<T: std::fmt::Debug + PartialEq>(
    op: impl std::fmt::Debug,
    got: T,
    expected: T,
) -> Result<(), String> {
    if got == expected {
        Ok(())
    } else {
        Err(format!("alice's {op:?}: got {got:?}, her model {expected:?}"))
    }
}

/// A short form of bytes, for saying what differed.
fn summary(bytes: &Result<Vec<u8>, impl std::fmt::Debug>) -> String {
    match bytes {
        Ok(bytes) => format!("{} bytes, {:02x?}..", bytes.len(), &bytes[..bytes.len().min(24)]),
        Err(err) => format!("{err:?}"),
    }
}

impl Alice {
    /// Alice's cell at `path`, through the daemon at `socket`, her window made, and her root
    /// committed.
    fn new(socket: &Path, path: &Path) -> Self {
        let ps = page_size() as usize;
        let client = CellClient::connect(socket, path, 1 << 27).unwrap();
        let window = memory(ALICE_PAGES);
        // SAFETY: The memory is hers, and outlives the cell, as it is never unmapped.
        unsafe {
            client.create_dax_window_in_host_memory(
                window,
                (ALICE_PAGES * ps) as u64,
                None,
                ALICE_PRIVATE as u64,
            )
        }
        .unwrap();
        cladefs_core::guest_thread_started();
        let root = marked(ALICE_MARK, u64::MAX >> 24, 64);
        client.commit(Some(&root)).unwrap();
        let cell = client.cell();
        let pages = vec![Page::Unmapped; ALICE_PAGES];
        Self {
            client,
            window,
            cell,
            next: 1,
            extents: BTreeMap::new(),
            freed: BTreeSet::new(),
            pages,
            root,
        }
    }

    fn id(&self, sequence: u64) -> ExtentId {
        ExtentId { cell: self.cell, sequence }
    }

    fn page_addr(&self, page: usize) -> usize {
        self.window.as_ptr() as usize + page * page_size() as usize
    }

    /// What page `page` of her window holds, as her model has it: none if unmapped.
    fn page_bytes(&self, page: &Page) -> Option<Vec<u8>> {
        let ps = page_size() as usize;
        match page {
            Page::Unmapped => None,
            Page::Zeros => Some(vec![0; ps]),
            Page::Private(bytes) => Some(bytes.clone()),
            Page::Extent { sequence, page } => {
                let bytes = &self.extents[sequence].bytes;
                let at = (*page as usize * ps).min(bytes.len());
                let mut held = bytes[at..bytes.len().min(at + ps)].to_vec();
                held.resize(ps, 0);
                Some(held)
            }
        }
    }

    fn private_pages(&self) -> usize {
        self.pages.iter().filter(|page| matches!(page, Page::Private(_))).count()
    }

    /// The window's pages `at`, and so many after as `pages` says, within it.
    fn range(at: u8, pages: u8) -> std::ops::Range<usize> {
        let at = at as usize % ALICE_PAGES;
        at..at + (1 + pages as usize % 4).min(ALICE_PAGES - at)
    }

    /// Does `op`, checking what it returns against her model, which it then changes as `op` does.
    fn run(&mut self, op: AliceOp) -> Result<(), String> {
        let ps = page_size() as usize;
        match op {
            AliceOp::Write { blocks } => {
                let sequence = self.next;
                self.next += 1;
                let bytes = marked(ALICE_MARK, sequence, blocks as usize * BLOCK);
                expect(op, write(&self.client, self.id(sequence), 0, &bytes), Ok(()))?;
                self.extents.insert(sequence, LiveExtent { bytes, sealed: false });
            }
            AliceOp::Commit { durable, root } => {
                let root = root.then(|| marked(ALICE_MARK, u64::MAX >> 24 ^ self.next, 64));
                let committed = match durable {
                    true => self.client.commit(root.as_deref()),
                    false => self.client.commit_ordered(root.as_deref()),
                };
                expect(op, committed, Ok(()))?;
                self.extents.values_mut().for_each(|kept| kept.sealed = true);
                if let Some(root) = root {
                    self.root = root;
                }
            }
            AliceOp::Read { pick, block, blocks } => {
                let written: Vec<u64> = self.extents.keys().chain(&self.freed).copied().collect();
                if written.is_empty() {
                    return Ok(());
                }
                let sequence = written[pick % written.len()];
                match self.extents.get(&sequence) {
                    Some(kept) => {
                        let count = kept.bytes.len() / BLOCK;
                        let block = block as usize % count;
                        let blocks = 1 + blocks as usize % (count - block);
                        let (at, len) = (block * BLOCK, blocks * BLOCK);
                        let got = read(&self.client, self.id(sequence), at as u64, len);
                        let expected = Ok(kept.bytes[at..at + len].to_vec());
                        if got != expected {
                            let (got, expected) = (summary(&got), summary(&expected));
                            return Err(format!(
                                "alice's {op:?} of {sequence}: got {got}, her model {expected}"
                            ));
                        }
                    }
                    None => {
                        let got = read(&self.client, self.id(sequence), 0, BLOCK).map(drop);
                        expect((op, sequence), got, Err(Errno::ENOENT))?;
                    }
                }
            }
            AliceOp::Free { pick } => {
                let sealed: Vec<u64> =
                    self.extents.iter().filter(|(_, kept)| kept.sealed).map(|(&s, _)| s).collect();
                if sealed.is_empty() {
                    return Ok(());
                }
                self.free(sealed[pick % sealed.len()])?;
            }
            AliceOp::Map { pick, page, pages, at } => {
                let live: Vec<u64> = self.extents.keys().copied().collect();
                if live.is_empty() {
                    return Ok(());
                }
                let sequence = live[pick % live.len()];
                let kept = &self.extents[&sequence];
                // A page past its end too, which maps zeros.
                let page = page as u64 % (kept.bytes.len().div_ceil(ps) as u64 + 1);
                let range = Self::range(at, pages);
                let (offset, len) = (page * ps as u64, (range.len() * ps) as u64);
                let mapped =
                    self.client.map(self.id(sequence), offset, len, (range.start * ps) as u64);
                if !kept.sealed {
                    return expect((op, sequence), mapped, Err(Errno::EPERM));
                }
                expect((op, sequence), mapped, Ok(()))?;
                for (index, at) in range.enumerate() {
                    self.pages[at] = Page::Extent { sequence, page: page + index as u64 };
                }
            }
            AliceOp::Zeros { at, pages } => {
                let range = Self::range(at, pages);
                let (at, len) = ((range.start * ps) as u64, (range.len() * ps) as u64);
                let mapped = futures::executor::block_on(self.client.map_zeros_async(at, len));
                expect(op, mapped, Ok(()))?;
                range.for_each(|at| self.pages[at] = Page::Zeros);
            }
            AliceOp::Unmap { at, pages } => {
                let range = Self::range(at, pages);
                let unmapped =
                    self.client.unmap((range.start * ps) as u64, (range.len() * ps) as u64);
                expect(op, unmapped, Ok(()))?;
                range.for_each(|at| self.pages[at] = Page::Unmapped);
            }
            AliceOp::GuestRead { at } => {
                // Of a page mapped: a read of one that isn't strays, which stops a VM.
                let mapped: Vec<usize> =
                    (0..ALICE_PAGES).filter(|&at| self.pages[at] != Page::Unmapped).collect();
                let Some(&at) = mapped.get(at as usize % mapped.len().max(1)) else {
                    return Ok(());
                };
                let got = guest_read(self.page_addr(at), ps);
                let expected = self.page_bytes(&self.pages[at]).ok_or(libc::EFAULT);
                if got != expected {
                    let (got, expected) = (summary(&got), summary(&expected));
                    return Err(format!(
                        "alice's {op:?} of page {at}: got {got}, her model {expected}"
                    ));
                }
            }
            AliceOp::GuestWrite { at, byte } => {
                // Of a page mapped, within her private pages: a write past them is refused, as one
                // to a page mapped nothing at, and stops a VM.
                let room = self.private_pages() < ALICE_PRIVATE;
                let storable: Vec<usize> = (0..ALICE_PAGES)
                    .filter(|&at| match self.pages[at] {
                        Page::Unmapped => false,
                        Page::Private(_) => true,
                        Page::Extent { .. } | Page::Zeros => room,
                    })
                    .collect();
                let Some(&at) = storable.get(at as usize % storable.len().max(1)) else {
                    return Ok(());
                };
                let offset = byte as usize * 61 % ps;
                let stored = guest_write(self.page_addr(at) + offset, byte);
                expect((op, at), stored, Ok(()))?;
                let mut bytes = self.page_bytes(&self.pages[at]).unwrap();
                bytes[offset] = byte;
                self.pages[at] = Page::Private(bytes);
            }
            AliceOp::Private { at, pages } => {
                let range = Self::range(at, pages);
                let made =
                    self.client.make_private((range.start * ps) as u64, (range.len() * ps) as u64);
                let needed =
                    range.clone().filter(|&at| !matches!(self.pages[at], Page::Private(_))).count();
                if range.clone().any(|at| self.pages[at] == Page::Unmapped) {
                    return expect(op, made, Err(Errno::EINVAL));
                }
                if self.private_pages() + needed > ALICE_PRIVATE {
                    return expect(op, made, Err(Errno::ENOMEM));
                }
                expect(op, made, Ok(()))?;
                for at in range {
                    self.pages[at] = Page::Private(self.page_bytes(&self.pages[at]).unwrap());
                }
            }
            AliceOp::Root => expect(op, self.client.root(), Ok(self.root.clone()))?,
        }
        Ok(())
    }

    /// Frees her extent `sequence`, sealed, whose mappings map zeros from then on.
    fn free(&mut self, sequence: u64) -> Result<(), String> {
        expect(("free", sequence), self.client.free(self.id(sequence)), Ok(()))?;
        self.extents.remove(&sequence);
        self.freed.insert(sequence);
        for page in &mut self.pages {
            if matches!(page, Page::Extent { sequence: mapped, .. } if *mapped == sequence) {
                *page = Page::Zeros;
            }
        }
        Ok(())
    }

    /// Begins a case of hers as she began the first, whatever came before, so that a case is
    /// replayed alone: every extent she has freed, committed, and her window unmapped.
    fn begin(&mut self) -> Result<(), String> {
        let ps = page_size();
        expect("a case's first commit", self.client.commit(None), Ok(()))?;
        self.extents.values_mut().for_each(|kept| kept.sealed = true);
        let kept: Vec<u64> = self.extents.keys().copied().collect();
        kept.into_iter().try_for_each(|sequence| self.free(sequence))?;
        self.freed.clear();
        let unmapped = self.client.unmap(0, ALICE_PAGES as u64 * ps);
        expect("a case's first unmap", unmapped, Ok(()))?;
        self.pages.fill(Page::Unmapped);
        Ok(())
    }

    /// `error`, at her op `index`, with what her window and model hold.
    fn explained(&self, index: usize, error: String) -> String {
        let model: Vec<String> = (self.pages.iter().enumerate())
            .filter(|(_, page)| **page != Page::Unmapped)
            .map(|(at, page)| match page {
                Page::Private(_) => format!("{at}: private"),
                page => format!("{at}: {page:?}"),
            })
            .collect();
        let sealed: Vec<(u64, bool)> =
            self.extents.iter().map(|(sequence, kept)| (*sequence, kept.sealed)).collect();
        format!(
            "{error}, at op {index}; her model's window {model:?}, extents {sealed:?}; the daemon's \
             mappings {:?}",
            self.client.window_mappings(),
        )
    }

    /// A case of hers, begun as her first was ([`Alice::begin`]): `ops`, and then her whole
    /// window as her guest reads it, a commit, and her latest extents as she reads them, each as
    /// her model says.
    fn case(&mut self, ops: &[AliceOp]) -> Result<(), String> {
        self.begin()?;
        for (index, op) in ops.iter().enumerate() {
            self.run(*op).map_err(|error| self.explained(index, error))?;
        }
        let ps = page_size() as usize;
        for at in 0..ALICE_PAGES {
            if let Some(expected) = self.page_bytes(&self.pages[at]) {
                let got = guest_read(self.page_addr(at), ps);
                if got != Ok(expected.clone()) {
                    let (got, expected) = (summary(&got), summary(&Ok::<_, ()>(expected)));
                    return Err(format!(
                        "alice's window's page {at}: got {got}, her model {expected}"
                    ));
                }
            }
        }
        expect("the case's last commit", self.client.commit(None), Ok(()))?;
        self.extents.values_mut().for_each(|kept| kept.sealed = true);
        for (sequence, kept) in self.extents.iter().rev().take(8) {
            let got = read(&self.client, self.id(*sequence), 0, kept.bytes.len());
            if got.as_ref() != Ok(&kept.bytes) {
                return Err(format!("alice's extent {sequence}: got {}", summary(&got)));
            }
        }
        Ok(())
    }
}

/// What a case names, by its words and places.
struct Names {
    case: u64,
    vms: PathBuf,
    alices: PathBuf,
    alice: u64,
    bob: PathBuf,
    bobs: u64,
    given: u64,
    given_version: u64,
    given_tenant: u64,
    /// Mallory's tenant's arena, whose cached pages a flood of page-ins evicts first.
    arena: ArenaId,
}

impl Names {
    fn place(&self, place: Place) -> PathBuf {
        let alices = self.alices.file_name().unwrap();
        match place {
            Place::Own(own) => self.vms.join(format!("{}-{own}", self.case)),
            Place::Escaped => self.vms.join("..").join(alices).join("archive"),
            Place::Linked => self.vms.join("link").join("archive"),
            Place::Alices => self.alices.join("archive"),
            Place::AlicesOpen => self.alices.join("store"),
            Place::AlicesNew => self.alices.join(format!("new-{}", self.case)),
            Place::LinkedNew => self.vms.join("link").join(format!("new-{}", self.case)),
            Place::Bobs => self.bob.clone(),
            Place::Relative => PathBuf::from("archive"),
            Place::Directory => self.vms.clone(),
        }
    }

    fn word(&self, word: Word, own: u64) -> u64 {
        match word {
            Word::Is(word) => word,
            Word::Own => own,
            Word::Alice => self.alice,
            Word::Bob => self.bobs,
            Word::Given => self.given,
            Word::GivenVersion => self.given_version,
            Word::GivenTenant => self.given_tenant,
        }
    }

    fn payload(&self, payload: &Payload, own: u64) -> Vec<u8> {
        match payload {
            Payload::Bytes(bytes) => bytes.clone(),
            Payload::Filled(byte, len) => vec![*byte; *len],
            Payload::Path(place) => self.place(*place).as_os_str().as_bytes().to_vec(),
            Payload::Open(place) => {
                let mut bytes = self.place(*place).as_os_str().as_bytes().to_vec();
                bytes.push(0);
                bytes.extend_from_slice(cladefs_core::BUILD_COMMIT.as_bytes());
                bytes
            }
            Payload::Extents(extents) => (extents.iter())
                .flat_map(|(cell, sequence)| [self.word(*cell, own), *sequence])
                .flat_map(u64::to_le_bytes)
                .collect(),
            Payload::Mapping { cell, sequence, offset, len, to, zeros } => {
                let contents = if *zeros {
                    cladefs_core::MappingContents::Zeros
                } else {
                    cladefs_core::MappingContents::Extent {
                        extent: ExtentId {
                            cell: self.word(*cell, own),
                            sequence: self.word(*sequence, own),
                        },
                        offset: self.word(*offset, own),
                    }
                };
                crate::codec::mappings_payload(&[cladefs_core::WindowMapping {
                    window_offset: self.word(*to, own),
                    len: self.word(*len, own),
                    contents,
                }])
            }
        }
    }
}

/// A descriptor of `area`, `given` the memory the connection was given: none if it was given none.
fn area_fd(area: Area, given: &Mutex<Vec<OwnedFd>>) -> Option<OwnedFd> {
    let memfd = |len: u64, sealed: bool, populate: bool| {
        // SAFETY: Makes a descriptor, owned below.
        let fd = unsafe { libc::memfd_create(c"transfer".as_ptr(), libc::MFD_ALLOW_SEALING) };
        assert!(fd >= 0);
        // SAFETY: Just made, and owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: Sizes it, and seals it if asked.
        unsafe {
            assert_eq!(libc::ftruncate(fd.as_raw_fd(), len as i64), 0);
            if sealed {
                assert_eq!(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK), 0);
            }
        }
        // cladefs: written, as a VM process writes what of its area it uses, which the daemon
        // verifies once at `TRANSFER` (TASK-547, board 11707).
        if populate && len > 0 {
            // SAFETY: Maps it, as large as it is, writes it, and unmaps it.
            unsafe {
                let len = len as usize;
                let shared = libc::MAP_SHARED;
                let rw = libc::PROT_READ | libc::PROT_WRITE;
                let addr = libc::mmap(std::ptr::null_mut(), len, rw, shared, fd.as_raw_fd(), 0);
                assert_ne!(addr, libc::MAP_FAILED);
                addr.cast::<u8>().write_bytes(0, len);
                assert_eq!(libc::munmap(addr, len), 0);
            }
        }
        fd
    };
    Some(match area {
        Area::Sealed(len) => memfd(len, true, true),
        Area::Sparse(len) => memfd(len, true, false),
        Area::Unsealed(len) => memfd(len, false, false),
        Area::Short(len) => memfd(len / 2, true, true),
        Area::Pipe => {
            let mut pipe = [0; 2];
            // SAFETY: Makes a pipe, whose ends are owned here, the write end closed.
            unsafe {
                assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
                drop(OwnedFd::from_raw_fd(pipe[1]));
                OwnedFd::from_raw_fd(pipe[0])
            }
        }
        Area::Given(which, _) => given.lock().unwrap().get(which as usize)?.try_clone().unwrap(),
    })
}

/// The bytes a `TRANSFER` of `area` says it has.
fn area_len(area: Area) -> u64 {
    match area {
        Area::Sealed(len)
        | Area::Sparse(len)
        | Area::Unsealed(len)
        | Area::Short(len)
        | Area::Given(_, len) => len,
        Area::Pipe => 1 << 20,
    }
}

/// Does `abuse` to the memory the connection was `given`, if it was given any, whatever it
/// comes to: the kernel may refuse it.
fn abuse(abuse: Abuse, given: &Mutex<Vec<OwnedFd>>) {
    let given = given.lock().unwrap();
    let which = match abuse {
        Abuse::Shrink { which, .. }
        | Abuse::Grow { which }
        | Abuse::Punch { which, .. }
        | Abuse::Scribble { which, .. } => which,
    };
    let Some(fd) = given.get(which as usize) else { return };
    let fd = fd.as_raw_fd();
    let at = |at: u64| (at % (1 << 46)) as libc::off_t;
    // SAFETY: Plain system calls on a descriptor the connection holds, its memory's.
    unsafe {
        match abuse {
            Abuse::Shrink { to, .. } => libc::ftruncate(fd, to as libc::off_t),
            Abuse::Grow { .. } => {
                let mut stat: libc::stat = std::mem::zeroed();
                libc::fstat(fd, &mut stat);
                libc::ftruncate(fd, stat.st_size + (1 << 40))
            }
            Abuse::Punch { at: from, len, .. } => {
                let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
                libc::fallocate(fd, punch, at(from), len as libc::off_t)
            }
            Abuse::Scribble { at: to, .. } => {
                libc::pwrite(fd, b"mallory!".as_ptr().cast(), 8, at(to)) as libc::c_int
            }
        };
    }
}

/// The device and inode of `fd`'s file.
fn identity(fd: BorrowedFd<'_>) -> (u64, u64) {
    // SAFETY: A stat buffer the call fills, of a descriptor the caller holds.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a descriptor the caller holds open, and the buffer above.
    assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) }, 0);
    (stat.st_dev as u64, stat.st_ino as u64)
}

/// Where in the memory `fd` has, read whole, a `mark` is: what of it holds data, as its holes hold
/// nothing.
fn find_marks(fd: BorrowedFd<'_>, marks: &[[u8; 8]]) -> Option<(u64, [u8; 8])> {
    const CHUNK: usize = 1 << 20;
    let raw = fd.as_raw_fd();
    let mut buffer = vec![0u8; CHUNK + 8];
    let mut at: i64 = 0;
    loop {
        // SAFETY: Seeks a descriptor the caller holds.
        let data = unsafe { libc::lseek(raw, at, libc::SEEK_DATA) };
        if data < 0 {
            return None;
        }
        // SAFETY: As above.
        let hole = unsafe { libc::lseek(raw, data, libc::SEEK_HOLE) };
        assert!(hole > data);
        let mut from = data;
        while from < hole {
            let len = (CHUNK as i64 + 8).min(hole - from) as usize;
            // SAFETY: Reads into the buffer, which has room.
            let read = unsafe { libc::pread(raw, buffer.as_mut_ptr().cast(), len, from) };
            assert_eq!(read, len as isize);
            for mark in marks {
                if let Some(found) = memchr::memmem::find(&buffer[..len], mark) {
                    return Some((from as u64 + found as u64, *mark));
                }
            }
            // The last bytes again, as a mark may span chunks.
            from += (len as i64 - 7).max(1);
        }
        at = hole;
    }
}

/// How a reply a connection of mallory's waits for was answered: its status, once it was.
#[derive(Default)]
struct Answers {
    statuses: Mutex<(HashMap<u64, i32>, bool)>,
    answered: Condvar,
}

impl Answers {
    fn answer(&self, id: u64, status: i32) {
        self.statuses.lock().unwrap().0.insert(id, status);
        self.answered.notify_all();
    }

    /// Nothing more is answered: the daemon let it go, or it hung up.
    fn end(&self) {
        self.statuses.lock().unwrap().1 = true;
        self.answered.notify_all();
    }

    /// The status request `id` was answered with, waiting for it: none if nothing more is.
    fn wait(&self, id: u64) -> Option<i32> {
        let statuses = self.statuses.lock().unwrap();
        let (statuses, _) = self
            .answered
            .wait_timeout_while(statuses, WAIT, |(statuses, ended)| {
                !statuses.contains_key(&id) && !*ended
            })
            .unwrap();
        statuses.0.get(&id).copied()
    }
}

/// What a connection of mallory's got, and what of it breaks an invariant.
#[derive(Default)]
struct Seen {
    served: u64,
    restored: u64,
    /// What it got of another tenant's: replies and stage operations naming their cells or
    /// holding their bytes, arenas and memory not its tenant's or the public one.
    stolen: Vec<String>,
    /// Windows it was granted past the most the daemon makes room for.
    vast: Vec<u64>,
    /// The memory it got, as its cells opened.
    memories: Vec<OwnedFd>,
    /// Its flood's write's status, and whether the daemon let it go.
    flood: Option<(Option<i32>, bool)>,
    /// Its last commit's status, of the cell it writes, if it opened one to write.
    committed: Option<Option<i32>>,
    /// The cell it opened to write, if any.
    wrote: Option<PathBuf>,
    /// What each of its `OPEN`s named, and how it was answered: what its last commit's failure
    /// says.
    opens: Vec<(PathBuf, u32, i32)>,
    /// How the `MAP`s of its floods of page-ins were answered, by status, `None` for those never
    /// answered: those answered 0 mapped their extents, to fault on.
    page_ins: BTreeMap<Option<i32>, u64>,
    /// Its fills' writes' statuses (TASK-94).
    filled: Vec<Option<i32>>,
    /// cladefs: whether it said what may end its connection, which its cell commits nothing after:
    /// a payload past what its kind holds (TASK-581, board 8459), or an acknowledgement of a stage
    /// operation past the last it was sent, which may be one never sent (board 8463).
    may_end: bool,
}

/// What both of a connection's threads share: what it sent, by ID, and what was answered.
#[derive(Default)]
struct Shared {
    sent: Mutex<HashMap<u64, Header>>,
    answers: Answers,
    own: AtomicU64,
    /// The memory it was given as its cell opened: its tenant's arena, and the public one.
    memories: Mutex<Vec<OwnedFd>>,
    /// Whether it opened a cell to write.
    writer: std::sync::atomic::AtomicBool,
    /// The paths its `OPEN`s named, by their IDs.
    paths: Mutex<HashMap<u64, PathBuf>>,
    /// The IDs of the `MAP`s of its floods of page-ins.
    flooding: Mutex<BTreeSet<u64>>,
    /// The largest window it was granted, as `LIMIT` was raised for it.
    granted: AtomicU64,
    /// cladefs: the last stage operation it was sent, as it received them.
    last_stage: AtomicU64,
    /// cladefs: whether it said what may end its connection ([`Seen::may_end`]).
    may_end: std::sync::atomic::AtomicBool,
}

impl Shared {
    /// cladefs: it acknowledges stage operation `id`: past the last it received, one the daemon
    /// may never have sent, which ends its connection (board 8463).
    fn acknowledging(&self, id: u64) {
        if id > self.last_stage.load(Ordering::Relaxed) {
            self.may_end.store(true, Ordering::Relaxed);
        }
    }
}

/// The tenants beside mallory's, what it may not reach of theirs.
struct Theirs {
    cells: [u64; 3],
    memories: [(u64, u64); 2],
    arenas: [ArenaId; 2],
}

/// Receives what the daemon sends a connection of mallory's, acknowledging its stage operations as
/// `acking` says, until it is let go or hangs up; or, if `stop_at_last`, until [`LAST`] is
/// answered, from when what the daemon sends it stays unread.
fn receive(
    mut receiver: Receiver,
    sender: Arc<Sender>,
    shared: Arc<Shared>,
    acking: Acking,
    stop_at_last: bool,
    theirs: Arc<Theirs>,
) -> Seen {
    let mut seen = Seen::default();
    let mut early = false;
    while let Ok(Some(message)) = receiver.receive(|| u64::MAX) {
        let header = message.header;
        let theirs_named = |word: &u64| theirs.cells.contains(word);
        let words: [u64; 5] = header.read_body();
        let mut said = words.iter().any(theirs_named);
        // A window's mappings name their extents' cells.
        said |= message
            .payload
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .any(|word| theirs_named(&word));
        if said {
            seen.stolen.push(format!("their cell named: {header:?}"));
        }
        for mark in [ALICE_MARK, BOB_MARK] {
            if memchr::memmem::find(&message.payload, &mark).is_some() {
                seen.stolen.push(format!("their bytes, of {header:?}"));
            }
        }
        // Each page it maps of its tenant's arena, or the public one, as KVM would map it.
        let mut arenas = Vec::new();
        match header.kind {
            STAGE_MAP => arenas.push(u64::from(header.read_body::<StageMapBody>().arena)),
            STAGE_REMAP => {
                if let Ok(entries) =
                    <[StageAnewEntry] as zerocopy::FromBytes>::ref_from_bytes(&message.payload)
                {
                    for entry in entries {
                        if entry.kind == STAGE_MAP {
                            arenas.push(u64::from(entry.arena));
                        }
                    }
                }
            }
            _ => {}
        }
        for arena in arenas {
            if theirs.arenas.iter().any(|theirs| u64::from(theirs.0) == arena) {
                seen.stolen.push(format!("their arena mapped: {header:?}"));
            }
        }
        for fd in &message.fds {
            if theirs.memories.contains(&identity(fd.as_fd())) {
                seen.stolen.push(format!("their memory given: {header:?}"));
            }
        }
        if header.kind == REPLY {
            let request = shared.sent.lock().unwrap().get(&header.id).copied();
            if header.status == 0 {
                seen.served += 1;
                match request {
                    Some(request) if request.kind == OPEN => {
                        shared.own.store(header.read_body::<OpenedBody>().cell, Ordering::Relaxed);
                        shared.writer.store(true, Ordering::Relaxed);
                        seen.wrote = shared.paths.lock().unwrap().get(&header.id).cloned();
                        seen.restored += u64::from(request.flags == OPEN_FORK);
                    }
                    Some(request) if request.kind == WINDOW => {
                        let size = request.read_body::<WindowBody>().size;
                        if size > MAX_WINDOW {
                            seen.vast.push(size);
                        } else {
                            // The connection's bound, and the heap's, are its credit's of it.
                            let before = shared.granted.fetch_max(size, Ordering::Relaxed);
                            let more = (credit(size.max(before)) - credit(before)) as usize;
                            // Unless the case is over, and checks nothing.
                            let _ =
                                LIMIT.try_update(Ordering::Relaxed, Ordering::Relaxed, |limit| {
                                    (limit != 0).then_some(limit + more)
                                });
                        }
                    }
                    _ => {}
                }
                if request.is_some_and(|request| request.kind == OPEN) {
                    let fds = message.fds.iter().map(|fd| fd.try_clone().unwrap()).collect();
                    *shared.memories.lock().unwrap() = fds;
                    seen.memories.extend(message.fds);
                }
            }
            if let Some(request) = request.filter(|request| request.kind == OPEN) {
                let path =
                    shared.paths.lock().unwrap().get(&header.id).cloned().unwrap_or_default();
                seen.opens.push((path, request.flags, header.status));
            }
            if shared.flooding.lock().unwrap().contains(&header.id) {
                *seen.page_ins.entry(Some(header.status)).or_default() += 1;
            }
            shared.answers.answer(header.id, header.status);
            if header.id == LAST && stop_at_last {
                return seen;
            }
        }
        if header.kind == REPLY || header.kind == CAPACITY {
            continue;
        }
        shared.last_stage.fetch_max(header.id, Ordering::Relaxed);
        let ack = |id| {
            shared.acknowledging(id);
            let _ = sender.send(Header::new(ACK, id), &[], &[]);
        };
        match acking {
            Acking::Honest if header.flags & STAGE_ACK != 0 => ack(header.id),
            Acking::Early if !early => {
                early = true;
                ack(u64::MAX);
            }
            Acking::Forged => {
                ack(header.id.wrapping_add(1000));
                ack(header.id);
                ack(header.id.saturating_sub(5));
            }
            Acking::Honest | Acking::Early | Acking::Never => {}
        }
    }
    shared.answers.end();
    seen
}

/// Runs `hostile`, one of the connections of mallory's case named `names`, at the daemon's
/// `socket`, starting what it says once every connection of the case is ready to (`start`).
fn run_hostile(
    hostile: Hostile,
    index: usize,
    names: Arc<Names>,
    socket: PathBuf,
    theirs: Arc<Theirs>,
    start: Arc<std::sync::Barrier>,
) -> Seen {
    let stream = UnixStream::connect(&socket).unwrap();
    let sender = Arc::new(Sender::new(stream.try_clone().unwrap()).unwrap());
    let receiver = Receiver::new(stream.try_clone().unwrap());
    let shared = Arc::new(Shared::default());
    let receiving = {
        let (sender, shared) = (sender.clone(), shared.clone());
        std::thread::spawn(move || {
            receive(receiver, sender, shared, hostile.acking, hostile.flood, theirs)
        })
    };
    let mut next = 0;
    // Each request by its ID, which mallory gives each of its own: what an acknowledgement says
    // is the stage operation's.
    let send =
        |header: Header, payload: &[u8], fd: Option<OwnedFd>| -> Result<u64, std::io::Error> {
            if ![ACK, STAGE_COUNTS].contains(&header.kind) {
                shared.sent.lock().unwrap().insert(header.id, header);
            }
            // What an `OPEN` names, before a restore's NUL.
            if header.kind == OPEN {
                let named = payload.split(|byte| *byte == 0).next().unwrap_or_default();
                let named = PathBuf::from(std::ffi::OsStr::from_bytes(named));
                shared.paths.lock().unwrap().insert(header.id, named);
            }
            // cladefs: past what its kind holds, refused at its header (board 8459).
            let said = Header { len: payload.len() as u64, ..header };
            if said.len > crate::codec::most_payload(&said) {
                shared.may_end.store(true, Ordering::Relaxed);
            }
            if header.kind == ACK {
                shared.acknowledging(header.id);
            }
            let fds: Vec<BorrowedFd<'_>> = fd.iter().map(|fd| fd.as_fd()).collect();
            sender.send(header, payload, &fds).map(|()| header.id)
        };
    let mut id = || {
        next += 1;
        next
    };
    let opened = match hostile.opens {
        Opens::Nothing => None,
        Opens::Shared(own) => Some(names.place(Place::Own(own))),
        Opens::Own => Some(names.vms.join(format!("{}-c{index}", names.case))),
        Opens::Elsewhere(place) => Some(names.place(place)),
        Opens::Restores | Opens::Twice => None,
    };
    let open_payload = |path: &std::path::Path| {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        bytes.extend_from_slice(cladefs_core::BUILD_COMMIT.as_bytes());
        bytes
    };
    if let Some(path) = opened {
        let open = Header::new(OPEN, id())
            .with_body(&OpenBody { capacity: CELL_BYTES, version: VERSION, ..Default::default() })
            .with_flags(OPEN_CREATE);
        if let Ok(id) = send(open, &open_payload(&path), None) {
            shared.answers.wait(id);
        }
    }
    if hostile.opens == Opens::Twice {
        let opens: Vec<_> = ["a", "b"]
            .map(|which| {
                let path = names.vms.join(format!("{}-t{index}{which}", names.case));
                let open = Header::new(OPEN, id()).with_body(&OpenBody {
                    capacity: CELL_BYTES,
                    version: VERSION,
                    ..Default::default()
                });
                send(open.with_flags(OPEN_CREATE), &open_payload(&path), None)
            })
            .into_iter()
            .collect();
        for open in opens.into_iter().flatten() {
            shared.answers.wait(open);
        }
    }
    if hostile.opens == Opens::Restores {
        let to = names.vms.join(format!("{}-r{index}", names.case));
        let restore = Header::new(OPEN, id())
            .with_body(&OpenBody {
                capacity: CELL_BYTES,
                cell: names.given,
                cell_version: names.given_version,
                tenant: names.given_tenant,
                version: VERSION,
            })
            .with_flags(OPEN_FORK);
        if let Ok(id) = send(restore, &open_payload(&to), None) {
            shared.answers.wait(id);
        }
    }
    let (size, copying) = hostile.window;
    let flags = if copying { WINDOW_COPYING } else { 0 };
    let window =
        Header::new(WINDOW, id()).with_body(&WindowBody { size, budget: 0 }).with_flags(flags);
    let window = send(window, &[], None);
    if let Some(area) = hostile.area {
        let transfer = Header::new(TRANSFER, id()).with_body(&TransferBody { len: area_len(area) });
        let _ = send(transfer, &[], area_fd(area, &shared.memories));
    }
    let ps = page_size();
    // A flood of page-ins (TASK-149.34): its extents cold, mapped, if its cell is a restore of the
    // version, and faulted on, `times` over, all at once, its replies left to come.
    let page_in = |id: &mut dyn FnMut() -> u64, times: u8| {
        cladefs_core::cache::evict_least_used(Some(names.arena), |_| false);
        for sequence in 1..=GIVEN_EXTENTS {
            let at = (sequence - 1) * ps;
            let mapped = Header::new(MAP, id());
            let payload = crate::codec::mappings_payload(&[cladefs_core::WindowMapping {
                window_offset: at,
                len: ps,
                contents: cladefs_core::MappingContents::Extent {
                    extent: ExtentId { cell: names.given, sequence },
                    offset: 0,
                },
            }]);
            shared.flooding.lock().unwrap().insert(mapped.id);
            let _ = send(mapped, &payload, None);
        }
        for _ in 0..times {
            for sequence in 1..=GIVEN_EXTENTS {
                let at = (sequence - 1) * ps;
                let faulted = Header::new(FAULT, id()).with_body(&FaultBody { offset: at });
                let _ = send(faulted, &[], None);
            }
        }
    };
    // A restore of the version floods it with page-ins at once, as a restored VM's first faults do.
    if hostile.opens == Opens::Restores {
        // Once its window is, which its MAPs, served beside it, may otherwise come before.
        if let Ok(window) = window {
            shared.answers.wait(window);
        }
        page_in(&mut id, 2);
    }
    start.wait();
    let mut filled = Vec::new();
    for act in &hostile.acts {
        let said = match act {
            Act::Say(said) => said,
            Act::Abuse(abused) => {
                abuse(*abused, &shared.memories);
                continue;
            }
            Act::PageIns { times } => {
                page_in(&mut id, *times);
                continue;
            }
            Act::Fill { extents } => {
                let own = shared.own.load(Ordering::Relaxed);
                for _ in 0..*extents {
                    let sequence = FILL_EXTENT + filled.len() as u64;
                    let written = Header::new(WRITE, id()).with_body(&WriteBody {
                        cell: own,
                        sequence,
                        data_len: 1 << 20,
                        ..Default::default()
                    });
                    let written = send(written, &noise(own ^ sequence, 1 << 20), None);
                    filled.push(written.ok().and_then(|id| shared.answers.wait(id)));
                }
                continue;
            }
        };
        for time in 0..u64::from(said.repeat) {
            let own = shared.own.load(Ordering::Relaxed);
            let mut args = said.args.map(|word| names.word(word, own));
            if said.repeat > 1 && [UNMAP, FAULT].contains(&said.kind) {
                args[0] = (args[0].wrapping_add(time * 7 * ps)) % WINDOW_BYTES;
            }
            let last = shared.last_stage.load(Ordering::Relaxed);
            let id = said.id.map_or_else(&mut id, |acked| acked.id(last));
            let header = Header::new(said.kind, id).with_body(&args).with_flags(said.flags);
            let payload = names.payload(&said.payload, own);
            // The daemon may have let it go by now.
            let sent =
                send(header, &payload, said.area.and_then(|area| area_fd(area, &shared.memories)));
            // What comes after a restore is said of the cell it opened, as the daemon may take
            // longer over it than over all that comes after.
            if let Ok(id) = sent
                && said.kind == OPEN
                && said.flags == OPEN_FORK
            {
                shared.answers.wait(id);
            }
        }
    }
    let mut flood = None;
    let mut committed = None;
    if hostile.waits {
        // Whatever it said, the cell it writes commits, as one used as the protocol allows does:
        // what the daemon took of it, it can commit (TASK-150.7). Once its OPENs are answered: one
        // waiting for its path, which a connection going holds, is served after a COMMIT sent
        // meanwhile, which finds no cell (EBADF), as it rightly would.
        let opens: Vec<u64> = shared.paths.lock().unwrap().keys().copied().collect();
        for open in opens {
            shared.answers.wait(open);
        }
        let commit = Header::new(COMMIT, id()).with_flags(COMMIT_UPLOADED);
        let status = send(commit, &[], None).ok().and_then(|id| shared.answers.wait(id));
        committed = shared.writer.load(Ordering::Relaxed).then_some(status);
        if hostile.flood {
            let own = shared.own.load(Ordering::Relaxed);
            let written = Header::new(WRITE, id()).with_body(&WriteBody {
                cell: own,
                sequence: FLOOD_EXTENT,
                data_len: FLOOD_READ,
                ..Default::default()
            });
            let written = send(written, &vec![0x3c; FLOOD_READ as usize], None);
            flood = Some(written.ok().and_then(|id| shared.answers.wait(id)));
        }
        if let Ok(id) = send(Header::new(INFO, LAST), &[], None) {
            shared.answers.wait(id);
        }
    }
    let mut let_go = false;
    if flood.is_some() {
        // Past the credit the window it was granted makes, by a few reads: none of their replies
        // read.
        let own = shared.own.load(Ordering::Relaxed);
        let reads = credit(shared.granted.load(Ordering::Relaxed)) / FLOOD_READ + 64;
        for _ in 0..reads {
            let read = Header::new(READ, id()).with_body(&ReadBody {
                cell: own,
                sequence: FLOOD_EXTENT,
                offset: 0,
                len: FLOOD_READ,
                transfer_at: 0,
            });
            if sender.send(read, &[], &[]).is_err() {
                break;
            }
        }
        let mut hung_up =
            libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
        // SAFETY: Polls the stream, which this keeps open, for 30 s at most.
        let ready = unsafe { libc::poll(&mut hung_up, 1, 30_000) };
        let_go = ready == 1 && hung_up.revents & libc::POLLRDHUP != 0;
    }
    let raw = |bytes: &[u8]| {
        let _ = std::io::Write::write_all(&mut &stream, bytes);
    };
    let header = |len, fds| Header { kind: WRITE, len, fds, ..Default::default() };
    // cladefs: a payload the daemon admits, none of which comes, its connection open: which the
    // daemon ends once none has come for `STALL` (board 8459), as the VM process says no more.
    let mut held_open = false;
    match hostile.last_words {
        LastWords::Nothing => {}
        LastWords::PartOfAHeader => raw(&header(0, 0).as_bytes()[..16]),
        LastWords::MissingPayload(len) => {
            // A payload of none is a message whole, which the daemon answers.
            held_open = len > 0 && len <= crate::codec::most_payload(&header(len, 0));
            raw(header(len, 0).as_bytes())
        }
        LastWords::MissingDescriptors(fds) => raw(header(0, fds).as_bytes()),
    }
    // Shut down for writing too: the daemon may be sending it what it never reads. cladefs: but a
    // payload held open, which the daemon ends.
    if !held_open {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let mut seen = receiving.join().unwrap();
    seen.may_end = shared.may_end.load(Ordering::Relaxed);
    let answered: u64 = seen.page_ins.values().sum();
    let unanswered = shared.flooding.lock().unwrap().len() as u64 - answered;
    if unanswered > 0 {
        seen.page_ins.insert(None, unanswered);
    }
    seen.flood = flood.map(|written| (written, let_go));
    seen.committed = committed;
    seen.filled = filled;
    seen
}

/// TASK-149.17: a compromised VM process of one tenant, saying anything, on connections at once,
/// harms no other tenant beside it, nor holds the daemon past the protocol's bounds, nor leaves
/// anything of its own behind: see the module's docs. Seeded by proptest, which says the case if
/// one fails: as many as `CLADEFS_HOSTILE_CASES` says, 48 by default, from `PROPTEST_RNG_SEED`'s
/// seed if set.
#[test]
fn a_compromised_vm_process_harms_no_tenant_beside_it() {
    static PANICS: AtomicU64 = AtomicU64::new(0);
    let panicked = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        panicked(info);
    }));

    host();
    let dir = tempfile::tempdir().unwrap();
    // Where the paths mallory names relative to the daemon's directory are, should a daemon take
    // them: not the crate's.
    std::env::set_current_dir(dir.path()).unwrap();
    let tenant = |name: &str, id: u64| {
        let cells = dir.path().join(name);
        std::fs::create_dir(&cells).unwrap();
        let tenant = Tenant::new(id, &cells).unwrap();
        let socket = dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        let served = tenant.clone();
        std::thread::spawn(move || serve_tenant(listener, served));
        (cells, socket, tenant)
    };
    let (vms, mallory_socket, mallory) = tenant("mallory", MALLORY);
    let (alices, alice_socket, alice_tenant) = tenant("alice", ALICE);
    let bob_socket = dir.path().join("bob.sock");
    let listener = UnixListener::bind(&bob_socket).unwrap();
    std::thread::spawn(move || serve(listener));
    std::os::unix::fs::symlink(&alices, vms.join("link")).unwrap();

    // Alice's cell she closed, which mallory names every way it can.
    let archive = alices.join("archive");
    let archived = {
        let client = CellClient::connect(&alice_socket, &archive, CELL_BYTES).unwrap();
        let extent = ExtentId { cell: client.cell(), sequence: 1 };
        write(&client, extent, 0, &marked(ALICE_MARK, 0, 4 * BLOCK)).unwrap();
        client.commit(Some(b"archived")).unwrap();
        extent
    };
    // Bob's, the default tenant's, which waits.
    let bob_path = dir.path().join("bob");
    let bob = CellClient::connect(&bob_socket, &bob_path, CELL_BYTES).unwrap();
    let bobs = ExtentId { cell: bob.cell(), sequence: 1 };
    write(&bob, bobs, 0, &marked(BOB_MARK, 1, BLOCK)).unwrap();
    bob.commit(None).unwrap();
    // A snapshot in the bucket, of a cell mallory's VM manager makes a version of in its directory.
    // Its extents a MiB each, which mallory's floods of page-ins page in (TASK-149.34).
    let given = CellClient::connect(&bob_socket, dir.path().join("given"), CELL_BYTES).unwrap();
    let given_cell = given.cell();
    for sequence in 1..=GIVEN_EXTENTS {
        let extent = ExtentId { cell: given_cell, sequence };
        write(&given, extent, 0, &vec![0x88; 1 << 20]).unwrap();
    }
    given.commit(Some(b"given")).unwrap();
    let given_version = given.make_version().unwrap();
    drop(given);
    let shared = (given_version.version, true);
    control(crate::cells::share_version(
        crate::Daemon::process(),
        dir.path().join("given"),
        shared.0,
        shared.1,
    ))
    .unwrap();

    // Alice, on a thread of her own, her guest's, doing what each case asks of her.
    let (asking, asked) = std::sync::mpsc::channel::<Vec<AliceOp>>();
    let (answering, answered) = std::sync::mpsc::channel();
    let (ready, alice_ready) = std::sync::mpsc::channel();
    let (alice_path, socket) = (alices.join("store"), alice_socket.clone());
    std::thread::spawn(move || {
        let mut alice = Alice::new(&socket, &alice_path);
        ready.send((alice.cell, identity(alice.client.physical_memory()))).unwrap();
        for ops in asked {
            if answering.send(alice.case(&ops)).is_err() {
                break;
            }
        }
    });
    let (alice_cell, alice_memory) = alice_ready.recv_timeout(WAIT).unwrap();
    let theirs = Arc::new(Theirs {
        cells: [alice_cell, archived.cell, bob.cell()],
        memories: [alice_memory, identity(bob.physical_memory())],
        arenas: [alice_tenant.arena(), crate::server::default_tenant().unwrap().arena()],
    });
    // What the others' directories hold, which nothing mallory says changes: alice's, and the
    // daemon's roots, bob's.
    let listing = |dir: &Path| -> BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name()).collect()
    };
    let (alices_listed, root_listed) = (listing(&alices), listing(dir.path()));
    // Alice's and bob's, once those that made the others' cells are gone.
    let connected = 2;
    let settled = within(WAIT, move || {
        while connections() > connected {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    });
    assert_eq!(settled, Ok(()));

    let cases: u32 =
        std::env::var("CLADEFS_HOSTILE_CASES").map_or(48, |cases| cases.parse().unwrap());
    // A failing case shrinks for two minutes at most.
    let config =
        Config { cases, failure_persistence: None, max_shrink_time: 120_000, ..Config::default() };
    let numbered = AtomicU64::new(0);
    let (served, restored, floods) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let commits = AtomicU64::new(0);
    let page_ins = Mutex::new(BTreeMap::<Option<i32>, u64>::new());
    let restores = Mutex::new(Vec::new());
    let (fills_written, fills_refused) = (AtomicU64::new(0), AtomicU64::new(0));
    let strategy =
        (proptest::collection::vec(hostile(), 1..=3), proptest::collection::vec(alice_op(), 0..32));
    TestRunner::new(config)
        .run(&strategy, |(hostiles, ops)| {
            let case = numbered.fetch_add(1, Ordering::Relaxed);
            let names = Arc::new(Names {
                case,
                vms: vms.clone(),
                alices: alices.clone(),
                alice: alice_cell,
                bob: bob_path.clone(),
                bobs: bob.cell(),
                given: given_cell,
                given_version: given_version.version,
                given_tenant: given_version.tenant,
                arena: mallory.arena(),
            });
            *CASE.lock().unwrap() = format!("case {case}: {hostiles:#?}");
            // As much as the heap may hold, the credits of mallory's connections raised as their
            // windows are granted.
            let limit = held() as u64 + SLACK + hostiles.len() as u64 * credit(0);
            LIMIT.store(limit as usize, Ordering::Relaxed);
            // Its disk limited a few MiB past what it takes and reserves, which its fills reach
            // (TASK-94): each MiB written into an extent of its own admitted with what its cell's
            // own work may add for it (doc-22, `RESERVE_FACTOR`); but as a connection of its
            // floods, whose write, which its reads read, is to be admitted.
            let disk::Counted { bytes: taken, reserved, .. } = disk::count(Some(MALLORY)).unwrap();
            let files_before = files_taking(&vms);
            let flooding = hostiles.iter().any(|hostile| hostile.flood);
            let admitted = 1 + 2 * bucket_cell::host_disk::RESERVE_FACTOR;
            let disk_limit = (!flooding).then_some(taken + reserved + admitted * FILL_ROOM);
            disk::set_limit(Some(MALLORY), disk_limit).unwrap();

            // Mallory's connections at once, and alice beside them.
            let start = Arc::new(std::sync::Barrier::new(hostiles.len()));
            let running: Vec<_> = (hostiles.iter().cloned().enumerate())
                .map(|(index, hostile)| {
                    let (names, socket, theirs, start) =
                        (names.clone(), mallory_socket.clone(), theirs.clone(), start.clone());
                    let (done, finished) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ =
                            done.send(run_hostile(hostile, index, names, socket, theirs, start));
                    });
                    finished
                })
                .collect();
            asking.send(ops.clone()).unwrap();
            let alice = answered.recv_timeout(WAIT * 4);
            let seen: Vec<Seen> = (running.into_iter())
                .map(|finished| finished.recv_timeout(WAIT * 4))
                .collect::<Result<_, _>>()
                .expect("a connection of mallory's never ended");
            LIMIT.store(0, Ordering::Relaxed);
            prop_assert_eq!(alice, Ok(Ok(())));

            // Nothing of theirs reached it; nor was it given a window past the daemon's room.
            for (hostile, seen) in hostiles.iter().zip(&seen) {
                prop_assert_eq!(&seen.stolen, &Vec::<String>::new());
                prop_assert_eq!(&seen.vast, &Vec::<u64>::new());
                // It has one cell at most, however many of its OPENs were at once (TASK-150.5).
                let opened = seen.opens.iter().filter(|(_, _, status)| *status == 0).count();
                prop_assert!(opened <= 1, "a connection opened {opened} cells: {:?}", seen.opens);
                // What follows, unless it said what may end its connection (TASK-581, board 8459,
                // board 8463).
                let ended = seen.may_end;
                // Its cell, if it writes one, commits.
                if let Some(status) = seen.committed.filter(|_| !ended) {
                    prop_assert_eq!(
                        status,
                        Some(0),
                        "mallory's cell didn't commit: {:?}",
                        seen.opens
                    );
                    commits.fetch_add(1, Ordering::Relaxed);
                }
                // A flood past its credit is let go.
                if hostile.flood && !ended {
                    prop_assert_eq!(seen.flood, Some((Some(0), true)));
                    floods.fetch_add(1, Ordering::Relaxed);
                }
                served.fetch_add(seen.served, Ordering::Relaxed);
                if hostile.opens == Opens::Restores {
                    restores.lock().unwrap().push(format!(
                        "window {:?}, acking {:?}, last words {:?}, restored {}, page-ins {:?}",
                        hostile.window,
                        hostile.acking,
                        hostile.last_words,
                        seen.restored,
                        seen.page_ins
                    ));
                }
                for (status, count) in &seen.page_ins {
                    *page_ins.lock().unwrap().entry(*status).or_default() += count;
                }
                restored.fetch_add(seen.restored, Ordering::Relaxed);
            }
            // Every memory it was given, read whole, holds nothing of theirs.
            let mut memories = BTreeMap::new();
            for fd in seen.iter().flat_map(|seen| &seen.memories) {
                memories.entry(identity(fd.as_fd())).or_insert(fd);
            }
            for fd in memories.values() {
                prop_assert_eq!(find_marks(fd.as_fd(), &[ALICE_MARK, BOB_MARK]), None);
            }

            // Every connection of its ends, and holds nothing.
            let ended = within(WAIT, move || {
                while connections() > connected {
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            });
            let stayed = connections().saturating_sub(connected);
            prop_assert!(ended.is_ok(), "{stayed} connections of mallory's stayed");
            // It took no more of the host's disk than its limit, whatever it said: what was
            // admitted, and what its cells' own work adds beside it, which admission reserves
            // room for (doc-22, TASK-539.8): their logs, cells and segments' tails; their extents'
            // holes, zeros a commit seals with them, counted as the extents open grow; framings in
            // copies. Before TASK-539.8 a MiB of slack held it (TASK-343).
            let counted = disk::count(Some(MALLORY)).unwrap().bytes;
            prop_assert!(
                disk_limit.is_none_or(|limit| counted <= limit),
                "mallory's disk takes {counted} bytes, past its limit of {disk_limit:?}, from \
                 {taken} as the case began; the files grown since: {}",
                grown_since(&files_before, &files_taking(&vms))
            );
            disk::set_limit(Some(MALLORY), None).unwrap();
            for status in seen.iter().flat_map(|seen| &seen.filled) {
                match status {
                    Some(0) => fills_written.fetch_add(1, Ordering::Relaxed),
                    Some(status) if *status == -Errno::ENOSPC.raw() => {
                        fills_refused.fetch_add(1, Ordering::Relaxed)
                    }
                    _ => 0,
                };
            }
            prop_assert_eq!(table().writers(), connected);
            let prefix = format!("{case}-");
            let mut named: Vec<_> = std::fs::read_dir(&vms)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with(&prefix))
                .collect();
            named.sort();
            for path in named {
                let opened = CellClient::connect(&mallory_socket, &path, CELL_BYTES);
                let errno = opened.map(drop).err().and_then(|error| error.errno());
                prop_assert_ne!(errno, Some(Errno::EBUSY), "{} stayed held", path.display());
            }
            let nothing = vms.join(format!("nothing-{case}"));
            prop_assert!(!nothing.exists(), "a version of a cell named was taken");
            // Every cell it opened to write opens again, and commits, whatever it said to it,
            // as far as it was served.
            let wrote: BTreeSet<&PathBuf> =
                seen.iter().filter_map(|seen| seen.wrote.as_ref()).collect();
            for path in wrote {
                let reopened = CellClient::connect(&mallory_socket, path, CELL_BYTES)
                    .map_err(|error| error.to_string())
                    .map(|client| client.commit(None));
                prop_assert_eq!(reopened, Ok(Ok(())), "{} opened again", path.display());
            }
            // And the daemon serves on.
            let after = vms.join(format!("{case}-after"));
            let after = CellClient::connect(&mallory_socket, &after, CELL_BYTES);
            prop_assert_eq!(after.map(drop).map_err(|error| error.to_string()), Ok(()));

            // Nothing came into the others' directories, nor went.
            prop_assert_eq!(listing(&alices), alices_listed.clone());
            prop_assert_eq!(listing(dir.path()), root_listed.clone());
            // Alice's cell she closed holds what she wrote; bob's serves.
            let reopened = CellClient::connect(&alice_socket, &archive, CELL_BYTES);
            let reopened = reopened.map_err(|error| error.to_string());
            let archived_bytes =
                reopened.as_ref().map(|client| read(client, archived, 0, 4 * BLOCK));
            prop_assert_eq!(archived_bytes, Ok(Ok(marked(ALICE_MARK, 0, 4 * BLOCK))));
            prop_assert_eq!(reopened.map(|client| client.root()), Ok(Ok(b"archived".to_vec())));
            prop_assert_eq!(read(&bob, bobs, 0, BLOCK), Ok(marked(BOB_MARK, 1, BLOCK)));
            let later = ExtentId { cell: bob.cell(), sequence: 2 + case };
            prop_assert_eq!(write(&bob, later, 0, &marked(BOB_MARK, 2 + case, BLOCK)), Ok(()));
            prop_assert_eq!(bob.commit(None), Ok(()));
            prop_assert_eq!(read(&bob, later, 0, BLOCK), Ok(marked(BOB_MARK, 2 + case, BLOCK)));
            // Freed, so that his cell holds no more than a case's, however many cases run.
            prop_assert_eq!(bob.free(later), Ok(()));
            prop_assert_eq!(bob.commit(None), Ok(()));
            prop_assert_eq!(PANICS.load(Ordering::Relaxed), 0);
            Ok(())
        })
        .unwrap();
    // What it says reaches past the checks: not every request is refused, restores among them,
    // and floods are let go.
    let (served, restored, floods, commits) =
        (served.into_inner(), restored.into_inner(), floods.into_inner(), commits.into_inner());
    let page_ins = page_ins.into_inner().unwrap();
    let flooded = page_ins.get(&Some(0)).copied().unwrap_or(0);
    let (fills_written, fills_refused) = (fills_written.into_inner(), fills_refused.into_inner());
    println!(
        "{served} of mallory's requests served, {restored} of them restores; {commits} of its \
         cells committed at the end; {floods} floods let go; {flooded} extents cold, flooded \
         with page-ins, their MAPs answered {page_ins:?}; {fills_written} MiB filled, \
         {fills_refused} refused at its disk's limit",
    );
    assert!(served > 8 * u64::from(cases), "{served} requests served");
    assert!(restored > 0, "{restored} restores served");
    // And its cells open and commit: a VM process the daemon refused every opening of would pass
    // the rest, as one of another wire version did (TASK-141.3).
    assert!(commits > 0, "{commits} of its cells committed");
    // And its floods of page-ins page in cold extents.
    assert!(
        flooded > 0,
        "{flooded} extents flooded with page-ins; their MAPs answered, by status (None: never): \
         {page_ins:?}; the restores': {:#?}",
        restores.into_inner().unwrap()
    );
    // And its fills write, and are refused at its disk's limit.
    assert!(
        fills_written > 0 && fills_refused > 0,
        "{fills_written} MiB filled, {fills_refused} refused"
    );
    drop(mallory);
}

/// TASK-149.17: a VM process that shrinks the memory it is given as its cell opens crashes
/// nothing: not its tenant's arena, whose pages past a new end would fault the daemon's own
/// accesses (`SIGBUS`), and every tenant's VMs with it; nor the public one. It may only read
/// both (doc-26: it copies its guest's writes into memory of its own), and so neither shrinks nor
/// grows either. The memory it reads is its owner's alone to open anew: a VM process, of another
/// user, can't reopen it to write through `/proc/self/fd`.
#[test]
fn vm_processes_shrinking_their_memory_crash_nothing() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let vms = dir.path().join("vms");
    std::fs::create_dir(&vms).unwrap();
    let socket = dir.path().join("vms.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let tenant = Tenant::new(1, &vms).unwrap();
    std::thread::spawn(move || serve_tenant(listener, tenant));
    let client = CellClient::connect(&socket, vms.join("store"), 1 << 26).unwrap();
    let extent = |sequence| ExtentId { cell: client.cell(), sequence };
    write(&client, extent(1), 0, &super::pattern(1, BLOCK)).unwrap();
    client.commit(None).unwrap();
    // In the extent cache, the arena's pages.
    assert_eq!(read(&client, extent(1), 0, BLOCK), Ok(super::pattern(1, BLOCK)));

    let (own, public) = (client.physical_memory().as_raw_fd(), client.public_memory().as_raw_fd());
    let truncated = |fd, len: i64| {
        // SAFETY: Sizes an object the test holds a descriptor of.
        match unsafe { libc::ftruncate(fd, len) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error().raw_os_error()),
        }
    };
    let shrunk = (truncated(own, 0), truncated(public, 0));
    // What the daemon reads of the arena's pages, which it would fault on, had the VM process
    // shrunk it.
    assert_eq!(read(&client, extent(1), 0, BLOCK), Ok(super::pattern(1, BLOCK)));
    assert_eq!(shrunk, (Err(Some(libc::EINVAL)), Err(Some(libc::EINVAL))));
    // Nor grown, by a tebibyte, as the daemon grows it further, as its extents take more slabs.
    // SAFETY: A stat buffer the call fills, of a descriptor the test holds.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a descriptor the test holds open, and the buffer above.
    assert_eq!(unsafe { libc::fstat(own, &mut stat) }, 0);
    assert_eq!(truncated(own, stat.st_size + (1 << 40)), Err(Some(libc::EINVAL)));
    let extents = (4 << 20) / (8 * BLOCK);
    for sequence in 2..2 + extents as u64 {
        write(&client, extent(sequence), 0, &super::pattern(sequence as u8, 8 * BLOCK)).unwrap();
    }
    client.commit(None).unwrap();
    for sequence in 1..2 + extents as u64 {
        let len = if sequence == 1 { BLOCK } else { 8 * BLOCK };
        assert_eq!(
            read(&client, extent(sequence), 0, len),
            Ok(super::pattern(sequence as u8, len))
        );
    }
    // SAFETY: As above.
    assert_eq!(unsafe { libc::fstat(public, &mut stat) }, 0);
    assert_eq!(stat.st_mode & 0o777, 0o600);
    drop(client);
}

/// TASK-149.17: what the daemon holds of a VM process's requests is within the connection's
/// credit ([`credit`]), whatever they ask: whole extents read and written, through the stream, as
/// many at once as the credit holds, and through its transfer area, more at once than the
/// connection serves, their replies read as they come. Each running held a copy of its whole
/// length, which the credit doesn't count, as a transfer's data isn't: 481 MiB held of reads,
/// 1,024 of writes from the area. And each `ROOT` copied a root as large as a payload: 768 MiB
/// held of 16.
#[test]
fn what_requests_hold_is_within_the_credit() {
    host();
    let dir = tempfile::tempdir().unwrap();
    // A bucket of files, as one in memory would hold what it is sent in the heap counted here.
    let bucket = format!("file://{}/", dir.path().canonicalize().unwrap().join("bucket").display());
    // SAFETY: nextest runs each test in a process of its own, whose other threads don't read the
    // environment yet.
    unsafe { std::env::set_var("CLADEFSD_BUCKET", bucket) };
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (sender, mut receiver, _stream, cell) =
        super::raw_connection(&socket, &dir.path().join("store"), 1 << 33);
    let len = MAX_PAYLOAD;
    let area = area_fd(Area::Sealed(len), &Mutex::default()).unwrap();
    let transfer = Header::new(TRANSFER, 1).with_body(&TransferBody { len });
    sender.send(transfer, &[], &[area.as_fd()]).unwrap();
    let transferred = receiver.receive(|| 0).unwrap().unwrap().header;
    assert_eq!((transferred.id, transferred.status), (1, 0));
    let written = Header::new(WRITE, 2).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: len,
        ..Default::default()
    });
    assert_eq!(
        super::call(&sender, &mut receiver, written, &vec![7; len as usize]).header.status,
        0
    );
    let committed = Header::new(COMMIT, 3);
    assert_eq!(super::call(&sender, &mut receiver, committed, &[]).header.status, 0);
    // In the extent cache, so that what bringing it in holds isn't counted here.
    let warm = Header::new(READ, 4).with_body(&ReadBody {
        cell,
        sequence: 1,
        offset: 0,
        len,
        transfer_at: 0,
    });
    assert_eq!(super::call(&sender, &mut receiver, warm, &[]).header.status, 0);
    // Its root, no larger than a root holds, read back as many at once as the connection serves:
    // one larger is refused at its header, ending its connection
    // (`payloads_of_kinds_that_carry_none_end_their_connections`, board 8459), as it would be
    // copied into each reply.
    let rooted = Header::new(COMMIT, 6).with_flags(COMMIT_ROOT);
    let status =
        super::call(&sender, &mut receiver, rooted, &vec![5; crate::wire::MAX_ROOT]).header.status;
    assert_eq!(status, 0);
    // Open a compact copying window (1 page) so 64 MiB MAP_MANY bursts test request/decode
    // memory against `credit(0) + MAX_PAYLOAD` rather than a multi-GiB persistent window map
    // (TASK-614, board 2624).
    let ps = page_size();
    let map_many_count = MAX_PAYLOAD / MAPPING_BYTES;
    const WINDOW_PAGES: u64 = 1;
    let window = Header::new(WINDOW, 7)
        .with_body(&WindowBody { size: WINDOW_PAGES * ps, budget: 0 })
        .with_flags(WINDOW_COPYING);
    assert_eq!(super::call(&sender, &mut receiver, window, &[]).header.status, 0);
    // So many of each kind, `at_once` in flight, as many as the protocol allows: those of the
    // stream whose data the credit holds, and those of the area all of them. Their replies are read
    // as they come.
    let mut burst = |kind: &str, headers: Vec<Header>, payload: &[u8], at_once: usize| {
        let count = headers.len();
        let held = peak_while(|| {
            let mut headers = headers.into_iter();
            for header in headers.by_ref().take(at_once) {
                sender.send(header, payload, &[]).unwrap();
            }
            for _ in 0..count {
                loop {
                    let reply = receiver.receive(|| 0).unwrap().unwrap();
                    if reply.header.kind == REPLY {
                        assert_eq!(reply.header.status, 0, "{kind}");
                        break;
                    }
                    if reply.header.flags & STAGE_ACK != 0 {
                        sender.send(Header::new(ACK, reply.header.id), &[], &[]).unwrap();
                    }
                }
                if let Some(header) = headers.next() {
                    sender.send(header, payload, &[]).unwrap();
                }
            }
        });
        // The daemon's, and the reply this reads, which its receiver holds as it does.
        (kind.to_owned(), held <= credit(0) + MAX_PAYLOAD, held >> 20)
    };
    let stream = (MAX_STREAM_BYTES / len) as usize;
    let many = 16;
    let reads = (0..many).map(|id| {
        Header::new(READ, 10 + id).with_body(&ReadBody {
            cell,
            sequence: 1,
            offset: 0,
            len,
            transfer_at: 0,
        })
    });
    let into_area = (0..many).map(|id| {
        Header::new(READ, 100 + id)
            .with_body(&ReadBody { cell, sequence: 1, offset: 0, len, transfer_at: 0 })
            .with_flags(READ_TRANSFER)
    });
    let from_area = (0..many).map(|id| {
        Header::new(WRITE, 200 + id)
            .with_body(&WriteBody {
                cell,
                sequence: 2 + id,
                offset: 0,
                transfer_at: 0,
                data_len: len,
            })
            .with_flags(WRITE_TRANSFER)
    });
    let writes = (0..many).map(|id| {
        Header::new(WRITE, 300 + id).with_body(&WriteBody {
            cell,
            sequence: 20 + id,
            data_len: len,
            ..Default::default()
        })
    });
    let roots = (0..many).map(|id| Header::new(ROOT, 400 + id));
    // TASK-614 (board 2624): 64 MiB FREE_MANY and MAP_MANY bursts stay within credit(0) + MAX_PAYLOAD.
    let free_many_payload: Vec<u8> = (0..(len / 16))
        .flat_map(|i| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&cell.to_le_bytes());
            bytes[8..].copy_from_slice(&(10_000 + i).to_le_bytes());
            bytes
        })
        .collect();
    let free_many_in_flight = (MAX_STREAM_BYTES / (2 * len)) as usize;
    let map_many_in_flight =
        (MAX_STREAM_BYTES / (crate::codec::MAP_MANY_HELD * len)).max(1) as usize;
    let free_manys: Vec<_> = (0..free_many_in_flight as u64)
        .map(|id| Header::new(FREE, 500 + id).with_body(&ExtentBody { cell, sequence: 9_999 + id }))
        .collect();
    let map_many_mappings: Vec<_> = (0..map_many_count)
        .map(|_| cladefs_core::WindowMapping {
            window_offset: 0,
            len: ps,
            contents: cladefs_core::MappingContents::Zeros,
        })
        .collect();
    let map_many_payload = crate::codec::mappings_payload(&map_many_mappings);
    let map_manys: Vec<_> = (0..2u64).map(|id| Header::new(MAP, 600 + id)).collect();
    let held = [
        burst("roots", roots.collect(), &[], many as usize),
        burst("reads", reads.collect(), &[], stream),
        burst("reads into the area", into_area.collect(), &[], many as usize),
        burst("writes from the area", from_area.collect(), &[], many as usize),
        burst("writes", writes.collect(), &vec![9; len as usize], stream),
        burst("free_many", free_manys, &free_many_payload, free_many_in_flight),
        burst("map_many", map_manys, &map_many_payload, map_many_in_flight),
    ];
    let within: Vec<_> = held.iter().map(|(kind, within, _)| (kind.as_str(), *within)).collect();
    assert_eq!(
        within,
        [
            ("roots", true),
            ("reads", true),
            ("reads into the area", true),
            ("writes from the area", true),
            ("writes", true),
            ("free_many", true),
            ("map_many", true),
        ],
        "MiB held past the credit's {} MiB and a reply's: {held:?}",
        credit(0) >> 20,
    );
}

/// TASK-149.34: the page-ins a connection's faults keep in flight, each a part of a cold extent
/// read into its tenant's memory, the part's frames held until its fetch from the bucket answers,
/// are bounded by the connection, not by how many faults it has in flight, as a fault holds no
/// worker while it waits for the pager (TASK-92.20). Here faults on a page of each of 256 extents
/// of a MiB, the snapshot's version of another host's, all in the bucket alone, whose reads are
/// held.
#[tokio::test(flavor = "multi_thread")]
async fn page_ins_in_flight_are_bounded() {
    use cladefs_core::{OpenCell, VolatileSlice};
    const EXTENTS: u64 = 256;
    const MIB: u64 = 1 << 20;
    host();
    let gated = Arc::new(super::GatedReads::new());
    crate::cells::use_bucket(
        bucket_cell::object_bucket::ObjectBucket::new(gated.clone(), "cells").unrouted(),
    );
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    // A version of the extents, made here, restored on another host, which has them in the bucket
    // alone.
    let path = dir.path().join("store");
    let cell = {
        let open_cell = OpenCell::create(&path, 1 << 30).await.unwrap();
        let cell = open_cell.cell();
        for sequence in 1..=EXTENTS {
            let data = super::pattern(sequence as u8, MIB as usize);
            open_cell
                .write(ExtentId { cell: cell.id, sequence }, 0, &[VolatileSlice::from(&data[..])])
                .await
                .unwrap();
        }
        open_cell.commit(None).await.unwrap();
        cell
    };
    let client = CellClient::connect(&socket, &path, 1 << 30).unwrap();
    let version = client.make_version().unwrap();
    drop(client);
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let restored = elsewhere.join("store");
    let forked = CellClient::connect_forked(&socket, &restored, version, 1 << 30);
    drop(forked.unwrap());

    let (sender, receiver, _stream, _) = super::raw_connection(&socket, &restored, 1 << 30);
    let sender = Arc::new(sender);
    // Its replies, by ID, the stage operations before them acknowledged.
    let (replied, replies) = std::sync::mpsc::channel();
    let acking = sender.clone();
    std::thread::spawn(move || {
        let mut receiver = receiver;
        while let Ok(Some(message)) = receiver.receive(|| u64::MAX) {
            let header = message.header;
            if header.kind == REPLY {
                let _ = replied.send((header.id, header.status));
            } else if header.flags & STAGE_ACK != 0 {
                let _ = acking.send(Header::new(ACK, header.id), &[], &[]);
            }
        }
    });
    // The replies as they come, in any order, by ID.
    let answered = std::cell::RefCell::new(HashMap::new());
    let answer = |id| loop {
        if let Some(status) = answered.borrow_mut().remove(&id) {
            break status;
        }
        let (replied, status) =
            replies.recv_timeout(WAIT).unwrap_or_else(|_| panic!("no reply to request {id}"));
        answered.borrow_mut().insert(replied, status);
    };
    let window = Header::new(WINDOW, 2)
        .with_body(&WindowBody { size: EXTENTS * MIB, budget: 0 })
        .with_flags(WINDOW_COPYING);
    sender.send(window, &[], &[]).unwrap();
    assert_eq!(answer(2), 0);
    for sequence in 1..=EXTENTS {
        let mapping = cladefs_core::WindowMapping {
            window_offset: (sequence - 1) * MIB,
            len: MIB,
            contents: cladefs_core::MappingContents::Extent {
                extent: ExtentId { cell: cell.id, sequence },
                offset: 0,
            },
        };
        let payload = crate::codec::mappings_payload(&[mapping]);
        sender.send(Header::new(MAP, 2 + sequence), &payload, &[]).unwrap();
        assert_eq!(answer(2 + sequence), 0, "MAP {sequence}");
    }
    let arena = crate::server::default_tenant().unwrap().arena();
    // What so many requests at once, all of them in flight, hold of the tenant's memory as their
    // page-ins wait for the bucket, and of the heap as their fetches answer: each a part, cold.
    let burst = |what: &str, headers: Vec<Header>| {
        cladefs_core::cache::evict_least_used(Some(arena), |_| false);
        let (before, frames) = (held(), cladefs_core::slabs::arena_bytes(arena));
        gated.hold(true);
        let ids: Vec<u64> = headers.iter().map(|header| header.id).collect();
        for header in headers {
            sender.send(header, &[], &[]).unwrap();
        }
        // Until no more page-ins begin.
        let mut paging = 0;
        for _ in 0..50 {
            std::thread::park_timeout(Duration::from_millis(200));
            let now = cladefs_core::slabs::arena_bytes(arena).saturating_sub(frames);
            if now == paging && gated.waiting() > 0 {
                break;
            }
            paging = now;
        }
        let heap = held().saturating_sub(before) as u64;
        let peak = peak_while(|| {
            gated.hold(false);
            for id in &ids {
                assert_eq!(answer(*id), 0, "{what} {id}");
            }
        });
        (what.to_owned(), paging >> 20, heap.max(peak) >> 20)
    };
    let faults = (1..=EXTENTS).map(|sequence| {
        let at = (sequence - 1) * MIB;
        Header::new(FAULT, 1000 + sequence).with_body(&FaultBody { offset: at })
    });
    // A page of each, through the stream.
    let reads = (1..=EXTENTS).map(|sequence| {
        Header::new(READ, 2000 + sequence).with_body(&ReadBody {
            cell: cell.id,
            sequence,
            offset: 0,
            len: page_size(),
            transfer_at: 0,
        })
    });
    let held = [burst("faults", faults.collect()), burst("reads", reads.collect())];
    // A connection's page-ins, each a part and those it reads ahead, as many at once as it
    // resolves; and the heap within its credit.
    let paged = (crate::server::PAGE_INS as u64
        * (1 + cladefs_core::cache::PARTS_AHEAD)
        * extent_frame::PART)
        >> 20;
    let credit = credit(EXTENTS * MIB) >> 20;
    println!("MiB of the tenant's memory and of the heap held: {held:?}");
    let within: Vec<_> = held
        .iter()
        .map(|(what, paging, heap)| (what.as_str(), *paging <= paged, *heap <= credit))
        .collect();
    assert_eq!(
        within,
        [("faults", true, true), ("reads", true, true)],
        "MiB of the tenant's memory and of the heap held, past {paged} and {credit}: {held:?}"
    );
}

/// A tenant of the test's, `id`, its cells in `dir`, served at a socket of its own.
fn served_tenant(dir: &std::path::Path, name: &str, id: u64) -> (PathBuf, PathBuf, Arc<Tenant>) {
    let cells = dir.join(name);
    std::fs::create_dir(&cells).unwrap();
    let tenant = Tenant::new(id, &cells).unwrap();
    let socket = dir.join(format!("{name}.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    let served = tenant.clone();
    std::thread::spawn(move || serve_tenant(listener, served));
    (cells, socket, tenant)
}

/// TASK-548: a connection lost partway through a payload the daemon admitted, which made no request
/// to answer, gives back what admitting it took of its tenant's bytes in flight: before, it held
/// them for the tenant's life, and a tenant whose bytes in flight such losses filled had every
/// later stream request of every VM process of its wait for good. Here with the budget a payload's
/// size, a `WRITE` of the most a payload holds, a page of it sent before the connection goes; a
/// new connection's write is answered after.
#[test]
fn connections_lost_mid_payload_hold_none_of_their_tenants_bytes_in_flight() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    crate::memory::set_in_flight(Some(MALLORY), Some(MAX_PAYLOAD)).unwrap();
    {
        let (sender, receiver, stream, cell) =
            super::raw_connection(&socket, &cells.join("store"), 1 << 30);
        let mut written = Header::new(WRITE, 2).with_body(&WriteBody {
            cell,
            sequence: 1,
            data_len: MAX_PAYLOAD,
            ..Default::default()
        });
        written.len = MAX_PAYLOAD;
        let mut sent = &stream;
        std::io::Write::write_all(&mut sent, zerocopy::IntoBytes::as_bytes(&written)).unwrap();
        std::io::Write::write_all(&mut sent, &[7; 4096]).unwrap();
        // Admitted, its bytes taken, before the connection goes: the payload's, not a reply the
        // daemon is sending meanwhile, counted until it is sent, as OPEN's header, 72 bytes
        // (board 6878).
        let deadline = std::time::Instant::now() + WAIT;
        while mallory.in_flight().held() != MAX_PAYLOAD && std::time::Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
        assert_eq!(mallory.in_flight().held(), MAX_PAYLOAD, "admitted");
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        drop((sender, receiver, stream));
    }
    let deadline = std::time::Instant::now() + WAIT;
    while mallory.in_flight().held() > 0 && std::time::Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(10));
    }
    assert_eq!(mallory.in_flight().held(), 0, "the lost connection's bytes in flight");
    let (sender, mut receiver, _stream, cell) =
        super::raw_connection(&socket, &cells.join("other"), 1 << 30);
    let written = Header::new(WRITE, 2).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: 1 << 20,
        ..Default::default()
    });
    let data = super::pattern(1, 1 << 20);
    let answered = within(WAIT, move || super::call(&sender, &mut receiver, written, &data))
        .expect("the write answered");
    assert_eq!(answered.header.status, 0);
}

/// TASK-581: a payload on a kind that carries none, an `ACK`, a `STAGE_COUNTS` or a
/// `STAGE_PRIVATE_COUNTS`, which the daemon counts in no bytes in flight, ends its connection
/// before any of it is held: before, it was read whole, 64 MiB of it at most a connection,
/// uncounted, and held for as long as the VM process sent all but its last byte. Board 8459: so
/// does one past what any kind holds, refused at its header, before any byte is reserved: here
/// an `INFO`'s and a `READ`'s, which carry none, and a kind no request is of. At once, long before
/// [`STALL`] would end a payload that never comes.
#[test]
fn payloads_of_kinds_that_carry_none_end_their_connections() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    let at_once = Duration::from_secs(2);
    assert!(at_once < STALL);
    let kinds = [ACK, STAGE_COUNTS, 33, INFO, READ, 0xffff].map(|kind| (kind, 0));
    // And a root past the most a root holds, which a commit's payload is.
    let kinds = kinds.into_iter().chain([(COMMIT, COMMIT_ROOT)]);
    for (n, (kind, flags)) in kinds.enumerate() {
        let (sender, _receiver, stream, _) =
            super::raw_connection(&socket, &cells.join(format!("store-{n}")), 1 << 30);
        let mut said = Header::new(kind, 1).with_flags(flags);
        said.len = MAX_PAYLOAD;
        let mut sent = &stream;
        std::io::Write::write_all(&mut sent, zerocopy::IntoBytes::as_bytes(&said)).unwrap();
        // The daemon may end the connection on the header alone, before the payload is sent: what
        // is checked, so a payload refused so is no failure (board 7053).
        match std::io::Write::write_all(&mut sent, &[7; 4096]) {
            Ok(()) => {}
            Err(err) if super::shut(&err) => {}
            Err(err) => panic!("sending the payload: {err}"),
        }
        super::assert_let_go(&sender, &stream, at_once);
        assert_eq!(mallory.in_flight().held(), 0, "a {kind}'s payload held");
    }
}

/// TASK-581 (board 2558): eight connections of one tenant each sending an `ACK` header with
/// `len = MAX_PAYLOAD` and partial payload bytes (stalling before the last byte) end immediately
/// without holding the uncounted payload: the daemon's heap peak stays within the tenant's
/// in-flight budget plus slack.
#[test]
fn ack_payloads_on_eight_connections_stay_within_budget() {
    const CONNECTIONS: usize = 8;
    const IN_FLIGHT: u64 = MAX_PAYLOAD;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    crate::memory::set_in_flight(Some(MALLORY), Some(IN_FLIGHT)).unwrap();
    let at_once = Duration::from_secs(2);
    assert!(at_once < STALL);
    let held = peak_while(|| {
        let mut conns = Vec::with_capacity(CONNECTIONS);
        for n in 0..CONNECTIONS {
            let (sender, _receiver, stream, _) =
                super::raw_connection(&socket, &cells.join(format!("ack-store-{n}")), 1 << 30);
            let mut said = Header::new(ACK, 1);
            said.len = MAX_PAYLOAD;
            let mut sent = &stream;
            std::io::Write::write_all(&mut sent, zerocopy::IntoBytes::as_bytes(&said)).unwrap();
            // Send partial payload bytes (fewer than MAX_PAYLOAD) and stall; the daemon closes
            // before allocating or reading the payload.
            match std::io::Write::write_all(&mut sent, &[0x5a; 4096]) {
                Ok(()) => {}
                Err(err) if super::shut(&err) => {}
                Err(err) => panic!("sending partial ACK payload: {err}"),
            }
            conns.push((sender, stream));
        }
        for (sender, stream) in &conns {
            super::assert_let_go(sender, stream, at_once);
        }
    });
    assert_eq!(mallory.in_flight().held(), 0);
    assert!(
        held <= IN_FLIGHT + SLACK,
        "heap peak {} MiB exceeded tenant budget + slack ({} MiB)",
        held >> 20,
        (IN_FLIGHT + SLACK) >> 20,
    );
}

/// TASK-546: a reply to `MAPPINGS` is counted as its request is admitted, as many mappings as its
/// window may hold, within the connection's credit, which holds one: a VM process asking for
/// them over and over, their replies unread, holds the daemon to its credit, and is let go once it
/// passes it. Before, each was admitted counted as nothing, before any reply was made: here
/// 256 of 32 MiB each, 8 GiB, queued at once.
#[test]
fn mappings_asked_for_past_the_credit_let_their_vm_process_go() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, _mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    let (sender, mut receiver, stream, _) =
        super::raw_connection(&socket, &cells.join("store"), 1 << 30);
    // A window every other page of which is mapped, zeros: their reply holds 32 MiB.
    let (ps, count) = (page_size(), (32 << 20) / MAPPING_BYTES);
    let size = 2 * count * ps;
    let window = Header::new(WINDOW, 2)
        .with_body(&WindowBody { size, budget: 0 })
        .with_flags(WINDOW_COPYING);
    assert_eq!(super::call(&sender, &mut receiver, window, &[]).header.status, 0);
    let mapping = |n: u64| cladefs_core::WindowMapping {
        window_offset: 2 * n * ps,
        len: ps,
        contents: cladefs_core::MappingContents::Zeros,
    };
    let most = MAX_PAYLOAD / MAPPING_BYTES;
    let mut id = 3;
    for first in (0..count).step_by(most as usize) {
        let mappings: Vec<_> = (first..count.min(first + most)).map(mapping).collect();
        let payload = crate::codec::mappings_payload(&mappings);
        sender.send(Header::new(MAP, id), &payload, &[]).unwrap();
        // Its window's changes acknowledged as they come, then its reply.
        loop {
            let message = receiver.receive(|| 0).unwrap().unwrap();
            if message.header.kind == REPLY {
                assert_eq!(message.header.status, 0);
                break;
            }
            if message.header.flags & STAGE_ACK != 0 {
                sender.send(Header::new(ACK, message.header.id), &[], &[]).unwrap();
            }
        }
        id += 1;
    }
    let credit = credit(size);
    let held = peak_while(|| {
        let asked = (0..256).map(|n| Header::new(MAPPINGS, id + n));
        super::send_until_let_go(&sender, asked);
        // TASK-677: before it is let go, the daemon makes two replies of a million mappings, 32
        // MiB each, the third past its credit: 8.7 to 8.8 s from the first asked, alone, in a
        // debug build on the test host (2026-11-20), so a loaded run passed the 10 s it once had.
        // Within what any of the case's steps may take, as the rest of them are.
        super::assert_let_go(&sender, &stream, WAIT);
    });
    assert!(held <= credit + SLACK, "{} MiB held, past {} MiB", held >> 20, credit >> 20);
}

/// TASK-149.35: what the daemon holds of one tenant's VM processes' requests is bounded by the
/// bytes in flight its connections share, not by how many connections it has: each connection has
/// a credit of its own (272 MiB without a window), and a tenant as many as 1,024, so one tenant,
/// within the protocol, held some 272 GiB, every tenant's daemon with it. Here many of mallory's
/// connections, of one tenant, each flood the daemon with reads whose replies they leave unread,
/// filling the connection's queue, which the tenant's bytes in flight bound across them; alice,
/// another tenant, flooding as hard, is unaffected, her bytes in flight her own.
#[test]
fn a_tenants_in_flight_data_is_bounded() {
    const CONNECTIONS: usize = 8;
    const MIB: u64 = 1 << 20;
    // Bytes in flight a payload's size at least; small, so the green run holds little.
    const IN_FLIGHT: u64 = MAX_PAYLOAD;
    host();
    let dir = tempfile::tempdir().unwrap();
    // Two tenants, each flooding: mallory's many connections, alice's one, their bytes in flight
    // their own.
    let tenant = |name: &str, id: u64| {
        let cells = dir.path().join(name);
        std::fs::create_dir(&cells).unwrap();
        let tenant = Tenant::new(id, &cells).unwrap();
        let socket = dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        let served = tenant.clone();
        std::thread::spawn(move || serve_tenant(listener, served));
        (cells, socket, tenant)
    };
    let (mdir, msocket, mallory) = tenant("mallory", MALLORY);
    let (adir, asocket, alice) = tenant("alice", ALICE);
    crate::memory::set_in_flight(Some(MALLORY), Some(IN_FLIGHT)).unwrap();
    crate::memory::set_in_flight(Some(ALICE), Some(IN_FLIGHT)).unwrap();

    // A connection flooding reads of an extent of its own, their replies unread, on a thread of
    // its own: the daemon stops reading it once the tenant's bytes in flight are full, which blocks
    // its synchronous sends, as a real VM process's socket fills. It holds its sockets open, and
    // never returns: the test's process ends with it (nextest runs each in its own).
    fn flood(socket: PathBuf, path: PathBuf) {
        std::thread::spawn(move || {
            let (sender, mut receiver, _stream, cell) =
                super::raw_connection(&socket, &path, 1 << 30);
            let written = Header::new(WRITE, 2).with_body(&WriteBody {
                cell,
                sequence: 1,
                data_len: MIB,
                ..Default::default()
            });
            sender.send(written, &super::pattern(1, MIB as usize), &[]).unwrap();
            assert_eq!(receiver.receive(|| 0).unwrap().unwrap().header.status, 0);
            // Reads past any room, none of whose replies it reads: what the daemon admits of them
            // it queues, held in the tenant's bytes in flight; the rest block here, as a full
            // socket does.
            for id in 0..300u64 {
                let read = Header::new(READ, 100 + id).with_body(&ReadBody {
                    cell,
                    sequence: 1,
                    offset: 0,
                    len: MIB,
                    transfer_at: 0,
                });
                if sender.send(read, &[], &[]).is_err() {
                    break;
                }
            }
            std::thread::park();
            drop((sender, receiver));
        });
    }

    let before = held();
    // Mallory's connections flood at once; their queues fill as far as the tenant's bytes in
    // flight let them.
    for n in 0..CONNECTIONS {
        flood(msocket.clone(), mdir.join(format!("store-{n}")));
    }
    let deadline = std::time::Instant::now() + WAIT;
    let (mut queued, mut stable) = (0, 0);
    while std::time::Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(100));
        let now = mallory.in_flight().held();
        stable = if now == queued && now > 0 { stable + 1 } else { 0 };
        queued = now;
        if stable >= 5 {
            break;
        }
    }
    let heap = (held().saturating_sub(before)) >> 20;
    let (mallory_held, mallory_peak) =
        (mallory.in_flight().held() >> 20, mallory.in_flight().peak() >> 20);

    // Alice, flooding as hard on her own bytes in flight, is served: her write commits and her read
    // answers.
    let (asender, mut areceiver, _astream, acell) =
        super::raw_connection(&asocket, &adir.join("store"), 1 << 24);
    let awritten = Header::new(WRITE, 2).with_body(&WriteBody {
        cell: acell,
        sequence: 1,
        data_len: MIB,
        ..Default::default()
    });
    let alice_write =
        super::call(&asender, &mut areceiver, awritten, &super::pattern(2, MIB as usize))
            .header
            .status;
    let alice_commit =
        super::call(&asender, &mut areceiver, Header::new(COMMIT, 3), &[]).header.status;
    let alice_read = super::call(
        &asender,
        &mut areceiver,
        Header::new(READ, 4).with_body(&ReadBody {
            cell: acell,
            sequence: 1,
            offset: 0,
            len: MIB,
            transfer_at: 0,
        }),
        &[],
    );
    let alice_budget = alice.in_flight().budget();

    println!(
        "mallory's {CONNECTIONS} connections held {mallory_held} MiB (peak {mallory_peak}), {heap}          MiB of the heap, of {} MiB in flight; alice's {} MiB in flight, her write {alice_write}, commit {alice_commit}, her read          {} bytes",
        IN_FLIGHT >> 20,
        alice_budget >> 20,
        alice_read.payload.len(),
    );
    // Mallory's tenant holds no more than its bytes in flight, a connection's own slack over them
    // at most (one request admitted as they emptied): far less than its connections' credits
    // together.
    assert!(
        mallory.in_flight().peak() <= IN_FLIGHT + MAX_PAYLOAD,
        "mallory's tenant held {mallory_peak} MiB, past its {} MiB in flight",
        IN_FLIGHT >> 20,
    );
    // Alice, another tenant, flooding too, is unaffected: served throughout.
    assert_eq!(
        (alice_write, alice_commit, alice_read.header.status),
        (0, 0, 0),
        "alice was held up"
    );
    assert_eq!(alice_read.payload, super::pattern(2, MIB as usize), "alice read other data");
}

/// Whether `ready` holds within `wait`, looked at every 10 ms.
fn comes_true(wait: Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + wait;
    while !ready() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
    true
}

/// Whether the daemon hung `stream` up within `wait`: let its VM process go, or ended its
/// connection.
fn hung_up(stream: &UnixStream, wait: Duration) -> bool {
    let mut hung_up = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
    let wait = libc::c_int::try_from(wait.as_millis()).unwrap();
    // SAFETY: Polls the stream, which the caller keeps open, for `wait` at most.
    let ready = unsafe { libc::poll(&mut hung_up, 1, wait) };
    ready == 1 && hung_up.revents & libc::POLLRDHUP != 0
}

/// Board 8459: a connection that stays open and idle holds none of its tenant's bytes in flight
/// past [`STALL`]: neither one that sent a header and none of the payload it was admitted, nor one
/// that leaves the replies to its reads unread. Here 128 of mallory's connections each send a
/// `WRITE`'s header claiming the most a payload holds, 8 GiB together, the tenant's whole room,
/// and nothing more, their sockets open: they are ended, the tenant holds nothing, and another VM
/// process of its opens its cell and writes. Then 16 connections each read 4 MiB, leaving the
/// replies unread, far within their credits: each is let go, and the tenant holds nothing. Before,
/// both held their bytes for good, the tenant's every later request with stream data, an `OPEN`'s
/// path too, waiting for them, and nothing ended them.
#[test]
fn idle_connections_hold_none_of_their_tenants_bytes_in_flight() {
    const IDLE: u64 = 128;
    const UNREAD: usize = 16;
    const READ_BYTES: u64 = 4 << 20;
    // What the daemon takes to end them: the bound, a look of its watchdog's, and slack.
    let ended = STALL + Duration::from_secs(10);
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    assert_eq!(mallory.in_flight().budget(), IDLE * MAX_PAYLOAD, "the tenant's room");
    // Headers alone, of no cell: admitted, their bytes taken, before their payloads are read.
    let idle: Vec<UnixStream> = (0..IDLE)
        .map(|n| {
            let stream = UnixStream::connect(&socket).unwrap();
            let mut said = Header::new(WRITE, 1 + n);
            said.len = MAX_PAYLOAD;
            std::io::Write::write_all(&mut &stream, said.as_bytes()).unwrap();
            stream
        })
        .collect();
    let full = comes_true(WAIT, || mallory.in_flight().held() == IDLE * MAX_PAYLOAD);
    assert!(full, "{} MiB admitted", mallory.in_flight().held() >> 20);
    // Another VM process of the tenant's: its OPEN's path waits for room, which comes back.
    let opening = (socket.clone(), cells.join("store"));
    let opened = within(ended, move || super::raw_connection(&opening.0, &opening.1, 1 << 30));
    let (sender, mut receiver, _stream, cell) = opened.expect("the cell opened");
    let written = Header::new(WRITE, 2).with_body(&WriteBody {
        cell,
        sequence: 1,
        data_len: 1 << 20,
        ..Default::default()
    });
    let data = super::pattern(1, 1 << 20);
    let answered = within(ended, move || super::call(&sender, &mut receiver, written, &data))
        .expect("the write answered");
    assert_eq!(answered.header.status, 0);
    for (n, stream) in idle.iter().enumerate() {
        assert!(hung_up(stream, ended), "idle connection {n} still open");
    }
    assert!(comes_true(ended, || mallory.in_flight().held() == 0), "the idle connections' bytes");
    drop(idle);

    // Replies left unread, by connections that stay open.
    let unread: Vec<_> = (0..UNREAD)
        .map(|n| {
            let path = cells.join(format!("unread-{n}"));
            let (sender, mut receiver, stream, cell) =
                super::raw_connection(&socket, &path, 1 << 30);
            let written = Header::new(WRITE, 2).with_body(&WriteBody {
                cell,
                sequence: 1,
                data_len: READ_BYTES,
                ..Default::default()
            });
            let data = super::pattern(n as u8, READ_BYTES as usize);
            assert_eq!(super::call(&sender, &mut receiver, written, &data).header.status, 0);
            let read = Header::new(READ, 3).with_body(&ReadBody {
                cell,
                sequence: 1,
                offset: 0,
                len: READ_BYTES,
                transfer_at: 0,
            });
            sender.send(read, &[], &[]).unwrap();
            (sender, receiver, stream)
        })
        .collect();
    let queued = comes_true(WAIT, || crate::server::most_queued() > 0);
    assert!(queued, "no reply queued");
    for (n, (sender, _, stream)) in unread.iter().enumerate() {
        assert!(hung_up(stream, ended), "connection {n} leaving its reply unread kept");
        let sent = sender.send(Header::new(INFO, 4), &[], &[]);
        assert!(sent.as_ref().is_err_and(super::shut), "sent once let go: {sent:?}");
    }
    assert!(comes_true(ended, || mallory.in_flight().held() == 0), "the unread replies' bytes");
}

/// Board 8463: an acknowledgement of a stage operation past the last the daemon sent lets its VM
/// process go at once, whatever it names: the one after the last, `u64::MAX - 1`, `u64::MAX`.
/// Before, it was taken for every operation sent after it made, so that what took pages away
/// was taken for made before it was, the pages freed while the VM process may map them, and the
/// watchdog never found it silent: let go here only at once, as the watchdog would let it go
/// later. One of 0, before any sent, takes nothing for made: the VM process, frozen with an unmap
/// unacknowledged, keeps the pages an eviction takes from it quarantined, and is let go as silent.
#[test]
fn acknowledgements_of_operations_never_sent_let_their_vm_process_go() {
    const EXTENT: u64 = 2 << 20;
    host();
    let dir = tempfile::tempdir().unwrap();
    let (cells, socket, mallory) = served_tenant(dir.path(), "mallory", MALLORY);
    let frozen = |n: usize| {
        let mut vm = super::HandVm::open(&socket, &cells.join(format!("store-{n}")));
        let its = vm.write_an_extent(EXTENT);
        let pages = vm.map_an_extent(its, EXTENT);
        let last = vm.freeze_with_an_unmap(its, EXTENT);
        (vm, pages, last)
    };
    // Well within what the watchdog lets it be silent for.
    let at_once = Duration::from_secs(2);
    assert!(at_once < crate::server::UNRESPONSIVE);
    for (n, past) in [None, Some(u64::MAX - 1), Some(u64::MAX)].into_iter().enumerate() {
        let (vm, _, last) = frozen(n);
        let acked = past.unwrap_or(last + 1);
        vm.ack(acked);
        assert!(hung_up(&vm.stream, at_once), "acknowledging {acked}, {last} the last sent, kept");
    }
    let (mut vm, pages, _) = frozen(3);
    vm.ack(0);
    // Served on.
    vm.call(INFO, &(), &[], false);
    // An eviction of its extent, which it may map: its pages wait for it.
    cladefs_core::cache::evict_least_used(Some(mallory.arena()), |_| false);
    let free = |page: &cladefs_core::pages::PhysLocation| {
        cladefs_core::slabs::is_free(*page) == Some(true)
    };
    assert!(!pages.is_empty() && !pages.iter().any(free), "freed while it may map them");
    // Silent, it is let go, and they are free.
    assert!(hung_up(&vm.stream, WAIT), "a VM process silent kept");
    assert!(comes_true(WAIT, || pages.iter().all(free)), "its pages kept once it was let go");
}

/// Board 8465: what tenants hold of the daemon together is bounded, however many there are, and
/// leaves another tenant room: three tenants each connecting past their share of the most
/// connections the daemon serves at once (12 here, 2 kept for each tenant alive) hold no more than
/// the daemon serves, and a fourth tenant's VM process connects, opens its cell and commits; then
/// they flood past their rooms with payloads that never come, and hold no more bytes in flight
/// than every tenant's together may. Before, each tenant's limits were its own, every tenant's
/// together their sum.
#[test]
fn tenants_together_are_bounded_and_leave_another_room() {
    const MOST: usize = 12;
    const KEPT: usize = 2;
    const EACH: usize = 8;
    let limits = crate::server::CONNECTION_LIMITS_GIVEN.set((Some(MOST), KEPT));
    assert!(limits.is_ok(), "the limits given");
    host();
    let dir = tempfile::tempdir().unwrap();
    let names = ["mallory", "trudy", "eve", "alice"];
    let tenants: Vec<_> = (names.iter().enumerate())
        .map(|(n, name)| served_tenant(dir.path(), name, 10 + n as u64))
        .collect();
    let total = crate::memory::total_in_flight_on(crate::Daemon::process());
    crate::memory::set_total_in_flight(Some(2 * MAX_PAYLOAD)).unwrap();
    for n in 0..3 {
        crate::memory::set_in_flight(Some(10 + n), Some(MAX_PAYLOAD)).unwrap();
    }
    // Each of the three connects past its share, in turn: the first takes what the others' shares
    // leave it, the fourth's among them; the others their shares alone.
    let shares = [MOST - 3 * KEPT, KEPT, KEPT];
    let mut floods = Vec::new();
    for ((_, socket, tenant), share) in tenants.iter().zip(shares) {
        floods.extend((0..EACH).map(|_| UnixStream::connect(socket).unwrap()));
        let served = comes_true(WAIT, || tenant.connections() == share);
        assert!(served, "{} of {share} connections served", tenant.connections());
    }
    // Refused, the rest are closed.
    let refused = || floods.iter().filter(|stream| hung_up(stream, Duration::ZERO)).count();
    let expected = 3 * EACH - shares.iter().sum::<usize>();
    assert!(comes_true(WAIT, || refused() == expected), "{} refused of {expected}", refused());
    // The fourth's VM process connects, within its share, opens its cell, and commits.
    let (cells, socket, _) = &tenants[3];
    let opening = (socket.clone(), cells.join("store"));
    let (sender, mut receiver, _stream, _) =
        within(WAIT, move || super::raw_connection(&opening.0, &opening.1, 1 << 24))
            .expect("the fourth tenant's cell opened");
    let committed = within(WAIT, move || {
        super::call(&sender, &mut receiver, Header::new(COMMIT, 2), &[]).header.status
    });
    assert_eq!(committed, Ok(0));
    // Each connection served sends a header of the most a payload holds, past its tenant's room,
    // and nothing more: within every tenant's together, those past it waiting, as past a
    // tenant's room, until those admitted are ended (`STALL`).
    for (n, stream) in floods.iter().enumerate() {
        let mut said = Header::new(WRITE, 1 + n as u64);
        said.len = MAX_PAYLOAD;
        // Refused, its connection closed: nothing to send.
        let _ = std::io::Write::write_all(&mut &*stream, said.as_bytes());
    }
    assert!(comes_true(WAIT, || total.held() == 2 * MAX_PAYLOAD), "{} held", total.held());
    assert!(total.peak() <= 2 * MAX_PAYLOAD, "{} MiB held at once", total.peak() >> 20);
    for (n, (_, _, tenant)) in tenants[..3].iter().enumerate() {
        let peak = tenant.in_flight().peak();
        assert!(peak <= MAX_PAYLOAD, "tenant {n} held {} MiB at once", peak >> 20);
    }
    drop(floods);
}

/// board 9975: a tenant's cells' files leave another tenant room, under a low limit of open files:
/// mallory's VM processes open as many cells as the daemon lets them, each writing and committing
/// extents, so that its cell opens files of its own; then alice's VM process, within its tenant's
/// share, connects, opens its cell and commits. Before, the daemon counted 4 files a connection,
/// its cell's as the rest's: mallory's cells took every file the process may open far below the
/// most connections it served, and alice's failed (`EMFILE`). The limit is what this process has
/// open, four times over, and room for a few connections at their most besides: the test's own
/// VM processes' files are this process's too.
#[test]
fn a_tenants_cells_files_leave_another_tenant_room() {
    const KEPT: usize = 2;
    /// The cells mallory tries to open at most, far more than a limit this low holds.
    const TRIES: usize = 512;
    /// Each one's extents, a block each, committed in fours.
    const EXTENTS: u64 = 16;
    let limits = crate::server::CONNECTION_LIMITS_GIVEN.set((None, KEPT));
    assert!(limits.is_ok(), "the limits given");
    host();
    let dir = tempfile::tempdir().unwrap();
    let (mallorys, mallory_socket, mallory) = served_tenant(dir.path(), "mallory", 10);
    let (alices, alice_socket, _) = served_tenant(dir.path(), "alice", 11);
    let open = || std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let per_connection = crate::cell_open_files();
    let files = (4 * (open() + 64)).max(4 * (2 * KEPT as u64 + 2) * per_connection / 3 + 4);
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    let lowered = libc::rlimit { rlim_cur: files, ..limit };
    // SAFETY: Lowers the process's own limit, a test's process's alone.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
    let write_extents = |client: &CellClient| -> Result<(), Errno> {
        for sequence in 1..=EXTENTS {
            let extent = ExtentId { cell: client.cell(), sequence };
            write(client, extent, 0, &[sequence as u8; BLOCK])?;
            if sequence % 4 == 0 {
                client.commit(None)?;
            }
        }
        Ok(())
    };
    // Mallory's, until one is refused, or fails for want of files.
    let mut mallorys_clients = Vec::new();
    for n in 0..TRIES {
        let opened =
            CellClient::connect(&mallory_socket, mallorys.join(format!("{n}")), CELL_BYTES);
        let Ok(client) = opened else { break };
        let written = write_extents(&client);
        mallorys_clients.push(client);
        if written.is_err() {
            break;
        }
    }
    assert!(mallory.connections() > KEPT, "{} of mallory's served", mallory.connections());
    assert!(mallorys_clients.len() < TRIES, "every one of mallory's {TRIES} served");
    let alice = CellClient::connect(&alice_socket, alices.join("store"), CELL_BYTES);
    let alice = alice.unwrap_or_else(|err| {
        panic!("alice's cell, beside mallory's {}: {err:#}", mallory.connections())
    });
    assert_eq!(write_extents(&alice), Ok(()));
    assert!(open() < files, "{} files open of {files}", open());
    drop((alice, mallorys_clients));
    // SAFETY: As above, its limit as it was.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

/// board 12555, board 12887: a tenant's extents written large, uncommitted and then committed,
/// leave another tenant room under a low limit of open files: mallory's one VM process writes a
/// block ending at `large_extent` in each of more extents than the process may open files, then
/// commits them under the low limit; alice's VM process connects, opens its cell and commits both
/// before and after mallory's commit. Before board 12555, each uncommitted large extent held a raw
/// file open; before board 12887, committing held every sealed own file open in `OwnFiles::unsynced`
/// / `Syncs` until its flush (`EMFILE`).
#[test]
fn a_tenants_uncommitted_large_extents_leave_another_tenant_room() {
    const KEPT: usize = 2;
    // Alice's extents, a block each, committed in fours.
    const ALICES: u64 = 8;
    let limits = crate::server::CONNECTION_LIMITS_GIVEN.set((Some(4), KEPT));
    assert!(limits.is_ok(), "the limits given");
    host();
    let dir = tempfile::tempdir().unwrap();
    let (mallorys, mallory_socket, _) = served_tenant(dir.path(), "mallory", 10);
    let (alices, alice_socket, _) = served_tenant(dir.path(), "alice", 11);
    let open = || std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    // Room for two connections at their most, mallory's and alice's, beside what is open.
    let files = open() + 2 * crate::cell_open_files();
    let large = cladefs_core::host_cell::options().large_extent;
    // As many extents as the process may open files, each as large as `large_extent` makes it.
    let extents = files;
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    let lowered = libc::rlimit { rlim_cur: files, ..limit };
    // SAFETY: Lowers the process's own limit, a test's process's alone.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
    let mallory = CellClient::connect(&mallory_socket, mallorys.join("store"), extents * large);
    let mallory = mallory.unwrap();
    let extent = |sequence| ExtentId { cell: mallory.cell(), sequence };
    let block = |sequence: u64| [sequence as u8; BLOCK];
    for sequence in 1..=extents {
        let written = write(&mallory, extent(sequence), large - BLOCK as u64, &block(sequence));
        assert_eq!(written, Ok(()), "mallory's extent {sequence} of {extents}, {} open", open());
    }
    let alice = CellClient::connect(&alice_socket, alices.join("store"), CELL_BYTES);
    let alice = alice.unwrap_or_else(|err| panic!("alice's cell, {} files open: {err:#}", open()));
    for sequence in 1..=ALICES {
        let extent = ExtentId { cell: alice.cell(), sequence };
        assert_eq!(write(&alice, extent, 0, &block(sequence)), Ok(()), "alice's {sequence}");
        if sequence % 4 == 0 {
            assert_eq!(alice.commit(None), Ok(()), "alice's commit at {sequence}");
        }
    }
    assert!(open() < files, "{} files open of {files}", open());
    // board 12887: committing mallory's `extents` large extents under the low limit stays bounded
    // and leaves alice room to keep committing.
    assert_eq!(mallory.commit(None), Ok(()), "mallory's commit under low RLIMIT_NOFILE");
    let alice_after = ExtentId { cell: alice.cell(), sequence: ALICES + 1 };
    assert_eq!(write(&alice, alice_after, 0, &block(ALICES + 1)), Ok(()));
    assert_eq!(alice.commit(None), Ok(()), "alice's commit after mallory's");
    assert!(open() < files, "{} files open of {files} after commit", open());
    for sequence in [1, extents / 2, extents] {
        let read = read(&mallory, extent(sequence), large - BLOCK as u64, BLOCK);
        assert_eq!(read, Ok(block(sequence).to_vec()), "mallory's extent {sequence}, committed");
    }
    drop((alice, mallory));
    // SAFETY: As above, its limit as it was.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

/// board 12887 (comment 12929): verifies `cladefsd::cell_open_files()` covers every open-file kind
/// a single cell holds simultaneously — closed segments' cache (`Segments::files`), sealed own
/// extents' cache (`OwnFiles::open`), open large extents' raw files (`Uncommitted`), plus segment
/// raw/log, directory lock, connection sockets, and transient syncs. Uses real `most_connections()`
/// (no `CONNECTION_LIMITS_GIVEN`) and asserts both that the actual FD delta of one fully-populated
/// connection stays `<= cell_open_files()` and that a second connection still opens and commits
/// under `open() + most_connections() * cell_open_files()`.
#[test]
fn a_cell_filling_every_open_file_kind_stays_within_cell_open_files() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let (mallorys, mallory_socket, _) = served_tenant(dir.path(), "mallory", 10);
    let (alices, alice_socket, _) = served_tenant(dir.path(), "alice", 11);
    let open = || std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let per_cell = crate::cell_open_files();
    let open_files = cladefs_core::host_cell::options().open_files as u64;
    let large = cladefs_core::host_cell::options().large_extent;
    // Choose a limit where real `most_connections() = (files - files / 4) / cell_open_files()`
    // admits at least 2 connections (no `CONNECTION_LIMITS_GIVEN`), sized from `open()` and
    // `most_connections() * cell_open_files()`.
    let base = open();
    let files = (base + 2 * per_cell).max(4 * (2 * per_cell) / 3 + 4);
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: Reads the process's limit into `limit`.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    let lowered = libc::rlimit { rlim_cur: files, ..limit };
    // SAFETY: Lowers the process's own limit, a test's process's alone.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
    let allowed = crate::server::most_connections() as u64;
    assert!(allowed >= 2, "expected at least 2 connections from most_connections(), got {allowed}");
    let budget = base + allowed * per_cell;

    let capacity = (3 * open_files + 64) * large;
    let mallory = CellClient::connect(&mallory_socket, mallorys.join("store"), capacity).unwrap();
    let extent = |sequence| ExtentId { cell: mallory.cell(), sequence };
    let block = |sequence: u64| [sequence as u8; BLOCK];
    let mut seq = 1u64;

    // 1. Fill closed segments LRU (`Segments::files`) to `open_files`: each `make_version` closes
    //    the open segment, then reading every segment brings `open_files` framed segment files open.
    let seg_start = seq;
    for _ in 0..open_files + 2 {
        assert_eq!(write(&mallory, extent(seq), 0, &block(seq)), Ok(()));
        assert_eq!(mallory.commit(None), Ok(()));
        mallory.make_version().unwrap();
        seq += 1;
    }
    let seg_end = seq;

    // 2. Fill sealed own-extents LRU (`OwnFiles::open`) to `open_files` and exercise commit syncs.
    let own_start = seq;
    for _ in 0..open_files + 4 {
        assert_eq!(write(&mallory, extent(seq), large - BLOCK as u64, &block(seq)), Ok(()));
        seq += 1;
    }
    let own_end = seq;
    assert_eq!(mallory.commit(None), Ok(()));

    // Read every closed segment and every sealed own extent so both LRU caches sit at full bound.
    for s in seg_start..seg_end {
        assert_eq!(read(&mallory, extent(s), 0, BLOCK), Ok(block(s).to_vec()));
    }
    for s in own_start..own_end {
        assert_eq!(read(&mallory, extent(s), large - BLOCK as u64, BLOCK), Ok(block(s).to_vec()));
    }

    // 3. Fill uncommitted open large-extent raw files (`Uncommitted` `Packing::Own`) to `open_files`
    //    plus overflow into segments.
    for _ in 0..open_files + 8 {
        assert_eq!(write(&mallory, extent(seq), large - BLOCK as u64, &block(seq)), Ok(()));
        seq += 1;
    }

    // With all three `open_files` caches and open/prepared/connection handles held at once,
    // one connection's FD growth must stay within `cell_open_files()`.
    let used_by_one = open().saturating_sub(base);
    assert!(
        used_by_one <= per_cell,
        "one connection holding every kind used {used_by_one} FDs, past cell_open_files() = {per_cell}"
    );
    assert_eq!(mallory.commit(None), Ok(()));

    // A second tenant's connection still fits cleanly under the real `most_connections()` limit.
    let alice = CellClient::connect(&alice_socket, alices.join("store"), CELL_BYTES)
        .unwrap_or_else(|err| panic!("alice's cell, {} open of {files}: {err:#}", open()));
    assert_eq!(write(&alice, ExtentId { cell: alice.cell(), sequence: 1 }, 0, &block(1)), Ok(()));
    assert_eq!(alice.commit(None), Ok(()));
    assert!(open() < files, "{} files open of {files}", open());
    assert!(
        open() <= budget,
        "{} files open, past base + {allowed} * {per_cell} = {budget}",
        open()
    );

    drop((alice, mallory));
    // SAFETY: Restores the original RLIMIT_NOFILE.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

/// `len` bytes of `seed`'s that don't compress, as a closed segment's framing would otherwise
/// shrink them.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// TASK-94: a tenant's VM processes fill no more of the host's disk than its limit, however many
/// write at once, each into a cell of its own, none committing, so that none of it uploads and
/// leaves the disk: past it their writes fail with `ENOSPC`, while alice, another tenant, writes
/// and commits throughout, unlimited. Without a limit a tenant's cells were bounded by their
/// capacities alone, and a tenant made as many as it liked. At its limit, mallory's cells still
/// commit, and once it is lifted they write on.
#[test]
fn a_tenant_filling_its_disk_leaves_others_committing() {
    use crate::disk::STORE_MADE;
    use bucket_cell::host_disk::{RESERVE_FACTOR, RESERVE_PER_STORE};
    const CONNECTIONS: u64 = 8;
    const MIB: u64 = 1 << 20;
    /// What a MiB written to an extent of its own is admitted as: it, and what its cell's own
    /// work may add for it, as raw data and as an extent open (doc-22).
    const ADMITTED: u64 = (1 + 2 * RESERVE_FACTOR) * MIB;
    /// Room for 16 such MiB, and for each cell made and open, what its making is admitted as and
    /// what each cell open reserves.
    const ROOM: u64 = 16 * ADMITTED + CONNECTIONS * (STORE_MADE + RESERVE_PER_STORE);
    const ALICES: u64 = 32;
    host();
    let dir = tempfile::tempdir().unwrap();
    let tenant = |name: &str, id: u64| {
        let cells = dir.path().join(name);
        std::fs::create_dir(&cells).unwrap();
        let tenant = Tenant::new(id, &cells).unwrap();
        let socket = dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        let served = tenant.clone();
        std::thread::spawn(move || serve_tenant(listener, served));
        (cells, socket, tenant)
    };
    let (mdir, msocket, mallory) = tenant("mallory", MALLORY);
    let (adir, asocket, _alice) = tenant("alice", ALICE);
    let taken = disk::count(Some(MALLORY)).unwrap().bytes;
    let limit = taken + ROOM;
    disk::set_limit(Some(MALLORY), Some(limit)).unwrap();

    // Mallory's VM processes, each its own cell, writing a MiB at a time until refused; alice's,
    // writing and committing as many beside them.
    let start = Arc::new(std::sync::Barrier::new(CONNECTIONS as usize + 1));
    let fills: Vec<_> = (0..CONNECTIONS)
        .map(|n| {
            let (socket, path, start) =
                (msocket.clone(), mdir.join(format!("store-{n}")), start.clone());
            std::thread::spawn(move || {
                // Its capacity bounds what it writes without a limit, 512 MiB in all.
                let client = CellClient::connect(&socket, &path, 64 * MIB);
                start.wait();
                let client = client.unwrap();
                let mut written = 0;
                let refused = loop {
                    let extent = ExtentId { cell: client.cell(), sequence: 1 + written };
                    match write(&client, extent, 0, &noise(n << 32 | written, MIB as usize)) {
                        Ok(()) => written += 1,
                        Err(errno) => break errno,
                    }
                };
                (client, written, refused)
            })
        })
        .collect();
    let alice = CellClient::connect(&asocket, adir.join("store"), 1 << 30).unwrap();
    start.wait();
    let alices: Vec<_> = (1..=ALICES)
        .map(|sequence| {
            let extent = ExtentId { cell: alice.cell(), sequence };
            let written = write(&alice, extent, 0, &noise(1 << 40 | sequence, MIB as usize));
            (written, alice.commit(None))
        })
        .collect();
    let filled: Vec<_> = fills.into_iter().map(|fill| fill.join().unwrap()).collect();
    let written: u64 = filled.iter().map(|(_, written, _)| written).sum();
    let refusals: Vec<Errno> = filled.iter().map(|(_, _, refused)| *refused).collect();
    let counted = disk::count(Some(MALLORY)).unwrap().bytes;
    println!(
        "mallory's {CONNECTIONS} VM processes wrote {written} MiB, refused with {:?}; its disk \
         takes {} KiB, of a {} KiB limit; alice wrote and committed {ALICES} MiB",
        refusals[0],
        counted >> 10,
        limit >> 10
    );

    // Alice wrote and committed throughout, and reads what she wrote.
    assert_eq!(alices, vec![(Ok(()), Ok(())); ALICES as usize], "alice was refused");
    let last = ExtentId { cell: alice.cell(), sequence: ALICES };
    assert_eq!(read(&alice, last, 0, MIB as usize), Ok(noise(1 << 40 | ALICES, MIB as usize)));
    // Mallory's VM processes were refused at its limit, having written within it: its cells
    // made, what their files took no more than they were admitted as, and open, and what they
    // wrote, each MiB with what their own work may add for it, its room.
    assert_eq!(refusals, vec![Errno::ENOSPC; CONNECTIONS as usize]);
    assert_eq!(written, 16);
    assert!(counted <= limit, "mallory's disk takes {counted} bytes, past its limit of {limit}");
    assert!(mallory.disk().unwrap().used() <= limit);
    // At its limit, its cells commit; lifted, they write on.
    for (client, _, _) in &filled {
        assert_eq!(client.commit(None), Ok(()));
    }
    disk::set_limit(Some(MALLORY), None).unwrap();
    let (client, written, _) = &filled[0];
    let extent = ExtentId { cell: client.cell(), sequence: 1 + written };
    assert_eq!(write(client, extent, 0, &noise(0, MIB as usize)), Ok(()));
    assert_eq!(client.commit(None), Ok(()));
}

/// A VM process's transfer area of hugetlbfs memory it never touched is refused, as one of
/// shared memory with holes is (TASK-547, board 11707): the daemon would fault its huge pages in
/// itself, as it reads into the area, charged to the daemon's hugetlb cgroup and taken from the
/// host's pool, past the VM's limits. `TransferArea::map` checks the shrink seal, the size and
/// `SEEK_HOLE`, but not the file system: hugetlbfs answers `SEEK_HOLE` with the file's size
/// (`default_llseek`), so the hole check passes on a memfd of no pages at all. Needs a pool of
/// huge pages (`sysctl vm.nr_hugepages=8`): without one the daemon's `mmap` fails, `ENOMEM`.
#[test]
#[ignore = "board 14711: a transfer area of untouched hugetlbfs memory is accepted (needs vm.nr_hugepages >= 1)"]
fn transfer_areas_of_hugetlbfs_memory_never_touched_are_refused() {
    host();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cladefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || serve(listener));
    let (sender, mut receiver, _stream, _cell) =
        super::raw_connection(&socket, &dir.path().join("store"), 1 << 30);
    // 2 MiB, one huge page, sealed against shrinking, never mapped nor written.
    let len: u64 = 2 << 20;
    // SAFETY: Makes a descriptor, owned below.
    let fd = unsafe {
        libc::memfd_create(c"transfer".as_ptr(), libc::MFD_ALLOW_SEALING | libc::MFD_HUGETLB)
    };
    assert!(fd >= 0, "memfd_create(MFD_HUGETLB): {}", std::io::Error::last_os_error());
    // SAFETY: Just made, and owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: Sizes and seals a descriptor this holds.
    unsafe {
        assert_eq!(libc::ftruncate(fd.as_raw_fd(), len as i64), 0);
        assert_eq!(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK), 0);
    }
    let free_before = huge_pages_free();
    let transfer = Header::new(TRANSFER, 2).with_body(&TransferBody { len });
    sender.send(transfer, &[], &[fd.as_fd()]).unwrap();
    let transferred = receiver.receive(|| 0).unwrap().unwrap().header;
    assert_eq!(
        (transferred.id, transferred.status, huge_pages_free()),
        (2, -libc::EINVAL, free_before),
        "the daemon took an untouched hugetlbfs area"
    );
}

/// The host's free huge pages of the default size (`/proc/meminfo`'s `HugePages_Free`).
fn huge_pages_free() -> u64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap();
    let line = meminfo.lines().find(|line| line.starts_with("HugePages_Free:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}
