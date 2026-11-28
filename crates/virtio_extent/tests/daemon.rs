// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's requests served by an extent daemon of the test's, in fake guest memory, and the
//! device made, activated, snapshotted and restored over one: what a VMM's adapter gets, apart
//! from any VMM. Linux alone, as the daemon is.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use cladefs::{CellClient, Errno, OpenCell};
use virtio_extent::protocol::*;
use virtio_extent::{
    BoxError, Buffers, CacheType, Config, ConfigInterrupt, Device, Guest, Notices, WindowVm, handle,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;
const RESPONSE: usize = size_of::<Response>();

/// The bounds of a device of the test's cells, which have no window (TASK-470).
fn bounds() -> Bounds {
    Bounds {
        block_size: BLOCK as u64,
        max_extent: MAX_EXTENT,
        map_alignment: BLOCK as u64,
        window: 0,
    }
}

/// Makes this process a host of the multi-node system, as an extent daemon's must be: its
/// cells in a bucket in its memory, as of placement 1, unless the test's environment names
/// others (scripts/test.sh's firecracker part names memory:///vmm). The daemon logs to standard
/// error, which a failed test shows: its EIO opening a cell with no bucket named said nothing of
/// why (cladefs TASK-220).
fn host() {
    let _ = cladefs::cladefsd::logging::to_stderr();
    for (name, value) in [("CLADEFSD_BUCKET", "memory:///cells"), ("CLADEFSD_TOKEN", "1")] {
        if std::env::var_os(name).is_none() {
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment yet.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// A directory of the test's, removed once dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "virtio-extent-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("cladefsd.sock")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A daemon of the test's, serving at its directory's socket.
fn daemon() -> TestDir {
    host();
    let dir = TestDir::new();
    let listener = std::os::unix::net::UnixListener::bind(dir.socket()).unwrap();
    std::thread::spawn(move || cladefs::cladefsd::serve(listener));
    dir
}

/// A cell of `capacity` bytes, which a daemon of the test's serves.
fn cell_of(capacity: u64) -> (CellClient, TestDir) {
    let dir = daemon();
    let cell = CellClient::connect(dir.socket(), dir.0.join("store"), capacity).unwrap();
    (cell, dir)
}

fn a_cell() -> (CellClient, TestDir) {
    cell_of(1 << 20)
}

/// The requests of `kind` the daemons of this process have served (their metrics).
fn served(kind: &str) -> u64 {
    let metrics = cladefs::cladefsd::metrics::render();
    let name = format!("cladefsd_requests_total{{kind=\"{kind}\"}} ");
    let line = (metrics.lines()).find_map(|line| line.strip_prefix(&name));
    let line = line.unwrap_or_else(|| panic!("no {kind} requests counted in {metrics}"));
    line.parse().unwrap()
}

/// The bytes of a request's header and arguments, followed by `data`.
fn bytes<T: IntoBytes + Immutable>(
    type_: u32,
    flags: u32,
    args: Option<T>,
    data: &[u8],
) -> Vec<u8> {
    let header = RequestHeader { type_: type_.to_le(), flags: flags.to_le(), unique: 7 };
    let mut bytes = header.as_bytes().to_vec();
    if let Some(args) = &args {
        bytes.extend_from_slice(args.as_bytes());
    }
    bytes.extend_from_slice(data);
    bytes
}

/// The buffers of a request of `readable` bytes, with `written`'s for the device to write, in
/// descriptors of at most `split` bytes: fake guest memory, as a VMM's adapter gives it.
fn buffers(readable: &mut [u8], written: &mut [u8], split: usize) -> Buffers {
    let mut buffers = Buffers::new();
    for part in readable.chunks_mut(split) {
        // SAFETY: The test's memory, which outlives the request.
        unsafe { buffers.push_readable(part.as_mut_ptr(), part.len()) };
    }
    for part in written.chunks_mut(split) {
        // SAFETY: As above, which nothing else accesses meanwhile.
        unsafe { buffers.push_writable(part.as_mut_ptr(), part.len()) }.unwrap();
    }
    buffers
}

/// Serves a request of `readable` bytes, with `writable` bytes for the device to write, the
/// response included, split into descriptors of at most `split` bytes; returns how much the device
/// says it wrote, and the `writable` bytes it was given.
fn serve(cell: &CellClient, readable: &[u8], writable: usize, split: usize) -> (usize, Vec<u8>) {
    let mut readable = readable.to_vec();
    let mut written = vec![0xa5; writable];
    let buffers = buffers(&mut readable, &mut written, split);
    let notices = Notices::default();
    let guest = Guest { notices: &notices, ordered_only: false };
    let served = handle(cell, &guest, bounds(), buffers);
    let used = futures::executor::block_on(served).len;
    (used as usize, written)
}

/// Serves a request as [`serve`] does, and returns what the device wrote, the response apart, and
/// the response's status.
fn request_split(
    cell: &CellClient,
    readable: &[u8],
    writable: usize,
    split: usize,
) -> (Vec<u8>, u32) {
    let (used, mut written) = serve(cell, readable, writable, split);
    assert_eq!(used, writable, "the device wrote {used} of {writable} bytes");
    let response = Response::read_from_bytes(&written[writable - RESPONSE..]).unwrap();
    written.truncate(writable - RESPONSE);
    (written, u32::from_le(response.status))
}

fn request(cell: &CellClient, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
    request_split(cell, readable, writable, usize::MAX)
}

fn read<T: FromBytes>(bytes: &[u8]) -> T {
    T::read_from_prefix(bytes).unwrap().0
}

fn io(cell: &CellClient, sequence: u64, offset: u64, len: usize) -> IoArgs {
    IoArgs {
        id: ExtentIdLe { cell: cell.cell().to_le(), sequence: sequence.to_le() },
        offset: offset.to_le(),
        len: (len as u32).to_le(),
        root_len: 0,
    }
}

fn errno(errno: Errno) -> u32 {
    errno.raw().cast_unsigned()
}

/// cladefs TASK-111.4: the guest's stats, text, go to the daemon, as the answer to the ask the
/// request names; what isn't text, or is longer than the daemon takes, is refused.
#[test]
fn stats_go_to_the_daemon() {
    let (cell, _dir) = a_cell();
    let asked = Some(StatsArgs { asked: 1u64.to_le() });
    let before = served("stats");
    let stats = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals 1\ncommit abc\n");
    assert_eq!(request(&cell, &stats, RESPONSE), (vec![], 0));
    assert_eq!(served("stats"), before + 1);
    let not_text = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals \xff\n");
    assert_eq!(request(&cell, &not_text, RESPONSE).1, errno(Errno::EINVAL));
    let long = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, &vec![b'\n'; MAX_STATS + 1]);
    assert_eq!(request(&cell, &long, RESPONSE).1, errno(Errno::EINVAL));
    assert_eq!(served("stats"), before + 1);
}

#[test]
fn writes_and_reads() {
    let (cell, _dir) = a_cell();
    let data: Vec<u8> = (0..2 * BLOCK).map(|i| i as u8).collect();
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&cell, 1, 0, data.len())), &data);
    assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
    let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&cell, 1, 0, data.len())), &[]);
    assert_eq!(request(&cell, &read, data.len() + RESPONSE), (data.clone(), 0));
    // Across descriptors of odd sizes, whose writes and reads the device splits and joins.
    assert_eq!(request_split(&cell, &read, data.len() + RESPONSE, 1000), (data.clone(), 0));
    let rewrite = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&cell, 2, 0, data.len())), &data);
    assert_eq!(request_split(&cell, &rewrite, RESPONSE, 777), (vec![], 0));
    let read2 = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&cell, 2, 0, data.len())), &[]);
    assert_eq!(request(&cell, &read2, data.len() + RESPONSE), (data.clone(), 0));

    // An extent the cell doesn't hold, and a write after a commit sealed the extent.
    let missing = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&cell, 3, 0, BLOCK)), &[]);
    assert_eq!(request(&cell, &missing, BLOCK + RESPONSE), (vec![0; BLOCK], errno(Errno::ENOENT)));
    let commit = bytes(
        VIRTIO_EXTENT_T_COMMIT,
        VIRTIO_EXTENT_COMMIT_UPLOADED,
        Some(CommitArgs::default()),
        &[],
    );
    assert_eq!(request(&cell, &commit, RESPONSE), (vec![], 0));
    let sealed = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&cell, 1, 0, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&cell, &sealed, RESPONSE).1, errno(Errno::EPERM));
    // A barrier commits what came before it.
    let barrier = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_BARRIER,
        Some(io(&cell, 4, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&cell, &barrier, RESPONSE).1, 0);
    // A commit after a write seals what it wrote, in one request: and only a WRITE's flags are a
    // WRITE's.
    let committed = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_UPLOADED,
        Some(io(&cell, 5, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&cell, &committed, RESPONSE).1, 0);
    let after =
        bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&cell, 5, BLOCK as u64, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&cell, &after, RESPONSE).1, errno(Errno::EPERM));
    let unknown = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_UPLOADED,
        Some(io(&cell, 6, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&cell, &unknown, RESPONSE).1, errno(Errno::EINVAL));

    let free = bytes(VIRTIO_EXTENT_T_FREE, 0, Some(io(&cell, 1, 0, 0).id), &[]);
    assert_eq!(request(&cell, &free, RESPONSE), (vec![], 0));
    assert_eq!(request(&cell, &read, data.len() + RESPONSE).1, errno(Errno::ENOENT));
}

/// cladefs: a FREE with FREE_MANY, of a flush's dead extents, as many as the guest's Fxfs names in
/// one (257), frees them in one request of the daemon's, not one each, each a round trip to it
/// (TASK-92.13).
#[test]
fn frees_many_in_one_request() {
    const EXTENTS: u64 = 257;
    let (cell, _dir) = cell_of(4 << 20);
    let data = vec![7; BLOCK];
    let ids: Vec<ExtentIdLe> = (1..=EXTENTS)
        .map(|sequence| {
            let id = cladefs::ExtentId { cell: cell.cell(), sequence };
            cell.write(id, 0, &[cladefs::VolatileSlice::from(&data[..])]).unwrap();
            ExtentIdLe { cell: cell.cell().to_le(), sequence: sequence.to_le() }
        })
        .collect();
    cell.commit(None).unwrap();
    let rest: Vec<u8> = ids[1..].iter().flat_map(|id| id.as_bytes().to_vec()).collect();
    let free = bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(ids[0]), &rest);
    let frees = || served("free");
    let before = frees();
    let start = Instant::now();
    assert_eq!(request(&cell, &free, RESPONSE), (vec![], 0));
    let took = start.elapsed();
    let served = frees() - before;
    println!("{EXTENTS} extents freed in {took:?}, in {served} of the daemon's requests");
    assert_eq!(served, 1);
    for sequence in [1, EXTENTS / 2, EXTENTS] {
        let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&cell, sequence, 0, BLOCK)), &[]);
        assert_eq!(
            request(&cell, &read, BLOCK + RESPONSE).1,
            errno(Errno::ENOENT),
            "extent {sequence} is still there"
        );
    }
}

#[test]
fn roots_and_info() {
    let (cell, _dir) = a_cell();
    let root = bytes::<RootLen>(VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
    let root_len = MAX_ROOT as usize + size_of::<RootLen>();
    let (written, status) = request(&cell, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(read::<RootLen>(&written[MAX_ROOT as usize..]).len, 0);

    let commit = bytes(
        VIRTIO_EXTENT_T_COMMIT,
        VIRTIO_EXTENT_COMMIT_ROOT,
        Some(CommitArgs { root_len: 5u32.to_le(), reserved: 0 }),
        b"first",
    );
    assert_eq!(request(&cell, &commit, RESPONSE), (vec![], 0));
    let (written, status) = request(&cell, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(&written[..5], b"first");
    assert!(written[5..MAX_ROOT as usize].iter().all(|b| *b == 0));
    assert_eq!(u32::from_le(read::<RootLen>(&written[MAX_ROOT as usize..]).len), 5);

    let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let (written, status) = request(&cell, &info, size_of::<Info>() + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(
        read::<Info>(&written),
        Info {
            capacity: (1u64 << 20).to_le(),
            used: 0,
            available: (1u64 << 20).to_le(),
            next_sequence: 0,
            // The cell's ID is kept in the daemon's bucket: its writer mints from its range,
            // its first taking's (cladefs's doc-7, The head).
            sequence_floor: (1u64 << 40).to_le(),
            cell: cell.cell().to_le(),
        }
    );
    // cladefs: room for what comes before the cell alone, as a driver without F_FREEZE had, is
    // refused (TASK-141.3).
    let short = std::mem::offset_of!(Info, cell);
    assert_eq!(request(&cell, &info, short + RESPONSE).1, errno(Errno::EINVAL));

    // cladefs: a WRITE's commit sets the root after its data, which isn't the data's; there is no
    // root without a commit to set it.
    let data = [7u8; BLOCK];
    let rooted = IoArgs { root_len: 6u32.to_le(), ..io(&cell, 1, 0, BLOCK) };
    let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT;
    let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), &[&data[..], b"second"].concat());
    assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
    let (written, status) = request(&cell, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(&written[..6], b"second");
    let mut back = vec![0; BLOCK];
    let id = cladefs::ExtentId { cell: cell.cell(), sequence: 1 };
    cell.read(id, 0, &mut [cladefs::VolatileSliceMut::from(&mut back[..])]).unwrap();
    assert_eq!(back, data);
    let uncommitted = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_ROOT,
        Some(IoArgs { root_len: 5u32.to_le(), ..io(&cell, 2, 0, BLOCK) }),
        &[&data[..], b"third"].concat(),
    );
    assert_eq!(request(&cell, &uncommitted, RESPONSE).1, errno(Errno::EINVAL));
}

/// cladefs TASK-77: a guest's driver is untrusted: whatever a request says, of the types and flags
/// it may name, of extents of its cell or another's, at offsets near the cell's ends or at the
/// ends of a u64, the device answers it, with a status, no more than the room it was given
/// written, and goes on serving. Seeded by proptest, which says the case if one fails.
#[test]
fn answers_whatever_a_driver_asks() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};

    let (client, _dir) = cell_of(64 << 20);
    // cladefs TASK-470: and blocks' multiples, which reads and writes take.
    let near_or_far = || {
        prop_oneof![
            0u64..(1 << 22),
            (0u64..16).prop_map(|blocks| blocks * BLOCK as u64),
            any::<u64>(),
            (0u64..4096).prop_map(|back| u64::MAX - back)
        ]
    };
    let cell = prop_oneof![Just(client.cell()), any::<u64>()];
    let args = (cell, 0u64..64, near_or_far(), near_or_far(), any::<u32>());
    let word = || prop_oneof![0u32..18, any::<u32>()];
    // cladefs board 2774: flags its type knows, three times in four, any else: a flag it doesn't
    // know is refused at the header, which would leave too few served.
    let flags = (any::<u32>(), 0u8..4);
    let data = proptest::collection::vec(any::<u8>(), 0..2 * BLOCK);
    let requests = (word(), flags, args, data, 0usize..4 * BLOCK);
    let config = Config { cases: 5000, failure_persistence: None, ..Config::default() };
    // Those the device served: refusing each at its header would show nothing.
    let served = AtomicU32::new(0);
    TestRunner::new(config)
        .run(&requests, |(type_, (bits, any), args, data, room)| {
            let flags = match any {
                0 => bits,
                _ => known_flags(type_).unwrap_or(0) & bits,
            };
            let (cell, sequence, first, second, last) = args;
            let mut readable = bytes::<Info>(type_, flags, None, &[]);
            for word in [cell, sequence, first, second, u64::from(last)] {
                readable.extend_from_slice(&word.to_le_bytes());
            }
            readable.extend_from_slice(&data);
            let writable = RESPONSE + room;
            // As a queue's 16 descriptors would hold it.
            let split = (readable.len() + writable) / 14 + 1;
            let (used, written) = serve(&client, &readable, writable, split);
            prop_assert!(used <= writable, "{} bytes used of {}", used, writable);
            let at = used.checked_sub(RESPONSE).expect("a response");
            let status = u32::from_le_bytes(written[at..at + 4].try_into().unwrap());
            prop_assert!(status < 4096, "status {}", status);
            served.fetch_add(u32::from(status == 0), Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    let served = served.load(Ordering::Relaxed);
    assert!(served > 100, "{served} requests of 5000 served");
    // The device, and the daemon behind it, serve on.
    let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    assert_eq!(request(&client, &info, size_of::<Info>() + RESPONSE).1, 0);
}

#[test]
fn refuses_bad_requests() {
    let (cell, _dir) = a_cell();
    let unknown = bytes::<Info>(99, 0, None, &[]);
    assert_eq!(request(&cell, &unknown, RESPONSE).1, errno(Errno::EINVAL));
    // A read with less room than it reads, and one whose header is short.
    let read_ = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&cell, 1, 0, 2 * BLOCK)), &[]);
    assert_eq!(request(&cell, &read_, BLOCK + RESPONSE), (vec![0; BLOCK], errno(Errno::EINVAL)));
    assert_eq!(request(&cell, &[0; 4], RESPONSE).1, errno(Errno::EINVAL));
    // A write whose data is shorter than it says.
    let short = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&cell, 1, 0, BLOCK)), &[1; 100]);
    assert_eq!(request(&cell, &short, RESPONSE).1, errno(Errno::EINVAL));
    // Without a DAX window, one map or many; and a MAP flag the device doesn't know, of the
    // request or of one of its maps (`F_MAP_MANY`): cladefs: writable DAX's `MAP_WRITE` among them
    // (backlog doc-13).
    let entries = |flags: &[u32]| -> Vec<u8> {
        (flags.iter())
            .flat_map(|flags| {
                let entry = MapEntry { flags: flags.to_le(), ..MapEntry::default() };
                entry.as_bytes().to_vec()
            })
            .collect()
    };
    let many = entries(&[VIRTIO_EXTENT_MAP_PREFAULT, 0]);
    for flags in [0, VIRTIO_EXTENT_MAP_PREFAULT] {
        let map = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(MapArgs::default()), &[]);
        assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::ENOSYS));
        let flags = flags | VIRTIO_EXTENT_MAP_MANY;
        let map = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(MapArgs::default()), &many);
        assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::ENOSYS));
    }
    for unknown in [1 << 0, 1 << 3] {
        let map = bytes(VIRTIO_EXTENT_T_MAP, unknown, Some(MapArgs::default()), &[]);
        assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::EINVAL));
    }
    // cladefs: zeros, and PRIVATE, of a cell without a window that copies on write (doc-13).
    let zeros = entries(&[VIRTIO_EXTENT_MAP_ZEROS]);
    let map = bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &zeros);
    assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::EINVAL));
    let private = bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(UnmapArgs::default()), &[]);
    assert_eq!(request(&cell, &private, RESPONSE).1, errno(Errno::EINVAL));
    let write_flag = entries(&[1 << 1]);
    let map =
        bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &write_flag);
    assert_eq!(request(&cell, &map, RESPONSE).1, errno(Errno::EINVAL));
}

/// cladefs: a FREE_MANY's or a MAP_MANY's data past MAX_MANY_DATA is refused, nothing done: the
/// device lists what it names in its own memory, and a chain naming the same guest memory again
/// and again names as much as it likes, which grew the VMM by as much. Up to it, the request is
/// served.
#[test]
fn refuses_many_past_their_most() {
    let (cell, _dir) = a_cell();
    let extent = io(&cell, 1, 0, BLOCK);
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(extent), &[7; BLOCK]);
    assert_eq!(request(&cell, &write, RESPONSE), (vec![], 0));
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(request(&cell, &commit, RESPONSE), (vec![], 0));
    let read_ = bytes(VIRTIO_EXTENT_T_READ, 0, Some(extent), &[]);

    // The extent named again past the most: refused, and the extent is there still.
    let id = extent.id.as_bytes();
    let free = |data_len: usize| {
        let data = id.repeat(data_len / id.len());
        bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(extent.id), &data)
    };
    assert_eq!(request(&cell, &free(MAX_MANY_DATA + id.len()), RESPONSE).1, errno(Errno::EINVAL));
    assert_eq!(request(&cell, &read_, BLOCK + RESPONSE), (vec![7; BLOCK], 0));
    // Up to the most, served: the extent freed, as often as it is named, and then not found.
    assert_eq!(request(&cell, &free(MAX_MANY_DATA), RESPONSE), (vec![], 0));
    assert_eq!(request(&cell, &read_, BLOCK + RESPONSE).1, errno(Errno::ENOENT));

    // Maps past the most: refused; up to it, served, as a cell without a window serves maps.
    let entry = MapEntry::default();
    let entry = entry.as_bytes();
    let map = |data_len: usize| {
        let data = entry.repeat(data_len / entry.len());
        bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &data)
    };
    assert_eq!(request(&cell, &map(MAX_MANY_DATA + entry.len()), RESPONSE).1, errno(Errno::EINVAL));
    assert_eq!(request(&cell, &map(MAX_MANY_DATA), RESPONSE).1, errno(Errno::ENOSYS));
}

/// A configuration change interrupt that counts how often it is raised.
#[derive(Default)]
struct Counted(AtomicUsize);

impl ConfigInterrupt for Counted {
    fn config_changed(&self) -> Result<(), BoxError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// A VM's guest physical address space as the test has it, recording what is asked of it.
#[derive(Debug, Default)]
struct FakeVm {
    calls: Mutex<Vec<String>>,
    host: AtomicUsize,
}

impl WindowVm for FakeVm {
    fn reserve(&self, len: u64, at: Option<u64>) -> Result<u64, BoxError> {
        let start = at.unwrap_or(1 << 40);
        self.calls.lock().unwrap().push(format!("reserve {start:#x} {len:#x}"));
        Ok(start)
    }

    fn release(&self, start: u64, len: u64) {
        self.calls.lock().unwrap().push(format!("release {start:#x} {len:#x}"));
    }

    fn add_slot(&self, guest_addr: u64, len: u64, host: NonNull<u8>) -> Result<u32, BoxError> {
        self.host.store(host.as_ptr() as usize, Ordering::Relaxed);
        self.calls.lock().unwrap().push(format!("add_slot {guest_addr:#x} {len:#x}"));
        Ok(1)
    }

    fn remove_slot(&self, slot: u32) {
        self.calls.lock().unwrap().push(format!("remove_slot {slot}"));
    }
}

fn config(dir: &TestDir, path: &str) -> Config {
    Config {
        id: "extents".into(),
        tag: "fxfs".into(),
        path_on_host: dir.0.join(path).display().to_string(),
        capacity: 1 << 20,
        dax_window_size: None,
        dax_private_max_bytes: None,
        cladefsd_socket: dir.socket().display().to_string(),
        cache_type: CacheType::Writeback,
        stream: None,
    }
}

/// Serves a request with `device`'s requests, as a VMM's worker does, on the threads receiving the
/// daemon's replies: the length the device wrote, and the response's status.
fn serve_with(device: &Device, readable: &[u8], writable: usize) -> (u32, u32) {
    let requests = Arc::new(device.requests());
    let mut readable = readable.to_vec();
    let mut written = vec![0xa5; writable];
    let buffers = buffers(&mut readable, &mut written, usize::MAX);
    let (completed, len) = mpsc::channel();
    requests.serve(Ok::<_, String>(buffers), move |len| completed.send(len).unwrap());
    let len = len.recv().unwrap();
    requests.wait_idle();
    let response = Response::read_from_bytes(&written[writable - RESPONSE..]).unwrap();
    (len, u32::from_le(response.status))
}

/// A writer's device opens its cell, says its version, tag, queues, cell and capacity in its
/// configuration, takes its guest's freezing, refuses a snapshot unfrozen, and, frozen, forks its
/// cell to a version, which a device restored from its state opens a cell of its own from, as
/// frozen as it was.
#[test]
fn devices_are_snapshotted_and_restored() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm: Arc<dyn WindowVm> = Arc::new(FakeVm::default());
    let mut device = Device::new(config(&dir, "store"), 2, vm.clone(), lost.clone()).unwrap();
    assert_eq!(device.features(), device.config().features());
    assert_eq!(device.shm_region(), None);
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let original = ConfigSpace::read_from_bytes(&space).unwrap();
    let tag = original.tag;
    assert_eq!(
        (u32::from_le(original.version), &tag[..5], u32::from_le(original.num_request_queues)),
        (VIRTIO_EXTENT_VERSION, &b"fxfs\0"[..], 2)
    );
    assert_eq!(u64::from_le(original.capacity), 1 << 20);
    assert_ne!(u64::from_le(original.cell), 0);

    let interrupt = Arc::new(Counted::default());
    device.activate(device.features() | (1 << 32), interrupt.clone()).unwrap();
    let refused = device.take_snapshot(|| panic!("quiesced unfrozen")).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "Extent device extents: its guest's filesystem is not frozen (fsfreeze it before the \
         snapshot, and thaw it after)"
    );
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE), (RESPONSE as u32, 0));
    let mut quiesced = false;
    device.take_snapshot(|| quiesced = true).unwrap();
    assert!(quiesced);
    let state = device.state();
    assert_eq!((state.frozen, state.window_addr), (true, None));
    assert_eq!(state.target, device.snapshot().unwrap().target);
    assert!(matches!(
        state.target,
        cladefs::CellSnapshot::Version(v) if v.cell == u64::from_le(original.cell)
    ));

    let mut restored_state = state.clone();
    restored_state.config.path_on_host = dir.0.join("restored").display().to_string();
    let restored = Device::restore(&restored_state, 2, vm, lost).unwrap();
    assert!(restored.snapshot().is_some());
    let mut space = [0u8; size_of::<ConfigSpace>()];
    restored.read_config(0, &mut space);
    let (restored_cell, original_cell) =
        (ConfigSpace::read_from_bytes(&space).unwrap().cell, original.cell);
    assert_ne!(restored_cell, original_cell);
    assert!(restored.state().frozen);
}

/// Resetting an activated device releases every `ConfigInterrupt` reference it kept (resizing,
/// following and stats notices), so a PCI transport reset can tear down its old MSI-X vector
/// group and free its KVM GSIs and irqfds before the device is activated again.
#[test]
fn reset_drops_interrupt_references() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm: Arc<dyn WindowVm> = Arc::new(FakeVm::default());
    let mut device = Device::new(config(&dir, "reset-int"), 2, vm, lost).unwrap();

    let interrupt = Arc::new(Counted::default());
    let weak = Arc::downgrade(&interrupt);
    device.activate(device.features() | (1 << 32), interrupt).unwrap();
    assert!(weak.upgrade().is_some());

    device.reset();
    assert!(weak.upgrade().is_none(), "reset kept a reference to the transport's interrupt");
}

/// TASK-575: a snapshot a device took, dropped as the VMM drops it when the VM's snapshot fails
/// after it, is gone: the device has none, and takes the next, of another version. (The daemon's
/// side, the version dropped, is cladefsd's `vm_processes_drop_the_versions_they_made_alone`.)
#[test]
fn snapshots_dropped_are_gone() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm: Arc<dyn WindowVm> = Arc::new(FakeVm::default());
    let mut device = Device::new(config(&dir, "store"), 2, vm, lost).unwrap();
    device.activate(device.features() | (1 << 32), Arc::new(Counted::default())).unwrap();
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE), (RESPONSE as u32, 0));
    device.take_snapshot(|| {}).unwrap();
    let dropped = device.snapshot().unwrap().target;
    device.drop_snapshot();
    assert!(device.snapshot().is_none());
    device.take_snapshot(|| {}).unwrap();
    assert_ne!(device.snapshot().unwrap().target, dropped);
}

/// cladefs doc-26 (TASK-203): a device whose window holds a private page, its VM process's own,
/// takes no snapshot: refused once its workers are quiesced, before a version is made, saying how
/// many, as no extent holds the page's bytes. The page rebound, an extent of its bytes mapped over
/// it as the guest's freeze does, it takes one.
#[test]
fn windows_holding_private_pages_take_no_snapshot() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm: Arc<dyn WindowVm> = Arc::new(FakeVm::default());
    let config =
        Config { capacity: 4 << 20, dax_window_size: Some(4 << 20), ..config(&dir, "store") };
    let mut device = Device::new(config, 1, vm, lost).unwrap();
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let cell = u64::from_le(ConfigSpace::read_from_bytes(&space).unwrap().cell);
    let ps = cladefs::pages::page_size();
    let id = |sequence: u64| ExtentIdLe { cell: cell.to_le(), sequence: sequence.to_le() };
    for sequence in [1, 2] {
        let io = IoArgs { id: id(sequence), offset: 0, len: (ps as u32).to_le(), root_len: 0 };
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io), &vec![sequence as u8; ps as usize]);
        assert_eq!(serve_with(&device, &write, RESPONSE).1, 0);
    }
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);
    let map = |sequence: u64| {
        let args = MapArgs { id: id(sequence), offset: 0, len: ps.to_le(), window_offset: 0 };
        bytes(VIRTIO_EXTENT_T_MAP, 0, Some(args), &[])
    };
    assert_eq!(serve_with(&device, &map(1), RESPONSE).1, 0);
    let args = UnmapArgs { window_offset: 0, len: ps.to_le() };
    let private = bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(args), &[]);
    assert_eq!(serve_with(&device, &private, RESPONSE).1, 0);
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE).1, 0);

    let mut quiesced = false;
    let refused = device.take_snapshot(|| quiesced = true).unwrap_err();
    assert!(quiesced, "refused before its workers were quiesced");
    assert_eq!(
        refused.to_string(),
        "Extent device extents: its DAX window holds 1 private pages, whose bytes no extent holds \
         (freeze the guest's filesystem, which rebinds them)"
    );
    assert!(device.snapshot().is_none(), "a version made");
    assert_eq!(serve_with(&device, &map(2), RESPONSE).1, 0);
    device.take_snapshot(|| {}).unwrap();
}

/// cladefs board 6917: `VIRTIO_EXTENT_MAP_MANY` forwards directly to the daemon's `MAP_MANY` in
/// one RPC (mixing extents, zeros and `MAP_PREFAULT`), and preserves partial-failure semantics
/// when an entry fails on the daemon (mapping entries before the error and none after it).
#[test]
fn maps_many_in_one_request() {
    const EXTENTS: u64 = 64;
    // The host's pages the test touches, as many on any page size: EXTENTS + 1 extents of a page
    // each in the cell, and windows' pages up to 102 (board 9977: a cell of 1 MiB fit 64 + 1
    // extents of 4 KiB pages, not of 16 KiB ones).
    const PAGES: u64 = 128;
    let ps = cladefs::pages::page_size();
    let size = (PAGES * ps).next_multiple_of(2 << 20);
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm = Arc::new(FakeVm::default());
    let config = Config { capacity: size, dax_window_size: Some(size), ..config(&dir, "store") };
    let mut device = Device::new(config, 1, vm.clone(), lost).unwrap();
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();
    let base = vm.host.load(Ordering::Relaxed) as *const u8;
    assert!(!base.is_null(), "DAX window slot was not mapped");
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let cell = u64::from_le(ConfigSpace::read_from_bytes(&space).unwrap().cell);
    let id = |sequence: u64| ExtentIdLe { cell: cell.to_le(), sequence: sequence.to_le() };

    for sequence in 1..=EXTENTS {
        let args = IoArgs { id: id(sequence), offset: 0, len: (ps as u32).to_le(), root_len: 0 };
        let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(args), &vec![sequence as u8; ps as usize]);
        assert_eq!(serve_with(&device, &write, RESPONSE).1, 0);
    }
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);
    // An uncommitted (open) extent, which the daemon refuses to map with EPERM.
    let open_seq = EXTENTS + 1;
    let args = IoArgs { id: id(open_seq), offset: 0, len: (ps as u32).to_le(), root_len: 0 };
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(args), &vec![0xff; ps as usize]);
    assert_eq!(serve_with(&device, &write, RESPONSE).1, 0);

    let map_args = |sequence: u64, window_offset: u64| MapArgs {
        id: id(sequence),
        offset: 0,
        len: ps.to_le(),
        window_offset: window_offset.to_le(),
    };
    let mut entries: Vec<u8> = (2..=EXTENTS)
        .flat_map(|sequence| {
            let flags = if sequence % 2 == 0 { VIRTIO_EXTENT_MAP_PREFAULT } else { 0 };
            let entry = MapEntry {
                map: map_args(sequence, (sequence - 1) * ps),
                flags: flags.to_le(),
                reserved: 0,
            };
            entry.as_bytes().to_vec()
        })
        .collect();
    let zeros_entry = MapEntry {
        map: map_args(0, EXTENTS * ps),
        flags: (VIRTIO_EXTENT_MAP_ZEROS | VIRTIO_EXTENT_MAP_PREFAULT).to_le(),
        reserved: 0,
    };
    entries.extend_from_slice(zeros_entry.as_bytes());
    let many = bytes(
        VIRTIO_EXTENT_T_MAP,
        VIRTIO_EXTENT_MAP_MANY | VIRTIO_EXTENT_MAP_PREFAULT,
        Some(map_args(1, 0)),
        &entries,
    );
    let maps = || served("map");
    let before = maps();
    assert_eq!(serve_with(&device, &many, RESPONSE).1, 0);
    assert_eq!(maps() - before, 1, "MAP_MANY fanned out into multiple RPCs");

    // Verify mapped extents and zeros actually read back from the DAX window.
    let read_page = |page: u64| -> Vec<u8> {
        assert!(page < PAGES, "page {page} past the window's {PAGES}");
        // SAFETY: `base` is the device's live DAX window mapping, of `size >= PAGES * ps` bytes,
        // and `page * ps + ps <= PAGES * ps`.
        unsafe { std::slice::from_raw_parts(base.add((page * ps) as usize), ps as usize).to_vec() }
    };
    assert_eq!(read_page(0), vec![1u8; ps as usize]);
    assert_eq!(read_page(EXTENTS - 1), vec![EXTENTS as u8; ps as usize]);
    assert_eq!(read_page(EXTENTS), vec![0u8; ps as usize]);

    // Partial failure: entry 1 sealed, entry 2 open (EPERM), entry 3 sealed -> EPERM in 1 RPC.
    let partial_entries: Vec<u8> = [
        MapEntry { map: map_args(open_seq, 101 * ps), flags: 0, reserved: 0 },
        MapEntry { map: map_args(2, 102 * ps), flags: 0, reserved: 0 },
    ]
    .iter()
    .flat_map(|entry| entry.as_bytes().to_vec())
    .collect();
    let partial = bytes(
        VIRTIO_EXTENT_T_MAP,
        VIRTIO_EXTENT_MAP_MANY,
        Some(map_args(1, 100 * ps)),
        &partial_entries,
    );
    let before = maps();
    assert_eq!(serve_with(&device, &partial, RESPONSE).1, errno(Errno::EPERM));
    assert_eq!(maps() - before, 1);

    // Entry 1 (page 100) was mapped before entry 2 failed; entries 2 and 3 (pages 101..=102)
    // were NOT mapped. Verify both by reading page 100 and inspecting the snapshot's mappings
    // (committing `open_seq` first so `make_version` allows the snapshot).
    assert_eq!(read_page(100), vec![1u8; ps as usize]);
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE).1, 0);
    device.take_snapshot(|| {}).unwrap();
    let mappings = &device.snapshot().unwrap().mappings;
    let at = |page: u64| mappings.iter().find(|m| m.window_offset == page * ps);
    assert_eq!(
        at(0),
        Some(&cladefs::WindowMapping {
            window_offset: 0,
            len: ps,
            contents: cladefs::MappingContents::Extent {
                extent: cladefs::ExtentId { cell, sequence: 1 },
                offset: 0,
            },
        })
    );
    assert_eq!(
        at(EXTENTS),
        Some(&cladefs::WindowMapping {
            window_offset: EXTENTS * ps,
            len: ps,
            contents: cladefs::MappingContents::Zeros,
        })
    );
    assert_eq!(
        at(100),
        Some(&cladefs::WindowMapping {
            window_offset: 100 * ps,
            len: ps,
            contents: cladefs::MappingContents::Extent {
                extent: cladefs::ExtentId { cell, sequence: 1 },
                offset: 0,
            },
        })
    );
    assert_eq!(at(101), None, "failing entry was mapped");
    assert_eq!(at(102), None, "entry after failing entry was mapped");
}

/// A device with a window reserves it of its VM, its private pages at most in its configuration,
/// creates it as the driver activates the device, and, restored from a snapshot, reserves it where
/// the snapshot had it; dropped, it removes its slot before releasing its addresses.
#[test]
fn windows_are_where_snapshots_had_them() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm = Arc::new(FakeVm::default());
    let config =
        Config { capacity: 4 << 20, dax_window_size: Some(4 << 20), ..config(&dir, "store") };
    let mut device = Device::new(config, 1, vm.clone(), lost.clone()).unwrap();
    let region = device.shm_region().unwrap();
    assert_eq!((region.id, region.guest_addr, region.len), (DAX_WINDOW_SHM_ID, 1 << 40, 4 << 20));
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let private_max = ConfigSpace::read_from_bytes(&space).unwrap().private_max;
    assert_eq!(u64::from_le(private_max), (4 << 20) / cladefs::pages::page_size());
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();

    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE).1, 0);
    device.take_snapshot(|| {}).unwrap();
    let mut state = device.state();
    assert_eq!(state.window_addr, Some(1 << 40));
    drop(device);
    state.config.path_on_host = dir.0.join("restored").display().to_string();
    state.window_addr = Some(3 << 40);
    let restored_vm = Arc::new(FakeVm::default());
    let restored = Device::restore(&state, 1, restored_vm.clone(), lost).unwrap();
    assert_eq!(restored.shm_region().unwrap().guest_addr, 3 << 40);
    drop(restored);
    assert_eq!(
        *vm.calls.lock().unwrap(),
        [
            "reserve 0x10000000000 0x400000",
            "add_slot 0x10000000000 0x400000",
            "remove_slot 1",
            "release 0x10000000000 0x400000"
        ]
    );
    assert_eq!(
        *restored_vm.calls.lock().unwrap(),
        [
            "reserve 0x30000000000 0x400000",
            "add_slot 0x30000000000 0x400000",
            "remove_slot 1",
            "release 0x30000000000 0x400000"
        ]
    );
}

/// A request queue as a VMM's transport has one, in the test's memory: the requests made
/// available, and those returned, with their lengths, which the test hears of as they are.
struct FakeQueue {
    available: std::collections::VecDeque<virtio_extent::Popped>,
    returned: mpsc::Sender<(u16, u32)>,
    used: Vec<(u16, u32)>,
}

impl virtio_extent::RequestQueue for FakeQueue {
    fn pop(&mut self) -> Result<Option<virtio_extent::Popped>, BoxError> {
        Ok(self.available.pop_front())
    }

    fn complete(&mut self, index: u16, len: u32) -> Result<bool, BoxError> {
        self.used.push((index, len));
        self.returned.send((index, len)).unwrap();
        Ok(true)
    }
}

/// A queue's interrupt that counts how often it is raised.
#[derive(Default)]
struct CountedQueue(AtomicUsize);

impl virtio_extent::QueueInterrupt for CountedQueue {
    fn notify(&self) -> Result<(), BoxError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The buffers of a request of `readable` bytes, with `writable` bytes for the device to write, in
/// memory that lives as long as the test.
fn leaked(readable: &[u8], writable: usize) -> (Buffers, &'static [u8]) {
    let readable = Box::leak(readable.to_vec().into_boxed_slice());
    let written: &'static mut [u8] = Box::leak(vec![0xa5; writable].into_boxed_slice());
    let ptr = written.as_ptr();
    let buffers = buffers(readable, written, usize::MAX);
    // SAFETY: Read once the device returned the request, which wrote it then.
    (buffers, unsafe { std::slice::from_raw_parts(ptr, writable) })
}

/// The device's workers take the requests the driver makes available as it notifies their
/// queue's event, serve them, return each to the driver with the length the device wrote,
/// interrupting it, and, stopped, give their queues back as they left them; a request whose chain
/// is none is returned with nothing written. Started again, they serve the same queue on.
#[test]
fn workers_take_serve_and_return_requests() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm: Arc<dyn WindowVm> = Arc::new(FakeVm::default());
    let mut device = Device::new(config(&dir, "store"), 1, vm, lost).unwrap();
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();

    let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let (info_buffers, info_written) = leaked(&info, size_of::<Info>() + RESPONSE);
    let (returned, returns) = mpsc::channel();
    let queue = FakeQueue {
        available: [
            virtio_extent::Popped { index: 3, buffers: Ok(info_buffers) },
            virtio_extent::Popped { index: 4, buffers: Err("not guest memory".into()) },
        ]
        .into(),
        returned,
        used: Vec::new(),
    };
    // SAFETY: A plain system call, whose result is checked.
    let event = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    assert!(event >= 0);
    // SAFETY: A new descriptor, the test's alone.
    let event = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(event) };
    let kick = || {
        let one = 1u64;
        // SAFETY: Writes `one`'s 8 bytes to the eventfd.
        let written = unsafe {
            libc::write(std::os::fd::AsRawFd::as_raw_fd(&event), (&raw const one).cast(), 8)
        };
        assert_eq!(written, 8);
    };
    let interrupt = Arc::new(CountedQueue::default());
    let queues = vec![virtio_extent::WorkerQueue {
        index: 0,
        queue,
        event: event.try_clone().unwrap(),
        interrupt: interrupt.clone(),
    }];
    let workers = device.start_workers(queues, "test_extent_q").unwrap();
    kick();
    let mut got = [returns.recv().unwrap(), returns.recv().unwrap()];
    got.sort_unstable();
    assert_eq!(got, [(3, (size_of::<Info>() + RESPONSE) as u32), (4, 0)]);
    let response = Response::read_from_bytes(&info_written[size_of::<Info>()..]).unwrap();
    assert_eq!(response.status, 0);
    assert_eq!(u64::from_le(read::<Info>(info_written).capacity), 1 << 20);

    let mut back = workers.stop();
    // A request is returned, then its interrupt raised, as virtio has it (the used element
    // before the interrupt), so a driver may see it returned first, as the test does above
    // (TASK-458); the workers stopped, every request they took is returned and interrupted.
    assert_eq!(interrupt.0.load(Ordering::Relaxed), 2);
    assert_eq!(back.len(), 1);
    let (index, mut queue) = back.pop().unwrap();
    assert_eq!((index, queue.used.len()), (0, 2));
    // Started again on the queue as it was left, they serve what was made available meanwhile.
    let (buffers, _) = leaked(&info, size_of::<Info>() + RESPONSE);
    queue.available.push_back(virtio_extent::Popped { index: 5, buffers: Ok(buffers) });
    let queues = vec![virtio_extent::WorkerQueue {
        index: 0,
        queue,
        event: event.try_clone().unwrap(),
        interrupt: interrupt.clone(),
    }];
    let workers = device.start_workers(queues, "test_extent_q").unwrap();
    kick();
    assert_eq!(returns.recv().unwrap(), (5, (size_of::<Info>() + RESPONSE) as u32));
    assert_eq!(workers.stop()[0].1.used.len(), 3);
    assert_eq!(interrupt.0.load(Ordering::Relaxed), 3);
}

/// TASK-535: a queue whose event poll can't wait on fails activation synchronously.
#[test]
fn workers_refuse_an_unwatchable_queue_event() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let mut device =
        Device::new(config(&dir, "store"), 2, Arc::new(FakeVm::default()), lost).unwrap();
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();
    // A regular file is an open descriptor, but one poll finds always readable: no event.
    let event = std::fs::File::create(dir.0.join("queue-event")).unwrap().into();
    let queue =
        FakeQueue { available: Default::default(), returned: mpsc::channel().0, used: Vec::new() };
    let queue = virtio_extent::WorkerQueue {
        index: 1,
        queue,
        event,
        interrupt: Arc::new(CountedQueue::default()),
    };
    // A watchable queue is already running when registration of the next one fails.
    let (returned, rollback) = mpsc::channel();
    let (watched, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let first = virtio_extent::WorkerQueue {
        index: 0,
        queue: FakeQueue { available: Default::default(), returned, used: Vec::new() },
        event: watched.into(),
        interrupt: Arc::new(CountedQueue::default()),
    };
    match device.start_workers(vec![first, queue], "unwatchable") {
        Err(_) => assert_eq!(rollback.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
        Ok(workers) => {
            workers.stop();
            panic!("the device started a queue whose event it cannot watch");
        }
    }
}

/// 4 KiB-aligned MAP, PRIVATE, UNMAP and snapshot through `virtio_extent::Device` work on both
/// 4 KiB and 16 KiB hosts (advertising `map_alignment == BLOCK_SIZE` so 4 KiB guests enable DAX).
#[test]
fn block_aligned_dax_maps_and_private_work_on_any_host_page_size() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm = Arc::new(FakeVm::default());
    let config =
        Config { capacity: 4 << 20, dax_window_size: Some(4 << 20), ..config(&dir, "store") };
    let mut device = Device::new(config, 1, vm.clone(), lost).unwrap();
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let cfg = ConfigSpace::read_from_bytes(&space).unwrap();
    assert_eq!(u32::from_le(cfg.block_size), BLOCK as u32);
    assert_eq!(u32::from_le(cfg.map_alignment), BLOCK as u32);

    device.activate(device.features(), Arc::new(Counted::default())).unwrap();
    let base = vm.host.load(Ordering::Relaxed) as *mut u8;
    assert!(!base.is_null(), "DAX window slot was not mapped");
    let cell = u64::from_le(cfg.cell);
    let id = |sequence: u64| ExtentIdLe { cell: cell.to_le(), sequence: sequence.to_le() };

    // Extent 1: 4 distinct 4 KiB blocks (0x10, 0x11, 0x12, 0x13); Extent 2: 1 block (0x22).
    let mut ext1 = Vec::with_capacity(4 * BLOCK);
    for b in 0..4u8 {
        ext1.extend(std::iter::repeat_n(0x10 + b, BLOCK));
    }
    let w1 = bytes(
        VIRTIO_EXTENT_T_WRITE,
        0,
        Some(IoArgs { id: id(1), offset: 0, len: (ext1.len() as u32).to_le(), root_len: 0 }),
        &ext1,
    );
    assert_eq!(serve_with(&device, &w1, RESPONSE).1, 0);
    let w2 = bytes(
        VIRTIO_EXTENT_T_WRITE,
        0,
        Some(IoArgs { id: id(2), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &vec![0x22u8; BLOCK],
    );
    assert_eq!(serve_with(&device, &w2, RESPONSE).1, 0);
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);

    // Map 4 KiB at unaligned extent offset BLOCK into window offset 0, and 4 KiB at extent offset
    // 3*BLOCK into window offset BLOCK (same 16 KiB host page on arm64).
    let map = |seq: u64, ext_off: u64, win_off: u64, len: u64| {
        let args = MapArgs {
            id: id(seq),
            offset: ext_off.to_le(),
            len: len.to_le(),
            window_offset: win_off.to_le(),
        };
        bytes(VIRTIO_EXTENT_T_MAP, 0, Some(args), &[])
    };
    assert_eq!(serve_with(&device, &map(1, BLOCK as u64, 0, BLOCK as u64), RESPONSE).1, 0);
    assert_eq!(
        serve_with(&device, &map(1, (3 * BLOCK) as u64, BLOCK as u64, BLOCK as u64), RESPONSE).1,
        0
    );

    // SAFETY: Within the mapped DAX window of 4 MiB, served by the activated device.
    let read_block = |blk: usize| unsafe { base.add(blk * BLOCK).read_volatile() };
    let write_block = |blk: usize, val: u8| unsafe { base.add(blk * BLOCK).write_volatile(val) };
    assert_eq!((read_block(0), read_block(1)), (0x11, 0x13));

    // Make block 0 private and dirty it; remap sibling block 1 to extent 2 (0x22) and verify
    // block 0 retains its dirty byte while block 1 reflects the new mapping.
    let priv0 = bytes(
        VIRTIO_EXTENT_T_PRIVATE,
        0,
        Some(UnmapArgs { window_offset: 0, len: (BLOCK as u64).to_le() }),
        &[],
    );
    assert_eq!(serve_with(&device, &priv0, RESPONSE).1, 0);
    write_block(0, 0xaa);
    assert_eq!(serve_with(&device, &map(2, 0, BLOCK as u64, BLOCK as u64), RESPONSE).1, 0);
    assert_eq!((read_block(0), read_block(1)), (0xaa, 0x22));

    // Rebind block 0 to extent 2 -> no private pages left -> freeze + snapshot succeeds.
    assert_eq!(serve_with(&device, &map(2, 0, 0, BLOCK as u64), RESPONSE).1, 0);
    assert_eq!((read_block(0), read_block(1)), (0x22, 0x22));
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE).1, 0);
    device.take_snapshot(|| {}).unwrap();
    let state = device.state();
    assert_eq!(state.mappings.len(), 2);
}

/// Host-local suspend/resume of an ephemeral rootfs cell: takes a `LocalEphemeral` snapshot
/// without creating a bucket version, restores in place, continues writing, re-snapshots at a
/// higher commit, and rejects restoring the stale snapshot with `EEXIST`.
#[test]
fn ephemeral_device_snapshots_restores_resuspends_and_rejects_stale_restore() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let store = dir.0.join("ephemeral-root");
    // Create a standalone ephemeral Fxfs cell via OpenCell so it starts ephemeral on disk.
    let opening = cladefs::Opening::here().unwrap();
    let created = futures::executor::block_on(cladefs::OpenCell::create_ephemeral_with(
        &store,
        4 << 20,
        0,
        opening,
    ))
    .unwrap();
    let cell_id = created.cell();
    futures::executor::block_on(created.close());

    let vm1 = Arc::new(FakeVm::default());
    let cfg = Config {
        capacity: 4 << 20,
        dax_window_size: Some(4 << 20),
        ..config(&dir, "ephemeral-root")
    };
    let mut device = Device::new(cfg, 1, vm1.clone(), lost.clone()).unwrap();
    device.activate(device.features(), Arc::new(Counted::default())).unwrap();

    // Write extent 1 and commit, then map it into the DAX window.
    let id = |seq: u64| ExtentIdLe { cell: cell_id.id.to_le(), sequence: seq.to_le() };
    let w1 = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_COMMIT,
        Some(IoArgs { id: id(1), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &[0x11u8; BLOCK],
    );
    assert_eq!(serve_with(&device, &w1, RESPONSE).1, 0);
    let map1 = bytes(
        VIRTIO_EXTENT_T_MAP,
        0,
        Some(MapArgs { id: id(1), offset: 0, len: (BLOCK as u64).to_le(), window_offset: 0 }),
        &[],
    );
    assert_eq!(serve_with(&device, &map1, RESPONSE).1, 0);

    // Freeze and take snapshot #1 (must record LocalEphemeral at commit 1).
    let freeze = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, VIRTIO_EXTENT_FREEZE_FROZEN, None, &[]);
    assert_eq!(serve_with(&device, &freeze, RESPONSE).1, 0);
    device.take_snapshot(|| {}).unwrap();
    let state1 = device.state();
    assert_eq!(
        state1.target,
        cladefs::CellSnapshot::LocalEphemeral {
            tenant: cell_id.tenant,
            cell: cell_id.id,
            committed: 1,
        }
    );
    let encoded1 = state1.encode();
    let decoded1 = virtio_extent::persist::State::decode(&encoded1).unwrap();
    assert_eq!(decoded1, state1);

    // Stop VMM #1.
    drop(device);

    // Restore VMM #2 in place from snapshot #1, verify DAX window mapping, thaw, write extent 2.
    let vm2 = Arc::new(FakeVm::default());
    let mut restored = Device::restore(&decoded1, 1, vm2.clone(), lost.clone()).unwrap();
    restored.activate(restored.features(), Arc::new(Counted::default())).unwrap();
    let base2 = vm2.host.load(Ordering::Relaxed) as *const u8;
    // SAFETY: Within the mapped DAX window of 4 MiB, served by `restored`.
    assert_eq!(unsafe { base2.read_volatile() }, 0x11);

    let thaw = bytes::<Info>(VIRTIO_EXTENT_T_FREEZE, 0, None, &[]);
    assert_eq!(serve_with(&restored, &thaw, RESPONSE).1, 0);
    let w2 = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_COMMIT,
        Some(IoArgs { id: id(2), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &[0x22u8; BLOCK],
    );
    assert_eq!(serve_with(&restored, &w2, RESPONSE).1, 0);
    assert_eq!(serve_with(&restored, &freeze, RESPONSE).1, 0);
    restored.take_snapshot(|| {}).unwrap();
    let state2 = restored.state();
    assert_eq!(
        state2.target,
        cladefs::CellSnapshot::LocalEphemeral {
            tenant: cell_id.tenant,
            cell: cell_id.id,
            committed: 2,
        }
    );
    drop(restored);

    // Attempting to restore stale snapshot #1 must fail with EEXIST.
    let stale_err =
        Device::restore(&decoded1, 1, Arc::new(FakeVm::default()), lost.clone()).err().unwrap();
    match stale_err {
        virtio_extent::Error::Cell(_, err) => assert_eq!(err.errno(), Some(Errno::EEXIST)),
        other => panic!("expected EEXIST for stale ephemeral snapshot, got {other}"),
    }

    // Restoring snapshot #2 succeeds and reads both extent 1 and extent 2.
    let vm3 = Arc::new(FakeVm::default());
    let mut restored2 = Device::restore(&state2, 1, vm3, lost).unwrap();
    restored2.activate(restored2.features(), Arc::new(Counted::default())).unwrap();
    let read2 = bytes(
        VIRTIO_EXTENT_T_READ,
        0,
        Some(IoArgs { id: id(2), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &[],
    );
    let (used, status) = serve_with(&restored2, &read2, BLOCK + RESPONSE);
    assert_eq!((used as usize, status), (BLOCK + RESPONSE, 0));
    drop(restored2);
}

/// Live-migration snapshot (`SnapshotMode::Migration`) records DAX mappings without requiring
/// `VIRTIO_EXTENT_T_FREEZE` or calling `make_version`, while still enforcing quiescence (no private
/// pages, no open extents), and restores directly against the existing cell while rejecting a
/// mismatched cell ID. `release_cell` closes the cell cleanly on a paused device (refusing
/// `take_snapshot` while released and refusing release on private pages or open extents), and
/// `reopen_cell` reopens the cell and restores its DAX window mappings (preserving rollback state
/// if reopening fails).
#[test]
fn migration_snapshot_and_release_reopen_cell() {
    let dir = daemon();
    let lost_count = Arc::new(AtomicUsize::new(0));
    let lost_counter = lost_count.clone();
    let lost: virtio_extent::OnLost = Arc::new(move || {
        lost_counter.fetch_add(1, Ordering::Relaxed);
    });
    let vm = Arc::new(FakeVm::default());
    let cfg = Config { capacity: 4 << 20, dax_window_size: Some(4 << 20), ..config(&dir, "store") };
    let mut device = Device::new(cfg, 1, vm.clone(), lost.clone()).unwrap();
    let mut space = [0u8; size_of::<ConfigSpace>()];
    device.read_config(0, &mut space);
    let cell = u64::from_le(ConfigSpace::read_from_bytes(&space).unwrap().cell);
    let id = |sequence: u64| ExtentIdLe { cell: cell.to_le(), sequence: sequence.to_le() };

    device.activate(device.features(), Arc::new(Counted::default())).unwrap();
    let base = vm.host.load(Ordering::Relaxed) as *mut u8;
    assert!(!base.is_null());

    // Write and commit extent 1, map it at window offset 0, and map a zero page at BLOCK.
    let w1 = bytes(
        VIRTIO_EXTENT_T_WRITE,
        0,
        Some(IoArgs { id: id(1), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &vec![0x33u8; BLOCK],
    );
    assert_eq!(serve_with(&device, &w1, RESPONSE).1, 0);
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);
    let map = |seq: u64, win_off: u64| {
        let args = MapArgs {
            id: id(seq),
            offset: 0,
            len: (BLOCK as u64).to_le(),
            window_offset: win_off.to_le(),
        };
        bytes(VIRTIO_EXTENT_T_MAP, 0, Some(args), &[])
    };
    let map_zero = |win_off: u64| {
        let args = MapArgs {
            id: ExtentIdLe { cell: 0, sequence: 0 },
            offset: 0,
            len: (BLOCK as u64).to_le(),
            window_offset: win_off.to_le(),
        };
        bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_ZEROS, Some(args), &[])
    };
    assert_eq!(serve_with(&device, &map(1, 0), RESPONSE).1, 0);
    assert_eq!(serve_with(&device, &map_zero(BLOCK as u64), RESPONSE).1, 0);
    // SAFETY: Within the 4 MiB mapped DAX window.
    let read_block = |blk: usize| unsafe { base.add(blk * BLOCK).read_volatile() };
    assert_eq!((read_block(0), read_block(1)), (0x33, 0x00));

    // Versioned snapshot still refuses without freeze.
    assert!(matches!(
        device.take_snapshot(|| {}),
        Err(virtio_extent::persist::SnapshotError::NotFrozen(_))
    ));

    // Uncommitted open extent blocks both Migration snapshot and release_cell.
    let w_open = bytes(
        VIRTIO_EXTENT_T_WRITE,
        0,
        Some(IoArgs { id: id(2), offset: 0, len: (BLOCK as u32).to_le(), root_len: 0 }),
        &vec![0x44u8; BLOCK],
    );
    assert_eq!(serve_with(&device, &w_open, RESPONSE).1, 0);
    assert!(matches!(
        device.take_snapshot_with(virtio_extent::SnapshotMode::Migration, || {}),
        Err(virtio_extent::persist::SnapshotError::NotQuiescent(_))
    ));
    assert!(matches!(
        device.release_cell(|| {}),
        Err(virtio_extent::ReleaseError::NotQuiescent(_))
    ));
    assert!(!device.is_cell_released());
    assert_eq!(serve_with(&device, &commit, RESPONSE).1, 0);

    // Private DAX page blocks both Migration snapshot and release_cell.
    let priv0 = bytes(
        VIRTIO_EXTENT_T_PRIVATE,
        0,
        Some(UnmapArgs { window_offset: 0, len: (BLOCK as u64).to_le() }),
        &[],
    );
    assert_eq!(serve_with(&device, &priv0, RESPONSE).1, 0);
    assert!(matches!(
        device.take_snapshot_with(virtio_extent::SnapshotMode::Migration, || {}),
        Err(virtio_extent::persist::SnapshotError::Private(_, _))
    ));
    assert!(matches!(device.release_cell(|| {}), Err(virtio_extent::ReleaseError::Private(_, _))));
    assert_eq!(serve_with(&device, &map(1, 0), RESPONSE).1, 0);

    // Migration snapshot succeeds without freeze and creates no version (version == 0, cell == cell).
    device.take_snapshot_with(virtio_extent::SnapshotMode::Migration, || {}).unwrap();
    let snap = device.snapshot().unwrap().clone();
    assert_eq!(
        snap.target,
        cladefs::CellSnapshot::Version(cladefs::CellVersion { tenant: 0, cell, version: 0 })
    );
    assert_eq!(snap.mappings.len(), 2);
    let state = device.state();
    assert!(!state.frozen);

    // Release closes the cell cleanly without triggering `on_lost`; release is idempotent;
    // take_snapshot is refused while released.
    device.release_cell(|| {}).unwrap();
    assert!(device.is_cell_released());
    assert_eq!(lost_count.load(Ordering::Relaxed), 0, "release must not trigger on_lost");
    device.release_cell(|| {}).unwrap();
    assert!(matches!(
        device.take_snapshot_with(virtio_extent::SnapshotMode::Migration, || {}),
        Err(virtio_extent::persist::SnapshotError::Released(_))
    ));

    // Reopen restores the cell and DAX window mappings (abort path).
    device.reopen_cell().unwrap();
    assert!(!device.is_cell_released());
    device.reopen_cell().unwrap(); // idempotent
    assert_eq!((read_block(0), read_block(1)), (0x33, 0x00));

    // Release again and restore a second device from the migration snapshot state (simulating
    // destination load over the handed-over cell).
    device.release_cell(|| {}).unwrap();
    let restored_vm = Arc::new(FakeVm::default());
    let mut restored = Device::restore(&state, 1, restored_vm.clone(), lost.clone()).unwrap();
    restored.activate(restored.features(), Arc::new(Counted::default())).unwrap();
    let restored_base = restored_vm.host.load(Ordering::Relaxed) as *mut u8;
    // SAFETY: Within the restored device's mapped DAX window.
    assert_eq!(unsafe { restored_base.read_volatile() }, 0x33);
    // SAFETY: Within the restored device's mapped DAX window.
    assert_eq!(unsafe { restored_base.add(BLOCK).read_volatile() }, 0x00);

    // Mismatched cell ID in migration snapshot state is refused on restore.
    drop(restored);
    let mut wrong_cell = state.clone();
    wrong_cell.target = cladefs::CellSnapshot::Version(cladefs::CellVersion {
        tenant: 0,
        cell: cell ^ 0x1,
        version: 0,
    });
    let err = Device::restore(&wrong_cell, 1, Arc::new(FakeVm::default()), lost).unwrap_err();
    assert!(matches!(err, virtio_extent::Error::CellMismatch { .. }), "got {err:?}");
    drop(device);
    assert_eq!(lost_count.load(Ordering::Relaxed), 0);
}
