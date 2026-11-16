// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the variables renamed with the daemon, `CLADEFS_EXTENTD_*` now `CLADEFSD_*`, and the
//! tests' `CLADEFS_EXTENTD` now `CLADEFSD` (TASK-475.1), which every cladefs program refuses as it
//! starts: a host left with the old names, its bucket, token and cache among them, would otherwise
//! run as if none were set, a daemon refusing to start for want of a bucket at best, a tool working
//! on the wrong one at worst.

/// What the variables' names began with.
const OLD: &str = "CLADEFS_EXTENTD";

/// What they begin with.
const NEW: &str = "CLADEFSD";

/// Fails, naming each and what it is now, if any of the process's variables has an old name.
pub fn refuse_old_variables() -> Result<(), String> {
    refuse(std::env::vars_os().map(|(name, _)| name))
}

/// [`refuse_old_variables`], of the variables `names`.
fn refuse(names: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let mut old: Vec<String> = names
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name == OLD || name.starts_with(&format!("{OLD}_")))
        .collect();
    if old.is_empty() {
        return Ok(());
    }
    old.sort();
    let renamed: Vec<String> =
        old.iter().map(|name| format!("{name} is {NEW}{}", &name[OLD.len()..])).collect();
    Err(format!(
        "variables renamed with the daemon, cladefs-extentd now cladefsd (TASK-475.1): {}; set \
         the new names instead",
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
            refuse(names(&["CLADEFSD_REMOTE", "CLADEFSD", "CLADEFS_EXTENTDX", "PATH"])),
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
        ]));
        assert_eq!(
            refused,
            Err("variables renamed with the daemon, cladefs-extentd now cladefsd (TASK-475.1): \
                 CLADEFS_EXTENTD is CLADEFSD, CLADEFS_EXTENTD_REMOTE is CLADEFSD_REMOTE, \
                 CLADEFS_EXTENTD_TOKEN is CLADEFSD_TOKEN; set the new names instead"
                .to_owned())
        );
    }
}
