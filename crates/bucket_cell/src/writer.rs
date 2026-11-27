// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A cell's writer: its commits, in segments, uploaded in parallel and acknowledged in order.
//!
//! A writer takes the cell's head ([`head`], doc-7, The head; TASK-407), swapping it to the
//! epoch after its last, and recovers the state it names, anew or on from what it found before
//! ([`Writer::open_recovered`]). Its segments are uploaded at keys naming its epoch, numbered on
//! from the head's end, each written once; it acknowledges them, once they and every one before
//! them are uploaded, by swapping the head to name them, with the condition that the head is still
//! the one it last wrote: its taking's. A writer taken over fails at that swap, and stops for good
//! ([`Writer::stopped`], [`Error::Fenced`]): what it uploaded since, no head names, nor ever will.
//! Only a writer taking the cell anew goes on with it.
//!
//! Its segments' footers are stamped by its chain ([`Writer::chain`]): those a local cell closed,
//! framed, which it uploads as they are (a cell's closed segment is the object, doc-7, decision
//! 1), and those of batches of commits it is given ([`SegmentBuilder`](crate::SegmentBuilder))
//! alike. It takes each one way ([`Writer::add_segment`]), admitting it only if its chain would
//! have stamped it so.
//!
//! The bytes of the objects it uploads are read only as their uploads go ([`Body`]), within a
//! budget it may share with other writers, a host's ([`Writer::uploading_within`]), and held only
//! while they go: what a host's writers hold at once is bounded, however far behind they are.
//!
//! cladefs: a writer holds its cell's ring ([`crate::keys::Ring`], TASK-411.3): it recovers the
//! cell reading under it, and encrypts its fences and authenticates its checkpoints under its
//! cell's newest DEK; the segments it is given come encrypted by their cell.

use crate::bucket::{self, Bucket, Created};
use crate::fetch::{Budget, Limits, Priority};
use crate::head::{self, Held};
use crate::keys::Ring;
use crate::layout::{self, CellId, Chain, Footer, Location};
use crate::recovery::{self, Recovered, State};
use crate::wal::TakenCell;
use crate::{EPOCH_SHIFT, Error, rules, trace};
use futures::{StreamExt as _, TryStreamExt as _};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use storage_device::ExtentId;

/// How many of a segment's extents of their own upload at once, within the writer's budget, which
/// bounds what they hold: a guest's sparse extents make thousands of little objects a commit, and
/// four at once, 60 a second at 15 ms a PUT, held commits up for minutes as they reached the bound
/// on their lag (GCE r353, pgbench's checkpoint at shutdown: TASK-142).
const OWN_AT_ONCE: usize = 32;

/// The bytes of an object a writer uploads, read only as its upload goes, within the writer's
/// budget, and held only while it does.
#[derive(Clone)]
pub struct Body {
    /// About how many bytes it holds once read: what the budget counts.
    len: u64,
    read: Arc<dyn Fn() -> std::io::Result<Vec<u8>> + Send + Sync>,
}

impl Body {
    /// The bytes `read` reads, about `len` of them.
    pub fn new(
        len: u64,
        read: impl Fn() -> std::io::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        Self { len, read: Arc::new(read) }
    }

    /// Its bytes.
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        (self.read)()
    }
}

impl From<Vec<u8>> for Body {
    /// `bytes`, held: each read copies them.
    fn from(bytes: Vec<u8>) -> Self {
        let bytes: Arc<[u8]> = bytes.into();
        Self::new(bytes.len() as u64, move || Ok(bytes.to_vec()))
    }
}

/// A segment's objects, as a writer uploads them ([`Writer::add_segment`]): its footer, which its
/// bytes end with; the segment's; and those of the extents its footer says are stored on their
/// own, by their keys, which go first. cladefs: and the length of the segment's data, framed
/// (`extent_frame::Frame::data_len`), which its checkpoints keep ([`State::lengths`]).
pub struct Objects {
    pub footer: Footer,
    pub segment: Body,
    pub own: Vec<(String, Body)>,
    pub data_len: u64,
}

pub struct Writer<B: Bucket> {
    bucket: Arc<B>,
    cell: CellId,
    epoch: u64,
    /// What its uploads may hold at once, with those of the writers it shares it with.
    budget: Arc<Budget>,
    /// What it found taking the cell over: the head as it took it.
    taken: TakenCell,
    /// The head as it last swapped it, which its next swap is conditional on: one at a time.
    held: futures::lock::Mutex<Held>,
    inner: Mutex<Inner>,
    /// cladefs: the keys it reads and encrypts under (TASK-411.3).
    keys: Arc<Ring>,
    /// cladefs: the decoded state its recovery started from, if any ([`Recovered::layer`]), which
    /// its state's map shares, as the cell opened with its state does (TASK-453).
    layer: Option<Arc<State>>,
}

struct Inner {
    /// As of the last segment acknowledged.
    state: State,
    /// Where its next segment goes: its number, the segment it follows, the last it took, and the
    /// cell's next sequence number as of that.
    chain: Chain,
    /// The segments taken and not yet acknowledged.
    pending: BTreeMap<u64, Pending>,
    /// The newest checkpoint, its own or the one the head named as it took it.
    checkpoint: Option<u64>,
    /// Why it stopped, if it did.
    stopped: Option<Error>,
    /// cladefs: the versions dropped, which no acknowledgement names, local ones among them, cut
    /// by segments not yet acknowledged (TASK-476).
    dropped: BTreeSet<u64>,
    /// cladefs: a fast WAL head update (`head.fast_wal`) applied atomically in the same head swap
    /// that acknowledges `through >= through_segment` ([`Writer::activate_fast_wal_past`], doc-29).
    fast_wal_switch: Option<(u64, Option<layout::FastWal>)>,
    /// cladefs: own-extent keys already uploaded directly to the cold bucket by the fast WAL
    /// large-write optimization (doc-29); skipped by [`Writer::upload_segment`] for zero double
    /// upload.
    preuploaded_own: BTreeSet<String>,
    /// cladefs: own-extent keys currently being pre-uploaded by [`Writer::put_large_extent`]
    /// (doc-29); [`Writer::upload_segment`] awaits any matching entry so the fast WAL writer and
    /// cold uploader never race to PUT the same large extent concurrently.
    preuploading_own: BTreeMap<String, Arc<tokio::sync::Notify>>,
}

/// What an acknowledgement names ([`Writer::naming`]): the last segment, the versions cut at the
/// ends of those it names, each with its segment, and the fast-WAL switch if this acknowledgement
/// crosses its target.
struct Naming {
    through: u64,
    cuts: Vec<(u64, layout::NewVersion)>,
    fast_wal_switch: Option<Option<layout::FastWal>>,
}

struct Pending {
    footer: Footer,
    /// The length of its data ([`Objects::data_len`]).
    data_len: u64,
    /// Its object, and those of its extents stored on their own, uploaded first, until they are
    /// uploaded.
    objects: Option<(Body, Vec<(String, Body)>)>,
}

impl<B: Bucket> Writer<B> {
    /// Takes `cell` over: its head, as `holder`, which must differ from every other taking's, of
    /// the placement whose fencing token is `token`, given the keys it lacks
    /// ([`crate::keys::taking`]): its KEK unwrapped by `kms`, the host's; then its state, recovered
    /// from it once it holds it. What takes a cell outside the daemon: tools, tests.
    pub async fn open_under(
        bucket: Arc<B>,
        cell: CellId,
        (holder, token): (u64, u64),
        kms: &Arc<crate::keys::CachingKms>,
    ) -> Result<Self, Error> {
        let read = head::read(&*bucket, cell).await?;
        let taking = head::Taking { holder, token, ..head::Taking::default() };
        let (held, keys) = crate::keys::taking(&*bucket, kms, cell, read, taking).await?;
        let ring = crate::keys::ring(&*bucket, &keys, cell, &held.head, true).await?;
        Self::holding(bucket, cell, held, None, ring).await
    }

    /// [`Writer::open_under`] the tests' KMS (`keys::testing`): every test's writer encrypts
    /// (TASK-411.28).
    #[cfg(any(test, feature = "testing"))]
    pub async fn open(
        bucket: Arc<B>,
        cell: CellId,
        holder: u64,
        token: u64,
    ) -> Result<Self, Error> {
        let kms = crate::keys::testing::host_kms();
        Self::open_under(bucket, cell, (holder, token), &kms).await
    }

    /// The writer of `cell`, whose head its taking took, `held`, holding `keys`, its cell's ring
    /// (TASK-411.3): its state recovered from the head it took, anew, or on from `before`, a
    /// recovery of it made before the taking ([`recovery::recover_on`]), which reads only what came
    /// since (TASK-165.11).
    pub async fn holding(
        bucket: Arc<B>,
        cell: CellId,
        held: Held,
        before: Option<Recovered>,
        keys: Arc<Ring>,
    ) -> Result<Self, Error> {
        let epoch = held.head.epoch;
        let (origin, named) = (held.head.origin(), Some(held.head.clone()));
        let recovered = match before {
            None => recovery::recover_from(&*bucket, cell, origin, named, &keys).await,
            Some(before) => recovery::recover_on(&*bucket, origin, before, named, &keys).await,
        };
        let Recovered { state, layer, .. } = recovered?;
        // Its sequences minted from its epoch's range, past every earlier writer's: what it
        // recovered is what the head it took names, no later writer's segment among it.
        let floor = if rules().epoch_sequences { epoch << EPOCH_SHIFT } else { 0 };
        let chain = Chain::new(cell, epoch, held.head.end, state.next_sequence.max(floor));
        let taken = TakenCell::of(held.head.clone(), &state, chain);
        let checkpoint = held.head.checkpoint.map(|(segment, _)| segment);
        let writer = Self {
            bucket,
            cell,
            epoch,
            budget: Budget::new(Limits::UNBOUNDED),
            taken,
            held: futures::lock::Mutex::new(held),
            inner: Mutex::new(Inner {
                state,
                chain,
                pending: BTreeMap::new(),
                checkpoint,
                stopped: None,
                dropped: BTreeSet::new(),
                fast_wal_switch: None,
                preuploaded_own: BTreeSet::new(),
                preuploading_own: BTreeMap::new(),
            }),
            keys,
            layer,
        };
        Ok(writer)
    }

    /// Uploads within `budget`, which it shares with other writers, a host's: each object read only
    /// once the budget has room for it, and held only while it goes. Alone, a writer's uploads are
    /// bounded only by how many at once it is asked for.
    pub fn uploading_within(self, budget: Arc<Budget>) -> Self {
        Self { budget, ..self }
    }

    /// The upload memory budget shared by this writer and its fast WAL path (doc-29).
    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    /// The bucket it writes to: for tests.
    #[cfg(test)]
    pub(crate) fn bucket(&self) -> &B {
        &self.bucket
    }

    pub fn cell(&self) -> CellId {
        self.cell
    }

    /// cladefs: the keys it reads and encrypts under (TASK-411.3).
    pub fn keys(&self) -> &Arc<Ring> {
        &self.keys
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// What it found taking the cell over, the head as it took it: what a host's write-ahead
    /// log is put after ([`Wal::rebase`](crate::Wal::rebase)), before the writer takes a segment.
    pub fn taken(&self) -> &TakenCell {
        &self.taken
    }

    /// cladefs: the decoded state its recovery started from, if it started from one, whose map of
    /// the extents its state shares ([`Recovered::layer`], TASK-453).
    pub fn layer(&self) -> Option<Arc<State>> {
        self.layer.clone()
    }

    /// The cell's state as of the last segment acknowledged.
    pub fn state(&self) -> State {
        self.inner().state.clone()
    }

    /// The tag of the cell's last commit, as of the last segment acknowledged.
    pub fn last_commit(&self) -> Option<u64> {
        self.inner().state.last_commit
    }

    /// Where `extent` is, as of the last segment acknowledged, if the cell holds it.
    pub fn location(&self, extent: ExtentId) -> Option<Location> {
        self.inner().state.extents.get(&extent).cloned()
    }

    /// Where the writer's next segment goes: its chain, as of the last segment it took, which
    /// stamps the footer of the next, a cell's or a batch's ([`Writer::add_segment`]). The
    /// cell's next sequence number it names is what the guest mints from.
    pub fn chain(&self) -> Chain {
        self.inner().chain
    }

    /// Why the writer stopped, if it did.
    pub fn stopped(&self) -> Option<Error> {
        self.inner().stopped.clone()
    }

    fn stop(&self, error: Error) -> Error {
        self.inner().stopped.get_or_insert(error).clone()
    }

    /// Takes a segment, `objects`, as the next to upload ([`Writer::upload`]): one a cell closed,
    /// or a segment builder's ([`SegmentBuilder`](crate::SegmentBuilder)), closed on the writer's
    /// chain. Its footer must be the next the chain would have stamped ([`Chain::admit`]), and the
    /// objects of the extents it says are stored on their own must come with it, by their keys.
    /// Returns its number.
    pub fn add_segment(&self, objects: Objects) -> Result<u64, Error> {
        let Objects { footer, segment: framed, own, data_len } = objects;
        let mut inner = self.inner();
        if let Some(error) = &inner.stopped {
            return Err(error.clone());
        }
        let chain = inner.chain.admit(&footer)?;
        let keys: BTreeSet<&String> = own.iter().map(|(key, _)| key).collect();
        for (extent, location) in footer.own() {
            if !keys.contains(&location.key) {
                return Err(Error::Corrupt(format!("{extent:?} is stored at {}", location.key)));
            }
        }
        inner.chain = chain;
        let segment = footer.segment;
        let pending = Pending { footer, data_len, objects: Some((framed, own)) };
        inner.pending.insert(segment, pending);
        Ok(segment)
    }

    /// Uploads segment `segment`: its extents stored on their own, a few at once, then it, its
    /// bytes ending with the footer it was taken with, each at a key naming the writer's epoch,
    /// which no other writer writes. cladefs: in a span of its own, `upload.segment`, its bucket's
    /// requests children of it (`crate::trace`, TASK-258).
    pub async fn upload(&self, segment: u64) -> Result<(), Error> {
        let span = trace::Span::start("upload.segment");
        span.set_cell(self.cell);
        span.set_count(trace::SEGMENT, segment);
        let uploaded = span.around(self.upload_segment(segment, &span)).await;
        span.result(&uploaded);
        uploaded
    }

    /// Uploads segment `segment`, as [`Writer::upload`] says, saying its extents and bytes on
    /// `span`.
    async fn upload_segment(&self, segment: u64, span: &trace::Span) -> Result<(), Error> {
        let Some((footer, framed, own)) = self.inner().pending.get(&segment).and_then(|pending| {
            let (framed, own) = pending.objects.clone()?;
            Some((pending.footer.clone(), framed, own))
        }) else {
            return Ok(());
        };
        // Upload own-extents via `create_own_once`, which coordinates with concurrent Fast WAL
        // pre-uploads in either order (fast-WAL-first or cold-first) so each key is created once.
        span.set_count(trace::EXTENTS, own.len() as u64);
        let bytes = framed.len + own.iter().map(|(_, body)| body.len).sum::<u64>();
        span.set_count(trace::BYTES, bytes);
        let creates = futures::stream::iter(own)
            .map(|(key, body)| async move { self.create_own_once(&key, body).await });
        // cladefs: in a simulation, one at a time, as a host short of connections would (doc-24).
        let at_once = if crate::buggify!(0.5) { 1 } else { OWN_AT_ONCE };
        if let Err(error) = creates.buffer_unordered(at_once).try_collect::<()>().await {
            return Err(self.stop(error));
        }
        let key = layout::segment(self.cell, segment, self.epoch);
        let taken = |bytes: &[u8]| {
            let keys = self.keys.of(self.cell.id);
            match layout::Segment::footer_under(bytes, &keys)? == footer {
                true => Ok(()),
                false => Err(Error::Corrupt(format!("{key} ends with another footer than taken"))),
            }
        };
        failpoint!("writer::upload");
        match self.create(&key, &framed, taken).await {
            Err(error) => Err(self.stop(error)),
            Ok(Created::Created(_)) => {
                failpoint!("writer::uploaded");
                // Its objects, uploaded, go. `preuploaded_own` entries stay until `acknowledge`
                // retires the segment so a late Fast WAL pre-upload for the same segment never
                // repeats a completed PUT.
                let mut inner = self.inner();
                if let Some(pending) = inner.pending.get_mut(&segment) {
                    pending.objects = None;
                }
                Ok(())
            }
            // Another object at its key, which only this writer writes: a key written twice.
            Ok(Created::Exists(_)) => {
                Err(self.stop(Error::Corrupt(format!("{key} holds another segment"))))
            }
        }
    }

    /// Creates own-extent `key` at most once across concurrent cold segment uploads and Fast WAL
    /// large-extent pre-uploads (doc-29): reserves `key` in `preuploading_own` with a drop guard so
    /// neither cold-first nor fast-WAL-first uploads can race on the same key.
    async fn create_own_once(&self, key: &str, body: Body) -> Result<(), Error> {
        struct InFlightGuard<'a, B: Bucket> {
            writer: &'a Writer<B>,
            key: String,
            notify: Arc<tokio::sync::Notify>,
        }
        impl<B: Bucket> Drop for InFlightGuard<'_, B> {
            fn drop(&mut self) {
                self.writer.inner().preuploading_own.remove(&self.key);
                self.notify.notify_waiters();
            }
        }
        let notify = loop {
            let wait = {
                let mut inner = self.inner();
                if let Some(error) = &inner.stopped {
                    return Err(error.clone());
                }
                if inner.preuploaded_own.contains(key) {
                    return Ok(());
                }
                if let Some(notify) = inner.preuploading_own.get(key) {
                    Some(notify.clone())
                } else {
                    let notify = Arc::new(tokio::sync::Notify::new());
                    inner.preuploading_own.insert(key.to_owned(), notify.clone());
                    break notify;
                }
            };
            if let Some(notify) = wait {
                let notified = notify.notified();
                if self.inner().preuploading_own.contains_key(key) {
                    notified.await;
                }
            }
        };
        let _guard = InFlightGuard { writer: self, key: key.to_owned(), notify };
        // Always validate the framed own-extent's part CRCs and structure before uploading so
        // neither cold upload nor Fast WAL pre-upload can ever publish damaged data (board 12295).
        let check = |bytes: &[u8]| {
            extent_frame::check(bytes).map_err(|err| Error::Corrupt(format!("{key}: {err}")))
        };
        match self.create(key, &body, check).await? {
            Created::Created(_) => {
                self.inner().preuploaded_own.insert(key.to_owned());
                Ok(())
            }
            Created::Exists(_) => Err(Error::Corrupt(format!("{key} holds another extent"))),
        }
    }

    /// cladefs: uploads a large own-extent directly to the cold bucket before referencing it
    /// metadata-only in the fast WAL (doc-29, `largeBeforeWal`), and records `key` so the later
    /// cold segment upload skips re-uploading it (zero double upload).
    pub async fn put_large_extent(&self, key: &str, bytes: Vec<u8>) -> Result<(), Error> {
        self.create_own_once(key, Body::from(bytes)).await.map_err(|err| self.stop(err))
    }

    /// cladefs: swaps `head.fast_wal` immediately to `fast_wal` (doc-29), used when a writer
    /// activates its fast WAL without needing to flush recovered segments first.
    pub async fn activate_fast_wal(&self, fast_wal: Option<layout::FastWal>) -> Result<(), Error> {
        if let Some(error) = &self.inner().stopped {
            return Err(error.clone());
        }
        let mut held = self.held.lock().await;
        if held.head.fast_wal == fast_wal {
            return Ok(());
        }
        let mut next = held.head.clone();
        next.fast_wal = fast_wal;
        match head::swap(&*self.bucket, self.cell, &held, next).await {
            Ok(swapped) => {
                *held = swapped;
                Ok(())
            }
            Err(error) => Err(self.stop(error)),
        }
    }

    /// cladefs: schedules `head.fast_wal = fast_wal` to land atomically in the same head CAS that
    /// acknowledges `through >= through_segment` (doc-29, `activateAfterFlush`).
    pub fn activate_fast_wal_past(&self, through_segment: u64, fast_wal: Option<layout::FastWal>) {
        self.inner().fast_wal_switch = Some((through_segment, fast_wal));
    }

    /// Creates the object at `key` of `body`'s bytes, unless an object is there
    /// ([`bucket::create`]), once `check` passes them: read once the writer's budget has room for
    /// them, and held only until the create is done.
    async fn create(
        &self,
        key: &str,
        body: &Body,
        check: impl FnOnce(&[u8]) -> Result<(), Error>,
    ) -> Result<Created, Error> {
        let _grant = self.budget.enter(Priority::Demand, body.len).1.await;
        let bytes = body.read().map_err(|err| Error::Read(format!("{key}: {err}")))?;
        check(&bytes)?;
        Ok(bucket::create(&*self.bucket, key, bytes).await?)
    }

    /// cladefs: the head as it last swapped it, or took it.
    pub async fn head(&self) -> layout::Head {
        self.held.lock().await.head.clone()
    }

    /// Acknowledges the segments uploaded with every one before them, by swapping the cell's head
    /// to name them, with the condition that it is the one this writer last wrote ([`head`], doc-7,
    /// The head; TASK-407): a writer taken over, or a deleted cell's, fails, and stops. Returns the
    /// last segment acknowledged. cladefs: to the last that goes on with no commit, a commit
    /// spanning segments acknowledged with the segment it ends in, not part of it
    /// (`extent_frame::segment::Continued`, TASK-92.41): what a cell lets go of, once acknowledged,
    /// the bucket has whole, and a head names no commit begun. cladefs: the swap names the versions
    /// cut at the ends of the segments it names too ([`layout::Head::naming`], doc-18, TASK-476): a
    /// version is uploaded once it does. cladefs: one swap names all that is uploaded until the
    /// bucket's backend takes the head's next write ([`Bucket::rewritable`]): a cell's head is
    /// swapped no faster than the backend takes it, GCS about once a second, however many segments
    /// upload meanwhile (TASK-690). cladefs: and the checkpoint this writer landed since the head
    /// last named one, which costs it no swap of its own (doc-27 step 4, [`Writer::checkpoint`]).
    pub async fn acknowledge(&self) -> Result<Option<u64>, Error> {
        if self.naming()?.is_none() {
            return Ok(self.inner().state.through);
        }
        failpoint!("writer::acknowledge");
        // Waited for before the head is held, which its other changes and readers wait for: its
        // store's turn for a write of the head (TASK-690).
        self.bucket.rewritable(&layout::head(self.cell)).await;
        let through = {
            let mut held = self.held.lock().await;
            let Some(Naming { through, cuts, fast_wal_switch }) = self.naming()? else {
                return Ok(self.inner().state.through);
            };
            if held.head.end <= through {
                // cladefs: those not dropped, as of now, holding the head: a drop that took it
                // first, between, found no version of these to unname (TASK-569).
                let named: Vec<_> = {
                    let inner = self.inner();
                    let kept = cuts.iter().filter(|(_, cut)| !inner.dropped.contains(&cut.id));
                    kept.map(|(seq, cut)| (*seq, cut)).collect()
                };
                let mut next = held.head.clone().holding(self.epoch, through + 1).naming(named);
                if let Some(landed) = self.landed(&held.head) {
                    next.checkpoint = Some((landed, self.epoch));
                }
                if let Some(fast_wal) = &fast_wal_switch {
                    next.fast_wal = fast_wal.clone();
                }
                match head::swap(&*self.bucket, self.cell, &held, next).await {
                    Ok(swapped) => *held = swapped,
                    Err(error) => return Err(self.stop(error)),
                }
                if fast_wal_switch.is_some() {
                    self.inner().fast_wal_switch = None;
                }
            }
            through
        };
        {
            let mut inner = self.inner();
            let inner = &mut *inner;
            while let Some(entry) = inner.pending.first_entry() {
                if *entry.key() > through {
                    break;
                }
                let Pending { footer, data_len, .. } = entry.remove();
                for (_, location) in footer.own() {
                    inner.preuploaded_own.remove(&location.key);
                }
                inner.state.apply(&footer, data_len);
            }
        }
        failpoint!("writer::acknowledged");
        Ok(self.inner().state.through)
    }

    /// What an acknowledgement now would name ([`Writer::acknowledge`]): none if no segment is
    /// uploaded to name; the writer's failure if it stopped.
    fn naming(&self) -> Result<Option<Naming>, Error> {
        let inner = self.inner();
        if let Some(error) = &inner.stopped {
            return Err(error.clone());
        }
        let created: Vec<_> =
            (inner.pending.iter()).take_while(|(_, pending)| pending.objects.is_none()).collect();
        let Some(end) =
            created.iter().rposition(|(_, pending)| pending.footer.contents.continued.is_none())
        else {
            return Ok(None);
        };
        let through = *created[end].0;
        // Without the rule, as a test shows it needed: every version taken, those of segments
        // this swap doesn't name among them.
        let cut = |(&seq, pending): (&u64, &Pending)| {
            let cuts = pending.footer.contents.versions.iter();
            cuts.map(move |cut| (seq, *cut)).collect::<Vec<_>>()
        };
        let cuts: Vec<_> = match rules().versions_acked {
            true => inner.pending.range(..=through).flat_map(cut).collect(),
            false => inner.pending.iter().flat_map(cut).collect(),
        };
        let fast_wal_switch = (inner.fast_wal_switch.as_ref())
            .filter(|(target_seg, _)| through >= *target_seg)
            .map(|(_, fast_wal)| fast_wal.clone());
        Ok(Some(Naming { through, cuts, fast_wal_switch }))
    }

    /// cladefs: the checkpoint this writer landed that `head` names not, its newest, if any: one
    /// past the head's, its own (doc-27 step 4).
    fn landed(&self, head: &layout::Head) -> Option<u64> {
        let newest = self.inner().checkpoint?;
        head.checkpoint.is_none_or(|(at, _)| at < newest).then_some(newest)
    }

    /// cladefs: the cell's versions, as its head names them, as this writer last swapped it
    /// (doc-18, TASK-476): those its acknowledgements named, and those named before it took the
    /// cell, but those dropped.
    pub async fn versions(&self) -> Vec<layout::Version> {
        self.held.lock().await.head.versions.clone()
    }

    /// cladefs: drops the versions `ids` names, those the cell's head names of them, and those of
    /// segments it is yet to acknowledge, which its acknowledgements name not: the head
    /// swapped to name them no more, with the condition every swap has (rule `head_swap`), a writer
    /// taken over failing and stopping ([`Error::Fenced`]). The collector's next pass over the
    /// cell keeps their states no more (doc-18, TASK-476). Failing so, they may be dropped
    /// all the same: a swap that took effect, its answer lost, and the head taken over before its
    /// retry found it its own, fails as one that didn't (TASK-499); the head says which.
    pub async fn drop_versions(&self, ids: &BTreeSet<u64>) -> Result<(), Error> {
        self.inner().dropped.extend(ids);
        self.change_versions(|head| head.without(ids)).await
    }

    /// cladefs: shares the version `id`, if `shared`, or no longer, if the cell's head names it:
    /// forks of it may be of other tenants' cells while it is (doc-7, Shared versions). Fails with
    /// [`Error::NoCell`] if the head names no such version.
    pub async fn share_version(&self, id: u64, shared: bool) -> Result<(), Error> {
        if self.held.lock().await.head.version(id).is_none() {
            return Err(Error::NoCell);
        }
        self.change_versions(|head| head.sharing(id, shared)).await
    }

    /// cladefs: the cell's head swapped to what `change` makes of it, its versions alone changed,
    /// unless it makes it the same.
    async fn change_versions(
        &self,
        change: impl FnOnce(layout::Head) -> layout::Head,
    ) -> Result<(), Error> {
        if let Some(error) = &self.inner().stopped {
            return Err(error.clone());
        }
        let mut held = self.held.lock().await;
        let next = change(held.head.clone());
        if next == held.head {
            return Ok(());
        }
        match head::swap(&*self.bucket, self.cell, &held, next).await {
            Ok(swapped) => *held = swapped,
            Err(error) => return Err(self.stop(error)),
        }
        Ok(())
    }

    /// Writes a checkpoint as of the last segment acknowledged, the cell's newest, for the head to
    /// name (TASK-407). cladefs: named by the next acknowledgement, in its swap, while segments are
    /// yet to be acknowledged; by a swap of its own only with none, as an idle cell's or a
    /// closing one's (doc-27 step 4). The collector keeps a checkpoint of the head's epoch it may
    /// yet name ([`crate::Rules::collector_epochs`]), so that naming it late loses nothing.
    /// cladefs: in a span of its own, `upload.checkpoint`, its bucket's requests children of it
    /// (`crate::trace`, TASK-258).
    pub async fn checkpoint(&self) -> Result<Option<u64>, Error> {
        let span = trace::Span::start("upload.checkpoint");
        span.set_cell(self.cell);
        let checkpointed = span.around(self.write_checkpoint(&span)).await;
        span.result(&checkpointed);
        checkpointed
    }

    /// Writes a checkpoint, as [`Writer::checkpoint`] says, saying what it is of on `span`.
    async fn write_checkpoint(&self, span: &trace::Span) -> Result<Option<u64>, Error> {
        let state = {
            let inner = self.inner();
            // A writer stopped names nothing more: its failure, each time.
            if let Some(error) = &inner.stopped {
                return Err(error.clone());
            }
            if inner.state.through.is_none() || inner.state.through <= inner.checkpoint {
                return Ok(inner.checkpoint);
            }
            // cladefs: acknowledged, its last segment ends its commits (TASK-92.41).
            debug_assert!(inner.state.begun.is_none(), "a checkpoint of a commit begun");
            inner.state.clone()
        };
        let segment = state.through.unwrap();
        span.set_count(trace::SEGMENT, segment);
        span.set_count(trace::EXTENTS, state.extents.len() as u64);
        failpoint!("writer::checkpoint");
        let key = layout::checkpoint(self.cell, segment, self.epoch);
        // One this writer wrote before, of the same segment, holds the same extents, perhaps
        // elsewhere: it stands, at its key, written once.
        let (checkpoint, kept) =
            crate::checkpoint::encode_kept(&state, self.keys.try_encrypting()?);
        bucket::create(&*self.bucket, &key, checkpoint).await?;
        {
            // Landed, and decided under the head, which acknowledgements hold as they swap it: a
            // segment taken that the head names not yet has an acknowledgement to come, which names
            // it; with none, this swap does. Those the head names, an acknowledgement lets go of
            // after it lets go of the head.
            let mut held = self.held.lock().await;
            let idle = {
                let mut inner = self.inner();
                inner.checkpoint = inner.checkpoint.max(Some(segment));
                inner.pending.range(held.head.end..).next().is_none()
            };
            if idle && let Some(landed) = self.landed(&held.head) {
                let next =
                    layout::Head { checkpoint: Some((landed, self.epoch)), ..held.head.clone() };
                match head::swap(&*self.bucket, self.cell, &held, next).await {
                    Ok(swapped) => *held = swapped,
                    Err(error) => return Err(self.stop(error)),
                }
            }
        }
        failpoint!("writer::checkpointed");
        let mut inner = self.inner();
        // cladefs: and lets go of those of segments it named none in as of it, which it names
        // none in again: what it applied since, it keeps (doc-27).
        for key in state.lengths.keys().filter(|key| !kept.contains_key(*key)) {
            inner.state.lengths.remove(key);
        }
        Ok(inner.checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rules;
    use crate::batch::{Commit, SegmentBuilder};
    use crate::layout::test_cell;
    use crate::layout::{Contents, NewVersion, Segment};
    use crate::lifecycle;
    use crate::object_bucket::{ObjectBucket, Request};
    use object_store::memory::InMemory;

    fn memory() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""))
    }

    /// Adds a segment of `commit` alone to `writer`, closed on its chain: returns its number.
    fn add(writer: &Writer<ObjectBucket>, commit: Commit) -> Result<u64, Error> {
        writer.add_segment(
            SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting()),
        )
    }

    /// A writer another took the cell from uploads its segment, at its own key, but its swap of
    /// the head fails: it stops, and says why; it takes no segment more (TASK-407).
    #[test]
    fn writers_taken_over_stop() {
        futures::executor::block_on(async {
            let bucket = memory();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let first = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let _second = Writer::open(bucket.clone(), test_cell(1), 11, 0).await.unwrap();
            let segment = add(&first, Commit { tag: 1, ..Commit::default() }).unwrap();
            assert_eq!(first.upload(segment).await, Ok(()));
            assert_eq!(first.acknowledge().await, Err(Error::Fenced));
            assert_eq!(first.stopped(), Some(Error::Fenced));
            assert_eq!(add(&first, Commit { tag: 2, ..Commit::default() }), Err(Error::Fenced));
        });
    }

    /// Adds a segment of an empty commit tagged `tag` to `writer`, closed on its chain, the version
    /// `cut` cut at its end: returns its number.
    fn cut(writer: &Writer<ObjectBucket>, tag: u64, cut: NewVersion) -> Result<u64, Error> {
        let commit = Commit { tag, ..Commit::default() };
        writer.add_segment(
            SegmentBuilder::from(commit)
                .cut(cut, &mut writer.chain())
                .frame(writer.keys().encrypting()),
        )
    }

    fn version(id: u64, keep_last: Option<u32>) -> NewVersion {
        NewVersion { id, keep_last }
    }

    /// The versions `head` names, by their IDs and the segments they are at.
    fn named(head: Option<layout::Head>) -> Vec<(u64, u64)> {
        let versions = head.map(|head| head.versions).unwrap_or_default();
        versions.iter().map(|version| (version.id, version.at)).collect()
    }

    /// TASK-476: a cell's head names a version once the acknowledgement naming the segment
    /// cutting it does, in the same swap; a window drops the oldest as a version asking for one is
    /// named; its writer drops a version by a swap; a takeover names what the head named; a
    /// deletion's final head names none.
    #[test]
    fn heads_name_versions_as_their_segments_are_acknowledged() {
        futures::executor::block_on(async {
            let bucket = memory();
            let cell = test_cell(1);
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let a = cut(&writer, 1, version(0xa, None)).unwrap();
            let b = cut(&writer, 2, version(0xb, None)).unwrap();
            writer.upload(a).await.unwrap();
            writer.acknowledge().await.unwrap();
            assert_eq!(named(head::get(&*bucket, cell).await.unwrap()), [(0xa, a)]);
            writer.upload(b).await.unwrap();
            writer.acknowledge().await.unwrap();
            assert_eq!(named(head::get(&*bucket, cell).await.unwrap()), [(0xa, a), (0xb, b)]);
            let c = cut(&writer, 3, version(0xc, Some(2))).unwrap();
            writer.upload(c).await.unwrap();
            writer.acknowledge().await.unwrap();
            assert_eq!(named(head::get(&*bucket, cell).await.unwrap()), [(0xb, b), (0xc, c)]);
            writer.drop_versions(&BTreeSet::from([0xb, 0xd])).await.unwrap();
            assert_eq!(named(head::get(&*bucket, cell).await.unwrap()), [(0xc, c)]);
            let versions: Vec<_> = writer.versions().await.iter().map(|v| (v.id, v.at)).collect();
            assert_eq!(versions, [(0xc, c)]);
            writer.share_version(0xc, true).await.unwrap();
            assert_eq!(writer.share_version(0xb, true).await, Err(Error::NoCell));
            let shared = head::get(&*bucket, cell).await.unwrap().unwrap();
            assert!(shared.version(0xc).unwrap().shared);

            let next = Writer::open(bucket.clone(), cell, 11, 0).await.unwrap();
            assert_eq!(named(head::get(&*bucket, cell).await.unwrap()), [(0xc, c)]);
            assert_eq!(writer.drop_versions(&BTreeSet::from([0xc])).await, Err(Error::Fenced));
            drop(next);
            lifecycle::delete(&*bucket, cell, 12, 0).await.unwrap();
            let ended = head::get(&*bucket, cell).await.unwrap().unwrap();
            assert_eq!((ended.epoch, named(Some(ended))), (layout::Head::FINAL, vec![]));
        });
    }

    /// TASK-569: an acknowledgement that read the versions to name before a drop of one, then
    /// waited for the head, which the drop took first and found naming none of them, names no
    /// version dropped: the drop, told done, is not undone.
    #[test]
    fn acknowledgements_name_no_version_dropped_while_they_wait_for_the_head() {
        use futures::FutureExt;
        futures::executor::block_on(async {
            let bucket = memory();
            let cell = test_cell(1);
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let a = cut(&writer, 1, version(0xa, None)).unwrap();
            writer.upload(a).await.unwrap();
            let waker = futures::task::noop_waker();
            let mut context = std::task::Context::from_waker(&waker);
            // The head held, as a control call's swap holds it across its write.
            let held = writer.held.lock().await;
            let mut acknowledging = Box::pin(writer.acknowledge());
            assert!(acknowledging.poll_unpin(&mut context).is_pending());
            let dropped = BTreeSet::from([0xa]);
            let mut dropping = Box::pin(writer.drop_versions(&dropped));
            assert!(dropping.poll_unpin(&mut context).is_pending());
            drop(held);
            // The drop takes the head first, which names no version yet.
            dropping.await.unwrap();
            acknowledging.await.unwrap();
            let head = head::get(&*bucket, cell).await.unwrap().unwrap();
            assert_eq!((head.end, named(Some(head))), (a + 1, vec![]));
        });
    }

    /// TASK-476, rule `versions_acked`: an acknowledgement names the versions of the segments it
    /// names alone. Without the rule, it names a version whose segment it does not, at a segment
    /// past the head's end, which no reader of the cell holds.
    #[test]
    fn heads_name_versions_with_their_segments_alone() {
        let (acked, unacked) = (Rules::ALL, Rules { versions_acked: false, ..Rules::ALL });
        let names = |rules: Rules| {
            crate::set_rules(rules);
            let named = futures::executor::block_on(async {
                let bucket = memory();
                let cell = test_cell(1);
                lifecycle::create(&*bucket, cell, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
                let a = cut(&writer, 1, version(0xa, None)).unwrap();
                cut(&writer, 2, version(0xb, None)).unwrap();
                writer.upload(a).await.unwrap();
                writer.acknowledge().await.unwrap();
                let head = head::get(&*bucket, cell).await.unwrap().unwrap();
                (head.end, named(Some(head)))
            });
            crate::set_rules(Rules::ALL);
            named
        };
        assert_eq!(names(acked), (1, vec![(0xa, 0)]));
        assert_eq!(names(unacked), (1, vec![(0xa, 0), (0xb, 1)]), "0xb past the head's end");
    }

    /// TASK-476, rule `versions_carried`: a takeover names the versions the head named. Without
    /// the rule, the head it swaps names none, though no writer dropped them.
    #[test]
    fn takeovers_carry_versions() {
        let carries = |rules: Rules| {
            crate::set_rules(rules);
            let named = futures::executor::block_on(async {
                let bucket = memory();
                let cell = test_cell(1);
                lifecycle::create(&*bucket, cell, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
                let a = cut(&writer, 1, version(0xa, None)).unwrap();
                writer.upload(a).await.unwrap();
                writer.acknowledge().await.unwrap();
                let _next = Writer::open(bucket.clone(), cell, 11, 0).await.unwrap();
                named(head::get(&*bucket, cell).await.unwrap())
            });
            crate::set_rules(Rules::ALL);
            named
        };
        assert_eq!(carries(Rules::ALL), [(0xa, 0)]);
        assert_eq!(carries(Rules { versions_carried: false, ..Rules::ALL }), []);
    }

    /// An extent of its own a writer uploads again is its own; one that finds another's object
    /// at its key stops the writer.
    #[test]
    fn extents_of_their_own_found_uploaded_are_the_writers() {
        futures::executor::block_on(async {
            let bucket = memory();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (again, other) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let again_key = layout::extent(
                layout::TEST_CELL,
                writer.epoch(),
                writer.chain().next_segment(),
                again,
            );
            let own = vec![(again.sequence, b"again".to_vec())];
            let objects = SegmentBuilder::from(Commit { tag: 1, own, ..Commit::default() })
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            // Its bytes, as this writer uploads them, there before: its own, landed though its
            // answer was lost (each framing encrypted under a salt of its own, TASK-411.28).
            let [(key, again_bytes)] = &objects.own[..] else { panic!("one of its own") };
            assert_eq!(key, &again_key);
            bucket::create(&*bucket, &again_key, again_bytes.read().unwrap()).await.unwrap();
            let segment = writer.add_segment(objects).unwrap();
            assert_eq!(writer.upload(segment).await, Ok(()));
            assert_eq!(writer.acknowledge().await, Ok(Some(segment)));

            let other_key = layout::extent(
                layout::TEST_CELL,
                writer.epoch(),
                writer.chain().next_segment(),
                other,
            );
            bucket::create(
                &*bucket,
                &other_key,
                layout::with_footer(
                    b"another",
                    &(),
                    crate::testing::encrypting(layout::test_cell(1)),
                ),
            )
            .await
            .unwrap();
            let own = vec![(other.sequence, b"other".to_vec())];
            let segment = add(&writer, Commit { tag: 2, own, ..Commit::default() }).unwrap();
            let corrupt = Error::Corrupt(format!("{other_key} holds another extent"));
            assert_eq!(writer.upload(segment).await, Err(corrupt.clone()));
            assert_eq!(writer.stopped(), Some(corrupt));
        });
    }

    /// A checkpoint reads nothing: one at a time, each segment's moves read at 25 ms of latency
    /// held a cell of a few minutes' pgbench's uploader up for tens of seconds (TASK-91.2), and
    /// cladefs: the collector makes no moves now (doc-27).
    #[test]
    fn checkpoints_read_nothing() {
        futures::executor::block_on(async {
            let bucket = memory();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            for n in 0..64 {
                let extents = vec![(ExtentId::new(1, first + n), b"segment".to_vec())];
                let segment =
                    add(&writer, Commit { tag: n + 1, extents, ..Commit::default() }).unwrap();
                writer.upload(segment).await.unwrap();
                writer.acknowledge().await.unwrap();
            }
            let before = bucket.stats();
            writer.checkpoint().await.unwrap();
            let after = bucket.stats();
            let made = |request| after.requests(request) - before.requests(request);
            assert_eq!(
                [Request::List, Request::Get, Request::GetRange].map(made),
                [0, 0, 0],
                "the checkpoint's listings, reads and ranged reads"
            );
        });
    }

    /// doc-27 step 4 (board 6220's AC #3): a checkpoint landed while a segment is yet to be
    /// acknowledged costs no swap of its own: the acknowledgement after it names it, in its own
    /// swap. An idle cell's checkpoint, no acknowledgement to come, swaps on its own.
    #[test]
    fn checkpoints_are_named_by_the_acknowledgements_after_them() {
        futures::executor::block_on(async {
            let bucket = memory();
            let cell = test_cell(1);
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let swaps = || bucket.stats().requests(Request::Update);
            let named = || async { head::get(&*bucket, cell).await.unwrap().unwrap() };
            let first = add(&writer, Commit { tag: 1, ..Commit::default() }).unwrap();
            writer.upload(first).await.unwrap();
            writer.acknowledge().await.unwrap();
            let second = add(&writer, Commit { tag: 2, ..Commit::default() }).unwrap();
            let before = swaps();
            assert_eq!(writer.checkpoint().await, Ok(Some(first)));
            assert_eq!((swaps() - before, named().await.checkpoint), (0, None));
            writer.upload(second).await.unwrap();
            writer.acknowledge().await.unwrap();
            assert_eq!(swaps() - before, 1, "the acknowledgement's one swap");
            let head = named().await;
            assert_eq!((head.end, head.checkpoint), (second + 1, Some((first, writer.epoch()))));
            assert_eq!(writer.checkpoint().await, Ok(Some(second)));
            assert_eq!(swaps() - before, 2, "an idle cell's checkpoint's own swap");
            assert_eq!(named().await.checkpoint, Some((second, writer.epoch())));
        });
    }

    /// TASK-79.19: a segment's extents of their own upload a few at once, each holding its bytes
    /// while it goes, rather than all at once: the next goes as one is done.
    #[test]
    fn extents_of_their_own_upload_a_few_at_once() {
        use crate::testing::{Fault, SimBucket};
        use std::task::{Context, Poll};
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        let first = writer.chain().next_sequence();
        let own = (0..2 * OWN_AT_ONCE as u64 + 1).map(|n| (first + n, vec![n as u8; 10])).collect();
        let commit = Commit { tag: 1, own, ..Commit::default() };
        let segment = writer.add_segment(
            SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting()),
        );
        let mut upload = std::pin::pin!(writer.upload(segment.unwrap()));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        // The most creates waiting at once.
        let mut most = 0;
        while let Poll::Pending = upload.as_mut().poll(&mut context) {
            most = most.max(bucket.waiting());
            bucket.complete(0, Fault::None);
        }
        assert_eq!(most, OWN_AT_ONCE);
    }

    /// TASK-79.19: a writer uploading within a budget reads each object only once the budget has
    /// room for it, and gives the room up once the object is created: with room for one, the next
    /// is read only as the one before is done.
    #[test]
    fn uploads_read_their_objects_within_their_budget() {
        use crate::testing::{Fault, SimBucket};
        use std::task::{Context, Poll};
        let bucket = SimBucket::new();
        let (writer, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        let writer = writer.uploading_within(Budget::new(Limits { requests: 1, bytes: u64::MAX }));
        // Two segments, a commit's each, whose reads are noted.
        let reads = Arc::new(Mutex::new(Vec::new()));
        let mut segments = Vec::new();
        for tag in 1..=2 {
            let commit = Commit { tag, ..Commit::default() };
            let Objects { footer, segment, own, data_len } = SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            let reads = reads.clone();
            let segment = Body::new(10, move || {
                reads.lock().unwrap().push(tag);
                segment.read()
            });
            let objects = Objects { footer, segment, own, data_len };
            segments.push(writer.add_segment(objects).unwrap());
        }
        let uploads =
            futures::future::join_all(segments.iter().map(|&segment| writer.upload(segment)));
        let mut uploads = std::pin::pin!(uploads);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut uploading = || match uploads.as_mut().poll(&mut context) {
            Poll::Ready(uploaded) => Some(uploaded),
            Poll::Pending => None,
        };
        assert!(uploading().is_none());
        assert_eq!((reads.lock().unwrap().clone(), bucket.waiting()), (vec![1], 1));
        bucket.complete(0, Fault::None);
        assert!(uploading().is_none());
        assert_eq!((reads.lock().unwrap().clone(), bucket.waiting()), (vec![1, 2], 1));
        bucket.complete(0, Fault::None);
        assert_eq!(uploading(), Some(vec![Ok(()), Ok(())]));
    }

    /// A segment taken with a footer its bytes do not end with stops its writer, which uploads
    /// nothing of it.
    #[test]
    fn segments_ending_with_another_footer_than_taken_stop_their_writers() {
        futures::executor::block_on(async {
            let bucket = memory();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let close = |tag| {
                let commit = Commit { tag, ..Commit::default() };
                SegmentBuilder::from(commit)
                    .close(&mut writer.chain())
                    .frame(writer.keys().encrypting())
            };
            let Objects { footer, own, data_len, .. } = close(1);
            let objects = Objects { footer, segment: close(2).segment, own, data_len };
            let segment = writer.add_segment(objects).unwrap();
            let key = layout::segment(test_cell(1), segment, 1);
            let corrupt = Error::Corrupt(format!("{key} ends with another footer than taken"));
            assert_eq!(writer.upload(segment).await, Err(corrupt.clone()));
            assert_eq!(writer.stopped(), Some(corrupt));
            assert_eq!(bucket.head(&key).await.map(|_| ()), Err(bucket::Error::NotFound));
        });
    }

    /// TASK-92.6: a writer taking over a cell recovered before it took the head recovers it on
    /// from there: it reads the head, takes it, then reads the segments the head names that came
    /// in between (TASK-165.11, TASK-407, TASK-515). Each round a round trip on a real bucket.
    #[test]
    fn writers_open_on_what_was_recovered_before_their_head() {
        use crate::testing::SimBucket;
        let bucket = SimBucket::new();
        let (first, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        let commit = |tag| {
            let objects = SegmentBuilder::from(Commit { tag, ..Commit::default() })
                .close(&mut first.chain())
                .frame(first.keys().encrypting());
            let segment = first.add_segment(objects).unwrap();
            bucket.served(first.upload(segment)).0.unwrap();
            bucket.served(first.acknowledge()).0.unwrap();
        };
        commit(1);
        let before = bucket.served(recovery::recover(&*bucket, test_cell(1), None)).0.unwrap();
        assert_eq!(before.end(), 1);
        // Committed in between.
        commit(2);
        let keys = first.keys().clone();
        let opening = async {
            let read = head::read(&*bucket, test_cell(1)).await.unwrap();
            let held = head::take(&*bucket, test_cell(1), read, &taking(11)).await?;
            Writer::holding(bucket.clone(), test_cell(1), held, Some(before), keys).await
        };
        let (second, rounds) = bucket.served_in_rounds(opening);
        let second = second.unwrap();
        assert_eq!(second.last_commit(), Some(2));
        let tail = |segment| {
            format!(
                "get {} last {}",
                layout::segment(test_cell(1), segment, 1),
                crate::object::TAIL
            )
        };
        // cladefs: the head read, then written in place.
        let head = layout::head(test_cell(1));
        assert_eq!(
            rounds,
            [vec![format!("get {head}")], vec![format!("put {head}")], vec![tail(1)]]
        );
        let anew = bucket.served(recovery::recover(&*bucket, test_cell(1), None)).0.unwrap();
        assert_eq!(second.state(), anew.state);
    }

    /// The taking of `holder`, of token 0, giving the head nothing.
    fn taking(holder: u64) -> head::Taking {
        head::Taking { holder, ..head::Taking::default() }
    }

    /// TASK-165.11: a writer taking over a cell from a read of its head made before takes the head
    /// from that read: its first round the head's update, a round trip less; and from a read gone
    /// stale, another writer having taken the head since, by reading it again once its update
    /// finds it changed, at the epoch after that writer's.
    #[test]
    fn writers_open_on_a_read_of_their_head_made_before() {
        use crate::testing::SimBucket;
        let bucket = SimBucket::new();
        let (first, _) = bucket.served(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap()
        });
        assert_eq!(first.epoch(), 1);
        let found = || {
            let before = bucket.served(recovery::recover(&*bucket, test_cell(1), None)).0.unwrap();
            let read = bucket.served(head::read(&*bucket, test_cell(1))).0.unwrap();
            (before, read)
        };
        let opening = |(before, read): (Recovered, head::Read), holder| {
            let (bucket, keys) = (bucket.clone(), first.keys().clone());
            async move {
                let held = head::take(&*bucket, test_cell(1), read, &taking(holder)).await?;
                Writer::holding(bucket, test_cell(1), held, Some(before), keys).await
            }
        };
        let head = layout::head(test_cell(1));
        let read = found();
        let (second, rounds) = bucket.served_in_rounds(opening(read, 11));
        let second = second.unwrap();
        assert_eq!(second.epoch(), 2);
        assert_eq!(rounds[0], [format!("put {head}")]);
        // A read of the head, then another writer's taking of it, then the opening.
        let read = found();
        let (third, _) = bucket.served(Writer::open(bucket.clone(), test_cell(1), 12, 0));
        assert_eq!(third.unwrap().epoch(), 3);
        let (fourth, rounds) = bucket.served_in_rounds(opening(read, 13));
        assert_eq!(fourth.unwrap().epoch(), 4);
        // Its update refused, its answer read back, as an update whose answer was lost is told
        // apart; then the head read again, and taken.
        let (put, get) = (vec![format!("put {head}")], vec![format!("get {head}")]);
        assert_eq!(rounds[..4], [put.clone(), get.clone(), get, put]);
    }

    /// A checkpoint as of the last one is not written again.
    #[test]
    fn checkpoints_are_written_once() {
        futures::executor::block_on(async {
            let bucket = memory();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let segment = writer.checkpoint().await.unwrap();
            let requests = bucket.stats().requests;
            assert_eq!(writer.checkpoint().await, Ok(segment));
            assert_eq!(bucket.stats().requests, requests);
        });
    }

    /// A writer takes a segment a cell closed for it, and uploads it as it is; one numbered
    /// otherwise, of another writer, or without the objects of its extents of their own at the
    /// keys it names them at, it refuses, and goes on as before.
    #[test]
    fn writers_upload_segments_cells_closed() {
        futures::executor::block_on(async {
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let chain = writer.chain();
            let segment = chain.next_segment();
            let (small, large) = (ExtentId::new(1, 2), ExtentId::new(1, 3));
            let own_key = layout::extent(layout::TEST_CELL, writer.epoch(), segment, large);
            let footer = Footer {
                cell: test_cell(1),
                segment,
                epoch: writer.epoch(),
                contents: Contents {
                    commits: vec![7],
                    extents: vec![(small, 0, 3)],
                    own: vec![(large.sequence, 5)],
                    frees: vec![],
                    root: Some(b"root".to_vec()),
                    next_sequence: 4,
                    capacity: None,
                    continued: None,
                    versions: vec![],
                },
            };
            let encoded = |footer: &Footer| -> Vec<u8> {
                Segment { data: b"abc".to_vec(), footer: footer.clone() }
                    .encode(writer.keys().encrypting())
            };
            let own = layout::own_extent(large, b"large", writer.keys().encrypting());
            let owns = || vec![(own_key.clone(), own.clone().into())];
            let objects = |footer: &Footer, own| {
                let segment = encoded(footer);
                let data_len = extent_frame::Frame::of(&segment).unwrap().data_len;
                Objects { footer: footer.clone(), segment: segment.into(), own, data_len }
            };
            let elsewhere = layout::extent(layout::TEST_CELL, writer.epoch() + 1, segment, large);
            // Its chain's next place: segment 0 of epoch 1, the head's end.
            assert_eq!((writer.epoch(), segment), (1, 0));
            let wanted = format!("{:?}", (test_cell(1), 0, 1));
            let misplaced = |place: &str| {
                Error::Corrupt(format!("a segment of cell 1/1, {place}, not {wanted}"))
            };
            for (wrong, refused) in [
                (Footer { segment: segment + 1, ..footer.clone() }, misplaced("number 1, epoch 1")),
                (
                    Footer { epoch: writer.epoch() + 1, ..footer.clone() },
                    misplaced("number 0, epoch 2"),
                ),
            ] {
                assert_eq!(writer.add_segment(objects(&wrong, owns())), Err(refused), "{wrong:?}");
            }
            let elsewhere = vec![(elsewhere, own.clone().into())];
            let missing = Error::Corrupt(format!("{large:?} is stored at {own_key}"));
            assert_eq!(writer.add_segment(objects(&footer, elsewhere)), Err(missing));
            let missing = Error::Corrupt(format!("{large:?} is stored at {own_key}"));
            assert_eq!(writer.add_segment(objects(&footer, vec![])), Err(missing));
            assert_eq!(writer.chain(), chain);
            // Its bytes, encrypted once, under a salt of their own (TASK-411.28).
            let taken = objects(&footer, owns());
            let ours = taken.segment.read().unwrap();
            assert_eq!(writer.add_segment(taken), Ok(segment));
            let chain = writer.chain();
            assert_eq!(chain.next_segment(), segment + 1);
            writer.upload(segment).await.unwrap();
            assert_eq!(writer.acknowledge().await, Ok(Some(segment)));
            let object =
                bucket::get(&*bucket, &layout::segment(test_cell(1), segment, 1)).await.unwrap();
            assert_eq!(object.map(|object| object.bytes), Some(ours));
            let object = bucket::get(&*bucket, &own_key).await.unwrap();
            assert_eq!(object.map(|object| object.bytes), Some(own));
            let state = writer.state();
            assert_eq!((state.last_commit, state.root), (Some(7), Some(b"root".to_vec())));
            assert_eq!(
                crate::testing::read(&bucket, small, &state.extents[&small]).await.unwrap(),
                b"abc"
            );
            assert_eq!(
                crate::testing::read(&bucket, large, &state.extents[&large]).await.unwrap(),
                b"large"
            );
        });
    }
}
