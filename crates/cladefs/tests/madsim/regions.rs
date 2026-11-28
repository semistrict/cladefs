// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Board 14521 (doc-23): cells across the buckets of cloud regions, under madsim, on the daemon's
//! own path. Each cloud region is a bucket of its own, with its own faults, its own KMS and its own
//! hosts, two each; every daemon boots as production boots one, naming its home bucket by its URL
//! (`CLADEFSD_BUCKET`), which it opens routed (`RoutedBucket`), the other regions' buckets found in
//! the registry of buckets and opened by it (`Registered`), each under the KMS its entry records,
//! which a host reaches as far as it trusts it (`extent_keys::kms::trust`). Nothing here hands a
//! daemon a bucket, a resolver or a KMS: the simulation serves the clouds
//! (`bucket_cell::clouds`), and the daemons reach them.
//!
//! What it checks is what `specs/buckets` models (`Buckets.tla`), on the code: a version forked
//! into another region's bucket is read from its cell's bucket until its mirror is marked, and
//! from its child's own after, its cell's bucket out of reach; mirrors raced, stopped by their
//! hosts' crashes and taken over, their versions dropped and their cells deleted under them; each
//! region's collector passing meanwhile; and, every cell deleted, nothing of any of it left.

use super::*;
use bucket_cell::faults::{Odds, Seeded, SeededBackend};
use bucket_cell::identity::{self, Details};
use bucket_cell::layout::{self, CellId, Kind, MirrorState};
use bucket_cell::object_bucket::Registered;
use bucket_cell::sweep::{Limits, Mode, Sweep, Swept};
use cladefs::cladefsd::control::proto::{DeleteCellRequest, DropVersionsRequest};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use madsim::runtime::NodeHandle;
use object_store::path::Path as Location;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// The hosts of each cloud region: hosts 1 and 2 are region 1's, 3 and 4 region 2's, and so on.
const HOSTS_EACH: u8 = 2;

/// The cloud region host `host` is of, by its home bucket's number.
fn region_of(host: u8) -> u16 {
    u16::from((host - 1) / HOSTS_EACH) + 1
}

/// The hosts of cloud region `region`.
fn hosts_of(region: u16) -> [u8; HOSTS_EACH as usize] {
    let first = (region as u8 - 1) * HOSTS_EACH + 1;
    [first, first + 1]
}

/// The URL of cloud region `region`'s bucket, as its hosts name it and the registry records it.
fn url_of(region: u16) -> String {
    format!("memory:///region{region}")
}

/// What was written to a region's bucket, or deleted from it, once it landed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landed {
    /// An object written: a mirror's marker's state, if it is one.
    Put(Option<MirrorState>),
    Deleted,
}

/// A cloud region's bucket as the cloud has it: its objects, and every write and deletion of them
/// that landed, in order, by their keys. It is under the region's faults, so that a request the
/// faults fail before it lands is none of its, and one they fail after it landed is.
#[derive(Debug)]
struct Journal {
    objects: Arc<dyn ObjectStore>,
    /// What every key of the bucket begins with in `objects`: its URL's path.
    prefix: String,
    /// The simulation's buckets' count of what landed, which orders it across them.
    order: Arc<std::sync::atomic::AtomicU64>,
    /// What landed: its place in that order, when by the simulation's clock, in milliseconds, its
    /// key, and what it was.
    landed: Arc<Mutex<Vec<(u64, u64, String, Landed)>>>,
}

impl Journal {
    fn new(region: u16, order: Arc<std::sync::atomic::AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            objects: Arc::new(object_store::memory::InMemory::new()),
            prefix: format!("region{region}/"),
            order,
            landed: Arc::default(),
        })
    }

    /// Notes that `landed` did, of the object at `key`.
    fn note(
        order: &std::sync::atomic::AtomicU64,
        all: &Mutex<Vec<(u64, u64, String, Landed)>>,
        key: String,
        landed: Landed,
    ) {
        let at = order.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        all.lock().unwrap().push((at, now.as_millis() as u64, key, landed));
    }

    /// The key of the object at `location`.
    fn key(prefix: &str, location: &Location) -> String {
        let location = location.as_ref();
        location.strip_prefix(prefix).unwrap_or(location).to_owned()
    }

    /// What landed, in order, each with its place among what landed in any of the simulation's
    /// buckets, and when.
    fn landed(&self) -> Vec<(u64, u64, String, Landed)> {
        self.landed.lock().unwrap().clone()
    }
}

impl std::fmt::Display for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Journal({})", self.prefix)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Journal {
    async fn put_opts(
        &self,
        location: &Location,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let key = Self::key(&self.prefix, location);
        let marker = matches!(layout::kind(&key), Kind::Mirror { .. }).then(|| {
            let bytes: Vec<u8> = payload.iter().flat_map(|chunk| chunk.iter().copied()).collect();
            layout::Mirror::decode(&bytes).expect("a marker written decodes").state
        });
        let put = self.objects.put_opts(location, payload, opts).await?;
        Self::note(&self.order, &self.landed, key, Landed::Put(marker));
        Ok(put)
    }

    async fn put_multipart_opts(
        &self,
        location: &Location,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.objects.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Location,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.objects.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Location>>,
    ) -> BoxStream<'static, object_store::Result<Location>> {
        let (order, landed, prefix) =
            (self.order.clone(), self.landed.clone(), self.prefix.clone());
        let deleted = self.objects.delete_stream(locations).inspect(move |deleted| {
            if let Ok(location) = deleted {
                Self::note(&order, &landed, Self::key(&prefix, location), Landed::Deleted);
            }
        });
        deleted.boxed()
    }

    fn list(
        &self,
        prefix: Option<&Location>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.objects.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Location>,
    ) -> object_store::Result<ListResult> {
        self.objects.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Location,
        to: &Location,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.objects.copy_opts(from, to, options).await
    }
}

/// A cloud region of the simulation.
struct Region {
    number: u16,
    /// Its bucket, as the cloud has it.
    journal: Arc<Journal>,
    /// Its bucket's faults, its own: how its requests are delayed and failed, and whether the
    /// whole of it is out of reach.
    faults: Arc<Seeded>,
    /// Its bucket as the checks read it: at once, whatever its faults ([`Own`]).
    seen: Arc<ObjectBucket>,
}

impl Region {
    /// Its whole bucket out of reach, every request of it failing, until [`Region::mend`].
    fn cut(&self) {
        let out_for = Duration::from_secs(24 * 3600);
        self.faults.set(Odds { out_of_reach: 1.0, out_for, ..Odds::NONE });
    }

    /// Its bucket in reach again, its requests as `odds` says.
    fn mend(&self, odds: Odds) {
        // None first, which ends a stretch out of reach under way.
        self.faults.set(Odds::NONE);
        self.faults.set(odds);
    }
}

/// How the regions' buckets fare while a case runs, each by its own seed: as the one bucket of the
/// other profiles does ([`world`]).
const FAULTY: Odds = Odds {
    fail_before: 0.05,
    fail_after: 0.05,
    out_of_reach: 0.01,
    out_for: Duration::from_millis(500),
};

/// Every region's own cells, as one bucket, as the checks that go over them all read them
/// (`bucket_cell::testing`'s `uncollected` and `uncollected_copies_of_daemons`): each read at once
/// and whole, whatever its bucket's faults; a request goes to the bucket its key's cell is of, and
/// a listing of no cell's prefix lists each region's bucket's own objects, a hold kept there for
/// another's mirror among them, not the copies it keeps of other regions' cells.
struct Own(Vec<Arc<ObjectBucket>>);

impl Own {
    fn of(&self, key: &str) -> &ObjectBucket {
        let number = layout::routed_to(key).unwrap_or(identity::ROOT);
        &self.0[usize::from(number) - 1]
    }
}

impl bucket_cell::Bucket for Own {
    async fn get(
        &self,
        key: &str,
    ) -> Result<bucket_cell::bucket::Object, bucket_cell::bucket::Error> {
        self.of(key).get(key).await
    }

    async fn get_range(
        &self,
        key: &str,
        range: bucket_cell::bucket::ByteRange,
    ) -> Result<Vec<u8>, bucket_cell::bucket::Error> {
        self.of(key).get_range(key, range).await
    }

    async fn head(
        &self,
        key: &str,
    ) -> Result<bucket_cell::bucket::Meta, bucket_cell::bucket::Error> {
        self.of(key).head(key).await
    }

    async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<bucket_cell::bucket::Meta>, bucket_cell::bucket::Error> {
        if layout::routed_to(prefix).is_some() {
            return self.of(prefix).list(prefix).await;
        }
        let mut listed = Vec::new();
        for (number, bucket) in (1u16..).zip(&self.0) {
            let own = bucket.list(prefix).await?;
            let own = own
                .into_iter()
                .filter(|meta| layout::routed_to(&meta.key).is_none_or(|of| of == number));
            listed.extend(own);
        }
        Ok(listed)
    }

    async fn list_delimited(
        &self,
        prefix: &str,
        page: bucket_cell::bucket::Page<'_>,
    ) -> Result<bucket_cell::bucket::Listing, bucket_cell::bucket::Error> {
        self.of(prefix).list_delimited(prefix, page).await
    }

    async fn put(
        &self,
        key: &str,
        _: bucket_cell::Bytes,
        _: bucket_cell::bucket::PutMode,
    ) -> Result<bucket_cell::bucket::Version, bucket_cell::bucket::Error> {
        panic!("a check writing {key}")
    }

    async fn delete(&self, key: &str) -> Result<(), bucket_cell::bucket::Error> {
        panic!("a check deleting {key}")
    }

    fn now(&self) -> u64 {
        self.0[0].now()
    }
}

/// Cloud regions, their buckets, KMSes and hosts.
struct Regions {
    regions: Vec<Region>,
    /// Hosts 1 on, [`HOSTS_EACH`] of each region.
    nodes: Vec<NodeHandle>,
    /// The regions' KMSes' directories, on the real host's disk: a KMS is no host's, and outlives
    /// every host's crash.
    _kmses: tempfile::TempDir,
}

/// What a region's hosts trust of the KMSes the registry of buckets records: whether each allows
/// a recorded `local://` one (`CLADEFSD_KMS_LOCAL`), by its place in the region.
#[derive(Clone, Copy)]
struct Kms {
    local: [bool; HOSTS_EACH as usize],
}

impl Kms {
    /// A region whose hosts trust the other regions' `local://` KMSes, as the simulation's are.
    const fn trusting() -> Self {
        Self { local: [true; HOSTS_EACH as usize] }
    }
}

impl Regions {
    /// Regions 1 to `kmses`' count, their buckets formatted and added to region 1's, the root's,
    /// as an operator does (`cladefsctl format-bucket`, `add-bucket`), each recording its URL and
    /// its KMS; each KMS holding tenant 0's KEK; their hosts booted, their buckets' requests going
    /// as `odds` says from then on.
    async fn boot(kmses: Vec<Kms>, odds: Odds) -> Self {
        // Once a process: a determinism check runs the test twice in it.
        let _ = cladefs::cladefsd::logging::to_stderr();
        bucket_cell::buggify::enable(madsim::runtime::Handle::current().seed());
        let kms_dirs = tempfile::tempdir().unwrap();
        let count = kmses.len() as u16;
        let order = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let journals: Vec<Arc<Journal>> =
            (1..=count).map(|region| Journal::new(region, order.clone())).collect();
        let seen: Vec<Arc<ObjectBucket>> = (1..=count)
            .zip(&journals)
            .map(|(region, journal)| {
                Arc::new(ObjectBucket::new(journal.clone(), &format!("region{region}")))
            })
            .collect();
        let mut regions = Vec::new();
        let mut own_kms = Vec::new();
        for ((region, journal), seen) in (1..=count).zip(&journals).zip(&seen) {
            // Its requests let go one at a time, the one its seed picks of those waiting, as the
            // one bucket of the other worlds lets its own ([`world`]).
            let faults = Arc::new(Seeded::new(journal.clone(), madsim::rand::random(), Odds::NONE));
            madsim::task::spawn(faults.clone().schedule(SCHEDULED_EVERY));
            bucket_cell::clouds::serve(&url_of(region), Arc::new(SeededBackend(faults.clone())));
            let dir = kms_dirs.path().join(format!("region{region}"));
            extent_keys::kms::LocalKms::open(&dir).unwrap().create(0).unwrap();
            own_kms.push(format!("local://{}", dir.display()));
            let (journal, seen) = (journal.clone(), seen.clone());
            regions.push(Region { number: region, journal, faults, seen });
        }
        // The operator's: each bucket reached by its URL, as its hosts will reach it.
        let details = |region: u16| Details {
            provider: Some("memory".to_owned()),
            region: Some(format!("region-{region}")),
            url: Some(url_of(region)),
            kms: Some(own_kms[usize::from(region) - 1].clone()),
            ..Details::default()
        };
        let opened: Vec<ObjectBucket> =
            (1..=count).map(|region| ObjectBucket::from_url(&url_of(region)).unwrap()).collect();
        identity::format(&opened[0], &details(1)).await.unwrap();
        let mut registry = identity::registry(&opened[0]).await.unwrap().unwrap();
        for region in 2..=count {
            let new = &opened[usize::from(region) - 1];
            let (added, with) =
                identity::add(&opened[0], &details(1), new, details(region)).await.unwrap();
            assert_eq!(added.number, region);
            registry = with;
        }
        for entry in &registry.buckets {
            let bucket = &opened[usize::from(entry.number) - 1];
            identity::spread_to(&registry, entry, bucket).await.unwrap();
        }
        for region in &regions {
            region.faults.set(odds);
        }
        let handle = madsim::runtime::Handle::current();
        let hosts = count as u8 * HOSTS_EACH;
        // Every host told every host, of its region or not: a host's peers are its region's
        // alone, whatever its list says (TASK-633.4).
        let peers: Vec<String> = (1..=hosts).map(|host| peer(host).to_string()).collect();
        let nodes = (1..=hosts)
            .map(|host| {
                let region = region_of(host);
                let kms = &kmses[usize::from(region) - 1];
                let root = Path::new(ROOTS).join(format!("host{host}"));
                let mut vars = vec![
                    ("CLADEFS_KMS", own_kms[usize::from(region) - 1].clone()),
                    ("CLADEFSD_PEER_ADDRESS", peer(host).to_string()),
                    ("CLADEFSD_PEERS", peers.join(",")),
                    (
                        "CLADEFSD_PEER_TLS",
                        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/peer-tls").to_owned(),
                    ),
                    ("CLADEFSD_PART_CACHE", root.join("cache").display().to_string()),
                ];
                if kms.local[usize::from((host - 1) % HOSTS_EACH)] {
                    vars.push(("CLADEFSD_KMS_LOCAL", "1".to_owned()));
                }
                let home = Home::At { url: url_of(region), vars };
                handle
                    .create_node()
                    .name(format!("host{host}"))
                    .ip(control(host).ip())
                    .restart_on_panic_matching(BOOT_FAILED)
                    .init(move || {
                        let (root, home) = (root.clone(), home.clone());
                        async move { boot(host, &root, home, Kept::NONE, None).await }
                    })
                    .build()
            })
            .collect();
        Self { regions, nodes, _kmses: kms_dirs }
    }

    fn region(&self, number: u16) -> &Region {
        &self.regions[usize::from(number) - 1]
    }

    /// Host `host`'s node, as it is now: one started again is a node anew.
    fn node(&self, host: u8) -> NodeHandle {
        let id = self.nodes[usize::from(host) - 1].id();
        madsim::runtime::Handle::current().get_node(id).unwrap()
    }

    /// Every region's bucket, as the checks see it.
    fn seen(&self) -> Vec<Arc<ObjectBucket>> {
        self.regions.iter().map(|region| region.seen.clone()).collect()
    }

    /// `cell`'s head, as its region's bucket has it.
    async fn head(&self, cell: CellId) -> Option<layout::Head> {
        let region = layout::routed_to(&cell.prefix()).expect("a cell of a region's bucket");
        bucket_cell::head::get(&*self.region(region).seen, cell).await.unwrap()
    }

    /// Whether `origin`'s version is marked mirrored in region `number`'s bucket.
    async fn marked(&self, number: u16, origin: layout::Origin) -> bool {
        let marker = layout::mirror(origin.parent, origin.at);
        let found = bucket_cell::bucket::get(&*self.region(number).seen, &marker).await.unwrap();
        found.is_some_and(|object| layout::Mirror::decode(&object.bytes).unwrap().is_marked())
    }

    /// The keys region `number`'s bucket holds of mirrors: copies of other regions' cells'
    /// objects, markers and pins; and holds, in whichever bucket.
    async fn of_mirrors(&self, number: u16) -> Vec<String> {
        let listed = bucket_cell::bucket::list(&*self.region(number).seen, "").await.unwrap();
        let of_mirror = |key: &String| {
            layout::routed_to(key).is_some_and(|of| of != number)
                || matches!(layout::kind(key), Kind::Hold { .. } | Kind::Pin { .. })
                || matches!(layout::kind(key), Kind::Mirror { .. })
        };
        listed.into_iter().map(|meta| meta.key).filter(of_mirror).collect()
    }

    /// What every region's bucket keeps that it should not, its collectors done, and what it keeps
    /// short (`bucket_cell::testing`'s checks, the other simulator's): of its own cells', and of
    /// its copies of other regions', their markers never without their holds, nor their copies
    /// short, nor leaving a live child referring in its version's cell's bucket alone.
    async fn uncollected(&self) -> Vec<String> {
        let own = Own(self.seen());
        let mut left = bucket_cell::testing::uncollected(&own).await;
        for region in &self.regions {
            let copies = bucket_cell::testing::uncollected_copies_of_daemons(
                &own,
                &*region.seen,
                region.number,
            );
            left.extend(copies.await);
        }
        left
    }
}

/// The value of the daemon's metric `series`, a name and its labels as `/metrics` says them, in
/// `metrics`: 0 if it says none.
fn metric(metrics: &str, series: &str) -> u64 {
    let found = metrics.lines().find_map(|line| line.strip_prefix(series)?.strip_prefix(' '));
    found.map_or(0, |value| value.trim().parse().expect("a metric's value is a count"))
}

/// This host's daemon's metrics, once it serves.
async fn metrics() -> String {
    loop {
        if let Some(daemon) = current_daemon().filter(|daemon| daemon.serving()) {
            return daemon.metrics();
        }
        madsim::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How many blocks a parent's first version holds, each committed uploaded on its own, so that
/// each is in an object of its own: those of the first half are read as a case goes, lazily; the
/// rest not until a check reads them, so that it reads them from a bucket.
const BLOCKS: u64 = 8;

/// How many blocks more a parent's second version holds, which shares the first's objects.
const MORE: u64 = 2;

/// A parent: a cell of one region, versions of which hosts of other regions fork.
#[derive(Clone, Copy, Debug)]
struct Parent {
    cell: u64,
    /// The host it was made on, which holds it.
    host: u8,
}

impl Parent {
    fn id(&self) -> CellId {
        CellId { tenant: 0, id: self.cell }
    }
}

/// Makes a version of a cell on this host: `count` blocks of `round`'s more written to `cell`, a
/// new cell if none, each committed uploaded, then the version made, and uploaded. The cell, the
/// version, and the blocks written, by their extents' sequences; none if a write or a commit of it
/// failed.
async fn cut_version(cell: Option<u64>, round: u64, count: u64) -> Option<(u64, u64, Blocks)> {
    let guest = guest().await;
    let cell = match cell {
        Some(cell) => guest.open_cell(CellId { tenant: 0, id: cell }).await.ok()?,
        None => guest.open_new(0, 64 << 20).await.ok()?,
    };
    let first = guest.next_sequence().await.ok()?;
    let mut blocks = Blocks::new();
    for sequence in first..first + count {
        let block = block_of(2000 + round, sequence);
        guest.write(cladefs::ExtentId { cell, sequence }, 0, &block).await.ok()?;
        guest.commit(cladefs::Commit::Uploaded).await.ok()?;
        blocks.insert(sequence, block);
    }
    let version = guest.make_version().await.ok()?;
    // Uploaded, which has the head name it, as a fork of it reads.
    guest.commit(cladefs::Commit::Uploaded).await.ok()?;
    guest.close();
    Some((cell, version, blocks))
}

/// A child: a fork of a parent's version, a cell of the forking host's region.
#[derive(Clone, Debug)]
struct Child {
    cell: u64,
    /// The host that holds it: the one that forked it, until another takes it.
    host: u8,
    of: Parent,
    /// The version it was forked from, as its head names it.
    origin: layout::Origin,
    /// The blocks it inherits: its parent's as of its version.
    inherits: Blocks,
    /// Its own blocks told uploaded.
    told: Blocks,
}

impl Child {
    fn id(&self) -> CellId {
        CellId { tenant: 0, id: self.cell }
    }

    /// The sequences of the blocks it inherits that are read as a case goes: its parent's first
    /// version's first half.
    fn lazily(&self) -> Vec<u64> {
        self.inherits.keys().copied().take(BLOCKS as usize / 2).collect()
    }

    /// Those no host reads until a check does: the rest.
    fn unread(&self) -> Vec<u64> {
        self.inherits.keys().copied().skip(BLOCKS as usize / 2).collect()
    }
}

/// The control API of host `host`'s daemon, asked once: unreached, as a host killed is, is a call
/// failed.
async fn reach(
    host: u8,
) -> Result<
    cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient<tonic::transport::Channel>,
    tonic::Status,
> {
    let connecting = cladefs::cladefsd::control::connect(control(host)).await;
    connecting.map_err(|err| tonic::Status::unavailable(format!("{err:?}")))
}

/// Calls `call` on host `host`'s control API, again while the buckets, a peer or the host itself
/// fail it, `tries` times at most, half a second apart: what it answered, or none if it never did,
/// as a call needing a bucket out of reach, or a host killed, has it.
async fn asked<T, F>(
    host: u8,
    tries: u32,
    call: impl Fn() -> F,
) -> Option<Result<T, cladefs::Errno>>
where
    F: std::future::Future<Output = Result<T, tonic::Status>>,
{
    for _ in 0..tries {
        match call().await {
            Ok(done) => return Some(Ok(done)),
            Err(status) => match cladefs::cladefsd::control::errno(&status) {
                Some(errno) if !matches!(errno, cladefs::Errno::ENOTCONN | cladefs::Errno::EIO) => {
                    return Some(Err(errno));
                }
                errno => log::info!("host {host}: {errno:?} {}", status.message()),
            },
        }
        madsim::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// Forks `version` of `parent` on host `host`, as the VM manager does, within a minute: the child's
/// cell, or none if it was refused, as a fork of a version dropped is, or never made, as one whose
/// version's cell's bucket is out of reach, or whose host was killed, is. Fails if it was refused
/// otherwise: as by a KMS (`EACCES`), say, where every host trusts every region's and a bucket
/// failing a request refuses nothing (board 14562).
async fn fork_on(host: u8, parent: u64, version: u64) -> Result<Option<u64>, String> {
    let forking = || async move {
        let request = ForkCellRequest { version, cell: parent, ..ForkCellRequest::default() };
        reach(host).await?.fork_cell(request).await
    };
    match asked(host, 120, forking).await {
        Some(Ok(forked)) => Ok(Some(forked.into_inner().cell)),
        // Its version dropped, or its cell deleted.
        Some(Err(cladefs::Errno::ENOENT)) => Ok(None),
        Some(Err(errno)) => Err(format!(
            "host {host}: forking {parent}'s version {version}: {errno:?}, nothing refusing it so"
        )),
        None => Ok(None),
    }
}

/// Deletes `cell` on host `host`, as the VM manager does: whether it was told deleted.
async fn delete_on(host: u8, cell: u64) -> bool {
    let deleting = || async move {
        reach(host).await?.delete_cell(DeleteCellRequest { tenant: 0, cell }).await
    };
    let deleted = asked(host, 60, deleting).await;
    log::info!("host {host}: deleting {cell}: {:?}", deleted.as_ref().map(|told| told.is_ok()));
    matches!(deleted, Some(Ok(_)))
}

/// Drops `version` of `cell` on host `host`, and has the cell's head say so: whether both were
/// told done.
async fn drop_on(host: u8, cell: u64, version: u64) -> bool {
    let dropping = || async move {
        let request = DropVersionsRequest { versions: vec![version], tenant: 0, cell };
        reach(host).await?.drop_versions(request).await
    };
    let syncing =
        || async move { reach(host).await?.sync_cell(SyncCellRequest { tenant: 0, cell }).await };
    let dropped = matches!(asked(host, 60, dropping).await, Some(Ok(_)))
        && matches!(asked(host, 60, syncing).await, Some(Ok(_)));
    log::info!("host {host}: dropping {cell}'s version {version}: {dropped}");
    dropped
}

/// What a guest of this host's read of `child`: each of `sequences`' extents of the cell `of`, its
/// first block, or the errno. None if the child opened not.
async fn read_blocks(
    child: u64,
    of: u64,
    sequences: Vec<u64>,
) -> Option<Vec<(u64, Result<Vec<u8>, cladefs::Errno>)>> {
    let guest = guest().await;
    if let Err(errno) = guest.open_cell(CellId { tenant: 0, id: child }).await {
        log::info!("opening {child}: {errno:?}");
        return None;
    }
    let mut read = Vec::new();
    for sequence in sequences {
        let extent = cladefs::ExtentId { cell: of, sequence };
        read.push((sequence, guest.read(extent, 0, 4096).await));
    }
    guest.close();
    Some(read)
}

/// The blocks of `read` that are not `wanted`'s: read otherwise, or, if `failing`, not at all.
/// While the buckets fail, a read's failure is none of a block's loss; one read is as written
/// always.
fn misread(
    read: &[(u64, Result<Vec<u8>, cladefs::Errno>)],
    wanted: &Blocks,
    failing: bool,
) -> Vec<u64> {
    let wrong = |(sequence, block): &&(u64, Result<Vec<u8>, cladefs::Errno>)| match block {
        Ok(block) => wanted.get(sequence) != Some(block),
        Err(_) => !failing,
    };
    read.iter().filter(wrong).map(|(sequence, _)| *sequence).collect()
}

/// Writes a block of `round`'s to the child `child` on this host and commits it uploaded: its
/// sequence and the block, once told uploaded.
async fn write_own(child: u64, round: u64) -> Option<(u64, Vec<u8>)> {
    let guest = guest().await;
    let cell = guest.open_cell(CellId { tenant: 0, id: child }).await.ok()?;
    let sequence = guest.next_sequence().await.ok()?;
    let block = block_of(3000 + round, sequence);
    guest.write(cladefs::ExtentId { cell, sequence }, 0, &block).await.ok()?;
    guest.commit(cladefs::Commit::Uploaded).await.ok()?;
    guest.close();
    Some((sequence, block))
}

/// What `work` gave on `node`, or none if its host was killed under it.
async fn on<T: Send + 'static>(
    node: NodeHandle,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    match node.spawn(work).await {
        Ok(done) => Some(done),
        Err(ended) if ended.is_cancelled() => None,
        Err(ended) => panic!("{:?}: {ended}", node.id()),
    }
}

/// A region's collector's node, which sweeps the region's bucket as `cladefs-gc` does.
fn collector_of(region: u16) -> NodeHandle {
    let handle = madsim::runtime::Handle::current();
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 110 + region as u8));
    handle.create_node().name(format!("collector{region}")).ip(ip).build()
}

/// A sweep of `cladefs-gc`'s over region `region`'s bucket, on this node: the bucket opened by its
/// URL, routed, the other regions' found in the registry; every cell of it passed over, and the
/// copies it keeps of other regions' cells' versions.
async fn sweep(region: u16) -> Swept {
    let resolve = Arc::new(Registered::from_env());
    let bucket = Arc::new(resolve.open_url(&url_of(region)).unwrap().routed(resolve.clone()));
    let sweep = Sweep { mode: Mode::Delete, at_once: 4, lease: 60_000, slowest: 0, spawned: true };
    let limits = Limits { refused_in_a_row: Some(8), ..Limits::default() };
    let own = bucket.own();
    let mut swept = sweep.run_limited(&bucket, Some(&own), None, &limits, |_| {}).await;
    // As `cladefs-gc` does after a sweep that did all it was to: the probes' objects left behind.
    if swept.complete() {
        match bucket_cell::bucket::reap_probes(&*own).await {
            Ok(reaped) => swept.report.deleted += reaped,
            Err(refused) => {
                log::info!("collector {region}: the probes' objects left behind: {refused}");
                swept.failed += 1;
            }
        }
    }
    if swept.report.deleted + swept.report.retired + swept.failed > 0 {
        log::info!("collector {region}: {:?}, failed {}", swept.report, swept.failed);
    }
    swept
}

/// Sweeps every region's bucket, one after another, until two rounds in a row delete and retire
/// nothing, each sweep complete: fails if eight rounds still do something.
async fn sweep_until_quiet(collectors: &[NodeHandle]) -> Result<(), String> {
    let mut quiet = 0;
    for _ in 0..8 {
        let mut did = 0;
        for (region, collector) in (1u16..).zip(collectors) {
            let swept = collector.spawn(sweep(region)).await.unwrap();
            if !swept.complete() {
                return Err(format!("region {region}'s sweep, the buckets whole: {swept:?}"));
            }
            did += swept.report.deleted + swept.report.retired;
        }
        quiet = if did == 0 { quiet + 1 } else { 0 };
        if quiet == 2 {
            return Ok(());
        }
    }
    Err("8 rounds of the regions' collectors alone, which kept doing something".to_owned())
}

/// What a run of a case reached, by what it is called, counted: so that a set of seeds that never
/// reaches one of them fails, rather than passing having checked nothing of it.
type Reached = BTreeMap<&'static str, usize>;

/// What a set of seeds of [`mirrored`] must reach ([`Reached`]).
const OUTCOMES: [&str; 11] = [
    // A child read a block it inherits with no marker of its version in its bucket, before the
    // read and after: from its version's cell's bucket, which its reads go to until then.
    "read before its marker",
    // A child, taken by a host that never had it, read blocks no host read before and committed,
    // its version's cell's bucket out of reach: from its own bucket.
    "read from its own bucket",
    // A version's mirror marked.
    "marked",
    // Two versions of one cell, sharing objects, marked in one bucket.
    "marked, sharing objects",
    // Two runs of one version's mirror into one bucket under way at once.
    "raced",
    // A host killed at a step of its mirror, or of its fork's leaving its version's cell's bucket.
    "crashed at a step",
    // A host stopped at such a step for longer than a pin stands, then going on.
    "stalled past its lease",
    // A marker's slot taken over by another run, its owner's pin stale.
    "taken over",
    // A version pinned for its mirror, and never marked: dropped, or its cell deleted, under it.
    "given up",
    // A copy collected, its cell and its children gone, before the end.
    "collected",
    // Nothing of any mirror left, every cell deleted.
    "torn down",
];

/// What the journals of `world`'s buckets say of mirrors, added to `reached`: markers marked,
/// runs racing, slots taken over, mirrors of `unnamed`'s cells given up, copies collected.
fn journalled(world: &Regions, unnamed: &BTreeSet<CellId>, reached: &mut Reached) {
    let mut count = |outcome: &'static str, by: usize| *reached.entry(outcome).or_default() += by;
    for region in &world.regions {
        // Of each version mirrored here: its runs' pins standing, each with when it was last
        // written, and whether it was marked.
        let mut pins: BTreeMap<(CellId, u64), BTreeMap<String, u64>> = BTreeMap::new();
        let mut owners: BTreeMap<(CellId, u64), u64> = BTreeMap::new();
        let (mut pinned, mut marked) = (BTreeSet::new(), BTreeSet::new());
        let mut collected = BTreeSet::new();
        for (_, now, key, landed) in region.journal.landed() {
            match (layout::kind(&key), landed) {
                (Kind::Pin { parent, at }, Landed::Put(_)) => {
                    pinned.insert((parent, at));
                    let standing = pins.entry((parent, at)).or_default();
                    // Another run's pin standing, and not stale: its run under way, as a pin
                    // written again, refreshed, or one a dead host left is not.
                    let fresh = |written: &u64| now < written + bucket_cell::mirror::PIN_STALE;
                    let racing =
                        standing.iter().any(|(other, written)| *other != key && fresh(written));
                    if standing.insert(key, now).is_none() && racing {
                        count("raced", 1);
                    }
                }
                (Kind::Pin { parent, at }, Landed::Deleted) => {
                    pins.entry((parent, at)).or_default().remove(&key);
                }
                (Kind::Mirror { cell, at }, Landed::Put(Some(MirrorState::Copying { owner }))) => {
                    if owners.insert((cell, at), owner).is_some_and(|was| was != owner) {
                        count("taken over", 1);
                    }
                }
                (Kind::Mirror { cell, at }, Landed::Put(Some(MirrorState::Marked))) => {
                    if marked.insert((cell, at)) {
                        count("marked", 1);
                    }
                }
                (Kind::Mirror { cell, at }, Landed::Deleted) => {
                    owners.remove(&(cell, at));
                    collected.insert(cell);
                }
                (Kind::Segment { cell, .. } | Kind::Checkpoint { cell, .. }, Landed::Deleted)
                    if layout::routed_to(&key).is_some_and(|of| of != region.number) =>
                {
                    collected.insert(cell);
                }
                _ => {}
            }
        }
        let given_up = pinned.difference(&marked).filter(|(cell, _)| unnamed.contains(cell));
        count("given up", given_up.count());
        count("collected", collected.len());
        let cells: BTreeSet<CellId> = marked.iter().map(|&(cell, _)| cell).collect();
        let sharing = |cell: &&CellId| marked.iter().filter(|(of, _)| of == *cell).count() > 1;
        count("marked, sharing objects", cells.iter().filter(sharing).count());
    }
}

/// What the journals of `world`'s buckets say went wrong, read together in the order things landed
/// (`MarkersWhole`, `MarkedHeld`, of `specs/buckets`, at every moment and not at a check's alone):
///
/// - a marker marked with no hold of its bucket's standing in its cell's bucket, or its version's
///   checkpoint not copied, or an object of its cell's copy deleted since its run claimed its slot
///   (`mirrorHolds`, `markerAfterCopy`, `marksOwn`, `mirrorPins`);
/// - an object of a copy deleted while a marker of its cell stood marked there, which a read sent
///   there for it then finds gone (`markerGoesFirst`): a collector deletes a copy whole, its
///   markers first.
fn journal_broken(world: &Regions) -> Vec<String> {
    let mut landed: Vec<(u64, u16, String, Landed)> = Vec::new();
    for region in &world.regions {
        let own = region.journal.landed().into_iter();
        landed.extend(own.map(|(at, _, key, landed)| (at, region.number, key, landed)));
    }
    landed.sort_by_key(|(at, ..)| *at);
    let mut broken = Vec::new();
    // By bucket: the versions marked, when each marker's slot was last claimed, when an object of
    // each cell's copy was last deleted, and the holds and copies' checkpoints there.
    let mut marked: BTreeSet<(u16, CellId, u64)> = BTreeSet::new();
    let mut claimed: BTreeMap<(u16, CellId, u64), u64> = BTreeMap::new();
    let mut deleted: BTreeMap<(u16, CellId), u64> = BTreeMap::new();
    let mut there: BTreeSet<(u16, String)> = BTreeSet::new();
    for (order, region, key, landed) in landed {
        let copied = layout::routed_to(&key).is_some_and(|of| of != region);
        match (layout::kind(&key), landed) {
            (Kind::Hold { .. }, Landed::Put(_)) => {
                there.insert((region, key));
            }
            (Kind::Hold { .. }, Landed::Deleted) => {
                there.remove(&(region, key));
            }
            (Kind::Checkpoint { .. }, Landed::Put(_)) if copied => {
                there.insert((region, key));
            }
            (
                Kind::Segment { cell, .. }
                | Kind::Extent { cell, .. }
                | Kind::Checkpoint { cell, .. },
                Landed::Deleted,
            ) if copied => {
                if marked.iter().any(|&(of, parent, _)| (of, parent) == (region, cell)) {
                    broken.push(format!(
                        "@{region}/{key}: deleted, a marker of its cell's marked there"
                    ));
                }
                deleted.insert((region, cell), order);
                there.remove(&(region, key));
            }
            (Kind::Mirror { cell, at }, Landed::Put(Some(MirrorState::Copying { .. }))) => {
                claimed.insert((region, cell, at), order);
            }
            (Kind::Mirror { cell, at }, Landed::Put(Some(MirrorState::Marked))) => {
                marked.insert((region, cell, at));
                let home = layout::routed_to(&cell.prefix());
                let held = there.iter().any(|(of, key)| {
                    Some(*of) == home
                        && matches!(
                            layout::kind(key),
                            Kind::Hold { parent, at: held, bucket }
                                if (parent, held, bucket) == (cell, at, region)
                        )
                });
                let checkpointed = there.iter().any(|(of, key)| {
                    *of == region
                        && matches!(
                            layout::kind(key),
                            Kind::Checkpoint { cell: of, segment, .. } if (of, segment) == (cell, at)
                        )
                });
                let since = claimed.get(&(region, cell, at)).copied().unwrap_or(0);
                let short = deleted.get(&(region, cell)).is_some_and(|&gone| gone > since);
                if !held || !checkpointed || short {
                    broken.push(format!(
                        "@{region}/{key}: marked: its hold standing: {held}; its checkpoint \
                         copied: {checkpointed}; an object of its cell's copy deleted since its \
                         run claimed it: {short}"
                    ));
                }
            }
            (Kind::Mirror { cell, at }, Landed::Deleted) => {
                marked.remove(&(region, cell, at));
            }
            _ => {}
        }
    }
    broken
}

/// What a case does to its mirrors as they go, besides its buckets' faults.
#[derive(Clone, Copy, Debug)]
struct Plan {
    /// Whether a host is stopped at a step of its mirror ([`STEPS`]): killed there and started
    /// again, or stalled there for longer than a pin stands, then going on.
    stops: bool,
    /// Whether versions are dropped, and their cells deleted, as their mirrors go.
    drops: bool,
    /// Whether a region's whole bucket is out of reach for stretches.
    cuts: bool,
}

impl Plan {
    const ALL: Self = Self { stops: true, drops: true, cuts: true };
}

/// What a round does to its parent once it is forked, as the seed says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Nothing: it lives on, its version named.
    Kept,
    /// Its version dropped.
    Dropped,
    /// Its cell deleted.
    Deleted,
    /// Its cell deleted, and the children forked of it this round after it.
    DeletedWithChildren,
}

/// The steps a round may stop a host at, by their failpoints: a fork's of a version
/// (`lifecycle::fork_version`, in its order, `version::unmarked` its reading of no marker before
/// it refers in its version's cell's bucket, `version::rechecked` its leaving that bucket for its
/// own), and a mirror's (`bucket_cell::mirror`, in its order).
const STEPS: [&str; 15] = [
    "version::recorded",
    "version::unmarked",
    "version::referred",
    "version::checked",
    "version::rechecked",
    "version::made",
    "mirror::pinned",
    "mirror::held",
    "mirror::checked",
    "mirror::copying",
    "mirror::copied",
    "mirror::refreshed",
    "mirror::marked",
    "mirror::moved",
    "mirror::referred",
];

/// Has the first of `hosts`' nodes to come to `step` stop there, as a host's process stopped does
/// (a VM stalled, a `SIGSTOP`): what it awaits next it goes on with once resumed, or never, killed.
/// The node, once one stopped. A fault aimed at the protocol's state, the daemon's own code
/// running on from where it stood; that it is no BUGGIFY site's is hotbed question 619.
fn stop_at(
    step: &'static str,
    hosts: Vec<madsim::task::NodeId>,
) -> Arc<Mutex<Option<madsim::task::NodeId>>> {
    let stopped: Arc<Mutex<Option<madsim::task::NodeId>>> = Arc::default();
    let at = stopped.clone();
    // The failpoints are the process's, and madsim numbers every simulation's nodes alike: this
    // simulation's hosts alone, by its thread.
    let simulation = std::thread::current().id();
    let stop = move || {
        if std::thread::current().id() != simulation {
            return;
        }
        let node = madsim::plugin::node();
        let mut at = at.lock().unwrap();
        if at.is_none() && hosts.contains(&node) {
            madsim::runtime::Handle::current().pause(node);
            *at = Some(node);
        }
    };
    fail::cfg_callback(step, stop).unwrap();
    stopped
}

/// The case (board 14521): three cloud regions, two hosts each. In rounds, a parent made in one
/// region, a version of it forked by both hosts of another at once, whose mirrors of it race;
/// then, as the seed and `plan` say: a host of theirs stopped at a step of its mirror, killed
/// there and started again, or stalled past its pin's lease; the version dropped, or its cell
/// deleted, its children after it or not; a second version made, sharing the first's objects, and
/// forked; a region's whole bucket out of reach a while; the children reading what they inherit
/// and committing meanwhile, each region's collector sweeping. Then the buckets heal, and:
///
/// - every child reads every block it inherits, and its own told uploaded;
/// - every mirror begun ends: a version its cell's head names still, a child of it living in
///   another region, is marked there, its children referring there and no more in its cell's
///   bucket, none of its copy short (`MarkersWhole`, `MirroredChildrenOnB`, `HoldsMarked`);
/// - its cell's bucket out of reach, each child of a marked version is taken by a host that never
///   had it, reads blocks no host read before, and commits;
/// - every cell deleted, the collectors leave no copy, hold, pin or marker.
///
/// What it lost, or what it reached.
async fn mirrored(plan: Plan) -> Result<Reached, String> {
    use madsim::rand::Rng as _;
    const ROUNDS: u64 = 4;
    const REGIONS: u16 = 3;
    let draw = |odds: f64| madsim::rand::thread_rng().gen_bool(odds);
    let within = |most: u64| Duration::from_millis(madsim::rand::thread_rng().gen_range(0..most));
    let world = Regions::boot(vec![Kms::trusting(); usize::from(REGIONS)], FAULTY).await;
    let manager = manager();
    let handle = madsim::runtime::Handle::current();
    let mut reached = Reached::new();
    let collectors: Vec<NodeHandle> = (1..=REGIONS).map(collector_of).collect();
    // How long a pin stands, and a while more: what a mirror stopped waits to be begun again.
    let stale = Duration::from_millis(bucket_cell::mirror::PIN_STALE) + Duration::from_secs(30);
    // The collectors' sweeps meanwhile, each region's on its own, until told to stop: a sweep
    // stopped midway would hold its cells for its lease.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sweeping: Vec<_> = (1..=REGIONS)
        .zip(&collectors)
        .map(|(region, collector)| {
            let stop = stop.clone();
            collector.spawn(async move {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let after = madsim::rand::thread_rng().gen_range(500..4000);
                    madsim::time::sleep(Duration::from_millis(after)).await;
                    sweep(region).await;
                }
            })
        })
        .collect();
    let synced = |host: u8, cell: u64| {
        let syncing = move || async move {
            reach(host).await?.sync_cell(SyncCellRequest { tenant: 0, cell }).await
        };
        let synced = manager.spawn(async move { asked(host, 120, syncing).await });
        async move { matches!(synced.await.unwrap(), Some(Ok(_))) }
    };
    let mut parents: Vec<Parent> = Vec::new();
    let mut children: Vec<Child> = Vec::new();
    // The children told deleted.
    let mut gone: BTreeSet<u64> = BTreeSet::new();
    // The tasks that mend the regions cut, which the heal aborts.
    let mut mending: Vec<madsim::task::JoinHandle<()>> = Vec::new();
    for round in 0..ROUNDS {
        let from = (round % u64::from(REGIONS)) as u16 + 1;
        let into = (from + madsim::rand::thread_rng().gen_range(0..REGIONS - 1)) % REGIONS + 1;
        let source = hosts_of(from)[madsim::rand::thread_rng().gen_range(0..2)];
        let made = on(world.node(source), cut_version(None, round, BLOCKS)).await.flatten();
        let Some((cell, version, mut blocks)) = made else {
            log::info!("round {round}: no parent made");
            continue;
        };
        if !synced(source, cell).await {
            log::info!("round {round}: its parent's version not uploaded");
            continue;
        }
        let parent = Parent { cell, host: source };
        parents.push(parent);
        // As the seed says: one of the two regions' whole buckets out of reach a while.
        if plan.cuts && draw(0.4) {
            let cut = world.region(if draw(0.5) { from } else { into });
            let (faults, mended) = (cut.faults.clone(), within(8000));
            cut.cut();
            mending.push(madsim::task::spawn(async move {
                madsim::time::sleep(mended).await;
                faults.set(Odds::NONE);
                faults.set(FAULTY);
            }));
        }
        // As the seed says: the first of the forking hosts to come to a step stops there.
        let [first, second] = hosts_of(into);
        let stopping = (plan.stops && draw(0.7)).then(|| {
            let step = STEPS[madsim::rand::thread_rng().gen_range(0..STEPS.len())];
            let hosts = [first, second].map(|host| world.nodes[usize::from(host) - 1].id());
            (step, stop_at(step, hosts.to_vec()), draw(0.5))
        });
        // Both hosts of the other region fork the version at once: each begins its mirror.
        let forks: Vec<_> = [first, second]
            .map(|host| (host, manager.spawn(fork_on(host, cell, version)), blocks.clone()))
            .into();
        let mut stopped = None;
        if let Some((step, at, _)) = &stopping {
            for _ in 0..100 {
                stopped = *at.lock().unwrap();
                if stopped.is_some() {
                    break;
                }
                madsim::time::sleep(Duration::from_millis(100)).await;
            }
            fail::remove(step);
            log::info!("round {round}: stopped at {step}: {stopped:?}");
        }
        // What the forks made so far: one whose host is stopped in it waits for its host.
        let mut forked = Vec::new();
        let mut waiting = Vec::new();
        for (host, mut fork, inherits) in forks {
            match madsim::time::timeout(Duration::from_secs(20), &mut fork).await {
                Ok(made) => {
                    if let Some(child) = made.unwrap()? {
                        forked.push(children.len());
                        children.push(adopted(&world, parent, host, child, inherits).await?);
                    }
                }
                Err(_) => waiting.push((host, fork, inherits)),
            }
        }
        let stalled = matches!((stopped, &stopping), (Some(_), Some((_, _, false))));
        // As the seed says: its version dropped, or its cell deleted, as its mirrors go, its
        // children after it or not: the likelier a mirror stalled, whose copy is then collected
        // under it.
        let fate = match (plan.drops, madsim::rand::thread_rng().gen_range(0..6)) {
            (true, 0) => Fate::Dropped,
            (true, 1) => Fate::Deleted,
            (true, 2) => Fate::DeletedWithChildren,
            (true, 3 | 4) if stalled => Fate::DeletedWithChildren,
            _ => Fate::Kept,
        };
        if fate != Fate::Kept {
            madsim::time::sleep(within(2000)).await;
            let ending = manager.spawn(async move {
                match fate {
                    Fate::Dropped => drop_on(source, cell, version).await,
                    _ => delete_on(source, cell).await,
                }
            });
            ending.await.unwrap();
        }
        let running = |host: &u8| Some(world.nodes[usize::from(*host) - 1].id()) != stopped;
        if fate == Fate::DeletedWithChildren {
            for &child in &forked {
                let (host, cell) = (children[child].host, children[child].cell);
                let deleted = match running(&host) {
                    true => manager.spawn(delete_on(host, cell)).await.unwrap(),
                    // Its host stopped: deleted as an operator deletes it.
                    false => {
                        let seen = &*world.region(into).seen;
                        let deleted =
                            bucket_cell::lifecycle::delete(seen, children[child].id(), 1, u64::MAX);
                        deleted.await.map_err(|err| format!("deleting {cell}: {err}"))?;
                        true
                    }
                };
                if deleted {
                    gone.insert(cell);
                }
            }
        }
        // As the seed says: the version forked once more, a while after, as its mirror goes or
        // is done: by a host that runs.
        if draw(0.4) {
            madsim::time::sleep(within(3000)).await;
            let host = shuffled(vec![first, second]).into_iter().find(running).unwrap();
            waiting.push((host, manager.spawn(fork_on(host, cell, version)), blocks.clone()));
        }
        // As the seed says: a second version of it, sharing the first's objects, forked by a host
        // of the same region.
        if fate == Fate::Kept && draw(0.5) {
            let more = on(world.node(source), cut_version(Some(cell), round, MORE)).await.flatten();
            if let Some((_, second_version, added)) = more
                && synced(source, cell).await
            {
                blocks.extend(added);
                let host = [first, second].into_iter().find(running).unwrap();
                let fork = manager.spawn(fork_on(host, cell, second_version));
                waiting.push((host, fork, blocks.clone()));
            }
        }
        // The host stopped: killed, and started again a while after; or going on, once its pin
        // stood stale a while, which another run of its mirror takes over meanwhile.
        if let (Some(node), Some((_, _, killed))) = (stopped, &stopping) {
            if *killed {
                node_local::kill(node).await;
                madsim::time::sleep(within(3000)).await;
                node_local::restart(node).await;
                *reached.entry("crashed at a step").or_default() += 1;
            } else {
                madsim::time::sleep(stale).await;
                handle.resume(node);
                *reached.entry("stalled past its lease").or_default() += 1;
            }
        }
        for (host, fork, inherits) in waiting {
            let Some(child) = fork.await.unwrap()? else { continue };
            forked.push(children.len());
            children.push(adopted(&world, parent, host, child, inherits).await?);
        }
        // The children there are read what they inherit, one of the blocks read as the case goes,
        // and commit one of their own, as the seed says.
        for child in children.iter_mut().filter(|child| !gone.contains(&child.cell)) {
            let here = region_of(child.host);
            if draw(0.6) {
                let lazily = child.lazily();
                let sequence = lazily[madsim::rand::thread_rng().gen_range(0..lazily.len())];
                let unmarked = !world.marked(here, child.origin).await;
                let (cell, of) = (child.cell, child.of.cell);
                let reading = read_blocks(cell, of, vec![sequence]);
                if let Some(Some(read)) = on(world.node(child.host), reading).await {
                    let misread = misread(&read, &child.inherits, true);
                    if !misread.is_empty() {
                        return Err(format!(
                            "round {round}: child {cell} read block {misread:?} of {of} otherwise"
                        ));
                    }
                    let read = read.iter().all(|(_, block)| block.is_ok());
                    if read && unmarked && !world.marked(here, child.origin).await {
                        *reached.entry("read before its marker").or_default() += 1;
                    }
                }
            }
            if draw(0.5) {
                let cell = child.cell;
                let wrote = on(world.node(child.host), write_own(cell, round)).await.flatten();
                child.told.extend(wrote);
            }
        }
        if fate == Fate::DeletedWithChildren {
            for &child in &forked {
                let (host, cell) = (children[child].host, children[child].cell);
                if !gone.contains(&cell) && manager.spawn(delete_on(host, cell)).await.unwrap() {
                    gone.insert(cell);
                }
            }
        }
    }

    // Healed: every bucket whole, the sweeps under way ended.
    for task in mending {
        task.abort();
    }
    for region in &world.regions {
        region.mend(Odds::NONE);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for sweeping in sweeping {
        sweeping.await.unwrap();
    }
    let living = |children: &[Child], gone: &BTreeSet<u64>| -> Vec<usize> {
        (0..children.len()).filter(|&at| !gone.contains(&children[at].cell)).collect()
    };

    // Every child reads what it inherits, of the blocks read as the case went, and its own told
    // uploaded: where it is, opened again, which begins its version's mirror if none stands.
    for at in living(&children, &gone) {
        let child = &children[at];
        let (cell, of, own) = (child.cell, child.of.cell, child.told.keys().copied().collect());
        let read =
            read_whole(&world, child.host, cell, [(of, child.lazily()), (cell, own)]).await?;
        let lost =
            [misread(&read[0], &child.inherits, false), misread(&read[1], &child.told, false)];
        if lost.iter().any(|lost| !lost.is_empty()) {
            return Err(format!(
                "child {cell} of {of}: blocks it inherits lost or other: {:?}; its own told \
                 uploaded: {:?}",
                lost[0], lost[1]
            ));
        }
    }

    // Every mirror begun ends. One stopped by its host's crash is begun again once its pin is
    // stale: by its region's collector, or its child's next opening.
    madsim::time::sleep(stale).await;
    sweep_until_quiet(&collectors).await?;
    for at in living(&children, &gone) {
        let (host, cell) = (children[at].host, children[at].cell);
        read_whole(&world, host, cell, [(cell, Vec::new())]).await?;
    }
    // The versions their cells' heads name still, by the regions their children live in; and the
    // parents a version forked of which is named no more.
    let mut named = BTreeSet::new();
    let mut unnamed = BTreeSet::new();
    for child in &children {
        let head = world.head(child.of.id()).await.filter(|head| !head.deleted());
        let names = head.is_some_and(|head| head.versions.iter().any(|v| v.at == child.origin.at));
        if !names {
            unnamed.insert(child.of.id());
        } else if !gone.contains(&child.cell) {
            named.insert((region_of(child.host), child.origin.parent, child.origin.at));
        }
    }
    let mut unmarked = named.clone();
    for _ in 0..300 {
        let mut still = BTreeSet::new();
        for &(region, parent, at) in &unmarked {
            if !world.marked(region, layout::Origin { parent, at, epoch: 0 }).await {
                still.insert((region, parent, at));
            }
        }
        unmarked = still;
        if unmarked.is_empty() {
            break;
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
    }
    if !unmarked.is_empty() {
        return Err(format!(
            "versions named still, children of theirs living in other regions, never marked \
             mirrored there (region, cell, version's end): {unmarked:?}"
        ));
    }
    // The mirrors that marked them done: their children's references moved.
    madsim::time::sleep(Duration::from_secs(30)).await;
    sweep_until_quiet(&collectors).await?;
    let left = world.uncollected().await;
    if !left.is_empty() {
        return Err(format!("the mirrors done, the collectors quiet: {left:#?}"));
    }
    // Each child of a version named and marked refers in its own bucket, and no more in its
    // version's cell's (`MirroredChildrenOnB`).
    for at in living(&children, &gone) {
        let child = &children[at];
        let (home, here) = (region_of(child.of.host), region_of(child.host));
        if !named.contains(&(here, child.origin.parent, child.origin.at)) {
            continue;
        }
        let reference = layout::reference(child.origin.parent, child.origin.at, child.id());
        let refers = |region: u16| {
            let seen = world.region(region).seen.clone();
            let reference = reference.clone();
            async move { bucket_cell::bucket::head(&*seen, &reference).await.unwrap().is_some() }
        };
        let (there, own) = (refers(home).await, refers(here).await);
        if there || !own {
            return Err(format!(
                "child {} of a version named and marked in region {here}: referring in its \
                 version's cell's bucket: {there}; in its own: {own}",
                child.cell
            ));
        }
    }

    // Its cell's bucket out of reach, each child of a marked version is taken by the host of its
    // region that never had it, which reads there the blocks no host read before, and commits.
    let mut token = 1000;
    for home in 1..=REGIONS {
        let mut cut = false;
        for at in living(&children, &gone) {
            let here = region_of(children[at].host);
            if region_of(children[at].of.host) != home
                || !world.marked(here, children[at].origin).await
            {
                continue;
            }
            if !std::mem::replace(&mut cut, true) {
                world.region(home).cut();
            }
            let other = hosts_of(here).into_iter().find(|&host| host != children[at].host).unwrap();
            let (cell, of) = (children[at].cell, children[at].of.cell);
            token += 1;
            let taken = token;
            let alone = async {
                manager.spawn(async move { take(other, cell, taken).await }).await.unwrap();
                let own = children[at].told.keys().copied().collect();
                let wanted = [(of, children[at].unread()), (cell, own)];
                let read = read_whole(&world, other, cell, wanted).await?;
                let wrote = on(world.node(other), write_own(cell, ROUNDS)).await.flatten();
                Ok::<_, String>((read, wrote))
            };
            let alone = madsim::time::timeout(Duration::from_secs(300), alone).await;
            let (read, wrote) = alone.map_err(|_| {
                format!(
                    "child {cell} of {of}'s version, marked mirrored in region {here}: taken, \
                     read and committed by host {other} not within 300 s, region {home}'s bucket \
                     out of reach"
                )
            })??;
            let lost = [
                misread(&read[0], &children[at].inherits, false),
                misread(&read[1], &children[at].told, false),
            ];
            if lost.iter().any(|lost| !lost.is_empty()) || wrote.is_none() {
                return Err(format!(
                    "child {cell} of {of}, region {home}'s bucket out of reach: blocks it \
                     inherits lost or other: {:?}; its own: {:?}; its commit told uploaded: {}",
                    lost[0],
                    lost[1],
                    wrote.is_some()
                ));
            }
            children[at].host = other;
            children[at].told.extend(wrote);
            *reached.entry("read from its own bucket").or_default() += 1;
        }
        world.region(home).mend(Odds::NONE);
    }
    journalled(&world, &unnamed, &mut reached);

    // Torn down: every cell deleted, by the host that holds it.
    let mut told = Vec::new();
    for at in living(&children, &gone) {
        let (host, cell) = (children[at].host, children[at].cell);
        manager.spawn(delete_on(host, cell)).await.unwrap();
        told.push(children[at].id());
    }
    for parent in &parents {
        let (host, cell) = (parent.host, parent.cell);
        manager.spawn(delete_on(host, cell)).await.unwrap();
        told.push(parent.id());
    }
    for cell in told {
        if world.head(cell).await.is_some_and(|head| !head.deleted()) {
            return Err(format!("{cell}: deleted by its host, as it was told, and living"));
        }
    }
    // And those no host knows: a fork's child whose host was killed before it answered, made or
    // being made still, which the VM manager's retry left for another cell (board 14559), deleted
    // as an operator deletes it (`cladefs-gc --delete-cell`).
    for region in &world.regions {
        for meta in bucket_cell::bucket::list(&*region.seen, "").await.unwrap() {
            let Kind::Head { cell } = layout::kind(&meta.key) else { continue };
            if layout::routed_to(&meta.key) != Some(region.number) {
                continue;
            }
            let head = bucket_cell::head::get(&*region.seen, cell).await.unwrap();
            if head.is_some_and(|head| !head.deleted()) {
                log::info!("{cell}: no host's, deleted as an operator deletes it");
                let deleted = bucket_cell::lifecycle::delete(&*region.seen, cell, 1, u64::MAX);
                deleted.await.map_err(|err| format!("deleting {cell}: {err}"))?;
            }
        }
    }
    // Its hosts gone, and a week and more: what a sweep leaves for a while, a mirror's stale pin,
    // read evidence and a probe's own object, it leaves no more.
    for node in &world.nodes {
        node_local::kill(node.id()).await;
    }
    let kept = bucket_cell::evidence::KEPT_FOR + bucket_cell::bucket::PROBE_KEPT;
    madsim::time::sleep(Duration::from_millis(kept) + stale).await;
    sweep_until_quiet(&collectors).await?;
    let mut left = world.uncollected().await;
    for region in &world.regions {
        let of_mirrors = world.of_mirrors(region.number).await;
        left.extend(of_mirrors.into_iter().map(|key| format!("@{}/{key}", region.number)));
    }
    if !left.is_empty() {
        return Err(format!("every cell deleted, the collectors quiet, left behind: {left:#?}"));
    }
    let broken = journal_broken(&world);
    if !broken.is_empty() {
        return Err(format!("the buckets' journals: {broken:#?}"));
    }
    *reached.entry("torn down").or_default() += 1;
    Ok(reached)
}

/// The child `child`, told forked on host `host` of `parent`'s version, inheriting `inherits`: with
/// the version its head names.
async fn adopted(
    world: &Regions,
    parent: Parent,
    host: u8,
    child: u64,
    inherits: Blocks,
) -> Result<Child, String> {
    let id = CellId { tenant: 0, id: child };
    let origin = world.head(id).await.and_then(|head| head.origin());
    let origin = origin.ok_or(format!("{id}, told forked on host {host}: no fork's head"))?;
    Ok(Child { cell: child, host, of: parent, origin, inherits, told: Blocks::new() })
}

/// What a guest of host `host`'s read of `child`, opened: each of `wanted`'s extents, a cell's and
/// its sequences. Again if its host starts anew meanwhile, or the child opens not yet: fails if it
/// never opens.
async fn read_whole<const N: usize>(
    world: &Regions,
    host: u8,
    child: u64,
    wanted: [(u64, Vec<u64>); N],
) -> Result<[Vec<(u64, Result<Vec<u8>, cladefs::Errno>)>; N], String> {
    for _ in 0..60 {
        let wanted = wanted.clone();
        let reading = async move {
            let guest = guest().await;
            guest.open_cell(CellId { tenant: 0, id: child }).await.ok()?;
            let mut read = Vec::new();
            for (of, sequences) in wanted {
                let mut blocks = Vec::new();
                for sequence in sequences {
                    let extent = cladefs::ExtentId { cell: of, sequence };
                    blocks.push((sequence, guest.read(extent, 0, 4096).await));
                }
                read.push(blocks);
            }
            guest.close();
            Some(read)
        };
        if let Some(Some(read)) = on(world.node(host), reading).await {
            return Ok(read.try_into().expect("each of the wanted read"));
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("child {child} never opened on host {host}"))
}

/// Board 14521: [`mirrored`], everything it does to its mirrors at once, as the seed says.
#[madsim::test]
async fn versions_forked_across_regions_are_mirrored_and_read_there() {
    match mirrored(Plan::ALL).await {
        Ok(reached) => eprintln!("reached: {reached:?}"),
        Err(lost) => panic!("{lost}"),
    }
}

/// What a seed's process of [`mirrors_across_regions_needs_every_outcome_reached`] says on its
/// standard output before what it reached, on a line.
const REACHED: &str = "cladefs-madsim-regions reached: ";

/// Board 14521: the regions' case reaches what it checks, over its seeds: each of [`OUTCOMES`] in
/// one seed at least of [`REACHING`], as a case no seed takes to a mirror raced, a host crashed
/// copying, a copy read with its cell's bucket out of reach, checks nothing of them and passes. A
/// seed that fails fails it too. Each seed a process of its own, as the rules' searches' are
/// ([`searched`]), and run once, as they are (`_needs_`, scripts/test.sh).
#[test]
fn mirrors_across_regions_needs_every_outcome_reached() {
    /// How many seeds it runs.
    const REACHING: u64 = 40;
    /// How long a seed may take, in real time: its hosts wait out a mirror's pin, ten minutes of
    /// theirs, twice.
    const SEED_TIME: Duration = Duration::from_secs(600);
    let name = "regions::mirrors_across_regions_needs_every_outcome_reached";
    if let Ok(seed) = std::env::var(SEARCH_SEED) {
        let seed = seed.parse().expect("a seed is a number");
        let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
        match runtime.block_on(mirrored(Plan::ALL)) {
            Ok(reached) => {
                let reached: Vec<String> =
                    reached.iter().map(|(outcome, count)| format!("{outcome}={count}")).collect();
                println!("{REACHED}{}", reached.join(";"));
            }
            Err(lost) => println!("{LOST}{lost}"),
        }
        return;
    }
    let binary = std::env::current_exe().unwrap();
    let mut reached = BTreeMap::<String, usize>::new();
    for seed in 1..=REACHING {
        let said = searched(&binary, name, seed, SEED_TIME)
            .unwrap_or_else(|| panic!("seed {seed} ran past {SEED_TIME:?}"));
        if let Some((_, lost)) = said.split_once(LOST) {
            panic!("seed {seed}: {}", lost.lines().next().unwrap_or_default());
        }
        let (_, line) = said
            .split_once(REACHED)
            .unwrap_or_else(|| panic!("seed {seed} said nothing of what it reached: {said}"));
        for outcome in line.lines().next().unwrap_or_default().split(';') {
            let (outcome, count) = outcome.split_once('=').expect("an outcome and its count");
            *reached.entry(outcome.to_owned()).or_default() += count.parse::<usize>().unwrap();
        }
    }
    eprintln!("reached over {REACHING} seeds: {reached:?}");
    for outcome in OUTCOMES {
        assert!(reached.get(outcome).is_some_and(|&count| count > 0), "no seed: {outcome}");
    }
}

/// The peers' counts of this host's daemon, as its metrics say them: the parts its peers gave it,
/// and those it served them.
async fn peer_parts() -> (u64, u64) {
    let metrics = metrics().await;
    let given = metric(&metrics, "cladefsd_peer_parts_total{outcome=\"given\"}");
    let served = metric(&metrics, "cladefsd_peer_served_parts_total{outcome=\"served\"}");
    (given, served)
}

/// Board 14521, TASK-633.4: peers serve parts within their cloud region alone, whatever a host's
/// list of them says. Two regions, two hosts each, every host told all four as its peers. A cell
/// of region 1's, taken by its second host, is read there; a version of it forked in region 2,
/// taken by its second host, is read there: each region's hosts are given parts by their peers,
/// which serve parts of either bucket, and every part a region's hosts are given one of that
/// region's hosts served, none another's.
#[madsim::test]
async fn peers_serve_parts_within_their_cloud_region_alone() {
    let world = Regions::boot(vec![Kms::trusting(); 2], Odds::NONE).await;
    let manager = manager();
    // Blocks enough that some of their parts' owners are the readers' peers, whatever the seed:
    // an owner is a host of the region, the reader itself as often as its peer.
    let made = on(world.node(1), cut_version(None, 0, 4 * BLOCKS)).await.flatten();
    let (cell, version, blocks) = made.expect("a parent made");
    manager
        .spawn(async move {
            let syncing = || async move {
                connect(1).await.sync_cell(SyncCellRequest { tenant: 0, cell }).await
            };
            calling(1, syncing).await.unwrap();
        })
        .await
        .unwrap();
    let child = manager
        .spawn(fork_on(3, cell, version))
        .await
        .unwrap()
        .unwrap()
        .expect("its version forked");
    let sequences: Vec<u64> = blocks.keys().copied().collect();
    // Each taken by its region's other host, which has nothing of it, and read there whole: its
    // parts asked of their owners among its peers.
    for (host, opened) in [(2, cell), (4, child)] {
        manager.spawn(async move { take(host, opened, 100).await }).await.unwrap();
        let read = read_whole(&world, host, opened, [(cell, sequences.clone())]).await.unwrap();
        assert_eq!(misread(&read[0], &blocks, false), Vec::<u64>::new(), "host {host}");
    }
    let mut parts = Vec::new();
    for host in 1..=4 {
        parts.push(on(world.node(host), peer_parts()).await.unwrap());
    }
    eprintln!("the parts each host was given, and served: {parts:?}");
    for region in 1..=2 {
        let of = hosts_of(region).map(|host| parts[usize::from(host) - 1]);
        let (given, served) = (of[0].0 + of[1].0, of[0].1 + of[1].1);
        assert_eq!(given, served, "region {region}: its hosts given, and what its hosts served");
        assert!(served > 0, "region {region}'s hosts served no part: {parts:?}");
    }
}

/// Board 14521, board 8525: a KMS URL the registry of buckets records reaches what a host trusts
/// alone, on the daemon's own path. Two regions, each host's own KMS a `local://` one
/// (`CLADEFS_KMS`, trusted as its own configuration), which its region's entry records. A version
/// of region 1's cell forks on region 2's first host, which allows recorded `local://` KMSes
/// (`CLADEFSD_KMS_LOCAL`), and not on its second, which allows none (`EACCES`, the reason in the
/// daemon's log): each host's own environment's say. Their own region's cells, under their own KMS,
/// both make and fork.
///
/// What a recorded URL may reach besides (`CLADEFSD_KMS_HOSTS`, the GCP projects, the AWS
/// accounts) no simulated daemon tells from its absence: a KMS at a host not allowed and one
/// allowed and not there both refuse the call, and the simulation has no KMS's server.
#[madsim::test]
async fn recorded_kms_urls_reach_what_the_host_trusts_alone() {
    let kmses = vec![Kms::trusting(), Kms { local: [true, false] }];
    let world = Regions::boot(kmses, Odds::NONE).await;
    let manager = manager();
    let mut versions = Vec::new();
    for host in [1, 3] {
        let made = on(world.node(host), cut_version(None, 0, BLOCKS)).await.flatten();
        let (cell, version, _) =
            made.unwrap_or_else(|| panic!("host {host} made no cell under its own KMS"));
        manager
            .spawn(async move {
                let syncing = || async move {
                    connect(host).await.sync_cell(SyncCellRequest { tenant: 0, cell }).await
                };
                calling(host, syncing).await.unwrap();
            })
            .await
            .unwrap();
        versions.push((cell, version));
    }
    let fork = |host: u8, (cell, version): (u64, u64)| {
        manager.spawn(async move {
            let request = ForkCellRequest { version, cell, ..ForkCellRequest::default() };
            let forked = connect(host).await.fork_cell(request).await;
            forked.map(drop).map_err(|status| cladefs::cladefsd::control::errno(&status))
        })
    };
    // Their own region's, under their own KMS.
    for host in [3, 4] {
        assert_eq!(fork(host, versions[1]).await.unwrap(), Ok(()), "host {host}, its own region's");
    }
    // Region 1's, its KMS recorded `local://`: by the host that allows it alone.
    assert_eq!(
        fork(3, versions[0]).await.unwrap(),
        Ok(()),
        "host 3 allows recorded local:// KMSes"
    );
    let refused = Err(Some(cladefs::Errno::EACCES));
    assert_eq!(fork(4, versions[0]).await.unwrap(), refused, "host 4 allows none");
}
