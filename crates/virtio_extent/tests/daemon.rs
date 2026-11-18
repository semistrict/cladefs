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
    BoxError, Buffers, CacheType, Config, ConfigInterrupt, Device, Notices, WindowVm, handle,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;
const RESPONSE: usize = size_of::<Response>();

/// The bounds of a device of the test's stores, which have no window (TASK-470).
fn bounds() -> Bounds {
    Bounds {
        block_size: BLOCK as u64,
        max_extent: MAX_EXTENT,
        map_alignment: cladefs::pages::page_size(),
        window: 0,
    }
}

/// Makes this process a host of the multi-node system, as an extent daemon's must be: its stores'
/// branches in a bucket in its memory, as of placement 1, unless the test's environment names
/// others (scripts/test.sh's firecracker part names memory:///vmm). The daemon logs to standard
/// error, which a failed test shows: its EIO opening a store with no bucket named said nothing of
/// why (cladefs TASK-220).
fn host() {
    let _ = cladefs::cladefsd::logging::to_stderr();
    for (name, value) in [("CLADEFSD_BUCKET", "memory:///branches"), ("CLADEFSD_TOKEN", "1")] {
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

/// A store of `capacity` bytes, which a daemon of the test's serves.
fn store_of(capacity: u64) -> (CellClient, TestDir) {
    let dir = daemon();
    let store = CellClient::connect(dir.socket(), dir.0.join("store"), capacity).unwrap();
    (store, dir)
}

fn store() -> (CellClient, TestDir) {
    store_of(1 << 20)
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
fn serve(store: &CellClient, readable: &[u8], writable: usize, split: usize) -> (usize, Vec<u8>) {
    let mut readable = readable.to_vec();
    let mut written = vec![0xa5; writable];
    let buffers = buffers(&mut readable, &mut written, split);
    let notices = Notices::default();
    let served = handle(store, &notices, CacheType::Writeback, bounds(), buffers);
    let used = futures::executor::block_on(served);
    (used as usize, written)
}

/// Serves a request as [`serve`] does, and returns what the device wrote, the response apart, and
/// the response's status.
fn request_split(
    store: &CellClient,
    readable: &[u8],
    writable: usize,
    split: usize,
) -> (Vec<u8>, u32) {
    let (used, mut written) = serve(store, readable, writable, split);
    assert_eq!(used, writable, "the device wrote {used} of {writable} bytes");
    let response = Response::read_from_bytes(&written[writable - RESPONSE..]).unwrap();
    written.truncate(writable - RESPONSE);
    (written, u32::from_le(response.status))
}

fn request(store: &CellClient, readable: &[u8], writable: usize) -> (Vec<u8>, u32) {
    request_split(store, readable, writable, usize::MAX)
}

fn read<T: FromBytes>(bytes: &[u8]) -> T {
    T::read_from_prefix(bytes).unwrap().0
}

fn io(store: &CellClient, sequence: u64, offset: u64, len: usize) -> IoArgs {
    IoArgs {
        id: ExtentIdLe { branch: store.branch().to_le(), sequence: sequence.to_le() },
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
    let (store, _dir) = store();
    let asked = Some(StatsArgs { asked: 1u64.to_le() });
    let before = served("stats");
    let stats = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals 1\ncommit abc\n");
    assert_eq!(request(&store, &stats, RESPONSE), (vec![], 0));
    assert_eq!(served("stats"), before + 1);
    let not_text = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, b"seals \xff\n");
    assert_eq!(request(&store, &not_text, RESPONSE).1, errno(Errno::EINVAL));
    let long = bytes(VIRTIO_EXTENT_T_STATS, 0, asked, &vec![b'\n'; MAX_STATS + 1]);
    assert_eq!(request(&store, &long, RESPONSE).1, errno(Errno::EINVAL));
    assert_eq!(served("stats"), before + 1);
}

#[test]
fn writes_and_reads() {
    let (store, _dir) = store();
    let data: Vec<u8> = (0..2 * BLOCK).map(|i| i as u8).collect();
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 1, 0, data.len())), &data);
    assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
    let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 1, 0, data.len())), &[]);
    assert_eq!(request(&store, &read, data.len() + RESPONSE), (data.clone(), 0));
    // Across descriptors of odd sizes, whose writes and reads the device splits and joins.
    assert_eq!(request_split(&store, &read, data.len() + RESPONSE, 1000), (data.clone(), 0));
    let rewrite = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 2, 0, data.len())), &data);
    assert_eq!(request_split(&store, &rewrite, RESPONSE, 777), (vec![], 0));
    let read2 = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 2, 0, data.len())), &[]);
    assert_eq!(request(&store, &read2, data.len() + RESPONSE), (data.clone(), 0));

    // An extent the store doesn't hold, and a write after a commit sealed the extent.
    let missing = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 3, 0, BLOCK)), &[]);
    assert_eq!(request(&store, &missing, BLOCK + RESPONSE), (vec![0; BLOCK], errno(Errno::ENOENT)));
    let commit = bytes(
        VIRTIO_EXTENT_T_COMMIT,
        VIRTIO_EXTENT_COMMIT_UPLOADED,
        Some(CommitArgs::default()),
        &[],
    );
    assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
    let sealed = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 1, 0, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&store, &sealed, RESPONSE).1, errno(Errno::EPERM));
    // A barrier commits what came before it.
    let barrier = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_BARRIER,
        Some(io(&store, 4, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&store, &barrier, RESPONSE).1, 0);
    // A commit after a write seals what it wrote, in one request: and only a WRITE's flags are a
    // WRITE's.
    let committed = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_UPLOADED,
        Some(io(&store, 5, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&store, &committed, RESPONSE).1, 0);
    let after =
        bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 5, BLOCK as u64, BLOCK)), &data[..BLOCK]);
    assert_eq!(request(&store, &after, RESPONSE).1, errno(Errno::EPERM));
    let unknown = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_UPLOADED,
        Some(io(&store, 6, 0, BLOCK)),
        &data[..BLOCK],
    );
    assert_eq!(request(&store, &unknown, RESPONSE).1, errno(Errno::EINVAL));

    let free = bytes(VIRTIO_EXTENT_T_FREE, 0, Some(io(&store, 1, 0, 0).id), &[]);
    assert_eq!(request(&store, &free, RESPONSE), (vec![], 0));
    assert_eq!(request(&store, &read, data.len() + RESPONSE).1, errno(Errno::ENOENT));
}

/// cladefs: a FREE with FREE_MANY, of a flush's dead extents, as many as the guest's Fxfs names in
/// one (257), frees them in one request of the daemon's, not one each, each a round trip to it
/// (TASK-92.13).
#[test]
fn frees_many_in_one_request() {
    const EXTENTS: u64 = 257;
    let (store, _dir) = store_of(4 << 20);
    let data = vec![7; BLOCK];
    let ids: Vec<ExtentIdLe> = (1..=EXTENTS)
        .map(|sequence| {
            let id = cladefs::ExtentId { branch: store.branch(), sequence };
            store.write(id, 0, &[cladefs::VolatileSlice::from(&data[..])]).unwrap();
            ExtentIdLe { branch: store.branch().to_le(), sequence: sequence.to_le() }
        })
        .collect();
    store.commit(None).unwrap();
    let rest: Vec<u8> = ids[1..].iter().flat_map(|id| id.as_bytes().to_vec()).collect();
    let free = bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(ids[0]), &rest);
    let frees = || served("free") + served("free_many");
    let before = frees();
    let start = Instant::now();
    assert_eq!(request(&store, &free, RESPONSE), (vec![], 0));
    let took = start.elapsed();
    let served = frees() - before;
    println!("{EXTENTS} extents freed in {took:?}, in {served} of the daemon's requests");
    assert_eq!(served, 1);
    for sequence in [1, EXTENTS / 2, EXTENTS] {
        let read = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, sequence, 0, BLOCK)), &[]);
        assert_eq!(
            request(&store, &read, BLOCK + RESPONSE).1,
            errno(Errno::ENOENT),
            "extent {sequence} is still there"
        );
    }
}

#[test]
fn roots_and_info() {
    let (store, _dir) = store();
    let root = bytes::<RootLen>(VIRTIO_EXTENT_T_ROOT, 0, None, &[]);
    let root_len = MAX_ROOT as usize + size_of::<RootLen>();
    let (written, status) = request(&store, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(read::<RootLen>(&written[MAX_ROOT as usize..]).len, 0);

    let commit = bytes(
        VIRTIO_EXTENT_T_COMMIT,
        VIRTIO_EXTENT_COMMIT_ROOT,
        Some(CommitArgs { root_len: 5u32.to_le(), reserved: 0 }),
        b"first",
    );
    assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
    let (written, status) = request(&store, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(&written[..5], b"first");
    assert!(written[5..MAX_ROOT as usize].iter().all(|b| *b == 0));
    assert_eq!(u32::from_le(read::<RootLen>(&written[MAX_ROOT as usize..]).len), 5);

    let info = bytes::<Info>(VIRTIO_EXTENT_T_INFO, 0, None, &[]);
    let (written, status) = request(&store, &info, size_of::<Info>() + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(
        read::<Info>(&written),
        Info {
            capacity: (1u64 << 20).to_le(),
            used: 0,
            available: (1u64 << 20).to_le(),
            next_sequence: 0,
            // The store's branch is kept in the daemon's bucket: its writer mints from its range,
            // its first taking's (cladefs's doc-7, The head).
            sequence_floor: (1u64 << 40).to_le(),
            branch: store.branch().to_le(),
        }
    );
    // cladefs: room for what comes before the branch alone, as a driver without F_FREEZE had, is
    // refused (TASK-141.3).
    let short = std::mem::offset_of!(Info, branch);
    assert_eq!(request(&store, &info, short + RESPONSE).1, errno(Errno::EINVAL));

    // cladefs: a WRITE's commit sets the root after its data, which isn't the data's; there is no
    // root without a commit to set it.
    let data = [7u8; BLOCK];
    let rooted = IoArgs { root_len: 6u32.to_le(), ..io(&store, 1, 0, BLOCK) };
    let flags = VIRTIO_EXTENT_WRITE_COMMIT | VIRTIO_EXTENT_WRITE_ROOT;
    let write = bytes(VIRTIO_EXTENT_T_WRITE, flags, Some(rooted), &[&data[..], b"second"].concat());
    assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
    let (written, status) = request(&store, &root, root_len + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(&written[..6], b"second");
    let mut back = vec![0; BLOCK];
    let id = cladefs::ExtentId { branch: store.branch(), sequence: 1 };
    store.read(id, 0, &mut [cladefs::VolatileSliceMut::from(&mut back[..])]).unwrap();
    assert_eq!(back, data);
    let uncommitted = bytes(
        VIRTIO_EXTENT_T_WRITE,
        VIRTIO_EXTENT_WRITE_ROOT,
        Some(IoArgs { root_len: 5u32.to_le(), ..io(&store, 2, 0, BLOCK) }),
        &[&data[..], b"third"].concat(),
    );
    assert_eq!(request(&store, &uncommitted, RESPONSE).1, errno(Errno::EINVAL));
}

/// cladefs: a follower's device reads its writer's store as of its view's commit, advances to the
/// writer's latest, lets go of the commits before and pins extents; what the writer wrote since it
/// reads as ENOENT, and what would write, it refuses (EROFS). A writer's device refuses a
/// follower's requests (EINVAL).
#[test]
fn follows_a_writer() {
    let (store, dir) = store();
    let follower = CellClient::connect_read_only(dir.socket(), dir.0.join("store")).unwrap();
    let (heard, commits) = mpsc::channel();
    follower.on_committed(move |committed| {
        // Unheard once the test stops listening.
        let _ = heard.send(committed);
    });
    let data = vec![7; BLOCK];
    let id = cladefs::ExtentId { branch: store.branch(), sequence: 1 };
    store.write(id, 0, &[cladefs::VolatileSlice::from(&data[..])]).unwrap();
    store.commit(None).unwrap();
    let committed = commits.recv().unwrap();
    let read_ = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 1, 0, BLOCK)), &[]);
    assert_eq!(
        request(&follower, &read_, BLOCK + RESPONSE),
        (vec![0; BLOCK], errno(Errno::ENOENT))
    );
    let advance = bytes::<CommitNumber>(VIRTIO_EXTENT_T_ADVANCE, 0, None, &[]);
    let (written, status) = request(&follower, &advance, size_of::<CommitNumber>() + RESPONSE);
    assert_eq!(status, 0);
    assert_eq!(
        read::<CommitNumber>(&written),
        CommitNumber { commit: committed.to_le(), reserved: 0 }
    );
    assert_eq!(request(&follower, &read_, BLOCK + RESPONSE), (data.clone(), 0));
    let let_go = bytes(
        VIRTIO_EXTENT_T_LET_GO,
        0,
        Some(CommitNumber { commit: committed.to_le(), reserved: 0 }),
        &[],
    );
    assert_eq!(request(&follower, &let_go, RESPONSE), (vec![], 0));
    let id = io(&store, 1, 0, 0).id;
    for type_ in [VIRTIO_EXTENT_T_PIN, VIRTIO_EXTENT_T_UNPIN] {
        let pin = bytes(type_, 0, Some(id), &[]);
        assert_eq!(request(&follower, &pin, RESPONSE), (vec![], 0));
        assert_eq!(request(&store, &pin, RESPONSE).1, errno(Errno::EINVAL));
    }
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 2, 0, BLOCK)), &data);
    assert_eq!(request(&follower, &write, RESPONSE).1, errno(Errno::EROFS));
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(request(&follower, &commit, RESPONSE).1, errno(Errno::EROFS));
    assert_eq!(
        request(&store, &advance, size_of::<CommitNumber>() + RESPONSE).1,
        errno(Errno::EINVAL)
    );
}

/// cladefs TASK-77: a guest's driver is untrusted: whatever a request says, of the types and flags
/// it may name, of extents of its branch or another's, at offsets near the store's ends or at the
/// ends of a u64, the device answers it, with a status, no more than the room it was given
/// written, and goes on serving. Seeded by proptest, which says the case if one fails.
#[test]
fn answers_whatever_a_driver_asks() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};

    let (store, _dir) = store_of(64 << 20);
    // cladefs TASK-470: and blocks' multiples, which reads and writes take.
    let near_or_far = || {
        prop_oneof![
            0u64..(1 << 22),
            (0u64..16).prop_map(|blocks| blocks * BLOCK as u64),
            any::<u64>(),
            (0u64..4096).prop_map(|back| u64::MAX - back)
        ]
    };
    let branch = prop_oneof![Just(store.branch()), any::<u64>()];
    let args = (branch, 0u64..64, near_or_far(), near_or_far(), any::<u32>());
    let word = || prop_oneof![0u32..18, any::<u32>()];
    let data = proptest::collection::vec(any::<u8>(), 0..2 * BLOCK);
    let requests = (word(), word(), args, data, 0usize..4 * BLOCK);
    let config = Config { cases: 5000, failure_persistence: None, ..Config::default() };
    // Those the device served: refusing each at its header would show nothing.
    let served = AtomicU32::new(0);
    TestRunner::new(config)
        .run(&requests, |(type_, flags, args, data, room)| {
            let (branch, sequence, first, second, last) = args;
            let mut readable = bytes::<Info>(type_, flags, None, &[]);
            for word in [branch, sequence, first, second, u64::from(last)] {
                readable.extend_from_slice(&word.to_le_bytes());
            }
            readable.extend_from_slice(&data);
            let writable = RESPONSE + room;
            // As a queue's 16 descriptors would hold it.
            let split = (readable.len() + writable) / 14 + 1;
            let (used, written) = serve(&store, &readable, writable, split);
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
    assert_eq!(request(&store, &info, size_of::<Info>() + RESPONSE).1, 0);
}

#[test]
fn refuses_bad_requests() {
    let (store, _dir) = store();
    let unknown = bytes::<Info>(99, 0, None, &[]);
    assert_eq!(request(&store, &unknown, RESPONSE).1, errno(Errno::EINVAL));
    // A read with less room than it reads, and one whose header is short.
    let read_ = bytes(VIRTIO_EXTENT_T_READ, 0, Some(io(&store, 1, 0, 2 * BLOCK)), &[]);
    assert_eq!(request(&store, &read_, BLOCK + RESPONSE), (vec![0; BLOCK], errno(Errno::EINVAL)));
    assert_eq!(request(&store, &[0; 4], RESPONSE).1, errno(Errno::EINVAL));
    // A write whose data is shorter than it says.
    let short = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(io(&store, 1, 0, BLOCK)), &[1; 100]);
    assert_eq!(request(&store, &short, RESPONSE).1, errno(Errno::EINVAL));
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
        assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::ENOSYS));
        let flags = flags | VIRTIO_EXTENT_MAP_MANY;
        let map = bytes(VIRTIO_EXTENT_T_MAP, flags, Some(MapArgs::default()), &many);
        assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::ENOSYS));
    }
    for unknown in [1 << 0, 1 << 3] {
        let map = bytes(VIRTIO_EXTENT_T_MAP, unknown, Some(MapArgs::default()), &[]);
        assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::EINVAL));
    }
    // cladefs: zeros, and PRIVATE, of a store without a window that copies on write (doc-13).
    let zeros = entries(&[VIRTIO_EXTENT_MAP_ZEROS]);
    let map = bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &zeros);
    assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::EINVAL));
    let private = bytes(VIRTIO_EXTENT_T_PRIVATE, 0, Some(UnmapArgs::default()), &[]);
    assert_eq!(request(&store, &private, RESPONSE).1, errno(Errno::EINVAL));
    let write_flag = entries(&[1 << 1]);
    let map =
        bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &write_flag);
    assert_eq!(request(&store, &map, RESPONSE).1, errno(Errno::EINVAL));
}

/// cladefs: a FREE_MANY's or a MAP_MANY's data past MAX_MANY_DATA is refused, nothing done: the
/// device lists what it names in its own memory, and a chain naming the same guest memory again
/// and again names as much as it likes, which grew the VMM by as much. Up to it, the request is
/// served.
#[test]
fn refuses_many_past_their_most() {
    let (store, _dir) = store();
    let extent = io(&store, 1, 0, BLOCK);
    let write = bytes(VIRTIO_EXTENT_T_WRITE, 0, Some(extent), &[7; BLOCK]);
    assert_eq!(request(&store, &write, RESPONSE), (vec![], 0));
    let commit = bytes(VIRTIO_EXTENT_T_COMMIT, 0, Some(CommitArgs::default()), &[]);
    assert_eq!(request(&store, &commit, RESPONSE), (vec![], 0));
    let read_ = bytes(VIRTIO_EXTENT_T_READ, 0, Some(extent), &[]);

    // The extent named again past the most: refused, and the extent is there still.
    let id = extent.id.as_bytes();
    let free = |data_len: usize| {
        let data = id.repeat(data_len / id.len());
        bytes(VIRTIO_EXTENT_T_FREE, VIRTIO_EXTENT_FREE_MANY, Some(extent.id), &data)
    };
    assert_eq!(request(&store, &free(MAX_MANY_DATA + id.len()), RESPONSE).1, errno(Errno::EINVAL));
    assert_eq!(request(&store, &read_, BLOCK + RESPONSE), (vec![7; BLOCK], 0));
    // Up to the most, served: the extent freed, as often as it is named, and then not found.
    assert_eq!(request(&store, &free(MAX_MANY_DATA), RESPONSE), (vec![], 0));
    assert_eq!(request(&store, &read_, BLOCK + RESPONSE).1, errno(Errno::ENOENT));

    // Maps past the most: refused; up to it, served, as a store without a window serves maps.
    let entry = MapEntry::default();
    let entry = entry.as_bytes();
    let map = |data_len: usize| {
        let data = entry.repeat(data_len / entry.len());
        bytes(VIRTIO_EXTENT_T_MAP, VIRTIO_EXTENT_MAP_MANY, Some(MapArgs::default()), &data)
    };
    assert_eq!(
        request(&store, &map(MAX_MANY_DATA + entry.len()), RESPONSE).1,
        errno(Errno::EINVAL)
    );
    assert_eq!(request(&store, &map(MAX_MANY_DATA), RESPONSE).1, errno(Errno::ENOSYS));
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

    fn add_slot(&self, guest_addr: u64, len: u64, _host: NonNull<u8>) -> Result<u32, BoxError> {
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
        read_only: false,
        cache_type: CacheType::Writeback,
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

/// A writer's device opens its store, says its version, tag, queues, branch and capacity in its
/// configuration, takes its guest's freezing, refuses a snapshot unfrozen, and, frozen, forks its
/// store to a version, which a device restored from its state opens a store of its own from, as
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
    assert_ne!(u64::from_le(original.branch), 0);

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
    assert_eq!(state.version, device.snapshot().unwrap().version);
    assert_eq!(state.version.cell, u64::from_le(original.branch));

    let mut restored_state = state.clone();
    restored_state.config.path_on_host = dir.0.join("restored").display().to_string();
    let restored = Device::restore(&restored_state, 2, vm, lost).unwrap();
    assert!(restored.snapshot().is_some());
    let mut space = [0u8; size_of::<ConfigSpace>()];
    restored.read_config(0, &mut space);
    let (restored_branch, original_branch) =
        (ConfigSpace::read_from_bytes(&space).unwrap().branch, original.branch);
    assert_ne!(restored_branch, original_branch);
    assert!(restored.state().frozen);
}

/// A device with a window reserves it of its VM, its private pages at most in its configuration,
/// creates it as the driver activates the device, and, restored from a snapshot, reserves it where
/// the snapshot had it; dropped, it removes its slot before releasing its addresses.
#[test]
fn windows_are_where_snapshots_had_them() {
    let dir = daemon();
    let lost: virtio_extent::OnLost = Arc::new(|| panic!("the daemon went"));
    let vm = Arc::new(FakeVm::default());
    let config = Config { dax_window_size: Some(4 << 20), ..config(&dir, "store") };
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
