// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a bucket as a degraded one is, between a backend and its objects ([`Faulty`]): each
//! request waits so long first, and fails as it is told, its writes as a full disk's do or every
//! request as an object store out of service answers, until it is told otherwise (TASK-330.3).
//! What a host's daemon does meanwhile, its memory, its stalled commits and what it says of the
//! failures, is what a soak against it measures; how far a bucket is, its latency, what LangSmith's
//! RustFS beside the daemon on a shared disk was (TASK-310), and what `CLADEFS_BUCKET_LATENCY_MS`
//! set alone before.
//!
//! A bucket made from a URL ([`crate::object_bucket::object_store_at`]) is one, if its process's
//! environment says so: `CLADEFS_BUCKET_LATENCY_MS`, milliseconds each request waits, as long as
//! the process lives (TASK-62); `CLADEFS_BUCKET_FAULTS`, a file of what it does now, read again
//! within [`READ_EVERY`] of each change, for a test to change as it runs: [`Fault::parse`]'s words,
//! such as `latency-ms=300 failing=no-space`, none for a bucket that answers. Neither set, a bucket
//! is the object store itself: nothing of this is in its requests' way.
//!
//! cladefs: a fault may also refuse a write of a key that comes too soon after the last write of it
//! answered (`rewrite-every-ms=1000`), as GCS refuses more than about one write a second of an
//! object, `429 Too Many Requests` (TASK-690).

use futures::StreamExt as _;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a file of faults is read again, at most, as requests come: what a phase's change
/// waits to take effect.
pub const READ_EVERY: Duration = Duration::from_millis(100);

/// How a bucket fails its requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Failing {
    /// It doesn't.
    #[default]
    Not,
    /// Its writes fail as a file bucket's on a full disk do, `object_store`'s generic error with
    /// the filesystem's `ENOSPC` behind it (TASK-314); its reads and deletes answer.
    NoSpace,
    /// Every request fails as an object store out of service answers them, `503 Service
    /// Unavailable` once `object_store`'s own retries gave up.
    Unavailable,
}

impl Failing {
    const NAMES: [(Self, &'static str); 3] =
        [(Self::Not, "not"), (Self::NoSpace, "no-space"), (Self::Unavailable, "unavailable")];

    /// Its name in a [`Fault`]'s words.
    pub fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(failing, _)| *failing == self).unwrap().1
    }
}

/// What a degraded bucket does to each request: waits `latency` first, then fails it as `failing`
/// says. cladefs: and refuses a write of a key within `rewrite_every` of the last write of it
/// answered, if not zero, as GCS refuses one within about a second (TASK-690).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fault {
    pub latency: Duration,
    pub failing: Failing,
    pub rewrite_every: Duration,
}

impl Fault {
    /// The fault `words` say, words separated by white space, each `NAME=VALUE`:
    /// `latency-ms=MILLISECONDS`, `failing=not|no-space|unavailable` and
    /// `rewrite-every-ms=MILLISECONDS`, each of the default fault's (none) unless said.
    pub fn parse(words: &str) -> Result<Self, String> {
        let mut fault = Self::default();
        for word in words.split_whitespace() {
            match word.split_once('=') {
                Some(("latency-ms", ms)) => {
                    let ms = ms.parse().map_err(|_| format!("latency-ms={ms} is no count"))?;
                    fault.latency = Duration::from_millis(ms);
                }
                Some(("rewrite-every-ms", ms)) => {
                    let ms =
                        ms.parse().map_err(|_| format!("rewrite-every-ms={ms} is no count"))?;
                    fault.rewrite_every = Duration::from_millis(ms);
                }
                Some(("failing", name)) => {
                    fault.failing = (Failing::NAMES.iter())
                        .find(|(_, known)| *known == name)
                        .map(|(failing, _)| *failing)
                        .ok_or_else(|| format!("failing={name}: not, no-space or unavailable"))?;
                }
                _ => {
                    return Err(format!(
                        "{word}: latency-ms=MILLISECONDS, failing=HOW or \
                         rewrite-every-ms=MILLISECONDS"
                    ));
                }
            }
        }
        Ok(fault)
    }

    /// Its words, which [`Fault::parse`] reads.
    pub fn words(&self) -> String {
        let (latency, failing) = (self.latency.as_millis(), self.failing.name());
        let rewrite_every = self.rewrite_every.as_millis();
        format!("latency-ms={latency} failing={failing} rewrite-every-ms={rewrite_every}")
    }

    /// What it fails a request with, a write if `write`, if it fails it.
    fn failure(&self, write: bool) -> object_store::Result<()> {
        let source: Box<dyn std::error::Error + Send + Sync> = match self.failing {
            Failing::NoSpace if write => Box::new(std::io::Error::from_raw_os_error(libc::ENOSPC)),
            Failing::Unavailable => "HTTP status server error (503 Service Unavailable)".into(),
            Failing::Not | Failing::NoSpace => return Ok(()),
        };
        Err(object_store::Error::Generic { store: "Faulty", source })
    }
}

/// What a bucket's faults are now: a fault set, and what a file says, as of when it was last read.
#[derive(Debug)]
pub struct Faults {
    /// What every request waits besides, as long as these live (`CLADEFS_BUCKET_LATENCY_MS`).
    latency: Duration,
    /// The file it reads its fault from, if any (`CLADEFS_BUCKET_FAULTS`).
    file: Option<std::path::PathBuf>,
    now: Mutex<Now>,
}

#[derive(Debug)]
struct Now {
    fault: Fault,
    /// When the file was last read.
    read: Option<Instant>,
}

impl Faults {
    /// Faults whose every request waits `latency` besides, of `file`'s, if any, read as requests
    /// come, or of what [`Faults::set`] sets.
    pub fn new(latency: Duration, file: Option<std::path::PathBuf>) -> Arc<Self> {
        let now = Mutex::new(Now { fault: Fault::default(), read: None });
        Arc::new(Self { latency, file, now })
    }

    /// The faults the process's environment says ([`self`]), if it says any.
    pub(crate) fn from_env() -> Result<Option<Arc<Self>>, String> {
        let latency = match std::env::var("CLADEFS_BUCKET_LATENCY_MS") {
            Ok(ms) if !ms.is_empty() => Duration::from_millis(
                ms.parse().map_err(|_| format!("CLADEFS_BUCKET_LATENCY_MS={ms} is no count"))?,
            ),
            _ => Duration::ZERO,
        };
        let file = std::env::var_os("CLADEFS_BUCKET_FAULTS").filter(|file| !file.is_empty());
        if latency.is_zero() && file.is_none() {
            return Ok(None);
        }
        let faults = Self::new(latency, file.map(Into::into));
        // A file that says no fault it can read fails now, not at a request.
        faults.read()?;
        Ok(Some(faults))
    }

    /// Has the requests from now on do as `fault` says, until a file's says otherwise.
    pub fn set(&self, fault: Fault) {
        self.now.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).fault = fault;
    }

    /// What a request does now: its file's, read again if it was last read [`READ_EVERY`] ago, or
    /// what was set, waiting besides what every request waits.
    pub fn now(&self) -> Fault {
        let due = {
            let now = self.now.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            self.file.is_some() && now.read.is_none_or(|read| read.elapsed() >= READ_EVERY)
        };
        if due && let Err(error) = self.read() {
            log::warn!("object storage: its faults: {error}");
        }
        let fault = self.now.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).fault;
        Fault { latency: fault.latency + self.latency, ..fault }
    }

    /// Reads its file, if it has one: a file gone, or empty, says no fault.
    fn read(&self) -> Result<(), String> {
        let Some(file) = &self.file else { return Ok(()) };
        let words = match std::fs::read_to_string(file) {
            Ok(words) => words,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("reading {}: {error}", file.display())),
        };
        let fault = Fault::parse(&words).map_err(|error| format!("{}: {error}", file.display()));
        let mut now = self.now.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        now.read = Some(Instant::now());
        let fault = fault?;
        if fault != now.fault {
            log::info!("object storage: as {} says, {}", file.display(), fault.words());
            now.fault = fault;
        }
        Ok(())
    }
}

/// An object store as degraded as its [`Faults`] say, before `objects`.
#[derive(Debug)]
pub struct Faulty {
    objects: Arc<dyn ObjectStore>,
    faults: Arc<Faults>,
    /// cladefs: when the last write of each key was answered, which a write of it within
    /// [`Fault::rewrite_every`] is refused for (TASK-690).
    written: Mutex<std::collections::HashMap<Path, tokio::time::Instant>>,
}

impl Faulty {
    pub fn new(objects: Arc<dyn ObjectStore>, faults: Arc<Faults>) -> Self {
        Self { objects, faults, written: Mutex::default() }
    }

    /// cladefs: a write of `location`, refused, as GCS refuses it, if the last write of it was
    /// answered within `fault`'s [`Fault::rewrite_every`]; else made by `write`, its answer's time
    /// noted.
    async fn rewrite<T>(
        &self,
        fault: Fault,
        location: &Path,
        write: impl Future<Output = object_store::Result<T>>,
    ) -> object_store::Result<T> {
        let written = || self.written.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if fault.rewrite_every.is_zero() {
            return write.await;
        }
        let last = written().get(location).copied();
        if last.is_some_and(|last| last.elapsed() < fault.rewrite_every) {
            let source = "HTTP status client error (429 Too Many Requests): the object's rate of \
                          change exceeded"
                .into();
            return Err(object_store::Error::Generic { store: "Faulty", source });
        }
        let answered = write.await;
        written().insert(location.clone(), tokio::time::Instant::now());
        answered
    }

    /// Waits as a request, a write if `write`, does now, then fails it, or doesn't: the fault it
    /// went as.
    async fn request(&self, write: bool) -> object_store::Result<Fault> {
        let fault = self.faults.now();
        if !fault.latency.is_zero() {
            tokio::time::sleep(fault.latency).await;
        }
        fault.failure(write).map(|()| fault)
    }

    /// A stream of what `stream` yields, once a request, not a write, has waited and failed, or
    /// not.
    fn streamed<T: Send + 'static>(
        &self,
        stream: BoxStream<'static, object_store::Result<T>>,
    ) -> BoxStream<'static, object_store::Result<T>> {
        let fault = self.faults.now();
        let waited = async move {
            if !fault.latency.is_zero() {
                tokio::time::sleep(fault.latency).await;
            }
            match fault.failure(false) {
                Ok(()) => stream,
                Err(error) => futures::stream::once(async { Err(error) }).boxed(),
            }
        };
        futures::stream::once(waited).flatten().boxed()
    }
}

impl std::fmt::Display for Faulty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Faulty({})", self.objects)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Faulty {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let fault = self.request(true).await?;
        self.rewrite(fault, location, self.objects.put_opts(location, payload, opts)).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.request(true).await?;
        self.objects.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.request(false).await?;
        self.objects.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.streamed(self.objects.delete_stream(locations))
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.streamed(self.objects.list(prefix))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.request(false).await?;
        self.objects.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.request(true).await?;
        self.objects.copy_opts(from, to, options).await
    }
}

/// cladefs: how often a [`Seeded`] bucket does what to a request (TASK-685.5, doc-24): each request
/// waits up to `delay_most`, as the seed draws it; then, as it draws, fails before it lands
/// (`fail_before`), or lands and fails after (`fail_after`), as a request answered with an error or
/// lost on its way back does, its write in the bucket all the same; and the bucket goes out of
/// reach for `out_for` at a time (`out_of_reach`, at each request), every request failing
/// meanwhile, as a region's link does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Odds {
    pub delay_most: Duration,
    pub fail_before: f64,
    pub fail_after: f64,
    pub out_of_reach: f64,
    pub out_for: Duration,
}

impl Odds {
    /// None: every request answered at once.
    pub const NONE: Self = Self {
        delay_most: Duration::ZERO,
        fail_before: 0.0,
        fail_after: 0.0,
        out_of_reach: 0.0,
        out_for: Duration::ZERO,
    };
}

/// cladefs: an object store whose requests are delayed and failed as a seed says, before `objects`
/// ([`Odds`]): the simulation's bucket, which the simulated hosts share (TASK-685.5, doc-24). Its
/// draws are its own generator's, seeded, and its waits the clock's, so that under madsim a seed's
/// run is a function of the seed.
#[derive(Debug)]
pub struct Seeded {
    objects: Arc<dyn ObjectStore>,
    odds: Mutex<Odds>,
    rng: Mutex<rand::rngs::StdRng>,
    /// Until when it is out of reach, if it is.
    out_until: Mutex<Option<tokio::time::Instant>>,
}

/// What a [`Seeded`] bucket does to a request it lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Answered as it is.
    Answered,
    /// Landed, then failed: its write in the bucket.
    FailedAfter,
}

impl Seeded {
    /// `objects`, its requests as `odds` says, drawn from `seed`.
    pub fn new(objects: Arc<dyn ObjectStore>, seed: u64, odds: Odds) -> Self {
        use rand::SeedableRng as _;
        let rng = Mutex::new(rand::rngs::StdRng::seed_from_u64(seed));
        Self { objects, odds: Mutex::new(odds), rng, out_until: Mutex::new(None) }
    }

    /// Has the requests from now on go as `odds` says: [`Odds::NONE`] heals it.
    pub fn set(&self, odds: Odds) {
        *self.odds.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = odds;
        if odds.out_of_reach == 0.0 {
            *self.out_until.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    /// Waits as a request does, then fails it before it lands, or says what it does after.
    async fn request(&self) -> object_store::Result<Fate> {
        use rand::RngExt as _;
        let odds = *self.odds.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (delay, before, after, out) = {
            let mut rng = self.rng.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let delay = match odds.delay_most.is_zero() {
                true => Duration::ZERO,
                false => odds.delay_most.mul_f64(rng.random_range(0.0..1.0)),
            };
            let before = rng.random_bool(odds.fail_before.clamp(0.0, 1.0));
            let after = rng.random_bool(odds.fail_after.clamp(0.0, 1.0));
            let out = rng.random_bool(odds.out_of_reach.clamp(0.0, 1.0));
            (delay, before, after, out)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let now = tokio::time::Instant::now();
        let reachable = {
            let mut until = self.out_until.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if until.is_some_and(|until| now >= until) {
                *until = None;
            }
            if until.is_none() && out && !odds.out_for.is_zero() {
                *until = Some(now + odds.out_for);
            }
            until.is_none()
        };
        if !reachable || before {
            return Err(Self::failure("out of reach, or failed before landing"));
        }
        Ok(if after { Fate::FailedAfter } else { Fate::Answered })
    }

    /// What it answers a request it fails with: a server's error, which a client retries.
    fn failure(why: &'static str) -> object_store::Error {
        let source = format!("HTTP status server error (503 Service Unavailable): {why}").into();
        object_store::Error::Generic { store: "Seeded", source }
    }

    /// `answered`, or, a request failed after landing, its failure.
    fn fated<T>(fate: Fate, answered: object_store::Result<T>) -> object_store::Result<T> {
        match (fate, answered) {
            (Fate::FailedAfter, Ok(_)) => Err(Self::failure("landed, then failed")),
            (_, answered) => answered,
        }
    }

    /// A stream of what `stream` yields, once a request has waited and failed, or not.
    fn streamed<T: Send + 'static>(
        self: &Arc<Self>,
        stream: BoxStream<'static, object_store::Result<T>>,
    ) -> BoxStream<'static, object_store::Result<T>> {
        let this = self.clone();
        let waited = async move {
            match this.request().await {
                Ok(_) => stream,
                Err(error) => futures::stream::once(async { Err(error) }).boxed(),
            }
        };
        futures::stream::once(waited).flatten().boxed()
    }
}

impl std::fmt::Display for Seeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Seeded({})", self.objects)
    }
}

/// cladefs: a [`Seeded`] bucket, shared: what an [`ObjectStore`] is made of where its streams
/// outlive the call that made them.
#[derive(Clone, Debug)]
pub struct SeededBackend(pub Arc<Seeded>);

impl std::fmt::Display for SeededBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[async_trait::async_trait]
impl ObjectStore for SeededBackend {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let fate = self.0.request().await?;
        Seeded::fated(fate, self.0.objects.put_opts(location, payload, opts).await)
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let fate = self.0.request().await?;
        Seeded::fated(fate, self.0.objects.put_multipart_opts(location, opts).await)
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let fate = self.0.request().await?;
        Seeded::fated(fate, self.0.objects.get_opts(location, options).await)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.0.streamed(self.0.objects.delete_stream(locations))
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.0.streamed(self.0.objects.list(prefix))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let fate = self.0.request().await?;
        Seeded::fated(fate, self.0.objects.list_with_delimiter(prefix).await)
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        let fate = self.0.request().await?;
        Seeded::fated(fate, self.0.objects.copy_opts(from, to, options).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-690: a bucket as GCS refuses a write of an object within a second of the last one
    /// answered, `429 Too Many Requests`, and takes it after; writes of other objects meanwhile
    /// it takes.
    #[tokio::test(start_paused = true)]
    async fn writes_of_an_object_too_soon_are_refused() {
        let faults = Faults::new(Duration::ZERO, None);
        faults.set(Fault { rewrite_every: Duration::from_secs(1), ..Fault::default() });
        let faulty = Faulty::new(Arc::new(object_store::memory::InMemory::new()), faults);
        let put = async |key: &str| {
            let (path, payload) = (Path::from(key), PutPayload::from_static(b"1"));
            faulty.put_opts(&path, payload, PutOptions::default()).await
        };
        put("head").await.unwrap();
        tokio::time::sleep(Duration::from_millis(999)).await;
        assert_eq!(
            put("head").await.unwrap_err().to_string(),
            "Generic Faulty error: HTTP status client error (429 Too Many Requests): the object's \
             rate of change exceeded"
        );
        put("other").await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        put("head").await.unwrap();
    }

    /// A bucket whose disk is full fails its writes with the filesystem's ENOSPC, and answers its
    /// reads; one out of service fails every request; each request waits its latency first; and
    /// its file's faults hold within [`READ_EVERY`] of its change, none once it is gone.
    #[tokio::test]
    async fn requests_fail_as_their_faults_say() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("faults");
        let faults = Faults::new(Duration::ZERO, Some(file.clone()));
        let objects = Arc::new(object_store::memory::InMemory::new());
        let faulty = Faulty::new(objects, faults.clone());
        let path = Path::from("a");
        let put = || faulty.put_opts(&path, PutPayload::from_static(b"1"), PutOptions::default());
        put().await.unwrap();

        std::fs::write(&file, "failing=no-space").unwrap();
        tokio::time::sleep(READ_EVERY).await;
        let error = put().await.unwrap_err().to_string();
        assert_eq!(error, "Generic Faulty error: No space left on device (os error 28)");
        assert_eq!(faulty.get_opts(&path, GetOptions::default()).await.unwrap().meta.size, 1);

        std::fs::write(&file, "latency-ms=200 failing=unavailable").unwrap();
        tokio::time::sleep(READ_EVERY).await;
        let started = Instant::now();
        let error = faulty.get_opts(&path, GetOptions::default()).await.unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(200), "{:?}", started.elapsed());
        assert_eq!(
            error.to_string(),
            "Generic Faulty error: HTTP status server error (503 Service Unavailable)"
        );
        let listed: Vec<_> = faulty.list(None).collect().await;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_err());
        // cladefs: listings by delimiter and batches of deletes fail as every request (TASK-395.5).
        let unavailable =
            "Generic Faulty error: HTTP status server error (503 Service Unavailable)";
        let level = faulty.list_with_delimiter(None).await.unwrap_err();
        assert_eq!(level.to_string(), unavailable);
        let deleting = futures::stream::iter([Ok(path.clone()), Ok(Path::from("b"))]).boxed();
        let deleted: Vec<String> = (faulty.delete_stream(deleting).collect::<Vec<_>>().await)
            .into_iter()
            .map(|deleted| deleted.unwrap_err().to_string())
            .collect();
        assert_eq!(deleted, [unavailable]);

        std::fs::remove_file(&file).unwrap();
        tokio::time::sleep(READ_EVERY).await;
        assert_eq!(faults.now(), Fault::default());
        put().await.unwrap();
        let listed: Vec<_> = faulty.list(None).collect().await;
        assert_eq!(
            listed.into_iter().map(|meta| meta.unwrap().location).collect::<Vec<_>>(),
            std::slice::from_ref(&path)
        );
        let level = faulty.list_with_delimiter(None).await.unwrap();
        let objects: Vec<Path> = level.objects.into_iter().map(|meta| meta.location).collect();
        assert_eq!((level.common_prefixes, objects), (vec![], vec![path.clone()]));
        let deleting = futures::stream::iter([Ok(path.clone())]).boxed();
        let deleted: Vec<Path> =
            faulty.delete_stream(deleting).map(|deleted| deleted.unwrap()).collect().await;
        assert_eq!(deleted, [path]);
        assert_eq!(faulty.list(None).collect::<Vec<_>>().await.len(), 0);
    }
}
