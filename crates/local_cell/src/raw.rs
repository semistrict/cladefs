// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell's raw files, as it writes and reads them (backlog doc-16, TASK-411.5): an open
//! segment's (`open-segments/`) and a large extent's own while it is open (`open-extents/`), which
//! hold a guest's writes until they are framed, read back by offset meanwhile. Every cell is
//! encrypted (TASK-411.28), its raw files encrypted a sector at a time ([`SECTOR`]), AES-256-XTS,
//! the sector's place in the file its tweak, under a key of the file's own: derived from the cell's
//! DEK, the newest, and a salt drawn for the file ([`RawEncryption`]). Every write reaching such a
//! file is whole sectors (TASK-411.8), so none reads, changes and writes a sector again. An open
//! segment's salt and DEK are in its log's header, which is MACed, as its records are, under
//! another subkey of the same DEK and salt (`wal::LogHeader`); a large extent's raw file never
//! outlives its process (opening a cell removes those left), so its salt is kept in memory alone.
//!
//! cladefs: the file's key is an object's ([`extent_frame::ObjectKey::sectors`], TASK-411.23): a
//! encrypted raw file framed in place is an object of its sectors as they are, its trailer naming
//! the DEK and the salt, each part tagged under another subkey of them ([`RawFile::sectors`]).

use crate::dir::File;
use crate::framed::Aligned;
use extent_frame::{KeyId, ObjectKey, Salt, Secret};
use extent_keys::MAC_LEN;
use std::io;
use std::ops::Range;
use std::sync::Arc;

/// The bytes of a sector of a raw file: what an encrypted one is encrypted by, each alone, its
/// place in the file its tweak. What reaches an open segment is whole sectors (TASK-411.8).
pub const SECTOR: u64 = extent_keys::xts::SECTOR as u64;

/// The most a raw file's write encrypts at once, through a buffer of its own ([`RawFile`]'s
/// `write_at`): whole sectors.
const ENCRYPTING_MOST: usize = 1 << 20;
const _: () = assert!((ENCRYPTING_MOST as u64).is_multiple_of(SECTOR));

/// cladefs: how an encrypted cell's raw file is encrypted, and its log authenticated: under subkeys
/// of its cell's DEK `key`, from a salt of the file's own, `salt` (TASK-411.5). Its sectors' key is
/// the key of the object the file is once framed in place (TASK-411.23).
pub(crate) struct RawEncryption {
    pub key: KeyId,
    pub salt: Salt,
    sectors: Arc<ObjectKey>,
    mac: Secret,
}

impl RawEncryption {
    /// The encryption of a new raw file, under `dek`, which `key` names, from a salt drawn now
    /// ([`crate::sync::salt`]).
    pub fn new(dek: &Secret, key: KeyId) -> Self {
        Self::of(dek, key, crate::sync::salt())
    }

    /// The encryption of a raw file encrypted under `dek`, which `key` names, from `salt`.
    pub fn of(dek: &Secret, key: KeyId, salt: Salt) -> Self {
        let sectors = Arc::new(ObjectKey::sectors(dek, key, salt));
        Self { key, salt, sectors, mac: dek.derive(&salt, &[b"cladefs raw file log mac"]) }
    }

    /// The MAC of `bytes`, of the file's log, at `at` in it: so that none is taken for another's,
    /// moved in the log, nor of another file's.
    pub fn mac(&self, at: u64, bytes: &[u8]) -> [u8; MAC_LEN] {
        self.mac.mac(&at_bytes(at, bytes))
    }

    /// Checks `mac` is the MAC of `bytes` at `at` in the file's log ([`RawEncryption::mac`]).
    pub fn verify(&self, at: u64, bytes: &[u8], mac: &[u8; MAC_LEN]) -> bool {
        self.mac.verify(&at_bytes(at, bytes), mac).is_ok()
    }

    /// Its sectors' key: the key of the object its raw file is once framed in place.
    pub fn sectors(&self) -> &Arc<ObjectKey> {
        &self.sectors
    }

    /// Encrypts `sectors` in place, whole sectors at `at`, a multiple of [`SECTOR`], in the file.
    pub fn encrypt(&self, at: u64, sectors: &mut [u8]) {
        self.sectors.encrypt_sectors(at / SECTOR, sectors);
    }

    /// Decrypts `sectors` in place, as [`RawEncryption::encrypt`] encrypted them.
    pub fn decrypt(&self, at: u64, sectors: &mut [u8]) {
        self.sectors.decrypt_sectors(at / SECTOR, sectors);
    }
}

/// `at`, little-endian, then `bytes`: what a log's MAC is of.
fn at_bytes(at: u64, bytes: &[u8]) -> Vec<u8> {
    let mut all = Vec::with_capacity(8 + bytes.len());
    all.extend_from_slice(&at.to_le_bytes());
    all.extend_from_slice(bytes);
    all
}

/// cladefs: a raw file of a cell's (see the module's docs): its bytes as written, encrypted a
/// sector at a time. As a [`File`] it reads and writes what was written, encrypting and
/// decrypting: it takes writes of whole sectors alone, and reads the sectors a read is in whole.
/// What it stores, as stored, is [`RawFile::file`]'s.
pub struct RawFile<F> {
    file: Arc<F>,
    encryption: Arc<RawEncryption>,
}

impl<F> RawFile<F> {
    /// cladefs: its sectors' key: the key of the object it is once framed in place, its parts its
    /// sectors as they are, each tagged under it ([`extent_frame::Packer::in_place`],
    /// TASK-411.23).
    pub(crate) fn sectors(&self) -> &Arc<ObjectKey> {
        &self.encryption.sectors
    }
}

impl<F: File> RawFile<F> {
    /// `file`, raw, encrypted by `encryption`.
    pub(crate) fn new(file: F, encryption: Arc<RawEncryption>) -> Self {
        Self { file: Arc::new(file), encryption }
    }

    /// `file`, already shared, raw, encrypted by `encryption`.
    pub(crate) fn from_arc(file: Arc<F>, encryption: Arc<RawEncryption>) -> Self {
        Self { file, encryption }
    }

    /// The file as it stores the bytes, encrypted: for a reader of them as stored, as a
    /// log's records hold them (`wal::LogRecord`), syncs and writing back; and for an I/O of a
    /// caller's own, of bytes [`RawFile::read_range`] and [`RawFile::decrypt`] read, or that
    /// [`crate::Write::stored`] gives. Never for writing anything else, but a framing in place's
    /// tail, past its data (TASK-310), in the clear, as any object's is (TASK-411.23).
    pub fn file(&self) -> &Arc<F> {
        &self.file
    }

    /// The range of the file a read of the `len` bytes at `offset` in it reads: the sectors they
    /// are in, which decrypt whole ([`RawFile::decrypt`]).
    pub fn read_range(&self, offset: u64, len: u64) -> Range<u64> {
        offset / SECTOR * SECTOR..(offset + len).next_multiple_of(SECTOR)
    }

    /// Decrypts `bytes` in place, read as stored at `at`, the start of a [`RawFile::read_range`]:
    /// whole sectors.
    pub fn decrypt(&self, at: u64, bytes: &mut [u8]) {
        self.encryption.decrypt(at, bytes);
    }

    /// Encrypts `bytes` in place, to be written at `at`: whole sectors, refused otherwise.
    pub(crate) fn encrypt(&self, at: u64, bytes: &mut [u8]) -> io::Result<()> {
        if !at.is_multiple_of(SECTOR) || !(bytes.len() as u64).is_multiple_of(SECTOR) {
            let len = bytes.len();
            let error = format!("{len} bytes at {at}: not whole sectors of an encrypted raw file");
            return Err(io::Error::new(io::ErrorKind::InvalidInput, error));
        }
        self.encryption.encrypt(at, bytes);
        Ok(())
    }

    /// Reads `buf.len()` bytes at `offset`, as `read` reads the stored bytes, decrypting them.
    fn read_with(
        &self,
        buf: &mut [u8],
        offset: u64,
        read: impl Fn(&F, &mut [u8], u64) -> io::Result<()>,
    ) -> io::Result<()> {
        let range = self.read_range(offset, buf.len() as u64);
        if range == (offset..offset + buf.len() as u64) {
            read(&self.file, buf, offset)?;
            self.decrypt(offset, buf);
            return Ok(());
        }
        let mut sectors = Aligned::new((range.end - range.start) as usize);
        read(&self.file, sectors.as_mut_slice(), range.start)?;
        self.decrypt(range.start, sectors.as_mut_slice());
        let skip = (offset - range.start) as usize;
        buf.copy_from_slice(&sectors.as_slice()[skip..skip + buf.len()]);
        Ok(())
    }
}

impl<F: File> File for RawFile<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_with(buf, offset, |file, buf, offset| file.read_at(buf, offset))
    }

    fn read_cached(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_with(buf, offset, |file, buf, offset| file.read_cached(buf, offset))
    }

    /// Writes `buf` at `offset`, encrypted through a buffer of its own, whole sectors: of
    /// [`ENCRYPTING_MOST`] bytes at most, a write larger written a piece at a time, so that it
    /// holds that much more, not its own size again, outside what its caller bounds (the extent
    /// daemon's room for requests' buffers). A crash may leave some pieces written, as it may of
    /// any write.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut encrypted = Aligned::new(buf.len().min(ENCRYPTING_MOST));
        for (piece, at) in buf.chunks(ENCRYPTING_MOST).zip((offset..).step_by(ENCRYPTING_MOST)) {
            let encrypted = &mut encrypted.as_mut_slice()[..piece.len()];
            encrypted.copy_from_slice(piece);
            self.encrypt(at, encrypted)?;
            self.file.write_at(encrypted, at)?;
        }
        Ok(())
    }

    fn write_back(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.write_back(offset, len)
    }

    fn write_behind(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.write_behind(offset, len)
    }

    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }

    fn sync_cached(&self) -> io::Result<()> {
        self.file.sync_cached()
    }

    fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.punch(offset, len)
    }

    fn allocated(&self) -> io::Result<u64> {
        self.file.allocated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dir as _, MemDir};

    fn encryption() -> Arc<RawEncryption> {
        let key = KeyId { cell: 7, generation: 1 };
        Arc::new(RawEncryption::new(&Secret::from_bytes(&[5; 32]), key))
    }

    /// A write larger than [`ENCRYPTING_MOST`], encrypted and written a piece at a time, stores
    /// what encrypting it whole would, each sector under its own tweak, and reads back whole.
    #[test]
    fn large_writes_are_encrypted_a_piece_at_a_time_as_if_whole() {
        let dir = MemDir::new();
        let len = 2 * ENCRYPTING_MOST + 3 * SECTOR as usize;
        let data: Vec<u8> = (0..len).map(|at| (at % 253) as u8).collect();
        let raw = RawFile::new(dir.create("encrypted", true).unwrap(), encryption());
        raw.write_at(&data, SECTOR).unwrap();
        let mut whole = data.clone();
        raw.encrypt(SECTOR, &mut whole).unwrap();
        let mut stored = vec![0; len];
        raw.file().read_at(&mut stored, SECTOR).unwrap();
        assert!(stored == whole, "stored otherwise than encrypted whole");
        let mut read = vec![0; len];
        raw.read_at(&mut read, SECTOR).unwrap();
        assert!(read == data, "read back otherwise than written");
    }

    /// An encrypted raw file stores no byte of what was written, and reads it back, at any offset
    /// and length, whole sectors or not; refuses writes of parts of sectors.
    #[test]
    fn encrypted_raw_files_store_ciphertext_and_read_plaintext() {
        let dir = MemDir::new();
        let data: Vec<u8> = (0..3 * SECTOR as usize).map(|at| (at % 251) as u8).collect();
        let raw = RawFile::new(dir.create("encrypted", true).unwrap(), encryption());
        raw.write_at(&data, SECTOR).unwrap();
        let mut stored = vec![0; 4 * SECTOR as usize];
        raw.file().read_at(&mut stored, 0).unwrap();
        assert_eq!(&stored[..SECTOR as usize], &[0; SECTOR as usize][..]);
        for sector in stored[SECTOR as usize..].chunks(SECTOR as usize) {
            assert!(!sector.windows(16).any(|window| data.windows(16).any(|d| d == window)));
        }
        for (offset, len) in [(SECTOR, 3 * SECTOR), (SECTOR + 100, 5000), (2 * SECTOR - 1, 2)] {
            let mut read = vec![0; len as usize];
            raw.read_cached(&mut read, offset).unwrap();
            let from = (offset - SECTOR) as usize;
            assert_eq!(read, &data[from..from + len as usize], "{len} bytes at {offset}");
        }
        assert_eq!(raw.read_range(SECTOR + 100, 5000), SECTOR..3 * SECTOR);
        let refused = raw.write_at(&data[..512], SECTOR).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            refused.to_string(),
            "512 bytes at 4096: not whole sectors of an encrypted raw file"
        );
    }
}
