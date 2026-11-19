// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: Azure Key Vault as a [`Kms`] (TASK-633.6), over its REST API: a tenant's KEK is
//! a key in a vault (an RSA key, or a Managed HSM's symmetric key) the tenant grants cladefs's
//! identity `wrapKey` and `unwrapKey` on (`Key Vault Crypto Service Encryption User`); a wrap is
//! its `wrapkey` of the key's 32 bytes, under the key's current version, an unwrap its
//! `unwrapkey` under the version that wrapped them, while the vault keeps it.
//!
//! Its URL is `azure-kv://VAULT_HOST?key=TEMPLATE[&keys=FILE][&alg=ALG][&endpoint=URL]`
//! ([`KeyNames`]): `VAULT_HOST` the vault's, `myvault.vault.azure.net`; a tenant's key named by the
//! template, a key's name in that vault, or by an override, a name there too or a key's whole
//! identifier, `https://THEIR_VAULT/keys/NAME`, in a vault of the tenant's own (BYOK). `alg` is the
//! wrapping algorithm, `RSA-OAEP-256` (the default, for RSA keys), `A256KW` or `CKM_AES_KEY_WRAP`
//! (a Managed HSM's AES keys). `endpoint` replaces `https://VAULT_HOST` for the template's keys:
//! a private endpoint, an emulator.
//!
//! Its credentials, as Azure's own libraries find them: the access token [`TOKEN_VAR`] holds, for
//! development alone (`az account get-access-token --resource https://vault.azure.net`), which
//! expires within the hour; else workload identity's (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
//! `AZURE_FEDERATED_TOKEN_FILE`: AKS); else a service principal's secret (`AZURE_TENANT_ID`,
//! `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`); else the managed identity's, from the instance
//! metadata service, `AZURE_CLIENT_ID` naming a user-assigned one. `AZURE_AUTHORITY_HOST` names the
//! authority, `https://login.microsoftonline.com/` if not set. Tokens are sent to the vault and the
//! authority alone, and neither they nor keys are logged or in any error.
//!
//! Key Vault's `wrapkey` takes no associated data, so what it wraps binds the tenant itself: the
//! tenant's ID (8 bytes, big-endian), then the key; an unwrap giving another tenant's is refused
//! as not authentic. What it keeps of a wrapped key is [`real`]'s: a header naming the backend and
//! the key's identifier, then the version that wrapped it (its length, a byte, then it) and the
//! vault's ciphertext. The service's documentation names no status for each refusal: they are
//! mapped as its error codes say ([`refusal`]), not yet checked against the service itself.

use super::{Error, KeyNames, Kms, real};
use crate::{KEY_LEN, Secret, Unauthentic, Wrapped};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use serde::Deserialize;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::{Zeroize as _, Zeroizing};

/// The scheme of its URLs, and the backend its wrapped keys' headers name.
const SCHEME: &str = "azure-kv";

/// The version of Key Vault's API its calls ask for.
const API_VERSION: &str = "7.4";

/// For development: an access token to call the vault with, in place of the identity's.
pub const TOKEN_VAR: &str = "CLADEFS_KMS_AZURE_TOKEN";

/// What its tokens are for.
const SCOPE: &str = "https://vault.azure.net/.default";
const RESOURCE: &str = "https://vault.azure.net";

/// The authority, unless `AZURE_AUTHORITY_HOST` names another.
const AUTHORITY: &str = "https://login.microsoftonline.com/";

/// The instance metadata service's token endpoint: the managed identity's.
const IMDS: &str = "http://169.254.169.254/metadata/identity/oauth2/token";

/// What a call waits for an answer at most, and for a connection.
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What each call is tried as at most, waiting before each retry, as Azure's libraries retry
/// throttling and the service's failures.
const BACKOFF: [Duration; 2] = [Duration::from_millis(200), Duration::from_secs(1)];

/// A token is fetched again once it has less than this left.
const REFRESH_BEFORE: Duration = Duration::from_secs(300);

/// The wrapping algorithms it takes: Key Vault's names.
const ALGORITHMS: [&str; 3] = ["RSA-OAEP-256", "A256KW", "CKM_AES_KEY_WRAP"];

/// What the query of an `azure-kv://` URL may name.
const QUERY: [&str; 4] = ["key", "keys", "alg", "endpoint"];

/// The KMS `url` names ([`super::from_url`]).
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    let parsed = parse(url)?;
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let source = source(&var);
    Ok(Arc::new(AzureKms::new(parsed, source).map_err(|why| format!("{url}: {why}"))?))
}

/// What an `azure-kv://` URL says.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    /// Where the template's keys are: `https://VAULT_HOST`, or the endpoint.
    vault: String,
    names: KeyNames,
    alg: &'static str,
}

/// What `url` says, each name checked.
fn parse(url: &str) -> Result<Parsed, String> {
    let usage = "azure-kv://VAULT_HOST?key=TEMPLATE[&keys=FILE][&alg=ALG][&endpoint=URL]";
    let rest = url.strip_prefix("azure-kv://").expect("an azure-kv:// URL");
    let (host, query) = rest.split_once('?').unwrap_or((rest, ""));
    let host_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-');
    if host.is_empty() || !host.chars().all(host_char) {
        return Err(format!("{url:?} names no vault: {usage}"));
    }
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    if let Some((name, _)) = query.iter().find(|(name, _)| !QUERY.contains(&&name[..])) {
        return Err(format!("{url:?}: no parameter {name:?}: {usage}"));
    }
    let value = |name: &str| query.iter().find(|(of, _)| of == name).map(|(_, v)| v.as_str());
    let alg = match value("alg") {
        None => ALGORITHMS[0],
        Some(alg) => ALGORITHMS
            .into_iter()
            .find(|known| *known == alg)
            .ok_or_else(|| format!("{url:?}: no algorithm {alg:?}: {}", ALGORITHMS.join(", ")))?,
    };
    let vault = match value("endpoint") {
        Some(endpoint) if endpoint.starts_with("https://") || endpoint.starts_with("http://") => {
            endpoint.trim_end_matches('/').to_owned()
        }
        Some(endpoint) => return Err(format!("{url:?}: the endpoint {endpoint:?} is no URL")),
        None => format!("https://{host}"),
    };
    let names = KeyNames::of_query(query.iter().map(|(name, value)| (&name[..], &value[..])))
        .map_err(|why| format!("{url:?}: {why}"))?;
    check_key(&names.of(u64::MAX)).map_err(|why| format!("{url:?}: the key template, {why}"))?;
    for (tenant, name) in names.overridden() {
        check_key(name)
            .map_err(|why| format!("{url:?}: tenant {}'s key, {why}", base32_id::Short(tenant)))?;
    }
    Ok(Parsed { vault, names, alg })
}

/// Checks `name` is a key's name, 1 to 127 letters, digits and `-`.
fn check_name(name: &str) -> Result<(), &'static str> {
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '-';
    match (1..=127).contains(&name.len()) && name.chars().all(name_char) {
        true => Ok(()),
        false => Err("a key's name is 1 to 127 letters, digits and -"),
    }
}

/// Checks `name`, a key's name in the URL's vault or its whole identifier, names a key.
fn check_key(name: &str) -> Result<(), String> {
    let refused = |why: &str| format!("{name:?}, names no key: {why}");
    let Some(rest) = name.strip_prefix("https://") else {
        return check_name(name).map_err(refused);
    };
    let (host, path) = rest.split_once('/').ok_or_else(|| refused("https://VAULT/keys/NAME"))?;
    let host_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-');
    let Some(key) = path.strip_prefix("keys/") else {
        return Err(refused("https://VAULT/keys/NAME"));
    };
    if host.is_empty() || !host.chars().all(host_char) {
        return Err(refused("https://VAULT/keys/NAME"));
    }
    check_name(key).map_err(refused)
}

/// The identifier of the key `name` names, a name in `vault` or an identifier itself, without
/// its version.
fn identifier(vault: &str, name: &str) -> String {
    match name.starts_with("https://") {
        true => name.to_owned(),
        false => format!("{vault}/keys/{name}"),
    }
}

/// What a wrap binds the tenant with: `tenant` (big-endian), then `key`.
fn bound(tenant: u64, key: &Secret) -> Zeroizing<Vec<u8>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(8 + KEY_LEN));
    bytes.extend_from_slice(&tenant.to_be_bytes());
    bytes.extend_from_slice(key.expose());
    bytes
}

/// Base64url, as the vault's answers have it: with or without padding, either alphabet (its own
/// documentation's examples mix them).
fn decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let normalized: String = (text.trim_end_matches('='))
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    BASE64URL.decode(normalized)
}

/// Where its tokens come from (see the module).
enum Source {
    Static(String),
    Federated { url: String, client_id: String, file: PathBuf },
    Secret { url: String, client_id: String, secret: Zeroizing<String> },
    ManagedIdentity { url: String, client_id: Option<String> },
}

/// Where its tokens come from, as the variables `var` gives by name say.
fn source(var: &dyn Fn(&str) -> Option<String>) -> Source {
    if let Some(token) = var(TOKEN_VAR) {
        return Source::Static(token);
    }
    let authority = var("AZURE_AUTHORITY_HOST").unwrap_or_else(|| AUTHORITY.to_owned());
    let authority = authority.trim_end_matches('/');
    if let (Some(tenant), Some(client_id)) = (var("AZURE_TENANT_ID"), var("AZURE_CLIENT_ID")) {
        let url = format!("{authority}/{tenant}/oauth2/v2.0/token");
        if let Some(file) = var("AZURE_FEDERATED_TOKEN_FILE") {
            return Source::Federated { url, client_id, file: file.into() };
        }
        if let Some(secret) = var("AZURE_CLIENT_SECRET") {
            return Source::Secret { url, client_id, secret: Zeroizing::new(secret) };
        }
    }
    Source::ManagedIdentity { url: IMDS.to_owned(), client_id: var("AZURE_CLIENT_ID") }
}

/// A token the authority, or the metadata service, answered with, what of it is read: its
/// lifetime a number of seconds (the authority's) or a string of them (the metadata service's).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Seconds,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Seconds {
    Number(u64),
    String(String),
}

/// Azure Key Vault, its tenants' keys in a vault, or named whole ([`self`]).
struct AzureKms {
    vault: String,
    names: KeyNames,
    alg: &'static str,
    http: reqwest::Client,
    source: Source,
    /// The token last fetched, and when it expires.
    held: Mutex<Option<(Zeroizing<String>, Instant)>>,
}

/// What a call asks of the vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Wrap,
    Unwrap,
}

impl Op {
    fn method(self) -> &'static str {
        match self {
            Op::Wrap => "wrapkey",
            Op::Unwrap => "unwrapkey",
        }
    }
}

/// `KeyOperationsParameters`, as JSON.
#[derive(serde::Serialize)]
struct KeyOperation<'a> {
    alg: &'a str,
    value: &'a str,
}

/// `KeyOperationResult`, what of it is read.
#[derive(Deserialize)]
struct KeyOperationResult {
    #[serde(default)]
    kid: String,
    #[serde(default)]
    value: String,
}

/// A call's failure: what it means, and whether trying it again may do better.
struct Failure {
    error: Error,
    transient: bool,
    /// The vault refused the token: fetch another, then try again.
    unauthorized: bool,
}

impl AzureKms {
    fn new(parsed: Parsed, source: Source) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|err| format!("an HTTP client: {err}"))?;
        let Parsed { vault, names, alg } = parsed;
        Ok(Self { vault, names, alg, http, source, held: Mutex::default() })
    }

    async fn wrap_now(&self, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let id = identifier(&self.vault, &self.names.of(tenant));
        let value = Zeroizing::new(BASE64URL.encode(bound(tenant, key)));
        let body = KeyOperation { alg: self.alg, value: &value };
        let body = Zeroizing::new(serde_json::to_vec(&body).expect("a request serializes"));
        let answer = self.call(tenant, &id, &format!("{id}/wrapkey"), Op::Wrap, &body).await?;
        let corrupt = |why: &str| io_error(tenant, &id, Op::Wrap, format!("its answer {why}"));
        let response: KeyOperationResult = serde_json::from_slice(&answer)
            .map_err(|err| corrupt(&format!("is not a KeyOperationResult: {err}")))?;
        let version = (response.kid.strip_prefix(&id))
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|version| {
                !version.is_empty() && version.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .ok_or_else(|| corrupt(&format!("names another key's version, {:?}", response.kid)))?;
        let ciphertext = decode(&response.value)
            .map_err(|err| corrupt(&format!("holds no ciphertext in base64url: {err}")))?;
        let version_len = u8::try_from(version.len())
            .map_err(|_| corrupt("names a version of over 255 bytes"))?;
        let mut kept = Vec::with_capacity(1 + version.len() + ciphertext.len());
        kept.push(version_len);
        kept.extend_from_slice(version.as_bytes());
        kept.extend_from_slice(&ciphertext);
        Ok(real::wrapped(SCHEME, &id, &kept))
    }

    async fn unwrap_now(&self, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        let id = identifier(&self.vault, &self.names.of(tenant));
        let unauthentic = |source| Error::Unauthentic { tenant, source };
        let kept = real::ciphertext(wrapped, SCHEME, &id).map_err(unauthentic)?;
        let header = Unauthentic("a wrapped key, by its header");
        let (&version_len, rest) = kept.split_first().ok_or(unauthentic(header))?;
        let (version, ciphertext) =
            rest.split_at_checked(version_len.into()).ok_or(unauthentic(header))?;
        let version = std::str::from_utf8(version).map_err(|_| unauthentic(header))?;
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(unauthentic(header));
        }
        let value = BASE64URL.encode(ciphertext);
        let body = serde_json::to_vec(&KeyOperation { alg: self.alg, value: &value })
            .expect("a request serializes");
        let url = format!("{id}/{version}/unwrapkey");
        let mut answer = self.call(tenant, &id, &url, Op::Unwrap, &body).await?;
        let corrupt = |why: &str| io_error(tenant, &id, Op::Unwrap, format!("its answer {why}"));
        let parsed = serde_json::from_slice::<KeyOperationResult>(&answer);
        answer.zeroize();
        let mut response =
            parsed.map_err(|err| corrupt(&format!("is not a KeyOperationResult: {err}")))?;
        let decoded = decode(&response.value);
        response.value.zeroize();
        let plaintext =
            Zeroizing::new(decoded.map_err(|_| corrupt("holds no plaintext in base64url"))?);
        let (of, key) = plaintext
            .split_first_chunk::<8>()
            .filter(|(_, key)| key.len() == KEY_LEN)
            .ok_or_else(|| unauthentic(Unauthentic("a wrapped key, of the wrong length")))?;
        if u64::from_be_bytes(*of) != tenant {
            return Err(unauthentic(Unauthentic("a wrapped key, wrapped for another tenant")));
        }
        let key = <&[u8; KEY_LEN]>::try_from(key).expect("of the key's length");
        Ok(Secret::from_bytes(key))
    }

    /// The body of the vault's answer to `op` at `url`, with the key `id` for `tenant`, of
    /// request `body`: tried again, after [`BACKOFF`], while it fails as it may not again; and
    /// at once with a new token, once, if the vault refuses the one it was given.
    async fn call(
        &self,
        tenant: u64,
        id: &str,
        url: &str,
        op: Op,
        body: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let mut waits = BACKOFF.iter();
        let mut refreshed = false;
        loop {
            let failure = match self.attempt(tenant, id, url, op, body).await {
                Ok(answer) => return Ok(answer),
                Err(failure) => failure,
            };
            if failure.unauthorized && !refreshed && !matches!(self.source, Source::Static(_)) {
                refreshed = true;
                *self.held.lock().unwrap() = None;
                continue;
            }
            match (failure.transient, waits.next()) {
                (true, Some(&wait)) => tokio::time::sleep(wait).await,
                _ => return Err(failure.error),
            }
        }
    }

    async fn attempt(
        &self,
        tenant: u64,
        id: &str,
        url: &str,
        op: Op,
        body: &[u8],
    ) -> Result<Vec<u8>, Failure> {
        let failed = |why: String, transient| Failure {
            error: io_error(tenant, id, op, why),
            transient,
            unauthorized: false,
        };
        let token =
            self.token().await.map_err(|why| failed(format!("credentials: {why}"), false))?;
        let response = self
            .http
            .post(format!("{url}?api-version={API_VERSION}"))
            .bearer_auth(&*token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|err| failed(chain(&err), true))?;
        let status = response.status();
        let answer = response.bytes().await.map_err(|err| failed(chain(&err), true))?;
        let mut owned = answer.to_vec();
        if let Ok(mut unique) = answer.try_into_mut() {
            unique[..].zeroize();
        }
        if status.is_success() {
            return Ok(owned);
        }
        let refused = refusal(tenant, id, op, status.as_u16(), &owned);
        owned.zeroize();
        Err(refused)
    }

    /// A token for the vault: the one held, while it has [`REFRESH_BEFORE`] left, else fetched.
    async fn token(&self) -> Result<Zeroizing<String>, String> {
        if let Source::Static(token) = &self.source {
            return Ok(Zeroizing::new(token.clone()));
        }
        if let Some((token, expires)) = &*self.held.lock().unwrap()
            && Instant::now() + REFRESH_BEFORE < *expires
        {
            return Ok(token.clone());
        }
        let asked = Instant::now();
        let request = match &self.source {
            Source::Static(_) => unreachable!("answered above"),
            Source::Federated { url, client_id, file } => {
                let assertion = std::fs::read_to_string(file).map_err(|err| {
                    format!("AZURE_FEDERATED_TOKEN_FILE {}: {err}", file.display())
                })?;
                let form = Zeroizing::new(
                    url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("grant_type", "client_credentials")
                        .append_pair("client_id", client_id)
                        .append_pair(
                            "client_assertion_type",
                            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                        )
                        .append_pair("client_assertion", assertion.trim())
                        .append_pair("scope", SCOPE)
                        .finish(),
                );
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, FORM)
                    .body(form.to_string())
            }
            Source::Secret { url, client_id, secret } => {
                let form = Zeroizing::new(
                    url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("grant_type", "client_credentials")
                        .append_pair("client_id", client_id)
                        .append_pair("client_secret", secret)
                        .append_pair("scope", SCOPE)
                        .finish(),
                );
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, FORM)
                    .body(form.to_string())
            }
            Source::ManagedIdentity { url, client_id } => {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.append_pair("api-version", "2018-02-01").append_pair("resource", RESOURCE);
                if let Some(client_id) = client_id {
                    query.append_pair("client_id", client_id);
                }
                self.http.get(format!("{url}?{}", query.finish())).header("Metadata", "true")
            }
        };
        let response = request.send().await.map_err(|err| chain(&err))?;
        let status = response.status();
        let mut answer = response.bytes().await.map_err(|err| chain(&err))?.to_vec();
        if !status.is_success() {
            // The authority's refusals name no secret: its error and description.
            let why = String::from_utf8_lossy(&answer).chars().take(300).collect::<String>();
            answer.zeroize();
            return Err(format!("the authority answered HTTP {}: {why}", status.as_u16()));
        }
        let parsed = serde_json::from_slice::<TokenResponse>(&answer);
        answer.zeroize();
        let parsed = parsed.map_err(|err| format!("the authority's answer: {err}"))?;
        let seconds = match &parsed.expires_in {
            Seconds::Number(seconds) => Some(*seconds),
            Seconds::String(seconds) => seconds.parse().ok(),
        };
        let lifetime = Duration::from_secs(seconds.unwrap_or(0));
        let token = Zeroizing::new(parsed.access_token);
        *self.held.lock().unwrap() = Some((token.clone(), asked + lifetime));
        Ok(token)
    }
}

/// The content type of a form.
const FORM: &str = "application/x-www-form-urlencoded";

impl Kms for AzureKms {
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

/// `KeyVaultError`'s `error`, what of it is read.
#[derive(Default, Deserialize)]
struct VaultError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// What the vault's refusal of `op` with key `id` for `tenant`, of HTTP status `code` and body
/// `body`, means:
///
/// - 404, `KeyNotFound`: the tenant has no KEK, [`Error::NoSuchTenant`] (a key deleted, purged or
///   soft-deleted, reads as none, as a real KMS forgets a deleted key);
/// - 403: the tenant took cladefs's access to its key away, or disabled the key:
///   [`Error::Revoked`];
/// - 400 of an unwrap: a ciphertext not the key's, [`Error::Unauthentic`];
/// - 401: the vault refused the token, fetched again once ([`AzureKms::call`]), else
///   [`Error::Io`];
/// - anything else: [`Error::Io`], transient if throttled (429) or the service's (5xx).
fn refusal(tenant: u64, id: &str, op: Op, code: u16, body: &[u8]) -> Failure {
    let error = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| serde_json::from_value::<VaultError>(value.get("error")?.clone()).ok())
        .unwrap_or_default();
    let refused = |error| Failure { error, transient: false, unauthorized: false };
    match code {
        404 if error.code == "KeyNotFound" => return refused(Error::NoSuchTenant { tenant }),
        403 => return refused(Error::Revoked { tenant }),
        400 if op == Op::Unwrap => {
            return refused(Error::Unauthentic {
                tenant,
                source: Unauthentic("a wrapped key, refused by the KMS"),
            });
        }
        _ => {}
    }
    let named = if error.code.is_empty() { "no code" } else { &error.code[..] };
    let why = format!("HTTP {code}, {named}: {}", error.message);
    Failure {
        error: io_error(tenant, id, op, why),
        transient: code == 429 || code >= 500,
        unauthorized: code == 401,
    }
}

/// The [`Error::Io`] of `op` with key `id` for `tenant`, failing as `why` says.
fn io_error(tenant: u64, id: &str, op: Op, why: String) -> Error {
    let source = io::Error::other(format!("Azure Key Vault, {} with {id}: {why}", op.method()));
    Error::Io { tenant, source }
}

/// `error` and its sources, each after the one before.
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

#[cfg(test)]
mod tests;
