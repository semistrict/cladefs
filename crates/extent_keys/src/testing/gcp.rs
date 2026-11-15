// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax's own: what tests against real Google Cloud KMS need (TASK-411.18): a key ring's keys,
//! made if missing, and what a tenant does to its key out of band: rotating it, disabling,
//! enabling and destroying its versions ([`Admin`]). The tests run only given an access token,
//! [`TOKEN_VAR`]'s, as the backend reads it (`gcp-kms://`): CI holds none, so they say why they
//! skip and pass.
//!
//! Cloud KMS keeps every key and key ring for good, and bills each version neither destroyed nor
//! scheduled to be: the tests share one key ring ([`RING_VAR`], else [`RING`]) and a few keys in
//! it, made the first time, and destroy each version they make but the one a key keeps enabled.
//! A version's state, changed, takes effect within a minute or two ([`eventually`]).

use std::future::Future;
use std::time::{Duration, Instant};

/// The access token the backend and [`Admin`] call the KMS with
/// (`gcloud auth print-access-token`).
pub use crate::kms::GCP_TOKEN_VAR as TOKEN_VAR;

/// The key ring the tests keep their keys in, if not [`RING`].
pub const RING_VAR: &str = "FXDAX_KMS_GCP_TEST_RING";

/// The key ring the tests keep their keys in, unless [`RING_VAR`] names another: the user's test
/// project's (TASK-411.18's notes).
pub const RING: &str =
    "projects/echophase-connectome-test/locations/us-central1/keyRings/fxdax-test";

/// How long [`eventually`] waits for the KMS to say what a test expects, and between asking.
const PATIENCE: Duration = Duration::from_secs(240);
const POLL: Duration = Duration::from_secs(5);

/// Where its calls go.
const ENDPOINT: &str = "https://cloudkms.googleapis.com/v1/";

/// A tenant of the KMS, as it manages its keys: in the key ring the tests share.
pub struct Admin {
    http: reqwest::Client,
    token: String,
    ring: String,
}

impl Admin {
    /// The tests' admin, if [`TOKEN_VAR`] holds a token; else none, `test` saying on stderr why it
    /// skips.
    pub fn from_env(test: &str) -> Option<Self> {
        let Some(token) = std::env::var(TOKEN_VAR).ok().filter(|token| !token.is_empty()) else {
            eprintln!(
                "{test}: skipped: {TOKEN_VAR} holds no access token for Google Cloud KMS \
                 (FXDAX_KMS_GCP_TOKEN=$(gcloud auth print-access-token))"
            );
            return None;
        };
        let ring = std::env::var(RING_VAR).unwrap_or_else(|_| RING.to_owned());
        Some(Self { http: reqwest::Client::new(), token, ring })
    }

    /// Its key ring's resource name.
    pub fn ring(&self) -> &str {
        &self.ring
    }

    /// The URL of the KMS naming its tenants' keys by `template` in its key ring, and `overrides`,
    /// a file of them, if any.
    pub fn url(&self, template: &str, overrides: Option<&std::path::Path>) -> String {
        let keys = overrides.map(|path| format!("&keys={}", path.display())).unwrap_or_default();
        format!("gcp-kms://{}?key={template}{keys}", self.ring)
    }

    /// The resource name of key `id` of its key ring, made, and the ring, if missing: symmetric,
    /// its versions destroyed a day after they are scheduled to be, the shortest Cloud KMS has.
    pub async fn key(&self, id: &str) -> String {
        let name = format!("{}/cryptoKeys/{id}", self.ring);
        if self.call(reqwest::Method::GET, &name, None).await.is_ok() {
            return name;
        }
        let (parent, ring_id) = self.ring.rsplit_once("/keyRings/").expect("a key ring's name");
        let ring = format!("{parent}/keyRings?keyRingId={ring_id}");
        self.made(self.call(reqwest::Method::POST, &ring, Some(serde_json::json!({}))).await);
        let key = serde_json::json!({
            "purpose": "ENCRYPT_DECRYPT",
            "destroyScheduledDuration": "86400s",
            "versionTemplate": {
                "algorithm": "GOOGLE_SYMMETRIC_ENCRYPTION",
                "protectionLevel": "SOFTWARE",
            },
        });
        let keys = format!("{}/cryptoKeys?cryptoKeyId={id}", self.ring);
        self.made(self.call(reqwest::Method::POST, &keys, Some(key)).await);
        name
    }

    /// The versions of `key` and their states, in order.
    pub async fn versions(&self, key: &str) -> Vec<(u32, String)> {
        let path = format!("{key}/cryptoKeyVersions?pageSize=1000");
        let listed = self.call(reqwest::Method::GET, &path, None).await.unwrap();
        let versions = listed["cryptoKeyVersions"].as_array().cloned().unwrap_or_default();
        let mut versions: Vec<_> = versions
            .iter()
            .map(|version| {
                (number(&version["name"]), version["state"].as_str().unwrap().to_owned())
            })
            .collect();
        versions.sort();
        versions
    }

    /// `key`'s primary version.
    pub async fn primary(&self, key: &str) -> u32 {
        number(&self.call(reqwest::Method::GET, key, None).await.unwrap()["primary"]["name"])
    }

    /// A new version of `key`, made its primary, once its encryptions are under it: a rotation.
    pub async fn rotate(&self, key: &str) -> u32 {
        let path = format!("{key}/cryptoKeyVersions");
        let made = self.call(reqwest::Method::POST, &path, Some(serde_json::json!({}))).await;
        let version = number(&made.unwrap()["name"]);
        self.make_primary(key, version).await;
        version
    }

    /// Makes `version` `key`'s primary, once its encryptions are under it.
    pub async fn make_primary(&self, key: &str, version: u32) {
        let path = format!("{key}:updatePrimaryVersion");
        let body = serde_json::json!({ "cryptoKeyVersionId": version.to_string() });
        self.call(reqwest::Method::POST, &path, Some(body)).await.unwrap();
        let encrypt = &format!("{key}:encrypt");
        let probe = serde_json::json!({ "plaintext": "cHJvYmU=" });
        eventually(&format!("{key}'s encryptions under version {version}"), move || {
            let probe = probe.clone();
            async move {
                match self.call(reqwest::Method::POST, encrypt, Some(probe)).await {
                    Ok(answer) if number(&answer["name"]) == version => Ok(()),
                    Ok(answer) => Err(format!("under {}", answer["name"])),
                    Err((code, body)) => Err(format!("HTTP {code}: {body}")),
                }
            }
        })
        .await;
    }

    /// Sets `version` of `key` to `state`, `ENABLED` or `DISABLED`.
    pub async fn set_state(&self, key: &str, version: u32, state: &str) {
        let path = format!("{key}/cryptoKeyVersions/{version}?updateMask=state");
        let body = serde_json::json!({ "state": state });
        self.call(reqwest::Method::PATCH, &path, Some(body)).await.unwrap();
    }

    /// Schedules `version` of `key` to be destroyed, a day from now: from now, it is as destroyed
    /// to every call, once that takes effect.
    pub async fn destroy(&self, key: &str, version: u32) {
        let path = format!("{key}/cryptoKeyVersions/{version}:destroy");
        self.call(reqwest::Method::POST, &path, Some(serde_json::json!({}))).await.unwrap();
    }

    /// Fails unless `made` made what it asked for, or found it made.
    fn made(&self, made: Result<serde_json::Value, (u16, String)>) {
        if let Err((code, body)) = made
            && code != 409
        {
            panic!("HTTP {code}: {body}");
        }
    }

    /// The JSON the KMS answered `method` of `path` with, `body` its request's; or the HTTP status
    /// and body of its refusal.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, (u16, String)> {
        let mut request =
            self.http.request(method, format!("{ENDPOINT}{path}")).bearer_auth(&self.token);
        if let Some(body) = body {
            let body = serde_json::to_vec(&body).unwrap();
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
        }
        let response = request.send().await.map_err(|err| (0, err.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|err| (0, err.to_string()))?;
        match status.is_success() {
            true => Ok(serde_json::from_str(&text).unwrap()),
            false => Err((status.as_u16(), text)),
        }
    }
}

/// The number of the key version whose resource name `name` holds.
fn number(name: &serde_json::Value) -> u32 {
    let name = name.as_str().unwrap_or_else(|| panic!("no version's name: {name}"));
    name.rsplit_once("/cryptoKeyVersions/").unwrap().1.parse().unwrap()
}

/// What `check` gives once it gives it, asked every few seconds: as a version's state changes
/// take effect in Cloud KMS, eventually. Fails, naming `what` and what `check` saw last, if they
/// take more than [`PATIENCE`].
pub async fn eventually<T, F: Future<Output = Result<T, String>>>(
    what: &str,
    mut check: impl FnMut() -> F,
) -> T {
    let start = Instant::now();
    loop {
        let seen = match check().await {
            Ok(found) => return found,
            Err(seen) => seen,
        };
        assert!(start.elapsed() < PATIENCE, "{what}: not after {PATIENCE:?}, but {seen}");
        tokio::time::sleep(POLL).await;
    }
}
