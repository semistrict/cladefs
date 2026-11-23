// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: Google Cloud KMS as a [`Kms`] (TASK-411.18), over its REST API: a tenant's KEK is
//! a symmetric `cryptoKey` (purpose `ENCRYPT_DECRYPT`) the tenant grants cladefs's service account
//! `roles/cloudkms.cryptoKeyEncrypterDecrypter` on; a wrap is its `:encrypt` of the key's 32
//! bytes, an unwrap its `:decrypt`, under whichever of the key's versions encrypted them while
//! the KMS keeps it.
//!
//! Its URL is `gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]`
//! ([`KeyNames`]): a tenant's key is named by the template, a `cryptoKey` id in ring `R`, or by an
//! override, an id there too or a key's whole resource name, `projects/../cryptoKeys/K`, in a
//! project of the tenant's own (BYOK). Each name is checked as the URL is read.
//!
//! Its credentials are Application Default Credentials, as `object_store` has them for cladefs's
//! `gs://` buckets: a service account's key file or a user's credentials at
//! `GOOGLE_APPLICATION_CREDENTIALS`, gcloud's well-known file, or else the metadata server
//! (GCE, GKE's workload identity); or, for development alone, the access token
//! [`TOKEN_VAR`] holds (`gcloud auth print-access-token`), which expires within the hour.
//! `GOOGLE_CLOUD_QUOTA_PROJECT` names the project billed for its calls, if not the key's.
//! Tokens are sent to the KMS alone, and neither they nor keys are logged or in any error. A key's
//! bytes, in base64, are zeroed in the buffers this module owns; the HTTP client's and TLS's own
//! copies are not.
//!
//! Each call binds the tenant as associated data ([`aad`]), so that the KMS refuses what it
//! encrypted for one tenant to another, even under one key; and carries CRC32Cs of what it sends,
//! checked by the KMS, and checks those of what it gets back, so that bytes corrupt in transit
//! fail the call rather than make a key no unwrap gives back. What it keeps of a wrapped key is
//! [`real`]'s: a header naming the backend and the key's resource name, then the KMS's ciphertext.

use super::background::Background;
use super::retry::{self, Failed};
use super::{Error, KeyNames, Kms, real};
use crate::{KEY_LEN, Secret, Unauthentic, Wrapped};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use object_store::StaticCredentialProvider;
use object_store::gcp::{GcpCredential, GcpCredentialProvider, GoogleCloudStorageBuilder};
use serde::Deserialize;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use zeroize::{Zeroize as _, Zeroizing};

/// The scheme of its URLs, and the backend its wrapped keys' headers name.
const SCHEME: &str = "gcp-kms";

/// Where its calls go.
const ENDPOINT: &str = "https://cloudkms.googleapis.com/v1/";

/// For development: an OAuth access token to call the KMS with, in place of Application Default
/// Credentials (`gcloud auth print-access-token`).
pub const TOKEN_VAR: &str = "CLADEFS_KMS_GCP_TOKEN";

/// The project billed for its calls, if not the one each key is in: Google's own client libraries'
/// variable.
const QUOTA_PROJECT_VAR: &str = "GOOGLE_CLOUD_QUOTA_PROJECT";

/// What a call waits for an answer at most, and for a connection.
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The KMS `url` names ([`super::from_url`]).
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    let (ring, names) = parse(url)?;
    let credentials = credentials().map_err(|why| format!("{url}: {why}"))?;
    // Unset, or not UTF-8, which no project is: none.
    let quota_project = std::env::var(QUOTA_PROJECT_VAR).ok().filter(|project| !project.is_empty());
    let kms = GcpKms::new(ENDPOINT, ring, names, credentials, quota_project);
    Ok(Arc::new(kms.map_err(|why| format!("{url}: {why}"))?))
}

/// The key ring `url` names and its tenants' keys' names, each checked.
fn parse(url: &str) -> Result<(String, KeyNames), String> {
    let usage = "gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]";
    let rest = url.strip_prefix("gcp-kms://").expect("a gcp-kms:// URL");
    let (ring, query) = rest.split_once('?').unwrap_or((rest, ""));
    let segments: Vec<&str> = ring.split('/').collect();
    let ["projects", project, "locations", location, "keyRings", id] = segments[..] else {
        return Err(format!("{url:?} names no key ring: {usage}"));
    };
    check_ring(project, location, id).map_err(|why| format!("{url:?}: {why}"))?;
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    if let Some((name, _)) = query.iter().find(|(name, _)| !matches!(&name[..], "key" | "keys")) {
        return Err(format!("{url:?}: no parameter {name:?}: {usage}"));
    }
    let names = KeyNames::of_query(query.iter().map(|(name, value)| (&name[..], &value[..])))
        .map_err(|why| format!("{url:?}: {why}"))?;
    // The template, as the longest tenant names it: its other tenants' names are as valid.
    check_key(&names.of(u64::MAX)).map_err(|why| format!("{url:?}: the key template, {why}"))?;
    for (tenant, name) in names.overridden() {
        check_key(name)
            .map_err(|why| format!("{url:?}: tenant {}'s key, {why}", base32_id::Short(tenant)))?;
    }
    Ok((ring.to_owned(), names))
}

/// Checks a key ring's resource name, of `project`, `location` and `id`, is one Cloud KMS has.
fn check_ring(project: &str, location: &str, id: &str) -> Result<(), String> {
    // A project's ID: lowercase letters, digits and hyphens, or a domain's, `example.com:id`.
    let project_char = |c: char| matches!(c, 'a'..='z' | '0'..='9' | '-' | '.' | ':');
    if project.is_empty() || !project.chars().all(project_char) {
        return Err(format!("no project {project:?}"));
    }
    let location_char = |c: char| matches!(c, 'a'..='z' | '0'..='9' | '-');
    if location.is_empty() || !location.chars().all(location_char) {
        return Err(format!("no location {location:?}"));
    }
    check_id(id).map_err(|why| format!("no key ring {id:?}: {why}"))
}

/// Checks `id` is a key ring's or a key's: 1 to 63 letters, digits, `_` and `-`.
fn check_id(id: &str) -> Result<(), &'static str> {
    let id_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-');
    match (1..=63).contains(&id.len()) && id.chars().all(id_char) {
        true => Ok(()),
        false => Err("an ID is 1 to 63 letters, digits, _ and -"),
    }
}

/// Checks `name`, a key's id in the URL's ring or its whole resource name, names one Cloud KMS
/// has.
fn check_key(name: &str) -> Result<(), String> {
    let refused = |why: &str| format!("{name:?}, names no key: {why}");
    if !name.starts_with("projects/") {
        return check_id(name).map_err(refused);
    }
    let segments: Vec<&str> = name.split('/').collect();
    let ["projects", project, "locations", location, "keyRings", ring_id, "cryptoKeys", id] =
        segments[..]
    else {
        return Err(refused("projects/P/locations/L/keyRings/R/cryptoKeys/K"));
    };
    check_ring(project, location, ring_id).map_err(|why| refused(&why))?;
    check_id(id).map_err(refused)
}

/// The resource name of the key `name` names, an id in `ring` or a resource name itself.
fn qualified(ring: &str, name: &str) -> String {
    match name.starts_with("projects/") {
        true => name.to_owned(),
        false => format!("{ring}/cryptoKeys/{name}"),
    }
}

/// The credentials its calls carry: [`TOKEN_VAR`]'s token, if it has one, else Application
/// Default Credentials, found as `object_store` finds them for a `gs://` bucket.
fn credentials() -> Result<GcpCredentialProvider, String> {
    // Unset, or not UTF-8, which no token is: Application Default Credentials.
    if let Some(bearer) = std::env::var(TOKEN_VAR).ok().filter(|token| !token.is_empty()) {
        return Ok(Arc::new(StaticCredentialProvider::new(GcpCredential { bearer })));
    }
    // A client of a bucket it never calls: its credentials are what it is built for.
    let gcs = GoogleCloudStorageBuilder::from_env().with_bucket_name("cladefs-kms-credentials");
    let gcs = gcs.build().map_err(|err| format!("Application Default Credentials: {err}"))?;
    Ok(gcs.credentials().clone())
}

/// What its calls bind the tenant with: `cladefs-tenant:` and the tenant in base 32.
fn aad(tenant: u64) -> Vec<u8> {
    format!("cladefs-tenant:{}", base32_id::Short(tenant)).into_bytes()
}

/// The CRC32C of `bytes`, as the KMS has its integrity fields.
fn crc32c(bytes: &[u8]) -> u64 {
    crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, bytes)
}

/// Google Cloud KMS, its tenants' keys in a key ring, or named whole ([`self`]). Its calls run on
/// a runtime of its own (board 8477, [`Background`]).
struct GcpKms {
    inner: Arc<Inner>,
    runtime: Background,
}

/// What [`GcpKms`]'s calls share, on its runtime.
struct Inner {
    /// Where its calls go: [`ENDPOINT`], but in tests.
    endpoint: String,
    /// The key ring's resource name, `projects/P/locations/L/keyRings/R`.
    ring: String,
    names: KeyNames,
    http: reqwest::Client,
    credentials: GcpCredentialProvider,
    quota_project: Option<String>,
}

impl GcpKms {
    /// The KMS at `endpoint` ([`ENDPOINT`]), its tenants' keys in `ring` named by `names`, called
    /// with `credentials`, billing `quota_project` if given.
    fn new(
        endpoint: &str,
        ring: String,
        names: KeyNames,
        credentials: GcpCredentialProvider,
        quota_project: Option<String>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|err| format!("an HTTP client: {err}"))?;
        let runtime =
            Background::new("cladefs-gcp-kms").map_err(|err| format!("its runtime: {err}"))?;
        let endpoint = endpoint.to_owned();
        let inner = Inner { endpoint, ring, names, http, credentials, quota_project };
        Ok(Self { inner: Arc::new(inner), runtime })
    }
}

/// What a call asks of the KMS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Encrypt,
    Decrypt,
}

impl Op {
    fn method(self) -> &'static str {
        match self {
            Op::Encrypt => "encrypt",
            Op::Decrypt => "decrypt",
        }
    }
}

/// `EncryptRequest`, as JSON.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EncryptRequest<'a> {
    plaintext: &'a str,
    plaintext_crc32c: String,
    additional_authenticated_data: &'a str,
    additional_authenticated_data_crc32c: String,
}

/// `EncryptResponse`, what of it is read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncryptResponse {
    name: String,
    ciphertext: String,
    ciphertext_crc32c: Option<Int64>,
    #[serde(default)]
    verified_plaintext_crc32c: bool,
    #[serde(default)]
    verified_additional_authenticated_data_crc32c: bool,
}

/// `DecryptRequest`, as JSON.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DecryptRequest<'a> {
    ciphertext: &'a str,
    ciphertext_crc32c: String,
    additional_authenticated_data: &'a str,
    additional_authenticated_data_crc32c: String,
}

/// `DecryptResponse`, what of it is read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecryptResponse {
    #[serde(default)]
    plaintext: String,
    plaintext_crc32c: Option<Int64>,
}

/// A protobuf `int64`, as JSON has it: a string of decimal digits, or a number.
#[derive(Deserialize)]
#[serde(untagged)]
enum Int64 {
    String(String),
    Number(u64),
}

impl Int64 {
    fn value(&self) -> Option<u64> {
        match self {
            // Not a number: none, which its caller refuses.
            Int64::String(digits) => digits.parse().ok(),
            Int64::Number(number) => Some(*number),
        }
    }
}

impl Inner {
    async fn wrap_now(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let name = qualified(&self.ring, &self.names.of(tenant));
        let aad = aad(tenant);
        let (plaintext, aad_base64) =
            (Zeroizing::new(BASE64.encode(key.expose())), BASE64.encode(&aad));
        let request = EncryptRequest {
            plaintext: &plaintext,
            plaintext_crc32c: crc32c(key.expose()).to_string(),
            additional_authenticated_data: &aad_base64,
            additional_authenticated_data_crc32c: crc32c(&aad).to_string(),
        };
        let body = Zeroizing::new(serde_json::to_vec(&request).expect("a request serializes"));
        let answer = self.call(tenant, &name, Op::Encrypt, &body).await?;
        let corrupt = |why: &str| io_error(tenant, &name, Op::Encrypt, format!("its answer {why}"));
        let response: EncryptResponse = serde_json::from_slice(&answer)
            .map_err(|err| corrupt(&format!("is not an EncryptResponse: {err}")))?;
        if !(response.verified_plaintext_crc32c
            && response.verified_additional_authenticated_data_crc32c)
        {
            return Err(corrupt("verified not the request's CRC32Cs"));
        }
        if !response
            .name
            .strip_prefix(&name)
            .is_some_and(|version| version.starts_with("/cryptoKeyVersions/"))
        {
            return Err(corrupt(&format!("names another key's version, {:?}", response.name)));
        }
        let ciphertext = BASE64
            .decode(&response.ciphertext)
            .map_err(|err| corrupt(&format!("holds no ciphertext in base64: {err}")))?;
        if response.ciphertext_crc32c.as_ref().and_then(Int64::value) != Some(crc32c(&ciphertext)) {
            return Err(corrupt("holds a ciphertext not of its CRC32C"));
        }
        Ok(real::wrapped(SCHEME, &name, &ciphertext))
    }

    async fn unwrap_now(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let name = qualified(&self.ring, &self.names.of(tenant));
        let unauthentic = |source| Error::Unauthentic { tenant, source };
        let ciphertext = real::ciphertext(wrapped, SCHEME, &name).map_err(unauthentic)?;
        let aad = aad(tenant);
        let (ciphertext_base64, aad_base64) = (BASE64.encode(ciphertext), BASE64.encode(&aad));
        let request = DecryptRequest {
            ciphertext: &ciphertext_base64,
            ciphertext_crc32c: crc32c(ciphertext).to_string(),
            additional_authenticated_data: &aad_base64,
            additional_authenticated_data_crc32c: crc32c(&aad).to_string(),
        };
        let body = serde_json::to_vec(&request).expect("a request serializes");
        let mut answer = self.call(tenant, &name, Op::Decrypt, &body).await?;
        let corrupt = |why: &str| io_error(tenant, &name, Op::Decrypt, format!("its answer {why}"));
        let parsed = serde_json::from_slice::<DecryptResponse>(&answer);
        answer.zeroize();
        let mut response =
            parsed.map_err(|err| corrupt(&format!("is not a DecryptResponse: {err}")))?;
        let decoded = BASE64.decode(&response.plaintext);
        response.plaintext.zeroize();
        let plaintext =
            Zeroizing::new(decoded.map_err(|_| corrupt("holds no plaintext in base64"))?);
        if response.plaintext_crc32c.as_ref().and_then(Int64::value) != Some(crc32c(&plaintext)) {
            return Err(corrupt("holds a plaintext not of its CRC32C"));
        }
        // Only cladefs encrypts with its tenants' associated data, and only keys.
        let key = <&[u8; KEY_LEN]>::try_from(&plaintext[..])
            .map_err(|_| unauthentic(Unauthentic("a wrapped key, of the wrong length")))?;
        Ok(Secret::from_bytes(key))
    }

    /// The body of the KMS's answer to `op` with key `name` for `tenant`, of request `body`:
    /// tried again while it fails as it may not next time, the service throttling it among them
    /// ([`retry`], board 8473), as Google's client libraries retry `Encrypt` and `Decrypt`.
    async fn call(&self, tenant: u64, name: &str, op: Op, body: &[u8]) -> Result<Vec<u8>, Error> {
        retry::retried(|| self.attempt(tenant, name, op, body)).await
    }

    async fn attempt(
        &self,
        tenant: u64,
        name: &str,
        op: Op,
        body: &[u8],
    ) -> Result<Vec<u8>, Failed<Error>> {
        let failed = |why: String| io_error(tenant, name, op, why);
        // The provider's own errors name no token: it retries its own requests.
        let credential = self
            .credentials
            .get_credential()
            .await
            .map_err(|err| Failed::refused(failed(format!("credentials: {err}"))))?;
        let mut request = self
            .http
            .post(format!("{}{name}:{}", self.endpoint, op.method()))
            .bearer_auth(&credential.bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(project) = &self.quota_project {
            request = request.header("x-goog-user-project", project);
        }
        let transport = |err: reqwest::Error| Failed::transient(failed(chain(&err)), None);
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let after = retry::retry_after(response.headers());
        let answer = response.bytes().await.map_err(transport)?;
        let mut owned = answer.to_vec();
        if let Ok(mut unique) = answer.try_into_mut() {
            unique[..].zeroize();
        }
        if status.is_success() {
            return Ok(owned);
        }
        let refused = refusal(tenant, name, op, status.as_u16(), &owned);
        owned.zeroize();
        Err(Failed { after, ..refused })
    }
}

impl Kms for GcpKms {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        let (inner, key) = (self.inner.clone(), Secret::from_bytes(key.expose()));
        self.runtime.run(tenant, async move { inner.wrap_now(tenant, &key).await })
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        let (inner, wrapped) = (self.inner.clone(), wrapped.clone());
        self.runtime.run(tenant, async move { inner.unwrap_now(tenant, &wrapped).await })
    }
}

/// `google.rpc.Status`, as the KMS answers a call it refuses, what of it is read.
#[derive(Default, Deserialize)]
struct Status {
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    details: Vec<Detail>,
}

/// One of a [`Status`]'s details: an `ErrorInfo`'s reason, a `PreconditionFailure`'s violations.
#[derive(Deserialize)]
struct Detail {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    violations: Vec<Violation>,
}

#[derive(Deserialize)]
struct Violation {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    subject: String,
}

/// What the KMS's refusal of `op` with key `name` for `tenant`, of HTTP status `code` and body
/// `body`, means (as Cloud KMS answered each, TASK-411.18's notes):
///
/// - 429 or 5xx, whatever its body: the service throttled the call (`RESOURCE_EXHAUSTED`, its
///   quota) or failed, which says nothing of the tenant's key: [`Error::Io`], tried again (board
///   8473);
/// - `NOT_FOUND`, the key named not found ("CryptoKey NAME not found.", a key ring missing
///   too): the tenant has no KEK, [`Error::NoSuchTenant`]. Any other thing not found (a
///   project, a location) is the URL's, not the tenant's: [`Error::Io`];
/// - `PERMISSION_DENIED` of IAM (`ErrorInfo` reason `IAM_PERMISSION_DENIED`, or none given), the
///   tenant took cladefs's role on its key away, or the key is in a project cladefs may not see:
///   [`Error::Revoked`]. Of any other reason, the host's or its project's configuration, not the
///   tenant's key: the KMS API disabled, billing, VPC Service Controls, the quota project, the
///   token's scopes ([`denial`], board 8481): [`Error::Io`], naming the reason;
/// - `FAILED_PRECONDITION`, a version not enabled ("NAME/cryptoKeyVersions/N is not enabled,
///   current state is: S.", [`version_state`]): of an encrypt, the key's primary version, so the
///   tenant's KEK: `DISABLED` [`Error::Revoked`], `DESTROYED` and `DESTROY_SCHEDULED`
///   [`Error::Destroyed`]; of a decrypt, the version that wrapped the key alone, its loss no
///   other cell's (board 8479): [`Error::VersionDisabled`], [`Error::VersionGone`];
/// - `INVALID_ARGUMENT` of a decrypt, not of a checksum ("Decryption failed."): a ciphertext
///   not the key's, or not of this tenant's associated data, [`Error::Unauthentic`];
/// - anything else, `UNAUTHENTICATED` (cladefs's credentials) among them: [`Error::Io`], transient
///   if the service may answer it next time.
fn refusal(tenant: u64, name: &str, op: Op, code: u16, body: &[u8]) -> Failed<Error> {
    // Not JSON, or not Google's error: the default, the status alone saying what failed.
    let rpc = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| serde_json::from_value::<Status>(value.get("error")?.clone()).ok())
        .unwrap_or_default();
    let status = if rpc.status.is_empty() { "no status" } else { &rpc.status[..] };
    let failed = |why: String| io_error(tenant, name, op, format!("HTTP {code}, {status}: {why}"));
    if retry::transient_status(code) {
        return Failed::transient(failed(rpc.message.clone()), None);
    }
    let reasons = || rpc.details.iter().map(|detail| &detail.reason[..]);
    let checksum = reasons().any(|reason| reason == "INVALID_CHECKSUM");
    let refused = Failed::refused;
    let version = || version_of(&rpc);
    match (&rpc.status[..], version_state(&rpc), op) {
        ("NOT_FOUND", ..) if rpc.message == format!("CryptoKey {name} not found.") => {
            refused(Error::NoSuchTenant { tenant })
        }
        ("PERMISSION_DENIED", ..) => match denial(&rpc) {
            None => refused(Error::Revoked { tenant }),
            Some(reason) => refused(failed(format!(
                "{reason}, the host's or its project's, not the tenant's key: {}",
                rpc.message
            ))),
        },
        ("FAILED_PRECONDITION", Some("DISABLED"), Op::Encrypt) => {
            refused(Error::Revoked { tenant })
        }
        ("FAILED_PRECONDITION", Some("DESTROYED" | "DESTROY_SCHEDULED"), Op::Encrypt) => {
            refused(Error::Destroyed { tenant })
        }
        ("FAILED_PRECONDITION", Some("DISABLED"), Op::Decrypt) => {
            refused(Error::VersionDisabled { tenant, version: version() })
        }
        ("FAILED_PRECONDITION", Some("DESTROYED" | "DESTROY_SCHEDULED"), Op::Decrypt) => {
            refused(Error::VersionGone { tenant, version: version() })
        }
        ("INVALID_ARGUMENT", _, Op::Decrypt) if !checksum => refused(Error::Unauthentic {
            tenant,
            source: Unauthentic("a wrapped key, refused by the KMS"),
        }),
        // Retried as Google's client libraries retry, and a checksum failed in transit.
        (status, ..) => Failed {
            transient: matches!(
                status,
                "UNAVAILABLE" | "DEADLINE_EXCEEDED" | "INTERNAL" | "RESOURCE_EXHAUSTED"
            ) || checksum,
            ..refused(failed(rpc.message.clone()))
        },
    }
}

/// The reason a `PERMISSION_DENIED` gives that is not IAM's refusal of the key, if it gives one:
/// its `ErrorInfo`'s (`SERVICE_DISABLED`, `BILLING_DISABLED`, `SECURITY_POLICY_VIOLATED`,
/// `USER_PROJECT_DENIED`, `ACCESS_TOKEN_SCOPE_INSUFFICIENT`, ...), the host's or its project's
/// configuration (board 8481). None for `IAM_PERMISSION_DENIED`, or no reason at all, as Cloud KMS
/// refuses a key whose role the tenant took away (TASK-411.18's notes; hotbed question 456).
fn denial(rpc: &Status) -> Option<&str> {
    let mut reasons = rpc.details.iter().map(|detail| &detail.reason[..]);
    reasons.find(|reason| !reason.is_empty() && *reason != "IAM_PERMISSION_DENIED")
}

/// The state of the key version a refusal says is not enabled: its `PreconditionFailure`'s
/// violation, `KEY_` and the state, or else its message's "current state is: S.".
fn version_state(rpc: &Status) -> Option<&str> {
    let violations = rpc.details.iter().flat_map(|detail| &detail.violations);
    let violated = violations.filter_map(|violation| violation.kind.strip_prefix("KEY_")).next();
    violated.or_else(|| {
        let (_, state) = rpc.message.rsplit_once("current state is: ")?;
        Some(state.trim_end_matches('.'))
    })
}

/// The number of the key version a refusal says is not enabled: of its violation's subject, or
/// its message, `.../cryptoKeyVersions/N`; `unknown` if neither names one.
fn version_of(rpc: &Status) -> String {
    let subjects = rpc.details.iter().flat_map(|detail| &detail.violations);
    let mut named = subjects.map(|violation| &violation.subject[..]).chain([&rpc.message[..]]);
    let number = named.find_map(|text| {
        let (_, after) = text.split_once("/cryptoKeyVersions/")?;
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty()).then_some(digits)
    });
    number.unwrap_or_else(|| "unknown".to_owned())
}

/// The [`Error::Io`] of `op` with key `name` for `tenant`, failing as `why` says.
fn io_error(tenant: u64, name: &str, op: Op, why: String) -> Error {
    let source = io::Error::other(format!("Google Cloud KMS, {} with {name}: {why}", op.method()));
    Error::Io { tenant, source }
}

/// `error` and its sources, each after the one before: a request's failure says why only so. A
/// request's URL, which reqwest names, is the key's, no secret.
fn chain(error: &dyn std::error::Error) -> String {
    let mut why = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        why.push_str(": ");
        why.push_str(&cause.to_string());
        source = cause.source();
    }
    why
}

/// Against real Cloud KMS, given an access token.
#[cfg(test)]
mod service;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::fake_http::{Request, Response, Server};
    use futures::executor::block_on;
    use std::sync::Mutex;

    /// The key the bodies Cloud KMS answered with name (TASK-411.18's notes).
    const KEY: &str = "projects/echophase-connectome-test/locations/us-central1/keyRings/fxdax-test/cryptoKeys/fxdax-probe";

    fn mapped(op: Op, code: u16, body: &str) -> (String, bool) {
        let Failed { error, transient, .. } = refusal(9, KEY, op, code, body.as_bytes());
        (error.to_string(), transient)
    }

    /// The statuses `google.rpc.Code` has, and none.
    const STATUSES: [&str; 18] = [
        "",
        "OK",
        "CANCELLED",
        "UNKNOWN",
        "INVALID_ARGUMENT",
        "DEADLINE_EXCEEDED",
        "NOT_FOUND",
        "ALREADY_EXISTS",
        "PERMISSION_DENIED",
        "RESOURCE_EXHAUSTED",
        "FAILED_PRECONDITION",
        "ABORTED",
        "OUT_OF_RANGE",
        "UNIMPLEMENTED",
        "INTERNAL",
        "UNAVAILABLE",
        "DATA_LOSS",
        "UNAUTHENTICATED",
    ];

    /// board 8473: the service throttling (429) or failing (5xx) is tried again whatever its body
    /// says, a status among them: Cloud KMS's quota refusal as it answers it, and every pair of a
    /// 429 or 5xx and a status, of either call, is [`Error::Io`] and transient. Before, only a 429
    /// of no body was: the test fed an empty body alone.
    #[test]
    fn the_services_failures_are_io_and_retried() {
        let unavailable = r#"{"error": {"code": 503, "message": "The service is currently unavailable.", "status": "UNAVAILABLE"}}"#;
        let (why, transient) = mapped(Op::Decrypt, 503, unavailable);
        assert_eq!(
            why,
            format!(
                "tenant 9: the KMS failed: Google Cloud KMS, decrypt with {KEY}: HTTP 503, \
                 UNAVAILABLE: The service is currently unavailable."
            )
        );
        assert!(transient);
        let (why, transient) = mapped(Op::Encrypt, 502, "<html>Bad Gateway</html>");
        assert_eq!(
            why,
            format!(
                "tenant 9: the KMS failed: Google Cloud KMS, encrypt with {KEY}: HTTP 502, no status: "
            )
        );
        assert!(transient);
        // Cloud KMS's refusal past its quota (cloudkms.googleapis.com/crypto_requests), its shape.
        let throttled = r#"{"error": {"code": 429, "message": "Quota exceeded for quota metric 'Cryptographic requests' and limit 'Cryptographic requests per minute' of service 'cloudkms.googleapis.com' for consumer 'project_number:1'.", "status": "RESOURCE_EXHAUSTED", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED", "domain": "googleapis.com"}]}}"#;
        let (why, transient) = mapped(Op::Encrypt, 429, throttled);
        assert!(why.contains("HTTP 429, RESOURCE_EXHAUSTED: Quota exceeded"), "{why}");
        assert!(transient);
        for op in [Op::Encrypt, Op::Decrypt] {
            for code in [429, 500, 501, 502, 503, 504, 599] {
                for status in STATUSES {
                    let body = format!(
                        r#"{{"error": {{"code": {code}, "message": "m", "status": "{status}"}}}}"#
                    );
                    let Failed { error, transient, .. } =
                        refusal(9, KEY, op, code, body.as_bytes());
                    assert!(
                        transient && matches!(error, Error::Io { tenant: 9, .. }),
                        "{op:?} {code} {status}: {error}"
                    );
                }
            }
        }
        // Encrypting refused as invalid is the request's fault, not the tenant's.
        let invalid = r#"{"error": {"code": 400, "message": "Plaintext too long.", "status": "INVALID_ARGUMENT"}}"#;
        let (why, transient) = mapped(Op::Encrypt, 400, invalid);
        assert!(why.starts_with("tenant 9: the KMS failed: "), "{why}");
        assert!(!transient);
    }

    /// board 8481: a `PERMISSION_DENIED` is the tenant's revocation only as IAM's refusal of the
    /// key, `IAM_PERMISSION_DENIED` (as TASK-411.18 recorded it) or no reason given; any other
    /// reason is the host's or its project's configuration: the error says which, never revoked,
    /// and is not tried again.
    #[test]
    fn denials_of_the_hosts_configuration_are_not_revocations() {
        let denied = |reason: &str| {
            let details = match reason {
                "" => String::new(),
                reason => format!(
                    r#", "details": [{{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "{reason}", "domain": "googleapis.com"}}]"#
                ),
            };
            format!(
                r#"{{"error": {{"code": 403, "message": "Permission denied.", "status": "PERMISSION_DENIED"{details}}}}}"#
            )
        };
        for op in [Op::Encrypt, Op::Decrypt] {
            for reason in ["IAM_PERMISSION_DENIED", ""] {
                assert_eq!(
                    mapped(op, 403, &denied(reason)),
                    ("tenant 9: its KEK is revoked".into(), false)
                );
            }
            for reason in [
                "SERVICE_DISABLED",
                "API_DISABLED",
                "BILLING_DISABLED",
                "SECURITY_POLICY_VIOLATED",
                "USER_PROJECT_DENIED",
                "CONSUMER_INVALID",
                "ACCESS_TOKEN_SCOPE_INSUFFICIENT",
            ] {
                let (why, transient) = mapped(op, 403, &denied(reason));
                assert_eq!(
                    why,
                    format!(
                        "tenant 9: the KMS failed: Google Cloud KMS, {} with {KEY}: HTTP 403, \
                         PERMISSION_DENIED: {reason}, the host's or its project's, not the \
                         tenant's key: Permission denied.",
                        op.method()
                    )
                );
                assert!(!transient);
            }
        }
    }

    /// board 8479: a version not enabled, of a decrypt, is the version's that wrapped the key, named,
    /// never the tenant's KEK; of an encrypt, the key's primary version, so the tenant's KEK, as
    /// before.
    #[test]
    fn versions_not_enabled_refuse_their_keys_alone() {
        let body = |state: &str| {
            format!(
                r#"{{"error": {{"code": 400, "message": "{KEY}/cryptoKeyVersions/3 is not enabled, current state is: {state}.", "status": "FAILED_PRECONDITION", "details": [{{"@type": "type.googleapis.com/google.rpc.PreconditionFailure", "violations": [{{"type": "KEY_{state}", "subject": "{KEY}/cryptoKeyVersions/3", "description": "not enabled"}}]}}]}}}}"#
            )
        };
        let gone = "tenant 9: version 3 of its KEK, which wrapped this key, is destroyed";
        let disabled = "tenant 9: version 3 of its KEK, which wrapped this key, is disabled";
        for (state, decrypt, encrypt) in [
            ("DESTROYED", gone, "tenant 9: its KEK is destroyed"),
            ("DESTROY_SCHEDULED", gone, "tenant 9: its KEK is destroyed"),
            ("DISABLED", disabled, "tenant 9: its KEK is revoked"),
        ] {
            assert_eq!(mapped(Op::Decrypt, 400, &body(state)), (decrypt.into(), false));
            assert_eq!(mapped(Op::Encrypt, 400, &body(state)), (encrypt.into(), false));
        }
        // Its message alone naming the version, as TASK-411.18's notes have it.
        let bare = format!(
            r#"{{"error": {{"code": 400, "message": "{KEY}/cryptoKeyVersions/12 is not enabled, current state is: DESTROYED.", "status": "FAILED_PRECONDITION"}}}}"#
        );
        let (why, _) = mapped(Op::Decrypt, 400, &bare);
        assert_eq!(why, "tenant 9: version 12 of its KEK, which wrapped this key, is destroyed");
    }

    /// A fake of Cloud KMS: keys `k-{tenant}` of ring `r`, of version 1, encrypting by reversing
    /// the plaintext, then the associated data; the first `throttle` calls answered as its quota
    /// refuses them, with `after` as their `Retry-After` if given.
    #[derive(Default)]
    struct Fake {
        calls: u64,
        throttle: u64,
        after: Option<&'static str>,
    }

    const RING: &str = "projects/p/locations/l/keyRings/r";

    fn answer(fake: &Mutex<Fake>, request: &Request) -> Response {
        let mut fake = fake.lock().unwrap();
        fake.calls += 1;
        if fake.calls <= fake.throttle {
            let body = r#"{"error": {"code": 429, "message": "Quota exceeded.", "status": "RESOURCE_EXHAUSTED"}}"#;
            let response = Response::of(429, body);
            return match fake.after {
                Some(after) => response.with("retry-after", after),
                None => response,
            };
        }
        assert_eq!(request.header("authorization"), Some("Bearer t"));
        let (name, method) =
            request.route().strip_prefix("/v1/").unwrap().rsplit_once(':').unwrap();
        let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let field = |name: &str| BASE64.decode(json[name].as_str().unwrap()).unwrap();
        let aad = field("additionalAuthenticatedData");
        let ciphertext = match method {
            "encrypt" => {
                let mut sealed = field("plaintext");
                sealed.reverse();
                sealed.extend_from_slice(&aad);
                sealed
            }
            _ => {
                let sealed = field("ciphertext");
                let (key, bound) = sealed.split_at(KEY_LEN);
                if bound != aad {
                    let body = r#"{"error": {"code": 400, "message": "Decryption failed.", "status": "INVALID_ARGUMENT"}}"#;
                    return Response::of(400, body);
                }
                let mut key = key.to_vec();
                key.reverse();
                let plaintext = BASE64.encode(&key);
                let crc = crc32c(&key);
                return Response::of(
                    200,
                    format!(r#"{{"plaintext":"{plaintext}","plaintextCrc32c":"{crc}"}}"#),
                );
            }
        };
        let (encoded, crc) = (BASE64.encode(&ciphertext), crc32c(&ciphertext));
        Response::of(
            200,
            format!(
                r#"{{"name":"{name}/cryptoKeyVersions/1","ciphertext":"{encoded}","ciphertextCrc32c":"{crc}","verifiedPlaintextCrc32c":true,"verifiedAdditionalAuthenticatedDataCrc32c":true}}"#
            ),
        )
    }

    /// The fake, and a backend over it.
    fn over(fake: &Arc<Mutex<Fake>>) -> (Server, GcpKms) {
        let fake = fake.clone();
        let server = Server::start(move |_, request| answer(&fake, request));
        let credentials: GcpCredentialProvider =
            Arc::new(StaticCredentialProvider::new(GcpCredential { bearer: "t".into() }));
        let names = KeyNames::new("k-{tenant}", None).unwrap();
        let endpoint = format!("{}/v1/", server.base());
        let kms = GcpKms::new(&endpoint, RING.into(), names, credentials, None).unwrap();
        (server, kms)
    }

    /// board 8473, AC #2: a burst of wraps at once, the first calls refused as past the KMS's
    /// quota, a `Retry-After` of none: each wrap backs off and tries again until all succeed, each
    /// unwrapping, every call refused made again.
    #[test]
    fn throttled_bursts_back_off_until_every_call_succeeds() {
        let fake = Arc::new(Mutex::new(Fake { throttle: 12, after: Some("0"), ..Fake::default() }));
        let (_server, kms) = over(&fake);
        let keys: Vec<Secret> = (0..8).map(|_| Secret::random()).collect();
        let wraps = keys.iter().map(|key| kms.wrap(9, key));
        let wrapped = block_on(futures::future::join_all(wraps));
        assert_eq!(fake.lock().unwrap().calls, 8 + 12);
        for (key, wrapped) in keys.iter().zip(wrapped) {
            let unwrapped = block_on(kms.unwrap(9, &wrapped.unwrap())).unwrap();
            assert_eq!(unwrapped.expose(), key.expose());
        }
    }

    /// board 8473, AC #2: a `Retry-After` the KMS sends is waited before the call is made again.
    #[test]
    fn retry_after_is_honoured() {
        let fake = Arc::new(Mutex::new(Fake { throttle: 1, after: Some("1"), ..Fake::default() }));
        let (_server, kms) = over(&fake);
        let started = std::time::Instant::now();
        block_on(kms.wrap(9, &Secret::random())).unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1), "{:?}", started.elapsed());
        assert_eq!(fake.lock().unwrap().calls, 2);
    }

    /// board 8477: its calls run on a runtime of its own: made on a runtime then dropped, then
    /// polled by an executor that is no runtime at all, they succeed, the client's connections
    /// never the dropped runtime's. Polled by `block_on` on its caller's runtime, reqwest would
    /// find no reactor; on the dropped one's connections, a transport error.
    #[test]
    fn calls_run_on_any_executor_after_their_callers_runtime_is_gone() {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let (_server, kms) = over(&fake);
        let key = Secret::random();
        let caller = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let wrapped = caller.block_on(kms.wrap(9, &key)).unwrap();
        drop(caller);
        assert_eq!(block_on(kms.unwrap(9, &wrapped)).unwrap().expose(), key.expose());
        assert_eq!(fake.lock().unwrap().calls, 2, "a call made again");
    }
}
