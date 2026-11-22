// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how a real KMS's URL names each tenant's KEK ([`KeyNames`]): what every real
//! backend shares (TASK-411.16, .17, .18).

use std::collections::HashMap;
use std::path::Path;

/// What stands for a tenant's ID in a [`KeyNames`] template.
const TENANT: &str = "{tenant}";

/// The name a real KMS knows each tenant's KEK by: a template, in which `{tenant}` stands for the
/// tenant's ID in base 32, as cladefs displays tenants (`TENANT/CELL`, `base32_id`); and overrides, for
/// tenants that bring their own key (BYOK) under a name of their choosing, perhaps in an account
/// of their own (an AWS key ARN, a GCP key's resource name). A KMS URL gives the template as its
/// `key` and the file of overrides as its `keys`: lines of a tenant in base 32 and its key's
/// name, separated by white space, `#` beginning a comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyNames {
    template: String,
    overrides: HashMap<u64, String>,
}

impl KeyNames {
    /// The names `template` and `overrides`, a file's contents, give. A template must hold
    /// `{tenant}`, or every tenant would share one KEK, and shredding one would shred all.
    pub fn new(template: &str, overrides: Option<&str>) -> Result<Self, String> {
        if !template.contains(TENANT) {
            return Err(format!(
                "the key template {template:?} holds no {TENANT}: every tenant would share one KEK"
            ));
        }
        let mut names = HashMap::new();
        for (number, line) in overrides.unwrap_or_default().lines().enumerate() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let at = |why: &str| format!("the key overrides' line {}: {why}: {line:?}", number + 1);
            let mut words = line.split_whitespace();
            let (Some(tenant), Some(name), None) = (words.next(), words.next(), words.next())
            else {
                return Err(at("not a tenant and a key's name"));
            };
            let tenant = base32_id::parse(tenant).ok_or_else(|| at("no tenant in base 32"))?;
            if names.insert(tenant, name.to_owned()).is_some() {
                return Err(at("a tenant named twice"));
            }
        }
        Ok(Self { template: template.to_owned(), overrides: names })
    }

    /// The names a KMS URL's query gives: its `key`, the template, and its `keys`, the path of a
    /// file of overrides, read now.
    pub fn of_query<'a>(
        query: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let (mut template, mut overrides) = (None, None);
        for (name, value) in query {
            match name {
                "key" => template = Some(value),
                "keys" => overrides = Some(value),
                _ => {}
            }
        }
        let template = template.ok_or("the KMS URL names no key template: key=...{tenant}...")?;
        let overrides = match overrides {
            Some(path) => Some(
                std::fs::read_to_string(Path::new(path))
                    .map_err(|err| format!("the key overrides {path:?}: {err}"))?,
            ),
            None => None,
        };
        Self::new(template, overrides.as_deref())
    }

    /// The name of `tenant`'s KEK.
    pub fn of(&self, tenant: u64) -> String {
        match self.overrides.get(&tenant) {
            Some(name) => name.clone(),
            None => self.template.replace(TENANT, &base32_id::Short(tenant).to_string()),
        }
    }

    /// The tenants overridden and their keys' names, in no order: for a backend to check each is
    /// a name its KMS has, before any call (TASK-411.18).
    pub fn overridden(&self) -> impl Iterator<Item = (u64, &str)> {
        self.overrides.iter().map(|(&tenant, name)| (tenant, name.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_name_every_tenant_apart() {
        let refused = KeyNames::new("cladefs", None);
        let why =
            "the key template \"cladefs\" holds no {tenant}: every tenant would share one KEK";
        assert_eq!(refused, Err(why.to_owned()));
    }
}
