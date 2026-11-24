// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opening, creating, taking, placing, and deleting [`OpenCell`]s through [`Opening`].

use super::{Arenas, OpenCell};
use crate::cache;
use crate::cell_dir;
use crate::dax::WindowChanges;
use crate::error::{Errno, Error, Result};
use crate::halt::{CellMutex, CellRwLock, Halt};
use crate::host_cell::{self, CellOf};
use crate::pages::Arena;
use bucket_cell::cell::Kind;
use bucket_cell::layout::CellId;
use bucket_cell::trace;
use local_cell::{HostDir, LocalCell};
use std::path::Path;
use std::sync::Arc;

/// cladefs: how an open cell is opened, created, or taken ([`Opening::open`], [`Opening::create`],
/// [`Opening::take`], TASK-165.9).
pub struct Opening {
    /// The arena of physical memory its own extents' pages are of; the default arena's if none.
    pub arena: Option<Arc<Arena>>,
    /// Whether a guest boots from it: of a fork of a version, what its version's read evidence
    /// predicts its guest reads is fetched ahead, and what it reads measured and recorded
    /// (`crate::evidence`, TASK-702.3).
    pub guest: bool,
    /// How its uploader keeps its cell in its bucket, taking its head.
    pub keeping: local_cell::upload::Options,
}

impl Opening {
    /// As this process opens open cells where no host's daemon serves them, as [`crate::Storage`]
    /// does: a copy never placed taking its cell with this host's token, or the first
    /// placement's (`host_cell::keeping`); its pages the default arena's, a guest booting from
    /// it.
    pub fn here() -> Result<Self, Error> {
        let keeping = host_cell::keeping()?;
        Ok(Self { arena: None, guest: true, keeping })
    }

    /// Opens with uploader options `keeping`: the default arena, a guest booting from it.
    pub fn keeping(keeping: local_cell::upload::Options) -> Self {
        Self { arena: None, guest: true, keeping }
    }

    /// Sets the arena of physical memory the open cell's own extents' pages are of (TASK-43).
    pub fn arena(mut self, arena: Option<Arc<Arena>>) -> Self {
        self.arena = arena;
        self
    }

    /// Sets whether a guest boots from the open cell ([`Opening::guest`]).
    pub fn guest(mut self, guest: bool) -> Self {
        self.guest = guest;
        self
    }

    /// Opens for host work that boots no guest: nothing is fetched ahead for it, nor its reads
    /// taken for its version's evidence ([`Opening::guest`], TASK-79.17).
    pub fn for_host(self) -> Self {
        self.guest(false)
    }

    /// Sets how the open cell's uploader keeps its cell in its bucket ([`Opening::keeping`]).
    pub fn with_keeping(mut self, keeping: local_cell::upload::Options) -> Self {
        self.keeping = keeping;
        self
    }

    /// Sets whether taking a replicated cell is forced (`keeping.force`, TASK-681).
    pub fn force(mut self, force: bool) -> Self {
        self.keeping.force = force;
        self
    }

    /// Opens the cell directory at `path`, as of its last commit, as this [`Opening`] configures:
    /// its uploader takes its cell's head, with the token its placement names or `keeping`'s, and
    /// recovers it, the open cell's write-ahead log put after its end (TASK-165.9); a log another
    /// writer's commits moved past refuses (`ESTALE`) until `cladefsctl take` drops it. A tenant's
    /// (TASK-43): the pages of its own extents, and of those it inherited from other cells but
    /// public ones ([`OpenCell::make_public`]), are of `arena`, which it holds, and what reads
    /// into the extent cache for it, until they are gone (TASK-79.36); or of the default arena,
    /// without one. Those of public cells' extents, which every tenant's forks of them share, are
    /// of the default arena.
    ///
    /// cladefs: in a span of its own, `store.open` (`bucket_cell::trace`, TASK-258).
    pub async fn open(self, path: impl AsRef<Path>) -> Result<OpenCell, Error> {
        let path = path.as_ref();
        let span = trace::Span::start("store.open");
        span.set_with(trace::STORE, || path.display().to_string());
        let opened = async {
            // cladefs: an ephemeral cell's, with no uploader (doc-25, TASK-698.3).
            let (ephemeral, migrating) = local_cell::upload::blocking(|| {
                let ephemeral = host_cell::is_ephemeral(path);
                let migrating = ephemeral && host_fs::exists(&path.join("migrating"));
                (ephemeral, migrating)
            });
            let cell = match ephemeral {
                // cladefs: given up to a live migration, it opens here no more (TASK-698.4).
                true if migrating => return Err(Errno::EBUSY.into()),
                true => {
                    let (local, bucket, keys, forked) =
                        host_cell::open_ephemeral(path, &self.keeping).await?;
                    let assembling =
                        OpenCell::assemble(path, (local, bucket, None), Some(forked), self);
                    let mut opened = assembling.await?;
                    opened.keys = Some(keys);
                    opened
                }
                false => {
                    let opened = host_cell::open(path, self.keeping.clone()).await?;
                    OpenCell::new(path, opened, self).await?
                }
            };
            span.set_cell(cell.cell());
            Ok(cell)
        };
        span.run_async(opened).await
    }

    /// Opens the cell directory at `path`, a fork attached to other open cells (backlog doc-10),
    /// to read through to: no guest boots from it ([`Opening::guest`]). Its pages are of `arena`,
    /// or the default arena if it is public, as a public base's are, which every tenant's VM
    /// processes hold read-only.
    pub async fn open_base(mut self, path: impl AsRef<Path>) -> Result<OpenCell, Error> {
        let path = path.as_ref();
        let (public, cell) = local_cell::upload::blocking(|| -> Result<_, Error> {
            Ok((cell_dir::public_cells(path)?, local_cell::host_cell(path).map_err(Error::new)?))
        })?;
        self.arena = self.arena.filter(|_| !public.contains(&cell.id));
        self.guest = false;
        self.open(path).await
    }

    /// Creates a new Fxfs cell directory at `path`, of `capacity` bytes and the cell `of` says,
    /// opened as this [`Opening`] configures (TASK-364, doc-20).
    pub async fn create(
        self,
        path: impl AsRef<Path>,
        capacity: u64,
        of: CellOf,
    ) -> Result<OpenCell, Error> {
        self.create_kind(path, capacity, of, Kind::Fxfs).await
    }

    /// Creates a new cell directory at `path`, of `capacity` bytes, the cell `of` says, and `kind`
    /// (TASK-364), opened as this [`Opening`] configures, in a span of its own (`store.create`,
    /// `bucket_cell::trace`, TASK-258).
    pub async fn create_kind(
        self,
        path: impl AsRef<Path>,
        capacity: u64,
        of: CellOf,
        kind: Kind,
    ) -> Result<OpenCell, Error> {
        let path = path.as_ref();
        OpenCell::creating(path, async |span| {
            let keeping = self.keeping.clone();
            let opened = host_cell::create(path, capacity, of, kind, keeping).await?;
            span.set_cell(opened.local.description().cell);
            OpenCell::new(path, opened, self).await
        })
        .await
    }

    /// Takes `cell` into the cell directory at `path`, of `capacity` bytes until a commit of the
    /// cell's sets one, placed at `token`'s placement if given (doc-14, Opening an open cell): a
    /// new one if nothing is at `path`, which recovers the cell as the bucket holds it; or the one
    /// there, opened, its write-ahead log dropped if another writer's commits moved the cell past
    /// it (`ESTALE` opening it otherwise), the commits it held, never acknowledged, lost (doc-7,
    /// Recovery on the same host; TASK-165.9).
    pub async fn take(
        self,
        path: impl AsRef<Path>,
        capacity: u64,
        cell: CellId,
        token: Option<u64>,
    ) -> Result<OpenCell, Error> {
        let path = path.as_ref();
        // cladefs: an open cell made here is opened once, as made, not made, closed and opened
        // again (TASK-449).
        let existing = local_cell::upload::blocking(|| -> Result<Option<CellId>, Error> {
            if !host_fs::exists(&path) {
                return Ok(None);
            }
            Ok(Some(local_cell::host_cell(path).map_err(Error::new)?))
        })?;
        let opened = if existing.is_none() {
            let kept = self.keeping.clone();
            let of = CellOf::Taken(cell);
            OpenCell::made_placed(path, (capacity, of, token), Kind::Fxfs, kept).await?
        } else if existing != Some(cell) {
            let refused = format!("{} holds another cell than {cell}", path.display());
            return Err(Error::new(anyhow::Error::new(Errno::EEXIST).context(refused)));
        } else {
            if let Some(token) = token {
                local_cell::upload::blocking(|| OpenCell::place(path, token))?;
            }
            host_cell::take(path, self.keeping.clone()).await?
        };
        OpenCell::new(path, opened, self).await
    }

    /// cladefs: a new cell directory at `path`, of `capacity` bytes and `of`, kept in the bucket
    /// at `url`, opened as this [`Opening`] configures: in tests, a memory bucket, which this
    /// process's open cells naming it share.
    #[cfg(test)]
    pub(crate) async fn create_kept(
        self,
        path: impl AsRef<Path>,
        capacity: u64,
        of: CellOf,
        url: &str,
    ) -> Result<OpenCell, Error> {
        let path = path.as_ref();
        let kept = (Kind::Fxfs, Some(url), self.keeping.clone());
        let opened = host_cell::create_unmade(path, capacity, of, kept.0, kept.1, kept.2).await?;
        local_cell::upload::blocking(|| opened.made()).map_err(Error::new)?;
        OpenCell::new(path, opened, self).await
    }
}

impl OpenCell {
    /// cladefs: a new cell directory at `path`, of `capacity` bytes and `(of, kind)` (TASK-364),
    /// opened as `opening` says ([`Opening::create_kind`]).
    pub async fn create_kind_with(
        path: impl AsRef<Path>,
        capacity: u64,
        (of, kind): (CellOf, Kind),
        opening: Opening,
    ) -> Result<Self, Error> {
        opening.create_kind(path, capacity, of, kind).await
    }

    /// cladefs: `make`'s making of a new cell directory at `path`, in a span of its own,
    /// `store.create` (`bucket_cell::trace`, TASK-258), which it names the cell in once it knows
    /// it: an open cell made, opened, or made closed ([`OpenCell::create_placed`]).
    async fn creating<T>(
        path: &Path,
        make: impl AsyncFnOnce(&trace::Span) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let span = trace::Span::start("store.create");
        span.set_with(trace::STORE, || path.display().to_string());
        span.run_async(make(&span)).await
    }

    /// The cell directory at `path`, as of its last commit. cladefs: opened as this process opens
    /// open cells ([`Opening::here`]).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Opening::here()?.open(path).await
    }

    /// cladefs: the cell directory at `path`, as of its last commit, opened as `opening` says
    /// ([`Opening::open`]).
    pub async fn open_with(path: impl AsRef<Path>, opening: Opening) -> Result<Self, Error> {
        opening.open(path).await
    }

    /// cladefs: the cell directory at `path`, a fork attached to other open cells, its uploader as
    /// `keeping` says ([`Opening::open_base`]).
    pub async fn open_base_with(
        path: impl AsRef<Path>,
        arena: Option<Arc<Arena>>,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        Opening::keeping(keeping).arena(arena).open_base(path).await
    }

    /// The open cell `opened` at `path`, opened as `opening` says, of whose cell and those it was
    /// forked from the cells `public` are public ([`cell_dir::public_cells`]).
    pub(crate) async fn new(
        path: &Path,
        opened: host_cell::Opened,
        opening: Opening,
    ) -> Result<Self, Error> {
        let host_cell::Opened { local, bucket, uploader } = opened;
        let forked = uploader.forked_from();
        Self::assemble(path, (local, bucket, Some(uploader)), forked, opening).await
    }

    /// cladefs: the cell `local` at `path`, its cell's extents read in `bucket`, with its uploader
    /// if it has one (an ephemeral cell's has none, doc-25), a fork of the version `forked` says,
    /// if any, opened as `opening` says: as [`OpenCell::new`] has it.
    async fn assemble(
        path: &Path,
        (local, bucket, uploader): (
            Arc<LocalCell<HostDir>>,
            Arc<bucket_cell::object_bucket::RoutedBucket>,
            Option<local_cell::upload::Uploader<HostDir>>,
        ),
        forked: Option<local_cell::upload::Forked>,
        opening: Opening,
    ) -> Result<Self, Error> {
        // cladefs: what may fail of it first, its open cell closed if it does (TASK-685.3).
        let made = local_cell::upload::blocking(|| -> Result<_, Error> {
            cache::relieve_pressure();
            let public = cell_dir::public_cells(path)?;
            let forked_from = Arc::new(cell_dir::ancestry(path)?);
            let cache = opening.keeping.cache.clone();
            let fetcher = crate::bucket::HostFetcher::new(bucket.clone(), cache)?;
            Ok((public, forked_from, fetcher))
        });
        let (public, forked_from, fetcher) = match made {
            Ok(made) => made,
            Err(err) => {
                drop((local, bucket));
                if let Some(uploader) = uploader {
                    uploader.close().await;
                }
                return Err(err);
            }
        };
        let Opening { arena, guest, keeping: _ } = opening;
        let arenas = Arenas { own: arena, public: Arc::new(public) };
        let cell = local.description().cell;
        // cladefs: a guest's boot of a fork of a version, whose reads its version's evidence
        // predicts, fetched ahead, and records (TASK-702.2, TASK-702.3).
        let evidence = (forked.filter(|_| guest)).map(|forked| {
            let (local, bucket, fetcher) = (local.clone(), bucket.clone(), fetcher.clone());
            let reading = crate::evidence::Reading { local, arenas: arenas.clone(), fetcher };
            crate::evidence::Evidence::opened(forked, cell, bucket, reading)
        });
        Ok(Self {
            evidence,
            arenas,
            forked_from,
            local,
            bucket,
            path: path.to_owned(),
            bases: Default::default(),
            dax: CellMutex::new("dax", None),
            uploader,
            keys: None,
            opening: CellRwLock::new("opening", ()),
            changes: CellMutex::new("changes", WindowChanges::default()),
            flush_turns: Default::default(),
            halt: Halt::new(path, cell),
            fetcher,
            closed: false,
            kept_until_closed: Default::default(),
        })
    }

    /// cladefs: takes `cell` into the cell directory at `path`, of `capacity` bytes, placed at
    /// `token`'s placement if given, and returns it, opened as `opening` says ([`Opening::take`]).
    pub async fn take_with(
        path: impl AsRef<Path>,
        (capacity, cell, token): (u64, CellId, Option<u64>),
        opening: Opening,
    ) -> Result<Self, Error> {
        opening.take(path, capacity, cell, token).await
    }

    /// cladefs: makes a new cell directory at `path`, which must not exist, of `capacity` bytes and
    /// the cell `of` says, a cell of `kind` (TASK-364), its uploader as `keeping`
    /// says, placed at `token`'s placement if given, closes it, and returns its cell: what
    /// [`Opening::take`] takes a cell into, and the daemon's `CreateCell` makes. Its
    /// cell, and so its head, is taken as it is made with the token of the placement it is given,
    /// not `keeping`'s, this host's: a new copy placed later
    /// took no cell from an earlier placement (`ESTALE`), the head taken before the placement
    /// was recorded (TASK-149.20, TASK-266).
    pub async fn create_placed(
        path: impl AsRef<Path>,
        (capacity, of, token): (u64, CellOf, Option<u64>),
        kind: Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<CellId, Error> {
        let opened = Self::made_placed(path.as_ref(), (capacity, of, token), kind, keeping).await?;
        let cell = opened.local.description().cell;
        opened.close().await;
        Ok(cell)
    }

    /// cladefs: makes a new cell directory at `path` as [`OpenCell::create_placed`] does, and
    /// returns it opened, as made: what [`Opening::take`] opens.
    async fn made_placed(
        path: &Path,
        (capacity, of, token): (u64, CellOf, Option<u64>),
        kind: Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<host_cell::Opened, Error> {
        let keeping =
            local_cell::upload::Options { token: token.unwrap_or(keeping.token), ..keeping };
        let force = keeping.force;
        let opened = Self::creating(path, async |span| {
            let opened = host_cell::create_unmade(path, capacity, of, kind, None, keeping).await?;
            span.set_cell(opened.local.description().cell);
            Ok(opened)
        })
        .await?;
        // cladefs: placed, then made whole, and removed if either fails: made first, a placement
        // failing left a store no placement had placed, which every making after refused
        // (`EEXIST`, board 5668); and the making's last step failing may leave the store looking
        // made. A forced taking's placement forced from then on (TASK-681).
        let placed = local_cell::upload::blocking(|| {
            match (token, force) {
                (Some(token), true) => Self::place_forced(path, token),
                (Some(token), false) => Self::place(path, token),
                (None, _) => Ok(()),
            }?;
            opened.made().map_err(Error::new)
        });
        if let Err(err) = placed {
            opened.close().await;
            local_cell::upload::blocking(|| host_cell::discard(path));
            return Err(err);
        }
        Ok(opened)
    }

    /// cladefs: places the cell directory at `path` at the placement
    /// whose fencing token, its boot ID, is `token` (TASK-39.7): its host takes its cell's
    /// cell with that token from then on, and its writer the cell's head with that taking's
    /// epoch; a later placement's token exceeds it, so a placement cut off takes no cell back
    /// from the one after it (`ESTALE`), and its commits fail once a later one takes it (`EROFS`).
    pub fn place(path: impl AsRef<Path>, token: u64) -> Result<(), Error> {
        local_cell::place_host(path.as_ref(), token, false).map_err(Error::new)
    }

    /// cladefs: places the cell directory at `path` as [`OpenCell::place`] does, its placement's
    /// takings of a replicated cell forced (doc-21, TASK-681): a taking goes on with the members of
    /// the configuration the cell's head names that answer, fewer than a take needs, as when f+1 of
    /// them are gone for good. Commits told replicated that only the others held are lost.
    pub fn place_forced(path: impl AsRef<Path>, token: u64) -> Result<(), Error> {
        local_cell::place_host(path.as_ref(), token, true).map_err(Error::new)
    }

    /// cladefs: deletes the cell directory at `path`, which nothing has open: with `bucket`, its
    /// cell there first, its forks' cells living on, which the collector keeps what they inherited
    /// for; then its directory, which holds its cell alone (TASK-165.7). A deletion that stopped
    /// partway is finished by deleting again. Fails with `ENOENT` if there is none at `path`, and
    /// with `ESTALE` if a later placement holds its cell in `bucket`.
    pub async fn delete(
        path: impl AsRef<Path>,
        bucket: Option<&bucket_cell::object_bucket::RoutedBucket>,
    ) -> Result<(), Error> {
        host_cell::delete(path.as_ref(), bucket, crate::bucket::host_token).await
    }

    /// cladefs: the bucket the description of the cell directory at `path` names, which its cell is
    /// kept in, if its description is there: what deleting it deletes its cell in. An open cell of
    /// another format is refused (`local_cell::Format`).
    pub fn bucket_at(
        path: impl AsRef<Path>,
    ) -> Result<Option<Arc<bucket_cell::object_bucket::RoutedBucket>>, Error> {
        host_cell::bucket_at(path.as_ref()).map_err(Error::new)
    }
}
