// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Azure Key Vault backend against a fake of the vault and its authority, in this process:
//! what it sends, and what it makes of the answers.

use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[test]
fn urls_name_a_vault_and_its_tenants_keys() {
    let parsed = parse("azure-kv://myvault.vault.azure.net?key=cladefs-tenant-{tenant}").unwrap();
    assert_eq!(
        (parsed.vault.as_str(), parsed.alg),
        ("https://myvault.vault.azure.net", "RSA-OAEP-256")
    );
    assert_eq!(
        identifier(&parsed.vault, &parsed.names.of(42)),
        "https://myvault.vault.azure.net/keys/cladefs-tenant-1a"
    );
    let hsm = parse("azure-kv://h.managedhsm.azure.net?key=t-{tenant}&alg=A256KW").unwrap();
    assert_eq!(hsm.alg, "A256KW");
    let private = parse("azure-kv://v.vault.azure.net?key=t-{tenant}&endpoint=http://127.0.0.1:9/");
    assert_eq!(private.unwrap().vault, "http://127.0.0.1:9");
}

#[test]
fn overrides_name_keys_in_the_vault_or_in_other_vaults() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    std::fs::write(&keys, "7 https://theirs.vault.azure.net/keys/own\n8 their-key\n").unwrap();
    let url = format!("azure-kv://v.vault.azure.net?key=t-{{tenant}}&keys={}", keys.display());
    let Parsed { vault, names, .. } = parse(&url).unwrap();
    assert_eq!(identifier(&vault, &names.of(7)), "https://theirs.vault.azure.net/keys/own");
    assert_eq!(identifier(&vault, &names.of(8)), "https://v.vault.azure.net/keys/their-key");
}

#[test]
fn urls_naming_no_vault_or_no_keys_are_refused() {
    let usage = "azure-kv://VAULT_HOST?key=TEMPLATE[&keys=FILE][&alg=ALG][&endpoint=URL]";
    let refused = |url: &str| parse(url).unwrap_err();
    let url = "azure-kv://?key=t-{tenant}";
    assert_eq!(refused(url), format!("{url:?} names no vault: {usage}"));
    let url = "azure-kv://V.vault.azure.net?key=t-{tenant}";
    assert_eq!(refused(url), format!("{url:?} names no vault: {usage}"));
    let url = "azure-kv://v.vault.azure.net?key=t-{tenant}&region=x";
    assert_eq!(refused(url), format!("{url:?}: no parameter \"region\": {usage}"));
    let url = "azure-kv://v.vault.azure.net?key=t-{tenant}&alg=RSA1_5";
    assert_eq!(
        refused(url),
        format!("{url:?}: no algorithm \"RSA1_5\": RSA-OAEP-256, A256KW, CKM_AES_KEY_WRAP")
    );
    let url = "azure-kv://v.vault.azure.net?key=t_{tenant}";
    assert_eq!(
        refused(url),
        format!(
            "{url:?}: the key template, \"t_fzzzzzzzzzzzz\", names no key: a key's name is 1 to \
             127 letters, digits and -"
        )
    );
    let url = "azure-kv://v.vault.azure.net";
    assert_eq!(
        refused(url),
        format!("{url:?}: the KMS URL names no key template: key=...{{tenant}}...")
    );
}

#[test]
fn credentials_are_found_as_azures_libraries_find_them() {
    let of = |vars: &[(&str, &str)]| {
        let vars: Vec<(String, String)> =
            vars.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
        source(&|name| vars.iter().find(|(of, _)| of == name).map(|(_, value)| value.clone()))
    };
    let kind = |source: Source| match source {
        Source::Static(_) => "static".to_owned(),
        Source::Federated { url, .. } => format!("federated {url}"),
        Source::Secret { url, .. } => format!("secret {url}"),
        Source::ManagedIdentity { url, client_id } => format!("managed {url} {client_id:?}"),
    };
    let principal = [("AZURE_TENANT_ID", "t"), ("AZURE_CLIENT_ID", "c")];
    assert_eq!(kind(of(&[(TOKEN_VAR, "x"), principal[0], principal[1]])), "static");
    let federated = [principal[0], principal[1], ("AZURE_FEDERATED_TOKEN_FILE", "/f")];
    assert_eq!(
        kind(of(&federated)),
        "federated https://login.microsoftonline.com/t/oauth2/v2.0/token"
    );
    let secret = [principal[0], principal[1], ("AZURE_CLIENT_SECRET", "s")];
    assert_eq!(kind(of(&secret)), "secret https://login.microsoftonline.com/t/oauth2/v2.0/token");
    let sovereign = [secret[0], secret[1], secret[2], ("AZURE_AUTHORITY_HOST", "https://a.cn/")];
    assert_eq!(kind(of(&sovereign)), "secret https://a.cn/t/oauth2/v2.0/token");
    assert_eq!(kind(of(&[])), format!("managed {IMDS} None"));
    assert_eq!(kind(of(&[principal[1]])), format!("managed {IMDS} Some(\"c\")"));
}

const ID: &str = "https://v.vault.azure.net/keys/t-9";

fn mapped(op: Op, code: u16, body: &str) -> (String, bool, bool) {
    let Failure { error, transient, unauthorized } = refusal(9, ID, op, code, body.as_bytes());
    (error.to_string(), transient, unauthorized)
}

#[test]
fn refusals_are_mapped_by_their_codes() {
    let missing = r#"{"error":{"code":"KeyNotFound","message":"A key with (name/id) t-9 was not found in this key vault."}}"#;
    assert_eq!(
        mapped(Op::Wrap, 404, missing),
        ("tenant 9: no KEK in the KMS".into(), false, false)
    );
    let forbidden = r#"{"error":{"code":"Forbidden","message":"Caller is not authorized to perform action on resource."}}"#;
    assert_eq!(
        mapped(Op::Unwrap, 403, forbidden),
        ("tenant 9: its KEK is revoked".into(), false, false)
    );
    let bad = r#"{"error":{"code":"BadParameter","message":"Error decrypting."}}"#;
    assert_eq!(
        mapped(Op::Unwrap, 400, bad),
        ("tenant 9: not authentic: a wrapped key, refused by the KMS".into(), false, false)
    );
    assert_eq!(
        mapped(Op::Wrap, 400, bad),
        (
            format!(
                "tenant 9: the KMS failed: Azure Key Vault, wrapkey with {ID}: HTTP 400, BadParameter: Error decrypting."
            ),
            false,
            false
        )
    );
    let vault = r#"{"error":{"code":"VaultNotFound","message":"no vault"}}"#;
    assert!(!mapped(Op::Wrap, 404, vault).0.contains("no KEK"));
    let unauthorized =
        r#"{"error":{"code":"Unauthorized","message":"AKV10022: Invalid audience."}}"#;
    assert_eq!(mapped(Op::Wrap, 401, unauthorized).2, true);
    assert_eq!(mapped(Op::Wrap, 429, "").1, true);
    assert_eq!(
        mapped(Op::Unwrap, 503, "<html/>"),
        (
            format!(
                "tenant 9: the KMS failed: Azure Key Vault, unwrapkey with {ID}: HTTP 503, no code: "
            ),
            true,
            false
        )
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
        assert!(
            has("scope", SCOPE) && has("client_secret", "s") && has("client_id", "c"),
            "{form:?}"
        );
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
    let names = KeyNames::new(template, keys).unwrap();
    let parsed = Parsed { vault: base.to_owned(), names, alg: ALGORITHMS[0] };
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
