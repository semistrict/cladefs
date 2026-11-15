// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: OpenBao's Transit engine as a [`Kms`] (TASK-411.16): [`OpenBao`]. OpenBao
//! (MPL-2.0) is self-hosted, so the one real KMS cladefs's tests run anywhere, `bao server -dev`
//! ([`crate::testing::openbao::DevServer`]).

use super::{Error, KeyNames, Kms};
use crate::{KEY_LEN, Secret, Unauthentic, Wrapped};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use reqwest::header::HeaderValue;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use zeroize::{Zeroize as _, Zeroizing};

/// The environment variable holding the token cladefs's processes call OpenBao with.
pub const TOKEN_ENV: &str = "CLADEFS_KMS_OPENBAO_TOKEN";

/// The environment variable naming a file holding the token, in place of [`TOKEN_ENV`]: read when
/// the KMS is made, and again whenever OpenBao refuses a call (403), so that a token renewed in it
/// (OpenBao Agent's file sink) is taken up.
pub const TOKEN_FILE_ENV: &str = "CLADEFS_KMS_OPENBAO_TOKEN_FILE";

/// OpenBao's port, if a URL names none.
const DEFAULT_PORT: u16 = 8200;

/// The start of what a real KMS of cladefs's wraps: its own, then the format of the rest.
const MAGIC: &[u8; 4] = b"fxkm";

/// The format of what [`OpenBao`] wraps.
const FORMAT: u8 = 1;

/// The backend a wrapped key's header names.
const BACKEND: &[u8] = b"openbao";

/// What starts the ciphertexts Transit makes, before their key's version.
const CIPHERTEXT_PREFIX: &str = "vault:v";

/// The longest key name a header holds.
const MAX_NAME_LEN: usize = 255;

/// How many times a call is made in all, while it fails as the network or the server does
/// ([`retried`]).
const TRIES: u32 = 3;

/// The wait before a failed call is made again, four times longer each time.
const BACKOFF: Duration = Duration::from_millis(100);

/// How long a connection to OpenBao may take to make.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one request may take, connecting, sending and reading its reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The token [`OpenBao`] calls with, or where it is.
pub enum Token {
    /// The token itself.
    Value(String),
    /// A file holding it, white space around it ignored, read again as [`TOKEN_FILE_ENV`] says.
    File(PathBuf),
}

impl Token {
    /// The token the environment gives: [`TOKEN_ENV`]'s, or [`TOKEN_FILE_ENV`]'s file, exactly one
    /// of them set.
    pub fn from_env() -> Result<Self, String> {
        Self::of_env(|name| std::env::var(name).ok())
    }

    fn of_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let set = |name| var(name).filter(|value| !value.is_empty());
        match (set(TOKEN_ENV), set(TOKEN_FILE_ENV)) {
            (Some(token), None) => Ok(Self::Value(token)),
            (None, Some(path)) => Ok(Self::File(path.into())),
            (Some(_), Some(_)) => {
                Err(format!("both {TOKEN_ENV} and {TOKEN_FILE_ENV} are set: one"))
            }
            (None, None) => {
                Err(format!("OpenBao's token is in neither {TOKEN_ENV} nor {TOKEN_FILE_ENV}"))
            }
        }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(_) => f.write_str("Token::Value(..)"),
            Self::File(path) => f.debug_tuple("Token::File").field(path).finish(),
        }
    }
}

/// OpenBao's Transit engine (openbao.org/api-docs/secret/transit), as a KMS: each tenant's KEK a
/// Transit key, named as [`KeyNames`] says, which never leaves it. Its URL is
/// `openbao://HOST[:PORT]/MOUNT?key=TEMPLATE[&keys=FILE][&namespace=NS]`, or `openbaos://` over
/// TLS, which may add `&ca=FILE`, a PEM file of the only CAs trusted (the system's are, without
/// it). `MOUNT` is the Transit engine's path, `NS` its namespace (`X-Vault-Namespace`); the token
/// is [`Token`]'s. `openbao://` sends the token in the clear: for a loopback or a trusted network.
///
/// A wrap is an `encrypt` of the key under the tenant's key's newest version; an unwrap a
/// `decrypt`, under whichever version made it, as long as the key keeps it. What it wraps is a
/// header, [`MAGIC`], [`FORMAT`] (1), the backend's name ([`BACKEND`], a byte of its length
/// first), the tenant (big-endian) and the key's name (two bytes of its length, big-endian,
/// first), then Transit's ciphertext (`vault:vN:...`). The header lets an unwrap refuse a key
/// wrapped by another KMS, for another tenant or under another key without asking OpenBao; it is
/// the `associated_data` of the encryption, so that one changed fails its AES-GCM's (or
/// ChaCha20-Poly1305's) check, and its `context`, so that a key made `derived` derives one of its
/// own for it. The keys must be of a type that encrypts with associated data: `aes256-gcm96`,
/// Transit's default, or `chacha20-poly1305`.
///
/// What it is told maps to [`Error`] as follows, each as OpenBao 2.7 says it (its tests check
/// each): a key not found (a 404 reading it, after a 403) is [`Error::NoSuchTenant`] when wrapping,
/// [`Error::Destroyed`] when unwrapping, as the header says it wrapped; a key soft-deleted, or a
/// version below its `min_decryption_version`, or a refused call (403) of a token OpenBao knows
/// (`auth/token/lookup-self`), is [`Error::Revoked`]; a version trimmed (below
/// `min_available_version`) is [`Error::Destroyed`]; a ciphertext failing its check, or of a
/// version the key never had, [`Error::Unauthentic`]; anything else, a token OpenBao refuses
/// altogether, a missing mount, the network or the server failing, [`Error::Io`], saying what
/// failed. A 403 is read on as it is because OpenBao answers one both for a token lacking rights
/// to a key and for an encryption of a key that does not exist, which would make it (Transit's
/// upsert), with a token that may not: the key's metadata says which. Nothing it says is
/// [`Error::NoSuchTenant`] but a key OpenBao says it has not, as a tenant without a KEK has its
/// cells made in the clear. A mount the token has no rights to, there or not, is one whose keys it
/// may not use: revoked, as OpenBao refuses the token before it looks for the mount.
///
/// The token needs, of each tenant's key `NAME`: `update` on `MOUNT/encrypt/NAME` and
/// `MOUNT/decrypt/NAME`, `read` on `MOUNT/keys/NAME` (without which a tenant without a key is
/// refused as revoked, its cells made neither sealed nor in the clear), and `auth/token/lookup-self`
/// (in the `default` policy); and not `create` on `MOUNT/encrypt/NAME`, which would make a key for
/// each tenant wrapping without one.
///
/// A call failing as the network does, or with a 412, 429 or 5xx, is made again, [`TRIES`] times
/// in all, after [`BACKOFF`], then four times that; each connection may take [`CONNECT_TIMEOUT`],
/// each request [`REQUEST_TIMEOUT`]. Its calls run on a Tokio runtime of its own, a thread, so that
/// whatever polls its futures, of any executor or none, waits for them alone. It logs nothing, and
/// no error says its token; the bytes of keys and of OpenBao's replies it holds are zeroed when
/// dropped, as far as its HTTP client lets it (not that client's own buffers).
pub struct OpenBao {
    inner: Arc<Inner>,
    runtime: Background,
}

/// What [`OpenBao`]'s calls share, on its runtime.
struct Inner {
    client: reqwest::Client,
    /// `http(s)://HOST:PORT/`.
    origin: url::Url,
    /// The Transit engine's path, a segment at a time.
    mount: Vec<String>,
    namespace: Option<HeaderValue>,
    names: KeyNames,
    token: Credentials,
    /// The KMS, as errors name it: its URL, without its query.
    shown: String,
}

/// The KMS `url` names ([`super::from_url`]), its token as the environment gives it.
pub(super) fn from_url(url: &str) -> Result<super::Shared, String> {
    let token = Token::from_env().map_err(|why| format!("{url:?}: {why}"))?;
    Ok(Arc::new(OpenBao::new(url, token)?))
}

impl OpenBao {
    /// The KMS `url` names (see [`OpenBao`]), calling with `token`.
    pub fn new(url: &str, token: Token) -> Result<Self, String> {
        let at = |why: &dyn std::fmt::Display| format!("{url:?}: {why}");
        let parsed = url::Url::parse(url).map_err(|err| at(&err))?;
        let tls = match parsed.scheme() {
            "openbao" => false,
            "openbaos" => true,
            _ => return Err(at(&"names no OpenBao: openbao:// or openbaos://")),
        };
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(at(&format!("holds credentials: OpenBao's token is {TOKEN_ENV}'s")));
        }
        let host = parsed.host_str().filter(|host| !host.is_empty());
        let host = host.ok_or_else(|| at(&"names no host: openbao://HOST[:PORT]/MOUNT"))?;
        let port = parsed.port().unwrap_or(DEFAULT_PORT);
        let mount: Vec<String> = (parsed.path_segments().into_iter().flatten())
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        if mount.is_empty() || mount.iter().any(|segment| segment == "." || segment == "..") {
            return Err(at(&"names no Transit engine's mount: openbao://HOST[:PORT]/MOUNT"));
        }
        let query: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        let (mut namespace, mut ca) = (None, None);
        for (name, value) in &query {
            match name.as_str() {
                "key" | "keys" => {}
                "namespace" => namespace = Some(value.as_str()),
                "ca" if tls => ca = Some(value.as_str()),
                "ca" => return Err(at(&"ca=FILE is for openbaos://, over TLS")),
                _ => return Err(at(&format!("{name:?} is none of key, keys, namespace or ca"))),
            }
        }
        let names = KeyNames::of_query(query.iter().map(|(n, v)| (n.as_str(), v.as_str())))
            .map_err(|why| at(&why))?;
        key_name(&names.of(0)).map_err(|why| at(&why))?;
        let namespace = (namespace.map(HeaderValue::from_str).transpose())
            .map_err(|_| at(&"the namespace is no HTTP header's value"))?;

        let mut client = reqwest::Client::builder()
            .user_agent(concat!("cladefs/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(tls);
        if let Some(path) = ca {
            let pem = std::fs::read(path).map_err(|err| at(&format!("the CAs {path:?}: {err}")))?;
            let cas = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|err| at(&format!("the CAs {path:?}: {err}")))?;
            if cas.is_empty() {
                return Err(at(&format!("the CAs {path:?}: no PEM certificate")));
            }
            client = client.tls_certs_only(cas);
        }
        let client = client.build().map_err(|err| at(&err))?;
        let origin = format!("{}://{host}:{port}/", if tls { "https" } else { "http" });
        let origin = url::Url::parse(&origin).map_err(|err| at(&err))?;
        let token = Credentials::new(token).map_err(|why| at(&why))?;
        let runtime = Background::new().map_err(|err| at(&format!("its runtime: {err}")))?;
        let shown = format!("{}://{host}:{port}/{}", parsed.scheme(), mount.join("/"));
        let inner = Inner { client, origin, mount, namespace, names, token, shown };
        Ok(Self { inner: Arc::new(inner), runtime })
    }

    /// Runs `call` on its runtime: dropped, its future drops the call.
    fn run<T: Send + 'static>(
        &self,
        tenant: u64,
        call: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> impl Future<Output = Result<T, Error>> + Send + 'static {
        let handle = self.runtime.handle();
        async move {
            let mut task = Aborting(handle.spawn(call));
            match (&mut task.0).await {
                Ok(done) => done,
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => Err(io(tenant, "the call was cancelled: its runtime is shut down")),
            }
        }
    }
}

impl Kms for OpenBao {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        let inner = self.inner.clone();
        let request = inner.encryption(tenant, key);
        self.run(tenant, async move { inner.wrap(tenant, request?).await })
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        let inner = self.inner.clone();
        let request = inner.decryption(tenant, wrapped);
        self.run(tenant, async move { inner.unwrap(tenant, request?).await })
    }
}

/// An encryption to ask of OpenBao: the key's name, the header, and the request's body.
struct Encryption {
    name: String,
    header: Vec<u8>,
    body: Bytes,
}

/// A decryption to ask of OpenBao: the key's name, the version of the key the ciphertext names,
/// and the request's body.
struct Decryption {
    name: String,
    version: u32,
    body: Bytes,
}

/// Which call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Encrypt,
    /// A decryption of a ciphertext of this version of the key.
    Decrypt(u32),
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Encrypt => "encrypt",
            Self::Decrypt(_) => "decrypt",
        }
    }
}

impl Inner {
    /// `tenant`'s key's name, if it is one Transit may have.
    fn name_of(&self, tenant: u64) -> Result<String, Error> {
        let name = self.names.of(tenant);
        key_name(&name).map_err(|why| io(tenant, &why))?;
        Ok(name)
    }

    fn encryption(&self, tenant: u64, key: &Secret) -> Result<Encryption, Error> {
        let name = self.name_of(tenant)?;
        let header = header(tenant, &name);
        let plaintext = Zeroizing::new(BASE64.encode(key.expose()));
        let header_text = BASE64.encode(&header);
        let body = json_body(&EncryptRequest {
            plaintext: &plaintext,
            associated_data: &header_text,
            context: &header_text,
        });
        Ok(Encryption { name, header, body })
    }

    fn decryption(&self, tenant: u64, wrapped: &Wrapped) -> Result<Decryption, Error> {
        let unauthentic = |why| Error::Unauthentic { tenant, source: Unauthentic(why) };
        let parsed = Parsed::of(&wrapped.0).map_err(|Unauthentic(why)| unauthentic(why))?;
        if parsed.tenant != tenant {
            return Err(unauthentic("a wrapped key, wrapped for another tenant"));
        }
        let name = self.name_of(tenant)?;
        if parsed.name != name.as_bytes() {
            return Err(unauthentic("a wrapped key, wrapped under another key"));
        }
        let header_text = BASE64.encode(parsed.header);
        let body = json_body(&DecryptRequest {
            ciphertext: parsed.ciphertext,
            associated_data: &header_text,
            context: &header_text,
        });
        Ok(Decryption { name, version: parsed.version, body })
    }

    async fn wrap(&self, tenant: u64, request: Encryption) -> Result<Wrapped, Error> {
        let Encryption { name, header, body } = request;
        let path = self.mounted(&["encrypt", &name]);
        let reply = self.call(tenant, Method::POST, &path, Some(body)).await?;
        if reply.status != StatusCode::OK {
            return Err(self.refusal(tenant, &name, Op::Encrypt, reply).await);
        }
        let encrypted: Reply<Encrypted> = reply.json(tenant, &self.what(Op::Encrypt, &name))?;
        let ciphertext = encrypted.data.ciphertext;
        if Parsed::version_of(&ciphertext).is_none() {
            let why = format!("{}: not Transit's ciphertext", self.what(Op::Encrypt, &name));
            return Err(io(tenant, &why));
        }
        Ok(Wrapped([&header[..], ciphertext.as_bytes()].concat()))
    }

    async fn unwrap(&self, tenant: u64, request: Decryption) -> Result<Secret, Error> {
        let Decryption { name, version, body } = request;
        let op = Op::Decrypt(version);
        let path = self.mounted(&["decrypt", &name]);
        let reply = self.call(tenant, Method::POST, &path, Some(body)).await?;
        if reply.status != StatusCode::OK {
            return Err(self.refusal(tenant, &name, op, reply).await);
        }
        let decrypted: Reply<Decrypted> = reply.json(tenant, &self.what(op, &name))?;
        let mut key = Zeroizing::new(Vec::with_capacity(KEY_LEN + 3));
        let unauthentic = |why| Error::Unauthentic { tenant, source: Unauthentic(why) };
        BASE64
            .decode_vec(decrypted.data.plaintext.as_bytes(), &mut key)
            .map_err(|_| unauthentic("a wrapped key, of no key's bytes"))?;
        let key = <&[u8; KEY_LEN]>::try_from(&key[..])
            .map_err(|_| unauthentic("a wrapped key, of the wrong length"))?;
        Ok(Secret::from_bytes(key))
    }

    /// What OpenBao's refusal of `op` with `tenant`'s key `name` says (see [`OpenBao`]).
    async fn refusal(&self, tenant: u64, name: &str, op: Op, reply: Received) -> Error {
        let why = reply.errors();
        let unauthentic = |why| Error::Unauthentic { tenant, source: Unauthentic(why) };
        match (reply.status, op) {
            (StatusCode::BAD_REQUEST, _) if why.contains("encryption key not found") => {
                missing(tenant, op)
            }
            (StatusCode::BAD_REQUEST, _) if why.contains("soft-deleted key") => {
                Error::Revoked { tenant }
            }
            (StatusCode::BAD_REQUEST, Op::Decrypt(_)) if why.contains("disallowed by policy") => {
                self.probe(tenant, name, op).await
            }
            (StatusCode::BAD_REQUEST, Op::Decrypt(_)) if why.contains("version is too new") => {
                unauthentic("a wrapped key, of a version the tenant's key has not")
            }
            (StatusCode::BAD_REQUEST, Op::Decrypt(_))
                if why.contains("message authentication failed")
                    || why.starts_with("invalid ciphertext") =>
            {
                unauthentic("a wrapped key, by the KMS's integrity check")
            }
            (StatusCode::FORBIDDEN, _) => self.probe(tenant, name, op).await,
            (status, _) => io(tenant, &format!("{}: {status}: {why}", self.what(op, name))),
        }
    }

    /// What became of `tenant`'s key `name` that OpenBao refused `op` with it, as its metadata and
    /// its token's say (see [`OpenBao`]).
    async fn probe(&self, tenant: u64, name: &str, op: Op) -> Error {
        let path = self.mounted(&["keys", name]);
        let reply = match self.call(tenant, Method::GET, &path, None).await {
            Ok(reply) => reply,
            Err(err) => return err,
        };
        let what = format!("reading key {name:?}, refused {}", op.name());
        match reply.status {
            // A key missing has no errors said; a route missing (no such mount) has.
            StatusCode::NOT_FOUND if reply.errors().is_empty() => missing(tenant, op),
            StatusCode::OK => {
                let key: Reply<KeyConfig> = match reply.json(tenant, &self.at(&what)) {
                    Ok(key) => key,
                    Err(err) => return err,
                };
                match op {
                    Op::Decrypt(version) if version < key.data.min_available_version => {
                        Error::Destroyed { tenant }
                    }
                    _ => Error::Revoked { tenant },
                }
            }
            StatusCode::FORBIDDEN => self.token_refused(tenant, &what).await,
            status => io(tenant, &format!("{}: {status}: {}", self.at(&what), reply.errors())),
        }
    }

    /// Why OpenBao refuses its token what it does: the tenant's revocation if it knows the token,
    /// else the token's own failing.
    async fn token_refused(&self, tenant: u64, what: &str) -> Error {
        let lookup = self.call(tenant, Method::GET, &["auth", "token", "lookup-self"], None);
        let reply = match lookup.await {
            Ok(reply) => reply,
            Err(err) => return err,
        };
        match reply.status {
            StatusCode::OK => Error::Revoked { tenant },
            StatusCode::FORBIDDEN => io(
                tenant,
                &format!(
                    "{}: OpenBao refuses cladefs's token: invalid, expired or revoked \
                     ({TOKEN_ENV}, {TOKEN_FILE_ENV})",
                    self.at(what)
                ),
            ),
            status => io(tenant, &format!("{}: {status}: {}", self.at(what), reply.errors())),
        }
    }

    /// The call `path` names, after `/v1/`, with the current token; refused (403) with a token read
    /// from a file, made once more if the file holds another since.
    async fn call(
        &self,
        tenant: u64,
        method: Method,
        path: &[&str],
        body: Option<Bytes>,
    ) -> Result<Received, Error> {
        let mut reread = false;
        loop {
            let token = self.token.current();
            let reply = retried(|| self.send(&method, path, body.clone(), &token))
                .await
                .map_err(|why| io(tenant, &format!("{}: {why}", self.at(&path.join("/")))))?;
            if reply.status == StatusCode::FORBIDDEN && !reread && self.token.reread(&token) {
                reread = true;
                continue;
            }
            return Ok(reply);
        }
    }

    /// One request, and its reply, read whole; or why neither was had.
    async fn send(
        &self,
        method: &Method,
        path: &[&str],
        body: Option<Bytes>,
        token: &HeaderValue,
    ) -> Result<Received, Failure> {
        let mut url = self.origin.clone();
        {
            let mut segments = url.path_segments_mut().expect("an http(s) URL has a path");
            segments.push("v1").extend(path);
        }
        let mut request = self.client.request(method.clone(), url).header("X-Vault-Token", token);
        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }
        if let Some(body) = body {
            request = request.header("Content-Type", "application/json").body(body);
        }
        let response = request.send().await.map_err(Failure::transport)?;
        let status = response.status();
        let body = response.bytes().await.map_err(Failure::transport)?;
        let reply = Received { status, body: Received::keep(body) };
        match status {
            StatusCode::PRECONDITION_FAILED | StatusCode::TOO_MANY_REQUESTS => {
                Err(Failure::Server(reply))
            }
            status if status.is_server_error() => Err(Failure::Server(reply)),
            _ => Ok(reply),
        }
    }

    /// `path`, under the Transit engine's mount.
    fn mounted<'a>(&'a self, path: &[&'a str]) -> Vec<&'a str> {
        self.mount.iter().map(String::as_str).chain(path.iter().copied()).collect()
    }

    /// The KMS, then `what` was asked of it.
    fn at(&self, what: &str) -> String {
        format!("{}: {what}", self.shown)
    }

    fn what(&self, op: Op, name: &str) -> String {
        self.at(&format!("{} with key {name:?}", op.name()))
    }
}

/// What a request had instead of a reply to keep.
enum Failure {
    /// The request was not made, or its reply not read: the network failed, or timed out.
    Transport(String),
    /// The server failed, or asked for the request again later.
    Server(Received),
}

impl Failure {
    fn transport(err: reqwest::Error) -> Self {
        let mut why = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            why.push_str(": ");
            why.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Transport(why)
    }
}

/// `send`'s reply, made again, [`TRIES`] times in all, while it fails as the network or the server
/// does: the last failure said, if every try failed.
async fn retried<F: Future<Output = Result<Received, Failure>>>(
    mut send: impl FnMut() -> F,
) -> Result<Received, String> {
    let mut wait = BACKOFF;
    let mut tries = 1;
    loop {
        let why = match send().await {
            Ok(reply) => return Ok(reply),
            Err(Failure::Transport(why)) => why,
            Err(Failure::Server(reply)) => format!("{}: {}", reply.status, reply.errors()),
        };
        if tries == TRIES {
            return Err(format!("{why} ({tries} tries)"));
        }
        tokio::time::sleep(wait).await;
        wait *= 4;
        tries += 1;
    }
}

/// A reply, read whole, its body zeroed when dropped if no one else holds it.
struct Received {
    status: StatusCode,
    body: Bytes,
}

impl Received {
    /// `body`, copied out of the client's buffer if it shares one, that one zeroed if it can be.
    fn keep(body: Bytes) -> Bytes {
        match body.try_into_mut() {
            Ok(body) => body.freeze(),
            Err(shared) => {
                let copy = Bytes::from_owner(Zeroizing::new(shared.to_vec()));
                drop(shared);
                copy
            }
        }
    }

    /// The reply's JSON, as a `T`.
    fn json<T: for<'de> Deserialize<'de>>(&self, tenant: u64, what: &str) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|err| {
            io(tenant, &format!("{what}: {}: a reply of no JSON it reads: {err}", self.status))
        })
    }

    /// The errors OpenBao's reply says, white space collapsed: none if it says none.
    fn errors(&self) -> String {
        #[derive(Deserialize)]
        struct Errors {
            errors: Vec<String>,
        }
        match serde_json::from_slice::<Errors>(&self.body) {
            Ok(errors) => errors.errors.join("; ").split_whitespace().collect::<Vec<_>>().join(" "),
            Err(_) => String::from_utf8_lossy(&self.body[..self.body.len().min(200)]).into_owned(),
        }
    }
}

impl Drop for Received {
    fn drop(&mut self) {
        if let Ok(mut body) = std::mem::take(&mut self.body).try_into_mut() {
            body[..].zeroize();
        }
    }
}

/// OpenBao's replies: what each holds in its `data`.
#[derive(Deserialize)]
struct Reply<T> {
    data: T,
}

#[derive(Deserialize)]
struct Encrypted {
    ciphertext: String,
}

#[derive(Deserialize)]
struct Decrypted {
    plaintext: Zeroizing<String>,
}

#[derive(Deserialize)]
struct KeyConfig {
    #[serde(default)]
    min_available_version: u32,
}

#[derive(serde::Serialize)]
struct EncryptRequest<'a> {
    plaintext: &'a str,
    associated_data: &'a str,
    context: &'a str,
}

#[derive(serde::Serialize)]
struct DecryptRequest<'a> {
    ciphertext: &'a str,
    associated_data: &'a str,
    context: &'a str,
}

/// `request`, as a body whose bytes are zeroed once the client drops it: written into room enough
/// for it, so that no copy is left behind growing it.
fn json_body(request: &impl serde::Serialize) -> Bytes {
    let mut body = Zeroizing::new(Vec::with_capacity(1024));
    serde_json::to_writer(&mut *body, request).expect("a request serializes");
    Bytes::from_owner(body)
}

/// The error of `tenant`'s key missing in OpenBao when `op` was asked: no KEK, for a wrap; for an
/// unwrap, a KEK destroyed, as the wrapped key's header says it wrapped it.
fn missing(tenant: u64, op: Op) -> Error {
    match op {
        Op::Encrypt => Error::NoSuchTenant { tenant },
        Op::Decrypt(_) => Error::Destroyed { tenant },
    }
}

fn io(tenant: u64, why: &str) -> Error {
    Error::Io { tenant, source: io::Error::other(why.to_owned()) }
}

/// Checks `name` is a Transit key's name, in a path segment of its own: letters, digits, `-`, `_`
/// and `.`, and not `.` or `..`.
fn key_name(name: &str) -> Result<(), String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.chars().all(plain) {
        return Err(format!("the key name {name:?} is no Transit key's: letters, digits, -, _, ."));
    }
    if name == "." || name == ".." {
        return Err(format!("the key name {name:?} is no Transit key's"));
    }
    Ok(())
}

/// The header of a key wrapped for `tenant` under its key `name` (see [`OpenBao`]).
fn header(tenant: u64, name: &str) -> Vec<u8> {
    let name_len = u16::try_from(name.len()).expect("key names are short");
    let mut header = Vec::with_capacity(MAGIC.len() + 2 + BACKEND.len() + 8 + 2 + name.len());
    header.extend_from_slice(MAGIC);
    header.push(FORMAT);
    header.push(BACKEND.len() as u8);
    header.extend_from_slice(BACKEND);
    header.extend_from_slice(&tenant.to_be_bytes());
    header.extend_from_slice(&name_len.to_be_bytes());
    header.extend_from_slice(name.as_bytes());
    header
}

/// A key [`OpenBao`] wrapped, as its header says: whether it is authentic only OpenBao tells.
#[derive(Debug, PartialEq, Eq)]
struct Parsed<'a> {
    header: &'a [u8],
    tenant: u64,
    name: &'a [u8],
    ciphertext: &'a str,
    version: u32,
}

impl<'a> Parsed<'a> {
    fn of(bytes: &'a [u8]) -> Result<Self, Unauthentic> {
        let by_header = Unauthentic("a wrapped key, by its header");
        let mut rest = bytes;
        let mut take = |len: usize| {
            let (taken, left) = rest.split_at_checked(len).ok_or(by_header)?;
            rest = left;
            Ok::<_, Unauthentic>(taken)
        };
        if take(MAGIC.len())? != MAGIC {
            return Err(by_header);
        }
        if take(1)? != [FORMAT] {
            return Err(Unauthentic("a wrapped key, of another format"));
        }
        let backend_len = take(1)?[0];
        if take(backend_len.into())? != BACKEND {
            return Err(Unauthentic("a wrapped key, wrapped by another KMS"));
        }
        let tenant = u64::from_be_bytes(take(8)?.try_into().expect("8 bytes"));
        let name_len = u16::from_be_bytes(take(2)?.try_into().expect("2 bytes"));
        let name = take(name_len.into())?;
        let header = &bytes[..bytes.len() - rest.len()];
        let not_transit = Unauthentic("a wrapped key, not Transit's ciphertext");
        let ciphertext = std::str::from_utf8(rest).map_err(|_| not_transit)?;
        let version = Self::version_of(ciphertext).ok_or(not_transit)?;
        Ok(Self { header, tenant, name, ciphertext, version })
    }

    /// The version of the key Transit's `ciphertext` names: `vault:vN:...`.
    fn version_of(ciphertext: &str) -> Option<u32> {
        let (version, sealed) = ciphertext.strip_prefix(CIPHERTEXT_PREFIX)?.split_once(':')?;
        let digits = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
        (digits && !sealed.is_empty()).then(|| version.parse().ok()).flatten()
    }
}

/// The token [`OpenBao`] calls with, and where it came from.
struct Credentials {
    source: Token,
    current: RwLock<HeaderValue>,
}

impl Credentials {
    fn new(source: Token) -> Result<Self, String> {
        let current = RwLock::new(Self::read(&source)?);
        Ok(Self { source, current })
    }

    /// The token `source` holds, white space around it ignored.
    fn read(source: &Token) -> Result<HeaderValue, String> {
        let from_file;
        let token = match source {
            Token::Value(token) => token.trim(),
            Token::File(path) => {
                from_file = Zeroizing::new(
                    std::fs::read_to_string(path)
                        .map_err(|err| format!("the token file {path:?}: {err}"))?,
                );
                from_file.trim()
            }
        };
        if token.is_empty() {
            return Err("OpenBao's token is empty".to_owned());
        }
        let mut header = HeaderValue::from_str(token)
            .map_err(|_| "OpenBao's token is no HTTP header's value".to_owned())?;
        header.set_sensitive(true);
        Ok(header)
    }

    fn current(&self) -> HeaderValue {
        self.current.read().unwrap().clone()
    }

    /// Reads the token's file again, if it is in one: whether it holds a token other than `used`,
    /// now the current one.
    fn reread(&self, used: &HeaderValue) -> bool {
        if matches!(self.source, Token::Value(_)) {
            return false;
        }
        match Self::read(&self.source) {
            Ok(token) if token != used => {
                *self.current.write().unwrap() = token;
                true
            }
            _ => false,
        }
    }
}

/// [`OpenBao`]'s runtime, one thread of its own, shut down without waiting when dropped.
struct Background(Option<tokio::runtime::Runtime>);

impl Background {
    fn new() -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("cladefs-openbao")
            .enable_all()
            .build()?;
        Ok(Self(Some(runtime)))
    }

    fn handle(&self) -> tokio::runtime::Handle {
        self.0.as_ref().expect("a runtime until dropped").handle().clone()
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// A task, aborted when dropped: a call whose caller gave up on it.
struct Aborting<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::openbao::DevServer;
    use assert_matches::assert_matches;
    use futures::executor::block_on;

    const MOUNT: &str = "transit";
    const QUERY: &str = "key=cladefs-tenant-{tenant}";

    /// A dev server, its Transit engine at [`MOUNT`] holding keys for `tenants`, and a token of
    /// cladefs's policy, the least it needs.
    struct Bao {
        server: DevServer,
        token: String,
    }

    fn bao(tenants: &[u64]) -> Option<Bao> {
        bao_of(DevServer::start()?, tenants)
    }

    fn bao_of(server: DevServer, tenants: &[u64]) -> Option<Bao> {
        server.transit(MOUNT);
        for &tenant in tenants {
            server.bao(&["write", "-f", &key_path(tenant)]);
        }
        server.policy("cladefs", &DevServer::cladefs_policy(MOUNT, "cladefs-tenant-*"));
        let token = server.token("cladefs");
        Some(Bao { server, token })
    }

    /// The path of `tenant`'s key in the Transit engine.
    fn key_path(tenant: u64) -> String {
        format!("{MOUNT}/keys/cladefs-tenant-{tenant:x}")
    }

    impl Bao {
        /// A KMS of its Transit engine, calling with its token.
        fn kms(&self) -> OpenBao {
            self.kms_with(QUERY, Token::Value(self.token.clone()))
        }

        fn kms_with(&self, query: &str, token: Token) -> OpenBao {
            OpenBao::new(&self.server.url(MOUNT, query), token).unwrap()
        }

        fn bao(&self, args: &[&str]) -> String {
            self.server.bao(args)
        }

        /// What a KMS's errors start with, of the call `path` makes.
        fn failed(&self, tenant: u64, path: &str) -> String {
            let port = self.server.port();
            format!("tenant {tenant}: the KMS failed: openbao://127.0.0.1:{port}/{MOUNT}: {path}")
        }
    }

    fn wrap(kms: &impl Kms, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        block_on(kms.wrap(tenant, key))
    }

    fn unwrap(kms: &impl Kms, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        block_on(kms.unwrap(tenant, wrapped))
    }

    /// The error's text: each names its tenant, which is what callers report.
    fn refusal<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        result.unwrap_err().to_string()
    }

    /// A port of 127.0.0.1 nothing listens on.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn token(value: &str) -> Token {
        Token::Value(value.to_owned())
    }

    #[test]
    fn urls_name_the_server_its_mount_and_its_keys() {
        let kms =
            OpenBao::new("openbao://bao.internal/transit?key=t-{tenant}", token("t")).unwrap();
        assert_eq!(kms.inner.origin.as_str(), "http://bao.internal:8200/");
        assert_eq!(kms.inner.mount, ["transit"]);
        assert_eq!(kms.inner.shown, "openbao://bao.internal:8200/transit");
        assert_eq!(kms.inner.names.of(0x2a), "t-2a");
        assert_eq!(kms.inner.namespace, None);
        let kms = OpenBao::new(
            "openbaos://[::1]:8300/kms/transit/?key=t-{tenant}&namespace=team%2Fa",
            token("t"),
        )
        .unwrap();
        assert_eq!(kms.inner.origin.as_str(), "https://[::1]:8300/");
        assert_eq!(kms.inner.mount, ["kms", "transit"]);
        assert_eq!(kms.inner.shown, "openbaos://[::1]:8300/kms/transit");
        assert_eq!(kms.inner.namespace, Some(HeaderValue::from_static("team/a")));
        assert_eq!(kms.inner.mounted(&["encrypt", "t-1"]), ["kms", "transit", "encrypt", "t-1"]);
    }

    #[test]
    fn urls_naming_no_openbao_are_refused() {
        let refused = |url: &str| OpenBao::new(url, token("t")).map(|_| ()).unwrap_err();
        assert_eq!(
            refused("openbao://h/transit"),
            "\"openbao://h/transit\": the KMS URL names no key template: key=...{tenant}..."
        );
        assert_eq!(
            refused("openbao://h/transit?key=one"),
            "\"openbao://h/transit?key=one\": the key template \"one\" holds no {tenant}: every \
             tenant would share one KEK"
        );
        assert_eq!(
            refused("openbao://h/?key={tenant}"),
            "\"openbao://h/?key={tenant}\": names no Transit engine's mount: \
             openbao://HOST[:PORT]/MOUNT"
        );
        assert_eq!(
            refused("openbao://root:secret@h/t?key={tenant}"),
            "\"openbao://root:secret@h/t?key={tenant}\": holds credentials: OpenBao's token is \
             CLADEFS_KMS_OPENBAO_TOKEN's"
        );
        assert_eq!(
            refused("openbao://h/t?key={tenant}&token=x"),
            "\"openbao://h/t?key={tenant}&token=x\": \"token\" is none of key, keys, namespace \
             or ca"
        );
        assert_eq!(
            refused("openbao://h/t?key={tenant}&ca=/ca.pem"),
            "\"openbao://h/t?key={tenant}&ca=/ca.pem\": ca=FILE is for openbaos://, over TLS"
        );
        assert_eq!(
            refused("openbao://h/t?key=a/{tenant}"),
            "\"openbao://h/t?key=a/{tenant}\": the key name \"a/0\" is no Transit key's: \
             letters, digits, -, _, ."
        );
        assert_eq!(
            refused("openbaos://h/t?key={tenant}&ca=/no/such/ca.pem"),
            "\"openbaos://h/t?key={tenant}&ca=/no/such/ca.pem\": the CAs \"/no/such/ca.pem\": No \
             such file or directory (os error 2)"
        );
        assert_eq!(
            refused("vault://h/t?key={tenant}"),
            "\"vault://h/t?key={tenant}\": names no OpenBao: openbao:// or openbaos://"
        );
    }

    #[test]
    fn tokens_come_from_one_variable_of_the_environment() {
        let env = |token: Option<&str>, file: Option<&str>| {
            Token::of_env(|name| match name {
                TOKEN_ENV => token.map(str::to_owned),
                TOKEN_FILE_ENV => file.map(str::to_owned),
                _ => None,
            })
        };
        assert_matches!(env(Some("s.abc"), None), Ok(Token::Value(token)) if token == "s.abc");
        assert_matches!(env(Some(""), Some("/run/token")), Ok(Token::File(path)) if path == std::path::Path::new("/run/token"));
        assert_eq!(
            env(Some("s.abc"), Some("/run/token")).unwrap_err(),
            "both CLADEFS_KMS_OPENBAO_TOKEN and CLADEFS_KMS_OPENBAO_TOKEN_FILE are set: one"
        );
        assert_eq!(
            env(None, None).unwrap_err(),
            "OpenBao's token is in neither CLADEFS_KMS_OPENBAO_TOKEN nor CLADEFS_KMS_OPENBAO_TOKEN_FILE"
        );
        // Neither says its token.
        assert_eq!(format!("{:?}", token("s.abc")), "Token::Value(..)");
    }

    #[test]
    fn token_files_are_read_trimmed_and_again_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "  s.first\n").unwrap();
        let credentials = Credentials::new(Token::File(path.clone())).unwrap();
        let first = credentials.current();
        assert_eq!(first, "s.first");
        assert!(first.is_sensitive());
        assert!(!credentials.reread(&first));
        std::fs::write(&path, "s.second\n").unwrap();
        assert!(credentials.reread(&first));
        assert_eq!(credentials.current(), "s.second");
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(
            Credentials::new(Token::File(path)).err(),
            Some("OpenBao's token is empty".to_owned())
        );
        assert!(!Credentials::new(token("s.v")).unwrap().reread(&HeaderValue::from_static("s.x")));
    }

    #[test]
    fn headers_name_the_backend_the_tenant_and_the_key() {
        let header = header(0x2a, "k-2a");
        assert_eq!(header, b"fxkm\x01\x07openbao\0\0\0\0\0\0\0\x2a\0\x04k-2a");
        let wrapped = [&header[..], b"vault:v12:c2VhbGVk"].concat();
        let parsed = Parsed::of(&wrapped).unwrap();
        assert_eq!(
            parsed,
            Parsed {
                header: &header,
                tenant: 0x2a,
                name: b"k-2a",
                ciphertext: "vault:v12:c2VhbGVk",
                version: 12,
            }
        );
        for ciphertext in ["vault:v:x", "vault:v1:", "vault:vx:y", "vault:v1", "v1:abc"] {
            assert_eq!(Parsed::version_of(ciphertext), None, "{ciphertext}");
        }
    }

    /// AC #3: what another KMS wrapped, or this one for another tenant or under another key, or
    /// in another format, is refused as not authentic, OpenBao not asked: this one names a port
    /// nothing listens on, which a call would fail as I/O.
    #[test]
    fn keys_wrapped_elsewhere_are_refused_unasked() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = dir.path().join("keys");
        std::fs::write(&overrides, "8 tenants-own\n").unwrap();
        let url = format!(
            "openbao://127.0.0.1:{}/{MOUNT}?{QUERY}&keys={}",
            closed_port(),
            overrides.display()
        );
        let kms = OpenBao::new(&url, token("s.token")).unwrap();
        let ciphertext = b"vault:v1:c2VhbGVk";
        let refused = |tenant, bytes: Vec<u8>| refusal(unwrap(&kms, tenant, &Wrapped(bytes)));
        let local = crate::kms::LocalKms::open(dir.path().join("local")).unwrap();
        local.create(7).unwrap();
        let by_local = wrap(&local, 7, &Secret::random()).unwrap();
        assert_eq!(refused(7, by_local.0), "tenant 7: not authentic: a wrapped key, by its header");
        assert_eq!(
            refused(7, Secret::random().wrap(&Secret::random()).0),
            "tenant 7: not authentic: a wrapped key, by its header"
        );
        let mut other_backend = header(7, "cladefs-tenant-7");
        other_backend[MAGIC.len() + 2..MAGIC.len() + 2 + BACKEND.len()].copy_from_slice(b"gcp-kms");
        assert_eq!(
            refused(7, [&other_backend[..], ciphertext].concat()),
            "tenant 7: not authentic: a wrapped key, wrapped by another KMS"
        );
        let mut other_format = header(7, "cladefs-tenant-7");
        other_format[MAGIC.len()] = 2;
        assert_eq!(
            refused(7, [&other_format[..], ciphertext].concat()),
            "tenant 7: not authentic: a wrapped key, of another format"
        );
        let for_7 = [&header(7, "cladefs-tenant-7")[..], ciphertext].concat();
        assert_eq!(
            refused(9, for_7.clone()),
            "tenant 9: not authentic: a wrapped key, wrapped for another tenant"
        );
        // Tenant 8's key is its own, by the overrides: one named by the template is not its.
        assert_eq!(
            refused(8, [&header(8, "cladefs-tenant-8")[..], ciphertext].concat()),
            "tenant 8: not authentic: a wrapped key, wrapped under another key"
        );
        assert_eq!(
            refused(7, [&header(7, "cladefs-tenant-7")[..], b"vault:c2VhbGVk"].concat()),
            "tenant 7: not authentic: a wrapped key, not Transit's ciphertext"
        );
        assert_eq!(
            refused(7, header(7, "cladefs-tenant-7")[..10].to_vec()),
            "tenant 7: not authentic: a wrapped key, by its header"
        );
        // A key it would ask of OpenBao fails as I/O, saying what failed, and not its token.
        let failed = refused(7, for_7);
        let start = format!(
            "tenant 7: the KMS failed: openbao://127.0.0.1:{}/{MOUNT}: \
             transit/decrypt/cladefs-tenant-7: error sending request for url",
            url::Url::parse(&url).unwrap().port().unwrap()
        );
        assert!(failed.starts_with(&start), "{failed}");
        assert!(failed.ends_with("(3 tries)"), "{failed}");
        assert!(!failed.contains("s.token"), "{failed}");
    }

    /// AC #1: keys wrapped under a tenant's Transit key unwrap for it, by any KMS of the same
    /// engine, whatever runs their futures; another tenant's key unwraps none of them.
    #[test]
    fn wrapped_keys_unwrap_for_their_tenant() {
        let Some(bao) = bao(&[7, 8]) else { return };
        let kms = bao.kms();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        let header_7 = header(7, "cladefs-tenant-7");
        assert_eq!(&wrapped.0[..header_7.len()], &header_7[..]);
        assert!(wrapped.0[header_7.len()..].starts_with(b"vault:v1:"), "{wrapped:?}");
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        // Another process's KMS, polled on a Tokio runtime, and dropped in it.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let again = bao.kms();
            assert_eq!(again.unwrap(7, &wrapped).await.unwrap().expose(), key.expose());
        });
        // Tenant 8's key, relabelled as its, fails Transit's check.
        let relabelled =
            [&header(8, "cladefs-tenant-8")[..], &wrapped.0[header_7.len()..]].concat();
        assert_eq!(
            refusal(unwrap(&kms, 8, &Wrapped(relabelled))),
            "tenant 8: not authentic: a wrapped key, by the KMS's integrity check"
        );
    }

    /// A `derived` key derives a key of its own for each header, its `context`: wraps and unwraps
    /// as any other, a relabelled header failing its check.
    #[test]
    fn derived_keys_wrap_and_unwrap() {
        let Some(bao) = bao(&[]) else { return };
        bao.bao(&["write", &key_path(7), "derived=true"]);
        bao.bao(&["write", &key_path(8), "derived=true"]);
        let kms = bao.kms();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        let header_len = header(7, "cladefs-tenant-7").len();
        let as_8 = [&header(8, "cladefs-tenant-8")[..], &wrapped.0[header_len..]].concat();
        assert_eq!(
            refusal(unwrap(&kms, 8, &Wrapped(as_8))),
            "tenant 8: not authentic: a wrapped key, by the KMS's integrity check"
        );
    }

    /// AC #1: a rotated key wraps under its new version, and its old ones still unwrap.
    #[test]
    fn rotated_keys_unwrap_what_older_versions_wrapped() {
        let Some(bao) = bao(&[7]) else { return };
        let kms = bao.kms();
        let (old_key, new_key) = (Secret::random(), Secret::random());
        let old = wrap(&kms, 7, &old_key).unwrap();
        bao.bao(&["write", "-f", &format!("{}/rotate", key_path(7))]);
        bao.bao(&["write", "-f", &format!("{}/rotate", key_path(7))]);
        let new = wrap(&kms, 7, &new_key).unwrap();
        assert_eq!(Parsed::of(&old.0).unwrap().version, 1);
        assert_eq!(Parsed::of(&new.0).unwrap().version, 3);
        assert_eq!(unwrap(&kms, 7, &old).unwrap().expose(), old_key.expose());
        assert_eq!(unwrap(&kms, 7, &new).unwrap().expose(), new_key.expose());
    }

    /// AC #2: a tenant's key whose use cladefs's token lost, soft-deleted, or with its older
    /// versions kept from decrypting, is revoked: until given back.
    #[test]
    fn revoked_keys_wrap_and_unwrap_nothing_until_given_back() {
        let Some(bao) = bao(&[7, 8]) else { return };
        let kms = bao.kms();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        let others = wrap(&kms, 8, &key).unwrap();
        let works = || assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        let revoked = |when: &str| {
            assert_matches!(unwrap(&kms, 7, &wrapped), Err(Error::Revoked { tenant: 7 }), "{when}");
            assert_eq!(refusal(wrap(&kms, 7, &key)), "tenant 7: its KEK is revoked", "{when}");
        };

        // The policy keeps tenant 8's key alone: 7's is neither usable nor readable.
        bao.server.policy("cladefs", &DevServer::cladefs_policy(MOUNT, "cladefs-tenant-8"));
        revoked("its policy");
        assert_eq!(unwrap(&kms, 8, &others).unwrap().expose(), key.expose());
        // Readable, but neither encrypting nor decrypting.
        let readable = format!(
            "{}path \"{MOUNT}/keys/cladefs-tenant-7\" {{ capabilities = [\"read\"] }}\n",
            DevServer::cladefs_policy(MOUNT, "cladefs-tenant-8")
        );
        bao.server.policy("cladefs", &readable);
        revoked("its policy, readable");
        bao.server.policy("cladefs", &DevServer::cladefs_policy(MOUNT, "cladefs-tenant-*"));
        works();

        bao.bao(&["delete", &format!("{}/soft-delete", key_path(7))]);
        revoked("soft-deleted");
        bao.bao(&["write", "-f", &format!("{}/soft-delete-restore", key_path(7))]);
        works();

        bao.bao(&["write", "-f", &format!("{}/rotate", key_path(7))]);
        bao.bao(&["write", &format!("{}/config", key_path(7)), "min_decryption_version=2"]);
        assert_matches!(unwrap(&kms, 7, &wrapped), Err(Error::Revoked { tenant: 7 }));
        let newer = wrap(&kms, 7, &key).unwrap();
        assert_eq!(unwrap(&kms, 7, &newer).unwrap().expose(), key.expose());
        bao.bao(&["write", &format!("{}/config", key_path(7)), "min_decryption_version=1"]);
        works();
    }

    /// AC #2: a tenant's key whose versions that wrapped are trimmed, or that is deleted, is
    /// destroyed for what they wrapped. Deleted, OpenBao has no more of it than of a key never
    /// made: a wrap says the tenant has none.
    #[test]
    fn destroyed_keys_unwrap_nothing_again() {
        let Some(bao) = bao(&[7, 8]) else { return };
        let kms = bao.kms();
        let key = Secret::random();
        let old = wrap(&kms, 7, &key).unwrap();
        bao.bao(&["write", "-f", &format!("{}/rotate", key_path(7))]);
        let config = format!("{}/config", key_path(7));
        bao.bao(&["write", &config, "min_decryption_version=2", "min_encryption_version=2"]);
        bao.bao(&["write", &format!("{}/trim", key_path(7)), "min_available_version=2"]);
        assert_matches!(unwrap(&kms, 7, &old), Err(Error::Destroyed { tenant: 7 }));
        assert_eq!(refusal(unwrap(&kms, 7, &old)), "tenant 7: its KEK is destroyed");
        let new = wrap(&kms, 7, &key).unwrap();
        assert_eq!(unwrap(&kms, 7, &new).unwrap().expose(), key.expose());

        let wrapped = wrap(&kms, 8, &key).unwrap();
        bao.bao(&["write", &format!("{}/config", key_path(8)), "deletion_allowed=true"]);
        bao.bao(&["delete", &key_path(8)]);
        assert_matches!(unwrap(&kms, 8, &wrapped), Err(Error::Destroyed { tenant: 8 }));
        assert_matches!(wrap(&kms, 8, &key), Err(Error::NoSuchTenant { tenant: 8 }));
    }

    /// AC #2: a tenant with no key is said to have none, if cladefs's token may read keys' names;
    /// else revoked, its cells made neither sealed nor in the clear. A token that may make keys
    /// (`create`, as root's) makes one for each tenant it wraps for (Transit's upsert).
    #[test]
    fn tenants_without_keys_are_named() {
        let Some(bao) = bao(&[7]) else { return };
        let kms = bao.kms();
        let key = Secret::random();
        assert_matches!(wrap(&kms, 9, &key), Err(Error::NoSuchTenant { tenant: 9 }));
        assert_eq!(refusal(wrap(&kms, 9, &key)), "tenant 9: no KEK in the KMS");
        let unreadable = "path \"transit/encrypt/*\" { capabilities = [\"update\"] }\n\
                          path \"transit/decrypt/*\" { capabilities = [\"update\"] }\n";
        bao.server.policy("cladefs", unreadable);
        assert_matches!(wrap(&kms, 9, &key), Err(Error::Revoked { tenant: 9 }));

        let root = bao.kms_with(QUERY, token(bao.server.root()));
        let wrapped = wrap(&root, 9, &key).unwrap();
        assert_eq!(unwrap(&root, 9, &wrapped).unwrap().expose(), key.expose());
        assert_eq!(bao.bao(&["read", "-field=name", &key_path(9)]), "cladefs-tenant-9");
    }

    /// Two tenants sharing a key (each naming it, as overrides may) unwrap only their own wraps:
    /// the header Transit authenticates names the tenant. Ciphertexts changed, or of versions the
    /// key never had, are not authentic.
    #[test]
    fn shared_keys_unwrap_only_their_tenants_wraps() {
        let Some(bao) = bao(&[]) else { return };
        bao.bao(&["write", "-f", &format!("{MOUNT}/keys/shared")]);
        bao.server.policy("cladefs", &DevServer::cladefs_policy(MOUNT, "*"));
        let overrides = bao.server.dir().join("keys");
        std::fs::write(&overrides, "1 shared\n2 shared\n").unwrap();
        let query = format!("{QUERY}&keys={}", overrides.display());
        let kms = bao.kms_with(&query, token(&bao.token));
        let key = Secret::random();
        let wrapped = wrap(&kms, 1, &key).unwrap();
        let header_len = header(1, "shared").len();
        let as_2 = [&header(2, "shared")[..], &wrapped.0[header_len..]].concat();
        assert_eq!(
            refusal(unwrap(&kms, 2, &Wrapped(as_2))),
            "tenant 2: not authentic: a wrapped key, by the KMS's integrity check"
        );
        let mut tampered = wrapped.clone();
        let at = tampered.0.len() - 6;
        tampered.0[at] = if tampered.0[at] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            refusal(unwrap(&kms, 1, &tampered)),
            "tenant 1: not authentic: a wrapped key, by the KMS's integrity check"
        );
        let ciphertext = std::str::from_utf8(&wrapped.0[header_len..]).unwrap();
        let too_new = ciphertext.replacen("vault:v1:", "vault:v9:", 1);
        let too_new = [&header(1, "shared")[..], too_new.as_bytes()].concat();
        assert_eq!(
            refusal(unwrap(&kms, 1, &Wrapped(too_new))),
            "tenant 1: not authentic: a wrapped key, of a version the tenant's key has not"
        );
        assert_eq!(unwrap(&kms, 1, &wrapped).unwrap().expose(), key.expose());
    }

    /// A token OpenBao refuses altogether is cladefs's failing, not the tenant's revoking: I/O,
    /// saying so, and not the token. One in a file is read again, and a new one taken up.
    #[test]
    fn tokens_refused_fail_and_files_give_new_ones() {
        let Some(bao) = bao(&[7]) else { return };
        let file = bao.server.dir().join("token");
        std::fs::write(&file, format!("{}\n", bao.token)).unwrap();
        let kms = bao.kms_with(QUERY, Token::File(file.clone()));
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        bao.bao(&["token", "revoke", &bao.token]);
        let refused = refusal(unwrap(&kms, 7, &wrapped));
        assert_eq!(
            refused,
            format!(
                "{}: reading key \"cladefs-tenant-7\", refused decrypt: OpenBao refuses cladefs's \
                 token: invalid, expired or revoked (CLADEFS_KMS_OPENBAO_TOKEN, \
                 CLADEFS_KMS_OPENBAO_TOKEN_FILE)",
                bao.failed(7, "").trim_end_matches(": ")
            )
        );
        assert!(!refused.contains(&bao.token));
        std::fs::write(&file, bao.server.token("cladefs")).unwrap();
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        assert_matches!(wrap(&kms, 7, &key), Ok(_));
    }

    /// AC #2: the server failing, sealed, or without the mount named, fails as I/O, saying what
    /// failed, after its tries. A mount cladefs's token has no rights to, there or not, is one whose
    /// keys it may not use: revoked, as OpenBao refuses the token before it looks for the mount.
    #[test]
    fn servers_failing_fail_as_io() {
        let Some(bao) = bao(&[7]) else { return };
        let kms = bao.kms();
        let url = bao.server.url("elsewhere", QUERY);
        let elsewhere = OpenBao::new(&url, token(bao.server.root())).unwrap();
        assert_eq!(
            refusal(wrap(&elsewhere, 7, &Secret::random())),
            format!(
                "tenant 7: the KMS failed: openbao://127.0.0.1:{}/elsewhere: encrypt with key \
                 \"cladefs-tenant-7\": 404 Not Found: no handler for route \
                 \"elsewhere/encrypt/cladefs-tenant-7\". route entry not found.",
                bao.server.port()
            )
        );
        let unpermitted = OpenBao::new(&url, token(&bao.token)).unwrap();
        assert_matches!(
            wrap(&unpermitted, 7, &Secret::random()),
            Err(Error::Revoked { tenant: 7 })
        );
        let wrapped = wrap(&kms, 7, &Secret::random()).unwrap();
        bao.bao(&["operator", "seal"]);
        assert_eq!(
            refusal(unwrap(&kms, 7, &wrapped)),
            format!(
                "{}: 503 Service Unavailable: Vault is sealed (3 tries)",
                bao.failed(7, "transit/decrypt/cladefs-tenant-7")
            )
        );
    }

    /// A namespace's Transit engine, named by the URL's `namespace`.
    #[test]
    fn namespaces_hold_their_own_engines() {
        let Some(server) = DevServer::start() else { return };
        server.bao(&["namespace", "create", "team"]);
        server.bao(&["secrets", "enable", "-namespace=team", &format!("-path={MOUNT}"), "transit"]);
        server.bao(&["write", "-namespace=team", "-f", &key_path(7)]);
        let in_team = OpenBao::new(
            &server.url(MOUNT, &format!("{QUERY}&namespace=team")),
            token(server.root()),
        )
        .unwrap();
        let key = Secret::random();
        let wrapped = wrap(&in_team, 7, &key).unwrap();
        assert_eq!(unwrap(&in_team, 7, &wrapped).unwrap().expose(), key.expose());
        let in_root = OpenBao::new(&server.url(MOUNT, QUERY), token(server.root())).unwrap();
        assert_eq!(
            refusal(unwrap(&in_root, 7, &wrapped)),
            format!(
                "tenant 7: the KMS failed: openbao://127.0.0.1:{}/{MOUNT}: decrypt with key \
                 \"cladefs-tenant-7\": 404 Not Found: no handler for route \
                 \"transit/decrypt/cladefs-tenant-7\". route entry not found.",
                server.port()
            )
        );
    }

    /// `openbaos://`, over TLS, trusting the CA its `ca` names; refused without it.
    #[test]
    fn tls_servers_are_trusted_by_their_ca() {
        let Some(bao) = DevServer::start_tls().and_then(|server| bao_of(server, &[7])) else {
            return;
        };
        let kms = bao.kms();
        let key = Secret::random();
        let wrapped = wrap(&kms, 7, &key).unwrap();
        assert_eq!(unwrap(&kms, 7, &wrapped).unwrap().expose(), key.expose());
        let url = format!("openbaos://127.0.0.1:{}/{MOUNT}?{QUERY}", bao.server.port());
        let untrusting = OpenBao::new(&url, Token::Value(bao.token.clone())).unwrap();
        assert_eq!(
            refusal(unwrap(&untrusting, 7, &wrapped)),
            format!(
                "tenant 7: the KMS failed: openbaos://127.0.0.1:{port}/{MOUNT}: \
                 transit/decrypt/cladefs-tenant-7: error sending request for url \
                 (https://127.0.0.1:{port}/v1/transit/decrypt/cladefs-tenant-7): client error \
                 (Connect): invalid peer certificate: UnknownIssuer (3 tries)",
                port = bao.server.port()
            )
        );
    }

    /// TASK-411.3's `CLADEFS_KMS` names it, its token in the environment: `kms::from_url`.
    #[test]
    fn kms_urls_name_openbao_its_token_in_the_environment() {
        let Some(bao) = bao(&[7]) else { return };
        let url = bao.server.url(MOUNT, QUERY);
        // SAFETY: nextest runs each test in a process of its own, this thread its only one.
        unsafe { std::env::remove_var(TOKEN_FILE_ENV) };
        unsafe { std::env::remove_var(TOKEN_ENV) };
        assert_eq!(
            crate::kms::from_url(&url).map(|_| ()).unwrap_err(),
            format!(
                "{url:?}: OpenBao's token is in neither CLADEFS_KMS_OPENBAO_TOKEN nor \
                 CLADEFS_KMS_OPENBAO_TOKEN_FILE"
            )
        );
        unsafe { std::env::set_var(TOKEN_ENV, &bao.token) };
        let shared = crate::kms::from_url(&url).unwrap();
        let key = Secret::random();
        let wrapped = block_on(shared.wrap_dyn(7, &key)).unwrap();
        assert_eq!(block_on(shared.unwrap_dyn(7, &wrapped)).unwrap().expose(), key.expose());
    }
}
