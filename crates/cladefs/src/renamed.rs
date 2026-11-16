// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the variables renamed with the daemon, `CLADEFS_EXTENTD_*` now `CLADEFSD_*`, and the
//! tests' `CLADEFS_EXTENTD` now `CLADEFSD` (TASK-475.1); and those named for the glossary's words
//! since (CONTEXT.md, TASK-474): the bucket's, `CLADEFSD_REMOTE` now `CLADEFSD_BUCKET`, and the part
//! cache's, `CLADEFSD_CACHE` now `CLADEFSD_PART_CACHE`. Every cladefs program refuses them as it
//! starts: a host left with the old names, its bucket, token and cache among them, would otherwise
//! run as if none were set, a daemon refusing to start for want of a bucket at best, a tool working
//! on the wrong one at worst.

/// What the variables' names began with.
const OLD: &str = "CLADEFS_EXTENTD";

/// What they begin with.
const NEW: &str = "CLADEFSD";

/// The variables renamed since, each with its new name (TASK-474).
const RENAMED: [(&str, &str); 3] = [
    ("CLADEFSD_REMOTE", "CLADEFSD_BUCKET"),
    ("CLADEFSD_CACHE", "CLADEFSD_PART_CACHE"),
    ("CLADEFSD_CACHE_BYTES", "CLADEFSD_PART_CACHE_BYTES"),
];

/// The name `name` has now, if it is an old one.
fn now(name: &str) -> Option<String> {
    let new = match name.strip_prefix(OLD) {
        Some(rest) if rest.is_empty() || rest.starts_with('_') => format!("{NEW}{rest}"),
        _ => name.to_owned(),
    };
    let new =
        RENAMED.iter().find(|(old, _)| *old == new).map_or(new, |(_, renamed)| renamed.to_string());
    (new != name).then_some(new)
}

/// Fails, naming each and what it is now, if any of the process's variables has an old name.
pub fn refuse_old_variables() -> Result<(), String> {
    refuse(std::env::vars_os().map(|(name, _)| name))
}

/// [`refuse_old_variables`], of the variables `names`.
fn refuse(names: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let mut renamed: Vec<String> = names
        .filter_map(|name| name.into_string().ok())
        .filter_map(|name| now(&name).map(|new| format!("{name} is {new}")))
        .collect();
    if renamed.is_empty() {
        return Ok(());
    }
    renamed.sort();
    Err(format!(
        "variables renamed: with the daemon, cladefs-extentd now cladefsd (TASK-475.1), and for \
         the glossary's words (TASK-474): {}; set the new names instead",
        renamed.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> impl Iterator<Item = std::ffi::OsString> {
        names.iter().map(std::ffi::OsString::from).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn new_names_pass() {
        assert_eq!(
            refuse(names(&["CLADEFSD_BUCKET", "CLADEFSD", "CLADEFS_EXTENTDX", "PATH"])),
            Ok(())
        );
    }

    #[test]
    fn old_names_are_refused_each_with_its_new_one() {
        let refused = refuse(names(&[
            "PATH",
            "CLADEFS_EXTENTD_TOKEN",
            "CLADEFS_EXTENTD",
            "CLADEFS_EXTENTD_REMOTE",
            "CLADEFSD_CACHE_BYTES",
        ]));
        assert_eq!(
            refused,
            Err("variables renamed: with the daemon, cladefs-extentd now cladefsd (TASK-475.1), \
                 and for the glossary's words (TASK-474): CLADEFSD_CACHE_BYTES is \
                 CLADEFSD_PART_CACHE_BYTES, CLADEFS_EXTENTD is CLADEFSD, CLADEFS_EXTENTD_REMOTE \
                 is CLADEFSD_BUCKET, CLADEFS_EXTENTD_TOKEN is CLADEFSD_TOKEN; set the new names \
                 instead"
                .to_owned())
        );
    }
}
