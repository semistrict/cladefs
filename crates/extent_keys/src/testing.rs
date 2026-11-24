// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what tests of encryption at rest need (the `testing` feature, backlog doc-16): a
//! KMS making every tenant's KEK on first use, in memory, the tests' default ([`MemoryKms`],
//! TASK-411.28); one whose every tenant's KEK is one known key, which tests' processes share with
//! those they start ([`KnownKms`], TASK-411.28); a KMS that counts the calls made of it
//! ([`Counted`]), so that a test can say how many a path costs: none on a fork's, one for a host
//! taking a cell; a KMS forgetting destroyed KEKs, as real ones forget deleted keys
//! ([`Forgetting`], TASK-411.19); AWS KMS's emulator, started for a test ([`LocalAwsKms`],
//! TASK-411.17); an OpenBao dev server ([`openbao::DevServer`], TASK-411.16), the real KMS a test
//! can start; and what tests against real Google Cloud KMS need ([`gcp`], TASK-411.18).

use crate::kms::{Error, Kms};
use crate::{Secret, Wrapped};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod gcp;
pub mod openbao;

/// A KMS that counts the wraps and unwraps asked of it, refused or not, as they are asked: each is
/// a call a real KMS would be made, and billed for.
pub struct Counted<K> {
    inner: K,
    wraps: AtomicU64,
    unwraps: AtomicU64,
}

impl<K: Kms> Counted<K> {
    pub fn new(inner: K) -> Self {
        Self { inner, wraps: AtomicU64::new(0), unwraps: AtomicU64::new(0) }
    }

    /// The KMS it counts the calls of, for what [`Kms`] has not: a fake's revoking, say.
    pub fn inner(&self) -> &K {
        &self.inner
    }

    /// The wraps asked of it since it was made, or [`Counted::reset`].
    pub fn wraps(&self) -> u64 {
        self.wraps.load(Ordering::Relaxed)
    }

    /// The unwraps asked of it since it was made, or [`Counted::reset`].
    pub fn unwraps(&self) -> u64 {
        self.unwraps.load(Ordering::Relaxed)
    }

    /// Counts from zero again: after a test's setup, say.
    pub fn reset(&self) {
        self.wraps.store(0, Ordering::Relaxed);
        self.unwraps.store(0, Ordering::Relaxed);
    }
}

impl<K: Kms> Kms for Counted<K> {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        self.wraps.fetch_add(1, Ordering::Relaxed);
        self.inner.wrap(tenant, key)
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        self.unwraps.fetch_add(1, Ordering::Relaxed);
        self.inner.unwrap(tenant, wrapped)
    }

    fn forget(&self, tenant: u64) {
        self.inner.forget(tenant)
    }

    fn prewrap(&self) -> bool {
        self.inner.prewrap()
    }

    fn names(&self) -> Option<&crate::kms::KeyNames> {
        self.inner.names()
    }
}

/// cladefs: the tests' KMS by default (TASK-411.28: every cell encrypted, every test too): each
/// tenant's KEK made on its first wrap and kept in memory, as the bucket KMS makes them in the
/// bucket (`bucket_cell::bucket_kms`), so that any tenant a test names is encrypted with no setup.
/// Its wraps are the bucket KMS's ([`crate::kms::bucket::wrap`]). A tenant [`MemoryKms::destroy`]ed
/// is refused for good, as a tenant whose KEK object is a tombstone; an unwrap for a tenant with
/// no KEK is refused as destroyed, what it names gone. Hosts of one test share one, as they share
/// a bucket: clone the `Arc` it is held in.
#[derive(Default)]
pub struct MemoryKms {
    keks: std::sync::Mutex<std::collections::HashMap<u64, Option<crate::kms::bucket::TenantKek>>>,
}

impl MemoryKms {
    pub fn new() -> Self {
        Self::default()
    }

    /// Destroys `tenant`'s KEK, or makes it never to be had: every call naming the tenant is
    /// refused from then on ([`Error::Destroyed`]).
    pub fn destroy(&self, tenant: u64) {
        self.keks.lock().unwrap().insert(tenant, None);
    }

    /// `tenant`'s KEK, made if `make` and it has none.
    fn kek<T>(
        &self,
        tenant: u64,
        make: bool,
        with: impl FnOnce(&crate::kms::bucket::TenantKek) -> T,
    ) -> Result<T, Error> {
        let mut keks = self.keks.lock().unwrap();
        let kek = match keks.entry(tenant) {
            std::collections::hash_map::Entry::Occupied(had) => had.into_mut(),
            std::collections::hash_map::Entry::Vacant(_) if !make => {
                return Err(Error::Destroyed { tenant });
            }
            std::collections::hash_map::Entry::Vacant(none) => {
                none.insert(Some(crate::kms::bucket::TenantKek::random()))
            }
        };
        kek.as_ref().map(with).ok_or(Error::Destroyed { tenant })
    }
}

impl Kms for MemoryKms {
    async fn wrap(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        self.kek(tenant, true, |kek| crate::kms::bucket::wrap(tenant, 0, kek, key))
    }

    async fn unwrap(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let unwrapped =
            self.kek(tenant, false, |kek| crate::kms::bucket::unwrap(tenant, 0, kek, wrapped))?;
        unwrapped.map_err(|source| Error::Unauthentic { tenant, source })
    }
}

/// A KMS whose every tenant's KEK is one known key, [`KnownKms::KEK`], in every process, never a
/// host's: what cladefs's tests' processes, and those they start, encrypt under when their
/// environment names no KMS, agreeing with no state shared, whatever bucket each keeps its cells in
/// (cladefs's `tests-kms`, TASK-411.28). Its wraps are the bucket KMS's
/// ([`crate::kms::bucket::wrap`]), each under a key derived for its tenant alone: one tenant's key
/// unwraps for no other. No tenant's is ever destroyed: a test of that has a KMS of its own.
pub struct KnownKms;

impl KnownKms {
    /// Every tenant's KEK: 32 bytes of 0x7e, as the tests' master key (`scripts/lib/test-kms.sh`).
    pub const KEK: [u8; 32] = [0x7e; 32];

    /// The tenants' KEK, of the ID of zeros.
    fn kek() -> crate::kms::bucket::TenantKek {
        let id = crate::kms::bucket::KekId([0; crate::kms::bucket::KEK_ID_LEN]);
        crate::kms::bucket::TenantKek::of(id, Secret::from_bytes(&Self::KEK))
    }
}

impl Kms for KnownKms {
    async fn wrap(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        Ok(crate::kms::bucket::wrap(tenant, 0, &Self::kek(), key))
    }

    async fn unwrap(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        crate::kms::bucket::unwrap(tenant, 0, &Self::kek(), wrapped)
            .map_err(|source| Error::Unauthentic { tenant, source })
    }
}

/// A KMS that forgets a tenant's KEK once it is destroyed, as a real KMS forgets a deleted key: AWS
/// KMS once its deletion's waiting period is over, OpenBao Transit's key deleted (TASK-411.19).
/// Where `inner` answers [`Error::Destroyed`], it answers [`Error::NoSuchTenant`], as for a tenant
/// that never had a KEK: so a [`crate::kms::LocalKms`]'s `destroy` reads as a real KMS's deletion,
/// which every cell refuses as destroyed all the same (TASK-411.28).
pub struct Forgetting<K> {
    inner: K,
}

impl<K: Kms> Forgetting<K> {
    pub fn new(inner: K) -> Self {
        Self { inner }
    }

    /// The KMS it forgets for: a fake's destroying, say.
    pub fn inner(&self) -> &K {
        &self.inner
    }
}

/// `answer`, a destruction told as no KEK at all.
fn forgotten<T>(answer: Result<T, Error>) -> Result<T, Error> {
    answer.map_err(|error| match error {
        Error::Destroyed { tenant } => Error::NoSuchTenant { tenant },
        error => error,
    })
}

impl<K: Kms> Kms for Forgetting<K> {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        let wrapping = self.inner.wrap(tenant, key);
        async move { forgotten(wrapping.await) }
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        let unwrapping = self.inner.unwrap(tenant, wrapped);
        async move { forgotten(unwrapping.await) }
    }

    fn forget(&self, tenant: u64) {
        self.inner.forget(tenant)
    }

    fn prewrap(&self) -> bool {
        self.inner.prewrap()
    }

    fn names(&self) -> Option<&crate::kms::KeyNames> {
        self.inner.names()
    }
}

/// local-kms (github.com/nsmithuk/local-kms, MIT), an emulator of AWS KMS, run for a test: a
/// process of its own, listening on a port free when it started, its keys in a directory of its
/// own, gone with it. `CLADEFS_LOCAL_KMS` names its binary, as `scripts/test-local-kms.sh` installs
/// and names it, along with the dummy credentials the SDK signs with, the environment's, for the
/// standard chain to find, as the backend's own tests need them: the emulator checks no signature.
/// What it differs in from AWS: it has no IAM, so never answers `AccessDeniedException` but for a
/// ciphertext naming no key it has; it rotates no key on demand, nor before a year, but has a key
/// seeded with several versions ([`LocalAwsKms::seeded`]); it answers a ciphertext of another key
/// with `InvalidCiphertextException` where AWS answers `IncorrectKeyException`; and it deletes no
/// key, pending deletion forever.
pub struct LocalAwsKms {
    child: std::process::Child,
    endpoint: String,
    admin: aws_sdk_kms::Client,
    /// Its admin calls' runtime: None once dropped ([`LocalAwsKms::run`]).
    runtime: Option<tokio::runtime::Runtime>,
    _dir: tempfile::TempDir,
}

/// The region the emulator says it is in, and [`LocalAwsKms::url`] names.
pub const LOCAL_KMS_REGION: &str = "eu-west-2";

/// The account the emulator says its keys are of.
pub const LOCAL_KMS_ACCOUNT: &str = "111122223333";

impl LocalAwsKms {
    /// The emulator, with no key.
    pub fn start() -> Self {
        Self::seeded("")
    }

    /// The emulator, with the keys and aliases `seed` makes, as local-kms's seeding file says
    /// (its README, Seeding file format): a key of several backing keys is one rotated.
    pub fn seeded(seed: &str) -> Self {
        use std::io::BufRead as _;
        let binary = std::env::var_os("CLADEFS_LOCAL_KMS").expect(
            "CLADEFS_LOCAL_KMS names no local-kms: scripts/test-local-kms.sh installs and names it",
        );
        assert!(
            std::env::var_os("AWS_ACCESS_KEY_ID").is_some(),
            "no AWS credentials in the environment for the SDK to sign with: \
             scripts/test-local-kms.sh gives dummy ones"
        );
        let dir = tempfile::tempdir().unwrap();
        let seed_path = dir.path().join("seed.yaml");
        std::fs::write(&seed_path, seed).unwrap();
        let mut child = std::process::Command::new(&binary)
            .env("PORT", "0")
            .env("KMS_REGION", LOCAL_KMS_REGION)
            .env("KMS_ACCOUNT_ID", LOCAL_KMS_ACCOUNT)
            .env("KMS_DATA_PATH", dir.path().join("data"))
            .env("KMS_SEED_PATH", &seed_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("{}: {err}", binary.display()));
        // It logs where it listens, then a line a request, which this reads on, lest it block on
        // a full pipe; the test's output keeps them.
        let log = child.stderr.take().unwrap();
        let (port, listening) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(log).lines().map_while(Result::ok) {
                eprintln!("local-kms: {line}");
                if let Some((_, at)) = line.split_once("Local KMS started on 0.0.0.0:") {
                    let at = at.split(|c: char| !c.is_ascii_digit()).next().unwrap_or_default();
                    let _ = port.send(at.to_owned());
                }
            }
        });
        let port = listening
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|err| panic!("local-kms said not where it listens: {err}"));
        let endpoint = format!("http://127.0.0.1:{port}");
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let credentials =
            aws_sdk_kms::config::Credentials::new("local-kms", "local-kms", None, None, "tests");
        let config = aws_sdk_kms::Config::builder()
            .behavior_version(aws_sdk_kms::config::BehaviorVersion::latest())
            .region(aws_sdk_kms::config::Region::new(LOCAL_KMS_REGION))
            .credentials_provider(credentials)
            .endpoint_url(&endpoint)
            .build();
        let admin = aws_sdk_kms::Client::from_conf(config);
        Self { child, endpoint, admin, runtime: Some(runtime), _dir: dir }
    }

    /// Its endpoint, `http://127.0.0.1:PORT`.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The URL of a KMS over it, its keys named by `template` (`CLADEFS_KMS`'s, as
    /// `kms::from_url` reads it).
    pub fn url(&self, template: &str) -> String {
        format!("aws-kms://{LOCAL_KMS_REGION}?key={template}&endpoint={}", self.endpoint)
    }

    /// What its ARNs begin with: `arn:aws:kms:REGION:ACCOUNT`.
    pub fn arn_prefix(&self) -> String {
        format!("arn:aws:kms:{LOCAL_KMS_REGION}:{LOCAL_KMS_ACCOUNT}")
    }

    /// A new symmetric key, for encrypting: its ID.
    pub fn create_key(&self) -> String {
        let made = self.run(self.admin.create_key().send()).unwrap();
        made.key_metadata.unwrap().key_id
    }

    /// Makes the alias `alias` (`alias/...`) name the key `key_id`.
    pub fn create_alias(&self, alias: &str, key_id: &str) {
        let call = self.admin.create_alias().alias_name(alias).target_key_id(key_id).send();
        self.run(call).unwrap();
    }

    /// Disables the key `key_id`, as a tenant revoking it would.
    pub fn disable(&self, key_id: &str) {
        self.run(self.admin.disable_key().key_id(key_id).send()).unwrap();
    }

    /// Enables the key `key_id` again.
    pub fn enable(&self, key_id: &str) {
        self.run(self.admin.enable_key().key_id(key_id).send()).unwrap();
    }

    /// Schedules the key `key_id`'s deletion, in a week, as a tenant shredding itself would.
    pub fn schedule_deletion(&self, key_id: &str) {
        let call = self.admin.schedule_key_deletion().key_id(key_id).pending_window_in_days(7);
        self.run(call.send()).unwrap();
    }

    /// `call`, run to its end on its runtime, on a thread of its own, which a test on a tokio
    /// runtime may ask too: block_on refuses a runtime's thread (board 6672). Its runtime is shut
    /// down in the background as it is dropped, which such a test may do too.
    fn run<F: std::future::Future + Send>(&self, call: F) -> F::Output
    where
        F::Output: Send,
    {
        std::thread::scope(|scope| {
            scope.spawn(|| self.runtime.as_ref().expect("dropped").block_on(call)).join().unwrap()
        })
    }
}

impl Drop for LocalAwsKms {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
