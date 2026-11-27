// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: frozen formats and storage-version ratchets (doc-28, board 10033).
//!
//! A family is the formats one writer writes together: a bucket's objects, a cell directory's
//! files, an ephemeral host's. It has one version number, which every object of it says; a type
//! unchanged between two versions decodes alike in both. A family declares itself once, as a
//! [`Format`], with literals, so that `scripts/format-freeze.py` can read the declaration:
//!
//! ```
//! pub const FORMAT: formats::Format = formats::Format::new("ephemeral host", 2, 2);
//! ```
//!
//! Once its golden corpus is under `formats/` at the repo's root ([`golden`]), the family is
//! frozen: every later build reads every version from `oldest` to `current`, `oldest` never
//! rises, `current` never falls, and no case of the corpus changes.
//!
//! Upgrade ratchets ([`gates::Gates`], [`gates::Ratchet`]) are kept separate from golden-corpus
//! pinning: each bucket stores just two linear monotonic versions — `storage` (mapping to a
//! well-defined [`StorageProfile`] for all host binaries) and `fxfs` (for guest Linux kernels) —
//! so partial upgrades and rollbacks remain safe without a combinatorial gate matrix.

use std::ops::RangeInclusive;

pub mod gates;
pub mod golden;

pub use gates::{GateError, Gates, Ratchet, StorageProfile};

/// A family of formats, and the versions of it this build reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    /// What it is, in words: in every refusal, and its corpus's directory ([`Format::slug`]).
    pub name: &'static str,
    /// The oldest version read: the family's version when it froze, for good.
    pub oldest: u32,
    /// The newest version this build reads, and the newest it may write once ungated.
    pub current: u32,
}

impl Format {
    pub const fn new(name: &'static str, oldest: u32, current: u32) -> Self {
        assert!(oldest <= current, "a format's oldest version is no newer than its current");
        Self { name, oldest, current }
    }

    /// The versions read.
    pub const fn versions(&self) -> RangeInclusive<u32> {
        self.oldest..=self.current
    }

    /// `found`, the version an object says, if this build reads it: every reader's one check.
    pub fn check(&self, found: u32) -> Result<u32, Refused> {
        if self.versions().contains(&found) {
            Ok(found)
        } else {
            Err(Refused { format: *self, found })
        }
    }

    /// The directory of its golden corpus under `formats/`: its name, lower case, with dashes.
    pub fn slug(&self) -> String {
        self.name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect()
    }
}

/// An object of a version of its family this build doesn't read: older than its oldest, or newer
/// than its current (a build rolled back past a change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{} version {found}: this build reads {}",
    format.name,
    Versions(format.oldest, format.current)
)]
pub struct Refused {
    pub format: Format,
    pub found: u32,
}

struct Versions(u32, u32);

impl std::fmt::Display for Versions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Versions(oldest, current) if oldest == current => write!(f, "version {current} alone"),
            Versions(oldest, current) => write!(f, "versions {oldest} to {current}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: Format = Format::new("test family", 2, 4);

    #[test]
    fn versions_outside_the_range_are_refused_by_name() {
        let refused = |found| Err(Refused { format: FORMAT, found });
        for (found, read) in
            [(0, refused(0)), (1, refused(1)), (2, Ok(2)), (3, Ok(3)), (4, Ok(4)), (5, refused(5))]
        {
            assert_eq!(FORMAT.check(found), read);
        }
        assert_eq!(FORMAT.check(u32::MAX), refused(u32::MAX));
        assert_eq!(
            FORMAT.check(1).unwrap_err().to_string(),
            "test family version 1: this build reads versions 2 to 4"
        );
        let alone = Format::new("test family alone", 28, 28);
        assert_eq!(
            alone.check(27).unwrap_err().to_string(),
            "test family alone version 27: this build reads version 28 alone"
        );
        assert_eq!(alone.slug(), "test-family-alone");
    }
}
