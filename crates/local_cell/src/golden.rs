// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the cell directory's golden corpus (doc-28, board 10044): every frozen version of its
//! files is read by every later build. Its framed segments and extents are the extent frames'
//! family's, their footers the bucket layout's; a local checkpoint's table is the bucket layout's
//! checkpoints', so its rest alone is the cell directory's here.

use crate::layout::{CellFile, FORMAT};
use bucket_cell::cell::Kind;
use extent_frame::segment::CellId;

#[test]
fn every_frozen_version_is_read() {
    formats::golden::check::<Files>(env!("CARGO_MANIFEST_DIR"));
}

#[test]
fn v18_local_checkpoint_rest_converts_to_forked_v19() {
    let cases = formats::golden::cases(FORMAT, env!("CARGO_MANIFEST_DIR"), 18);
    let (_, bytes) = cases
        .iter()
        .find(|(name, _)| name == "local-checkpoint-rest")
        .expect("v18 corpus has local-checkpoint-rest");
    crate::cell::ephemeral::rest_read(bytes).expect("v18 local-checkpoint-rest decodes into v19");
}

struct Files;

/// A description naming everything a description names.
fn described() -> CellFile {
    CellFile {
        block_size: 4096,
        cell: CellId { tenant: 0x2f, id: 7 },
        bucket: Some("gs://cladefs-golden/cells".to_owned()),
        capacity: 1 << 30,
        token: Some(0x51),
        forced: Some(0x51),
        forced_after: Some(12),
        writer_copies: vec![3, 4],
        kind: Some(Kind::Sqlite),
        version: Some(0x99),
        ephemeral: true,
    }
}

impl formats::golden::Family for Files {
    const FORMAT: formats::Format = FORMAT;

    fn write() -> Vec<(&'static str, Vec<u8>)> {
        let least = CellFile {
            bucket: None,
            token: None,
            forced: None,
            forced_after: None,
            writer_copies: Vec::new(),
            kind: None,
            version: None,
            ephemeral: false,
            ..described()
        };
        vec![
            ("description", described().text().into_bytes()),
            ("description-least", least.text().into_bytes()),
            ("wal", crate::wal::golden::start()),
            ("log", crate::wal::golden::log()),
            ("local-checkpoint-rest", crate::cell::ephemeral::golden_rest()),
            ("local-checkpoint-rest-root", crate::cell::ephemeral::golden_rest_root()),
            ("ephemeral-keys", crate::upload::golden_keys()),
        ]
    }

    fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
        if !matches!(version, 18 | 19) {
            return Err(format!("no decoder of version {version}"));
        }
        Self::read_written(version, case, bytes)
    }

    fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
        Self::read_current(case, bytes)
    }

    fn fingerprint(version: u32) -> Option<String> {
        let rest = crate::cell::ephemeral::rest_fingerprint(version)?;
        let keys = crate::upload::keys_fingerprint();
        Some(format!("{}\n{rest}\n{keys}\n", crate::wal::golden::fingerprint()))
    }
}

impl Files {
    /// `bytes` decoded as `version` wrote `case`, as its `{:#?}`.
    fn read_written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
        let said = match case {
            "description" | "description-least" => {
                let text = std::str::from_utf8(bytes).map_err(|err| err.to_string())?;
                CellFile::parse(text).map(|file| format!("{file:#?}"))
            }
            "wal" => crate::wal::golden::start_said(bytes),
            "log" => crate::wal::golden::log_said(bytes),
            "local-checkpoint-rest" | "local-checkpoint-rest-root" => {
                crate::cell::ephemeral::rest_written(version, bytes)
            }
            "ephemeral-keys" => crate::upload::keys_said(bytes),
            _ => return Err(format!("no file {case}")),
        };
        said.map_err(|err| format!("{err:#}"))
    }

    /// `bytes` read by this build's current reader for `case`.
    fn read_current(case: &str, bytes: &[u8]) -> Result<(), String> {
        let res = match case {
            "description" | "description-least" => {
                let text = std::str::from_utf8(bytes).map_err(|err| err.to_string())?;
                CellFile::parse(text).map(drop)
            }
            "wal" => crate::wal::golden::start_said(bytes).map(drop),
            "log" => crate::wal::golden::log_said(bytes).map(drop),
            "local-checkpoint-rest" | "local-checkpoint-rest-root" => {
                crate::cell::ephemeral::rest_read(bytes)
            }
            "ephemeral-keys" => crate::upload::keys_said(bytes).map(drop),
            _ => return Err(format!("no file {case}")),
        };
        res.map_err(|err| format!("{err:#}"))
    }
}
