// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-411.18 AC #4: the backend against real Google Cloud KMS, through [`kms::from_url`], as a
//! host configures it; each test its own key of the tests' key ring ([`Admin`]), as tenants'
//! keys are named, `fxdax-test-<tenant>`. Skipped, saying why, without an access token.

use super::SCHEME;
use crate::kms::{self, Error, Kms as _, LocalKms, real};
use crate::testing::gcp::{Admin, eventually};
use crate::{Secret, Wrapped};

/// How the tests' tenants' keys are named.
const TEMPLATE: &str = "fxdax-test-{tenant}";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

/// The KMS `admin`'s key ring holds, its tenants' keys named by [`TEMPLATE`] and `overrides`.
fn kms_of(admin: &Admin, overrides: Option<&std::path::Path>) -> kms::Shared {
    kms::from_url(&admin.url(TEMPLATE, overrides)).unwrap()
}

/// The error of `result`, displayed.
fn refused<T>(result: Result<T, Error>) -> String {
    result.map(drop).unwrap_err().to_string()
}

fn holds(bytes: &[u8], piece: &[u8]) -> bool {
    bytes.windows(piece.len()).any(|window| window == piece)
}

/// Destroys every version of `key` neither `kept` nor destroyed already, nor scheduled to be: what
/// a run that failed partway may have left billed.
async fn destroy_all_but(admin: &Admin, key: &str, kept: u32) {
    for (version, state) in admin.versions(key).await {
        if version != kept && matches!(&state[..], "ENABLED" | "DISABLED") {
            admin.destroy(key, version).await;
        }
    }
}

/// A key wrapped and unwrapped: its header naming the backend and the key, no byte of the key in
/// it, each wrap anew. Refused as not authentic: what names another tenant's key, before any call;
/// what names this key for another tenant, whose associated data the KMS refuses; a ciphertext
/// changed; a key [`LocalKms`] wrapped. A tenant with no key has no KEK; one whose key cladefs may
/// not use is revoked.
#[test]
fn keys_wrap_unwrap_and_bind_their_tenant() {
    let Some(admin) = Admin::from_env("keys_wrap_unwrap_and_bind_their_tenant") else { return };
    runtime().block_on(async {
        let name = admin.key("fxdax-test-1").await;
        let dir = tempfile::tempdir().unwrap();
        let overrides = dir.path().join("keys");
        let foreign = "projects/cloud-samples-data/locations/global/keyRings/r/cryptoKeys/k";
        std::fs::write(&overrides, format!("11 fxdax-test-1\nbeef {foreign}\n")).unwrap();
        let kms = kms_of(&admin, Some(&overrides));
        let key = Secret::random();
        let wrapped = kms.wrap_dyn(1, &key).await.unwrap();
        let header = real::wrapped(SCHEME, &name, b"");
        assert!(wrapped.0.starts_with(&header.0), "{wrapped:?}");
        assert!(!holds(&wrapped.0, key.expose()));
        assert_eq!(kms.unwrap_dyn(1, &wrapped).await.unwrap().expose(), key.expose());
        let again = kms.wrap_dyn(1, &key).await.unwrap();
        assert_ne!(again, wrapped);
        assert_eq!(kms.unwrap_dyn(1, &again).await.unwrap().expose(), key.expose());

        let unauthentic = |tenant: u64, why: &str| format!("tenant {tenant}: not authentic: {why}");
        let other_key = "a wrapped key, wrapped under another key";
        assert_eq!(refused(kms.unwrap_dyn(3, &wrapped).await), unauthentic(3, other_key));
        let by_kms = "a wrapped key, refused by the KMS";
        assert_eq!(refused(kms.unwrap_dyn(0x11, &wrapped).await), unauthentic(0x11, by_kms));
        let mut changed = wrapped.clone();
        *changed.0.last_mut().unwrap() ^= 1;
        assert_eq!(refused(kms.unwrap_dyn(1, &changed).await), unauthentic(1, by_kms));
        let truncated = Wrapped(wrapped.0[..header.0.len() + 8].to_vec());
        assert_eq!(refused(kms.unwrap_dyn(1, &truncated).await), unauthentic(1, by_kms));
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(1).unwrap();
        let locally = local.wrap(1, &key).await.unwrap();
        let by_header = "a wrapped key, by its header";
        assert_eq!(refused(kms.unwrap_dyn(1, &locally).await), unauthentic(1, by_header));
        assert_eq!(refused(local.unwrap(1, &wrapped).await), unauthentic(1, by_header));

        assert_eq!(refused(kms.wrap_dyn(0xdead, &key).await), "tenant 57005: no KEK in the KMS");
        let missing = format!("{}/cryptoKeys/fxdax-test-dead", admin.ring());
        let ciphertext = &wrapped.0[header.0.len()..];
        let naming_missing = real::wrapped(SCHEME, &missing, ciphertext);
        let no_kek = refused(kms.unwrap_dyn(0xdead, &naming_missing).await);
        assert_eq!(no_kek, "tenant 57005: no KEK in the KMS");
        let revoked = "tenant 48879: its KEK is revoked";
        assert_eq!(refused(kms.wrap_dyn(0xbeef, &key).await), revoked);
        let naming_foreign = real::wrapped(SCHEME, foreign, ciphertext);
        assert_eq!(refused(kms.unwrap_dyn(0xbeef, &naming_foreign).await), revoked);
    });
}

/// A key rotated, a new version its primary: what the old one wrapped unwraps still, what is
/// wrapped after is under the new one, which unwraps once the old one is destroyed, and what the
/// old one wrapped no more, refused naming the version alone (board 8479).
#[test]
fn rotated_keys_unwrap_old_wraps_until_their_version_is_destroyed() {
    let test = "rotated_keys_unwrap_old_wraps_until_their_version_is_destroyed";
    let Some(admin) = Admin::from_env(test) else { return };
    runtime().block_on(async {
        let name = admin.key("fxdax-test-2").await;
        let old = admin.primary(&name).await;
        destroy_all_but(&admin, &name, old).await;
        let kms = kms_of(&admin, None);
        let (first, second) = (Secret::random(), Secret::random());
        let under_old = kms.wrap_dyn(2, &first).await.unwrap();
        let new = admin.rotate(&name).await;
        assert!(new > old, "{new} after {old}");
        let under_new = kms.wrap_dyn(2, &second).await.unwrap();
        assert_eq!(kms.unwrap_dyn(2, &under_old).await.unwrap().expose(), first.expose());
        assert_eq!(kms.unwrap_dyn(2, &under_new).await.unwrap().expose(), second.expose());

        admin.destroy(&name, old).await;
        let (kms, under_old) = (&kms, &under_old);
        eventually(&format!("version {old} of {name} destroyed"), move || async move {
            match kms.unwrap_dyn(2, under_old).await {
                // board 8479: the version's loss, not the tenant's KEK's.
                Err(Error::VersionGone { tenant: 2, version }) if version == old.to_string() => {
                    Ok(())
                }
                other => Err(format!("{:?}", other.map(drop))),
            }
        })
        .await;
        assert_eq!(kms.unwrap_dyn(2, &under_new).await.unwrap().expose(), second.expose());
        kms.wrap_dyn(2, &first).await.unwrap();
    });
}

/// A key's version disabled: nothing wraps under it, the tenant's KEK revoked, nor unwraps what it
/// wrapped, that version disabled (board 8479); enabled again, what it wrapped unwraps.
#[test]
fn disabled_versions_revoke_until_enabled() {
    let Some(admin) = Admin::from_env("disabled_versions_revoke_until_enabled") else { return };
    runtime().block_on(async {
        let name = admin.key("fxdax-test-3").await;
        let version = admin.primary(&name).await;
        destroy_all_but(&admin, &name, version).await;
        let kms = &kms_of(&admin, None);
        // Enabled, as a run that failed partway may not have left it.
        admin.set_state(&name, version, "ENABLED").await;
        let key = &Secret::random();
        let wrapped = &eventually("a wrap", move || async move {
            kms.wrap_dyn(3, key).await.map_err(|err| err.to_string())
        })
        .await;

        admin.set_state(&name, version, "DISABLED").await;
        eventually(&format!("version {version} of {name} disabled"), move || async move {
            let unwrapped = kms.unwrap_dyn(3, wrapped).await.map(drop);
            let rewrapped = kms.wrap_dyn(3, key).await.map(drop);
            match (&unwrapped, &rewrapped) {
                // board 8479: the version's that wrapped the key, of an unwrap; the key's primary,
                // so the tenant's KEK, of a wrap.
                (
                    Err(Error::VersionDisabled { tenant: 3, version: of }),
                    Err(Error::Revoked { tenant: 3 }),
                ) if *of == version.to_string() => Ok(()),
                _ => Err(format!("{unwrapped:?}, {rewrapped:?}")),
            }
        })
        .await;

        admin.set_state(&name, version, "ENABLED").await;
        let unwrapped =
            eventually(&format!("version {version} of {name} enabled"), move || async move {
                kms.unwrap_dyn(3, wrapped).await.map_err(|err| err.to_string())
            })
            .await;
        assert_eq!(unwrapped.expose(), key.expose());
    });
}

/// A key whose primary version is destroyed: nothing wraps under it, the tenant's KEK destroyed,
/// nor unwraps what it wrapped, that version gone (board 8479). The key keeps no version
/// undestroyed, none billed.
#[test]
fn destroyed_primaries_destroy() {
    let Some(admin) = Admin::from_env("destroyed_primaries_destroy") else { return };
    runtime().block_on(async {
        let name = admin.key("fxdax-test-4").await;
        let version = admin.rotate(&name).await;
        destroy_all_but(&admin, &name, version).await;
        let kms = &kms_of(&admin, None);
        let key = &Secret::random();
        let wrapped = &kms.wrap_dyn(4, key).await.unwrap();
        admin.destroy(&name, version).await;
        eventually(&format!("version {version} of {name} destroyed"), move || async move {
            let unwrapped = kms.unwrap_dyn(4, wrapped).await.map(drop);
            let rewrapped = kms.wrap_dyn(4, key).await.map(drop);
            match (&unwrapped, &rewrapped) {
                // board 8479: the version's that wrapped the key, of an unwrap; the key's primary,
                // so the tenant's KEK, of a wrap.
                (
                    Err(Error::VersionGone { tenant: 4, version: of }),
                    Err(Error::Destroyed { tenant: 4 }),
                ) if *of == version.to_string() => Ok(()),
                _ => Err(format!("{unwrapped:?}, {rewrapped:?}")),
            }
        })
        .await;
    });
}
