// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: AWS KMS as a [`super::Kms`] (TASK-411.17, backlog doc-16, Real KMSes: AWS KMS):
//! [`AwsKms`], `aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]`.

use super::background::Background;
use super::{Error, KeyNames, Kms, Origin, Vars};
use crate::{KEY_LEN, Secret, Unauthentic, Wrapped};
use aws_sdk_kms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_kms::primitives::Blob;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use zeroize::{Zeroize as _, Zeroizing};

/// The [`AwsKms`] `url` names ([`super::from_url_with`]), of `origin`, its credentials as `vars`
/// give them ([`Credentials::of`]).
pub(super) fn from_url(url: &str, vars: &Vars, origin: Origin) -> Result<super::Shared, String> {
    let credentials = Credentials::of(vars).map_err(|why| format!("{url:?}: {why}"))?;
    Ok(Arc::new(AwsKms::of(url, credentials, origin)?))
}

/// The variables its credentials are read from ([`Credentials::of`]), as AWS's SDKs read them: a
/// bucket's own replace the host's as one (board 8485, `super::vars`).
pub(super) const CREDENTIAL_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
];

/// The credentials an [`AwsKms`] calls with (board 8485).
enum Credentials {
    /// The standard chain's, as every AWS SDK finds them in the process's environment, its files,
    /// its container's or its instance's.
    Chain,
    /// A key's, and its session's token if any.
    Key { id: String, secret: Zeroizing<String>, session: Option<Zeroizing<String>> },
    /// A role's, assumed with a web identity's token in a file (EKS).
    WebIdentity { file: String, role: String, session: Option<String> },
    /// A profile's, of the shared files: the process's (`~/.aws/config`, `~/.aws/credentials`), or
    /// those named.
    Profile { name: String, config: Option<String>, credentials: Option<String> },
}

impl Credentials {
    /// The credentials `vars` give. The process's own, and a bucket's taking the host's, are the
    /// standard chain's, which reads them the same. A bucket's own ([`Vars::numbered`]) are read
    /// from them alone, never the process's environment, which the chain would read first: a key's (`AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`), a web identity's (`AWS_WEB_IDENTITY_TOKEN_FILE`,
    /// `AWS_ROLE_ARN`, `AWS_ROLE_SESSION_NAME`) or a profile's (`AWS_PROFILE`, `AWS_CONFIG_FILE`,
    /// `AWS_SHARED_CREDENTIALS_FILE`); none of them, the chain's, its container's or instance's. A
    /// container's credentials named, or a key's half, are refused, never the chain taken instead.
    fn of(vars: &Vars) -> Result<Self, String> {
        let get = |name| vars.get(name).map(str::to_owned);
        // The process's, as a bucket that takes the host's has them too, which the chain reads the
        // same: a container's among them (hotbed review of board 8485).
        if vars.reads_process(CREDENTIAL_VARS) {
            return Ok(Self::Chain);
        }
        if !vars.any(CREDENTIAL_VARS) {
            return match vars.owns(CREDENTIAL_VARS) {
                // Its own set empty: the chain would find the host's, which it set them to forgo.
                true => Err("a bucket's own AWS credentials are set, and empty: a key's, a web \
                             identity's or a profile's"
                    .to_owned()),
                // None given, none the process's: the instance's, or a container's, as the chain
                // finds them.
                false => Ok(Self::Chain),
            };
        }
        // The container's, the last of them.
        let container = &CREDENTIAL_VARS[CREDENTIAL_VARS.len() - 4..];
        if let Some(name) = container.iter().find(|name| vars.get(name).is_some()) {
            return Err(format!(
                "{name}: a container's credentials are the process's alone; a bucket's own are a \
                 key's, a web identity's or a profile's"
            ));
        }
        match (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY")) {
            (Some(id), Some(secret)) => Ok(Self::Key {
                id,
                secret: Zeroizing::new(secret),
                session: get("AWS_SESSION_TOKEN").map(Zeroizing::new),
            }),
            (None, None) => match (get("AWS_WEB_IDENTITY_TOKEN_FILE"), get("AWS_ROLE_ARN")) {
                (Some(file), Some(role)) => {
                    Ok(Self::WebIdentity { file, role, session: get("AWS_ROLE_SESSION_NAME") })
                }
                (None, None) => match get("AWS_PROFILE") {
                    Some(name) => Ok(Self::Profile {
                        name,
                        config: get("AWS_CONFIG_FILE"),
                        credentials: get("AWS_SHARED_CREDENTIALS_FILE"),
                    }),
                    None => Err("AWS_* names no credentials: a key's (AWS_ACCESS_KEY_ID, \
                                 AWS_SECRET_ACCESS_KEY), a web identity's \
                                 (AWS_WEB_IDENTITY_TOKEN_FILE, AWS_ROLE_ARN) or a profile's \
                                 (AWS_PROFILE)"
                        .to_owned()),
                },
                _ => Err("AWS_WEB_IDENTITY_TOKEN_FILE and AWS_ROLE_ARN go together".to_owned()),
            },
            _ => Err("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY go together".to_owned()),
        }
    }

    /// The provider of them, for a client in `region`; none for the chain's, which the SDK's
    /// defaults find. The SDK's copies of a key, and its own providers', are its own, which nothing
    /// here zeroes (board 8589).
    fn provider(&self, region: &str) -> Option<aws_sdk_kms::config::SharedCredentialsProvider> {
        use aws_config::provider_config::ProviderConfig;
        use aws_sdk_kms::config::SharedCredentialsProvider as Shared;
        let configured = || {
            ProviderConfig::without_region()
                .with_region(Some(aws_config::Region::new(region.to_owned())))
        };
        Some(match self {
            Self::Chain => return None,
            Self::Key { id, secret, session } => {
                Shared::new(aws_sdk_kms::config::Credentials::new(
                    id,
                    secret.as_str(),
                    session.as_ref().map(|token| token.to_string()),
                    None,
                    "cladefs: a bucket's own key",
                ))
            }
            Self::WebIdentity { file, role, session } => {
                use aws_config::web_identity_token::{
                    StaticConfiguration, WebIdentityTokenCredentialsProvider,
                };
                let session = session.clone().unwrap_or_else(|| "cladefs".to_owned());
                let configuration = StaticConfiguration {
                    web_identity_token_file: file.into(),
                    role_arn: role.clone(),
                    session_name: session,
                };
                Shared::new(
                    WebIdentityTokenCredentialsProvider::builder()
                        .configure(&configured())
                        .static_configuration(configuration)
                        .build(),
                )
            }
            Self::Profile { name, config, credentials } => {
                // aws-config names the files' kinds by these aliases alone, deprecated for
                // aws-runtime's, a crate cladefs does not depend on.
                #[allow(deprecated)]
                let files = {
                    use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
                    let mut files = ProfileFiles::builder();
                    files = match config {
                        Some(path) => files.with_file(ProfileFileKind::Config, path),
                        None => files.include_default_config_file(true),
                    };
                    files = match credentials {
                        Some(path) => files.with_file(ProfileFileKind::Credentials, path),
                        None => files.include_default_credentials_file(true),
                    };
                    files.build()
                };
                Shared::new(
                    aws_config::profile::ProfileFileCredentialsProvider::builder()
                        .configure(&configured())
                        .profile_name(name)
                        .profile_files(files)
                        .build(),
                )
            }
        })
    }
}

/// The start of what [`AwsKms`] wraps: cladefs's key, AWS's; [`LocalKms`](super::LocalKms)'s is
/// `fxkw`, so that neither's is sent to the other.
const MAGIC: &[u8; 4] = b"fxka";

/// The format of what [`AwsKms`] wraps.
const FORMAT: u8 = 1;

/// The bytes of a wrapped key's header before its key's name: [`MAGIC`], [`FORMAT`] and the
/// name's length (big-endian).
const FIXED_LEN: usize = MAGIC.len() + 1 + 2;

/// The encryption context's key whose value is the tenant, in base 32: what binds a wrapped
/// key to its tenant, in the KMS, and what a key policy's condition names
/// (`kms:EncryptionContext:cladefs-tenant`).
const CONTEXT_TENANT: &str = "cladefs-tenant";

/// What the query of an `aws-kms://` URL may name.
const QUERY: [&str; 3] = ["key", "keys", "endpoint"];

/// How long one attempt of a call to the KMS may take, and the call, its retries all told: a cell
/// opening waits on it, so that a KMS that does not answer fails it, as [`Error::Io`], rather than
/// hold it for good. The SDK retries what AWS says is worth retrying (throttling, 5xx, a lost
/// connection), three attempts, backing off.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// AWS KMS, in a region, holding each tenant's KEK as a symmetric KMS key named as [`KeyNames`]
/// says: a key ID, a key ARN, an alias name (`alias/...`) or an alias ARN; a key in another
/// account by its ARN, BYOK, as a tenant's override. Its credentials are the standard chain's, as
/// every AWS SDK finds them: the environment's (`AWS_ACCESS_KEY_ID`...), the shared profile's
/// (`AWS_PROFILE`, `~/.aws/config` and `credentials`, SSO, `credential_process`), web identity's
/// (EKS), the container's (ECS) or the instance's (IMDSv2). It never logs them, nor a key.
///
/// A wrap is KMS's `Encrypt` of the key's 32 bytes under the tenant's key, with the encryption
/// context `{"cladefs-tenant": TENANT}` (the tenant in base 32, as [`KeyNames`] names it): so that
/// the ciphertext decrypts for no other tenant even under the same key, and a key policy can let
/// cladefs use a key for its tenant alone. What it wraps is a header, [`MAGIC`], [`FORMAT`] (1),
/// the key's name's length (2 bytes, big-endian) and name, then KMS's ciphertext blob, which names
/// the key's version itself: so a rotated key unwraps what any of its versions wrapped. An unwrap
/// refuses, as [`Error::Unauthentic`] and asking the KMS nothing, a wrapped key whose header is not
/// its own (another backend's) or names a key other than the tenant's (another tenant's, or the
/// tenant's under a name since changed); then it is KMS's `Decrypt`, naming the key, so that a
/// ciphertext of another key is refused, and with the same context.
///
/// The KMS's refusals are [`Error`]s naming the tenant ([`refusal_of`]):
///
/// | AWS KMS's error | [`Error`] |
/// |---|---|
/// | `NotFoundException` | [`Error::NoSuchTenant`] |
/// | `AccessDeniedException`, `DisabledException` | [`Error::Revoked`] |
/// | `KMSInvalidStateException`, pending deletion | [`Error::Destroyed`] |
/// | `KMSInvalidStateException`, any other state (pending import, unavailable...) | [`Error::Revoked`] |
/// | `InvalidCiphertextException`, `IncorrectKeyException` | [`Error::Unauthentic`] |
/// | any other: throttling, `KeyUnavailableException`, `KMSInternalException`, 5xx, the network, a timeout, no credentials | [`Error::Io`], naming the call and key |
///
/// A key deleted once its waiting period is over is gone from AWS, which answers
/// `NotFoundException` for it as for a key never made: the tenant reads as one with no KEK
/// ([`Error::NoSuchTenant`]), not as destroyed (doc-16, Real KMSes: AWS KMS).
///
/// Its calls run on a runtime of its own ([`Background`]), its HTTPS client made there as first
/// needed: so its futures run on any executor, and the client's connections are never another
/// runtime's, gone when it is.
pub struct AwsKms {
    names: KeyNames,
    client: Arc<Client>,
    runtime: Background,
}

/// The client of an [`AwsKms`], made as first needed, on its runtime.
struct Client {
    region: String,
    endpoint: Option<String>,
    credentials: Credentials,
    made: tokio::sync::OnceCell<aws_sdk_kms::Client>,
}

impl Client {
    async fn get(&self) -> &aws_sdk_kms::Client {
        self.made
            .get_or_init(|| async {
                let timeouts = aws_config::timeout::TimeoutConfig::builder()
                    .operation_attempt_timeout(ATTEMPT_TIMEOUT)
                    .operation_timeout(CALL_TIMEOUT)
                    .build();
                let mut config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_config::Region::new(self.region.clone()))
                    .timeout_config(timeouts);
                if let Some(provider) = self.credentials.provider(&self.region) {
                    config = config.credentials_provider(provider);
                }
                let config = config.load().await;
                let mut config = aws_sdk_kms::config::Builder::from(&config);
                if let Some(endpoint) = &self.endpoint {
                    config = config.endpoint_url(endpoint);
                }
                aws_sdk_kms::Client::from_conf(config.build())
            })
            .await
    }
}

impl AwsKms {
    /// The KMS `url` names: `aws-kms://REGION?key=TEMPLATE[&keys=FILE][&endpoint=URL]`, its query
    /// percent-encoded as any URL's; `endpoint` for a VPC endpoint, or an emulator. It calls with
    /// `credentials`; `url` is of `origin`: one the registry of buckets records names no file of
    /// the host's, and an endpoint of AWS's own over HTTPS, or of this host, alone (board 8525).
    fn of(url: &str, credentials: Credentials, origin: Origin) -> Result<Self, String> {
        let parsed = url::Url::parse(url).map_err(|err| format!("{url:?}: {err}"))?;
        let refused = |why: &str| format!("{url:?} names no AWS KMS: {why}");
        if parsed.scheme() != "aws-kms" {
            return Err(refused("its scheme is not aws-kms"));
        }
        let region = parsed.host_str().unwrap_or_default();
        let is_region = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if region.is_empty() || !region.chars().all(is_region) {
            return Err(refused("aws-kms://REGION?key=TEMPLATE names a region, as us-east-1"));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || !matches!(parsed.path(), "" | "/")
            || parsed.fragment().is_some()
        {
            return Err(refused(
                "aws-kms://REGION?key=TEMPLATE names a region alone, then a query",
            ));
        }
        let query: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        if let Some((name, _)) = query.iter().find(|(name, _)| !QUERY.contains(&name.as_str())) {
            return Err(refused(&format!("its query names {name:?}: key, keys or endpoint")));
        }
        let endpoint = query.iter().find(|(name, _)| name == "endpoint").map(|(_, v)| v.clone());
        if let Some(endpoint) = &endpoint {
            origin.aws_endpoint(endpoint, region).map_err(|why| refused(&why))?;
        }
        let names = KeyNames::of_query(query.iter().map(|(n, v)| (n.as_str(), v.as_str())), origin)
            .map_err(|why| refused(&why))?;
        Ok(Self::with(region, endpoint, names, credentials))
    }

    /// The KMS in `region`, at `endpoint` if given, else AWS's own, naming tenants' keys by
    /// `names`, its credentials the standard chain's.
    #[cfg(test)]
    fn new(region: &str, endpoint: Option<String>, names: KeyNames) -> Self {
        Self::with(region, endpoint, names, Credentials::Chain)
    }

    fn with(
        region: &str,
        endpoint: Option<String>,
        names: KeyNames,
        credentials: Credentials,
    ) -> Self {
        let runtime = Background::new("cladefs-aws-kms").expect("a runtime for AWS KMS's calls");
        let made = tokio::sync::OnceCell::new();
        let region = region.to_owned();
        let client = Arc::new(Client { region, endpoint, credentials, made });
        Self { names, client, runtime }
    }

    /// Runs `call` on its runtime, given its client ([`Background::run`]).
    async fn run<T, F>(&self, tenant: u64, call: impl FnOnce(Arc<Client>) -> F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
    {
        self.runtime.run(tenant, call(self.client.clone())).await
    }
}

impl Kms for AwsKms {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        let name = self.names.of(tenant);
        let key = Secret::from_bytes(key.expose());
        async move {
            let header = header(tenant, &name)?;
            self.run(tenant, |client| async move {
                // The SDK's copies of the key, this and its request's body, are not zeroed.
                let encrypted = (client.get().await)
                    .encrypt()
                    .key_id(&name)
                    .plaintext(Blob::new(key.expose().to_vec()))
                    .encryption_context(CONTEXT_TENANT, base32_id::Short(tenant).to_string())
                    .send()
                    .await
                    .map_err(|err| refusal_of(tenant, "Encrypt", &name, err))?;
                let blob = encrypted.ciphertext_blob.ok_or_else(|| {
                    let why = format!("AWS KMS's Encrypt under {name}: no ciphertext");
                    Error::Io { tenant, source: io::Error::new(io::ErrorKind::InvalidData, why) }
                })?;
                Ok(Wrapped([&header[..], blob.as_ref()].concat()))
            })
            .await
        }
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        let name = self.names.of(tenant);
        let unauthentic = move |why| Error::Unauthentic { tenant, source: Unauthentic(why) };
        let blob = key_name_of(wrapped).map_err(|Unauthentic(why)| unauthentic(why)).and_then(
            |(named, blob)| match named == name {
                true => Ok(blob.to_vec()),
                false => Err(unauthentic("a wrapped key, under another key than the tenant's")),
            },
        );
        async move {
            let blob = blob?;
            self.run(tenant, |client| async move {
                let decrypted = (client.get().await)
                    .decrypt()
                    .key_id(&name)
                    .ciphertext_blob(Blob::new(blob))
                    .encryption_context(CONTEXT_TENANT, base32_id::Short(tenant).to_string())
                    .send()
                    .await
                    .map_err(|err| refusal_of(tenant, "Decrypt", &name, err))?;
                let mut plaintext = decrypted.plaintext.map(Blob::into_inner).unwrap_or_default();
                let key = <&[u8; KEY_LEN]>::try_from(&plaintext[..]).map(Secret::from_bytes);
                plaintext.zeroize();
                key.map_err(|_| unauthentic("a wrapped key, of the wrong length"))
            })
            .await
        }
    }
}

/// The header of a key wrapped for `tenant` under the key `name` (see [`AwsKms`]).
fn header(tenant: u64, name: &str) -> Result<Vec<u8>, Error> {
    let len = u16::try_from(name.len()).map_err(|_| {
        let why = format!("a key name of {} bytes, more than 65535: {name:?}", name.len());
        Error::Io { tenant, source: io::Error::new(io::ErrorKind::InvalidInput, why) }
    })?;
    let mut header = Vec::with_capacity(FIXED_LEN + name.len());
    header.extend_from_slice(MAGIC);
    header.push(FORMAT);
    header.extend_from_slice(&len.to_be_bytes());
    header.extend_from_slice(name.as_bytes());
    Ok(header)
}

/// The name of the key `wrapped` says wrapped it, and KMS's ciphertext blob, as its header says:
/// whether it is authentic only the KMS tells.
fn key_name_of(wrapped: &Wrapped) -> Result<(&str, &[u8]), Unauthentic> {
    let bytes = &wrapped.0;
    if bytes.len() < FIXED_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(Unauthentic("a wrapped key, by its header"));
    }
    if bytes[MAGIC.len()] != FORMAT {
        return Err(Unauthentic("a wrapped key, of another format"));
    }
    let len = u16::from_be_bytes([bytes[MAGIC.len() + 1], bytes[MAGIC.len() + 2]]);
    let (name, blob) = bytes[FIXED_LEN..]
        .split_at_checked(len.into())
        .ok_or(Unauthentic("a wrapped key, by its header"))?;
    let name =
        std::str::from_utf8(name).map_err(|_| Unauthentic("a wrapped key, by its header"))?;
    if blob.is_empty() {
        return Err(Unauthentic("a wrapped key, by its header"));
    }
    Ok((name, blob))
}

/// What AWS KMS's refusal, the error `code` with `message`, is, as [`AwsKms`]'s table says;
/// [`None`] for what fails as [`Error::Io`].
fn refusal(tenant: u64, code: Option<&str>, message: Option<&str>) -> Option<Error> {
    Some(match code? {
        "NotFoundException" => Error::NoSuchTenant { tenant },
        "AccessDeniedException" | "DisabledException" => Error::Revoked { tenant },
        "KMSInvalidStateException" => {
            // AWS says which state in its message alone: "<key ARN> is pending deletion."
            let message = message.unwrap_or_default().to_ascii_lowercase();
            match message.contains("pending") && message.contains("deletion") {
                true => Error::Destroyed { tenant },
                false => Error::Revoked { tenant },
            }
        }
        "InvalidCiphertextException" | "IncorrectKeyException" => Error::Unauthentic {
            tenant,
            source: Unauthentic("a wrapped key, as AWS KMS decrypts it"),
        },
        _ => return None,
    })
}

/// The [`Error`] AWS KMS's call `call` under the key `name` failing with `err` is: a refusal as
/// [`refusal`] says, else [`Error::Io`], naming the call, the key and what AWS or the SDK said,
/// which holds no credential nor key.
fn refusal_of<E, R>(tenant: u64, call: &str, name: &str, err: SdkError<E, R>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    if let Some(refused) = refusal(tenant, err.code(), err.message()) {
        return refused;
    }
    let kind = match &err {
        SdkError::TimeoutError(_) => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    let why = match err.code() {
        Some(code) => format!("{code}: {}", err.message().unwrap_or_default()),
        None => {
            // What failed, and why, each cause in turn: no SDK's Debug, which says more than
            // needed.
            let mut why = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                why = format!("{why}: {cause}");
                source = cause.source();
            }
            why
        }
    };
    Error::Io {
        tenant,
        source: io::Error::new(kind, format!("AWS KMS's {call} under {name}: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::LocalAwsKms;
    use assert_matches::assert_matches;
    use futures::executor::block_on;

    fn wrap(kms: &impl Kms, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        block_on(kms.wrap(tenant, key))
    }

    fn unwrap(kms: &impl Kms, tenant: u64, wrapped: &Wrapped) -> Result<Secret, Error> {
        block_on(kms.unwrap(tenant, wrapped))
    }

    /// The error's text: each names its tenant, which is what callers report.
    fn refusal_text<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        result.unwrap_err().to_string()
    }

    fn names(template: &str) -> KeyNames {
        KeyNames::new(template, None).unwrap()
    }

    /// Keys wrapped by another backend, or under another tenant's key, are refused as not
    /// authentic before any call: this KMS's endpoint answers nothing.
    #[test]
    fn wrapped_keys_not_this_kms_tenants_are_refused_unasked() {
        let kms = AwsKms::new("us-east-1", Some("http://127.0.0.1:9".into()), names("k-{tenant}"));
        let dir = tempfile::tempdir().unwrap();
        let local = crate::kms::LocalKms::open(dir.path()).unwrap();
        local.create(7).unwrap();
        let foreign = wrap(&local, 7, &Secret::random()).unwrap();
        assert_eq!(
            refusal_text(unwrap(&kms, 7, &foreign)),
            "tenant 7: not authentic: a wrapped key, by its header"
        );
        let others = Wrapped([&header(8, "k-8").unwrap()[..], b"blob"].concat());
        assert_matches!(
            unwrap(&kms, 7, &others),
            Err(Error::Unauthentic { tenant: 7, source: Unauthentic(why) })
                if why == "a wrapped key, under another key than the tenant's"
        );
    }

    /// hotbed review of board 8485: a bucket taking the host's credentials takes the standard
    /// chain, as the host does, a container's among them, which a bucket's own may not name; one
    /// with its own takes them alone; one setting its own empty, or half a key, is refused, never
    /// given the chain, which would find the host's it set them to forgo.
    #[test]
    fn buckets_take_the_hosts_chain_or_credentials_of_their_own() {
        let host = Vars::as_process(
            [
                ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/credentials"),
                ("AWS_ACCESS_KEY_ID", "host-id"),
                ("CLADEFSD_BUCKET_2_AWS_ACCESS_KEY_ID", "two-id"),
                ("CLADEFSD_BUCKET_2_AWS_SECRET_ACCESS_KEY", "two-secret"),
                ("CLADEFSD_BUCKET_3_AWS_ACCESS_KEY_ID", ""),
                ("CLADEFSD_BUCKET_4_AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://169.254.170.2/x"),
                ("CLADEFSD_BUCKET_5_AWS_ACCESS_KEY_ID", "five-id"),
                ("CLADEFSD_BUCKET_6_AWS_PROFILE", "six"),
                ("CLADEFSD_BUCKET_6_AWS_CONFIG_FILE", "/six/config"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
        );
        let of = |number| Credentials::of(&host.numbered(number));
        assert!(matches!(Credentials::of(&host), Ok(Credentials::Chain)));
        assert!(matches!(of(1), Ok(Credentials::Chain)));
        assert!(matches!(of(2), Ok(Credentials::Key { id, session: None, .. }) if id == "two-id"));
        let refused = |number| of(number).err().unwrap();
        assert!(refused(3).starts_with("a bucket's own AWS credentials are set, and empty"));
        assert!(refused(4).starts_with("AWS_CONTAINER_CREDENTIALS_FULL_URI: a container's"));
        assert_eq!(refused(5), "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY go together");
        assert!(matches!(
            of(6),
            Ok(Credentials::Profile { name, config: Some(config), credentials: None })
                if name == "six" && config == "/six/config"
        ));
    }

    // The tests against local-kms, AWS KMS's emulator, which scripts/test-local-kms.sh runs: each
    // starts one of its own.

    /// A tenant of the tests', 31: its key's name is in base 32, `z`.
    const TENANT: u64 = 31;

    /// The template the tests name tenants' keys by.
    const TEMPLATE: &str = "alias/cladefs-tenant-{tenant}";

    /// The emulator, with a key for [`TENANT`] named by [`TEMPLATE`], its ID; and the KMS over it,
    /// as `CLADEFS_KMS` names one.
    fn emulated() -> (LocalAwsKms, String, crate::kms::Shared) {
        let emulator = LocalAwsKms::start();
        let key_id = emulator.create_key();
        emulator.create_alias("alias/cladefs-tenant-z", &key_id);
        let kms = crate::kms::from_url(&emulator.url(TEMPLATE)).unwrap();
        (emulator, key_id, kms)
    }

    fn wrap_dyn(kms: &crate::kms::Shared, tenant: u64, key: &Secret) -> Result<Wrapped, Error> {
        block_on(kms.wrap_dyn(tenant, key))
    }

    fn unwrap_dyn(
        kms: &crate::kms::Shared,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> Result<Secret, Error> {
        block_on(kms.unwrap_dyn(tenant, wrapped))
    }

    /// Tenants sharing a key, as overrides may name one for both, unwrap none of each other's:
    /// the encryption context binds each wrapped key to its tenant. A ciphertext of another key,
    /// or tampered with, is refused too, by the KMS: each not authentic.
    #[test]
    #[ignore = "needs local-kms: scripts/test-local-kms.sh, CI's object-stores job"]
    fn local_kms_wrapped_keys_of_another_tenant_context_or_key_are_refused() {
        let (emulator, key_id, kms) = emulated();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keys");
        std::fs::write(&file, format!("1 {key_id}\n2 {key_id}\n")).unwrap();
        let url = format!("{}&keys={}", emulator.url(TEMPLATE), file.display());
        let shared = crate::kms::from_url(&url).unwrap();
        let wrapped = wrap_dyn(&shared, 1, &Secret::random()).unwrap();
        let refused = unwrap_dyn(&shared, 2, &wrapped);
        assert_matches!(
            refused,
            Err(Error::Unauthentic { tenant: 2, source: Unauthentic(why) })
                if why == "a wrapped key, as AWS KMS decrypts it"
        );
        // Tenant 1's under its own name, the template's, the header refuses, unasked.
        assert_eq!(
            refusal_text(unwrap_dyn(&kms, 1, &wrapped)),
            "tenant 1: not authentic: a wrapped key, under another key than the tenant's"
        );

        // A ciphertext of another key, for the tenant, its context the tenant's, as one who holds
        // that key could make (its owner encrypts what it likes, with any context), relabelled as
        // under the tenant's key: KMS decrypts under the key named alone, and refuses it.
        let other = emulator.create_key();
        std::fs::write(&file, format!("z {other}\n")).unwrap();
        let elsewhere = crate::kms::from_url(&url).unwrap();
        let others = wrap_dyn(&elsewhere, TENANT, &Secret::random()).unwrap();
        assert_eq!(key_name_of(&others).unwrap().0, other);
        let blob = key_name_of(&others).unwrap().1;
        let header = header(TENANT, "alias/cladefs-tenant-z").unwrap();
        let relabelled = Wrapped([&header[..], blob].concat());
        assert_eq!(
            refusal_text(unwrap_dyn(&kms, TENANT, &relabelled)),
            "tenant 31: not authentic: a wrapped key, as AWS KMS decrypts it"
        );

        // Tampered with, at its blob's end, in its tag.
        let mut tampered = wrap_dyn(&kms, TENANT, &Secret::random()).unwrap();
        *tampered.0.last_mut().unwrap() ^= 1;
        assert_eq!(
            refusal_text(unwrap_dyn(&kms, TENANT, &tampered)),
            "tenant 31: not authentic: a wrapped key, as AWS KMS decrypts it"
        );
    }

    /// A rotated key unwraps what its older versions wrapped and wraps under its newest. The
    /// emulator rotates no key on demand, nor before a year: two emulators stand for the key
    /// before and after its rotation, seeded with the same key ID, the second with a second
    /// version (backing key), as local-kms's seeding has rotations.
    #[test]
    #[ignore = "needs local-kms: scripts/test-local-kms.sh, CI's object-stores job"]
    fn local_kms_rotated_keys_unwrap_what_their_old_versions_wrapped() {
        let id = "8a2f6c5e-0d6b-4b8e-9d1c-5a3e7f0b2c41";
        let first = "1111111111111111111111111111111111111111111111111111111111111111";
        let second = "2222222222222222222222222222222222222222222222222222222222222222";
        let seed = |backing: &[&str]| {
            let backing: String =
                backing.iter().map(|key| format!("\n          - {key}")).collect();
            format!(
                "Keys:\n  Symmetric:\n    Aes:\n      - Metadata:\n          KeyId: {id}\n        \
                 BackingKeys:{backing}\nAliases:\n  - AliasName: alias/cladefs-tenant-z\n    \
                 TargetKeyId: {id}\n"
            )
        };
        let before = LocalAwsKms::seeded(&seed(&[first]));
        let kms = crate::kms::from_url(&before.url(TEMPLATE)).unwrap();
        let old_key = Secret::random();
        let old = wrap_dyn(&kms, TENANT, &old_key).unwrap();
        drop((kms, before));

        let after = LocalAwsKms::seeded(&seed(&[first, second]));
        let kms = crate::kms::from_url(&after.url(TEMPLATE)).unwrap();
        let new_key = Secret::random();
        let new = wrap_dyn(&kms, TENANT, &new_key).unwrap();
        assert_eq!(unwrap_dyn(&kms, TENANT, &old).unwrap().expose(), old_key.expose());
        assert_eq!(unwrap_dyn(&kms, TENANT, &new).unwrap().expose(), new_key.expose());
        // The new wrap is under the second version: an emulator of the key before its rotation
        // has it not.
        let before = LocalAwsKms::seeded(&seed(&[first]));
        let kms = crate::kms::from_url(&before.url(TEMPLATE)).unwrap();
        assert_eq!(unwrap_dyn(&kms, TENANT, &old).unwrap().expose(), old_key.expose());
        assert_matches!(
            unwrap_dyn(&kms, TENANT, &new),
            Err(Error::Unauthentic { tenant: TENANT, .. })
        );
    }

    /// A KMS that answers not is an I/O error naming the call and key, after the SDK's retries:
    /// here, nothing listens at its endpoint any more.
    #[test]
    #[ignore = "needs local-kms: scripts/test-local-kms.sh, CI's object-stores job"]
    fn local_kms_unreachable_kmses_fail_as_io() {
        let (emulator, _, kms) = emulated();
        let wrapped = wrap_dyn(&kms, TENANT, &Secret::random()).unwrap();
        let url = emulator.url(TEMPLATE);
        drop(emulator);
        let kms = crate::kms::from_url(&url).unwrap();
        let refused = unwrap_dyn(&kms, TENANT, &wrapped);
        assert_matches!(refused, Err(Error::Io { tenant: TENANT, .. }));
        // But for the OS's errno.
        let refused = |text: String| text.replace("os error 111", "os error 61");
        let why = "dispatch failure: io error: client error (Connect): tcp connect error: \
                   Connection refused (os error 61)";
        assert_eq!(
            refused(refusal_text(unwrap_dyn(&kms, TENANT, &wrapped))),
            format!(
                "tenant 31: the KMS failed: AWS KMS's Decrypt under alias/cladefs-tenant-z: {why}"
            )
        );
        assert_eq!(
            refused(refusal_text(wrap_dyn(&kms, TENANT, &Secret::random()))),
            format!(
                "tenant 31: the KMS failed: AWS KMS's Encrypt under alias/cladefs-tenant-z: {why}"
            )
        );
    }
}
