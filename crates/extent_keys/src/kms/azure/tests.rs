// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Azure Key Vault backend against a fake of the vault and its authority, in this process:
//! what it sends, and what it makes of the answers. Its calls are polled by `block_on`, no runtime
//! at all: they run on the backend's own (board 8477).

use super::*;
use crate::kms::fake_http::{Endless, Request, Response, Server};
use futures::executor::block_on;

/// A tenant's key named whole names a vault: never another host, which its tokens would go to.
#[test]
fn overrides_naming_no_vault_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    std::fs::write(&keys, "7 https://evil.example/keys/x\n").unwrap();
    let url = format!("azure-kv://v.vault.azure.net?key=t-{{tenant}}&keys={}", keys.display());
    let refused = parse(&url, Origin::Host).unwrap_err();
    assert!(
        refused.starts_with(&format!(
            "{url:?}: tenant 7's key, \"https://evil.example/keys/x\", names no key: \"evil.example\" is no vault's host"
        )),
        "{refused}"
    );
}

/// A fake of a vault, its authority and the instance metadata service: tokens `token-N`, of
/// `expires_in` seconds, each fetch answered after `token_delay`; keys of version `v1`, wrapping by
/// reversing what they are given; a key named `revoked-...` refused (403), `firewalled-...` refused
/// by the vault's firewall (403), `missing-...` none (404); the first call of the vault refused as
/// unauthorized if `refuse_first`; the first `imds_starting` asks of the metadata service answered
/// 404, as it does before the identity is the machine's; the authority refusing the secret (400)
/// if `bad_secret`.
struct Fake {
    tokens: u64,
    expires_in: u64,
    token_delay: Duration,
    refuse_first: bool,
    imds_starting: u64,
    bad_secret: bool,
    /// Each request: its method and path.
    asked: Vec<String>,
    /// The scope of each token asked for.
    scopes: Vec<String>,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            tokens: 0,
            expires_in: 3600,
            token_delay: Duration::ZERO,
            refuse_first: false,
            imds_starting: 0,
            bad_secret: false,
            asked: Vec::new(),
            scopes: Vec::new(),
        }
    }
}

/// The fake's answer to `request`, served at `base`.
fn answer(fake: &Mutex<Fake>, base: &str, request: &Request) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.asked.push(format!("{} {}", request.method, request.path));
    let path = request.route();
    let token = |fake: &mut Fake| {
        fake.tokens += 1;
        let body = format!(
            r#"{{"token_type":"Bearer","expires_in":{},"access_token":"token-{}"}}"#,
            fake.expires_in, fake.tokens
        );
        Response::of(200, body).after(fake.token_delay)
    };
    if path.ends_with("/oauth2/v2.0/token") {
        let form: Vec<(String, String)> =
            url::form_urlencoded::parse(&request.body).into_owned().collect();
        let has = |name: &str, value: &str| form.iter().any(|(n, v)| n == name && v == value);
        let scope = form.iter().find(|(n, _)| n == "scope").map(|(_, v)| v.clone()).unwrap();
        fake.scopes.push(scope);
        assert!(has("client_secret", "s") && has("client_id", "c"), "{form:?}");
        if fake.bad_secret {
            let body = r#"{"error":"invalid_client","error_description":"AADSTS7000215: Invalid client secret provided."}"#;
            return Response::of(400, body);
        }
        return token(&mut fake);
    }
    if path == "/metadata/identity/oauth2/token" {
        assert_eq!(request.header("metadata"), Some("true"));
        if fake.imds_starting > 0 {
            fake.imds_starting -= 1;
            let body = r#"{"error":"invalid_request","error_description":"Identity not found"}"#;
            return Response::of(404, body);
        }
        return token(&mut fake);
    }
    let bearer = request.header("authorization").and_then(|value| value.strip_prefix("Bearer "));
    if bearer.is_none_or(|token| !token.starts_with("token-"))
        || std::mem::take(&mut fake.refuse_first)
    {
        return Response::of(401, r#"{"error":{"code":"Unauthorized","message":"no token"}}"#);
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let (name, version, op) = match segments[..] {
        ["keys", name, "wrapkey"] => (name, "v1", "wrapkey"),
        ["keys", name, version, "unwrapkey"] => (name, version, "unwrapkey"),
        _ => return Response::of(404, r#"{"error":{"code":"NotFound","message":"no such path"}}"#),
    };
    if name.starts_with("revoked-") {
        return Response::of(403, r#"{"error":{"code":"Forbidden","message":"not allowed"}}"#);
    }
    if name.starts_with("firewalled-") {
        let body = r#"{"error":{"code":"Forbidden","message":"Client address is not authorized and caller is not a trusted service.","innererror":{"code":"ForbiddenByFirewall"}}}"#;
        return Response::of(403, body);
    }
    if name.starts_with("missing-") {
        return Response::of(404, r#"{"error":{"code":"KeyNotFound","message":"no key"}}"#);
    }
    let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(request["alg"], "RSA-OAEP-256");
    let mut value = decode(request["value"].as_str().unwrap()).unwrap();
    if op == "unwrapkey" && (version != "v1" || value.len() != 40) {
        let body = r#"{"error":{"code":"BadParameter","message":"Error decrypting."}}"#;
        return Response::of(400, body);
    }
    value.reverse();
    let value = BASE64URL.encode(value);
    Response::of(200, format!(r#"{{"kid":"{base}/keys/{name}/{version}","value":"{value}"}}"#))
}

/// The fake, serving `fake`, its base URL that of [`Server::base`].
fn serve(fake: &Arc<Mutex<Fake>>) -> Server {
    let fake = fake.clone();
    Server::start(move |base, request| answer(&fake, base, request))
}

/// A backend over the fake at `base`, its keys named `prefix{tenant}`, overridden by `keys`.
fn over(base: &str, template: &str, keys: Option<&str>) -> AzureKms {
    over_for(base, template, keys, "https://vault.azure.net")
}

/// The same, its URL's vault's tokens for `audience`.
fn over_for(base: &str, template: &str, keys: Option<&str>, audience: &'static str) -> AzureKms {
    let source = Source::Secret {
        url: format!("{base}/t/oauth2/v2.0/token"),
        client_id: "c".into(),
        secret: Zeroizing::new("s".into()),
    };
    over_from(base, template, keys, audience, source)
}

/// The same, its tokens from `source`.
fn over_from(
    base: &str,
    template: &str,
    keys: Option<&str>,
    audience: &'static str,
    source: Source,
) -> AzureKms {
    let names = KeyNames::new(template, keys).unwrap();
    let parsed = Parsed { vault: base.to_owned(), audience, names, alg: ALGORITHMS[0] };
    AzureKms::new(parsed, source).unwrap()
}

/// A key wrapped unwraps for its tenant: one token fetched for both calls, the version that
/// wrapped it named in what is kept and asked for on the unwrap, the tenant bound in what the
/// vault wraps. A key wrapped for another tenant under the same key is refused as not authentic;
/// one kept under another key's name is refused asking the vault nothing.
#[test]
fn keys_wrapped_unwrap_for_their_tenant_alone() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = serve(&fake);
    let base = server.base();
    let kms = over(base, "t-{tenant}", Some("9 shared\na shared\n"));
    let key = Secret::random();
    let wrapped = block_on(kms.wrap(9, &key)).unwrap();
    let id = format!("{base}/keys/shared");
    let kept = real::ciphertext(&wrapped, SCHEME, &id).unwrap();
    assert_eq!((&kept[..3], kept.len()), (&b"\x02v1"[..], 3 + 40));
    assert_eq!(block_on(kms.unwrap(9, &wrapped)).unwrap().expose(), key.expose());
    assert_eq!(
        fake.lock().unwrap().asked,
        [
            "POST /t/oauth2/v2.0/token".to_owned(),
            "POST /keys/shared/wrapkey?api-version=7.4".to_owned(),
            "POST /keys/shared/v1/unwrapkey?api-version=7.4".to_owned(),
        ]
    );
    let refused = block_on(kms.unwrap(10, &wrapped)).unwrap_err().to_string();
    assert_eq!(refused, "tenant 10: not authentic: a wrapped key, wrapped for another tenant");
    let asked = fake.lock().unwrap().asked.len();
    let refused = block_on(kms.unwrap(11, &wrapped)).unwrap_err().to_string();
    assert_eq!(refused, "tenant 11: not authentic: a wrapped key, wrapped under another key");
    assert_eq!(fake.lock().unwrap().asked.len(), asked, "the vault asked of another key's");
    assert_eq!(fake.lock().unwrap().tokens, 1);
}

/// The vault refusing: a key disabled or not granted is revoked, one missing none; a token the
/// vault refuses is fetched again, once, and the call goes on. board 8481: the vault's firewall
/// refusing the host is the host's, not the tenant's: no revocation, the vault and why named.
#[test]
fn refusals_reach_their_tenants_and_tokens_are_fetched_again() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = serve(&fake);
    let base = server.base();
    let revoked = over(base, "revoked-{tenant}", None);
    let refused = block_on(revoked.wrap(9, &Secret::random())).unwrap_err().to_string();
    assert_eq!(refused, "tenant 9: its KEK is revoked");
    let missing = over(base, "missing-{tenant}", None);
    let refused = block_on(missing.wrap(9, &Secret::random())).unwrap_err().to_string();
    assert_eq!(refused, "tenant 9: no KEK in the KMS");
    let firewalled = over(base, "firewalled-{tenant}", None);
    let refused = block_on(firewalled.wrap(9, &Secret::random())).unwrap_err();
    assert!(matches!(refused, Error::Io { tenant: 9, .. }), "{refused}");
    assert_eq!(
        refused.to_string(),
        format!(
            "tenant 9: the KMS failed: Azure Key Vault, wrapkey with {base}/keys/firewalled-9: \
             HTTP 403, ForbiddenByFirewall: the vault refuses this host's network, not the \
             tenant's key: Client address is not authorized and caller is not a trusted service."
        )
    );
    let kms = over(base, "t-{tenant}", None);
    fake.lock().unwrap().refuse_first = true;
    let tokens = fake.lock().unwrap().tokens;
    block_on(kms.wrap(9, &Secret::random())).unwrap();
    assert_eq!(fake.lock().unwrap().tokens, tokens + 2, "a token refused not fetched again");
}

/// board 8481: a 403 is revoked but for the vault's network refusing the host, by its code or its
/// inner error's, at any depth; a 403 of no body is revoked, as before.
#[test]
fn only_network_refusals_of_403_are_not_revocations() {
    let mapped = |body: &str| {
        refusal(9, "https://v.vault.azure.net/keys/k", Op::Unwrap, 403, body.as_bytes())
    };
    for body in [
        r#"{"error":{"code":"Forbidden","message":"m","innererror":{"code":"ForbiddenByPolicy"}}}"#,
        r#"{"error":{"code":"Forbidden","message":"m","innererror":{"code":"ForbiddenByRbac"}}}"#,
        r#"{"error":{"code":"Forbidden","message":"Operation unwrapKey is not allowed on a disabled key."}}"#,
        "",
    ] {
        let Failure { failed, unauthorized } = mapped(body);
        assert_eq!(failed.error.to_string(), "tenant 9: its KEK is revoked", "{body}");
        assert!(!failed.transient && !unauthorized);
    }
    for (body, code) in [
        (
            r#"{"error":{"code":"Forbidden","message":"m","innererror":{"code":"ForbiddenByFirewall"}}}"#,
            "ForbiddenByFirewall",
        ),
        (
            r#"{"error":{"code":"Forbidden","message":"m","innererror":{"code":"x","innererror":{"code":"ForbiddenByConnection"}}}}"#,
            "ForbiddenByConnection",
        ),
        (r#"{"error":{"code":"ForbiddenByFirewall","message":"m"}}"#, "ForbiddenByFirewall"),
    ] {
        let Failure { failed, .. } = mapped(body);
        assert!(matches!(failed.error, Error::Io { tenant: 9, .. }), "{body}");
        assert!(
            failed
                .error
                .to_string()
                .contains(&format!("HTTP 403, {code}: the vault refuses this host's network"))
        );
        assert!(!failed.transient);
    }
}

/// TASK-633.6 (review): each vault's tokens are for its own audience: a Managed HSM's, a sovereign
/// cloud's, the public vaults'; a key of an emulator on this host takes the URL's vault's.
#[test]
fn tokens_are_for_the_audience_of_each_vault() {
    assert_eq!(audience_of("v.vault.azure.net"), Some("https://vault.azure.net"));
    assert_eq!(audience_of("h.managedhsm.azure.net"), Some("https://managedhsm.azure.net"));
    assert_eq!(audience_of("v.vault.azure.cn"), Some("https://vault.azure.cn"));
    assert_eq!(
        audience_of("h.managedhsm.usgovcloudapi.net"),
        Some("https://managedhsm.usgovcloudapi.net")
    );
    assert_eq!(audience_of("evil.example"), None);
    let parsed =
        parse("azure-kv://h.managedhsm.azure.net?key=t-{tenant}&alg=A256KW", Origin::Host).unwrap();
    assert_eq!(parsed.audience, "https://managedhsm.azure.net");
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = serve(&fake);
    let base = server.base();
    let keys = "9 https://theirs.vault.azure.cn/keys/k\n";
    let kms = over_for(base, "t-{tenant}", Some(keys), "https://managedhsm.azure.net");
    let inner = &kms.inner;
    assert_eq!(inner.audience("https://theirs.vault.azure.cn/keys/k"), "https://vault.azure.cn");
    assert_eq!(inner.audience(&format!("{base}/keys/t-8")), "https://managedhsm.azure.net");
    block_on(kms.wrap(8, &Secret::random())).unwrap();
    assert_eq!(fake.lock().unwrap().scopes, ["https://managedhsm.azure.net/.default"]);
}

/// board 8475, AC #1: the metadata service not yet giving the machine's identity (404) is asked
/// again, after a wait, and the call goes on; the authority refusing the credentials (400) fails
/// the call at once, asked once, naming the authority's answer.
#[test]
fn token_fetches_are_retried_unless_the_credentials_are_refused() {
    let fake = Arc::new(Mutex::new(Fake { imds_starting: 1, ..Fake::default() }));
    let server = serve(&fake);
    let base = server.base();
    let source = Source::ManagedIdentity {
        url: format!("{base}/metadata/identity/oauth2/token"),
        client_id: None,
    };
    let kms = over_from(base, "t-{tenant}", None, "https://vault.azure.net", source);
    block_on(kms.wrap(9, &Secret::random())).unwrap();
    let asked = fake.lock().unwrap().asked.clone();
    let imds = "GET /metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fvault.azure.net";
    assert_eq!(asked, [imds, imds, "POST /keys/t-9/wrapkey?api-version=7.4"]);

    let refusing = Arc::new(Mutex::new(Fake { bad_secret: true, ..Fake::default() }));
    let server = serve(&refusing);
    let kms = over(server.base(), "t-{tenant}", None);
    let refused = block_on(kms.wrap(9, &Secret::random())).unwrap_err().to_string();
    assert!(
        refused.ends_with(
            "credentials: the authority answered HTTP 400: {\"error\":\"invalid_client\",\
             \"error_description\":\"AADSTS7000215: Invalid client secret provided.\"}"
        ),
        "{refused}"
    );
    assert_eq!(refusing.lock().unwrap().asked, ["POST /t/oauth2/v2.0/token"]);
}

/// board 8475, AC #2: calls at once, none holding a token, share one request of the authority,
/// however long it takes to answer.
#[test]
fn concurrent_calls_share_one_token_fetch() {
    let delay = Duration::from_millis(200);
    let fake = Arc::new(Mutex::new(Fake { token_delay: delay, ..Fake::default() }));
    let server = serve(&fake);
    let kms = over(server.base(), "t-{tenant}", None);
    let keys: Vec<Secret> = (0..32).map(|_| Secret::random()).collect();
    let wraps = keys.iter().map(|key| kms.wrap(9, key));
    for wrapped in block_on(futures::future::join_all(wraps)) {
        wrapped.unwrap();
    }
    assert_eq!(fake.lock().unwrap().tokens, 1);
}

/// board 8475, AC #3: a call the vault refused the token of drops it only if it is still the one
/// held: one another call fetched since, meanwhile, survives, and the next call uses it.
#[test]
fn a_refused_token_evicts_itself_alone() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = serve(&fake);
    let kms = over(server.base(), "t-{tenant}", None);
    let audience = "https://vault.azure.net";
    block_on(kms.wrap(9, &Secret::random())).unwrap();
    // Another call, the vault having refused token-1, evicted it and fetched token-2.
    kms.inner.tokens.evict(audience, "token-1");
    block_on(kms.wrap(9, &Secret::random())).unwrap();
    assert_eq!(fake.lock().unwrap().tokens, 2);
    // A call that sent token-1 before then, refused, comes to evict it: token-2 stays.
    kms.inner.tokens.evict(audience, "token-1");
    block_on(kms.wrap(9, &Secret::random())).unwrap();
    assert_eq!(fake.lock().unwrap().tokens, 2, "a token fetched since was evicted");
}

/// board 8475, AC #4: a token living 300 s or less is used, fetched again half its lifetime in,
/// not at every call.
#[test]
fn short_lived_tokens_are_used() {
    let fake = Arc::new(Mutex::new(Fake { expires_in: 120, ..Fake::default() }));
    let server = serve(&fake);
    let kms = over(server.base(), "t-{tenant}", None);
    for _ in 0..10 {
        block_on(kms.wrap(9, &Secret::random())).unwrap();
    }
    assert_eq!(fake.lock().unwrap().tokens, 1);
    let asked = Instant::now();
    let held = Held::new(Zeroizing::new("t".into()), asked, Duration::from_secs(120));
    assert_eq!(held.refresh_at, asked + Duration::from_secs(60));
    let held = Held::new(Zeroizing::new("t".into()), asked, Duration::from_secs(3600));
    assert_eq!(held.refresh_at, asked + Duration::from_secs(3300));
}

/// board 8477: its calls run on a runtime of its own: made on a runtime then dropped, then polled
/// by an executor that is no runtime at all, they succeed, the token and the client's connections
/// never the dropped runtime's.
#[test]
fn calls_run_on_any_executor_after_their_callers_runtime_is_gone() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = serve(&fake);
    let kms = over(server.base(), "t-{tenant}", None);
    let key = Secret::random();
    let caller = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let wrapped = caller.block_on(kms.wrap(9, &key)).unwrap();
    drop(caller);
    assert_eq!(block_on(kms.unwrap(9, &wrapped)).unwrap().expose(), key.expose());
    assert_eq!(fake.lock().unwrap().asked.len(), 3, "a call made again");
}

/// hotbed review 47: a token's fetch whose every caller gave up on it holds nothing of the KMS, so
/// that the KMS dropped is gone, the fetch with it: the slot keeping the fetch is the KMS's own,
/// and a fetch holding the KMS kept it alive for good.
#[test]
fn a_fetch_given_up_on_keeps_no_kms_alive() {
    let delay = Duration::from_secs(30);
    let fake = Arc::new(Mutex::new(Fake { token_delay: delay, ..Fake::default() }));
    let server = serve(&fake);
    let kms = over(server.base(), "t-{tenant}", None);
    let inner = Arc::downgrade(&kms.inner);
    let key = Secret::random();
    let mut wrap = Box::pin(kms.wrap(9, &key));
    let waker = futures::task::noop_waker();
    let pending = wrap.as_mut().poll(&mut std::task::Context::from_waker(&waker)).is_pending();
    assert!(pending);
    let eventually = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    eventually("no token asked for", &|| !fake.lock().unwrap().asked.is_empty());
    drop(wrap);
    drop(kms);
    eventually("the KMS kept alive", &|| inner.upgrade().is_none());
}

/// board 8527: a vault's reply without end is refused, read no further than [`http::MAX`] bytes,
/// at once and never asked again, whether it says its length (past any reply's) or none; and a
/// token's, past [`http::TOKEN_MAX`]. A redirect, of the vault or of the authority, is followed
/// not, the call failing, its target never asked: it would have had the wrapped key again, or the
/// client's secret.
#[test]
fn endless_replies_and_redirects_are_refused() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let served = asked.clone();
    let server = Server::start(move |_, request| {
        let route = request.route().to_owned();
        served.lock().unwrap().push(route.clone());
        let token = r#"{"token_type":"Bearer","expires_in":3600,"access_token":"token-1"}"#;
        match &route[..] {
            "/t/oauth2/v2.0/token" => Response::of(200, token),
            "/endless/oauth2/v2.0/token" => Response::endless(200, Endless::Unsaid),
            "/moved/oauth2/v2.0/token" => Response::of(308, "").with("location", "/elsewhere"),
            "/keys/said-9/wrapkey" => Response::endless(200, Endless::Said),
            "/keys/unsaid-9/wrapkey" => Response::endless(200, Endless::Unsaid),
            "/keys/moved-9/wrapkey" => Response::of(307, "").with("location", "/elsewhere"),
            _ => Response::of(200, "{}"),
        }
    });
    let base = server.base();
    for name in ["said", "unsaid"] {
        let kms = over(base, &format!("{name}-{{tenant}}"), None);
        let refused = block_on(kms.wrap(9, &Secret::random())).unwrap_err().to_string();
        assert_eq!(
            refused,
            format!(
                "tenant 9: the KMS failed: Azure Key Vault, wrapkey with {base}/keys/{name}-9: a \
                 reply of over 16384 bytes, refused"
            )
        );
    }
    let kms = over(base, "moved-{tenant}", None);
    let refused = block_on(kms.wrap(9, &Secret::random())).unwrap_err().to_string();
    assert_eq!(
        refused,
        format!(
            "tenant 9: the KMS failed: Azure Key Vault, wrapkey with {base}/keys/moved-9: HTTP \
             307, no code: "
        )
    );
    for (authority, why) in [
        ("endless", "a reply of over 65536 bytes, refused".to_owned()),
        ("moved", "the authority answered HTTP 308: ".to_owned()),
    ] {
        let source = Source::Secret {
            url: format!("{base}/{authority}/oauth2/v2.0/token"),
            client_id: "c".into(),
            secret: Zeroizing::new("s".into()),
        };
        let kms = over_from(base, "t-{tenant}", None, "https://vault.azure.net", source);
        let refused = block_on(kms.wrap(9, &Secret::random())).unwrap_err().to_string();
        assert!(refused.ends_with(&format!("credentials: {why}")), "{refused}");
    }
    let wrapped = ["said", "unsaid", "moved"].map(|name| format!("/keys/{name}-9/wrapkey"));
    let tokens = ["/t", "/endless", "/moved"].map(|at| format!("{at}/oauth2/v2.0/token"));
    let expected = [
        tokens[0].clone(),
        wrapped[0].clone(),
        tokens[0].clone(),
        wrapped[1].clone(),
        tokens[0].clone(),
        wrapped[2].clone(),
        tokens[1].clone(),
        tokens[2].clone(),
    ];
    assert_eq!(*asked.lock().unwrap(), expected);
}
