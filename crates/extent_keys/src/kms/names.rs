// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how a real KMS's URL names each tenant's KEK ([`KeyNames`]): what every real
//! backend shares (TASK-411.16, .17, .18); and the overrides' reload (board 8487).

use super::Origin;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};

/// What stands for a tenant's ID in a [`KeyNames`] template.
const TENANT: &str = "{tenant}";

/// A backend's check of a key's name: why a name is none its KMS has.
pub type Check = fn(&str) -> Result<(), String>;

/// Overrides of tenants' keys' names, by tenant.
type Overrides = HashMap<u64, String>;

/// Shared cell holding a `keys=FILE`'s current overrides map.
type OverridesCell = Arc<RwLock<Arc<Overrides>>>;

/// cladefs: per-host registry of `keys=FILE` override cells, keyed by path (board 10466): every
/// KMS instance on a host naming the same `keys=FILE` shares one cell, read once on first use and
/// changed only by reload. Weak references keep no stale entry if a construction fails or drops.
#[derive(Default)]
pub struct OverrideFiles {
    cells: Mutex<HashMap<PathBuf, Weak<RwLock<Arc<Overrides>>>>>,
}

impl OverrideFiles {
    /// Registers `names`'s file cell so later KMS instances on the same host naming the same file
    /// share it without re-reading disk.
    pub fn register(&self, names: &KeyNames) {
        if let Some(file) = &names.file {
            self.cells.lock().unwrap().insert(file.clone(), Arc::downgrade(&names.overrides));
        }
    }

    /// Returns the shared cell for `path`, validating its current entries against `check`, or
    /// reads and parses `path` once if no live KMS instance holds it yet.
    fn get_or_load(&self, path: &Path, check: Check) -> Result<OverridesCell, String> {
        let mut cells = self.cells.lock().unwrap();
        if let Some(cell) = cells.get(path).and_then(Weak::upgrade) {
            let current = cell.read().unwrap().clone();
            for (&tenant, name) in current.iter() {
                check(name).map_err(|why| {
                    format!("the key overrides: tenant {}'s key, {why}", base32_id::Short(tenant))
                })?;
            }
            return Ok(cell);
        }
        let overrides = parse(&read(path)?, check)?;
        let cell = Arc::new(RwLock::new(Arc::new(overrides)));
        cells.insert(path.to_path_buf(), Arc::downgrade(&cell));
        Ok(cell)
    }
}

/// The name a real KMS knows each tenant's KEK by: a template, in which `{tenant}` stands for the
/// tenant's ID in base 32, as cladefs displays tenants (`TENANT/CELL`, `base32_id`); and overrides, for
/// tenants that bring their own key (BYOK) under a name of their choosing, perhaps in an account
/// of their own (an AWS key ARN, a GCP key's resource name). A KMS URL gives the template as its
/// `key` and the file of overrides as its `keys`: lines of a tenant in base 32 and its key's
/// name, separated by white space, `#` beginning a comment.
///
/// cladefs: the template is the URL's, for the process's life; the overrides are the file's as it
/// was read last, at the start or by a reload ([`KeyNames::reread`], board 8487), which every clone
/// shares: a backend's calls see a reload's overrides from the next on.
#[derive(Clone, Debug)]
pub struct KeyNames {
    template: String,
    /// The file `keys` named, if any: what a reload reads again.
    file: Option<PathBuf>,
    /// What every override's name must pass: the backend's check.
    check: Check,
    overrides: Arc<RwLock<Arc<Overrides>>>,
}

impl PartialEq for KeyNames {
    fn eq(&self, other: &Self) -> bool {
        self.template == other.template
            && self.file == other.file
            && *self.current() == *other.current()
    }
}

impl Eq for KeyNames {}

impl KeyNames {
    /// The names `template` and `overrides`, a file's contents, give. A template must hold
    /// `{tenant}`, or every tenant would share one KEK, and shredding one would shred all.
    pub fn new(template: &str, overrides: Option<&str>) -> Result<Self, String> {
        Self::checked(template, None, overrides, |_| Ok(()))
    }

    /// The same, read from `file`, if any, every override's name passing `check`.
    fn checked(
        template: &str,
        file: Option<PathBuf>,
        overrides: Option<&str>,
        check: Check,
    ) -> Result<Self, String> {
        if !template.contains(TENANT) {
            return Err(format!(
                "the key template {template:?} holds no {TENANT}: every tenant would share one KEK"
            ));
        }
        let overrides = parse(overrides.unwrap_or_default(), check)?;
        let overrides = Arc::new(RwLock::new(Arc::new(overrides)));
        Ok(Self { template: template.to_owned(), file, check, overrides })
    }

    /// The names a KMS URL's query gives: its `key`, the template, and its `keys`, the path of a
    /// file of overrides, read now, each override's name passing `check`, the backend's; refused
    /// unread if the URL is of `origin` the registry of buckets (board 8525).
    pub fn of_query<'a>(
        query: impl IntoIterator<Item = (&'a str, &'a str)>,
        origin: Origin,
        check: Check,
    ) -> Result<Self, String> {
        Self::of_query_in(query, origin, check, None)
    }

    /// cladefs: like [`KeyNames::of_query`], sharing the overrides cell in `files` when another
    /// KMS on the same host has already loaded the same `keys=FILE` (board 10466).
    pub fn of_query_in<'a>(
        query: impl IntoIterator<Item = (&'a str, &'a str)>,
        origin: Origin,
        check: Check,
        files: Option<&OverrideFiles>,
    ) -> Result<Self, String> {
        let (mut template, mut overrides) = (None, None);
        for (name, value) in query {
            match name {
                "key" => template = Some(value),
                "keys" => overrides = Some(value),
                _ => {}
            }
        }
        if overrides.is_some() {
            origin.file("keys=FILE")?;
        }
        let template = template.ok_or("the KMS URL names no key template: key=...{tenant}...")?;
        if !template.contains(TENANT) {
            return Err(format!(
                "the key template {template:?} holds no {TENANT}: every tenant would share one KEK"
            ));
        }
        let file = overrides.map(PathBuf::from);
        let overrides = match (&file, files) {
            (Some(path), Some(files)) => files.get_or_load(path, check)?,
            (Some(path), None) => {
                let parsed = parse(&read(path)?, check)?;
                Arc::new(RwLock::new(Arc::new(parsed)))
            }
            (None, _) => Arc::new(RwLock::new(Arc::new(HashMap::new()))),
        };
        Ok(Self { template: template.to_owned(), file, check, overrides })
    }

    /// The overrides file `keys` named, if any.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// The template, `{tenant}` in it.
    pub fn template(&self) -> &str {
        &self.template
    }

    /// The name of `tenant`'s KEK.
    pub fn of(&self, tenant: u64) -> String {
        named(&self.template, &self.current(), tenant)
    }

    /// The overrides in use now.
    fn current(&self) -> Arc<Overrides> {
        self.overrides.read().unwrap().clone()
    }

    /// cladefs: its `keys` file read again, checked as at the start, not yet in use (board 8487):
    /// the tenants whose KEK's name it changes, and [`Staged::apply`] to put it in use. None if
    /// the URL named no file. A file not to be read, or with a line that is no override, is
    /// refused naming the line by its number alone, as at the start, and nothing changes.
    pub fn reread(&self) -> Result<Option<Staged>, String> {
        let Some(file) = &self.file else { return Ok(None) };
        self.reread_text(&read(file)?).map(Some)
    }

    /// cladefs: stages overrides already read from its `keys` file (`text`), so a reload over
    /// multiple KMS instances sharing one file reads the file once (board 10466).
    pub fn reread_text(&self, text: &str) -> Result<Staged, String> {
        let overrides = parse(text, self.check)?;
        let current = self.current();
        let tenants: BTreeSet<u64> = current.keys().chain(overrides.keys()).copied().collect();
        let renamed = (tenants.into_iter())
            .filter(|&tenant| {
                named(&self.template, &current, tenant) != named(&self.template, &overrides, tenant)
            })
            .collect();
        Ok(Staged { names: self.clone(), from: current, overrides: Arc::new(overrides), renamed })
    }

    /// cladefs: like [`KeyNames::reread`], reusing previously-read file text from `files` so one
    /// reload reads each unique `keys=FILE` once (board 10466).
    pub fn reread_in(
        &self,
        files: &mut HashMap<PathBuf, String>,
    ) -> Result<Option<Staged>, String> {
        let Some(file) = &self.file else { return Ok(None) };
        let text = match files.entry(file.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(read(file)?),
        };
        self.reread_text(text).map(Some)
    }
}

/// cladefs: a [`KeyNames`]'s overrides as its file holds them now, read and checked, not yet in use
/// ([`KeyNames::reread`], board 8487).
#[must_use = "a reload staged is applied, or nothing changes"]
pub struct Staged {
    names: KeyNames,
    /// The overrides it was read against.
    from: Arc<Overrides>,
    overrides: Arc<Overrides>,
    renamed: Vec<u64>,
}

impl Staged {
    /// The tenants whose KEK's name it changes, in order: an override added, changed or removed.
    pub fn renamed(&self) -> &[u64] {
        &self.renamed
    }

    /// Puts it in use, for every clone of its [`KeyNames`], from their next call on: refused,
    /// changing nothing, if another reload put others in use since it was read.
    pub fn apply(self) -> Result<(), String> {
        Self::apply_all([self])
    }

    /// cladefs: puts every staged reload in `staged` in use, deduplicating instances that share
    /// the same overrides cell (board 10466): refused, changing nothing, if any cell changed since
    /// its reload read it.
    pub fn apply_all(staged: impl IntoIterator<Item = Self>) -> Result<(), String> {
        let mut unique: Vec<Self> = Vec::new();
        for item in staged {
            if let Some(existing) =
                unique.iter().find(|done| Arc::ptr_eq(&done.names.overrides, &item.names.overrides))
            {
                if !Arc::ptr_eq(&existing.from, &item.from) {
                    return Err(
                        "the key overrides changed since this reload read them: reload again"
                            .into(),
                    );
                }
                continue;
            }
            unique.push(item);
        }
        // Hold every cell's write lock before swapping any, so a concurrent change refuses all.
        let mut guards: Vec<_> =
            unique.iter().map(|item| item.names.overrides.write().unwrap()).collect();
        for (guard, item) in guards.iter().zip(&unique) {
            if !Arc::ptr_eq(guard, &item.from) {
                return Err(
                    "the key overrides changed since this reload read them: reload again".into()
                );
            }
        }
        for (guard, item) in guards.iter_mut().zip(&unique) {
            **guard = item.overrides.clone();
        }
        Ok(())
    }
}

/// `tenant`'s KEK's name, of `template` and `overrides`.
fn named(template: &str, overrides: &Overrides, tenant: u64) -> String {
    match overrides.get(&tenant) {
        Some(name) => name.clone(),
        None => template.replace(TENANT, &base32_id::Short(tenant).to_string()),
    }
}

/// The overrides file `path` holds.
fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("the key overrides {path:?}: {err}"))
}

/// The overrides `text` holds, each name passing `check`.
fn parse(text: &str, check: Check) -> Result<Overrides, String> {
    let mut names = HashMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        // Its number alone, never what it holds: the file may be any (board 8525 AC #4).
        let at = |why: &str| format!("the key overrides' line {}: {why}", number + 1);
        let mut words = line.split_whitespace();
        let (Some(tenant), Some(name), None) = (words.next(), words.next(), words.next()) else {
            return Err(at("not a tenant and a key's name"));
        };
        let tenant = base32_id::parse(tenant).ok_or_else(|| at("no tenant in base 32"))?;
        // The name, the backend's check says: a key's name, tested as one.
        check(name)
            .map_err(|why| at(&format!("tenant {}'s key, {why}", base32_id::Short(tenant))))?;
        if names.insert(tenant, name.to_owned()).is_some() {
            return Err(at("a tenant named twice"));
        }
    }
    Ok(names)
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

    /// board 8525 AC #4: a line of the overrides refused is named by its number alone, never what
    /// it holds, whatever file `keys` named: /etc/shadow's first line, a token's file.
    #[test]
    fn overrides_refused_say_their_line_number_alone() {
        let secret = "root:$6$s3cr3t:19000:0:99999:7:::";
        for (overrides, why) in [
            (format!("{secret}\n"), "the key overrides' line 1: not a tenant and a key's name"),
            (format!("7 k\n\n{secret} x\n"), "the key overrides' line 3: no tenant in base 32"),
            (format!("7 k\n# {secret}\n7 j\n"), "the key overrides' line 3: a tenant named twice"),
            (format!("a b {secret}\n"), "the key overrides' line 1: not a tenant and a key's name"),
        ] {
            assert_eq!(KeyNames::new("k{tenant}", Some(&overrides)), Err(why.to_owned()));
        }
    }

    /// board 8487: a reload names the tenants whose KEK's name it changes (an override added,
    /// changed, removed; one naming the template's own name changes none), each once, in order;
    /// applied, every clone names them so. A file refused keeps the overrides in use; a reload read
    /// before another applied is refused, changing nothing.
    #[test]
    fn reloads_name_the_tenants_they_rename_and_apply_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keys");
        std::fs::write(&file, "1 one\n2 two\n3 k3\n").unwrap();
        let path = file.to_str().unwrap();
        let names = KeyNames::of_query([("key", "k{tenant}"), ("keys", path)], Origin::Host, |n| {
            (n != "bad").then_some(()).ok_or_else(|| "no key".to_owned())
        })
        .unwrap();
        let clone = names.clone();
        std::fs::write(&file, "2 two\n1 uno\n4 four\n5 k5\n").unwrap();
        let staged = names.reread().unwrap().unwrap();
        assert_eq!(staged.renamed(), [1, 4]);
        assert_eq!((clone.of(1), clone.of(4)), ("one".into(), "k4".into()));
        staged.apply().unwrap();
        let named = [1, 2, 3, 4, 5].map(|tenant| clone.of(tenant));
        assert_eq!(named, ["uno", "two", "k3", "four", "k5"]);

        for (text, why) in [
            ("1 uno\n2 bad\n", "the key overrides' line 2: tenant 2's key, no key"),
            ("1 uno\n\n1 again\n", "the key overrides' line 3: a tenant named twice"),
            ("1 uno 2\n", "the key overrides' line 1: not a tenant and a key's name"),
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(names.reread().err(), Some(why.to_owned()));
        }
        std::fs::remove_file(&file).unwrap();
        assert!(names.reread().err().unwrap().starts_with("the key overrides "));
        assert_eq!(clone.of(4), "four");

        std::fs::write(&file, "4 cuatro\n").unwrap();
        let (first, second) = (names.reread().unwrap().unwrap(), names.reread().unwrap().unwrap());
        assert_eq!(first.renamed(), [1, 2, 4]);
        first.apply().unwrap();
        let refused = second.apply().unwrap_err();
        assert_eq!(refused, "the key overrides changed since this reload read them: reload again");
        assert_eq!((clone.of(1), clone.of(4)), ("k1".into(), "cuatro".into()));
        assert!(KeyNames::new("k{tenant}", None).unwrap().reread().unwrap().is_none());
    }
}
