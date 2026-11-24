// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/journal/reader.rs`. cladefs: the reader reads the
// journal ahead of the blocks it verifies (`ReadAhead`), and its handle's spans change only
// through it, so that it forgets what it read ahead across span changes (`try_push_stream_span`,
// board 6923). What it reads of an image, which may be anything, is refused, where
// upstream asserts (`deserialize`, TASK-149.9). Nothing is consumed past the blocks it verified,
// and each block end crossed takes that block's checksum, however many one record crosses
// (`consume`, TASK-149.23).

use crate::checksum::{Checksum, fletcher64};
use crate::errors::FxfsError;
use crate::object_store::journal::super_block::JournalSpan;
use crate::object_store::journal::{BLOCK_SIZE, JournalCheckpoint, JournalHandle, RESET_XOR};
use crate::serialized_types::{Version, Versioned, VersionedLatest};
use anyhow::{Context, Error, anyhow, bail, ensure};
use byteorder::{ByteOrder, LittleEndian};
use storage_device::buffer::BufferRef;
use storage_units::BlockSize;

/// cladefs: the most blocks a reader reads ahead at once (`ReadAhead`).
const READ_AHEAD_BLOCKS: usize = 32;

/// cladefs: the blocks a reader read ahead of those it verified, from its `read_offset` on. It reads
/// them a window at once, each window twice as long as the last, up to READ_AHEAD_BLOCKS: replaying
/// the journal, whose blocks are each an extent of their own, and each a round trip to the device to
/// read, waits for one round trip per window rather than per block (TASK-92.18). As windows double,
/// what is read past the journal's end, which the reader can't know until it reads it, is no more
/// than what was read before it.
struct ReadAhead {
    /// The blocks read, those before `taken` gone to the reader's buffer.
    blocks: Vec<u8>,
    taken: usize,
    /// How many blocks the next window holds.
    window: usize,
}

impl ReadAhead {
    fn new() -> Self {
        Self { blocks: Vec::new(), taken: 0, window: 1 }
    }

    /// The next block, of `bs` bytes, if it was read.
    fn next(&self, bs: usize) -> Option<&[u8]> {
        self.blocks.get(self.taken..self.taken + bs)
    }

    /// Takes the next block, of `bs` bytes.
    fn take(&mut self, bs: usize) {
        debug_assert!(self.taken + bs <= self.blocks.len());
        self.taken += bs;
    }

    /// Holds `blocks`, the next window, read once those before it were taken. The window after it
    /// is twice as long.
    fn fill(&mut self, blocks: BufferRef<'_>) {
        self.blocks.clear();
        self.taken = 0;
        blocks.append_to(&mut self.blocks);
        self.window = (self.window * 2).min(READ_AHEAD_BLOCKS);
    }

    /// Forgets the blocks from `len` bytes past the next one's start on.
    fn keep(&mut self, len: usize) {
        self.blocks.truncate(self.taken + len.min(self.blocks.len() - self.taken));
    }
}

/// JournalReader supports reading from a journal file which consist of blocks that have a trailing
/// fletcher64 checksum in the last 8 bytes of each block.  The checksum takes the check-sum of the
/// preceding block as an input to the next block so that merely copying a block to a different
/// location will cause a checksum failure.  The serialization of a single record *must* fit within
/// a single block.
pub struct JournalReader {
    // The handle of the journal file that we are reading.
    handle: Box<dyn JournalHandle>,

    // The currently buffered data.
    buf: Vec<u8>,

    // The range within the buffer containing outstanding data.
    buf_range: std::ops::Range<usize>,

    // The next offset we should read from in handle.
    read_offset: u64,

    // cladefs: the blocks read ahead, from `read_offset` on.
    ahead: ReadAhead,

    // The file offset for next record due to be deserialized.
    buf_file_offset: u64,

    // The checksums required for the currently buffered data.  The first checksum is for
    // the *preceding* block since that is what is required to generate a checkpoint.
    checksums: Vec<Checksum>,

    // The version of types to deserialize.
    version: Version,

    // Indicates a bad checksum has been detected and no more data can be read.
    bad_checksum: bool,

    // Indicates a reset checksum has been detected.
    found_reset: bool,

    // Indicates whether the next read is the first read.
    first_read: bool,

    /// For the filesystem journal, we should never hit the end of the file but
    /// when initially reading the superblock, we only know the first block.
    /// In this case, we don't treat EOF in fill_buf as an error.
    eof_ok: bool,
}

impl JournalReader {
    pub(super) fn new(handle: impl JournalHandle, checkpoint: &JournalCheckpoint) -> Self {
        JournalReader {
            handle: Box::new(handle),
            buf: Vec::new(),
            buf_range: 0..0,
            read_offset: BLOCK_SIZE.align_down(checkpoint.file_offset),
            ahead: ReadAhead::new(),
            buf_file_offset: checkpoint.file_offset,
            checksums: vec![checkpoint.checksum],
            version: checkpoint.version,
            bad_checksum: false,
            found_reset: false,
            first_read: true,
            eof_ok: false,
        }
    }

    pub(super) fn journal_file_checkpoint(&self) -> JournalCheckpoint {
        JournalCheckpoint {
            file_offset: self.buf_file_offset,
            checksum: self.checksums[0],
            version: self.version,
        }
    }

    fn last_read_checksum(&self) -> Checksum {
        *self.checksums.last().unwrap()
    }

    /// Sets the version of Key and Value to deserialize.
    /// This can change across Journal RESET boundaries when the tail of the journal is written to
    /// by newer versions of the code.
    pub fn set_version(&mut self, version: Version) {
        self.version = version;

        // Reset bad_checksum in case this version change triggers a change in the block size.
        self.bad_checksum = false;
    }

    /// Used by superblock reader where we expect to hit EOF until we hit the first extent record.
    pub fn set_eof_ok(&mut self) {
        self.eof_ok = true;
    }

    /// To allow users to flush a block, they can store a record that indicates the rest of the
    /// block should be skipped.  When that record is read, users should call this function.
    /// cladefs: it fails if the block wasn't read, as one a journal starts in that doesn't verify
    /// (`consume`).
    pub fn skip_to_end_of_block(&mut self) -> Result<(), Error> {
        let bs = self.block_size();
        let block_offset = self.buf_file_offset % bs;
        if block_offset > 0 {
            self.consume((bs - block_offset) as usize - std::mem::size_of::<Checksum>())?;
        }
        Ok(())
    }

    /// cladefs: appends an in-stream `JournalRecord::JournalSpan` if it extends the handle's
    /// current last span, forgetting any read-ahead from the new span's start onwards (board 6923).
    pub fn try_push_stream_span(
        &mut self,
        span: JournalSpan,
        prev_cell: u64,
        prev_end_sequence: u64,
    ) -> Result<(), Error> {
        if let Some(changed_at) =
            self.handle.try_push_stream_span(span, prev_cell, prev_end_sequence)?
        {
            self.forget_from(changed_at);
        }
        Ok(())
    }

    /// Tries to deserialize a record of type T from the journal stream.  It might return
    /// ReadResult::Reset if a reset marker is encountered, or ReadResult::ChecksumMismatch (which
    /// is expected at the end of the journal stream).
    pub async fn deserialize<T: VersionedLatest>(&mut self) -> Result<ReadResult<T>, Error>
    where
        T: Versioned,
    {
        self.fill_buf().await?;
        let mut cursor = std::io::Cursor::new(self.buffer());
        let version = self.version;
        match T::deserialize_from_version(&mut cursor, version) {
            Ok(record) => {
                let consumed = cursor.position() as usize;
                // cladefs: what's read of a corrupt journal may be as long as a record may be, which
                // the writer keeps shorter than a block.
                ensure!(
                    consumed < self.block_size().get() as usize,
                    anyhow!(FxfsError::Inconsistent).context("Journal record too long")
                );
                self.consume(consumed)?;
                Ok(ReadResult::Some(record))
            }
            Err(e) => {
                if self.found_reset {
                    self.found_reset = false;

                    // Fix up buf_range now...
                    self.consume(self.buf_range.end - self.buf_range.start)?;
                    self.buf_range =
                        self.buf_range.end..self.buf.len() - std::mem::size_of::<Checksum>();
                    let (version, to_consume) = {
                        let mut cursor = std::io::Cursor::new(self.buffer());
                        let version = Version::deserialize_from(&mut cursor)
                            .context("Failed to deserialize version")?;
                        (version, cursor.position() as usize)
                    };
                    self.version = version.clone();
                    self.consume(to_consume)?;
                    return Ok(ReadResult::Reset(version));
                } else if let Some(io_error) = e.downcast_ref::<std::io::Error>() {
                    if io_error.kind() == std::io::ErrorKind::UnexpectedEof && self.bad_checksum {
                        return Ok(ReadResult::ChecksumMismatch);
                    }
                }
                Err(e.into())
            }
        }
    }

    // Fills the buffer so that at least one blocks worth of data is contained within the buffer.
    // After reading a block, it verifies the checksum.  Once done, it should be possible to
    // deserialize records.
    pub async fn fill_buf(&mut self) -> Result<(), Error> {
        let block_size = self.block_size();
        let bs = block_size.get() as usize;
        let min_required = bs - std::mem::size_of::<Checksum>();

        if self.found_reset
            || self.bad_checksum
            || self.buf_range.end - self.buf_range.start >= min_required
        {
            return Ok(());
        }

        // Before we read, shuffle existing data to the beginning of the buffer.
        self.buf.copy_within(self.buf_range.clone(), 0);
        self.buf_range = 0..self.buf_range.end - self.buf_range.start;

        while self.buf_range.end - self.buf_range.start < min_required {
            self.buf.resize(self.buf_range.end + bs, 0);
            let last_read_checksum = self.last_read_checksum();

            // Read the next block's worth, verify its checksum, and append it to |buf|. cladefs: of
            // those read ahead, reading the next window if none is.
            assert!(block_size.is_aligned(self.read_offset));
            if !self.read_block(self.buf_range.end).await? {
                if self.eof_ok {
                    return Ok(());
                } else {
                    bail!("unexpected end of journal file");
                }
            }

            let (contents_slice, checksum_slice) = self.buf.as_slice()
                [self.buf_range.end..self.buf_range.end + bs]
                .split_at(bs - std::mem::size_of::<Checksum>());
            let stored_checksum = LittleEndian::read_u64(checksum_slice);

            match verify_checksum(
                contents_slice,
                stored_checksum,
                last_read_checksum,
                !self.first_read,
            ) {
                ChecksumResult::Ok => {}
                ChecksumResult::Reset => {
                    // Record that we've encountered a reset in the stream (a point where the
                    // journal wasn't cleanly closed in the past) and it starts afresh in this
                    // block.  We don't adjust buf_range until the reset has been processed, but
                    // we do push the checksum.
                    self.found_reset = true;
                    // We need to adjust the last pushed checksum since it could possibly be
                    // for checkpoints later (see Self::journal_file_checkpoint), and
                    // checkpoints should always use the correct seed.
                    if let Some(checksum) = self.checksums.last_mut() {
                        *checksum ^= RESET_XOR;
                    }
                    self.checksums.push(stored_checksum);
                    self.advance();
                    return Ok(());
                }
                ChecksumResult::Bad => {
                    self.bad_checksum = true;
                    return Ok(());
                }
            }

            self.first_read = false;
            self.checksums.push(stored_checksum);

            // If this was our first read, our buffer offset might be somewhere within the middle of
            // the block, so we need to adjust buf_range accordingly.
            if self.buf_file_offset > self.read_offset {
                assert!(self.buf_range.start == 0);
                self.buf_range = (self.buf_file_offset - self.read_offset) as usize
                    ..self.buf.len() - std::mem::size_of::<Checksum>();
            } else {
                self.buf_range =
                    self.buf_range.start..self.buf.len() - std::mem::size_of::<Checksum>();
            }
            self.advance();
        }
        Ok(())
    }

    /// cladefs: copies the block at `read_offset` to `buf` at `at`, having read it ahead with those
    /// after it, as many as the handle has of the next window (`ReadAhead`), unless it was. Returns
    /// whether there was one: none is past the handle's end.
    async fn read_block(&mut self, at: usize) -> Result<bool, Error> {
        let bs = self.block_size().get() as usize;
        if self.ahead.next(bs).is_none() {
            let mut buffer = self.handle.allocate_buffer(self.ahead.window * bs).await;
            let read = self.handle.read_aligned(self.read_offset, buffer.as_mut()).await?;
            self.ahead.fill(buffer.subslice(..read - read % bs));
        }
        let Some(block) = self.ahead.next(bs) else { return Ok(false) };
        self.buf[at..at + bs].copy_from_slice(block);
        Ok(true)
    }

    /// cladefs: moves past the block at `read_offset`, which `buf` now holds, taking it of those read
    /// ahead.
    fn advance(&mut self) {
        let block_size = self.block_size();
        self.ahead.take(block_size.get() as usize);
        self.read_offset += block_size;
    }

    /// cladefs: forgets what was read ahead from `offset` on, which the handle's extents no longer
    /// map as they did.
    fn forget_from(&mut self, offset: u64) {
        self.ahead.keep(offset.saturating_sub(self.read_offset) as usize);
    }

    // Used after deserializing to indicate how many bytes were deserialized and adjusts the buffer
    // pointers accordingly. cladefs: no more than the buffer holds, of the blocks verified, so that
    // each block end crossed has the checksum it takes, and a checkpoint always has one: a skip to
    // the end of a block a journal started in that didn't verify, which was never read, took the
    // only one, and the checkpoint after indexed none (TASK-149.23). It takes one for each end
    // crossed, where upstream takes one for any: a record of a corrupt journal may cross two.
    pub fn consume(&mut self, amount: usize) -> Result<(), Error> {
        ensure!(
            amount <= self.buf_range.len(),
            anyhow!(FxfsError::Inconsistent).context("Journal consumed past what was read")
        );
        let bs = self.block_size().get();
        let seal = std::mem::size_of::<Checksum>() as u64;
        let mut left = amount as u64;
        // Past each block end crossed is the block's checksum, which seeds the next block: the
        // offset moves past it, and it is the checkpoint's from there.
        while self.buf_file_offset % bs + left >= bs - seal {
            let to_end = bs - seal - self.buf_file_offset % bs;
            self.buf_file_offset += to_end + seal;
            left -= to_end;
            self.checksums.drain(0..1);
        }
        self.buf_file_offset += left;
        self.buf_range.start += amount;
        Ok(())
    }

    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.buf_range.clone()]
    }

    fn block_size(&self) -> BlockSize {
        BLOCK_SIZE
    }
}

enum ChecksumResult {
    Ok,
    Reset,
    Bad,
}

fn verify_checksum(
    contents: &[u8],
    stored_checksum: u64,
    last_read_checksum: u64,
    allow_reset: bool,
) -> ChecksumResult {
    if stored_checksum == fletcher64(contents, last_read_checksum) {
        ChecksumResult::Ok
    } else {
        if allow_reset {
            if stored_checksum == fletcher64(contents, last_read_checksum ^ RESET_XOR) {
                return ChecksumResult::Reset;
            }
        }
        ChecksumResult::Bad
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReadResult<T> {
    Reset(Version),
    Some(T),
    ChecksumMismatch,
}

#[cfg(test)]
mod tests {
    // The following tests use JournalWriter to test our reader implementation. This works so long
    // as JournalWriter doesn't use JournalReader to test its implementation.
    use super::{BLOCK_SIZE, JournalReader, ReadResult};
    use crate::object_handle::{ObjectHandle, WriteObjectHandle};
    use crate::object_store::journal::writer::JournalWriter;
    use crate::object_store::journal::{Checksum, JournalCheckpoint, RESET_XOR};
    use crate::serialized_types::{LATEST_VERSION, Version, VersionedLatest};
    use crate::testing::fake_object::{FakeObject, FakeObjectHandle};
    use std::io::Write;
    use std::sync::Arc;

    async fn write_items<T: VersionedLatest + std::fmt::Debug>(
        handle: FakeObjectHandle,
        items: &[T],
    ) {
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        for item in items {
            writer.write_record(item).unwrap();
        }
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");
    }

    /// cladefs: a journal of records, 0 on, filling `blocks` blocks, and how many records it holds:
    /// the last block ends with padding, which `skip_to_end_of_block` skips.
    async fn journal(device: &dyn storage_device::Device, blocks: u64) -> (Vec<u8>, u64) {
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        let mut records = 0u64;
        while writer.journal_file_checkpoint().file_offset < (blocks - 1) * BLOCK_SIZE.get() {
            writer.write_record(&records).unwrap();
            records += 1;
        }
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = device.allocate_buffer(writer.flushable_bytes()).await;
        assert_eq!(writer.take_flushable(buf.as_mut()), 0);
        assert_eq!(buf.len() as u64, blocks * BLOCK_SIZE.get());
        (buf.to_vec(), records)
    }

    /// cladefs: writes `data` to `device`, at the start of `extent`.
    async fn write_to(
        device: &dyn storage_device::Device,
        extent: storage_device::ExtentId,
        data: &[u8],
    ) {
        let mut buf = device.allocate_buffer(data.len()).await;
        buf.as_mut().copy_from_slice(data);
        device.write(extent, 0, buf.as_ref()).await.expect("write failed");
    }

    /// cladefs: reads `records`, and then the end of the journal.
    async fn read_records(reader: &mut JournalReader, records: std::ops::Range<u64>) {
        for record in records {
            assert_eq!(
                reader.deserialize().await.expect("deserialize failed"),
                ReadResult::Some(record)
            );
        }
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(
            reader.deserialize::<u64>().await.expect("deserialize failed"),
            ReadResult::ChecksumMismatch
        );
    }

    /// cladefs: a reader reads ahead of what it consumes, in windows that double up to
    /// `READ_AHEAD_BLOCKS`, each of whose blocks it reads at once: through the journal's handle,
    /// whose blocks are each an extent of their own, it waits for a round trip to the device per
    /// window, not per block (TASK-92.18).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reads_ahead() {
        use crate::object_store::journal::bootstrap_handle::JournalExtentHandle;
        use crate::object_store::journal::super_block::JournalSpan;
        use storage_device::ExtentId;
        use storage_device::fake_device::{DEFAULT_CELL, FakeDevice, RoundTrips};

        // The journal's 100 blocks are written, and 28 more known.
        const WRITTEN: u64 = 100;
        const KNOWN: u64 = 128;
        let bs = BLOCK_SIZE.get();
        let trips = Arc::new(RoundTrips::default());
        let device = Arc::new(FakeDevice::new(KNOWN, bs as u32).with_round_trips(trips.clone()));
        let extent = |block| ExtentId::new(DEFAULT_CELL, block);
        let (data, records) = journal(&*device, WRITTEN).await;
        for (block, data) in data.chunks(bs as usize).enumerate() {
            write_to(&*device, extent(block as u64), data).await;
        }

        let handle = JournalExtentHandle::new(
            device.clone(),
            vec![JournalSpan {
                start_offset: 0,
                block_count: KNOWN,
                cell: DEFAULT_CELL,
                base_sequence: 0,
            }],
        );
        let mut reader = JournalReader::new(
            handle,
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        read_records(&mut reader, 0..records).await;
        // Windows of 1, 2, 4, 8, 16 and three of 32 blocks, the last holding the first block not
        // written.
        assert_eq!((trips.reads.count(), trips.reads.trips()), (127, 8));
    }

    /// cladefs: a record may be as long as a block, which a writer never writes: what a corrupt
    /// journal holds is refused.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_record_as_long_as_a_block() {
        use crate::errors::FxfsError;
        use crate::object_store::ObjectDescriptor;
        use crate::object_store::journal::JournalRecord;
        use crate::object_store::object_record::{ObjectKey, ObjectValue};
        use crate::object_store::transaction::Mutation;
        use crate::serialized_types::{DEFAULT_MAX_SERIALIZED_RECORD_SIZE, Versioned as _};

        // A name as long as it takes.
        let long = (0..4096).find_map(|len| {
            let key = ObjectKey::child(7, &"n".repeat(len));
            let value = ObjectValue::child(9, ObjectDescriptor::File);
            let mutation = Mutation::replace_or_insert_object(key, value);
            let mut bytes = Vec::new();
            JournalRecord::Mutation { object_id: 1, mutation }.serialize_into(&mut bytes).ok()?;
            (bytes.len() as u64 == DEFAULT_MAX_SERIALIZED_RECORD_SIZE).then_some(bytes)
        });
        let long = long.expect("a record as long as a block");

        let object = Arc::new(FakeObject::new());
        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer((BLOCK_SIZE * 3) as usize).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        // A record first, so that a reader has more than a block's worth when it reads the next.
        writer.write_record(&JournalRecord::Commit).unwrap();
        assert_eq!(writer.write(&long).expect("write failed"), long.len());
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        let first = reader.deserialize::<JournalRecord>().await.expect("deserialize failed");
        assert!(matches!(first, ReadResult::Some(JournalRecord::Commit)), "{first:?}");
        let error = reader.deserialize::<JournalRecord>().await.expect_err("read");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");
    }

    /// cladefs: a reader that starts within a block that doesn't verify read none of it, and can't
    /// skip to its end: that took the one checksum it had, and the checkpoint after indexed none,
    /// as a mount does when a super-block says the journal starts there (TASK-149.23).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_no_skip_through_a_block_not_read() {
        use crate::errors::FxfsError;

        let object = Arc::new(FakeObject::new());
        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer((BLOCK_SIZE * 2) as usize).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;

        // The block was sealed from 0, not 1.
        let checkpoint = JournalCheckpoint { file_offset: 4, checksum: 1, version: LATEST_VERSION };
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &checkpoint);
        assert_eq!(
            reader.deserialize::<u32>().await.expect("deserialize failed"),
            ReadResult::ChecksumMismatch
        );
        let error = reader.skip_to_end_of_block().expect_err("skipped what it never read");
        assert!(FxfsError::Inconsistent.matches(&error), "{error:?}");
        assert_eq!(reader.journal_file_checkpoint(), checkpoint);
    }

    /// cladefs: a record of a corrupt journal may cross two block ends, as no writer's does: the
    /// reader moves past both checksums, and the checkpoint after is where the record ends, with
    /// the checksum of the block it ends in (TASK-149.23).
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_record_crossing_two_block_ends() {
        use crate::object_store::ObjectDescriptor;
        use crate::object_store::journal::JournalRecord;
        use crate::object_store::object_record::{ObjectKey, ObjectValue};
        use crate::object_store::transaction::Mutation;
        use crate::serialized_types::Versioned as _;

        let bs = BLOCK_SIZE.get() as usize;
        let contents = bs - std::mem::size_of::<Checksum>();
        // From a block's last byte to the end of the next.
        let len = 1 + contents;
        let record = (0..bs).find_map(|name| {
            let key = ObjectKey::child(7, &"n".repeat(name));
            let value = ObjectValue::child(9, ObjectDescriptor::File);
            let mutation = Mutation::replace_or_insert_object(key, value);
            let mut bytes = Vec::new();
            JournalRecord::Mutation { object_id: 1, mutation }.serialize_into(&mut bytes).ok()?;
            (bytes.len() == len).then_some(bytes)
        });
        let record = record.expect("a record as long as that");

        let object = Arc::new(FakeObject::new());
        let handle = FakeObjectHandle::new(object.clone());
        let mut buf = handle.allocate_buffer(bs * 3).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        writer.write_all(&vec![0u8; contents - 1]).expect("write failed");
        writer.write_all(&record).expect("write failed");
        let end = writer.journal_file_checkpoint();
        assert_eq!(end.file_offset, BLOCK_SIZE * 2);
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");

        let start = JournalCheckpoint {
            file_offset: (contents - 1) as u64,
            checksum: 0,
            version: LATEST_VERSION,
        };
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &start);
        let read = reader.deserialize::<JournalRecord>().await.expect("deserialize failed");
        assert!(matches!(read, ReadResult::Some(JournalRecord::Mutation { .. })), "{read:?}");
        assert_eq!(reader.journal_file_checkpoint(), end);
    }

    /// cladefs: a stream span whose end overflows or crosses the journal limit is refused.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_spans_past_any_offset() {
        use crate::object_store::journal::bootstrap_handle::JournalExtentHandle;
        use crate::object_store::journal::super_block::JournalSpan;
        use storage_device::fake_device::{DEFAULT_CELL, FakeDevice};

        let device = Arc::new(FakeDevice::new(16, 4096));
        let handle = JournalExtentHandle::new(
            device.clone(),
            vec![JournalSpan {
                start_offset: 0,
                block_count: 1,
                cell: DEFAULT_CELL,
                base_sequence: 0,
            }],
        );
        let mut reader = JournalReader::new(handle, &JournalCheckpoint::default());
        reader
            .try_push_stream_span(
                JournalSpan {
                    start_offset: 4096,
                    block_count: u64::MAX / 4096,
                    cell: DEFAULT_CELL,
                    base_sequence: 1,
                },
                DEFAULT_CELL,
                1,
            )
            .expect_err("past any offset");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_single_record() {
        let object = Arc::new(FakeObject::new());
        let handle = FakeObjectHandle::new(object.clone());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let len = (BLOCK_SIZE * 2) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32]).await;

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        let value = reader.deserialize().await.expect("deserialize failed");
        assert_eq!(value, ReadResult::Some(4u32));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_journal_file_checkpoint() {
        let object = Arc::new(FakeObject::new());
        let checkpoint =
            JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() };
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &checkpoint);
        assert_eq!(reader.journal_file_checkpoint(), checkpoint);
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 2) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;

        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));

        // If we take the checkpoint here and then create another reader, we should see the second
        // item.
        let checkpoint = reader.journal_file_checkpoint();
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &checkpoint);
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_skip_to_end_of_block() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        writer.write_record(&4u32).unwrap();
        writer.pad_to_block().expect("pad_to_block failed");
        writer.write_record(&7u32).unwrap();
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");
        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_handle() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        assert_eq!(reader.handle.get_size(), BLOCK_SIZE * 3);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_item_spanning_block() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        // Write one byte so that everything else is misaligned.
        writer.write_record(&4u8).unwrap();
        let mut count: i32 = 0;
        while writer.journal_file_checkpoint().file_offset < BLOCK_SIZE {
            writer.write_record(&12345678u32).unwrap();
            count += 1;
        }
        // Check that writing didn't end up being aligned on a block.
        assert_ne!(writer.journal_file_checkpoint().file_offset, BLOCK_SIZE);
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u8));
        for _ in 0..count {
            assert_eq!(
                reader.deserialize().await.expect("deserialize failed"),
                ReadResult::Some(12345678u32)
            );
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reset() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        write_items(FakeObjectHandle::new(object.clone()), &[4u32, 7u32]).await;

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: LATEST_VERSION, ..JournalCheckpoint::default() },
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(
            reader.deserialize::<u32>().await.expect("deserialize failed"),
            ReadResult::ChecksumMismatch
        );

        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        let new_version = Version { minor: LATEST_VERSION.minor + 1, ..LATEST_VERSION };
        writer.seek(JournalCheckpoint {
            file_offset: BLOCK_SIZE.get(),
            checksum: reader.last_read_checksum() ^ RESET_XOR,
            version: new_version,
        });
        new_version.serialize_into(&mut writer).expect("write version failed");
        writer.write_record(&13u32).unwrap();
        let checkpoint = writer.journal_file_checkpoint();
        writer.write_record(&78u32).unwrap();
        writer.pad_to_block().expect("pad_to_block failed");
        writer.write_record(&90u32).unwrap();
        writer.pad_to_block().expect("pad_to_block failed");
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint { version: new_version, ..JournalCheckpoint::default() },
        );
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(4u32));
        assert_eq!(reader.deserialize().await.expect("deserialize failed"), ReadResult::Some(7u32));
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(
            reader.deserialize::<u32>().await.expect("deserialize failed"),
            ReadResult::Reset(new_version),
        );
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(13u32)
        );
        assert_eq!(reader.journal_file_checkpoint(), checkpoint);
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(78u32)
        );
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(90u32)
        );

        // Make sure a reader can start from the middle of a reset block.
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &checkpoint);
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(78u32)
        );
        reader.skip_to_end_of_block().expect("skip_to_end_of_block failed");
        assert_eq!(
            reader.deserialize().await.expect("deserialize failed"),
            ReadResult::Some(90u32)
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_reader_starting_near_end_of_block() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        let len = 2 * (BLOCK_SIZE.get() as usize - std::mem::size_of::<Checksum>());
        assert_eq!(writer.write(&vec![78u8; len]).expect("write failed"), len);
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");

        let checkpoint = JournalCheckpoint {
            file_offset: BLOCK_SIZE - std::mem::size_of::<Checksum>() as u64 - 1,
            checksum: 0,
            version: LATEST_VERSION,
        };
        let mut reader = JournalReader::new(FakeObjectHandle::new(object.clone()), &checkpoint);
        let mut offset = checkpoint.file_offset as usize;
        while offset < len {
            reader.fill_buf().await.expect("fill_buf failed");
            let mut buf = reader.buffer();
            let amount = std::cmp::min(buf.len(), len - offset);
            buf = &buf[..amount];
            assert_eq!(&buf[..amount], vec![78; amount]);
            offset += amount;
            reader.consume(amount).expect("consume failed");
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_write_to_block_boundary() {
        let object = Arc::new(FakeObject::new());
        // Make the journal file a minimum of two blocks since reading to EOF is an error.
        let handle = FakeObjectHandle::new(object.clone());
        let len = (BLOCK_SIZE * 3) as usize;
        let mut buf = handle.allocate_buffer(len).await;
        buf.fill(0u8);
        handle.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let mut writer = JournalWriter::new(BLOCK_SIZE, 0);
        let len = BLOCK_SIZE.get() as usize - std::mem::size_of::<Checksum>();
        assert_eq!(writer.write(&vec![78u8; len]).expect("write failed"), len);
        let mut buf = handle.allocate_buffer(writer.flushable_bytes()).await;
        let offset = writer.take_flushable(buf.as_mut());
        handle.write_or_append(Some(offset), buf.as_ref()).await.expect("overwrite failed");
        assert_eq!(offset, 0);

        // The checkpoint should be at the start of the next block.
        let writer_checkpoint = writer.journal_file_checkpoint();
        assert_eq!(writer_checkpoint.file_offset, BLOCK_SIZE);

        let mut reader = JournalReader::new(
            FakeObjectHandle::new(object.clone()),
            &JournalCheckpoint::default(),
        );
        reader.set_version(LATEST_VERSION);

        reader.fill_buf().await.expect("fill_buf failed");

        assert_eq!(&reader.buffer()[..len], &buf.subslice(..len).to_vec());

        reader.consume(len).expect("consume failed");

        assert_eq!(reader.journal_file_checkpoint(), writer_checkpoint);
    }
}
