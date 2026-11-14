// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax: stores of sealed cells, opened as a host opens them (TASK-411.3, doc-16): their cell's
//! KEK and their DEKs kept wrapped, unwrapped once a taking of the cell; their objects sealed, and
//! refused in the clear, or copied to another's key; a host's fetchers reading them under the keys
//! of the stores it serves.

use super::*;
use crate::{Description, MemDir, RemoteData};
use extent_frame::{KeyId, Secret};
use extent_keys::kms::{Kms as _, LocalKms};
use extent_keys::testing::Counted;
use extent_remote::Bucket as _;
use extent_remote::bucket::PutMode;
use extent_remote::fetch::{Fetcher, Limits, Memory, Priority};
use extent_remote::layout::{self, Branch, Location, TEST_CELL, test_branch};
use futures::TryStreamExt as _;
use object_store::ObjectStore as _;
use storage_device::ExtentId;

/// The tenant of [`TEST_CELL`], whose KEK the tests' KMS keeps.
const TENANT: u64 = TEST_CELL.tenant;

/// What the tests' hosts share: a bucket, its objects as object storage holds them, and a KMS,
/// counting its calls, holding [`TENANT`]'s KEK.
struct World {
    runtime: tokio::runtime::Runtime,
    objects: Arc<object_store::memory::InMemory>,
    bucket: Arc<Bucket>,
    kms: Arc<Counted<LocalKms>>,
    _kms: tempfile::TempDir,
}

/// A host: its runtime, keys and all, and what its fetchers keep, whose keys are its runtime's.
struct Host {
    runtime: Runtime,
    memory: Arc<Memory>,
    token: u64,
}

impl World {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let objects = Arc::new(object_store::memory::InMemory::new());
        let bucket = Arc::new(Bucket::new(objects.clone(), ""));
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(TENANT).unwrap();
        Self { runtime, objects, bucket, kms: Arc::new(Counted::new(local)), _kms: dir }
    }

    /// A host of placement `token`, its KMS this world's, unwrapping anew what it asks.
    fn host(&self, token: u64) -> Host {
        let memory = Memory::new();
        let kms: extent_keys::kms::Shared = self.kms.clone();
        let keys = keys::Host { kms: Some(keys::Kms::new(kms)), ring: memory.keys().clone() };
        let uploads = Budget::new(Limits::UNBOUNDED);
        Host {
            runtime: Runtime { handle: self.runtime.handle().clone(), uploads, keys },
            memory,
            token,
        }
    }

    /// The KMS's wraps and unwraps since it was made or reset.
    fn calls(&self) -> (u64, u64) {
        (self.kms.wraps(), self.kms.unwraps())
    }

    /// Every object's key and bytes.
    fn objects(&self) -> Vec<(String, Vec<u8>)> {
        self.runtime.block_on(async {
            let listed: Vec<_> = self.objects.list(None).try_collect().await.unwrap();
            let mut objects = Vec::new();
            for meta in listed {
                let key = meta.location.to_string();
                let object = extent_remote::bucket::get(&*self.bucket, &key).await.unwrap();
                objects.push((key, object.bytes.to_vec()));
            }
            objects.sort();
            objects
        })
    }

    /// The bytes of the object at `key`.
    fn object(&self, key: &str) -> Vec<u8> {
        let got = self.runtime.block_on(extent_remote::bucket::get(&*self.bucket, key));
        got.unwrap_or_else(|| panic!("no object at {key}")).bytes.to_vec()
    }

    /// Puts `bytes` at `key`, in place of what is there: one who can write the bucket.
    fn put(&self, key: &str, bytes: Vec<u8>) {
        let put = self.bucket.put(key, bytes.into(), PutMode::Overwrite);
        self.runtime.block_on(put).unwrap();
    }

    /// `branch`'s record.
    fn record(&self, branch: Branch) -> extent_remote::record::Record {
        self.runtime.block_on(branch::record(&*self.bucket, branch)).unwrap().unwrap()
    }

    /// The cell's object.
    fn cell(&self) -> cell::Object {
        self.runtime.block_on(cell::read(&*self.bucket, TEST_CELL)).unwrap().unwrap()
    }
}

/// A store's directory, made, of `branch`, the volume `name` of its cell.
fn made(branch: Branch, name: &str) -> MemDir {
    let dir = MemDir::new();
    let description = Description { block_size: 4096, capacity: 1 << 30, branch };
    let volume = crate::Volume { name: name.into(), kind: cell::Kind::Fxfs };
    Store::make(&dir, description, Some("memory:///encrypted"), Some(volume)).unwrap();
    dir
}

/// The store in `dir`, opened by `host`, made whole if it was unmade; checkpointed each segment.
fn open_on(
    world: &World,
    host: &Host,
    dir: &MemDir,
) -> Result<(Arc<Store<MemDir>>, Remote<MemDir>), Failure> {
    let recovering = match Store::recover(dir.clone()) {
        Ok(recovering) => recovering,
        Err(err) if err.is::<crate::Unmade>() => Store::recover_unmade(dir.clone())?,
        Err(err) => return Err(err.into()),
    };
    let options = Options { token: host.token, checkpoint_every: 1, ..Options::default() };
    let holding = (rand::random(), options);
    let opened =
        open(recovering, crate::Options::default(), world.bucket.clone(), holding, &host.runtime)?;
    opened.0.made().unwrap();
    Ok(opened)
}

/// Data that compresses not: from a xorshift generator seeded with `seed`.
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

/// Writes `data` to `extent`, commits and waits for its upload.
fn written(store: &Store<MemDir>, remote: &Remote<MemDir>, extent: ExtentId, data: &[u8]) {
    store.write(extent, 0, data).unwrap();
    store.commit(None).unwrap();
    remote.wait_uploaded(store.epoch().unwrap()).unwrap();
}

/// The data of `extent`, `len` bytes, as `store` reads it: through `host`'s fetchers, if its data
/// is in the bucket.
fn read(
    world: &World,
    host: &Host,
    store: &Store<MemDir>,
    extent: ExtentId,
    len: usize,
) -> Result<Vec<u8>, extent_remote::Error> {
    let mut buf = vec![0; len];
    let location = match store.read(extent, 0, &mut buf) {
        Ok(()) => return Ok(buf),
        Err(err) => match err.downcast_ref::<RemoteData>() {
            Some(remote) => remote.location.clone(),
            None => panic!("{err:#}"),
        },
    };
    fetched(world, &host.memory, extent, &location)
}

/// `extent`'s data at `location`, read by a fetcher keeping what it reads in `memory`.
fn fetched(
    world: &World,
    memory: &Arc<Memory>,
    extent: ExtentId,
    location: &Location,
) -> Result<Vec<u8>, extent_remote::Error> {
    let budget = Budget::new(Limits::UNBOUNDED);
    let fetcher = Fetcher::new(world.bucket.clone(), budget, memory.clone(), None);
    world.runtime.block_on(fetcher.read(extent, location, 0, location.len, Priority::Demand))
}

/// Where `extent`'s data is in the bucket, as `store` names it.
fn location(store: &Store<MemDir>, extent: ExtentId) -> Location {
    let mut buf = vec![0; 4096];
    let err = store.read(extent, 0, &mut buf).unwrap_err();
    err.downcast_ref::<RemoteData>().unwrap_or_else(|| panic!("{err:#}")).location.clone()
}

/// Whether `bytes` hold `piece` anywhere.
fn holds(bytes: &[u8], piece: &[u8]) -> bool {
    bytes.windows(piece.len()).any(|window| window == piece)
}

/// AC #1, #2: three volumes of a cell opened by a host, the first making it: the cell's KEK
/// wrapped by its tenant's in the KMS, its branches' DEKs by it, each kept only so, no plaintext
/// of either nor of their data in any object; their segments sealed under their own branch's DEK,
/// their checkpoints authenticated. A host making the cell unwraps nothing; one taking it after
/// unwraps its KEK once for the three, and reads what the first wrote.
#[test]
fn cells_keep_keys_wrapped_and_one_unwrap_takes_a_cell() {
    let world = World::new();
    let first = world.host(5);
    let volumes = [(test_branch(7), "a"), (test_branch(8), "b"), (test_branch(9), "c")];
    let dirs: Vec<MemDir> = volumes.iter().map(|&(branch, name)| made(branch, name)).collect();
    let mut deks = Vec::new();
    let mut data = Vec::new();
    for (at, dir) in dirs.iter().enumerate() {
        let (store, remote) = open_on(&world, &first, dir).unwrap();
        let branch = volumes[at].0;
        let written_data = noise(at as u64 + 1, 4 * 4096);
        written(&store, &remote, ExtentId::new(branch.id, 1), &written_data);
        let (dek, id) = store.keys().sealing().unwrap();
        assert_eq!(id, KeyId { branch: branch.id, generation: 1 });
        deks.push(Secret::from_bytes(dek.expose()));
        data.push(written_data);
    }
    // One wrap for the cell made, one more for the second store's opening, its pool empty, whose
    // KEK the third took and gave back; none unwrapped: the cell was made here.
    assert_eq!(world.calls(), (2, 0));

    let object = world.cell();
    let [kek] = &object.keks[..] else { panic!("{:?}", object.keks) };
    assert_eq!(kek.generation, 1);
    let unwrapped = futures::executor::block_on(world.kms.inner().unwrap(TENANT, &kek.wrapped));
    let kek_bytes = *unwrapped.unwrap().expose();
    let objects = world.objects();
    for ((branch, _), dek) in volumes.iter().zip(&deks) {
        let record = world.record(*branch);
        let [wrapped] = &record.deks[..] else { panic!("{:?}", record.deks) };
        assert_eq!((wrapped.generation, wrapped.kek), (1, 1));
        let unwrapped = keys::unwrap_dek(&Secret::from_bytes(&kek_bytes), *branch, wrapped);
        assert_eq!(unwrapped.unwrap().expose(), dek.expose());
        for (key, bytes) in &objects {
            assert!(!holds(bytes, dek.expose()), "{key} holds {branch}'s DEK");
        }
    }
    for ((key, bytes), (at, (branch, _))) in
        objects.iter().flat_map(|object| volumes.iter().enumerate().map(move |v| (object, v)))
    {
        assert!(!holds(bytes, &kek_bytes), "{key} holds the cell's KEK");
        assert!(!holds(bytes, &data[at][..64]), "{key} holds {branch}'s data");
    }
    let mut kinds = std::collections::BTreeMap::new();
    for (key, bytes) in &objects {
        let seal = || extent_frame::Frame::of(bytes).unwrap().seal;
        let kind = layout::kind(key);
        match kind {
            layout::Kind::Segment { branch, .. } => {
                let sealed = seal();
                let extent_frame::Seal::Sealed { key: id, .. } = sealed else {
                    panic!("{key}: {sealed:?}")
                };
                assert_eq!(id.branch, branch.id, "{key}");
            }
            layout::Kind::Checkpoint { branch, .. } => {
                let sealed = seal();
                let extent_frame::Seal::Authenticated { key: id, .. } = sealed else {
                    panic!("{key}: {sealed:?}")
                };
                assert_eq!(id.branch, branch.id, "{key}");
            }
            _ => {}
        }
        *kinds.entry(kind.name()).or_insert(0) += 1;
    }
    assert!(kinds["segment"] >= 3 && kinds["checkpoint"] >= 3, "{kinds:?}");

    // Another placement takes the cell: one unwrap for the three, none wrapped.
    world.kms.reset();
    let second = world.host(6);
    let mut opened = Vec::new();
    for dir in &dirs {
        opened.push(open_on(&world, &second, &dir.crash(0)).unwrap());
    }
    assert_eq!(world.calls(), (0, 1));
    assert_eq!(world.cell().epoch, 2);
    for (at, (store, _)) in opened.iter().enumerate() {
        let extent = ExtentId::new(volumes[at].0.id, 1);
        assert_eq!(read(&world, &second, store, extent, 4 * 4096), Ok(data[at].clone()));
    }
    assert_eq!(world.calls(), (0, 1));
}

/// AC #1's twin: a cell of a tenant with no KEK in the host's KMS is made in the clear, its
/// records with no DEK, its objects in the clear, read as before.
#[test]
fn cells_of_tenants_with_no_kek_are_in_the_clear() {
    let world = World::new();
    let host = world.host(5);
    let branch = Branch { cell: extent_remote::layout::Cell { tenant: 2, id: 1 }, id: 7 };
    let dir = made(branch, "a");
    let (store, remote) = open_on(&world, &host, &dir).unwrap();
    let data = noise(1, 4096);
    written(&store, &remote, ExtentId::new(branch.id, 1), &data);
    assert!(store.keys().is_empty());
    let object = world.runtime.block_on(cell::read(&*world.bucket, branch.cell)).unwrap().unwrap();
    assert_eq!(object.keks, Vec::new());
    assert_eq!(world.record(branch).deks, Vec::new());
    let segment = world.object(&layout::segment(branch, 1));
    assert_eq!(extent_frame::Frame::of(&segment).unwrap().seal, extent_frame::Seal::Clear);
    assert_eq!(read(&world, &host, &store, ExtentId::new(branch.id, 1), 4096), Ok(data));
    // The tenant's KEK asked for once, which it has not.
    assert_eq!(world.calls(), (1, 0));
}

/// AC #3: a store whose tenant's KEK is revoked opens on no host taking its cell, the error naming
/// the tenant; its KEK given back, it opens.
#[test]
fn stores_of_revoked_tenants_open_not() {
    let world = World::new();
    let dir = made(test_branch(7), "a");
    let (store, remote) = open_on(&world, &world.host(5), &dir).unwrap();
    written(&store, &remote, ExtentId::new(7, 1), &noise(1, 4096));
    drop((store, remote));
    world.kms.inner().revoke(TENANT).unwrap();
    let refused = open_on(&world, &world.host(6), &dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused.to_string(), "tenant 1: its KEK is revoked");
    assert_eq!(refused, Failure::Remote(extent_remote::Error::Kms(refused.to_string())));
    world.kms.inner().unrevoke(TENANT).unwrap();
    let host = world.host(6);
    let (store, _remote) = open_on(&world, &host, &dir.crash(0)).unwrap();
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
}

/// AC #4: a generation of a branch's DEK added, the store's next opening seals its new objects
/// under it, what was sealed under the one before read as before. The cell's KEK rotated, its
/// records' DEKs are rewrapped under the new one, the same DEKs, by one conditional update each,
/// and its stores open and read as before.
#[test]
fn generations_seal_new_objects_and_rotations_rewrap() {
    let world = World::new();
    let host = world.host(5);
    let branches = [test_branch(7), test_branch(8)];
    let dirs = [made(branches[0], "a"), made(branches[1], "b")];
    let (store, remote) = open_on(&world, &host, &dirs[0]).unwrap();
    written(&store, &remote, ExtentId::new(7, 1), &noise(1, 4096));
    drop(open_on(&world, &host, &dirs[1]).unwrap());
    let kms = host.runtime.keys.kms.clone();
    let cell_keys = CellKeys::of(TEST_CELL, &world.cell(), kms.clone());
    let added =
        world.runtime.block_on(keys::add_generation(&*world.bucket, &cell_keys, branches[0]));
    assert_eq!(added, Ok(2));
    assert_eq!(store.keys().generations(7), [1]);
    drop((store, remote));
    let (store, remote) = open_on(&world, &host, &dirs[0].crash(0)).unwrap();
    assert_eq!(store.keys().generations(7), [1, 2]);
    assert_eq!(store.keys().sealing().unwrap().1, KeyId { branch: 7, generation: 2 });
    written(&store, &remote, ExtentId::new(7, 2), &noise(2, 4096));
    let first = location(&store, ExtentId::new(7, 2)).key;
    let sealed = extent_frame::Frame::of(&world.object(&first)).unwrap().seal;
    assert!(
        matches!(sealed, extent_frame::Seal::Sealed { key, .. } if key.generation == 2),
        "{first}: {sealed:?}"
    );
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 2), 4096), Ok(noise(2, 4096)));
    let deks_before: Vec<_> = branches.iter().map(|&branch| world.record(branch).deks).collect();
    drop((store, remote));

    // The cell's KEK rotated by its holder.
    let held = cell::Held { cell: TEST_CELL, epoch: world.cell().epoch, holder: 5 };
    let kms = kms.unwrap();
    let rotated = world.runtime.block_on(keys::rotate(&*world.bucket, &kms, held, world.cell()));
    assert_eq!(rotated, Ok((2, 2)));
    assert_eq!(world.cell().keks.iter().map(|kek| kek.generation).collect::<Vec<_>>(), [1, 2]);
    let new = futures::executor::block_on(
        world.kms.inner().unwrap(TENANT, &world.cell().keks[1].wrapped),
    );
    let (old, new) = (
        futures::executor::block_on(
            world.kms.inner().unwrap(TENANT, &world.cell().keks[0].wrapped),
        )
        .unwrap(),
        new.unwrap(),
    );
    for (branch, before) in branches.iter().zip(deks_before) {
        let after = world.record(*branch).deks;
        assert_eq!(after.len(), before.len(), "{branch}");
        for (before, after) in before.iter().zip(&after) {
            assert_eq!((after.generation, after.kek), (before.generation, 2), "{branch}");
            assert_ne!(after.wrapped, before.wrapped, "{branch}");
            let was = keys::unwrap_dek(&old, *branch, before).unwrap();
            let is = keys::unwrap_dek(&new, *branch, after).unwrap();
            assert_eq!(was.expose(), is.expose(), "{branch}");
        }
    }
    // Another placement takes the cell after the rotation, unwrapping the new KEK: both
    // generations read.
    let other = world.host(6);
    let (store, _remote) = open_on(&world, &other, &dirs[0].crash(0)).unwrap();
    assert_eq!(read(&world, &other, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
    assert_eq!(read(&world, &other, &store, ExtentId::new(7, 2), 4096), Ok(noise(2, 4096)));
}

/// AC #5: an object of a sealed branch put in the clear at its key, its data and footer framed
/// anew, CRCs and all, by one who can write the bucket: refused by the branch's readers, the error
/// naming it, whether its tail was kept before or not.
#[test]
fn objects_of_sealed_branches_in_the_clear_are_refused() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, remote) = open_on(&world, &host, &dir).unwrap();
    let extent = ExtentId::new(7, 1);
    written(&store, &remote, extent, &noise(1, 4096));
    let at = location(&store, extent);
    let bytes = world.object(&at.key);
    let keys = store.keys().of(7);
    let (data, footer) = (
        extent_frame::data(&bytes, &keys).unwrap(),
        extent_frame::footer(&bytes, &keys).unwrap().to_vec(),
    );
    world.put(&at.key, extent_frame::framed(&data, &footer, None));
    let downgraded = extent_remote::Error::Downgrade(at.key.clone());
    assert_eq!(
        downgraded.to_string(),
        format!("{}: in the clear, of a branch whose objects are sealed", at.key)
    );
    // A host's fetchers, holding the store's keys, of no tail kept.
    assert_eq!(read(&world, &host, &store, extent, 4096), Err(downgraded.clone()));
    // And a reader holding none, as the collector reads, reads it.
    assert_eq!(fetched(&world, &Memory::new(), extent, &at), Ok(noise(1, 4096)));
    // Its newest checkpoint so, authenticated no more: its store's recovery refuses it, naming it.
    drop((store, remote));
    let objects = world.objects();
    let checkpoints = objects.iter().filter(|(key, _)| {
        matches!(layout::kind(key), layout::Kind::Checkpoint { branch, .. } if branch.id == 7)
    });
    let (key, bytes) = checkpoints.last().unwrap();
    let keyless = &extent_frame::NoKeys;
    let (data, footer) = (
        extent_frame::data(bytes, keyless).unwrap(),
        extent_frame::footer(bytes, keyless).unwrap().to_vec(),
    );
    world.put(key, extent_frame::framed(&data, &footer, None));
    let refused = open_on(&world, &world.host(6), &dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused, Failure::Remote(extent_remote::Error::Downgrade(key.clone())));
}

/// AC #6: the object of an extent stored on its own, copied whole to another extent's key of the
/// same branch, is refused there: its footer, which its MAC covers, names the extent it is of. The
/// moves of an object, copied to another's moves, likewise.
#[test]
fn own_extents_copied_to_another_key_are_refused() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, remote) = open_on(&world, &host, &dir).unwrap();
    let large = crate::Options::default().large_extent as usize;
    let (one, other) = (ExtentId::new(7, 1), ExtentId::new(7, 2));
    store.write(one, 0, &noise(1, large)).unwrap();
    store.write(other, 0, &noise(2, large)).unwrap();
    store.commit(None).unwrap();
    remote.wait_uploaded(store.epoch().unwrap()).unwrap();
    let (at_one, at_other) = (location(&store, one), location(&store, other));
    assert!(matches!(layout::kind(&at_one.key), layout::Kind::Extent { .. }), "{}", at_one.key);
    assert_eq!(read(&world, &host, &store, other, large), Ok(noise(2, large)));
    world.put(&at_other.key, world.object(&at_one.key));
    let refused = extent_remote::Error::Corrupt(format!("{} holds extent {one:?}", at_other.key));
    assert_eq!(fetched(&world, &Memory::new(), other, &at_other), Err(refused.clone()));
    let host = world.host(5);
    let _held = host.memory.keys().hold(store.keys().clone());
    assert_eq!(fetched(&world, &host.memory, other, &at_other), Err(refused));
}

/// AC #7: a host's fetchers open what they keep under the keys of the stores the host serves, not
/// a store's own: a sealed part read before the host holds its key is unread, and read once it
/// holds it, by the tail and part kept; and served to a peer as stored, holding none.
#[test]
fn hosts_fetchers_open_parts_under_the_keys_of_the_stores_they_serve() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, remote) = open_on(&world, &host, &dir).unwrap();
    let extent = ExtentId::new(7, 1);
    written(&store, &remote, extent, &noise(1, 4096));
    let at = location(&store, extent);
    // Another host's memory, of no store yet, which keeps what it reads in a cache.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(extent_remote::cache::Cache::open(cache_dir.path(), 1 << 30, 0).unwrap());
    let memory = Memory::new();
    let budget = Budget::new(Limits::UNBOUNDED);
    let fetcher = Fetcher::new(world.bucket.clone(), budget, memory.clone(), Some(cache));
    let reading = || world.runtime.block_on(fetcher.read(extent, &at, 0, 4096, Priority::Demand));
    let key = KeyId { branch: 7, generation: 1 };
    assert_eq!(reading(), Err(extent_remote::Error::NoKey(key)));
    // Served to a peer as stored, sealed.
    let tail = world.runtime.block_on(extent_remote::object::tail(&*world.bucket, &at.key));
    let stored = tail.unwrap().unwrap().parts.stored[0];
    let served = world.runtime.block_on(fetcher.stored_part(&at.key, 0, &stored)).unwrap();
    assert_eq!(
        served.as_deref(),
        Some(&world.object(&at.key)[stored.range().start as usize..][..stored.len as usize])
    );
    // The store's keys held among the host's: read, by what it kept, with no GET more.
    let held = memory.keys().hold(store.keys().clone());
    world.runtime.block_on(world.bucket.delete(&at.key)).unwrap();
    assert_eq!(reading(), Ok(noise(1, 4096)));
    drop(held);
}

/// Forks a store's branch, as of its last commit, to `child`: the fork's commit made, then the
/// fork in the bucket, its child's record and checkpoint.
fn forked(store: &Store<MemDir>, remote: &Remote<MemDir>, child: Child) {
    let epoch = match remote.is_point() {
        true => 0,
        false => store.commit_fork(child).unwrap(),
    };
    remote.forked(epoch, child).unwrap();
}

/// TASK-411.3: a fork within a sealed cell, and a point there, read what they inherit under their
/// parent's DEK, which their rings hold, their parent's record's, unwrapped under the cell's KEK; a
/// fork's child gets a DEK of its own from its first writer, its records' writers before naming
/// none, and seals its own objects under it. A fork into another cell of a sealed branch opens
/// not, its error naming both (TASK-411.4 rewraps them).
#[test]
fn forks_and_points_within_a_sealed_cell_read_what_they_inherit() {
    let world = World::new();
    let host = world.host(5);
    let parent = test_branch(7);
    let dir = made(parent, "parent");
    let (store, remote) = open_on(&world, &host, &dir).unwrap();
    let inherited = ExtentId::new(7, 1);
    written(&store, &remote, inherited, &noise(1, 4096));

    // A fork's child, of the parent's cell.
    let child = Child { branch: test_branch(8), owner: 0x81, point: false };
    forked(&store, &remote, child);
    assert_eq!(world.record(child.branch).deks, Vec::new());
    let (forked_store, forked_remote) =
        open_on(&world, &host, &made(child.branch, "child")).unwrap();
    assert_eq!(world.record(child.branch).deks.len(), 1);
    let keys = forked_store.keys();
    assert_eq!((keys.generations(7), keys.generations(8)), (vec![1], vec![1]));
    assert_eq!(keys.sealing().unwrap().1, KeyId { branch: 8, generation: 1 });
    assert_eq!(read(&world, &host, &forked_store, inherited, 4096), Ok(noise(1, 4096)));
    let own = ExtentId::new(8, forked_remote.next_sequence().max(1));
    written(&forked_store, &forked_remote, own, &noise(2, 4096));
    let at = location(&forked_store, own).key;
    let sealed = extent_frame::Frame::of(&world.object(&at)).unwrap().seal;
    assert!(
        matches!(sealed, extent_frame::Seal::Sealed { key, .. } if key.branch == 8),
        "{sealed:?}"
    );
    assert_eq!(read(&world, &host, &forked_store, own, 4096), Ok(noise(2, 4096)));

    // A point, of the parent's cell: no DEK of its own, nothing writing it.
    let point = Child { branch: test_branch(9), owner: 0x91, point: true };
    forked(&store, &remote, point);
    let (point_store, _point_remote) =
        open_on(&world, &host, &made(point.branch, "point")).unwrap();
    assert_eq!(world.record(point.branch).deks, Vec::new());
    assert_eq!(point_store.keys().generations(7), [1]);
    assert_eq!(point_store.keys().sealing().map(|(_, id)| id), None);
    assert_eq!(read(&world, &host, &point_store, inherited, 4096), Ok(noise(1, 4096)));

    // A fork into another cell of the tenant's.
    let elsewhere = Branch { cell: extent_remote::layout::Cell { tenant: TENANT, id: 2 }, id: 10 };
    forked(&store, &remote, Child { branch: elsewhere, owner: 0xa1, point: false });
    let refused = open_on(&world, &host, &made(elsewhere, "elsewhere")).map(drop).unwrap_err();
    let inherits = extent_remote::Error::Inherited { branch: elsewhere, from: parent };
    assert_eq!(
        refused.to_string(),
        "branch 1/2/a inherits from branch 1/1/7, of another cell, whose objects are sealed: forks \
         across cells of sealed branches are not made yet (TASK-411.4)"
    );
    assert_eq!(refused, Failure::Remote(inherits));
}
