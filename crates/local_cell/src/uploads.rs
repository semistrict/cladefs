// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a segment of the cell's write-ahead log, for its uploader to upload ([`Closed`]).

use crate::dir::File;
use crate::framed::read_object;
use crate::segments::footer_last;
use extent_frame::segment::Footer;
use std::io;
use std::sync::Arc;

/// cladefs: one own-extent object named by a [`Closed`] segment, read on demand when uploaded
/// rather than holding its file descriptor open while queued in `Closed::own` (board 12887).
#[derive(Clone)]
pub struct ClosedOwn {
    pub key: String,
    pub size: u64,
    read: Arc<dyn Fn() -> io::Result<Vec<u8>> + Send + Sync>,
}

impl ClosedOwn {
    /// Creates an on-demand reader for an own extent object at `key` of `size` bytes.
    pub fn new(
        key: String,
        size: u64,
        read: impl Fn() -> io::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        Self { key, size, read: Arc::new(read) }
    }

    /// Reads and CRC-verifies the own extent's bytes on demand.
    pub fn bytes(&self) -> io::Result<Vec<u8>> {
        (self.read)()
    }
}

/// cladefs: a segment the cell closed, for its uploader to upload: its footer, its file, framed,
/// and the files of the extents stored on their own its footer names, by their keys; and whether
/// its writer's chain stamped it anew after a takeover, which its framed file says not
/// (TASK-165.9).
pub struct Closed<F> {
    /// Its number in the cell.
    pub number: u64,
    pub footer: Footer,
    pub file: Arc<F>,
    pub own: Vec<ClosedOwn>,
    pub restamped: bool,
    /// cladefs: the cell's keys, which an encrypted segment is restamped under (TASK-411.3).
    pub keys: std::sync::Arc<bucket_cell::keys::Ring>,
    /// cladefs: the length of its data, framed ([`bucket_cell::Objects::data_len`]).
    pub data_len: u64,
}

impl<F: File> Closed<F> {
    /// The segment's bytes: the object it is, checked ([`read_object`]), with its footer, stamped
    /// anew if it was (`extent_frame::restamp`): cladefs: one encrypted, its new footer and table
    /// authenticated under its key, which restamping it needs, and fails without (TASK-411.2).
    pub fn bytes(&self) -> io::Result<Vec<u8>> {
        let object = read_object(&*self.file)?;
        if !self.restamped {
            return Ok(object);
        }
        let keys = &self.keys.of(self.footer.cell.id);
        extent_frame::restamp(
            &object,
            &bucket_cell::layout::encode_segment_footer(&self.footer),
            keys,
        )
        .map_err(io::Error::other)
    }

    /// The bytes of the files of its extents stored on their own, by their keys, checked.
    pub fn own_bytes(&self) -> io::Result<Vec<(String, Vec<u8>)>> {
        self.own.iter().map(|own| Ok((own.key.clone(), own.bytes()?))).collect()
    }

    /// The cell's epoch of the last commit it holds.
    pub fn last(&self) -> u64 {
        footer_last(&self.footer)
    }

    /// The tag of the last commit it holds, which names the commit in object storage: each segment
    /// the cell closes names one, the last of its commits; cladefs: or the one it goes on with past
    /// them, if any (TASK-92.41).
    pub fn tag(&self) -> u64 {
        let contents = &self.footer.contents;
        let continued = contents.continued.as_ref().map(|continued| continued.commit);
        continued
            .or(contents.commits.last().copied())
            .expect("a segment the cell closed names its last commit")
    }
}
