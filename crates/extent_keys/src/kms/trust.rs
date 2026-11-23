// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a KMS URL the host did not choose may reach (board 8525). A URL the registry
//! of buckets records for a bucket (`kms`) is an unauthenticated object's, which anyone who can
//! write the registry chooses: given it as the host's own, a host would send its KMS credentials,
//! and plaintext keys, wherever it says. So such a URL is [`Origin::Recorded`], and each backend,
//! as it reads it and before it opens a file or a connection, refuses what the host has not
//! allowed ([`Trust`]):
//!
//! - a file of the host's: `keys=FILE`, `ca=FILE`, `CLADEFS_KMS`'s alone;
//! - `local://`, a fake for tests, unless [`LOCAL_VAR`] allows it;
//! - an OpenBao or a vault (Azure Key Vault) of a host none of [`HOSTS_VAR`] names, nor the host's
//!   own `CLADEFS_KMS`; and OpenBao over cleartext but on this host;
//! - an AWS KMS endpoint not AWS's own over HTTPS (`*.amazonaws.com`), nor on this host.
//!
//! The host's own `CLADEFS_KMS` is [`Origin::Host`]: trusted, as the host's own configuration
//! (hotbed question 475).

use super::vars::{KMS_VAR, Vars};
use std::net::IpAddr;

/// The variable naming the hosts a KMS URL the registry records may reach, besides the host's own
/// KMS's: separated by commas or white space, each a host's name or address, alone (any port) or
/// with a port (`vault.example.com`, `10.0.0.5:8200`, `[::1]:8200`, `myvault.vault.azure.net`).
pub const HOSTS_VAR: &str = "CLADEFSD_KMS_HOSTS";

/// The variable which, `1`, lets a KMS URL the registry records be `local://`, a fake for tests.
pub const LOCAL_VAR: &str = "CLADEFSD_KMS_LOCAL";

/// Where a KMS URL came from, and so what it may reach.
#[derive(Clone, Copy)]
pub enum Origin<'a> {
    /// The host's own configuration (`CLADEFS_KMS`): anything.
    Host,
    /// The registry of buckets: what `Trust` allows alone.
    Recorded(&'a Trust),
}

/// What a recorded KMS URL may reach, as the host's variables say: the hosts [`HOSTS_VAR`] names,
/// and its own `CLADEFS_KMS`'s; `local://` if [`LOCAL_VAR`] allows it.
#[derive(Debug, Default)]
pub struct Trust {
    /// Each a host, and its port if named.
    hosts: Vec<(String, Option<u16>)>,
    local: bool,
}

impl Trust {
    /// What `vars`, a host's or its bucket's, allow. A [`LOCAL_VAR`] neither `1` nor unset, or a
    /// host of [`HOSTS_VAR`] with a port that is none, is refused, never taken for allowing less.
    pub fn of(vars: &Vars) -> Result<Self, String> {
        let mut hosts = Vec::new();
        for entry in vars.get(HOSTS_VAR).unwrap_or_default().split([',', ' ', '\t', '\n']) {
            if !entry.is_empty() {
                hosts.push(host_and_port(entry).ok_or_else(|| {
                    format!("{HOSTS_VAR}: {entry:?} is no host, nor a host and its port")
                })?);
            }
        }
        // The host's own KMS's host: of an OpenBao or a vault, which a URL names by its host.
        let own = vars.get(KMS_VAR).and_then(|own| {
            // No URL: none of its host, the host's KMS refusing it as it is made.
            let parsed = url::Url::parse(own).ok()?;
            let named = matches!(parsed.scheme(), "openbao" | "openbaos" | "azure-kv");
            Some((named.then(|| parsed.host_str())??.to_owned(), None))
        });
        hosts.extend(own);
        let local = match vars.get(LOCAL_VAR) {
            None => false,
            Some("1") => true,
            Some(other) => return Err(format!("{LOCAL_VAR}: {other:?}, not 1")),
        };
        Ok(Self { hosts, local })
    }

    /// Whether it allows `host`, at `port`.
    fn allows(&self, host: &str, port: Option<u16>) -> bool {
        self.hosts.iter().any(|(allowed, at)| {
            allowed.eq_ignore_ascii_case(host) && (at.is_none() || *at == port)
        })
    }
}

impl Origin<'_> {
    /// Refuses `what`, a file of the host's a recorded URL names (`keys=FILE`, `ca=FILE`), before
    /// it is opened: the file's being there, or what it holds, said nowhere.
    pub(super) fn file(&self, what: &str) -> Result<(), String> {
        match self {
            Origin::Host => Ok(()),
            Origin::Recorded(_) => Err(format!(
                "{what} names a file of this host's: a KMS URL the registry of buckets records may \
                 not; {KMS_VAR}'s alone may"
            )),
        }
    }

    /// Refuses, of a recorded URL, the host `host` at `port` (a vault's, an OpenBao's) unless
    /// [`HOSTS_VAR`] or the host's own KMS names it.
    pub(super) fn host(&self, host: &str, port: Option<u16>) -> Result<(), String> {
        match self {
            Origin::Recorded(trust) if !trust.allows(host, port) => {
                let at = port.map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
                Err(format!(
                    "{at}: a KMS URL the registry of buckets records reaches only the hosts \
                     {HOSTS_VAR} names, or the host's own {KMS_VAR}'s"
                ))
            }
            _ => Ok(()),
        }
    }

    /// Refuses, of a recorded URL, cleartext to `host` unless it is this host's (loopback): the
    /// host's token would cross the network in the clear.
    pub(super) fn cleartext(&self, host: &str) -> Result<(), String> {
        match self {
            Origin::Recorded(_) if !loopback(host) => Err(format!(
                "{host}: a KMS URL the registry of buckets records reaches another host over TLS \
                 alone (openbaos://)"
            )),
            _ => Ok(()),
        }
    }

    /// Refuses, of a recorded URL, `local://` unless [`LOCAL_VAR`] allows it: a fake for tests,
    /// which makes a directory of the host's.
    pub(super) fn local(&self) -> Result<(), String> {
        match self {
            Origin::Recorded(trust) if !trust.local => Err(format!(
                "local:// is a fake for tests: a KMS URL the registry of buckets records may be one \
                 only if {LOCAL_VAR} is 1"
            )),
            _ => Ok(()),
        }
    }

    /// Refuses, of a recorded URL, an AWS KMS endpoint `endpoint` but AWS's own over HTTPS (a host
    /// of `amazonaws.com`, a VPC endpoint's among them) or one of this host's (an emulator).
    pub(super) fn aws_endpoint(&self, endpoint: &str) -> Result<(), String> {
        let Origin::Recorded(_) = self else { return Ok(()) };
        let refused = || {
            format!(
                "the endpoint {endpoint:?}: a KMS URL the registry of buckets records reaches AWS \
                 over HTTPS (https://...amazonaws.com), or this host, alone"
            )
        };
        let parsed = url::Url::parse(endpoint).map_err(|_| refused())?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(refused());
        }
        let host = parsed.host_str().ok_or_else(refused)?.to_ascii_lowercase();
        let aws = [".amazonaws.com", ".amazonaws.com.cn"]
            .iter()
            .any(|domain| host.strip_suffix(domain).is_some_and(|name| !name.is_empty()));
        match (parsed.scheme(), aws || loopback(&host)) {
            ("https", true) => Ok(()),
            ("http", _) if loopback(&host) => Ok(()),
            _ => Err(refused()),
        }
    }
}

/// Whether `host` is this host's: `localhost`, or a loopback address (`127.0.0.0/8`, `::1`).
fn loopback(host: &str) -> bool {
    let address = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || address.parse::<IpAddr>().is_ok_and(|address| address.is_loopback())
}

/// The host and port `entry` names, `HOST`, `HOST:PORT` or `[V6]:PORT`: none if its port is none.
fn host_and_port(entry: &str) -> Option<(String, Option<u16>)> {
    if entry.ends_with(']') || entry.matches(':').count() > 1 && !entry.starts_with('[') {
        return Some((entry.to_owned(), None));
    }
    match entry.rsplit_once(':') {
        // A port that is none: the entry refused, never taken for a host alone.
        Some((host, port)) if !host.is_empty() => Some((host.to_owned(), Some(port.parse().ok()?))),
        Some(_) => None,
        None => Some((entry.to_owned(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::from_url_with;

    /// The variables of the tests' host: its hosts allowed, its own KMS, a token of each backend's.
    fn host_vars(local: Option<&str>) -> Vars {
        let mut vars = vec![
            (HOSTS_VAR, "vault.example.com, 10.0.0.5:8200 [::1]:8201\tv.vault.azure.net"),
            (KMS_VAR, "openbaos://own.example.com:8200/transit?key=k{tenant}"),
            (crate::kms::openbao::TOKEN_ENV, "s.token"),
            (crate::kms::GCP_TOKEN_VAR, "t"),
            (crate::kms::AZURE_TOKEN_VAR, "t"),
        ];
        vars.extend(local.map(|value| (LOCAL_VAR, value)));
        Vars::new(vars.into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())))
    }

    /// board 8525 AC #3: a KMS URL the registry records reaches what the host allows alone, each
    /// other refused as its backend reads it, before it opens a file or a connection, its refusal
    /// echoing no file's content nor saying whether the file is there: a host's file named
    /// (`keys`, `ca`), `local://` (its directory never made), an OpenBao or a vault the host names
    /// not (by host, by port, by case, by suffix), cleartext but to this host, an AWS endpoint not
    /// AWS's own over HTTPS nor this host's. The same URLs, the host's own, are taken.
    #[test]
    fn recorded_urls_reach_what_the_host_allows_alone() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("token");
        std::fs::write(&secret, "s.hosts-secret-token\n").unwrap();
        let (secret, missing) = (secret.display(), dir.path().join("missing"));
        let missing = missing.display();
        let local = dir.path().join("kms");
        let vars = host_vars(None);
        let trust = Trust::of(&vars).unwrap();
        let recorded = |url: &str| from_url_with(url, &vars, Origin::Recorded(&trust)).map(drop);
        let bao = |at: &str| format!("{at}/transit?key=k{{tenant}}");
        let aws =
            |endpoint: &str| format!("aws-kms://us-east-1?key=k{{tenant}}&endpoint={endpoint}");
        let gcp = "gcp-kms://projects/p/locations/l/keyRings/r?key=k-{tenant}";
        let not_allowed = "a KMS URL the registry of buckets records reaches only the hosts";
        let cleartext = "reaches another host over TLS alone";
        let file = "names a file of this host's: a KMS URL the registry of buckets records may not";
        let not_aws = "reaches AWS over HTTPS (https://...amazonaws.com), or this host, alone";
        let refused = [
            (bao("openbaos://10.0.0.6:8200"), not_allowed),
            (bao("openbaos://10.0.0.5:8201"), not_allowed),
            (bao("openbaos://10.0.0.5:0"), not_allowed),
            (bao("openbaos://10.0.0.5:65535"), not_allowed),
            (bao("openbaos://vault.example.com.evil.example"), not_allowed),
            (bao("openbaos://evil-vault.example.com"), not_allowed),
            (bao("openbao://127.0.0.1:8200"), not_allowed),
            (bao("openbao://[::1]:8200"), not_allowed),
            (bao("openbao://10.0.0.5:8200"), cleartext),
            (bao("openbao://vault.example.com"), cleartext),
            (bao("openbao://own.example.com"), cleartext),
            (format!("{}&keys={secret}", bao("openbaos://vault.example.com")), file),
            (format!("{}&keys={missing}", bao("openbaos://vault.example.com")), file),
            (format!("{}&ca={secret}", bao("openbaos://vault.example.com")), file),
            (format!("{}&ca={missing}", bao("openbaos://vault.example.com")), file),
            (format!("{gcp}&keys={secret}"), file),
            (format!("{gcp}&keys={missing}"), file),
            (format!("{}&keys={secret}", aws("https://kms.us-east-1.amazonaws.com")), file),
            (aws("http://169.254.169.254"), not_aws),
            (aws("http://169.254.169.254/latest/meta-data"), not_aws),
            (aws("https://evil.example"), not_aws),
            (aws("http://kms.us-east-1.amazonaws.com"), not_aws),
            (aws("https://amazonaws.com"), not_aws),
            (aws("https://kms.us-east-1.amazonaws.com.evil.example"), not_aws),
            (aws("https://kms.us-east-1.amazonaws.com."), not_aws),
            (aws("https://user@kms.us-east-1.amazonaws.com"), not_aws),
            (aws("http://128.0.0.1"), not_aws),
            (aws("http://0.0.0.0:9"), not_aws),
            (aws("ftp://127.0.0.1"), not_aws),
            (aws("127.0.0.1:9"), not_aws),
            ("azure-kv://attacker.vault.azure.net?key=k-{tenant}".into(), not_allowed),
            (
                "azure-kv://v.vault.azure.net?key=k-{tenant}&endpoint=https://attacker.vault.azure.net"
                    .into(),
                not_allowed,
            ),
            (
                "azure-kv://v.vault.azure.net?key=k-{tenant}&endpoint=http://127.0.0.1:8080".into(),
                not_allowed,
            ),
            (
                "azure-kv://v.vault.azure.net?key=https://attacker.vault.azure.net/keys/k-{tenant}"
                    .into(),
                not_allowed,
            ),
            (
                "azure-kv://v.vault.azure.net?key=https://v{tenant}.vault.azure.net/keys/k".into(),
                not_allowed,
            ),
            (format!("azure-kv://v.vault.azure.net?key=k-{{tenant}}&keys={secret}"), file),
            (format!("local://{}", local.display()), "local:// is a fake for tests"),
        ];
        for (url, why) in &refused {
            let refusal = recorded(url).err().unwrap_or_else(|| panic!("{url}: taken"));
            assert!(refusal.contains(why), "{url}: {refusal}");
            assert!(!refusal.contains("hosts-secret-token"), "{url}: {refusal}");
            assert!(!refusal.contains("No such file"), "{url}: {refusal}");
        }
        assert!(!local.exists(), "local:// made its directory");
        let allowed = [
            bao("openbaos://vault.example.com"),
            bao("openbaos://VAULT.example.COM:8300"),
            bao("openbaos://10.0.0.5:8200"),
            // OpenBao's port, 8200, as the host allows it.
            bao("openbaos://10.0.0.5"),
            bao("openbaos://own.example.com:9200"),
            bao("openbao://[::1]:8201"),
            gcp.to_owned(),
            "aws-kms://us-east-1?key=k{tenant}".to_owned(),
            aws("https://kms.us-east-1.amazonaws.com"),
            aws("https://vpce-1-a.kms.us-east-1.vpce.amazonaws.com"),
            aws("https://kms.cn-north-1.amazonaws.com.cn"),
            aws("http://127.0.0.1:9"),
            aws("http://localhost:9"),
            aws("http://[::1]:9"),
            "azure-kv://v.vault.azure.net?key=k-{tenant}".to_owned(),
            "azure-kv://v.vault.azure.net?key=https://v.vault.azure.net/keys/k-{tenant}".to_owned(),
        ];
        for url in &allowed {
            recorded(url).unwrap_or_else(|why| panic!("{url}: {why}"));
        }
        // The host's own takes them, its files read: a line refused names its number alone.
        let hosts = refused.iter().filter(|(url, why)| url.starts_with("openbao") && *why != file);
        for (url, _) in hosts {
            from_url_with(url, &vars, Origin::Host).unwrap_or_else(|why| panic!("{url}: {why}"));
        }
        let keys = format!("{}&keys={secret}", bao("openbaos://vault.example.com"));
        let own = from_url_with(&keys, &vars, Origin::Host).map(drop).unwrap_err();
        assert!(own.ends_with("the key overrides' line 1: not a tenant and a key's name"), "{own}");
        // Allowed, `local://` makes its directory.
        let vars = host_vars(Some("1"));
        let trust = Trust::of(&vars).unwrap();
        let url = format!("local://{}", local.display());
        from_url_with(&url, &vars, Origin::Recorded(&trust)).map(drop).unwrap();
        assert!(local.exists());
    }

    /// What the host allows is read whole or refused: a host of [`HOSTS_VAR`] with a port that is
    /// none, or a [`LOCAL_VAR`] not `1`, is never taken for allowing less.
    #[test]
    fn trust_misnamed_is_refused() {
        let trust = |pairs: &[(&str, &str)]| {
            let vars = pairs.iter().map(|(name, value)| (name.to_string(), value.to_string()));
            Trust::of(&Vars::new(vars)).map(|trust| trust.hosts)
        };
        assert_eq!(
            trust(&[(HOSTS_VAR, "h:0,h:65535 [::1] [::1]:1")]),
            Ok(vec![
                ("h".into(), Some(0)),
                ("h".into(), Some(65535)),
                ("[::1]".into(), None),
                ("[::1]".into(), Some(1)),
            ])
        );
        for hosts in ["h:", "h:65536", ":8200", "h:x", "[::1]:"] {
            assert_eq!(
                trust(&[(HOSTS_VAR, hosts)]),
                Err(format!("{HOSTS_VAR}: {hosts:?} is no host, nor a host and its port"))
            );
        }
        for local in ["0", "true", "yes", " 1"] {
            assert_eq!(trust(&[(LOCAL_VAR, local)]), Err(format!("{LOCAL_VAR}: {local:?}, not 1")));
        }
        assert_eq!(
            trust(&[(KMS_VAR, "gcp-kms://projects/p/locations/l/keyRings/r?key=k")]),
            Ok(vec![])
        );
    }
}
