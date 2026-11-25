// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: the variables a host's KMSes are configured by ([`Vars`], board 8485, board
//! 8593): which KMS, the bucket KMS's master keys, what a KMS URL the registry of buckets records
//! may reach ([`super::trust`]), and each real KMS's credentials, read from these alone, never from
//! the process's environment behind them. A bucket's KMS reads the bucket's own
//! (`CLADEFSD_BUCKET_<N>_NAME`) over the host's, a group of credentials at a time
//! ([`Vars::numbered`]), so that two buckets may each have a KMS of their own with credentials of
//! their own, and none mixes its own with the host's (hotbed question 476).

use super::master::{MASTER_KEY_FILE_VAR, MASTER_KEY_VAR};
use super::{azure, gcp, openbao, trust};
use std::sync::Arc;
use zeroize::{Zeroize as _, Zeroizing};

/// The variable naming a host's KMS by its URL ([`super::from_url`]), which takes it over a master
/// key.
pub const KMS_VAR: &str = "CLADEFS_KMS";

/// AWS credential variables replaced as a group by a numbered bucket (board 8485, board 9684).
pub const AWS_CREDENTIAL_VARS: &[&str] = super::aws::CREDENTIAL_VARS;

/// GCP credential variables replaced as a group by a numbered bucket (board 8485, board 9684).
pub const GCP_CREDENTIAL_VARS: &[&str] = gcp::CREDENTIAL_VARS;

/// Azure credential variables replaced as a group by a numbered bucket (board 8485, board 9684).
pub const AZURE_CREDENTIAL_VARS: &[&str] = azure::CREDENTIAL_VARS;

/// The variables a bucket's own replace the host's of as one: whichever of them a bucket sets, it
/// takes none of the host's (board 8485 AC #4). The KMS's choice, then each real KMS's credentials,
/// as its backend reads them: so that a bucket's own key id never meets the host's secret, nor a
/// bucket's own KMS the host's master key.
const GROUPS: [&[&str]; 5] = [
    &[KMS_VAR, MASTER_KEY_VAR, MASTER_KEY_FILE_VAR],
    &[openbao::TOKEN_ENV, openbao::TOKEN_FILE_ENV],
    GCP_CREDENTIAL_VARS,
    AZURE_CREDENTIAL_VARS,
    AWS_CREDENTIAL_VARS,
];

/// The variables read besides: what a recorded KMS URL may reach ([`trust`]), each its own.
const OTHERS: [&str; 4] =
    [trust::HOSTS_VAR, trust::LOCAL_VAR, trust::GCP_PROJECTS_VAR, trust::AWS_ACCOUNTS_VAR];

/// What starts each variable of a numbered bucket's own, before its number.
const NUMBERED: &str = "CLADEFSD_BUCKET_";

/// Variables a host's KMSes read, by name, each value zeroed when dropped; the process's own, or a
/// bucket's view of them ([`Vars::numbered`]). Printed by their names alone.
pub struct Vars {
    vars: Vec<(String, Zeroizing<String>)>,
    /// Whether they are the process's environment, or a bucket's view of it, as what reads that
    /// alike reads them (AWS's default credential chain).
    process: bool,
    /// The groups a bucket's view took of its own ([`GROUPS`], by place): what the process's
    /// environment says of them is not its.
    own: Vec<usize>,
}

impl Vars {
    /// The process's environment, of it those variables a KMS reads, a bucket's own among them
    /// ([`kept`]): read once, by whoever asks, and shared (board 8593). One of them not text is
    /// refused, naming it, never taken for unset.
    pub fn from_env() -> Result<Self, String> {
        let mut vars = Vec::new();
        for (name, value) in std::env::vars_os() {
            // A name not text is none of these.
            let Some(name) = name.to_str().filter(|name| kept(name)) else { continue };
            let value = value.into_string().map_err(|value| {
                value.into_encoded_bytes().zeroize();
                format!("{name}: not text")
            })?;
            vars.push((name.to_owned(), Zeroizing::new(value)));
        }
        Ok(Self { vars, process: true, own: Vec::new() })
    }

    /// `vars`, none of the process's: tests'.
    pub fn new(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let vars = vars.into_iter().map(|(name, value)| (name, Zeroizing::new(value))).collect();
        Self { vars, process: false, own: Vec::new() }
    }

    /// `vars`, as if the process's environment: tests'.
    #[cfg(test)]
    pub(super) fn as_process(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { process: true, ..Self::new(vars) }
    }

    /// The value of `name`, if set and not empty, the last if it is there twice.
    pub fn get(&self, name: &str) -> Option<&str> {
        let found = self.vars.iter().rev().find(|(of, _)| of == name);
        found.map(|(_, value)| value.as_str()).filter(|value| !value.is_empty())
    }

    /// Whether any of `names` is set.
    pub(super) fn any(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.get(name).is_some())
    }

    /// Whether what they say of `group`, one of [`GROUPS`], is what the process's environment says
    /// of it, which readers of it read alike: theirs is the process's ([`Vars::from_env`]), or a
    /// bucket's view of it that took none of `group` of its own.
    pub(super) fn reads_process(&self, group: &[&str]) -> bool {
        self.process && !self.owns(group)
    }

    /// Whether, a bucket's view, they took `group`, one of [`GROUPS`], of the bucket's own: set
    /// empty too.
    pub(super) fn owns(&self, group: &[&str]) -> bool {
        self.own.iter().any(|&at| GROUPS[at] == group)
    }

    /// The variables bucket `number`'s KMS reads (board 8485): each `CLADEFSD_BUCKET_<number>_NAME`
    /// as `NAME`, over `NAME` itself; and of each group of variables taken as one ([`GROUPS`]), the
    /// bucket's own alone if it sets any. None of other buckets'.
    pub fn numbered(&self, number: u16) -> Vars {
        let prefix = format!("{NUMBERED}{number}_");
        let own: Vec<(String, Zeroizing<String>)> = (self.vars.iter())
            .filter_map(|(name, value)| {
                Some((name.strip_prefix(&prefix)?.to_owned(), value.clone()))
            })
            .collect();
        let replaced = |name: &str| own.iter().any(|(theirs, _)| together(theirs, name));
        let host = (self.vars.iter())
            .filter(|(name, _)| unnumbered(name) == name.as_str() && !replaced(name))
            .map(|(name, value)| (name.clone(), value.clone()));
        let groups = (0..GROUPS.len())
            .filter(|&at| own.iter().any(|(name, _)| GROUPS[at].contains(&name.as_str())));
        let vars = host.chain(own.iter().cloned()).collect();
        Vars { vars, process: self.process, own: groups.collect() }
    }

    /// Whether bucket `number` sets credentials of its own for the KMS `url` names: the variables
    /// its backend reads them by. Its KMS is then one of its own, not shared with another bucket's
    /// naming the same URL (board 8485 AC #2).
    pub fn own_credentials(&self, number: u16, url: &str) -> bool {
        let Some(group) = credentials_of(url) else { return false };
        let prefix = format!("{NUMBERED}{number}_");
        // Set empty, its own too: it takes the host's no more ([`Vars::numbered`]).
        (self.vars.iter())
            .filter_map(|(name, _)| name.strip_prefix(&prefix))
            .any(|name| group.contains(&name))
    }

    /// Them, shared: what a host keeps, once (board 8593).
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl std::fmt::Debug for Vars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.vars.iter().map(|(name, _)| name)).finish()
    }
}

/// Whether the process's variable `name` is one a KMS reads, its own or a numbered bucket's.
fn kept(name: &str) -> bool {
    let name = unnumbered(name);
    GROUPS.iter().any(|group| group.contains(&name)) || OTHERS.contains(&name)
}

/// `name` without a numbered bucket's prefix (`CLADEFSD_BUCKET_<digits>_`), if it has one.
pub fn unnumbered(name: &str) -> &str {
    let numbered = name.strip_prefix(NUMBERED).and_then(|rest| {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0).then(|| rest[digits..].strip_prefix('_')).flatten()
    });
    numbered.unwrap_or(name)
}

/// Whether a bucket's own `ours` replaces the host's `theirs`: the same variable, or of one group.
fn together(ours: &str, theirs: &str) -> bool {
    ours == theirs || GROUPS.iter().any(|group| group.contains(&ours) && group.contains(&theirs))
}

/// The credentials' variables of the KMS `url` names, by its scheme: none for one that takes none.
fn credentials_of(url: &str) -> Option<&'static [&'static str]> {
    match url.split_once("://")?.0 {
        "openbao" | "openbaos" => Some(GROUPS[1]),
        "gcp-kms" => Some(gcp::CREDENTIAL_VARS),
        "azure-kv" => Some(azure::CREDENTIAL_VARS),
        "aws-kms" => Some(super::aws::CREDENTIAL_VARS),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        Vars::new(pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())))
    }

    fn named(vars: &Vars) -> Vec<(String, String)> {
        let mut named: Vec<_> =
            vars.vars.iter().map(|(name, value)| (name.clone(), value.to_string())).collect();
        named.sort();
        named
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut pairs: Vec<_> = pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        pairs.sort();
        pairs
    }

    /// board 8593: the process's variables kept are those a KMS reads, its own or a numbered
    /// bucket's, by exactly that prefix: digits, then `_`.
    #[test]
    fn only_the_variables_kmses_read_are_kept() {
        for name in [
            "CLADEFS_KMS",
            "CLADEFS_MASTER_KEY",
            "CLADEFS_KMS_OPENBAO_TOKEN_FILE",
            "AWS_SECRET_ACCESS_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AZURE_CLIENT_SECRET",
            "CLADEFSD_KMS_HOSTS",
            "CLADEFSD_KMS_GCP_PROJECTS",
            "CLADEFSD_KMS_AWS_ACCOUNTS",
            "CLADEFSD_BUCKET_0_CLADEFS_KMS",
            "CLADEFSD_BUCKET_2_AWS_ACCESS_KEY_ID",
            "CLADEFSD_BUCKET_65535_CLADEFS_KMS_AZURE_TOKEN",
            "CLADEFSD_BUCKET_99999999999999999999_CLADEFS_KMS",
        ] {
            assert!(kept(name), "{name}");
        }
        for name in [
            "",
            "PATH",
            "HOME",
            "AWS_REGION",
            "AZURE_STORAGE_ACCOUNT_KEY",
            "GOOGLE_BUCKET",
            "CLADEFS_KMSX",
            "CLADEFSD_BUCKET",
            "CLADEFSD_BUCKET_",
            "CLADEFSD_BUCKET__CLADEFS_KMS",
            "CLADEFSD_BUCKET_2CLADEFS_KMS",
            "CLADEFSD_BUCKET_2x_CLADEFS_KMS",
            "CLADEFSD_BUCKET_-1_CLADEFS_KMS",
            "CLADEFSD_BUCKET_2_AWS_REGION",
            "CLADEFSD_BUCKET_2_CLADEFSD_BUCKET_3_CLADEFS_KMS",
        ] {
            assert!(!kept(name), "{name}");
        }
    }

    /// board 8485 AC #4: a bucket setting any variable of a group takes none of the host's of that
    /// group, its credentials never mixed with the host's: its own key id, the host's session
    /// token; a variable of no group it sets replaces the host's alone. Other buckets' are not
    /// read.
    #[test]
    fn buckets_take_a_group_of_their_own_or_the_hosts() {
        let host = vars(&[
            ("CLADEFS_KMS", "openbao://h/m?key=k{tenant}"),
            ("CLADEFS_MASTER_KEY_FILE", "/host/master"),
            ("CLADEFS_KMS_OPENBAO_TOKEN", "host-token"),
            ("AWS_ACCESS_KEY_ID", "host-id"),
            ("AWS_SECRET_ACCESS_KEY", "host-secret"),
            ("AWS_SESSION_TOKEN", "host-session"),
            ("CLADEFSD_KMS_HOSTS", "h"),
            ("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY", "two"),
            ("CLADEFSD_BUCKET_2_AWS_ACCESS_KEY_ID", "two-id"),
            ("CLADEFSD_BUCKET_2_AWS_SECRET_ACCESS_KEY", "two-secret"),
            ("CLADEFSD_BUCKET_2_CLADEFSD_KMS_HOSTS", "two-hosts"),
            ("CLADEFSD_BUCKET_3_CLADEFS_KMS_OPENBAO_TOKEN_FILE", "/three/token"),
        ]);
        assert_eq!(
            named(&host.numbered(2)),
            pairs(&[
                ("CLADEFS_MASTER_KEY", "two"),
                ("CLADEFS_KMS_OPENBAO_TOKEN", "host-token"),
                ("AWS_ACCESS_KEY_ID", "two-id"),
                ("AWS_SECRET_ACCESS_KEY", "two-secret"),
                ("CLADEFSD_KMS_HOSTS", "two-hosts"),
            ])
        );
        assert_eq!(
            named(&host.numbered(3)),
            pairs(&[
                ("CLADEFS_KMS", "openbao://h/m?key=k{tenant}"),
                ("CLADEFS_MASTER_KEY_FILE", "/host/master"),
                ("CLADEFS_KMS_OPENBAO_TOKEN_FILE", "/three/token"),
                ("AWS_ACCESS_KEY_ID", "host-id"),
                ("AWS_SECRET_ACCESS_KEY", "host-secret"),
                ("AWS_SESSION_TOKEN", "host-session"),
                ("CLADEFSD_KMS_HOSTS", "h"),
            ])
        );
        let url = "openbao://h/m?key=k{tenant}";
        assert!(host.own_credentials(3, url) && !host.own_credentials(2, url));
        assert!(host.own_credentials(2, "aws-kms://us-east-1?key=k{tenant}"));
        assert!(!host.own_credentials(2, "local:///kms") && !host.own_credentials(4, url));
        let (two, three) = (host.numbered(2), host.numbered(3));
        assert!(two.owns(GROUPS[0]) && two.owns(GROUPS[4]) && !two.owns(GROUPS[1]));
        assert!(three.owns(GROUPS[1]) && !three.owns(GROUPS[0]) && !three.owns(GROUPS[4]));
        assert!(!two.reads_process(GROUPS[1]), "not the process's: Vars::new's");
    }
}
