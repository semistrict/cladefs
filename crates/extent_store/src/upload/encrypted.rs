// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: stores of encrypted cells, opened as a host opens them (TASK-411.3, doc-16): their
//! cell's KEK and their DEKs kept wrapped, unwrapped once a taking of the cell; their objects
//! encrypted, and refused in the clear, or copied to another's key; a host's fetchers reading them
//! under the keys of the stores it serves.

use super::*;
use crate::dir::read_file;
use crate::layout::SegmentId;
use crate::{BucketData, Description, MemDir};
use extent_bucket::Bucket as _;
use extent_bucket::bucket::PutMode;
use extent_bucket::fetch::{Fetcher, Limits, Memory, Priority};
use extent_bucket::layout::{self, Branch, Cell, Location, TEST_CELL, test_branch};
use extent_frame::{KeyId, Secret};
use extent_keys::kms::{Kms as _, LocalKms};
use extent_keys::testing::Counted;
use futures::TryStreamExt as _;
use object_store::ObjectStore as _;
use std::collections::BTreeSet;
use storage_device::ExtentId;

/// The tenant of [`TEST_CELL`], whose KEK the tests' KMS keeps.
const TENANT: u64 = TEST_CELL.tenant;

/// Another tenant, whose KEK tests that fork across tenants have the KMS keep (TASK-411.4).
const OTHER: u64 = 2;

/// A child of a fork, `id`, the one volume of cell `cell` of `tenant`'s.
fn child_in(tenant: u64, cell: u64, id: u64, point: bool) -> Child {
    Child { branch: Branch { cell: Cell { tenant, id: cell }, id }, owner: 0x100 + id, point }
}

/// What the tests' hosts share: a bucket, its objects as object storage holds them, and a KMS,
/// counting its calls, holding [`TENANT`]'s KEK.
struct World {
    runtime: tokio::runtime::Runtime,
    objects: Arc<object_store::memory::InMemory>,
    bucket: Arc<ObjectBucket>,
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
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(TENANT).unwrap();
        Self { runtime, objects, bucket, kms: Arc::new(Counted::new(local)), _kms: dir }
    }

    /// A host of placement `token`, its KMS this world's, unwrapping anew what it asks.
    fn host(&self, token: u64) -> Host {
        let memory = Memory::new();
        let kms: extent_keys::kms::Shared = self.kms.clone();
        let keys =
            keys::Host { kms: Some(keys::CachingKms::new(kms)), ring: memory.keys().clone() };
        let uploads = Budget::new(Limits::UNBOUNDED);
        Host {
            runtime: Runtime { handle: self.runtime.handle().clone(), uploads, keys },
            memory,
            token,
        }
    }

    /// A host of placement `token` as [`World::host`] makes one, its pools of new cell KEKs refilled
    /// in batches of 4 once a take leaves fewer than 2, on this world's runtime (TASK-411.4).
    fn pooled_host(&self, token: u64) -> Host {
        let memory = Memory::new();
        let kms: extent_keys::kms::Shared = self.kms.clone();
        let pooling = keys::Pooling { low: 2, batch: 4, handle: self.runtime.handle().clone() };
        let keys = keys::Host {
            kms: Some(keys::CachingKms::pooled(kms, pooling)),
            ring: memory.keys().clone(),
        };
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
                let object = extent_bucket::bucket::get(&*self.bucket, &key).await.unwrap();
                objects.push((key, object.bytes.to_vec()));
            }
            objects.sort();
            objects
        })
    }

    /// The bytes of the object at `key`.
    fn object(&self, key: &str) -> Vec<u8> {
        let got = self.runtime.block_on(extent_bucket::bucket::get(&*self.bucket, key));
        got.unwrap_or_else(|| panic!("no object at {key}")).bytes.to_vec()
    }

    /// Puts `bytes` at `key`, in place of what is there: one who can write the bucket.
    fn put(&self, key: &str, bytes: Vec<u8>) {
        let put = self.bucket.put(key, bytes.into(), PutMode::Overwrite);
        self.runtime.block_on(put).unwrap();
    }

    /// `branch`'s record.
    fn record(&self, branch: Branch) -> extent_bucket::record::Record {
        self.runtime.block_on(branch::record(&*self.bucket, branch)).unwrap().unwrap()
    }

    /// The cell's object.
    fn cell(&self) -> cell::Object {
        self.cell_of(TEST_CELL).unwrap()
    }

    /// `cell`'s object, if it has one.
    fn cell_of(&self, cell: Cell) -> Option<cell::Object> {
        self.runtime.block_on(cell::read(&*self.bucket, cell)).unwrap()
    }

    /// Whether `branch` has a record.
    fn has_record(&self, branch: Branch) -> bool {
        self.runtime.block_on(branch::record(&*self.bucket, branch)).unwrap().is_some()
    }

    /// `wrapped`, unwrapped by `tenant`'s KEK, asked of the KMS uncounted.
    fn unwrapped(&self, tenant: u64, wrapped: &extent_keys::Wrapped) -> Secret {
        futures::executor::block_on(self.kms.inner().unwrap(tenant, wrapped)).unwrap()
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
) -> Result<(Arc<Store<MemDir>>, Uploader<MemDir>), Failure> {
    open_with(world, host, dir, Options { checkpoint_every: 1, ..Options::default() })
}

/// The store in `dir`, opened by `host` as [`open_on`] opens it, its uploader's options `options`
/// but for its placement's token, `host`'s.
fn open_with(
    world: &World,
    host: &Host,
    dir: &MemDir,
    options: Options,
) -> Result<(Arc<Store<MemDir>>, Uploader<MemDir>), Failure> {
    let recovering = match Store::recover(dir.clone()) {
        Ok(recovering) => recovering,
        Err(err) if err.is::<crate::Unmade>() => Store::recover_unmade(dir.clone())?,
        Err(err) => return Err(err.into()),
    };
    let options = Options { token: host.token, ..options };
    let holding = (rand::random(), options);
    let opened =
        open(recovering, crate::Options::default(), world.bucket.clone(), holding, &host.runtime)?;
    opened.0.made()?;
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
fn written(store: &Store<MemDir>, uploader: &Uploader<MemDir>, extent: ExtentId, data: &[u8]) {
    store.write(extent, 0, data).unwrap();
    store.commit(None).unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
}

/// The data of `extent`, `len` bytes, as `store` reads it: through `host`'s fetchers, if its data
/// is in the bucket.
fn read(
    world: &World,
    host: &Host,
    store: &Store<MemDir>,
    extent: ExtentId,
    len: usize,
) -> Result<Vec<u8>, extent_bucket::Error> {
    let mut buf = vec![0; len];
    let location = match store.read(extent, 0, &mut buf) {
        Ok(()) => return Ok(buf),
        Err(err) => match err.downcast_ref::<BucketData>() {
            Some(uploader) => uploader.location.clone(),
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
) -> Result<Vec<u8>, extent_bucket::Error> {
    let budget = Budget::new(Limits::UNBOUNDED);
    let fetcher = Fetcher::new(world.bucket.clone(), budget, memory.clone(), None);
    world.runtime.block_on(fetcher.read(extent, location, 0, location.len, Priority::Demand))
}

/// Where `extent`'s data is in the bucket, as `store` names it.
fn location(store: &Store<MemDir>, extent: ExtentId) -> Location {
    let mut buf = vec![0; 4096];
    let err = store.read(extent, 0, &mut buf).unwrap_err();
    err.downcast_ref::<BucketData>().unwrap_or_else(|| panic!("{err:#}")).location.clone()
}

/// Whether `bytes` hold `piece` anywhere.
fn holds(bytes: &[u8], piece: &[u8]) -> bool {
    bytes.windows(piece.len()).any(|window| window == piece)
}

/// AC #1, #2: three volumes of a cell opened by a host, the first making it: the cell's KEK wrapped
/// by its tenant's in the KMS, its branches' DEKs by it, each kept only so, no plaintext of either
/// nor of their data in any object; their segments encrypted under their own branch's DEK, their
/// checkpoints authenticated. A host making the cell unwraps nothing; one taking it after unwraps
/// its KEK once for the three, and reads what the first wrote.
#[test]
fn cells_keep_keys_wrapped_and_one_unwrap_takes_a_cell() {
    let world = World::new();
    let first = world.host(5);
    let volumes = [(test_branch(7), "a"), (test_branch(8), "b"), (test_branch(9), "c")];
    let dirs: Vec<MemDir> = volumes.iter().map(|&(branch, name)| made(branch, name)).collect();
    let mut deks = Vec::new();
    let mut data = Vec::new();
    for (at, dir) in dirs.iter().enumerate() {
        let (store, uploader) = open_on(&world, &first, dir).unwrap();
        let branch = volumes[at].0;
        let written_data = noise(at as u64 + 1, 4 * 4096);
        written(&store, &uploader, ExtentId::new(branch.id, 1), &written_data);
        let (dek, id) = store.keys().encrypting().unwrap();
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
        let encryption = || extent_frame::Frame::of(bytes).unwrap().encryption;
        let kind = layout::kind(key);
        match kind {
            layout::Kind::Segment { branch, .. } => {
                let encrypted = encryption();
                let extent_frame::Encryption::Encrypted { key: id, .. } = encrypted else {
                    panic!("{key}: {encrypted:?}")
                };
                assert_eq!(id.branch, branch.id, "{key}");
            }
            layout::Kind::Checkpoint { branch, .. } => {
                let encrypted = encryption();
                let extent_frame::Encryption::Authenticated { key: id, .. } = encrypted else {
                    panic!("{key}: {encrypted:?}")
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
    let branch = Branch { cell: extent_bucket::layout::Cell { tenant: 2, id: 1 }, id: 7 };
    let dir = made(branch, "a");
    let (store, uploader) = open_on(&world, &host, &dir).unwrap();
    let data = noise(1, 4096);
    written(&store, &uploader, ExtentId::new(branch.id, 1), &data);
    assert!(store.keys().is_empty());
    let object = world.runtime.block_on(cell::read(&*world.bucket, branch.cell)).unwrap().unwrap();
    assert_eq!(object.keks, Vec::new());
    assert_eq!(world.record(branch).deks, Vec::new());
    let segment = world.object(&layout::segment(branch, 0, 1));
    assert_eq!(
        extent_frame::Frame::of(&segment).unwrap().encryption,
        extent_frame::Encryption::None
    );
    assert_eq!(read(&world, &host, &store, ExtentId::new(branch.id, 1), 4096), Ok(data));
    // The tenant's KEK asked for once, which it has not.
    assert_eq!(world.calls(), (1, 0));
    // TASK-411.10: its other stores opened by the host, a volume of the cell, one of a new cell,
    // and the first again, ask nothing more: the answer is believed for `keys::KEKLESS_FOR`.
    drop((store, uploader));
    let other = Branch { id: 8, ..branch };
    let elsewhere = Branch { cell: extent_bucket::layout::Cell { tenant: 2, id: 2 }, id: 9 };
    let dirs = [made(other, "b"), made(elsewhere, "a"), dir.crash(0)];
    for dir in &dirs {
        let (store, _uploader) = open_on(&world, &host, dir).unwrap();
        assert!(store.keys().is_empty());
    }
    assert_eq!(world.calls(), (1, 0));
    // Another host asks once itself.
    open_on(&world, &world.host(6), &made(Branch { id: 10, ..elsewhere }, "b")).unwrap();
    assert_eq!(world.calls(), (2, 0));
}

/// AC #3: a store whose tenant's KEK is revoked opens on no host taking its cell, the error naming
/// the tenant; its KEK given back, it opens.
#[test]
fn stores_of_revoked_tenants_open_not() {
    let world = World::new();
    let dir = made(test_branch(7), "a");
    let (store, uploader) = open_on(&world, &world.host(5), &dir).unwrap();
    written(&store, &uploader, ExtentId::new(7, 1), &noise(1, 4096));
    drop((store, uploader));
    world.kms.inner().revoke(TENANT).unwrap();
    let refused = open_on(&world, &world.host(6), &dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused.to_string(), "tenant 1: its KEK is revoked");
    assert_eq!(refused, Failure::Bucket(extent_bucket::Error::Kms(refused.to_string())));
    world.kms.inner().unrevoke(TENANT).unwrap();
    let host = world.host(6);
    let (store, _uploader) = open_on(&world, &host, &dir.crash(0)).unwrap();
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
}

/// AC #4: a generation of a branch's DEK added, the store's next opening encrypts its new objects
/// under it, what was encrypted under the one before read as before. The cell's KEK rotated, its
/// records' DEKs are rewrapped under the new one, the same DEKs, by one conditional update each,
/// and its stores open and read as before.
#[test]
fn generations_encrypt_new_objects_and_rotations_rewrap() {
    let world = World::new();
    let host = world.host(5);
    let branches = [test_branch(7), test_branch(8)];
    let dirs = [made(branches[0], "a"), made(branches[1], "b")];
    let (store, uploader) = open_on(&world, &host, &dirs[0]).unwrap();
    written(&store, &uploader, ExtentId::new(7, 1), &noise(1, 4096));
    drop(open_on(&world, &host, &dirs[1]).unwrap());
    let kms = host.runtime.keys.kms.clone();
    let cell_keys = CellKeys::of(TEST_CELL, &world.cell(), kms.clone());
    let added =
        world.runtime.block_on(keys::add_generation(&*world.bucket, &cell_keys, branches[0]));
    assert_eq!(added, Ok(2));
    assert_eq!(store.keys().generations(7), [1]);
    drop((store, uploader));
    let (store, uploader) = open_on(&world, &host, &dirs[0].crash(0)).unwrap();
    assert_eq!(store.keys().generations(7), [1, 2]);
    assert_eq!(store.keys().encrypting().unwrap().1, KeyId { branch: 7, generation: 2 });
    written(&store, &uploader, ExtentId::new(7, 2), &noise(2, 4096));
    let first = location(&store, ExtentId::new(7, 2)).key;
    let encrypted = extent_frame::Frame::of(&world.object(&first)).unwrap().encryption;
    assert!(
        matches!(encrypted, extent_frame::Encryption::Encrypted { key, .. } if key.generation == 2),
        "{first}: {encrypted:?}"
    );
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
    assert_eq!(read(&world, &host, &store, ExtentId::new(7, 2), 4096), Ok(noise(2, 4096)));
    let deks_before: Vec<_> = branches.iter().map(|&branch| world.record(branch).deks).collect();
    drop((store, uploader));

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
    let (store, _uploader) = open_on(&world, &other, &dirs[0].crash(0)).unwrap();
    assert_eq!(read(&world, &other, &store, ExtentId::new(7, 1), 4096), Ok(noise(1, 4096)));
    assert_eq!(read(&world, &other, &store, ExtentId::new(7, 2), 4096), Ok(noise(2, 4096)));
}

/// AC #5: an object of an encrypted branch put in the clear at its key, its data and footer framed
/// anew, CRCs and all, by one who can write the bucket: refused by the branch's readers, the error
/// naming it, whether its tail was kept before or not.
#[test]
fn objects_of_encrypted_branches_unencrypted_are_refused() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, uploader) = open_on(&world, &host, &dir).unwrap();
    let extent = ExtentId::new(7, 1);
    written(&store, &uploader, extent, &noise(1, 4096));
    let at = location(&store, extent);
    let bytes = world.object(&at.key);
    let keys = store.keys().of(7);
    let (data, footer) = (
        extent_frame::data(&bytes, &keys).unwrap(),
        extent_frame::footer(&bytes, &keys).unwrap().to_vec(),
    );
    world.put(&at.key, extent_frame::framed(&data, &footer, None));
    let downgraded = extent_bucket::Error::Downgrade(at.key.clone());
    assert_eq!(
        downgraded.to_string(),
        format!("{}: unencrypted, of a branch whose objects are encrypted", at.key)
    );
    // A host's fetchers, holding the store's keys, of no tail kept.
    assert_eq!(read(&world, &host, &store, extent, 4096), Err(downgraded.clone()));
    // And a reader holding none, as the collector reads, reads it.
    assert_eq!(fetched(&world, &Memory::new(), extent, &at), Ok(noise(1, 4096)));
    // Its newest checkpoint so, authenticated no more: its store's recovery refuses it, naming it.
    drop((store, uploader));
    let objects = world.objects();
    let mut checkpoints = objects.iter().filter(|(key, _)| {
        matches!(layout::kind(key), layout::Kind::Checkpoint { branch, .. } if branch.id == 7)
    });
    let (key, bytes) = checkpoints.next_back().unwrap();
    let keyless = &extent_frame::NoKeys;
    let (data, footer) = (
        extent_frame::data(bytes, keyless).unwrap(),
        extent_frame::footer(bytes, keyless).unwrap().to_vec(),
    );
    world.put(key, extent_frame::framed(&data, &footer, None));
    let refused = open_on(&world, &world.host(6), &dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused, Failure::Bucket(extent_bucket::Error::Downgrade(key.clone())));
}

/// AC #6: the object of an extent stored on its own, copied whole to another extent's key of the
/// same branch, is refused there: its footer, which its MAC covers, names the extent it is of. The
/// moves of an object, copied to another's moves, likewise.
#[test]
fn own_extents_copied_to_another_key_are_refused() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, uploader) = open_on(&world, &host, &dir).unwrap();
    let large = crate::Options::default().large_extent as usize;
    let (one, other) = (ExtentId::new(7, 1), ExtentId::new(7, 2));
    store.write(one, 0, &noise(1, large)).unwrap();
    store.write(other, 0, &noise(2, large)).unwrap();
    store.commit(None).unwrap();
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    let (at_one, at_other) = (location(&store, one), location(&store, other));
    assert!(matches!(layout::kind(&at_one.key), layout::Kind::Extent { .. }), "{}", at_one.key);
    assert_eq!(read(&world, &host, &store, other, large), Ok(noise(2, large)));
    world.put(&at_other.key, world.object(&at_one.key));
    let refused = extent_bucket::Error::Corrupt(format!("{} holds extent {one:?}", at_other.key));
    assert_eq!(fetched(&world, &Memory::new(), other, &at_other), Err(refused.clone()));
    let host = world.host(5);
    let _held = host.memory.keys().hold(store.keys().clone());
    assert_eq!(fetched(&world, &host.memory, other, &at_other), Err(refused));
}

/// AC #7: a host's fetchers open what they keep under the keys of the stores the host serves, not
/// a store's own: an encrypted part read before the host holds its key is unread, and read once it
/// holds it, by the tail and part kept; and served to a peer as stored, holding none.
#[test]
fn hosts_fetchers_open_parts_under_the_keys_of_the_stores_they_serve() {
    let world = World::new();
    let host = world.host(5);
    let dir = made(test_branch(7), "a");
    let (store, uploader) = open_on(&world, &host, &dir).unwrap();
    let extent = ExtentId::new(7, 1);
    written(&store, &uploader, extent, &noise(1, 4096));
    let at = location(&store, extent);
    // Another host's memory, of no store yet, which keeps what it reads in a cache.
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(extent_bucket::cache::Cache::open(cache_dir.path(), 1 << 30, 0).unwrap());
    let memory = Memory::new();
    let budget = Budget::new(Limits::UNBOUNDED);
    let fetcher = Fetcher::new(world.bucket.clone(), budget, memory.clone(), Some(cache));
    let reading = || world.runtime.block_on(fetcher.read(extent, &at, 0, 4096, Priority::Demand));
    let key = KeyId { branch: 7, generation: 1 };
    assert_eq!(reading(), Err(extent_bucket::Error::NoKey(key)));
    // Served to a peer as stored, encrypted.
    let tail = world.runtime.block_on(extent_bucket::object::tail(&*world.bucket, &at.key));
    let stored = tail.unwrap().unwrap().parts.stored[0];
    let served = world.runtime.block_on(fetcher.stored_part(&at.key, 0)).unwrap();
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

/// Forks a store's branch, as of its last commit, to `child`, as a host does: what the child
/// inherits asked for first, then the fork's commit made, then the fork in the bucket, its child's
/// record and checkpoint, and its keys.
fn forked(store: &Store<MemDir>, uploader: &Uploader<MemDir>, child: Child) -> Result<(), Failure> {
    let inheriting = uploader.inheriting(child)?;
    let epoch = match uploader.is_point() {
        true => 0,
        false => store.commit_fork(child).unwrap(),
    };
    uploader.forked(epoch, child, inheriting)
}

/// TASK-411.3: a fork within an encrypted cell, and a point there, read what they inherit under
/// their parent's DEK, which their rings hold, their parent's record's, unwrapped under the cell's
/// KEK; a fork's child gets a DEK of its own from its first writer, its records' writers before
/// naming none, and encrypts its own objects under it. Forks into other cells: below (TASK-411.4).
#[test]
fn forks_and_points_within_an_encrypted_cell_read_what_they_inherit() {
    let world = World::new();
    let host = world.host(5);
    let parent = test_branch(7);
    let dir = made(parent, "parent");
    let (store, uploader) = open_on(&world, &host, &dir).unwrap();
    let inherited = ExtentId::new(7, 1);
    written(&store, &uploader, inherited, &noise(1, 4096));

    // A fork's child, of the parent's cell.
    let child = Child { branch: test_branch(8), owner: 0x81, point: false };
    forked(&store, &uploader, child).unwrap();
    assert_eq!(world.record(child.branch).deks, Vec::new());
    let (forked_store, forked_uploader) =
        open_on(&world, &host, &made(child.branch, "child")).unwrap();
    assert_eq!(world.record(child.branch).deks.len(), 1);
    let keys = forked_store.keys();
    assert_eq!((keys.generations(7), keys.generations(8)), (vec![1], vec![1]));
    assert_eq!(keys.encrypting().unwrap().1, KeyId { branch: 8, generation: 1 });
    assert_eq!(read(&world, &host, &forked_store, inherited, 4096), Ok(noise(1, 4096)));
    let own = ExtentId::new(8, forked_uploader.next_sequence().max(1));
    written(&forked_store, &forked_uploader, own, &noise(2, 4096));
    let at = location(&forked_store, own).key;
    let encrypted = extent_frame::Frame::of(&world.object(&at)).unwrap().encryption;
    assert!(
        matches!(encrypted, extent_frame::Encryption::Encrypted { key, .. } if key.branch == 8),
        "{encrypted:?}"
    );
    assert_eq!(read(&world, &host, &forked_store, own, 4096), Ok(noise(2, 4096)));

    // A point, of the parent's cell: no DEK of its own, nothing writing it.
    let point = Child { branch: test_branch(9), owner: 0x91, point: true };
    forked(&store, &uploader, point).unwrap();
    let (point_store, _point_uploader) =
        open_on(&world, &host, &made(point.branch, "point")).unwrap();
    assert_eq!(world.record(point.branch).deks, Vec::new());
    assert_eq!(point_store.keys().generations(7), [1]);
    assert_eq!(point_store.keys().encrypting().map(|(_, id)| id), None);
    assert_eq!(read(&world, &host, &point_store, inherited, 4096), Ok(noise(1, 4096)));
}

/// TASK-411.4 AC #2, #3, #4: a fork into a cell of another tenant's, and its fork into a third
/// cell, as every fork a host makes is into a new cell: each child's record keeps, beside its own
/// DEK, the DEKs of every ancestor of another cell it reads, rewrapped under its own cell's KEK,
/// which came from the host's pool, its cell made with it at the fork. The forks make no KMS call,
/// nor do their stores' openings here; a take leaving the pool low refills it, a batch of wraps.
/// The parent's tenant's KEK destroyed, another host opens the child and the grandchild, reading
/// every extent they inherited, with only their own tenant's KEK unwrapped, one call a cell; the
/// parent opens no more.
#[test]
fn forks_into_other_cells_keep_what_they_inherit_rewrapped_and_outlive_their_parents_tenant() {
    let world = World::new();
    world.kms.inner().create(OTHER).unwrap();
    let host = world.pooled_host(5);
    let kms = host.runtime.keys.kms.clone().unwrap();
    let parent = test_branch(7);
    let parent_dir = made(parent, "parent");
    let (store, uploader) = open_on(&world, &host, &parent_dir).unwrap();
    let a = ExtentId::new(7, 1);
    written(&store, &uploader, a, &noise(1, 4096));
    // The host serves the other tenant too: its pool filled, as a store of its opening fills it.
    kms.refill(OTHER);
    world.runtime.block_on(kms.filled(OTHER));
    world.runtime.block_on(kms.filled(TENANT));
    assert_eq!(kms.in_pool(OTHER), 4);
    world.kms.reset();

    let child = child_in(OTHER, 5, 8, false);
    forked(&store, &uploader, child).unwrap();
    assert_eq!(world.calls(), (0, 0));
    assert_eq!(kms.in_pool(OTHER), 3);
    let record = world.record(child.branch);
    assert_eq!(record.deks.len(), 1);
    let inherited = |record: &extent_bucket::record::Record| {
        let had = record.inherited.iter();
        had.map(|had| (had.branch, had.dek.generation, had.dek.kek)).collect::<Vec<_>>()
    };
    assert_eq!(inherited(&record), [(parent, 1, 1)]);
    // Its cell, made at the fork with a KEK of the pool, held by no placement: the parent's DEK
    // unwraps under it, for the child alone.
    let made_cell = world.cell_of(child.branch.cell).unwrap();
    assert_eq!((made_cell.epoch, made_cell.token, made_cell.volumes.len()), (1, 0, 0));
    let kek = world.unwrapped(OTHER, &made_cell.keks[0].wrapped);
    let dek = keys::unwrap_inherited(&kek, child.branch, &record.inherited[0]).unwrap();
    assert_eq!(dek.expose(), store.keys().encrypting().unwrap().0.expose());
    assert!(keys::unwrap_inherited(&kek, test_branch(8), &record.inherited[0]).is_err());
    world.kms.reset();

    // Opened here: no call either, the cell's KEK kept from its making for its first taking.
    let child_dir = made(child.branch, "child");
    let (child_store, child_uploader) = open_on(&world, &host, &child_dir).unwrap();
    assert_eq!(world.calls(), (0, 0));
    assert_eq!(world.cell_of(child.branch.cell).unwrap().epoch, 2);
    assert_eq!(read(&world, &host, &child_store, a, 4096), Ok(noise(1, 4096)));
    let b = ExtentId::new(8, child_uploader.next_sequence().max(1));
    written(&child_store, &child_uploader, b, &noise(2, 4096));

    // Its fork into a third cell keeps both its parents' DEKs: the child's, and the parent's the
    // child kept.
    let grandchild = child_in(OTHER, 6, 9, false);
    forked(&child_store, &child_uploader, grandchild).unwrap();
    assert_eq!(world.calls(), (0, 0));
    assert_eq!(inherited(&world.record(grandchild.branch)), [(parent, 1, 1), (child.branch, 1, 1)]);
    let grandchild_dir = made(grandchild.branch, "grandchild");
    let (grandchild_store, _grandchild_uploader) = open_on(&world, &host, &grandchild_dir).unwrap();
    assert_eq!(world.calls(), (0, 0));
    for (extent, data) in [(a, noise(1, 4096)), (b, noise(2, 4096))] {
        assert_eq!(read(&world, &host, &grandchild_store, extent, 4096), Ok(data));
    }
    // A take leaving the pool low refills it: a batch of wraps, once the fork took its KEK.
    assert_eq!(kms.in_pool(OTHER), 2);
    forked(&child_store, &child_uploader, child_in(OTHER, 7, 10, false)).unwrap();
    world.runtime.block_on(kms.filled(OTHER));
    assert_eq!((world.calls(), kms.in_pool(OTHER)), ((4, 0), 5));
    drop((store, uploader, child_store, child_uploader, grandchild_store, _grandchild_uploader));

    // The parent's tenant shredded.
    world.kms.inner().destroy(TENANT).unwrap();
    world.kms.reset();
    let other = world.host(6);
    let refused = open_on(&world, &other, &parent_dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused.to_string(), "tenant 1: its KEK is destroyed");
    assert_eq!(world.calls(), (0, 1));
    for (dir, name) in [(&child_dir, "child"), (&grandchild_dir, "grandchild")] {
        let (store, _uploader) = open_on(&world, &other, &dir.crash(0)).unwrap();
        for (extent, data) in [(a, noise(1, 4096)), (b, noise(2, 4096))] {
            let at = location(&store, extent).key;
            assert_eq!(read(&world, &other, &store, extent, 4096), Ok(data), "{name}: {at}");
        }
    }
    // One unwrap a cell of the other tenant's; none of the parent's tenant's.
    assert_eq!(world.calls(), (0, 3));
}

/// TASK-411.4 AC #1, #2: a point made into another cell of its tenant's keeps no key, nothing
/// writing it: its copies read what it names under its origin's cell's KEK. Shared, it forks into
/// a cell of another tenant's, the clone keeping the keys it inherits rewrapped under its own
/// cell's KEK, with no KMS call. A host whose KMS refuses the point's tenant opens no copy of it, so forks none; a
/// host holding a copy open, told the point's tenant's KEK is revoked, refuses forks of it: no
/// record made either way, no KEK taken from the pool. The point's tenant's KEK destroyed, the
/// clone reads all it inherited; the point forks no more.
#[test]
fn points_fork_into_other_tenants_cells_rewrapped_unless_their_keys_are_refused() {
    let world = World::new();
    world.kms.inner().create(OTHER).unwrap();
    let host = world.pooled_host(5);
    let parent_dir = made(test_branch(7), "parent");
    let (store, uploader) = open_on(&world, &host, &parent_dir).unwrap();
    let a = ExtentId::new(7, 1);
    written(&store, &uploader, a, &noise(1, 4096));
    let point = child_in(TENANT, 3, 9, true);
    forked(&store, &uploader, point).unwrap();
    let record = world.record(point.branch);
    assert!(record.is_point());
    assert_eq!((record.deks.len(), record.inherited.len()), (0, 0));
    assert!(world.cell_of(point.branch.cell).is_none());
    world.runtime.block_on(branch::share(&*world.bucket, point.branch, true)).unwrap();
    drop((store, uploader));
    let point_dir = made(point.branch, "point");

    // A host whose KMS refuses the point's tenant: no copy, no fork.
    world.kms.inner().revoke(TENANT).unwrap();
    let refused = open_on(&world, &world.pooled_host(6), &point_dir.crash(0)).map(drop);
    assert_eq!(refused.unwrap_err().to_string(), "tenant 1: its KEK is revoked");
    world.kms.inner().unrevoke(TENANT).unwrap();

    // A host holding a copy open, told the point's tenant's KEK is revoked: the clone refused
    // before anything of it is made.
    let forker = world.pooled_host(7);
    let kms = forker.runtime.keys.kms.clone().unwrap();
    let (copy, copy_uploader) = open_on(&world, &forker, &point_dir.crash(0)).unwrap();
    kms.refill(OTHER);
    world.runtime.block_on(kms.filled(OTHER));
    kms.revoked(TENANT, "tenant 1: its KEK is revoked");
    let clone = child_in(OTHER, 4, 10, false);
    let refused = forked(&copy, &copy_uploader, clone).unwrap_err();
    let revoked = extent_bucket::Error::Kms("tenant 1: its KEK is revoked".into());
    assert_eq!(refused, Failure::Bucket(revoked));
    assert!(!world.has_record(clone.branch) && world.cell_of(clone.branch.cell).is_none());
    assert_eq!(kms.in_pool(OTHER), 4);
    drop((copy, copy_uploader));

    // Another host's copy forks it: no KMS call but its origin's cell's KEK unwrapped as the copy
    // opened.
    let forker = world.pooled_host(8);
    let kms = forker.runtime.keys.kms.clone().unwrap();
    world.kms.reset();
    let (copy, copy_uploader) = open_on(&world, &forker, &point_dir.crash(0)).unwrap();
    assert_eq!(world.calls(), (0, 1));
    kms.refill(OTHER);
    world.runtime.block_on(kms.filled(OTHER));
    world.kms.reset();
    forked(&copy, &copy_uploader, clone).unwrap();
    assert_eq!(world.calls(), (0, 0));
    let record = world.record(clone.branch);
    let inherited: Vec<_> = record.inherited.iter().map(|had| had.branch).collect();
    assert_eq!(inherited, [test_branch(7)]);
    assert_eq!(record.origin(), Some(layout::Origin { parent: test_branch(7), at: 1 }));
    let clone_dir = made(clone.branch, "clone");
    let (clone_store, _clone_uploader) = open_on(&world, &forker, &clone_dir).unwrap();
    assert_eq!(read(&world, &forker, &clone_store, a, 4096), Ok(noise(1, 4096)));
    drop((copy, copy_uploader, clone_store, _clone_uploader));

    world.kms.inner().destroy(TENANT).unwrap();
    let other = world.host(9);
    let (clone_store, _clone_uploader) = open_on(&world, &other, &clone_dir.crash(0)).unwrap();
    assert_eq!(read(&world, &other, &clone_store, a, 4096), Ok(noise(1, 4096)));
    let refused = open_on(&world, &world.host(10), &point_dir.crash(0)).map(drop).unwrap_err();
    assert_eq!(refused.to_string(), "tenant 1: its KEK is destroyed");
}

/// TASK-411.4 AC #5: a tenant's KEK revoked, the control API saying so, has its pool dropped: the
/// next fork into a new cell of the tenant's asks the KMS, refused, and makes nothing. Given back,
/// a fork asks for a KEK once and refills the pool. Destroyed, unheard, the KEKs the pool wrapped
/// before make cells still, until a refill's wraps are refused: the pool goes with them, and the
/// next fork is refused.
#[test]
fn revoked_tenants_pools_go_and_forks_into_their_new_cells_are_refused() {
    let world = World::new();
    world.kms.inner().create(OTHER).unwrap();
    let host = world.pooled_host(5);
    let kms = host.runtime.keys.kms.clone().unwrap();
    let (store, uploader) = open_on(&world, &host, &made(test_branch(7), "parent")).unwrap();
    written(&store, &uploader, ExtentId::new(7, 1), &noise(1, 4096));
    kms.refill(OTHER);
    world.runtime.block_on(kms.filled(OTHER));
    assert_eq!(kms.in_pool(OTHER), 4);

    world.kms.inner().revoke(OTHER).unwrap();
    kms.revoked(OTHER, "tenant 2: its KEK is revoked");
    assert_eq!(kms.in_pool(OTHER), 0);
    world.kms.reset();
    let child = child_in(OTHER, 5, 8, false);
    let refused = forked(&store, &uploader, child).unwrap_err();
    let revoked = extent_bucket::Error::Kms("tenant 2: its KEK is revoked".into());
    assert_eq!(refused, Failure::Bucket(revoked));
    assert_eq!(world.calls(), (1, 0));
    assert!(!world.has_record(child.branch) && world.cell_of(child.branch.cell).is_none());

    world.kms.inner().unrevoke(OTHER).unwrap();
    forked(&store, &uploader, child).unwrap();
    world.runtime.block_on(kms.filled(OTHER));
    assert_eq!((world.calls(), kms.in_pool(OTHER)), ((1 + 1 + 4, 0), 4));

    world.kms.inner().destroy(OTHER).unwrap();
    for (cell, id) in [(6, 9), (7, 10), (8, 11)] {
        forked(&store, &uploader, child_in(OTHER, cell, id, false)).unwrap();
    }
    // The last take left 1: its refill, refused, dropped the pool.
    world.runtime.block_on(kms.filled(OTHER));
    assert_eq!(kms.in_pool(OTHER), 0);
    let last = child_in(OTHER, 9, 12, false);
    let refused = forked(&store, &uploader, last).unwrap_err();
    let destroyed = extent_bucket::Error::Kms("tenant 2: its KEK is destroyed".into());
    assert_eq!(refused, Failure::Bucket(destroyed.clone()));
    assert_eq!(kms.refuses(OTHER), Some(destroyed));
    assert!(!world.has_record(last.branch) && world.cell_of(last.branch.cell).is_none());
}

/// TASK-411.4's twin in the clear: a fork of a store in the clear into another cell asks for
/// nothing more than before, no request made for what it inherits, on a host with no KMS or with
/// one its tenant has no KEK in; its child's record keeps no key, its store opening as before. A
/// encrypted store forks into no cell in the clear: refused before anything is made (`Inherited`).
#[test]
fn forks_in_the_clear_ask_for_nothing_more_and_encrypted_ones_go_into_no_clear_cell() {
    let clear = Branch { cell: Cell { tenant: 3, id: 1 }, id: 7 };
    for kms in [true, false] {
        let world = World::new();
        let host = world.host(5);
        let host = match kms {
            true => host,
            false => Host {
                runtime: Runtime {
                    keys: keys::Host { kms: None, ring: host.memory.keys().clone() },
                    ..host.runtime
                },
                ..host
            },
        };
        let dir = made(clear, "clear");
        let (store, uploader) = open_on(&world, &host, &dir).unwrap();
        written(&store, &uploader, ExtentId::new(7, 1), &noise(1, 4096));
        let child = child_in(3, 2, 8, false);
        // Nothing asked of the KMS, nor of the bucket, which it is not given.
        world.kms.reset();
        assert!(uploader.inheriting(child).unwrap().is_none());
        assert_eq!(world.calls(), (0, 0));
        forked(&store, &uploader, child).unwrap();
        let record = world.record(child.branch);
        assert_eq!((record.deks.len(), record.inherited.len()), (0, 0));
        let (forked_store, _forked_uploader) =
            open_on(&world, &host, &made(child.branch, "child")).unwrap();
        let read = read(&world, &host, &forked_store, ExtentId::new(7, 1), 4096);
        assert_eq!(read, Ok(noise(1, 4096)));
    }
    // Encrypted, into a cell of a tenant with no KEK.
    let world = World::new();
    let host = world.host(5);
    let (store, uploader) = open_on(&world, &host, &made(test_branch(7), "encrypted")).unwrap();
    written(&store, &uploader, ExtentId::new(7, 1), &noise(1, 4096));
    let child = child_in(3, 9, 8, false);
    let refused = forked(&store, &uploader, child).unwrap_err();
    let inherited = extent_bucket::Error::Inherited { branch: child.branch, from: test_branch(7) };
    assert_eq!(refused, Failure::Bucket(inherited));
    assert!(!world.has_record(child.branch) && world.cell_of(child.branch.cell).is_none());
}

/// The paths of the files of `dir` that hold any of `data`'s sectors' first 32 bytes: data the
/// store wrote, in the clear.
fn clear_files(dir: &MemDir, data: &[u8]) -> Vec<String> {
    let pieces: std::collections::HashSet<&[u8]> =
        data.chunks(4096).map(|sector| &sector[..32]).collect();
    (dir.files().into_iter())
        .filter(|(_, bytes)| bytes.windows(32).any(|window| pieces.contains(window)))
        .map(|(path, _)| path)
        .collect()
}

/// TASK-411.9, AC #2: an encrypted store crashed with its open segment holding a durable commit,
/// which its next opening closes again, reopened with its directory failing after each of the
/// opening's operations in turn, as a crash stops it, until one opens: no file of the directory,
/// its open segment's raw file and log among them (TASK-411.5), holds the commit's data in the
/// clear at any point of any of them; the store opened reads it, as it was before the crash
/// (TASK-411.5, AC #3), and its segment closed again is encrypted, in its directory and in the
/// bucket.
#[test]
fn segments_closed_again_are_never_in_the_clear() {
    let world = World::new();
    let host = world.host(5);
    let branch = test_branch(7);
    let dir = made(branch, "a");
    // No closing between commits: the open segment holds the commit as the crash leaves it.
    let quiet = Options { interval: Duration::from_secs(3600), ..Options::default() };
    let extent = ExtentId::new(branch.id, 1);
    let data = noise(1, 8 * 4096);
    let crashed = {
        let (store, uploader) = open_with(&world, &host, &dir, quiet.clone()).unwrap();
        store.write(extent, 0, &data).unwrap();
        store.commit(None).unwrap();
        let crashed = dir.crash(0);
        uploader.cut_short()();
        crashed
    };
    assert_eq!(clear_files(&crashed, &data), Vec::<String>::new());
    let mut attempts = 0;
    let (store, uploader) = loop {
        let attempt = crashed.crash(0);
        attempt.fail_after(Some(attempts));
        let opened = open_with(&world, &host, &attempt, quiet.clone());
        assert_eq!(clear_files(&attempt, &data), Vec::<String>::new(), "budget {attempts}");
        match opened {
            Ok(opened) if !attempt.has_failed() => break opened,
            Ok((_, uploader)) => uploader.cut_short()(),
            Err(_) => {}
        }
        attempts += 1;
    };
    assert!(attempts > 10, "the opening took {attempts} operations");
    assert_eq!(read(&world, &host, &store, extent, data.len()), Ok(data.clone()));
    uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
    let mut segments = 0;
    for (key, bytes) in world.objects() {
        assert!(!holds(&bytes, &data[..64]), "{key} holds the data");
        if let layout::Kind::Segment { .. } = layout::kind(&key) {
            let encryption = extent_frame::Frame::of(&bytes).unwrap().encryption;
            assert!(
                matches!(encryption, extent_frame::Encryption::Encrypted { .. }),
                "{key}: {encryption:?}"
            );
            segments += 1;
        }
    }
    assert!(segments > 0);
}

/// The branch of a cell of a tenant with no KEK in the tests' KMS: in the clear.
fn clear_branch() -> Branch {
    Branch { cell: extent_bucket::layout::Cell { tenant: 2, id: 1 }, id: 7 }
}

/// The bytes of the files of `dir` whose paths start with `kind`, by their paths.
fn files_of(dir: &MemDir, kind: &str) -> Vec<(String, Vec<u8>)> {
    dir.files().into_iter().filter(|(path, _)| path.starts_with(kind)).collect()
}

/// TASK-411.5, AC #1, #2, #4: what an encrypted store's guest writes is in no file of its
/// directory, its open segment's raw file, its log and a large extent's raw file among them,
/// written, committed, or framed; reads of the extents, open, sealed in the open segment or in a
/// file of their own, and framed, give it back. Its clear twin's raw files hold it as written, and
/// its logs are headed as before (`wal::LOG_MAGIC`).
#[test]
fn encrypted_stores_raw_files_and_logs_hold_no_plaintext() {
    for encrypted in [true, false] {
        let world = World::new();
        let host = world.host(5);
        let branch = if encrypted { test_branch(7) } else { clear_branch() };
        let dir = made(branch, "a");
        let quiet = Options { interval: Duration::from_secs(3600), ..Options::default() };
        let (store, uploader) = open_with(&world, &host, &dir, quiet).unwrap();
        assert_eq!(store.keys().encrypting().is_some(), encrypted);
        let (small, large) = (ExtentId::new(branch.id, 1), ExtentId::new(branch.id, 2));
        let small_data = noise(1, 8 * 4096);
        let large_data = noise(2, crate::Options::default().large_extent as usize);
        store.write(small, 0, &small_data).unwrap();
        store.write(large, 0, &large_data).unwrap();
        let check = |when: &str| {
            for (extent, data) in [(small, &small_data), (large, &large_data)] {
                let mut buf = vec![0; data.len()];
                store.read(extent, 0, &mut buf).unwrap();
                assert!(buf == **data, "{when}, encrypted {encrypted}: {extent:?} read back");
            }
            let mut found = clear_files(&dir, &small_data);
            found.extend(clear_files(&dir, &large_data));
            found
        };
        let found = check("written");
        match encrypted {
            true => assert_eq!(found, Vec::<String>::new(), "written"),
            false => {
                for kind in ["open-segments/", "open-extents/"] {
                    assert!(found.iter().any(|path| path.starts_with(kind)), "{kind}: {found:?}");
                }
                let [(_, raw)] = &files_of(&dir, "open-segments/")[..] else { panic!() };
                assert_eq!(&raw[..small_data.len()], &small_data[..]);
            }
        }
        store.commit(None).unwrap();
        let found = check("committed");
        assert_eq!(found.is_empty(), encrypted, "committed: {found:?}");
        store.close_segment().unwrap();
        store.commit(None).unwrap();
        store.closed(usize::MAX).unwrap();
        let found = check("framed");
        assert_eq!(found.is_empty(), encrypted, "framed: {found:?}");
        let magic = if encrypted { crate::wal::ENCRYPTED_LOG_MAGIC } else { crate::wal::LOG_MAGIC };
        let logs = files_of(&dir, "logs/");
        assert!(!logs.is_empty());
        for (path, log) in logs {
            assert_eq!(&log[..8], magic, "{path}");
        }
        uploader.cut_short()();
    }
}

/// TASK-400 in an encrypted store (TASK-411.5): an encrypted store's segment that compresses has
/// none of its parts packed or held as it fills, its parts' bytes in its raw file ciphertext, and
/// its framing, in a copy, reads every part back through the raw file, decrypting, then packs and
/// encrypts it: nothing of its data in any file of its directory, nor held, and read back as
/// written. Its clear twin holds its whole parts packed as they are written, and its framing reads
/// only its last, never whole. Both on a directory whose I/O is its callers' alone, which packs
/// ahead then and there, so that what is held is known at the commit.
#[test]
fn encrypted_stores_hold_no_parts_packed_ahead() {
    const MIB: usize = 1 << 20;
    // Letters at random, sixteen of them: compresses by about half, framed in a copy.
    let text = |seed: u64, len: usize| -> Vec<u8> {
        noise(seed, len).into_iter().map(|byte| b'a' + byte % 16).collect()
    };
    let data = |sequence: u64| text(sequence, if sequence <= 6 { MIB } else { 4096 });
    for encrypted in [true, false] {
        let world = World::new();
        let host = world.host(5);
        let branch = if encrypted { test_branch(7) } else { clear_branch() };
        let dir = MemDir::serial();
        let description = Description { block_size: 4096, capacity: 1 << 30, branch };
        let volume = crate::Volume { name: "a".into(), kind: cell::Kind::Fxfs };
        Store::make(&dir, description, Some("memory:///encrypted"), Some(volume)).unwrap();
        let held = crate::HeldParts::new();
        let interval = Duration::from_secs(3600);
        let options = Options { interval, held: Some(held.clone()), ..Options::default() };
        let (store, uploader) = open_with(&world, &host, &dir, options).unwrap();
        assert_eq!(store.keys().encrypting().is_some(), encrypted);
        for sequence in 1..=7 {
            store.write(ExtentId::new(branch.id, sequence), 0, &data(sequence)).unwrap();
        }
        store.commit(None).unwrap();
        let holding = held.counts();
        match encrypted {
            true => assert_eq!(holding, crate::HeldCounts::default(), "encrypted: none held"),
            false => assert!(holding.bytes > 2 * MIB as u64, "clear: held {holding:?}"),
        }
        let before = crate::framing_reads().0;
        store.close_segment().unwrap();
        store.commit(None).unwrap();
        store.closed(usize::MAX).unwrap();
        let read_back = crate::framing_reads().0 - before;
        let all = 6 * MIB as u64 + 4096;
        assert_eq!(
            read_back,
            if encrypted { all } else { 4096 },
            "encrypted {encrypted}: read back"
        );
        for sequence in 1..=7 {
            let (extent, data) = (ExtentId::new(branch.id, sequence), data(sequence));
            let mut buf = vec![0; data.len()];
            store.read(extent, 0, &mut buf).unwrap();
            assert!(buf == data, "encrypted {encrypted}: {extent:?} read back");
            if encrypted {
                assert_eq!(clear_files(&dir, &data), Vec::<String>::new(), "{extent:?}");
            }
        }
        if encrypted {
            assert_eq!(held.counts(), crate::HeldCounts::default(), "encrypted: none held framing");
        }
        uploader.cut_short()();
    }
}

/// TASK-411.5, AC #5: a write's bytes, as a caller writing them by an I/O of its own takes them
/// ([`crate::Write::stored`]), are encrypted for the file it names, at the offset it names, and
/// read back as written; a write of an encrypted store's large extent that is not whole sectors is
/// refused (TASK-411.8 refuses such writes to segments, encrypted or not), its clear twin's taken.
#[test]
fn writes_stored_by_callers_are_encrypted_and_large_extents_take_whole_sectors() {
    for encrypted in [true, false] {
        let world = World::new();
        let host = world.host(5);
        let branch = if encrypted { test_branch(7) } else { clear_branch() };
        let dir = MemDir::new();
        let description = Description { block_size: 512, capacity: 1 << 30, branch };
        let volume = crate::Volume { name: "a".into(), kind: cell::Kind::Fxfs };
        Store::make(&dir, description, Some("memory:///encrypted"), Some(volume)).unwrap();
        let (store, uploader) = open_on(&world, &host, &dir).unwrap();
        let extent = ExtentId::new(branch.id, 1);
        let data = noise(3, 4 * 4096);
        let write = store.begin_write(extent, 0, data.len() as u64).unwrap();
        let mut bytes = crate::Aligned::new(data.len());
        bytes.as_mut_slice().copy_from_slice(&data);
        let stored = write.stored(bytes).unwrap();
        assert_eq!(stored.bytes.as_slice() == &data[..], !encrypted);
        stored.file.write_at(stored.bytes.as_slice(), stored.offset).unwrap();
        write.finish();
        let mut buf = vec![0; data.len()];
        store.read(extent, 0, &mut buf).unwrap();
        assert!(buf == data, "encrypted {encrypted}: read back");
        assert_eq!(clear_files(&dir, &data).is_empty(), encrypted);
        let large = crate::Options::default().large_extent;
        let written = store.write(ExtentId::new(branch.id, 2), large, &data[..512]);
        match encrypted {
            true => assert_eq!(
                written.unwrap_err().downcast::<crate::Unsectored>().unwrap(),
                crate::Unsectored { offset: large, len: 512 }
            ),
            false => written.unwrap(),
        }
        uploader.cut_short()();
    }
}

/// TASK-411.5: an encrypted store's segment's log whose header, or a record, someone who can write
/// the host's disk changed, each record's hash made anew so that recovery, holding no key, takes
/// it, is refused as the store opens holding its keys, naming the segment, before anything is
/// framed from it.
#[test]
fn encrypted_logs_changed_are_refused() {
    let world = World::new();
    let host = world.host(5);
    let branch = test_branch(7);
    let dir = made(branch, "a");
    let quiet = Options { interval: Duration::from_secs(3600), ..Options::default() };
    let extent = ExtentId::new(branch.id, 1);
    let crashed = {
        let (store, uploader) = open_with(&world, &host, &dir, quiet.clone()).unwrap();
        store.write(extent, 0, &noise(1, 4096)).unwrap();
        store.commit(None).unwrap();
        let crashed = dir.crash(0);
        uploader.cut_short()();
        crashed
    };
    let segment = SegmentId { branch: branch.id, number: 0 };
    let path = crate::layout::log_path(segment);
    let log = read_file(&crashed.open(&path, false).unwrap()).unwrap();
    let header = crate::wal::LogHeader::of(&log, segment).unwrap();
    // A record's payload changed, its MAC kept, its hash made anew: the last byte of the run it
    // holds (`wal::Runs::Held`), which recovery writes into the raw data as it is.
    let changed_record = {
        let at = crate::wal::LOG_RECORDS as usize;
        let len = u32::from_le_bytes(log[at..at + 4].try_into().unwrap()) as usize;
        let mut payload = log[at + 12..at + 12 + len].to_vec();
        payload[len - extent_keys::MAC_LEN - 1] ^= 1;
        let mut log = log.clone();
        let framed = crate::layout::frame_salted(&payload, header.salt);
        log[at..at + framed.len()].copy_from_slice(&framed);
        log
    };
    // The header's DEK generation changed, or its salt.
    let changed_header = |at: usize| {
        let mut log = log.clone();
        log[at] ^= 1;
        log
    };
    let (generation, salt) = (8 + 3 * 8 + 8, 8 + 3 * 8 + 8 + 4);
    for (changed, refusal) in [
        (changed_record, "its record at 4096 is not authentic"),
        (changed_header(salt), "its header is not authentic"),
        (
            changed_header(generation),
            "is encrypted under KeyId { branch: 7, generation: 0 }, which the store holds not",
        ),
    ] {
        let attempt = crashed.crash(0);
        attempt.open(&path, false).unwrap().write_at(&changed, 0).unwrap();
        // Recovery holds no key, and takes the log as it is.
        assert_eq!(Store::recover(attempt.clone()).unwrap().branch(), branch);
        let refused = open_with(&world, &host, &attempt, quiet.clone()).map(drop).unwrap_err();
        assert!(format!("{refused:#}").contains(refusal), "{refused:#}");
        let segments = files_of(&attempt, "segments/");
        assert_eq!(segments, Vec::new(), "framed from a log changed");
    }
}

/// A host's fetcher, keeping what it reads in a part cache on disk, in `dir`, as a host's NVMe
/// cache does (doc-7, Reading), and its memory `memory`, whose keys are the host's.
fn caching_fetcher(
    world: &World,
    memory: &Arc<Memory>,
    dir: &std::path::Path,
) -> Fetcher<ObjectBucket> {
    let cache = Arc::new(extent_bucket::cache::Cache::open(dir, 1 << 30, 0).unwrap());
    Fetcher::new(world.bucket.clone(), Budget::new(Limits::UNBOUNDED), memory.clone(), Some(cache))
}

/// What `fetcher` reads of `extent`, `len` bytes, at `location`.
fn fetched_by(
    world: &World,
    fetcher: &Fetcher<ObjectBucket>,
    extent: ExtentId,
    location: &Location,
    len: u64,
) -> Result<Vec<u8>, extent_bucket::Error> {
    world.runtime.block_on(fetcher.read(extent, location, 0, len, Priority::Demand))
}

/// The paths of the files under `dir`, on the host's disk, that hold any of `data`'s sectors' first
/// 32 bytes.
fn clear_files_on_disk(dir: &std::path::Path, data: &[u8]) -> Vec<String> {
    // As on the disk, what the store's page cache holds written there first (TASK-433).
    crate::pages::shared().write_back_all().unwrap();
    let pieces: std::collections::HashSet<&[u8]> =
        data.chunks(4096).map(|sector| &sector[..32]).collect();
    let mut found = Vec::new();
    let mut paths = vec![dir.to_path_buf()];
    while let Some(path) = paths.pop() {
        if path.is_dir() {
            paths.extend(std::fs::read_dir(&path).unwrap().map(|entry| entry.unwrap().path()));
        } else if std::fs::read(&path).unwrap().windows(32).any(|window| pieces.contains(window)) {
            found.push(path.display().to_string());
        }
    }
    found
}

/// How many files there are under `dir`.
fn files_under(dir: &std::path::Path) -> usize {
    let mut count = 0;
    let mut paths = vec![dir.to_path_buf()];
    while let Some(path) = paths.pop() {
        match path.is_dir() {
            true => paths.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path())),
            false => count += 1,
        }
    }
    count
}

/// TASK-411.7, AC #1 (and TASK-411's AC #1): a tenant shredded, its KEK destroyed in the KMS, and
/// the hosts told so: none of its volumes is readable on any host. Host 5 holds its store open, its
/// data in the bucket, in its write-ahead log's closed segment and in its open segment, a commit
/// durable there (its closed segment, acknowledged, left the log: a closing's own test is
/// `segments_closed_again_are_never_in_the_clear`); host 6 read it into its part cache on disk, holding the store's keys as a host
/// serving it does. Told, each forgets the tenant's cell KEKs and pool, and its fetchers the
/// store's ring: host 6 opens nothing it cached of it, no file of either host holds its data, and
/// the store, crashed as it is, opens on neither host nor a third, the error naming the tenant,
/// before anything of its write-ahead log is read. A store of another tenant's reads on. A fork of
/// the shredded store into another tenant's cell, whose record keeps the DEK it inherited
/// rewrapped under its own cell's KEK, keeps it in host 5's memory and reads on, there and on a
/// host opening it afresh: shredding a parent leaves its children what they inherited.
#[test]
fn shredded_tenants_leave_none_of_their_volumes_readable_on_any_host() {
    let world = World::new();
    world.kms.inner().create(OTHER).unwrap();
    let (writer, reader) = (world.host(5), world.host(6));
    // A segment the guest fills is closed by a commit waiting for its upload alone.
    let filling = Options { filling_wait: Duration::from_secs(3600), ..Options::default() };
    let dir = made(test_branch(7), "a");
    let (store, uploader) = open_with(&world, &writer, &dir, filling).unwrap();
    let (uploaded, open) = (ExtentId::new(7, 1), ExtentId::new(7, 2));
    const LEN: usize = 8 * 4096;
    let (uploaded_data, open_data) = (noise(1, LEN), noise(2, LEN));
    written(&store, &uploader, uploaded, &uploaded_data);
    store.write(open, 0, &open_data).unwrap();
    store.commit(None).unwrap();
    for kind in ["open-segments/", "logs/"] {
        assert!(!files_of(&dir, kind).is_empty(), "{kind}: {:?}", dir.files());
    }
    let at = location(&store, uploaded);

    // Another tenant's store, and a fork of the shredded tenant's into a cell of the other's.
    let theirs = Branch { cell: Cell { tenant: OTHER, id: 1 }, id: 9 };
    let (their_store, their_uploader) = open_on(&world, &writer, &made(theirs, "theirs")).unwrap();
    let (their_extent, their_data) = (ExtentId::new(9, 1), noise(3, 4096));
    written(&their_store, &their_uploader, their_extent, &their_data);
    let their_at = location(&their_store, their_extent);
    let child = child_in(OTHER, 2, 8, false);
    forked(&store, &uploader, child).unwrap();
    let child_dir = made(child.branch, "child");
    let (child_store, _child_uploader) = open_on(&world, &writer, &child_dir).unwrap();
    let first = uploaded_data[..4096].to_vec();
    assert_eq!(read(&world, &writer, &child_store, uploaded, LEN), Ok(uploaded_data.clone()));

    // Host 6 serves the shredded tenant's store and the other's, reading into its part cache.
    let cache_dir = tempfile::tempdir().unwrap();
    let fetcher = caching_fetcher(&world, &reader.memory, cache_dir.path());
    let _held =
        [store.keys(), their_store.keys()].map(|keys| reader.memory.keys().hold(keys.clone()));
    assert_eq!(fetched_by(&world, &fetcher, uploaded, &at, 4096), Ok(first.clone()));
    assert_eq!(fetched_by(&world, &fetcher, their_extent, &their_at, 4096), Ok(their_data.clone()));
    assert!(files_under(cache_dir.path()) >= 2, "{}", files_under(cache_dir.path()));

    // Shredded: its KEK destroyed, then each host told.
    world.kms.inner().destroy(TENANT).unwrap();
    let why = "tenant 1: its KEK is destroyed";
    for host in [&writer, &reader] {
        assert_eq!(host.runtime.keys.forget(keys::Scope::Tenant(TENANT), why), 1);
        host.memory.forget_kept(keys::Scope::Tenant(TENANT));
        let kms = host.runtime.keys.kms.as_ref().unwrap();
        assert_eq!(kms.refuses(TENANT), Some(extent_bucket::Error::Kms(why.into())));
        assert_eq!(kms.in_pool(TENANT), 0);
    }
    // Host 5's fetchers keep the fork's ring, which holds the DEK it inherited of branch 7.
    assert_eq!(writer.memory.keys().ring().branches(), BTreeSet::from([7, 8, 9]));
    assert_eq!(reader.memory.keys().ring().branches(), BTreeSet::from([9]));
    let refused = extent_bucket::Error::NoKey(KeyId { branch: 7, generation: 1 });
    assert_eq!(fetched_by(&world, &fetcher, uploaded, &at, 4096), Err(refused));
    assert_eq!(fetched_by(&world, &fetcher, their_extent, &their_at, 4096), Ok(their_data.clone()));
    assert_eq!(read(&world, &writer, &child_store, uploaded, LEN), Ok(uploaded_data.clone()));

    // No file of either host holds its data; the store opens nowhere, its error naming the tenant.
    for data in [&uploaded_data, &open_data] {
        assert_eq!(clear_files(&dir, data), Vec::<String>::new());
        assert_eq!(clear_files_on_disk(cache_dir.path(), data), Vec::<String>::new());
    }
    for host in [&writer, &reader, &world.host(7)] {
        let refused = open_on(&world, host, &dir.crash(0)).map(drop).unwrap_err();
        assert_eq!(refused.to_string(), why, "host {}", host.token);
    }
    let third = world.host(8);
    let (child_store, _child_uploader) = open_on(&world, &third, &child_dir.crash(0)).unwrap();
    assert_eq!(read(&world, &third, &child_store, uploaded, LEN), Ok(uploaded_data.clone()));
    drop((store, uploader));
}

/// TASK-411.7, AC #2: a cell shredded, its wrapped KEK taken out of its object, leaves its volumes
/// unreadable, and the tenant's other cell readable. A host told nothing opens no store of it
/// afresh, the cell taken by no one, the error naming the cell; hosts told forget its KEK and their
/// fetchers its store's ring, opening nothing more of what they cached of it, and the other cell's
/// store reads on, there and afresh. The collector passes over it, marking, leaving its encrypted
/// objects uncompacted, not failing. The cell's object as it was before, put back, as a bucket
/// keeping non-current versions would let someone do, opens the store again: why a cell's
/// shredding needs non-current versions of its object expired, or versioning off.
#[test]
fn shredded_cells_leave_their_volumes_unreadable_and_the_tenants_others_readable() {
    let world = World::new();
    let (writer, reader) = (world.host(5), world.host(6));
    let ours = test_branch(7);
    let sibling = Branch { cell: Cell { tenant: TENANT, id: 2 }, id: 8 };
    let dirs = [made(ours, "ours"), made(sibling, "sibling")];
    let cache_dir = tempfile::tempdir().unwrap();
    let fetcher = caching_fetcher(&world, &reader.memory, cache_dir.path());
    let mut opened = Vec::new();
    let mut at = Vec::new();
    for (dir, branch) in dirs.iter().zip([ours, sibling]) {
        let (store, uploader) = open_on(&world, &writer, dir).unwrap();
        // Two extents, one freed: a segment the collector would compact.
        let (kept, freed) = (ExtentId::new(branch.id, 1), ExtentId::new(branch.id, 2));
        store.write(freed, 0, &noise(9, 4096)).unwrap();
        written(&store, &uploader, kept, &noise(branch.id, 4096));
        store.free(freed).unwrap();
        store.commit(None).unwrap();
        uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
        let location = location(&store, kept);
        let _held = reader.memory.keys().hold(store.keys().clone());
        assert_eq!(fetched_by(&world, &fetcher, kept, &location, 4096), Ok(noise(branch.id, 4096)));
        drop(_held);
        at.push((kept, location));
        opened.push((store, uploader));
    }
    let held: Vec<_> =
        opened.iter().map(|(store, _)| reader.memory.keys().hold(store.keys().clone())).collect();
    let cell_key = layout::cell(TEST_CELL);
    let before = world.object(&cell_key);

    assert_eq!(world.runtime.block_on(cell::shred(&*world.bucket, TEST_CELL)), Ok(1));
    let object = world.cell();
    assert_eq!((object.keks, object.shredded), (Vec::new(), true));
    let why = "tenant 1: cell 1/1 is shredded, its KEK gone";
    let untold = world.host(7);
    let refused = open_on(&world, &untold, &dirs[0].crash(0)).map(drop).unwrap_err();
    assert_eq!(refused, Failure::Bucket(extent_bucket::Error::Kms(why.into())));

    for host in [&writer, &reader] {
        assert_eq!(host.runtime.keys.forget(keys::Scope::Cell(TEST_CELL), why), 1);
        host.memory.forget_kept(keys::Scope::Cell(TEST_CELL));
        assert_eq!(host.memory.keys().ring().branches(), BTreeSet::from([8]));
    }
    let (ours_extent, ours_at) = &at[0];
    let key = KeyId { branch: 7, generation: 1 };
    let unread = fetched_by(&world, &fetcher, *ours_extent, ours_at, 4096);
    assert_eq!(unread, Err(extent_bucket::Error::NoKey(key)));
    let (sibling_extent, sibling_at) = &at[1];
    assert_eq!(fetched_by(&world, &fetcher, *sibling_extent, sibling_at, 4096), Ok(noise(8, 4096)));
    drop((held, opened));
    for host in [&writer, &reader, &untold] {
        let refused = open_on(&world, host, &dirs[0].crash(0)).map(drop).unwrap_err();
        assert_eq!(refused.to_string(), why, "host {}", host.token);
    }
    let (store, _uploader) = open_on(&world, &untold, &dirs[1].crash(0)).unwrap();
    assert_eq!(read(&world, &untold, &store, *sibling_extent, 4096), Ok(noise(8, 4096)));
    for data in [noise(7, 4096), noise(9, 4096)] {
        assert_eq!(clear_files(&dirs[0], &data), Vec::<String>::new());
        assert_eq!(clear_files_on_disk(cache_dir.path(), &data), Vec::<String>::new());
    }

    // The collector, with the tenant's KEK at hand, can't have the cell's.
    let options = extent_bucket::collector::Options { compact_below: 1.0, grace: 0 };
    let kms: extent_keys::kms::Shared = world.kms.clone();
    let mut collector = extent_bucket::collector::Collector::new(options).kms(Some(kms));
    let pass = collector.collect_cell(&*world.bucket, TEST_CELL, 0x51);
    let report = world.runtime.block_on(pass).unwrap();
    assert_eq!((report.locked, report.uncompacted, report.compacted), (1, 1, 0), "{report:?}");

    // Its object as it was before, put back: the store opens.
    world.put(&cell_key, before);
    let host = world.host(8);
    let (store, _uploader) = open_on(&world, &host, &dirs[0].crash(0)).unwrap();
    assert_eq!(read(&world, &host, &store, *ours_extent, 4096), Ok(noise(7, 4096)));
}
