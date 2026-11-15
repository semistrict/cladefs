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

/// What each call is tried as at most, waiting before each retry: as Google's client libraries do
/// for `Encrypt` and `Decrypt`, retrying what the service did not answer.
const BACKOFF: [Duration; 2] = [Duration::from_millis(200), Duration::from_secs(1)];

/// The KMS `url` names ([`super::from_url`]).
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    let (ring, names) = parse(url)?;
    let credentials = credentials().map_err(|why| format!("{url}: {why}"))?;
    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|err| format!("{url}: an HTTP client: {err}"))?;
    let quota_project = std::env::var(QUOTA_PROJECT_VAR).ok().filter(|project| !project.is_empty());
    Ok(Arc::new(GcpKms { ring, names, http, credentials, quota_project }))
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
        check_key(name).map_err(|why| format!("{url:?}: tenant {tenant:x}'s key, {why}"))?;
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
    if let Some(bearer) = std::env::var(TOKEN_VAR).ok().filter(|token| !token.is_empty()) {
        return Ok(Arc::new(StaticCredentialProvider::new(GcpCredential { bearer })));
    }
    // A client of a bucket it never calls: its credentials are what it is built for.
    let gcs = GoogleCloudStorageBuilder::from_env().with_bucket_name("cladefs-kms-credentials");
    let gcs = gcs.build().map_err(|err| format!("Application Default Credentials: {err}"))?;
    Ok(gcs.credentials().clone())
}

/// What its calls bind the tenant with: `cladefs-tenant:` and the tenant in hexadecimal.
fn aad(tenant: u64) -> Vec<u8> {
    format!("cladefs-tenant:{tenant:x}").into_bytes()
}

/// The CRC32C of `bytes`, as the KMS has its integrity fields.
fn crc32c(bytes: &[u8]) -> u64 {
    crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, bytes)
}

/// Google Cloud KMS, its tenants' keys in a key ring, or named whole ([`self`]).
struct GcpKms {
    /// The key ring's resource name, `projects/P/locations/L/keyRings/R`.
    ring: String,
    names: KeyNames,
    http: reqwest::Client,
    credentials: GcpCredentialProvider,
    quota_project: Option<String>,
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
            Int64::String(digits) => digits.parse().ok(),
            Int64::Number(number) => Some(*number),
        }
    }
}

/// A call's failure: what it means, and whether trying it again may do better.
struct Failure {
    error: Error,
    transient: bool,
}

impl GcpKms {
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
    /// tried again, after [`BACKOFF`], while it fails as it may not again.
    async fn call(&self, tenant: u64, name: &str, op: Op, body: &[u8]) -> Result<Vec<u8>, Error> {
        let mut waits = BACKOFF.iter();
        loop {
            let failure = match self.attempt(tenant, name, op, body).await {
                Ok(answer) => return Ok(answer),
                Err(failure) => failure,
            };
            match (failure.transient, waits.next()) {
                (true, Some(&wait)) => tokio::time::sleep(wait).await,
                _ => return Err(failure.error),
            }
        }
    }

    async fn attempt(
        &self,
        tenant: u64,
        name: &str,
        op: Op,
        body: &[u8],
    ) -> Result<Vec<u8>, Failure> {
        let failed =
            |why: String, transient| Failure { error: io_error(tenant, name, op, why), transient };
        // The provider's own errors name no token: it retries its own requests.
        let credential = self
            .credentials
            .get_credential()
            .await
            .map_err(|err| failed(format!("credentials: {err}"), false))?;
        let mut request = self
            .http
            .post(format!("{ENDPOINT}{name}:{}", op.method()))
            .bearer_auth(&credential.bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(project) = &self.quota_project {
            request = request.header("x-goog-user-project", project);
        }
        let response = request.send().await.map_err(|err| failed(chain(&err), true))?;
        let status = response.status();
        let answer = response.bytes().await.map_err(|err| failed(chain(&err), true))?;
        let mut owned = answer.to_vec();
        if let Ok(mut unique) = answer.try_into_mut() {
            unique[..].zeroize();
        }
        if status.is_success() {
            return Ok(owned);
        }
        let refused = refusal(tenant, name, op, status.as_u16(), &owned);
        owned.zeroize();
        Err(refused)
    }
}

impl Kms for GcpKms {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        self.wrap_now(tenant, key)
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        self.unwrap_now(tenant, wrapped)
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
}

/// What the KMS's refusal of `op` with key `name` for `tenant`, of HTTP status `code` and body
/// `body`, means (as Cloud KMS answered each, TASK-411.18's notes):
///
/// - `NOT_FOUND`, the key named not found ("CryptoKey NAME not found.", a key ring missing
///   too): the tenant has no KEK, [`Error::NoSuchTenant`]. Any other thing not found (a
///   project, a location) is the URL's, not the tenant's: [`Error::Io`];
/// - `PERMISSION_DENIED`, the tenant took cladefs's role on its key away, or the key is in a
///   project cladefs may not see: [`Error::Revoked`];
/// - `FAILED_PRECONDITION`, a version not enabled ("NAME/cryptoKeyVersions/N is not enabled,
///   current state is: S.", [`version_state`]): `DISABLED` [`Error::Revoked`], `DESTROYED` and
///   `DESTROY_SCHEDULED` [`Error::Destroyed`];
/// - `INVALID_ARGUMENT` of a decrypt, not of a checksum ("Decryption failed."): a ciphertext
///   not the key's, or not of this tenant's associated data, [`Error::Unauthentic`];
/// - anything else, `UNAUTHENTICATED` (cladefs's credentials) and the service's failures among
///   them: [`Error::Io`], transient if the service may answer it next time.
fn refusal(tenant: u64, name: &str, op: Op, code: u16, body: &[u8]) -> Failure {
    let rpc = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| serde_json::from_value::<Status>(value.get("error")?.clone()).ok())
        .unwrap_or_default();
    let reasons = || rpc.details.iter().map(|detail| &detail.reason[..]);
    let refused = |error| Failure { error, transient: false };
    match (&rpc.status[..], version_state(&rpc)) {
        ("NOT_FOUND", _) if rpc.message == format!("CryptoKey {name} not found.") => {
            return refused(Error::NoSuchTenant { tenant });
        }
        ("PERMISSION_DENIED", _) | ("FAILED_PRECONDITION", Some("DISABLED")) => {
            return refused(Error::Revoked { tenant });
        }
        ("FAILED_PRECONDITION", Some("DESTROYED" | "DESTROY_SCHEDULED")) => {
            return refused(Error::Destroyed { tenant });
        }
        ("INVALID_ARGUMENT", _)
            if op == Op::Decrypt && !reasons().any(|reason| reason == "INVALID_CHECKSUM") =>
        {
            return refused(Error::Unauthentic {
                tenant,
                source: Unauthentic("a wrapped key, refused by the KMS"),
            });
        }
        _ => {}
    }
    // Retried as Google's client libraries retry, and a checksum failed in transit.
    let transient = matches!(&rpc.status[..], "UNAVAILABLE" | "DEADLINE_EXCEEDED" | "INTERNAL")
        || (rpc.status.is_empty() && (code == 429 || code >= 500))
        || reasons().any(|r| r == "INVALID_CHECKSUM");
    let status = if rpc.status.is_empty() { "no status" } else { &rpc.status[..] };
    let why = format!("HTTP {code}, {status}: {}", rpc.message);
    Failure { error: io_error(tenant, name, op, why), transient }
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

    const RING: &str = "projects/p-1/locations/us-central1/keyRings/cladefs";

    #[test]
    fn urls_name_a_key_ring_and_its_tenants_keys() {
        let (ring, names) =
            parse(&format!("gcp-kms://{RING}?key=cladefs-tenant-{{tenant}}")).unwrap();
        assert_eq!(ring, RING);
        assert_eq!(qualified(&ring, &names.of(0x2a)), format!("{RING}/cryptoKeys/cladefs-tenant-2a"));
        let (_, percent) = parse(&format!("gcp-kms://{RING}?key=t-%7Btenant%7D")).unwrap();
        assert_eq!(percent.of(1), "t-1");
    }

    #[test]
    fn overrides_name_keys_in_the_ring_or_in_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        let own = "projects/tenant.example:own/locations/europe-west1/keyRings/r/cryptoKeys/k";
        std::fs::write(&keys, format!("7 {own}\n8 their-key\n")).unwrap();
        let url = format!("gcp-kms://{RING}?key=t-{{tenant}}&keys={}", keys.display());
        let (ring, names) = parse(&url).unwrap();
        assert_eq!(qualified(&ring, &names.of(7)), own);
        assert_eq!(qualified(&ring, &names.of(8)), format!("{RING}/cryptoKeys/their-key"));
        assert_eq!(qualified(&ring, &names.of(9)), format!("{RING}/cryptoKeys/t-9"));
    }

    #[test]
    fn urls_naming_no_key_ring_or_no_keys_are_refused() {
        let usage = "gcp-kms://projects/P/locations/L/keyRings/R?key=TEMPLATE[&keys=FILE]";
        let refused = |url: &str| parse(url).unwrap_err();
        let url = "gcp-kms://projects/p/locations/l?key=t-{tenant}";
        assert_eq!(refused(url), format!("{url:?} names no key ring: {usage}"));
        let url = "gcp-kms://projects/P/locations/l/keyRings/r?key=t-{tenant}";
        assert_eq!(refused(url), format!("{url:?}: no project \"P\""));
        let url = "gcp-kms://projects/p/locations/l/keyRings/r%2f?key=t-{tenant}";
        assert_eq!(
            refused(url),
            format!("{url:?}: no key ring \"r%2f\": an ID is 1 to 63 letters, digits, _ and -")
        );
        let url = format!("gcp-kms://{RING}?key=t-{{tenant}}&region=x");
        assert_eq!(refused(&url), format!("{url:?}: no parameter \"region\": {usage}"));
        let url = format!("gcp-kms://{RING}?key=t/{{tenant}}");
        assert_eq!(
            refused(&url),
            format!(
                "{url:?}: the key template, \"t/ffffffffffffffff\", names no key: an ID is 1 to \
                 63 letters, digits, _ and -"
            )
        );
        let url = format!("gcp-kms://{RING}?key={}{{tenant}}", "k".repeat(48));
        assert!(refused(&url).contains("an ID is 1 to 63"), "{}", refused(&url));
        let url = format!("gcp-kms://{RING}");
        assert_eq!(
            refused(&url),
            format!("{url:?}: the KMS URL names no key template: key=...{{tenant}}...")
        );
    }

    #[test]
    fn overrides_naming_no_key_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        std::fs::write(&keys, "a projects/p/locations/l/keyRings/r\n").unwrap();
        let url = format!("gcp-kms://{RING}?key=t-{{tenant}}&keys={}", keys.display());
        assert_eq!(
            parse(&url).unwrap_err(),
            format!(
                "{url:?}: tenant a's key, \"projects/p/locations/l/keyRings/r\", names no key: \
                 projects/P/locations/L/keyRings/R/cryptoKeys/K"
            )
        );
    }

    #[test]
    fn tenants_are_bound_as_associated_data() {
        assert_eq!(aad(0x2a), b"cladefs-tenant:2a");
        // RFC 3720's check value of CRC32C.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    /// The key the bodies Cloud KMS answered with name (TASK-411.18's notes).
    const KEY: &str = "projects/echophase-connectome-test/locations/us-central1/keyRings/cladefs-test/cryptoKeys/cladefs-probe";

    fn mapped(op: Op, code: u16, body: &str) -> (String, bool) {
        let Failure { error, transient } = refusal(9, KEY, op, code, body.as_bytes());
        (error.to_string(), transient)
    }

    /// Cloud KMS's refusals, as it answered them (2026-11-15), and what each means.
    #[test]
    fn refusals_are_mapped_as_cloud_kms_answers_them() {
        let not_found = format!(
            r#"{{"error": {{"code": 404, "message": "CryptoKey {KEY} not found.", "status": "NOT_FOUND"}}}}"#
        );
        assert_eq!(
            mapped(Op::Encrypt, 404, &not_found),
            ("tenant 9: no KEK in the KMS".into(), false)
        );
        assert_eq!(
            mapped(Op::Decrypt, 404, &not_found),
            ("tenant 9: no KEK in the KMS".into(), false)
        );
        let project = r#"{"error": {"code": 404, "message": "Project 'projects/cladefs-no-such-project-0' not found.", "status": "NOT_FOUND"}}"#;
        assert_eq!(
            mapped(Op::Encrypt, 404, project),
            (
                format!(
                    "tenant 9: the KMS failed: Google Cloud KMS, encrypt with {KEY}: HTTP 404, \
                     NOT_FOUND: Project 'projects/cladefs-no-such-project-0' not found."
                ),
                false
            )
        );
        let denied = r#"{"error": {"code": 403, "message": "Permission 'cloudkms.cryptoKeyVersions.useToEncrypt' denied on resource 'projects/cloud-samples-data/locations/global/keyRings/r/cryptoKeys/k' (or it may not exist).", "status": "PERMISSION_DENIED", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "IAM_PERMISSION_DENIED", "domain": "cloudkms.googleapis.com"}]}}"#;
        assert_eq!(
            mapped(Op::Encrypt, 403, denied),
            ("tenant 9: its KEK is revoked".into(), false)
        );
        let state = |state: &str, violation: &str| {
            format!(
                r#"{{"error": {{"code": 400, "message": "{KEY}/cryptoKeyVersions/1 is not enabled, current state is: {state}.", "status": "FAILED_PRECONDITION", "details": [{{"@type": "type.googleapis.com/google.rpc.PreconditionFailure", "violations": [{{"type": "{violation}", "subject": "{KEY}/cryptoKeyVersions/1"}}]}}]}}}}"#
            )
        };
        let destroyed = "tenant 9: its KEK is destroyed";
        let scheduled = state("DESTROY_SCHEDULED", "KEY_DESTROY_SCHEDULED");
        assert_eq!(mapped(Op::Decrypt, 400, &scheduled), (destroyed.into(), false));
        // A version a day past its scheduling, DESTROYED: as the service names the others' states
        // (not seen yet: the tests' versions are scheduled a day ahead).
        assert_eq!(
            mapped(Op::Encrypt, 400, &state("DESTROYED", "KEY_DESTROYED")),
            (destroyed.into(), false)
        );
        // Its message alone says the state, if its details do not.
        let message_only = format!(
            r#"{{"error": {{"code": 400, "message": "{KEY}/cryptoKeyVersions/1 is not enabled, current state is: DESTROYED.", "status": "FAILED_PRECONDITION"}}}}"#
        );
        assert_eq!(mapped(Op::Decrypt, 400, &message_only), (destroyed.into(), false));
        let disabled = state("DISABLED", "KEY_DISABLED");
        assert_eq!(
            mapped(Op::Decrypt, 400, &disabled),
            ("tenant 9: its KEK is revoked".into(), false)
        );
        let unauthentic = "tenant 9: not authentic: a wrapped key, refused by the KMS";
        for body in [
            r#"{"error": {"code": 400, "message": "Decryption failed.", "status": "INVALID_ARGUMENT"}}"#,
            r#"{"error": {"code": 400, "message": "Decryption failed: the ciphertext is invalid.", "status": "INVALID_ARGUMENT"}}"#,
            r#"{"error": {"code": 400, "message": "Decryption failed: verify that 'name' refers to the correct CryptoKey.", "status": "INVALID_ARGUMENT", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "INDECIPHERABLE_CIPHERTEXT_FOR_CRYPTOKEY", "domain": "cloudkms.googleapis.com"}]}}"#,
        ] {
            assert_eq!(mapped(Op::Decrypt, 400, body), (unauthentic.into(), false), "{body}");
        }
        let checksum = r#"{"error": {"code": 400, "message": "The checksum in field ciphertext_crc32c did not match the data in field ciphertext.", "status": "INVALID_ARGUMENT", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "INVALID_CHECKSUM", "domain": "cloudkms.googleapis.com", "metadata": {"data_field": "ciphertext", "checksum_field": "ciphertext_crc32c"}}]}}"#;
        assert_eq!(
            mapped(Op::Decrypt, 400, checksum),
            (
                format!(
                    "tenant 9: the KMS failed: Google Cloud KMS, decrypt with {KEY}: HTTP 400, \
                     INVALID_ARGUMENT: The checksum in field ciphertext_crc32c did not match the \
                     data in field ciphertext."
                ),
                true
            )
        );
        let unauthenticated = r#"{"error": {"code": 401, "message": "Request had invalid authentication credentials.", "status": "UNAUTHENTICATED"}}"#;
        assert_eq!(
            mapped(Op::Encrypt, 401, unauthenticated),
            (
                format!(
                    "tenant 9: the KMS failed: Google Cloud KMS, encrypt with {KEY}: HTTP 401, \
                     UNAUTHENTICATED: Request had invalid authentication credentials."
                ),
                false
            )
        );
    }

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
        let (_, transient) = mapped(Op::Encrypt, 429, "");
        assert!(transient);
        // Encrypting refused as invalid is the request's fault, not the tenant's.
        let invalid = r#"{"error": {"code": 400, "message": "Plaintext too long.", "status": "INVALID_ARGUMENT"}}"#;
        let (why, transient) = mapped(Op::Encrypt, 400, invalid);
        assert!(why.starts_with("tenant 9: the KMS failed: "), "{why}");
        assert!(!transient);
    }
}
