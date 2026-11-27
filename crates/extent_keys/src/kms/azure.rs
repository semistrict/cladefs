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
//! a private endpoint (a vault's name), or an emulator on this host (`http://127.0.0.1:PORT`).
//! Every host named must be a vault's or a Managed HSM's ([`VAULT_DOMAINS`]): its tokens unwrap
//! every tenant's key the hosts' identity may use, and go nowhere else.
//!
//! Its tokens are for each vault's audience (`https://vault.azure.net`,
//! `https://managedhsm.azure.net`, or a sovereign cloud's), fetched and kept apart. Its credentials,
//! as Azure's own libraries find them: the access token [`TOKEN_VAR`] holds, for
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

#[cfg(test)]
use super::Kms;
use super::background::Background;
use super::http::{self, Unread};
use super::retry::{self, Failed};
use super::{Error, KeyNames, Origin, OverrideFiles, Vars, real};
use crate::{KEY_LEN, Secret, Unauthentic, Wrapped};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use bytes::Bytes;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use serde::Deserialize;
use std::collections::HashMap;
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

/// The variables its credentials are read from ([`source`]): a bucket's own replace the host's as
/// one (board 8485, `super::vars`).
pub(super) const CREDENTIAL_VARS: &[&str] = &[
    TOKEN_VAR,
    "AZURE_AUTHORITY_HOST",
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_FEDERATED_TOKEN_FILE",
    "AZURE_CLIENT_SECRET",
];

/// The authority, unless `AZURE_AUTHORITY_HOST` names another.
const AUTHORITY: &str = "https://login.microsoftonline.com/";

/// The instance metadata service's token endpoint: the managed identity's.
const IMDS: &str = "http://169.254.169.254/metadata/identity/oauth2/token";

/// What a call waits for an answer at most, and for a connection.
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A token is fetched again once it has less than this left, or half its lifetime if shorter
/// ([`Held::refresh_at`]).
const REFRESH_BEFORE: Duration = Duration::from_secs(300);

/// The wrapping algorithms it takes: Key Vault's names.
const ALGORITHMS: [&str; 3] = ["RSA-OAEP-256", "A256KW", "CKM_AES_KEY_WRAP"];

/// What the query of an `azure-kv://` URL may name.
const QUERY: [&str; 4] = ["key", "keys", "alg", "endpoint"];

/// The KMS `url` names ([`super::from_url_with`]), of `origin`, its credentials as `vars` give
/// them.
pub(super) fn from_url(
    url: &str,
    vars: &Vars,
    origin: Origin,
    files: Option<&OverrideFiles>,
) -> Result<super::Shared, String> {
    let parsed = parse(url, origin, files)?;
    Ok(Arc::new(AzureKms::new(parsed, source(vars)).map_err(|why| format!("{url}: {why}"))?))
}

/// What an `azure-kv://` URL says.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    /// Where the template's keys are: `https://VAULT_HOST`, or the endpoint.
    vault: String,
    /// The audience of the tokens for the URL's vault ([`audience_of`]): those of an endpoint
    /// on this host's, an emulator's, too.
    audience: &'static str,
    names: KeyNames,
    alg: &'static str,
}

pub(super) fn check_url(url: &str, origin: Origin) -> Result<(), String> {
    parse(url, origin, None).map(drop)
}

/// What `url` says, each name checked; of `origin` the registry of buckets, each host it names (its
/// vault's, its endpoint's, its template's) one the host allows, and no file of the host's (board
/// 8525), before any is opened.
fn parse(url: &str, origin: Origin, files: Option<&OverrideFiles>) -> Result<Parsed, String> {
    let usage = "azure-kv://VAULT_HOST?key=TEMPLATE[&keys=FILE][&alg=ALG][&endpoint=URL]";
    let rest = url.strip_prefix("azure-kv://").expect("an azure-kv:// URL");
    let (host, query) = rest.split_once('?').unwrap_or((rest, ""));
    check_vault(host).map_err(|why| format!("{url:?} names no vault: {why}: {usage}"))?;
    origin.host(host, None).map_err(|why| format!("{url:?}: {why}"))?;
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
        Some(endpoint) => {
            let (host, port) =
                check_endpoint(endpoint).map_err(|why| format!("{url:?}: the endpoint, {why}"))?;
            origin.host(host, port).map_err(|why| format!("{url:?}: the endpoint, {why}"))?;
            endpoint.trim_end_matches('/').to_owned()
        }
        None => format!("https://{host}"),
    };
    let query_pairs = query.iter().map(|(name, value)| (&name[..], &value[..]));
    let names = KeyNames::of_query_in(query_pairs, origin, check_key, files)
        .map_err(|why| format!("{url:?}: {why}"))?;
    check_key(&names.of(u64::MAX)).map_err(|why| format!("{url:?}: the key template, {why}"))?;
    // A template naming keys whole names their vault: one the host allows, as the template has it,
    // so that a vault a tenant's ID makes (`https://t{tenant}.vault.azure.net/...`) is none.
    if let Some(rest) = names.template().strip_prefix("https://") {
        let vault = rest.split('/').next().unwrap_or_default();
        origin.host(vault, None).map_err(|why| format!("{url:?}: the key template, {why}"))?;
    }
    let audience = audience_of(host).expect("a vault's host, checked");
    Ok(Parsed { vault, audience, names, alg })
}

/// The domains of Key Vault's vaults and Managed HSMs, in Azure's public, China and US Government
/// clouds: the only hosts its tokens are sent to, as Azure's own libraries check, so that a key's
/// identifier given by a tenant, or a URL recorded in a bucket's registry, takes no token elsewhere.
/// Each with the audience its tokens are for, its resource: a Managed HSM's, and a sovereign
/// cloud's, take no token for another (TASK-633.6's review).
const VAULT_DOMAINS: [(&str, &str); 6] = [
    (".vault.azure.net", "https://vault.azure.net"),
    (".managedhsm.azure.net", "https://managedhsm.azure.net"),
    (".vault.azure.cn", "https://vault.azure.cn"),
    (".managedhsm.azure.cn", "https://managedhsm.azure.cn"),
    (".vault.usgovcloudapi.net", "https://vault.usgovcloudapi.net"),
    (".managedhsm.usgovcloudapi.net", "https://managedhsm.usgovcloudapi.net"),
];

/// Checks `host` is a vault's or a Managed HSM's: a name of lowercase letters, digits and `-`, in
/// one of [`VAULT_DOMAINS`].
fn check_vault(host: &str) -> Result<(), String> {
    match audience_of(host) {
        Some(_) => Ok(()),
        None => {
            let domains: Vec<&str> = VAULT_DOMAINS.iter().map(|(domain, _)| *domain).collect();
            Err(format!("{host:?} is no vault's host: NAME{}", domains.join(", NAME")))
        }
    }
}

/// The audience of the tokens for `host`, a vault's or a Managed HSM's ([`VAULT_DOMAINS`]); none
/// for another host.
fn audience_of(host: &str) -> Option<&'static str> {
    let label_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    VAULT_DOMAINS.iter().find_map(|(domain, audience)| {
        let name = host.strip_suffix(domain)?;
        (!name.is_empty() && name.chars().all(label_char)).then_some(*audience)
    })
}

/// Checks `endpoint` is a vault's (`https://VAULT_HOST`, a private endpoint resolving a vault's
/// name), or an emulator's on this host (`http://127.0.0.1:PORT`, `http://localhost:PORT`):
/// nowhere else is a token sent, nor in the clear but to this host. Its host, and its port if it
/// names one.
fn check_endpoint(endpoint: &str) -> Result<(&str, Option<u16>), String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(host) = endpoint.strip_prefix("https://") {
        return check_vault(host).map(|()| (host, None));
    }
    let local = endpoint.strip_prefix("http://").and_then(|rest| {
        let (host, port) = rest.rsplit_once(':').unwrap_or((rest, "80"));
        let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
        // A port that is none: no endpoint, refused below.
        loopback.then_some((host, Some(port.parse::<u16>().ok()?)))
    });
    local.ok_or_else(|| {
        format!("{endpoint:?} is neither a vault's (https://VAULT_HOST) nor this host's (http://127.0.0.1:PORT)")
    })
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
    let Some(key) = path.strip_prefix("keys/") else {
        return Err(refused("https://VAULT/keys/NAME"));
    };
    check_vault(host).map_err(|why| refused(&why))?;
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
    Static(Zeroizing<String>),
    Federated { url: String, client_id: String, file: PathBuf },
    Secret { url: String, client_id: String, secret: Zeroizing<String> },
    ManagedIdentity { url: String, client_id: Option<String> },
}

/// Where its tokens come from, as `vars` say ([`CREDENTIAL_VARS`]).
fn source(vars: &Vars) -> Source {
    if let Some(token) = vars.get(TOKEN_VAR) {
        return Source::Static(Zeroizing::new(token.to_owned()));
    }
    let authority = vars.get("AZURE_AUTHORITY_HOST").unwrap_or(AUTHORITY);
    let authority = authority.trim_end_matches('/');
    let client_id = vars.get("AZURE_CLIENT_ID").map(str::to_owned);
    if let (Some(tenant), Some(client_id)) = (vars.get("AZURE_TENANT_ID"), client_id.clone()) {
        let url = format!("{authority}/{tenant}/oauth2/v2.0/token");
        if let Some(file) = vars.get("AZURE_FEDERATED_TOKEN_FILE") {
            return Source::Federated { url, client_id, file: file.into() };
        }
        if let Some(secret) = vars.get("AZURE_CLIENT_SECRET") {
            return Source::Secret { url, client_id, secret: Zeroizing::new(secret.to_owned()) };
        }
    }
    Source::ManagedIdentity { url: IMDS.to_owned(), client_id }
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

/// Azure Key Vault, its tenants' keys in a vault, or named whole ([`self`]). Its calls run on
/// the shared KMS runtime ([`Background`]).
struct AzureKms {
    inner: Arc<Inner>,
    runtime: Background,
}

/// What [`AzureKms`]'s calls share, on its runtime.
struct Inner {
    vault: String,
    audience: &'static str,
    names: KeyNames,
    alg: &'static str,
    http: reqwest::Client,
    tokens: Tokens,
}

/// Its tokens, each audience's ([`Tokens::token`]), and where they come from.
struct Tokens {
    fetcher: Arc<Fetcher>,
    /// Each audience's token: the one held, and the fetch of the next, if one goes.
    slots: Mutex<HashMap<&'static str, Slot>>,
}

/// What fetches a token: a [`Slot`] holds a fetch while it goes, which holds this alone, never
/// [`Tokens`] or [`Inner`], so that a fetch whose callers all gave up on it is no cycle keeping the
/// KMS alive once dropped (hotbed review 47).
struct Fetcher {
    http: reqwest::Client,
    source: Source,
}

/// An audience's tokens ([`Tokens::token`]).
#[derive(Default)]
struct Slot {
    held: Option<Held>,
    /// The fetch going, which every call wanting a token for the audience meanwhile waits for:
    /// one request of the authority however many ask (board 8475).
    fetching: Option<Fetch>,
}

/// A fetch of a token, shared by those waiting for it.
type Fetch = Shared<BoxFuture<'static, Result<Held, String>>>;

/// A token fetched, and when to fetch the next.
#[derive(Clone)]
struct Held {
    token: Zeroizing<String>,
    refresh_at: Instant,
}

impl Held {
    /// `token`, asked for at `asked` and good for `lifetime`: fetched again [`REFRESH_BEFORE`] it
    /// expires, or, if it lives less than twice that, half its lifetime in, so that a short-lived
    /// one is used, not fetched at every call (board 8475).
    fn new(token: Zeroizing<String>, asked: Instant, lifetime: Duration) -> Self {
        Self { token, refresh_at: asked + lifetime - REFRESH_BEFORE.min(lifetime / 2) }
    }
}

/// What a call asks of the vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op<'a> {
    Wrap,
    Unwrap(&'a str),
}

impl Op<'_> {
    fn method(self) -> &'static str {
        match self {
            Op::Wrap => "wrapkey",
            Op::Unwrap(_) => "unwrapkey",
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

/// A call's failure: what it means, whether trying it again may do better and after how long the
/// vault said, and whether the vault refused the token it was made with.
struct Failure {
    failed: Failed<Error>,
    /// The vault refused the token: fetch another, then try again.
    unauthorized: bool,
}

impl AzureKms {
    fn new(parsed: Parsed, source: Source) -> Result<Self, String> {
        // No redirect followed (board 8527): one would take a request's body, a wrapped key or a
        // client's secret, elsewhere.
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| format!("an HTTP client: {err}"))?;
        let runtime = Background::shared().map_err(|err| format!("its runtime: {err}"))?;
        let Parsed { vault, audience, names, alg } = parsed;
        let fetcher = Arc::new(Fetcher { http: http.clone(), source });
        let tokens = Tokens { fetcher, slots: Mutex::default() };
        let inner = Inner { vault, audience, names, alg, http, tokens };
        Ok(Self { inner: Arc::new(inner), runtime })
    }
}

impl Inner {
    /// The audience of the tokens for the key `id`: its vault's; the URL's vault's for a key of an
    /// endpoint on this host, an emulator.
    fn audience(&self, id: &str) -> &'static str {
        let host = id.strip_prefix("https://").and_then(|rest| rest.split('/').next());
        host.and_then(audience_of).unwrap_or(self.audience)
    }

    async fn wrap_now(self: &Arc<Self>, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        let id = identifier(&self.vault, &self.names.of(tenant));
        let value = Zeroizing::new(BASE64URL.encode(bound(tenant, key)));
        let body = http::json(&KeyOperation { alg: self.alg, value: &value });
        let answer = self.call(tenant, &id, Op::Wrap, body).await?;
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

    async fn unwrap_now(self: &Arc<Self>, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
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
        let body = http::json(&KeyOperation { alg: self.alg, value: &value });
        let op = Op::Unwrap(version);
        let answer = self.call(tenant, &id, op, body).await?;
        let corrupt = |why: &str| io_error(tenant, &id, op, format!("its answer {why}"));
        let parsed = serde_json::from_slice::<KeyOperationResult>(&answer);
        drop(answer);
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
    /// request `body`: tried again while it fails as it may not next time ([`retry`]); and at
    /// once with a new token, once, if the vault refuses the one it was given.
    async fn call(
        self: &Arc<Self>,
        tenant: u64,
        id: &str,
        op: Op<'_>,
        body: Bytes,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        let audience = self.audience(id);
        let url = match op {
            Op::Wrap => format!("{id}/wrapkey"),
            Op::Unwrap(version) => format!("{id}/{version}/unwrapkey"),
        };
        let (mut retries, mut refreshed) = (0, false);
        loop {
            let token = (self.tokens.token(audience).await)
                .map_err(|why| io_error(tenant, id, op, format!("credentials: {why}")))?;
            let Failure { failed, unauthorized } =
                match self.attempt(tenant, id, &url, op, body.clone(), &token).await {
                    Ok(answer) => return Ok(answer),
                    Err(failure) => failure,
                };
            if unauthorized
                && !refreshed
                && !matches!(self.tokens.fetcher.source, Source::Static(_))
            {
                refreshed = true;
                self.tokens.evict(audience, &token);
                continue;
            }
            match failed.transient.then(|| retry::wait(retries, failed.after)).flatten() {
                Some(wait) => tokio::time::sleep(wait).await,
                None => return Err(failed.error),
            }
            retries += 1;
        }
    }

    async fn attempt(
        &self,
        tenant: u64,
        id: &str,
        url: &str,
        op: Op<'_>,
        body: Bytes,
        token: &str,
    ) -> Result<Zeroizing<Vec<u8>>, Failure> {
        let unread = |unread: Unread| {
            let error = io_error(tenant, id, op, unread.why());
            let failed = match unread.transient() {
                true => Failed::transient(error, None),
                false => Failed::refused(error),
            };
            Failure { failed, unauthorized: false }
        };
        // board 8589: the bearer's header, and the copies reqwest makes of it and of the body, are
        // its own, which nothing here zeroes.
        let response = self
            .http
            .post(format!("{url}?api-version={API_VERSION}"))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| unread(Unread::Transport(err)))?;
        let status = response.status();
        let after = retry::retry_after(response.headers());
        let answer = http::body(response, http::MAX).await.map_err(unread)?;
        if status.is_success() {
            return Ok(answer);
        }
        let mut refused = refusal(tenant, id, op, status.as_u16(), &answer);
        refused.failed.after = after;
        Err(refused)
    }
}

impl Tokens {
    /// A token for `audience`, a vault's: the one held, until its [`Held::refresh_at`]; else the
    /// one the fetch going gets, or a fetch started now, which those asking meanwhile share
    /// (board 8475).
    async fn token(&self, audience: &'static str) -> Result<Zeroizing<String>, String> {
        if let Source::Static(token) = &self.fetcher.source {
            return Ok(token.clone());
        }
        let fetch = {
            let mut slots = self.slots.lock().unwrap();
            let slot = slots.entry(audience).or_default();
            if let Some(held) = &slot.held
                && Instant::now() < held.refresh_at
            {
                return Ok(held.token.clone());
            }
            let fetcher = self.fetcher.clone();
            let start = move || async move { fetcher.fetch(audience).await }.boxed().shared();
            slot.fetching.get_or_insert_with(start).clone()
        };
        let fetched = fetch.clone().await;
        // The first to see its fetch end keeps what it got, and ends it: a failure is not kept,
        // the next call fetching anew.
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.entry(audience).or_default();
        if slot.fetching.as_ref().is_some_and(|going| going.ptr_eq(&fetch)) {
            slot.fetching = None;
            if let Ok(held) = &fetched {
                slot.held = Some(held.clone());
            }
        }
        fetched.map(|held| held.token)
    }

    /// Drops `audience`'s token if it is still `used`, which the vault refused: not one fetched
    /// since by another call, which the vault has yet to refuse (board 8475).
    fn evict(&self, audience: &'static str, used: &str) {
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.entry(audience).or_default();
        if slot.held.as_ref().is_some_and(|held| *held.token == *used) {
            slot.held = None;
        }
    }
}

impl Fetcher {
    /// A token for `audience`, fetched from the source: tried again while it fails as the network
    /// does, or as the authority or the metadata service throttling or failing, which says nothing
    /// of the credentials ([`Fetcher::fetch_once`]).
    async fn fetch(&self, audience: &'static str) -> Result<Held, String> {
        retry::retried(|| self.fetch_once(audience)).await
    }

    /// A token for `audience`, asked for once.
    async fn fetch_once(&self, audience: &'static str) -> Result<Held, Failed<String>> {
        let scope = format!("{audience}/.default");
        let asked = Instant::now();
        let request = match &self.source {
            Source::Static(_) => unreachable!("never fetched"),
            Source::Federated { url, client_id, file } => {
                let assertion =
                    std::fs::read_to_string(file).map(Zeroizing::new).map_err(|err| {
                        Failed::refused(format!(
                            "AZURE_FEDERATED_TOKEN_FILE {}: {err}",
                            file.display()
                        ))
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
                        .append_pair("scope", &scope)
                        .finish(),
                );
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, FORM)
                    .body(http::zeroed(form))
            }
            Source::Secret { url, client_id, secret } => {
                let form = Zeroizing::new(
                    url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("grant_type", "client_credentials")
                        .append_pair("client_id", client_id)
                        .append_pair("client_secret", secret)
                        .append_pair("scope", &scope)
                        .finish(),
                );
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, FORM)
                    .body(http::zeroed(form))
            }
            Source::ManagedIdentity { url, client_id } => {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.append_pair("api-version", "2018-02-01").append_pair("resource", audience);
                if let Some(client_id) = client_id {
                    query.append_pair("client_id", client_id);
                }
                self.http.get(format!("{url}?{}", query.finish())).header("Metadata", "true")
            }
        };
        let unread = |unread: Unread| match unread.transient() {
            true => Failed::transient(unread.why(), None),
            false => Failed::refused(unread.why()),
        };
        let response = request.send().await.map_err(|err| unread(Unread::Transport(err)))?;
        let status = response.status();
        let after = retry::retry_after(response.headers());
        let answer = http::body(response, http::TOKEN_MAX).await.map_err(unread)?;
        if !status.is_success() {
            // The authority's refusals name no secret: its error and description.
            let why = String::from_utf8_lossy(&answer).chars().take(300).collect::<String>();
            let code = status.as_u16();
            let why = format!("the authority answered HTTP {code}: {why}");
            // The metadata service answers 404 while the identity is not yet the machine's, and
            // 410 as it starts, each to be asked again (Azure's IMDS documentation); the
            // authority's 4xx are the credentials', which no try again changes.
            let starting =
                matches!(self.source, Source::ManagedIdentity { .. }) && matches!(code, 404 | 410);
            return Err(match retry::transient_status(code) || starting {
                true => Failed::transient(why, after),
                false => Failed::refused(why),
            });
        }
        let parsed = serde_json::from_slice::<TokenResponse>(&answer);
        drop(answer);
        let parsed =
            parsed.map_err(|err| Failed::refused(format!("the authority's answer: {err}")))?;
        let seconds = match &parsed.expires_in {
            Seconds::Number(seconds) => Some(*seconds),
            // Not a number: refused below, naming it.
            Seconds::String(seconds) => seconds.parse().ok(),
        };
        // No lifetime said: refused, not taken for none, which would fetch at every call (hotbed
        // question 461).
        let seconds = seconds.ok_or_else(|| {
            Failed::refused("the authority's answer: its expires_in is no number of seconds".into())
        })?;
        Ok(Held::new(Zeroizing::new(parsed.access_token), asked, Duration::from_secs(seconds)))
    }
}

/// The content type of a form.
const FORM: &str = "application/x-www-form-urlencoded";

impl_runtime_kms!(AzureKms);

/// `KeyVaultError`'s `error`, what of it is read: its code, message, and inner error, whose code
/// says more (`ForbiddenByFirewall`), and so on.
#[derive(Default, Deserialize)]
struct VaultError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    innererror: Option<Box<VaultError>>,
}

impl VaultError {
    /// Its code, then its inner errors', each in turn.
    fn codes(&self) -> impl Iterator<Item = &str> {
        std::iter::successors(Some(self), |error| error.innererror.as_deref())
            .map(|error| &error.code[..])
    }
}

/// The vault's firewall and private endpoint refusals (board 8481).
const NETWORK_REFUSALS: [&str; 2] = ["ForbiddenByFirewall", "ForbiddenByConnection"];

/// What the vault's refusal of `op` with key `id` for `tenant`, of HTTP status `code` and body
/// `body`, means:
///
/// - 404 `KeyNotFound`: [`Error::NoSuchTenant`] on a wrap, [`Error::VersionGone`] on an unwrap;
/// - 403 with a key-state code: [`Error::Revoked`] on a wrap, [`Error::VersionDisabled`] or
///   [`Error::VersionGone`] on an unwrap, scoped to the version in the request;
/// - other 403 responses, including access and network refusals: [`Error::Io`] (board 10577);
/// - 400 of an unwrap: a ciphertext not the key's, [`Error::Unauthentic`];
/// - 401: the vault refused the token, fetched again once ([`AzureKms::call`]), else
///   [`Error::Io`];
/// - anything else: [`Error::Io`], transient if throttled (429) or the service's (5xx).
fn refusal(tenant: u64, id: &str, op: Op<'_>, code: u16, body: &[u8]) -> Failure {
    // Not JSON, or not Key Vault's error: the default, the status alone saying what failed.
    let error = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| serde_json::from_value::<VaultError>(value.get("error")?.clone()).ok())
        .unwrap_or_default();
    let refused = |error| Failure { failed: Failed::refused(error), unauthorized: false };
    let network = error.codes().find(|code| NETWORK_REFUSALS.contains(code));
    if let (403, Some(network)) = (code, network) {
        let why = format!(
            "HTTP 403, {network}: the vault refuses this host's network, not the tenant's key: {}",
            error.message
        );
        return refused(io_error(tenant, id, op, why));
    }
    let named = error.codes().filter(|code| !code.is_empty()).last().unwrap_or("no code");
    match (code, named, op) {
        (404, "KeyNotFound", Op::Wrap) => return refused(Error::NoSuchTenant { tenant }),
        (404, "KeyNotFound", Op::Unwrap(version))
        | (403, "KeyDeleted" | "KeyPurged", Op::Unwrap(version)) => {
            return refused(Error::VersionGone { tenant, version: version.to_owned() });
        }
        (403, "KeyDisabled" | "KeyRevoked" | "KeyExpired", Op::Unwrap(version)) => {
            return refused(Error::VersionDisabled { tenant, version: version.to_owned() });
        }
        (
            403,
            "KeyDisabled" | "KeyRevoked" | "KeyExpired" | "KeyDeleted" | "KeyPurged",
            Op::Wrap,
        ) => {
            return refused(Error::Revoked { tenant });
        }
        (400, _, Op::Unwrap(_)) => {
            return refused(Error::Unauthentic {
                tenant,
                source: Unauthentic("a wrapped key, refused by the KMS"),
            });
        }
        _ => {}
    }
    let why = format!("HTTP {code}, {named}: {}", error.message);
    let error = io_error(tenant, id, op, why);
    Failure {
        failed: match retry::transient_status(code) {
            true => Failed::transient(error, None),
            false => Failed::refused(error),
        },
        unauthorized: code == 401,
    }
}

/// The [`Error::Io`] of `op` with key `id` for `tenant`, failing as `why` says.
fn io_error(tenant: u64, id: &str, op: Op<'_>, why: String) -> Error {
    let source = io::Error::other(format!("Azure Key Vault, {} with {id}: {why}", op.method()));
    Error::Io { tenant, source }
}

#[cfg(test)]
mod tests;
