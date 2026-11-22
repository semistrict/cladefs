// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Azure Key Vault backend against a fake of the vault and its authority, in this process:
//! what it sends, and what it makes of the answers.

use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A tenant's key named whole names a vault: never another host, which its tokens would go to.
#[test]
fn overrides_naming_no_vault_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    std::fs::write(&keys, "7 https://evil.example/keys/x\n").unwrap();
    let url = format!("azure-kv://v.vault.azure.net?key=t-{{tenant}}&keys={}", keys.display());
    let refused = parse(&url).unwrap_err();
    assert!(
        refused.starts_with(&format!(
            "{url:?}: tenant 7's key, \"https://evil.example/keys/x\", names no key: \"evil.example\" is no vault's host"
        )),
        "{refused}"
    );
}

/// A fake of a vault and its authority: tokens `token-N`; keys of version `v1`, wrapping by
/// reversing what they are given; a key named `revoked-...` refused (403), `missing-...` none
/// (404); the first call of the vault refused as unauthorized if `refuse_first`.
#[derive(Default)]
struct Fake {
    tokens: u64,
    refuse_first: bool,
    /// Each request: its method and path.
    asked: Vec<String>,
    /// The scope of each token asked for.
    scopes: Vec<String>,
}

/// A response: its status and JSON body.
fn answer(
    fake: &Mutex<Fake>,
    base: &str,
    method: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> (u16, String) {
    let mut fake = fake.lock().unwrap();
    fake.asked.push(format!("{method} {path}"));
    let (path, _) = path.split_once('?').unwrap_or((path, ""));
    if path.ends_with("/oauth2/v2.0/token") {
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body).into_owned().collect();
        let has = |name: &str, value: &str| form.iter().any(|(n, v)| n == name && v == value);
        let scope = form.iter().find(|(n, _)| n == "scope").map(|(_, v)| v.clone()).unwrap();
        fake.scopes.push(scope);
        assert!(has("client_secret", "s") && has("client_id", "c"), "{form:?}");
        fake.tokens += 1;
        return (
            200,
            format!(
                r#"{{"token_type":"Bearer","expires_in":3600,"access_token":"token-{}"}}"#,
                fake.tokens
            ),
        );
    }
    let bearer = headers.lines().find_map(|line| line.strip_prefix("authorization: Bearer "));
    if bearer.is_none_or(|token| !token.starts_with("token-"))
        || std::mem::take(&mut fake.refuse_first)
    {
        return (401, r#"{"error":{"code":"Unauthorized","message":"no token"}}"#.into());
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let (name, version, op) = match segments[..] {
        ["keys", name, "wrapkey"] => (name, "v1", "wrapkey"),
        ["keys", name, version, "unwrapkey"] => (name, version, "unwrapkey"),
        _ => return (404, r#"{"error":{"code":"NotFound","message":"no such path"}}"#.into()),
    };
    if name.starts_with("revoked-") {
        return (403, r#"{"error":{"code":"Forbidden","message":"not allowed"}}"#.into());
    }
    if name.starts_with("missing-") {
        return (404, r#"{"error":{"code":"KeyNotFound","message":"no key"}}"#.into());
    }
    let request: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(request["alg"], "RSA-OAEP-256");
    let mut value = decode(request["value"].as_str().unwrap()).unwrap();
    if op == "unwrapkey" && (version != "v1" || value.len() != 40) {
        return (400, r#"{"error":{"code":"BadParameter","message":"Error decrypting."}}"#.into());
    }
    value.reverse();
    let value = BASE64URL.encode(value);
    (200, format!(r#"{{"kid":"{base}/keys/{name}/{version}","value":"{value}"}}"#))
}

/// Serves `fake` on a port of this host's, on the runtime this is called on: its base URL.
async fn serve(fake: Arc<Mutex<Fake>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = base.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (fake, base) = (fake.clone(), served.clone());
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                loop {
                    let end = loop {
                        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at;
                        }
                        let mut chunk = [0; 4096];
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                    let original = String::from_utf8_lossy(&buffer[..end]).into_owned();
                    let mut words = original.split_whitespace();
                    let (method, path) =
                        (words.next().unwrap().to_owned(), words.next().unwrap().to_owned());
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .map_or(0, |n| n.trim().parse().unwrap());
                    while buffer.len() < end + 4 + length {
                        let mut chunk = [0; 4096];
                        let read = stream.read(&mut chunk).await.unwrap();
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body = buffer[end + 4..end + 4 + length].to_vec();
                    buffer.drain(..end + 4 + length);
                    // Header names as sent, values as sent: the bearer's token is case-sensitive.
                    let headers: String = original
                        .lines()
                        .map(|line| match line.split_once(": ") {
                            Some((name, value)) => format!("{}: {value}\n", name.to_lowercase()),
                            None => String::new(),
                        })
                        .collect();
                    let (status, json) = answer(&fake, &base, &method, &path, &headers, &body);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{json}",
                        json.len()
                    );
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    base
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

/// A backend over the fake at `base`, its keys named `prefix{tenant}`, overridden by `keys`.
fn over(base: &str, template: &str, keys: Option<&str>) -> AzureKms {
    over_for(base, template, keys, "https://vault.azure.net")
}

/// The same, its URL's vault's tokens for `audience`.
fn over_for(base: &str, template: &str, keys: Option<&str>, audience: &'static str) -> AzureKms {
    let names = KeyNames::new(template, keys).unwrap();
    let parsed = Parsed { vault: base.to_owned(), audience, names, alg: ALGORITHMS[0] };
    let source = Source::Secret {
        url: format!("{base}/t/oauth2/v2.0/token"),
        client_id: "c".into(),
        secret: Zeroizing::new("s".into()),
    };
    AzureKms::new(parsed, source).unwrap()
}

/// A key wrapped unwraps for its tenant: one token fetched for both calls, the version that
/// wrapped it named in what is kept and asked for on the unwrap, the tenant bound in what the
/// vault wraps. A key wrapped for another tenant under the same key is refused as not authentic;
/// one kept under another key's name is refused asking the vault nothing.
#[test]
fn keys_wrapped_unwrap_for_their_tenant_alone() {
    let runtime = runtime();
    runtime.block_on(async {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone()).await;
        let kms = over(&base, "t-{tenant}", Some("9 shared\na shared\n"));
        let key = Secret::random();
        let wrapped = kms.wrap(9, &key).await.unwrap();
        let id = format!("{base}/keys/shared");
        let kept = real::ciphertext(&wrapped, SCHEME, &id).unwrap();
        assert_eq!((&kept[..3], kept.len()), (&b"\x02v1"[..], 3 + 40));
        assert_eq!(kms.unwrap(9, &wrapped).await.unwrap().expose(), key.expose());
        assert_eq!(
            fake.lock().unwrap().asked,
            [
                "POST /t/oauth2/v2.0/token".to_owned(),
                "POST /keys/shared/wrapkey?api-version=7.4".to_owned(),
                "POST /keys/shared/v1/unwrapkey?api-version=7.4".to_owned(),
            ]
        );
        let refused = kms.unwrap(10, &wrapped).await.unwrap_err().to_string();
        assert_eq!(refused, "tenant 10: not authentic: a wrapped key, wrapped for another tenant");
        let asked = fake.lock().unwrap().asked.len();
        let refused = kms.unwrap(11, &wrapped).await.unwrap_err().to_string();
        assert_eq!(refused, "tenant 11: not authentic: a wrapped key, wrapped under another key");
        assert_eq!(fake.lock().unwrap().asked.len(), asked, "the vault asked of another key's");
        assert_eq!(fake.lock().unwrap().tokens, 1);
    });
}

/// The vault refusing: a key disabled or not granted is revoked, one missing none; a token the
/// vault refuses is fetched again, once, and the call goes on.
#[test]
fn refusals_reach_their_tenants_and_tokens_are_fetched_again() {
    let runtime = runtime();
    runtime.block_on(async {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone()).await;
        let revoked = over(&base, "revoked-{tenant}", None);
        let refused = revoked.wrap(9, &Secret::random()).await.unwrap_err().to_string();
        assert_eq!(refused, "tenant 9: its KEK is revoked");
        let missing = over(&base, "missing-{tenant}", None);
        let refused = missing.wrap(9, &Secret::random()).await.unwrap_err().to_string();
        assert_eq!(refused, "tenant 9: no KEK in the KMS");
        let kms = over(&base, "t-{tenant}", None);
        fake.lock().unwrap().refuse_first = true;
        let tokens = fake.lock().unwrap().tokens;
        kms.wrap(9, &Secret::random()).await.unwrap();
        assert_eq!(fake.lock().unwrap().tokens, tokens + 2, "a token refused not fetched again");
    });
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
    let parsed = parse("azure-kv://h.managedhsm.azure.net?key=t-{tenant}&alg=A256KW").unwrap();
    assert_eq!(parsed.audience, "https://managedhsm.azure.net");
    let runtime = runtime();
    runtime.block_on(async {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let base = serve(fake.clone()).await;
        let keys = "9 https://theirs.vault.azure.cn/keys/k\n";
        let kms = over_for(&base, "t-{tenant}", Some(keys), "https://managedhsm.azure.net");
        assert_eq!(kms.audience("https://theirs.vault.azure.cn/keys/k"), "https://vault.azure.cn");
        assert_eq!(kms.audience(&format!("{base}/keys/t-8")), "https://managedhsm.azure.net");
        kms.wrap(8, &Secret::random()).await.unwrap();
        assert_eq!(fake.lock().unwrap().scopes, ["https://managedhsm.azure.net/.default"]);
    });
}
