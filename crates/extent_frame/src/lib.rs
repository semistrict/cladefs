// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how an object holding extents' data is framed (backlog doc-7, Segments). Its data
//! is in parts of [`PART`] bytes, each stored on its own, compressed with zstd, or raw if that is
//! no smaller, at an offset that is a multiple of [`ALIGN`]; then its footer; then a table of where
//! each part is stored, its stored length, the CRC32C of its stored bytes, its encoding and its
//! tag; then a [`TRAILER`] of the lengths of the data, the parts stored and the footer, the CRC of
//! the footer, the object's [`Encryption`], the MACs of its footer and of its table, and the CRC of
//! its table. A cell's closed segments and sealed extents are framed so, as are the objects a
//! cell keeps in object storage: a reader reads and decompresses only the parts it needs, each
//! checked against its CRC before it is decompressed.
//!
//! cladefs: version 3, frozen ([`FORMAT`], doc-28); since 2 (TASK-411.2, doc-16, What is encrypted). Every object is encrypted, or,
//! metadata, authenticated, under its cell's DEK, as its trailer says: none is in the clear
//! (TASK-411.28), a trailer saying so corrupt. Encrypted, each of its parts is compressed first,
//! then encrypted with AES-256-GCM under a subkey of the object's own: its cell's DEK's, of the
//! generation its trailer names, derived with a salt drawn at random for the object and kept in its
//! trailer ([`Secret::derive`]); the nonce is the part's index, and the associated data its index,
//! encoding and lengths. The table's entry holds the part's tag; the CRC stays, over the bytes
//! stored, encrypted, so that a reader holding no key, the collector, checks them as before, and a
//! part corrupt is told from one under another key. The footer, the table and the trailer stay
//! unencrypted, authenticated by HMAC-SHA256 under a second subkey from the same salt, which a
//! reader holding the key checks ([`Keys`]), and one holding none does not.
//!
//! Parts are bound to their object by its salt, not by its key in object storage, as doc-16 first
//! had it: a segment's framed file is uploaded as it is, or with its footer stamped anew
//! ([`restamp`]), at the key of the number and epoch its writer's chain gives it then, which a
//! takeover may change after it was framed (doc-14; `wal::rebase`), and the extents stored on their
//! own its footer names go to keys naming that number and epoch too. A part moved to another
//! object is opened under that object's salt, another subkey, and fails; one moved to another
//! index fails by its nonce and associated data. A whole object copied to another key is authentic
//! there: what it is, a reader tells by its footer, as it does now (a segment's names its cell,
//! number and epoch, `segment::Footer`).
//!
//! A cell's checkpoints, locally and in object storage, are such objects: tables of where its
//! extents are ([`table`]). cladefs: authenticated, not encrypted, under their cell's DEK
//! (TASK-411.3, [`Encryption::Authenticated`]): metadata, which the collector reads holding no key
//! (doc-16, What is encrypted), each part's tag a GMAC of its stored bytes, which a reader holding
//! the key checks.
//!
//! cladefs: no object is in the clear (TASK-411.28): a trailer whose encryption is of kind 0, as
//! one whose encryption, MACs and tags are zeroed, is corrupt to every reader, holding keys or
//! none. (Before, a cell with no KEK wrote in the clear, and a reader of an encrypted cell refused
//! its objects in the clear, TASK-411.3: the trailer's encryption is not authenticated, and one who
//! can write the bucket could otherwise zero an object's encryption, MACs and tags, and have it
//! read as its cell's.)
//!
//! cladefs: an object framed in place from an encrypted cell's raw file ([`Encryption::Sectors`],
//! TASK-411.23): every part stored raw, as its raw file holds it, AES-256-XTS a sector at a time
//! ([`SECTOR`]), the sector's place in the object its tweak, under the raw file's own key, which
//! the trailer names the DEK and salt of ([`ObjectKey::sectors`]); each part tagged, its tag the
//! entry's, with HMAC-SHA256 truncated to 16 bytes ([`Secret::tag`]) under another subkey of the
//! same DEK and salt, of its index, encoding and lengths and its stored bytes: encrypt-then-MAC, so
//! that a reader holding the key checks a part before it decrypts it, and one holding none checks
//! its CRC, as for any object. Framing a segment or a large extent in place so writes none of its
//! data again, encrypted as in the clear (TASK-310). Such an object's parts are all raw and whole
//! sectors; one that compresses is framed in a copy, encrypted with AES-GCM as above: no object
//! mixes the two.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(all(not(feature = "crypto"), not(feature = "pure-zstd")))]
compile_error!("extent_frame requires either the `crypto` or `pure-zstd` feature");

pub mod segment;
pub mod spent;
pub mod table;

pub use extent_keys::xts::SECTOR;
pub use extent_keys::{KeyId, Salt, Secret};

#[cfg(feature = "crypto")]
use extent_keys::Aead;
#[cfg(feature = "crypto")]
use extent_keys::xts::Xts;
use extent_keys::{MAC_LEN, SALT_LEN, TAG_LEN};
use std::collections::HashMap;
use std::ops::Range;
#[cfg(feature = "crypto")]
use std::sync::Arc;

/// Each this many bytes of an object's data, the last fewer, is a part, stored on its own, with
/// its own CRC32C: what a reader reads, checks and decompresses at once.
pub const PART: u64 = 1 << 20;

/// Parts are stored at multiples of this, so that a host reads them bypassing its page cache.
pub const ALIGN: u64 = 4096;

// cladefs: a part of an object of a raw file's sectors begins at one (TASK-411.23); and a part is
// whole sectors.
const _: () = assert!(ALIGN.is_multiple_of(SECTOR as u64) && PART.is_multiple_of(SECTOR as u64));

/// The bytes of the table for each part: its offset, stored length, CRC32C and encoding, and its
/// tag. cladefs: one size for every kind of object (TASK-411.2), so that where a table is, and how
/// a reader of a suffix finds it, never depends on the encryption its trailer names.
pub const ENTRY: u64 = 8 + 4 + 4 + 1 + TAG_LEN as u64;

/// A part's tag, which its table's entry holds: AES-256-GCM's, or a truncated HMAC's.
type Tag = [u8; TAG_LEN];

/// The bytes of a trailer's [`Encryption`]: its kind, a byte, 1 encrypted, 2 authenticated, 3 a raw
/// file's sectors (0, in the clear, is refused: TASK-411.28); the cell and generation of the DEK
/// its subkeys came from; and its salt.
const ENCRYPTION_LEN: usize = 1 + 8 + 4 + SALT_LEN;

/// Where each piece of a trailer is in it.
mod at {
    use super::MAC_LEN;

    pub const DATA_LEN: usize = 0;
    pub const STORED_LEN: usize = 8;
    pub const FOOTER_LEN: usize = 16;
    pub const FOOTER_CRC: usize = 24;
    pub const ENCRYPTION: usize = 28;
    pub const FOOTER_MAC: usize = ENCRYPTION + super::ENCRYPTION_LEN;
    /// The MAC of the table and the trailer before it.
    pub const MAC: usize = FOOTER_MAC + MAC_LEN;
    /// The CRC of the table and the trailer before it.
    pub const TABLE_CRC: usize = MAC + MAC_LEN;
    /// The frame's version, [`super::FORMAT`]'s: the last but the magic, where every version has it.
    pub const VERSION: usize = TABLE_CRC + 4;
    pub const MAGIC: usize = VERSION + 4;
}

/// The bytes after an object's table: the lengths of its data, its parts stored and its footer; the
/// CRC32C of its footer; its [`Encryption`]; the MACs of its footer and of its table and trailer
/// before that MAC, and its version; the CRC32C of its table and trailer before it; its version;
/// and [`MAGIC`].
pub const TRAILER: u64 = (at::MAGIC + MAGIC.len()) as u64;

/// cladefs: the trailer of a frame's `bytes`, if they end as a frame does ([`MAGIC`]): what tells
/// this framing of an object from every other, its salt drawn at random for it, the MACs of its
/// footer and table and the lengths and CRCs beside them, as a bucket's create tells its own write
/// from another's without reading the object whole (board 5738).
pub fn trailer_of(bytes: &[u8]) -> Option<&[u8]> {
    let at = bytes.len().checked_sub(TRAILER as usize)?;
    bytes.ends_with(&MAGIC).then(|| &bytes[at..])
}

/// cladefs: the refusal of the frame whose last bytes are `suffix`, if they end as a frame of a
/// version this build doesn't read does: one of 3 on, its version before its magic, or of before 3,
/// its version its magic's last digit. None if they end as no frame does, or as one read. What
/// tells a frame of another build from one a crash left torn, which a cell's recovery deletes
/// (hotbed review 85).
pub fn refused(suffix: &[u8]) -> Option<formats::Refused> {
    let magic = &suffix[suffix.len().checked_sub(MAGIC.len())?..];
    let version = if magic == MAGIC {
        u32_at(suffix, suffix.len().checked_sub(TRAILER as usize)? + at::VERSION)
    } else {
        let (of, digit) = magic.split_at(MAGIC_OF.len());
        (of == MAGIC_OF).then_some(())?;
        digit[0].checked_sub(b'0').filter(|digit| *digit <= 9)?.into()
    };
    FORMAT.check(version).err()
}

/// The frames this crate writes and reads, frozen at 3 (doc-28, board 10040): a frame says its
/// version, a number before its [`MAGIC`], and a reader of another names it rather than call it
/// corrupt. 2, objects encrypted (TASK-411.2), and 1 before, said theirs as the magic's last byte,
/// a digit, which read frames of 10 on as corrupt; neither is read, nothing having been kept that
/// needs them.
pub const FORMAT: formats::Format = formats::Format::new("extent frame", 3, 3);

/// What a frame of before 3 ends with, its format after it, a digit.
const MAGIC_OF: [u8; 7] = *b"cladefs";

/// An object's last bytes, the same in every version from 3 on: a frame of 3 as a reader of 2
/// named one, which names a frame of 3 or later as of 3, newer than its own.
const MAGIC: [u8; 8] = *b"cladefs3";

/// The zstd level parts are compressed at, unless `CLADEFS_ZSTD_LEVEL` says another, to measure it
/// against (TASK-29.4): the fastest, as sealing and closing segments wait for it. At 0, parts are
/// stored raw, as before compression.
#[cfg(feature = "crypto")]
const LEVEL: i32 = 1;

/// The zstd level parts are compressed at: `CLADEFS_ZSTD_LEVEL`'s, or [`LEVEL`].
#[cfg(feature = "crypto")]
fn level() -> i32 {
    // The process's environment, read once: the same for every simulated host.
    static CHOSEN: std::sync::LazyLock<i32> =
        std::sync::LazyLock::new(|| match std::env::var("CLADEFS_ZSTD_LEVEL") {
            Ok(level) => {
                level.parse().unwrap_or_else(|_| panic!("CLADEFS_ZSTD_LEVEL is {level:?}"))
            }
            // Unset, or not UTF-8, which no level is: the default.
            Err(_) => LEVEL,
        });
    *CHOSEN
}

/// Pure-Rust Castagnoli (iSCSI) CRC32C lookup table, used in keyless wasm32 builds without
/// `crc-fast` (and cross-checked against `crc-fast` in unit tests).
#[cfg(any(not(feature = "crypto"), test))]
const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

#[cfg(any(not(feature = "crypto"), test))]
fn crc32c_pure_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc = CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// The CRC32C of `bytes`.
pub fn crc(bytes: &[u8]) -> u32 {
    #[cfg(feature = "crypto")]
    {
        crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, bytes) as u32
    }
    #[cfg(not(feature = "crypto"))]
    {
        !crc32c_pure_update(!0u32, bytes)
    }
}

/// Streaming CRC32C hasher, matching [`crc`] without buffering the whole input in memory.
#[cfg(feature = "crypto")]
pub struct Crc(crc_fast::Digest);

#[cfg(not(feature = "crypto"))]
pub struct Crc(u32);

impl Crc {
    pub fn new() -> Self {
        #[cfg(feature = "crypto")]
        {
            Self(crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi))
        }
        #[cfg(not(feature = "crypto"))]
        {
            Self(!0u32)
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        #[cfg(feature = "crypto")]
        self.0.update(bytes);
        #[cfg(not(feature = "crypto"))]
        {
            self.0 = crc32c_pure_update(self.0, bytes);
        }
    }

    pub fn finish(self) -> u32 {
        #[cfg(feature = "crypto")]
        {
            self.0.finalize() as u32
        }
        #[cfg(not(feature = "crypto"))]
        {
            !self.0
        }
    }
}

impl Default for Crc {
    fn default() -> Self {
        Self::new()
    }
}

/// An object that is not as its frame says: corrupt, or, encrypted, tampered with or opened under
/// another key than it was encrypted under.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Corrupt(pub String);

fn corrupt(what: impl std::fmt::Display) -> Corrupt {
    Corrupt(format!("an object's {what}"))
}

/// cladefs: why an object's data can't be read (TASK-411.2): it is corrupt, of an unsupported
/// format version, or encrypted under a key its reader doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Corrupt(#[from] Corrupt),
    #[error(transparent)]
    Format(#[from] formats::Refused),
    #[error(
        "an object encrypted under cell {}'s key of generation {}, which its reader doesn't hold",
        base32_id::Short(.0.cell),
        .0.generation
    )]
    NoKey(KeyId),
}

#[cfg(test)]
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn slice_at<'a>(
    bytes: &'a [u8],
    at: usize,
    len: usize,
    what: &'static str,
) -> Result<&'a [u8], Corrupt> {
    let end = at.checked_add(len).ok_or_else(|| corrupt(what))?;
    bytes.get(at..end).ok_or_else(|| corrupt(what))
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn u64_at_checked(bytes: &[u8], at: usize, what: &'static str) -> Result<u64, Corrupt> {
    let slice = slice_at(bytes, at, 8, what)?;
    let arr: [u8; 8] = slice.try_into().map_err(|_| corrupt(what))?;
    Ok(u64::from_le_bytes(arr))
}

#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn u32_at_checked(bytes: &[u8], at: usize, what: &'static str) -> Result<u32, Corrupt> {
    let slice = slice_at(bytes, at, 4, what)?;
    let arr: [u8; 4] = slice.try_into().map_err(|_| corrupt(what))?;
    Ok(u32::from_le_bytes(arr))
}

/// cladefs: the keys a reader of objects holds: the DEK a [`KeyId`] names, if it holds it
/// (TASK-411.2). A reader holding none of an object's reads its footer and table, checked against
/// their CRCs, and checks its parts' stored bytes, but unpacks none of them ([`Error::NoKey`]).
pub trait Keys {
    fn dek(&self, id: KeyId) -> Option<&Secret>;

    /// cladefs: whether its reader holds the objects it reads under their cell's DEK, as every
    /// reader holding keys does, every cell being encrypted (TASK-411.3, TASK-411.28): one
    /// authenticated under a key it doesn't hold is unread ([`Error::NoKey`]), not read as stored.
    /// None by default: a reader of metadata alone, as the collector's marking is, which checks
    /// CRCs.
    fn encrypted(&self) -> bool {
        false
    }
}

/// cladefs: no keys: a reader of objects' metadata alone, checking CRCs, as the collector's marking
/// and a cell's recovery of its write-ahead log are. It unpacks no part.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoKeys;

impl Keys for NoKeys {
    fn dek(&self, _: KeyId) -> Option<&Secret> {
        None
    }
}

impl Keys for HashMap<KeyId, Secret> {
    fn dek(&self, id: KeyId) -> Option<&Secret> {
        self.get(&id)
    }
}

/// cladefs: how an object's parts are encrypted, as its trailer says, under which DEK's subkeys,
/// from which salt (TASK-411.2). None is in the clear (TASK-411.28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encryption {
    /// Its parts encrypted with AES-256-GCM, its footer and table authenticated with HMAC-SHA256,
    /// under subkeys HKDF-SHA256 derives from the DEK `key` names and `salt`.
    Encrypted { key: KeyId, salt: Salt },
    /// cladefs: its parts in the clear, as stored, each tagged with AES-256-GCM's GMAC of them, its
    /// footer and table authenticated as an encrypted object's are (TASK-411.3): metadata, a
    /// cell's checkpoints, which a reader holding no key reads, checked against their CRCs, as
    /// the collector does, and one holding the key checks against their tags and MACs.
    Authenticated { key: KeyId, salt: Salt },
    /// cladefs: framed in place from an encrypted cell's raw file (TASK-411.23): its parts stored
    /// raw, its raw file's AES-256-XTS sectors, under the subkeys of the DEK `key` names and
    /// `salt`, the raw file's, each tagged with a truncated HMAC-SHA256 of them; its footer and
    /// table authenticated as an encrypted object's are.
    Sectors { key: KeyId, salt: Salt },
}

impl Encryption {
    /// The key its subkeys came from, and its salt.
    pub fn keyed(&self) -> (KeyId, Salt) {
        match *self {
            Self::Encrypted { key, salt }
            | Self::Authenticated { key, salt }
            | Self::Sectors { key, salt } => (key, salt),
        }
    }
}

/// cladefs: how a writer frames an object under its cell's DEK, the key it names (TASK-411.3):
/// its parts encrypted, as data is, or authenticated alone, as metadata the collector reads is; or
/// left where a raw file holds them, its XTS sectors, tagged (TASK-411.23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyed {
    Encrypted,
    Authenticated,
    Sectors,
}

/// What HKDF derives an object's subkey that encrypts its parts with, as its info.
#[cfg(feature = "crypto")]
const PARTS_INFO: &[u8] = b"cladefs frame parts";

/// What HKDF derives an object's subkey that authenticates its footer and table with, as its info.
#[cfg(feature = "crypto")]
const MAC_INFO: &[u8] = b"cladefs frame mac";

/// cladefs: what HKDF derives a raw file's XTS subkeys with, as their info's first piece
/// ([`Xts::derive`], TASK-411.5): an object of the file's sectors' parts' too (TASK-411.23).
#[cfg(feature = "crypto")]
const RAW_FILE_INFO: &[u8] = b"cladefs raw file";

/// cladefs: what HKDF derives the subkey that tags the parts of an object of a raw file's sectors
/// with, as its info (TASK-411.23).
#[cfg(feature = "crypto")]
const RAW_PART_TAG_INFO: &[u8] = b"cladefs raw part tag";

/// cladefs: an object's subkeys, derived from its DEK and its salt (TASK-411.2): one that encrypts
/// and opens its parts, and one that authenticates its footer and table. A packer draws a salt of
/// its own for each object it packs ([`Packer::new`]), so that no two objects' parts are encrypted
/// under one subkey, as nonces, the parts' indices, repeat across objects. Shared by the threads
/// that pack an object's parts ([`PartPacker::pack`]) and by its readers. cladefs: of an object of
/// a raw file's sectors, the raw file's own, from the salt drawn for it, which encrypts the file as
/// it is written and tags its parts as it is framed in place ([`ObjectKey::sectors`], TASK-411.23).
#[cfg(feature = "crypto")]
pub struct ObjectKey {
    id: KeyId,
    salt: Salt,
    /// cladefs: whether its parts are encrypted, or authenticated alone (TASK-411.3); or a raw
    /// file's sectors (TASK-411.23).
    keyed: Keyed,
    parts: PartKey,
    mac: Secret,
}

/// cladefs: what an object's parts are encrypted, tagged or encrypted under, as it is keyed.
#[cfg(feature = "crypto")]
enum PartKey {
    /// AES-256-GCM: its parts encrypted, or tagged alone (TASK-411.2, TASK-411.3).
    Aead(Aead),
    /// AES-256-XTS by sector, and HMAC-SHA256 tagging each part's ciphertext (TASK-411.23): its
    /// two key schedules boxed, so that a key of the other kind takes none of their room.
    Sectors { xts: Box<Xts>, tag: Secret },
}

#[cfg(feature = "crypto")]
impl ObjectKey {
    fn new(dek: &Secret, id: KeyId, salt: Salt, keyed: Keyed) -> Self {
        let parts = match keyed {
            Keyed::Encrypted | Keyed::Authenticated => {
                PartKey::Aead(Aead::new(&dek.derive(&salt, &[PARTS_INFO])))
            }
            Keyed::Sectors => PartKey::Sectors {
                xts: Box::new(Xts::derive(dek, &salt, RAW_FILE_INFO)),
                tag: dek.derive(&salt, &[RAW_PART_TAG_INFO]),
            },
        };
        Self { id, salt, keyed, parts, mac: dek.derive(&salt, &[MAC_INFO]) }
    }

    /// cladefs: the key of an encrypted cell's raw file, under `dek`, which `id` names, from
    /// `salt`, drawn for the file (TASK-411.5): its sectors encrypted with AES-256-XTS
    /// ([`ObjectKey::encrypt_sectors`]); and, framed in place, the key of the object it is
    /// ([`Packer::in_place`], [`Encryption::Sectors`], TASK-411.23).
    pub fn sectors(dek: &Secret, id: KeyId, salt: Salt) -> Self {
        Self::new(dek, id, salt, Keyed::Sectors)
    }

    /// cladefs: which DEK it came from.
    pub fn id(&self) -> KeyId {
        self.id
    }

    /// cladefs: the salt it came from.
    pub fn salt(&self) -> Salt {
        self.salt
    }

    fn encryption(&self) -> Encryption {
        let (key, salt) = (self.id, self.salt);
        match self.keyed {
            Keyed::Encrypted => Encryption::Encrypted { key, salt },
            Keyed::Authenticated => Encryption::Authenticated { key, salt },
            Keyed::Sectors => Encryption::Sectors { key, salt },
        }
    }

    /// cladefs: whether its parts are encrypted with AES-GCM, not authenticated alone, nor a raw
    /// file's sectors.
    fn encrypts(&self) -> bool {
        self.keyed == Keyed::Encrypted
    }

    /// cladefs: its AES-256-GCM key, of an object encrypted or authenticated.
    fn aead(&self) -> &Aead {
        match &self.parts {
            PartKey::Aead(aead) => aead,
            PartKey::Sectors { .. } => {
                unreachable!("a raw file's sectors are under no AES-GCM key")
            }
        }
    }

    /// cladefs: its XTS key, and the subkey tagging its parts, of a raw file's sectors.
    fn sectors_keys(&self) -> (&Xts, &Secret) {
        match &self.parts {
            PartKey::Sectors { xts, tag } => (xts, tag),
            PartKey::Aead(_) => unreachable!("an object sealed with AES-GCM has no sectors"),
        }
    }

    /// cladefs: encrypts `sectors` in place, whole sectors ([`SECTOR`]) of its raw file, the first
    /// of them the file's sector `first` (TASK-411.5).
    pub fn encrypt_sectors(&self, first: u64, sectors: &mut [u8]) {
        self.sectors_keys().0.encrypt(first, sectors);
    }

    /// cladefs: decrypts `sectors` in place, as [`ObjectKey::encrypt_sectors`] encrypted them.
    pub fn decrypt_sectors(&self, first: u64, sectors: &mut [u8]) {
        self.sectors_keys().0.decrypt(first, sectors);
    }

    /// cladefs: the tag of `stored`, the stored bytes of part `index`, of a raw file's sectors: its
    /// associated data, as an encrypted part's, then its bytes, under its tagging subkey
    /// ([`Secret::tag`], TASK-411.23).
    fn tag_sectors(&self, index: u64, stored: &[u8]) -> Tag {
        let len = stored.len() as u64;
        let aad = Self::aad(index, Encoding::Raw, stored.len(), len);
        self.sectors_keys().1.tag(&[&aad, stored])
    }

    /// cladefs: opens in place `bytes`, the stored bytes of part `index`, of a raw file's sectors,
    /// stored at `offset` in its object, given its tag: checked against its tag, then decrypted,
    /// its sectors numbered by their places in the object (TASK-411.23).
    fn open_sectors(
        &self,
        index: u64,
        offset: u64,
        bytes: &mut [u8],
        tag: &Tag,
    ) -> Result<(), Corrupt> {
        let len = bytes.len() as u64;
        let aad = Self::aad(index, Encoding::Raw, bytes.len(), len);
        let (xts, tagging) = self.sectors_keys();
        tagging.verify_tag(&[&aad, bytes], tag).map_err(|_| corrupt("part, by its tag"))?;
        if !offset.is_multiple_of(SECTOR as u64) || !len.is_multiple_of(SECTOR as u64) {
            return Err(corrupt("part of sectors, not whole sectors"));
        }
        xts.decrypt(offset / SECTOR as u64, bytes);
        Ok(())
    }

    /// The associated data of part `index` of `len` bytes, stored `encoding`, its stored bytes
    /// `stored` long: what a part moved to another index, or made to say it is stored otherwise,
    /// fails to open by.
    fn aad(index: u64, encoding: Encoding, stored: usize, len: u64) -> [u8; 25] {
        let mut aad = [0; 25];
        aad[..8].copy_from_slice(&index.to_le_bytes());
        aad[8] = encoding.byte();
        aad[9..17].copy_from_slice(&(stored as u64).to_le_bytes());
        aad[17..].copy_from_slice(&len.to_le_bytes());
        aad
    }

    /// Encrypts `bytes` in place, part `index`, of `len` bytes, packed `encoding`: its tag.
    fn encrypt_part(&self, index: u64, encoding: Encoding, len: u64, bytes: &mut [u8]) -> Tag {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        self.aead().seal(index, &aad, bytes)
    }

    /// cladefs: the tag of `bytes`, part `index`, of `len` bytes, packed `encoding`, authenticated
    /// alone: AES-256-GCM's GMAC, of nothing encrypted, its associated data the part's and its
    /// bytes (TASK-411.3), fed one after the other rather than copied into one buffer
    /// ([`Aead::tag`], board 10553).
    fn tag_part(&self, index: u64, encoding: Encoding, len: u64, bytes: &[u8]) -> Tag {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        self.aead().tag(index, &[&aad, bytes])
    }

    /// cladefs: checks `bytes` against `tag`, what [`ObjectKey::tag_part`] gave them.
    fn check_part(
        &self,
        index: u64,
        encoding: Encoding,
        len: u64,
        bytes: &[u8],
        tag: &Tag,
    ) -> Result<(), Corrupt> {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        (self.aead().verify_tag(index, &[&aad, bytes], tag))
            .map_err(|_| corrupt("part, by its tag"))
    }

    /// Opens in place what [`ObjectKey::encrypt_part`] encrypted, given its tag.
    fn open_part(
        &self,
        index: u64,
        encoding: Encoding,
        len: u64,
        bytes: &mut [u8],
        tag: &Tag,
    ) -> Result<(), Corrupt> {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        (self.aead().open(index, &aad, bytes, tag)).map_err(|_| corrupt("part, by its tag"))
    }
}

#[cfg(feature = "crypto")]
impl std::fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ObjectKey");
        debug.field("id", &self.id).field("salt", &self.salt).field("keyed", &self.keyed).finish()
    }
}

/// cladefs: how a reader opens an object's parts and checks its footer and table (TASK-411.2).
#[derive(Clone, Debug)]
enum Opening {
    /// It is encrypted, or authenticated, under a key the reader holds: its subkeys.
    #[cfg(feature = "crypto")]
    Open(Arc<ObjectKey>),
    /// It is encrypted under a key the reader doesn't hold: its parts are checked, not opened.
    Locked(KeyId),
    /// cladefs: it is authenticated under a key the reader doesn't hold, which holds none encrypted
    /// ([`Keys::encrypted`]): its parts are read as they are stored, checked against their CRCs
    /// alone, as the collector reads a checkpoint (TASK-411.3).
    Unchecked(KeyId),
}

impl Opening {
    /// How a reader holding `keys` opens an object encrypted as `encryption` says: refused, if it
    /// is authenticated under a key the reader doesn't hold, and the reader holds its objects
    /// encrypted (TASK-411.3).
    fn of(encryption: Encryption, keys: &dyn Keys) -> Result<Self, Error> {
        #[cfg(feature = "crypto")]
        let opened = |dek: &Secret, key: KeyId, salt: Salt, keyed: Keyed| {
            Self::Open(Arc::new(ObjectKey::new(dek, key, salt, keyed)))
        };
        #[cfg(not(feature = "crypto"))]
        let opened =
            |dek: &Secret, _key: KeyId, _salt: Salt, _keyed: Keyed| -> Self { dek.absurd() };
        Ok(match encryption {
            // cladefs: a raw file's sectors as an encrypted object's parts (TASK-411.23).
            Encryption::Encrypted { key, salt } | Encryption::Sectors { key, salt } => {
                match keys.dek(key) {
                    Some(dek) => {
                        let keyed = match encryption {
                            Encryption::Sectors { .. } => Keyed::Sectors,
                            _ => Keyed::Encrypted,
                        };
                        opened(dek, key, salt, keyed)
                    }
                    None => Self::Locked(key),
                }
            }
            Encryption::Authenticated { key, salt } => match keys.dek(key) {
                Some(dek) => opened(dek, key, salt, Keyed::Authenticated),
                None if keys.encrypted() => return Err(Error::NoKey(key)),
                None => Self::Unchecked(key),
            },
        })
    }

    /// The object's subkeys, if the reader holds them.
    #[cfg(feature = "crypto")]
    fn key(&self) -> Option<&ObjectKey> {
        match self {
            Self::Open(key) => Some(key),
            Self::Locked(_) | Self::Unchecked(_) => None,
        }
    }
}

/// Openings are alike if they open alike: under the same subkeys, as those of one DEK and salt are.
impl PartialEq for Opening {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(feature = "crypto")]
            (Self::Open(one), Self::Open(other)) => {
                (one.id, one.salt, one.keyed) == (other.id, other.salt, other.keyed)
            }
            (Self::Locked(one), Self::Locked(other)) => one == other,
            (Self::Unchecked(one), Self::Unchecked(other)) => one == other,
            _ => false,
        }
    }
}

impl Eq for Opening {}

/// How a part is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    Raw,
    Zstd,
}

impl Encoding {
    /// Its byte in a table's entry.
    #[cfg(feature = "crypto")]
    fn byte(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Zstd => 1,
        }
    }
}

/// Where and how a part is stored: its stored bytes, which a reader holding no key checks
/// ([`Stored::check`]), and a peer or a cache keeps, as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stored {
    pub offset: u64,
    pub len: u64,
    pub crc: u32,
    pub encoding: Encoding,
}

impl Stored {
    /// Where its bytes are in the object.
    pub fn range(&self) -> Range<u64> {
        self.offset..self.offset + self.len
    }

    /// Checks `bytes`, its stored bytes and any after them, against its CRC.
    pub fn check<'a>(&self, bytes: &'a [u8]) -> Result<&'a [u8], Corrupt> {
        let bytes = bytes.get(..self.len as usize).ok_or_else(|| corrupt("part, cut short"))?;
        if crc(bytes) != self.crc {
            return Err(corrupt("part, by its CRC"));
        }
        Ok(bytes)
    }
}

/// Pure-Rust zstd decompression used in keyless `wasm32` builds (`pure-zstd` feature).
#[cfg(feature = "pure-zstd")]
#[cfg_attr(
    feature = "crypto",
    allow(dead_code, reason = "used by keyless wasm32 builds without crypto and tested directly")
)]
fn decode_zstd_pure(packed: &[u8], out: &mut [u8]) -> Result<(), Corrupt> {
    use std::io::Read as _;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(packed)
        .map_err(|err| corrupt(format_args!("part, by zstd: {err}")))?;
    let mut written = 0;
    while written < out.len() {
        let n = decoder
            .read(&mut out[written..])
            .map_err(|err| corrupt(format_args!("part, by zstd: {err}")))?;
        if n == 0 {
            return Err(corrupt("compressed part, of the wrong length"));
        }
        written += n;
    }
    let mut extra = [0u8; 1];
    let more =
        decoder.read(&mut extra).map_err(|err| corrupt(format_args!("part, by zstd: {err}")))?;
    if more != 0 {
        return Err(corrupt("compressed part, of the wrong length"));
    }
    Ok(())
}

/// Decompresses a zstd frame into `out` reusing a per-thread `ZSTD_DCtx` so each part unpack
/// avoids allocating and freeing a ~130 KiB decompression context (board 12313).
#[cfg(feature = "crypto")]
fn decompress_zstd(packed: &[u8], out: &mut [u8]) -> std::io::Result<usize> {
    thread_local! {
        // Per thread scratch ZSTD_DCtx holding no host or cell state; safe to share across
        // simulated hosts on one thread.
        static DECOMPRESSOR: std::cell::RefCell<zstd::bulk::Decompressor<'static>> =
            std::cell::RefCell::new(
                zstd::bulk::Decompressor::new().expect("zstd makes a decompressor"),
            );
    }
    DECOMPRESSOR.with(|cell| cell.borrow_mut().decompress_to_buffer(packed, out))
}

/// Decodes `packed`, a part's packed bytes, opened if it was encrypted, stored `encoding`, into
/// `out`, which is as long as the part.
fn decode(encoding: Encoding, packed: &[u8], out: &mut [u8]) -> Result<(), Corrupt> {
    match encoding {
        Encoding::Raw if packed.len() == out.len() => out.copy_from_slice(packed),
        Encoding::Raw => return Err(corrupt("raw part, of the wrong length")),
        Encoding::Zstd => {
            #[cfg(feature = "crypto")]
            {
                let unpacked = decompress_zstd(packed, out)
                    .map_err(|err| corrupt(format_args!("part, by zstd: {err}")))?;
                if unpacked != out.len() {
                    return Err(corrupt("compressed part, of the wrong length"));
                }
            }
            #[cfg(all(not(feature = "crypto"), feature = "pure-zstd"))]
            {
                decode_zstd_pure(packed, out)?;
            }
        }
    }
    Ok(())
}

/// cladefs: a part of an object, as its reader reads it ([`Parts::part`], TASK-411.2): its index,
/// its place in the data, where it is stored, its tag, and how its reader opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub index: u64,
    pub place: Range<u64>,
    pub stored: Stored,
    tag: Tag,
    opening: Opening,
}

impl Part {
    /// Its bytes, unpacked.
    pub fn len(&self) -> u64 {
        self.place.end - self.place.start
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether its stored bytes are its data, as they are, its reader having nothing to check:
    /// stored raw, authenticated under a key its reader doesn't hold (TASK-411.3), as the
    /// collector reads a checkpoint's.
    pub fn as_is(&self) -> bool {
        self.stored.encoding == Encoding::Raw && matches!(self.opening, Opening::Unchecked(_))
    }

    /// Returns `Err(Error::NoKey)` if this part is encrypted and the reader does not hold its DEK.
    pub fn check_open(&self) -> Result<(), Error> {
        match &self.opening {
            Opening::Locked(key) => Err(Error::NoKey(*key)),
            #[cfg(feature = "crypto")]
            Opening::Open(_) => Ok(()),
            Opening::Unchecked(_) => Ok(()),
        }
    }

    /// Opens `bytes` in place, its stored bytes checked against its CRC, if it is encrypted;
    /// cladefs: checks them against its tag, if it is authenticated under a key its reader holds
    /// (TASK-411.3); checks them against its tag and decrypts them, if a raw file's sectors
    /// (TASK-411.23). Authenticated under a key its reader doesn't hold, as they are.
    fn open_in_place(&self, bytes: &mut [u8]) -> Result<(), Error> {
        #[cfg(feature = "crypto")]
        let (index, encoding) = (self.index, self.stored.encoding);
        #[cfg(not(feature = "crypto"))]
        let _ = bytes;
        match &self.opening {
            Opening::Unchecked(_) => Ok(()),
            Opening::Locked(key) => Err(Error::NoKey(*key)),
            #[cfg(feature = "crypto")]
            Opening::Open(key) => Ok(match key.keyed {
                Keyed::Encrypted => key.open_part(index, encoding, self.len(), bytes, &self.tag),
                Keyed::Authenticated => {
                    key.check_part(index, encoding, self.len(), bytes, &self.tag)
                }
                Keyed::Sectors => key.open_sectors(index, self.stored.offset, bytes, &self.tag),
            }?),
        }
    }

    /// Its packed bytes, from `stored`, its stored bytes and any after them: checked against its
    /// CRC, and opened in place, if it is encrypted. Raw, they are its data; else compressed.
    pub fn open<'a>(&self, stored: &'a mut [u8]) -> Result<&'a [u8], Error> {
        let stored =
            stored.get_mut(..self.stored.len as usize).ok_or_else(|| corrupt("part, cut short"))?;
        if crc(stored) != self.stored.crc {
            return Err(corrupt("part, by its CRC").into());
        }
        self.open_in_place(stored)?;
        Ok(stored)
    }

    /// Unpacks it from `stored`, its stored bytes and any after them, opened in place if it is
    /// encrypted ([`Part::open`]), into `out`, which is as long as it is.
    pub fn unpack_into(&self, stored: &mut [u8], out: &mut [u8]) -> Result<(), Error> {
        let packed = self.open(stored)?;
        Ok(decode(self.stored.encoding, packed, out)?)
    }

    /// Unpacks it from `stored`, its stored bytes and any after them, checked against its CRC,
    /// and opened, if encrypted, into `out`, which must be as long as the part (board 12313).
    pub fn unpack_copy_into(&self, stored: &[u8], out: &mut [u8]) -> Result<(), Error> {
        debug_assert_eq!(out.len(), self.len() as usize);
        let checked = self.stored.check(stored)?;
        match (&self.opening, self.stored.encoding) {
            (Opening::Unchecked(_), encoding) => decode(encoding, checked, out)?,
            // cladefs: checked where it is, then unpacked (TASK-411.3).
            #[cfg(feature = "crypto")]
            (Opening::Open(key), encoding) if key.keyed == Keyed::Authenticated => {
                key.check_part(self.index, encoding, self.len(), checked, &self.tag)?;
                decode(encoding, checked, out)?;
            }
            // Opened where it is unpacked to.
            (_, Encoding::Raw) => {
                if checked.len() != out.len() {
                    return Err(corrupt("raw part, of the wrong length").into());
                }
                out.copy_from_slice(checked);
                self.open_in_place(out)?;
            }
            (_, Encoding::Zstd) => {
                let mut packed = checked.to_vec();
                self.open_in_place(&mut packed)?;
                decode(Encoding::Zstd, &packed, out)?;
            }
        }
        Ok(())
    }

    /// It, unpacked from `stored`, its stored bytes and any after them, checked against its CRC,
    /// and opened, if encrypted, in a copy of them.
    pub fn unpack(&self, stored: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = vec![0; self.len() as usize];
        self.unpack_copy_into(stored, &mut out)?;
        Ok(out)
    }
}

#[cfg(feature = "crypto")]
mod packing {
    use super::*;

    /// A part, packed: compressed, or raw if compressing does not shrink it.
    fn pack_part(data: &[u8], compressor: &mut zstd::bulk::Compressor<'_>) -> Option<Vec<u8>> {
        // Room for no more than the raw part: compressing what will not fit fails.
        let mut packed = Vec::with_capacity(data.len().saturating_sub(1));
        match compressor.compress_to_buffer(data, &mut packed) {
            Ok(_) if !packed.is_empty() && packed.len() < data.len() => Some(packed),
            _ => None,
        }
    }

    /// cladefs: a part of an object's data, packed on its own ([`PartPacker::pack`]): compressed, if
    /// that shrinks it, or raw; encrypted, or tagged, under its object's key; the CRC of its bytes as
    /// stored, and its tag (TASK-336, TASK-411.2). Or compressed alone, in memory, to be encrypted once
    /// its object's key is drawn ([`PartPacker::compress`], [`PackedPart::encrypted`], TASK-421): no
    /// object takes it so ([`Packer::push_stored`]).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PackedPart {
        /// Its index in its object, which its nonce is.
        index: u64,
        encoding: Encoding,
        /// Its bytes as stored, unless they are the part's, as they are: raw, authenticated alone or
        /// compressed alone.
        stored: Option<Vec<u8>>,
        crc: u32,
        tag: Tag,
        /// cladefs: how its object is keyed (TASK-411.3); none, compressed alone, in memory.
        keyed: Option<Keyed>,
    }

    impl PackedPart {
        pub fn encoding(&self) -> Encoding {
            self.encoding
        }

        /// The CRC32C of its bytes as stored.
        pub fn crc(&self) -> u32 {
            self.crc
        }

        /// Its bytes as stored, `part`'s, the part it packs, or others.
        pub(super) fn stored<'a>(&'a self, part: &'a [u8]) -> &'a [u8] {
            self.stored.as_deref().unwrap_or(part)
        }

        /// cladefs: its bytes as stored, unless they are the part's own, raw: compressed, or encrypted.
        /// What a cell holds of a part compressed as its segment fills, with the part's bytes if these
        /// are none (TASK-400).
        pub fn stored_bytes(&self) -> Option<&[u8]> {
            self.stored.as_deref()
        }

        /// cladefs: the bytes of memory [`PackedPart::stored_bytes`] take.
        pub fn stored_capacity(&self) -> usize {
            self.stored.as_ref().map_or(0, Vec::capacity)
        }

        /// cladefs: lets go of what the buffer of [`PackedPart::stored_bytes`] holds past them: a
        /// part's room, which compressing took (TASK-400).
        pub fn shrink_to_fit(&mut self) {
            if let Some(stored) = &mut self.stored {
                stored.shrink_to_fit();
            }
        }

        /// cladefs: it, compressed alone ([`PartPacker::compress`]), of `len` bytes of data, `stored`
        /// its bytes as stored then ([`PackedPart::stored_bytes`], or the part's own), encrypted under
        /// `key`, its object's: as [`PartPacker::pack`] packs the part under `key`, byte for byte. What
        /// a cell's framing in a copy makes of a part packed ahead and held in memory, as the extent
        /// cache holds data, encrypted only once its framing draws its object's key (TASK-421).
        pub fn encrypted(&self, len: u64, stored: &[u8], key: &ObjectKey) -> PackedPart {
            assert_eq!(self.keyed, None, "a part compressed alone");
            assert!(key.encrypts(), "encrypted under an encrypted object's key");
            let mut bytes = stored.to_vec();
            let tag = key.encrypt_part(self.index, self.encoding, len, &mut bytes);
            let crc = crc(&bytes);
            let (index, encoding, keyed) = (self.index, self.encoding, Some(Keyed::Encrypted));
            PackedPart { index, encoding, stored: Some(bytes), crc, tag, keyed }
        }
    }

    /// cladefs: a part of an object framed in place, stored raw where its data was written (TASK-310):
    /// its bytes, the CRC32C of its stored bytes, and its tag, of a raw file's sectors (TASK-411.23).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct RawPart {
        pub len: u64,
        pub crc: u32,
        pub tag: [u8; TAG_LEN],
    }

    impl RawPart {
        /// Part `index` of an object framed in place, `stored` its stored bytes, its raw file's XTS
        /// sectors under `key`, tagged ([`ObjectKey::sectors`]).
        pub fn of(index: u64, stored: &[u8], key: &ObjectKey) -> Self {
            let tag = key.tag_sectors(index, stored);
            Self { len: stored.len() as u64, crc: crc(stored), tag }
        }
    }

    /// cladefs: packs parts of an object's data one at a time, as [`Packer::push`] packs each: so that
    /// a cell packs a segment's parts on threads of its own, each part read there, rather than all its
    /// data read on one thread first (TASK-336).
    pub struct PartPacker {
        /// None at level 0, which stores parts raw.
        compressor: Option<zstd::bulk::Compressor<'static>>,
    }

    impl Default for PartPacker {
        fn default() -> Self {
            let compressor = (level() != 0)
                .then(|| zstd::bulk::Compressor::new(level()).expect("zstd makes a compressor"));
            Self { compressor }
        }
    }

    impl PartPacker {
        pub fn new() -> Self {
            Self::default()
        }

        /// `part`, part `index` of an object's data, packed, as [`Packer::push`] packs it: encrypted
        /// under `key`, its object's ([`Packer::key`]), if it is encrypted (TASK-411.2); cladefs:
        /// tagged under it, if it is authenticated (TASK-411.3).
        pub fn pack(&mut self, index: u64, part: &[u8], key: &ObjectKey) -> PackedPart {
            let (encoding, compressed) = self.compressed(part);
            let len = part.len() as u64;
            let (stored, tag) = match key.keyed {
                Keyed::Encrypted => {
                    let mut stored = compressed.unwrap_or_else(|| part.to_vec());
                    let tag = key.encrypt_part(index, encoding, len, &mut stored);
                    (Some(stored), tag)
                }
                Keyed::Authenticated => {
                    let tag =
                        key.tag_part(index, encoding, len, compressed.as_deref().unwrap_or(part));
                    (compressed, tag)
                }
                Keyed::Sectors => unreachable!("a raw file's sectors are framed in place"),
            };
            let crc = crc(stored.as_deref().unwrap_or(part));
            PackedPart { index, encoding, stored, crc, tag, keyed: Some(key.keyed) }
        }

        /// cladefs: `part`, part `index` of an object's data, compressed alone, in memory, as
        /// [`PartPacker::pack`] would compress it: what a cell holds of a part packed ahead of its
        /// framing, encrypted only once that draws its object's key ([`PackedPart::encrypted`],
        /// TASK-421). No object takes it as it is.
        pub fn compress(&mut self, index: u64, part: &[u8]) -> PackedPart {
            let (encoding, stored) = self.compressed(part);
            let crc = crc(stored.as_deref().unwrap_or(part));
            PackedPart { index, encoding, stored, crc, tag: [0; TAG_LEN], keyed: None }
        }

        /// How `part` is stored, and its bytes compressed, if compressing shrinks it.
        fn compressed(&mut self, part: &[u8]) -> (Encoding, Option<Vec<u8>>) {
            let compressed =
                self.compressor.as_mut().and_then(|compressor| pack_part(part, compressor));
            let encoding = if compressed.is_some() { Encoding::Zstd } else { Encoding::Raw };
            (encoding, compressed)
        }

        /// cladefs: the bytes `part`, a part of an object's data, or a piece of one, is stored in:
        /// compressed, or raw if that is no smaller. What a cell samples a segment's parts by, to tell
        /// whether compressing them is worth writing it again (TASK-310). Encryption adds no byte.
        pub fn packed_len(&mut self, part: &[u8]) -> u64 {
            let compressor = self.compressor.as_mut();
            let packed = compressor.and_then(|compressor| pack_part(part, compressor));
            packed.map_or(part.len(), |packed| packed.len()) as u64
        }
    }

    /// cladefs: [`PartPacker::packed_len`], with a packer of its own (TASK-310).
    pub fn packed_len(part: &[u8]) -> u64 {
        PartPacker::new().packed_len(part)
    }

    /// `data`'s parts, packed, the first part `first` of its object, under `key`, its object's: on
    /// threads of their own if there are several.
    fn pack_parts(data: &[u8], first: u64, key: &ObjectKey) -> Vec<PackedPart> {
        let parts: Vec<&[u8]> = data.chunks(PART as usize).collect();
        let threads = std::thread::available_parallelism()
            .map_or(1, |threads| threads.get())
            .min(parts.len());
        let pack = |first: u64, parts: &[&[u8]]| {
            let mut packer = PartPacker::new();
            let parts = parts.iter().zip(first..);
            parts.map(|(part, index)| packer.pack(index, part, key)).collect::<Vec<_>>()
        };
        if threads <= 1 {
            return pack(first, &parts);
        }
        let each = parts.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let packing: Vec<_> = (parts.chunks(each).zip((first..).step_by(each)))
                .map(|(parts, first)| scope.spawn(move || pack(first, parts)))
                .collect();
            packing.into_iter().flat_map(|thread| thread.join().unwrap()).collect()
        })
    }

    /// Frames an object written front to back: its parts, given in order, then its footer.
    #[derive(Debug)]
    pub struct Packer {
        pub(super) stored: Vec<Stored>,
        pub(super) tags: Vec<Tag>,
        pub(super) data_len: u64,
        /// The bytes of the object so far.
        pub(super) end: u64,
        /// cladefs: its subkeys (TASK-411.2).
        pub(super) key: Arc<ObjectKey>,
    }

    impl Packer {
        /// A packer of an object encrypted, given its cell's DEK and which it is, `encrypt`, under
        /// subkeys of the DEK from a salt drawn for it alone (TASK-411.2).
        pub fn new((dek, id): (&Secret, KeyId)) -> Self {
            Self::keyed((dek, id, Keyed::Encrypted))
        }

        /// cladefs: a packer of an object, given its cell's DEK, which it is, and how, `keyed` under
        /// subkeys of the DEK from a salt drawn for it alone: encrypted, or authenticated alone
        /// (TASK-411.3).
        pub fn keyed((dek, id, keyed): (&Secret, KeyId, Keyed)) -> Self {
            assert_ne!(keyed, Keyed::Sectors, "a raw file's sectors are framed in place");
            Self::with(Arc::new(ObjectKey::new(dek, id, Salt::random(), keyed)))
        }

        /// cladefs: a packer of an object framed in place, its parts raw ([`Packer::push_raw_parts`]),
        /// from a cell's raw file, whose sectors `key` encrypted ([`ObjectKey::sectors`]): the
        /// object's parts are those sectors, as they are (TASK-411.23).
        pub fn in_place(key: Arc<ObjectKey>) -> Self {
            assert_eq!(key.keyed, Keyed::Sectors, "framed in place under a raw file's sectors key");
            Self::with(key)
        }

        pub(super) fn with(key: Arc<ObjectKey>) -> Self {
            Self { stored: Vec::new(), tags: Vec::new(), data_len: 0, end: 0, key }
        }

        /// The bytes of the object so far.
        pub fn end(&self) -> u64 {
            self.end
        }

        /// cladefs: its subkeys: what the threads that pack its parts encrypt them under
        /// ([`PartPacker::pack`], TASK-411.2).
        pub fn key(&self) -> &ObjectKey {
            &self.key
        }

        /// cladefs: how the object is encrypted, as its trailer will say (TASK-411.2).
        pub fn encryption(&self) -> Encryption {
            self.key.encryption()
        }

        /// cladefs: the index of the object's next part (TASK-411.2).
        pub fn next_part(&self) -> u64 {
            self.stored.len() as u64
        }

        /// cladefs: whether every part so far is stored raw, none compressing: what an object framed in
        /// place holds, raw (TASK-310, TASK-411.23).
        pub fn stored_raw(&self) -> bool {
            self.stored.iter().all(|stored| stored.encoding == Encoding::Raw)
        }

        /// cladefs: its key, of an object framed in place, raw: a raw file's sectors key
        /// ([`Packer::in_place`]).
        fn raw_key(&self) -> &ObjectKey {
            assert_eq!(
                self.key.keyed,
                Keyed::Sectors,
                "an encrypted object's bytes are not its data"
            );
            &self.key
        }

        /// cladefs: packs the object's first parts as stored, each raw, compressing none, given by
        /// their lengths, CRCs and tags ([`RawPart::of`]), as a cell reads and checksums them on
        /// threads of its own (TASK-336): of an object framed in place ([`Packer::in_place`]), a raw
        /// file's sectors, their ciphertext, each part tagged (TASK-411.23).
        pub fn push_raw_parts(&mut self, parts: impl IntoIterator<Item = RawPart>) {
            assert_eq!(self.end, 0, "raw parts first");
            self.raw_key();
            for RawPart { len, crc, tag } in parts {
                assert!(self.data_len.is_multiple_of(PART), "parts follow a part cut short");
                let whole = len.is_multiple_of(SECTOR as u64);
                assert!(whole, "a part of a raw file's sectors, {len} bytes, is whole sectors");
                let offset = self.end;
                self.stored.push(Stored { offset, len, crc, encoding: Encoding::Raw });
                self.tags.push(tag);
                self.data_len += len;
                self.end += len;
            }
        }

        /// Packs `data`, the object's next parts: whole parts, but for the object's last. Returns the
        /// object's next bytes, which follow those returned before.
        pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
            let mut bytes = Vec::new();
            let packed = pack_parts(data, self.next_part(), self.key());
            for (part, packed) in data.chunks(PART as usize).zip(packed) {
                self.push_part(&mut bytes, part, &packed);
            }
            bytes
        }

        /// cladefs: packs `part`, the object's next part, as `packed`, [`PartPacker::pack`]'s of it,
        /// says, appending the object's next bytes to `bytes` (TASK-336). A whole part, but for the
        /// object's last; packed as the object's next, encrypted under its key if it is encrypted.
        pub fn push_part(&mut self, bytes: &mut Vec<u8>, part: &[u8], packed: &PackedPart) {
            self.push_stored(bytes, part.len() as u64, packed.stored(part), packed);
        }

        /// cladefs: [`Packer::push_part`], of a part of `len` bytes of data given by its bytes as
        /// stored, `stored`: compressed or encrypted if `packed` is, else the part's own. So that a
        /// cell packs a part as its segment fills, and holds it compressed, its data gone, until the
        /// segment's framing (TASK-400).
        pub fn push_stored(
            &mut self,
            bytes: &mut Vec<u8>,
            len: u64,
            stored: &[u8],
            packed: &PackedPart,
        ) {
            assert!(self.data_len.is_multiple_of(PART), "parts follow a part cut short");
            assert_eq!(packed.index, self.next_part(), "a part packed as the object's next");
            assert_eq!(packed.keyed, Some(self.key.keyed), "a part encrypted as its object is");
            match &packed.stored {
                Some(packed) => debug_assert!(packed[..] == *stored, "a part packed, stored so"),
                None => assert_eq!(stored.len() as u64, len, "a raw part, stored as it is"),
            }
            let offset = self.end.next_multiple_of(ALIGN);
            bytes.resize(bytes.len() + (offset - self.end) as usize, 0);
            bytes.extend_from_slice(stored);
            let stored_len = stored.len() as u64;
            let encoding = packed.encoding;
            self.stored.push(Stored { offset, len: stored_len, crc: packed.crc, encoding });
            self.tags.push(packed.tag);
            self.data_len += len;
            self.end = offset + stored_len;
        }

        /// The object's last bytes, which follow those [`Packer::push`] returned: `footer`, the table,
        /// and the trailer.
        pub fn finish(self, footer: &[u8]) -> Vec<u8> {
            self.finish_parts(footer).0
        }

        /// cladefs: [`Packer::finish`]'s bytes, and the object's parts, as its writer reads them:
        /// opened under its key, which its writer holds, needing no [`Keys`] to find it (TASK-411.2).
        pub fn finish_parts(self, footer: &[u8]) -> (Vec<u8>, Parts) {
            let table = Table { stored: &self.stored, tags: &self.tags };
            let tail = tail(table, self.data_len, self.end, footer, &self.key);
            let frame = Frame::of_trailer(&tail).expect("a trailer as packed");
            let opening = Opening::Open(self.key);
            (tail, Parts { frame, stored: self.stored, tags: self.tags, opening })
        }
    }

    /// An object's table: where its parts are stored, and their tags.
    #[derive(Clone, Copy)]
    struct Table<'a> {
        stored: &'a [Stored],
        tags: &'a [Tag],
    }

    /// The last bytes of an object of `data_len` bytes of data, whose parts are as `table` says, ending
    /// at `end`, encrypted under `key`: `footer`, the table, and the trailer.
    fn tail(table: Table<'_>, data_len: u64, end: u64, footer: &[u8], key: &ObjectKey) -> Vec<u8> {
        let mut bytes = footer.to_vec();
        let start = bytes.len();
        for (stored, tag) in table.stored.iter().zip(table.tags) {
            bytes.extend_from_slice(&stored.offset.to_le_bytes());
            bytes.extend_from_slice(&(stored.len as u32).to_le_bytes());
            bytes.extend_from_slice(&stored.crc.to_le_bytes());
            bytes.push(stored.encoding.byte());
            bytes.extend_from_slice(tag);
        }
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend_from_slice(&end.to_le_bytes());
        bytes.extend_from_slice(&(footer.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&crc(footer).to_le_bytes());
        let mut encryption = [0; ENCRYPTION_LEN];
        encryption[0] = match key.keyed {
            Keyed::Encrypted => 1,
            Keyed::Authenticated => 2,
            Keyed::Sectors => 3,
        };
        encryption[1..9].copy_from_slice(&key.id.cell.to_le_bytes());
        encryption[9..13].copy_from_slice(&key.id.generation.to_le_bytes());
        encryption[13..].copy_from_slice(&key.salt.0);
        bytes.extend_from_slice(&encryption);
        bytes.extend_from_slice(&key.mac.mac(footer));
        // The table's MAC covers the trailer before it, the footer's MAC, the encryption, the lengths,
        // and the version, which is after it: so that no version is read as another.
        let version = FORMAT.current.to_le_bytes();
        let mac = key.mac.mac_of(&[&bytes[start..], &version]);
        bytes.extend_from_slice(&mac);
        // And its CRC, the MAC too.
        bytes.extend_from_slice(&crc(&bytes[start..]).to_le_bytes());
        bytes.extend_from_slice(&version);
        bytes.extend_from_slice(&MAGIC);
        bytes
    }

    /// cladefs: the whole object `object`, of `footer` in place of its own: its parts as they are,
    /// stored where they are, then a tail anew, so that its data reads as it did, nothing unpacked or
    /// packed again. What a host's write-ahead log uploads of a segment its writer's chain stamped anew
    /// after a takeover (doc-14, Opening a store; TASK-165.9). Fails if `object` is not a whole object.
    /// An encrypted object's new footer and table are authenticated under its own subkeys, from its
    /// salt, which its parts stay encrypted under: so `keys` must hold its DEK, its table checked
    /// against its MAC first, or it fails ([`Error::NoKey`], TASK-411.2); cladefs: an authenticated
    /// one's alike (TASK-411.3).
    pub fn restamp(object: &[u8], footer: &[u8], keys: &dyn Keys) -> Result<Vec<u8>, Error> {
        let parts = Parts::of(object, keys)?;
        let key = match &parts.opening {
            Opening::Locked(key) | Opening::Unchecked(key) => return Err(Error::NoKey(*key)),
            Opening::Open(key) => key,
        };
        let (frame, table) = (parts.frame, Table { stored: &parts.stored, tags: &parts.tags });
        let mut bytes = object[..frame.stored_len as usize].to_vec();
        bytes.extend_from_slice(&tail(table, frame.data_len, frame.stored_len, footer, key));
        Ok(bytes)
    }

    /// The bytes of an object of `data` and `footer`, encrypted under the DEK `encrypt` gives, and
    /// which it is ([`Packer::new`]).
    pub fn framed(data: &[u8], footer: &[u8], (dek, id): (&Secret, KeyId)) -> Vec<u8> {
        framed_keyed(data, footer, (dek, id, Keyed::Encrypted))
    }

    /// cladefs: the bytes of an object of `data` and `footer`, keyed as `keyed` says: under its DEK,
    /// which it is, and how ([`Packer::keyed`], TASK-411.3).
    pub fn framed_keyed(data: &[u8], footer: &[u8], keyed: (&Secret, KeyId, Keyed)) -> Vec<u8> {
        let mut packer = Packer::keyed(keyed);
        let mut bytes = packer.push(data);
        bytes.extend_from_slice(&packer.finish(footer));
        bytes
    }
}

#[cfg(feature = "crypto")]
pub use packing::{
    PackedPart, Packer, PartPacker, RawPart, framed, framed_keyed, packed_len, restamp,
};

/// Where the pieces of an object [`framed`] are, as its trailer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub data_len: u64,
    /// The bytes of the parts stored, which the footer follows.
    pub stored_len: u64,
    pub footer_len: u64,
    footer_crc: u32,
    /// cladefs: whether its parts are encrypted, and how (TASK-411.2).
    pub encryption: Encryption,
    footer_mac: [u8; MAC_LEN],
    mac: [u8; MAC_LEN],
    table_crc: u32,
}

impl Frame {
    /// The frame an object's last bytes, its [`TRAILER`] at their end, give.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn of_trailer(bytes: &[u8]) -> Result<Self, Corrupt> {
        // Its version first: a frame of another version, its trailer perhaps of another length,
        // is named.
        if let Some(refused) = refused(bytes) {
            return Err(corrupt(format_args!("trailer: {refused}")));
        }
        if !bytes.ends_with(&MAGIC) {
            return Err(corrupt("trailer, by its magic"));
        }
        let at = bytes.len().checked_sub(TRAILER as usize).ok_or_else(|| corrupt("trailer"))?;
        let trailer = bytes.get(at..).ok_or_else(|| corrupt("trailer"))?;
        let mac = |at: usize| -> Result<[u8; MAC_LEN], Corrupt> {
            slice_at(trailer, at, MAC_LEN, "trailer")?.try_into().map_err(|_| corrupt("trailer"))
        };
        let frame = Self {
            data_len: u64_at_checked(trailer, at::DATA_LEN, "trailer")?,
            stored_len: u64_at_checked(trailer, at::STORED_LEN, "trailer")?,
            footer_len: u64_at_checked(trailer, at::FOOTER_LEN, "trailer")?,
            footer_crc: u32_at_checked(trailer, at::FOOTER_CRC, "trailer")?,
            encryption: Self::encryption_of(slice_at(
                trailer,
                at::ENCRYPTION,
                ENCRYPTION_LEN,
                "trailer",
            )?)?,
            footer_mac: mac(at::FOOTER_MAC)?,
            mac: mac(at::MAC)?,
            table_crc: u32_at_checked(trailer, at::TABLE_CRC, "trailer")?,
        };
        // Lengths so large that the object's would overflow are corrupt.
        (frame.data_len.checked_add(PART))
            .and_then(|_| frame.stored_len.checked_add(frame.footer_len))
            .and_then(|end| end.checked_add(frame.parts().checked_mul(ENTRY)?))
            .and_then(|end| end.checked_add(TRAILER))
            .ok_or_else(|| corrupt("trailer, by its lengths"))?;
        Ok(frame)
    }

    /// The encryption a trailer's [`ENCRYPTION_LEN`] bytes say.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn encryption_of(encryption: &[u8]) -> Result<Encryption, Corrupt> {
        let Some(&kind) = encryption.first() else {
            return Err(corrupt("trailer"));
        };
        match kind {
            // cladefs: no object is in the clear (TASK-411.28).
            0 => Err(corrupt("trailer: an encryption in the clear, which no object has")),
            1..=3 => {
                let key = KeyId {
                    cell: u64_at_checked(encryption, 1, "trailer")?,
                    generation: u32_at_checked(encryption, 9, "trailer")?,
                };
                let salt = Salt(
                    slice_at(encryption, 13, SALT_LEN, "trailer")?
                        .try_into()
                        .map_err(|_| corrupt("trailer"))?,
                );
                Ok(match kind {
                    1 => Encryption::Encrypted { key, salt },
                    2 => Encryption::Authenticated { key, salt },
                    _ => Encryption::Sectors { key, salt },
                })
            }
            kind => Err(corrupt(format_args!("trailer: an encryption of kind {kind}"))),
        }
    }

    /// The frame of a whole object.
    pub fn of(bytes: &[u8]) -> Result<Self, Corrupt> {
        let frame = Self::of_trailer(bytes)?;
        if frame.size() != bytes.len() as u64 {
            return Err(corrupt("length, by its trailer"));
        }
        Ok(frame)
    }

    pub fn parts(&self) -> u64 {
        self.data_len.div_ceil(PART)
    }

    /// Where part `part` is in the data.
    pub fn part(&self, part: u64) -> Range<u64> {
        (part * PART).min(self.data_len)..((part + 1) * PART).min(self.data_len)
    }

    /// The parts `range` of the data touches.
    pub fn parts_of(&self, range: Range<u64>) -> Range<u64> {
        if range.is_empty() {
            return 0..0;
        }
        range.start / PART..range.end.div_ceil(PART)
    }

    pub fn footer(&self) -> Range<u64> {
        self.stored_len..self.stored_len + self.footer_len
    }

    pub fn table(&self) -> Range<u64> {
        let at = self.footer().end;
        at..at + ENTRY * self.parts()
    }

    /// What follows the parts: the footer, the table and the trailer.
    pub fn tail(&self) -> Range<u64> {
        self.stored_len..self.size()
    }

    /// The object's length.
    pub fn size(&self) -> u64 {
        self.table().end + TRAILER
    }

    /// The footer in `bytes`, checked against its CRC; and against its MAC, if the object is
    /// encrypted under a key `keys` holds (TASK-411.2). A footer read with its trailer alone, not
    /// its table, is checked so: its MAC, under the object's subkeys, says the object's writer
    /// wrote it with that trailer. cladefs: refused as [`Opening`] refuses an object (TASK-411.3).
    pub fn check_footer<'a>(&self, bytes: &'a [u8], keys: &dyn Keys) -> Result<&'a [u8], Error> {
        Ok(self.check_footer_opened(bytes, &Opening::of(self.encryption, keys)?)?)
    }

    /// The footer in `bytes`, checked against its CRC, and its MAC, if `opening` opens it.
    fn check_footer_opened<'a>(
        &self,
        bytes: &'a [u8],
        opening: &Opening,
    ) -> Result<&'a [u8], Corrupt> {
        if bytes.len() as u64 != self.footer_len || crc(bytes) != self.footer_crc {
            return Err(corrupt("footer, by its CRC"));
        }
        #[cfg(feature = "crypto")]
        if let Some(key) = opening.key() {
            key.mac.verify(bytes, &self.footer_mac).map_err(|_| corrupt("footer, by its MAC"))?;
        }
        #[cfg(not(feature = "crypto"))]
        let _ = opening;
        Ok(bytes)
    }

    /// Where its parts are stored, and their tags, as the table in `suffix`, the object's last
    /// bytes, which must hold it, says, checked against its CRC; and where in `suffix` the table
    /// begins.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn stored_in(&self, suffix: &[u8]) -> Result<(Vec<Stored>, Vec<Tag>, usize), Corrupt> {
        let table_and_trailer = self
            .parts()
            .checked_mul(ENTRY)
            .and_then(|table_len| table_len.checked_add(TRAILER))
            .ok_or_else(|| corrupt("table, cut short"))?;
        let at = (suffix.len() as u64)
            .checked_sub(table_and_trailer)
            .ok_or_else(|| corrupt("table, cut short"))?;
        let at = usize::try_from(at).map_err(|_| corrupt("table, cut short"))?;
        let trailer =
            suffix.len().checked_sub(TRAILER as usize).ok_or_else(|| corrupt("trailer"))?;
        let crc_end = trailer.checked_add(at::TABLE_CRC).ok_or_else(|| corrupt("trailer"))?;
        let crc_bytes = suffix.get(at..crc_end).ok_or_else(|| corrupt("table, cut short"))?;
        if crc(crc_bytes) != self.table_crc {
            return Err(corrupt("table, by its CRC"));
        }
        let table = suffix.get(at..trailer).ok_or_else(|| corrupt("table, cut short"))?;
        let count = usize::try_from(self.parts()).map_err(|_| corrupt("table, cut short"))?;
        let mut stored = Vec::with_capacity(count);
        let mut tags = Vec::with_capacity(count);
        let mut end = 0u64;
        let chunks = table.chunks_exact(ENTRY as usize);
        if !chunks.remainder().is_empty() {
            return Err(corrupt("table, cut short"));
        }
        for (part, entry) in chunks.enumerate() {
            let &encoding_byte = entry.get(16).ok_or_else(|| corrupt("table, cut short"))?;
            let encoding = match encoding_byte {
                0 => Encoding::Raw,
                1 => Encoding::Zstd,
                other => return Err(corrupt(format_args!("part {part}'s encoding {other}"))),
            };
            let entry_stored = Stored {
                offset: u64_at_checked(entry, 0, "table, cut short")?,
                len: u64::from(u32_at_checked(entry, 8, "table, cut short")?),
                crc: u32_at_checked(entry, 12, "table, cut short")?,
                encoding,
            };
            let tag: Tag = slice_at(entry, 17, TAG_LEN, "table, cut short")?
                .try_into()
                .map_err(|_| corrupt("table, cut short"))?;
            let part_u64 = part as u64;
            let part_start = part_u64
                .checked_mul(PART)
                .ok_or_else(|| corrupt(format_args!("part {part}'s place")))?
                .min(self.data_len);
            let part_end = part_u64
                .checked_add(1)
                .and_then(|next| next.checked_mul(PART))
                .ok_or_else(|| corrupt(format_args!("part {part}'s place")))?
                .min(self.data_len);
            let len = part_end
                .checked_sub(part_start)
                .ok_or_else(|| corrupt(format_args!("part {part}'s place")))?;
            let fits = match encoding {
                Encoding::Raw => entry_stored.len == len,
                Encoding::Zstd => entry_stored.len > 0 && entry_stored.len < len,
            };
            // cladefs: a raw file's sectors, whole, as they are (TASK-411.23).
            let sectors = match self.encryption {
                Encryption::Sectors { .. } => {
                    encoding == Encoding::Raw && entry_stored.len.is_multiple_of(SECTOR as u64)
                }
                _ => true,
            };
            if !sectors {
                return Err(corrupt(format_args!("part {part}, not whole sectors of a raw file")));
            }
            let entry_end = entry_stored
                .offset
                .checked_add(entry_stored.len)
                .ok_or_else(|| corrupt(format_args!("part {part}'s place")))?;
            if !fits
                || !entry_stored.offset.is_multiple_of(ALIGN)
                || entry_stored.offset < end
                || entry_end > self.stored_len
            {
                return Err(corrupt(format_args!("part {part}'s place")));
            }
            end = entry_end;
            stored.push(entry_stored);
            tags.push(tag);
        }
        Ok((stored, tags, at))
    }
}

/// An object's frame, where its parts are stored, and how its reader opens them: what a reader of
/// its data needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parts {
    pub frame: Frame,
    pub stored: Vec<Stored>,
    tags: Vec<Tag>,
    opening: Opening,
}

impl Parts {
    /// The parts of the object whose last bytes are `suffix`, which must hold its table, read by a
    /// reader holding `keys`: its table checked against its CRC, and against its MAC if it is
    /// encrypted under a key `keys` holds (TASK-411.2).
    pub fn of_suffix(suffix: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        Self::in_suffix(Frame::of_trailer(suffix)?, suffix, keys)
    }

    /// The parts of a whole object, read by a reader holding `keys`, as [`Parts::of_suffix`].
    pub fn of(bytes: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        Self::in_suffix(Frame::of(bytes)?, bytes, keys)
    }

    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    fn in_suffix(frame: Frame, suffix: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        #[allow(unused_variables)]
        let (stored, tags, table) = frame.stored_in(suffix)?;
        let opening = Opening::of(frame.encryption, keys)?;
        #[cfg(feature = "crypto")]
        if let Some(key) = opening.key() {
            let trailer =
                suffix.len().checked_sub(TRAILER as usize).ok_or_else(|| corrupt("trailer"))?;
            let mac_end = trailer.checked_add(at::MAC).ok_or_else(|| corrupt("trailer"))?;
            let version_at = trailer.checked_add(at::VERSION).ok_or_else(|| corrupt("trailer"))?;
            let authenticated = [
                suffix.get(table..mac_end).ok_or_else(|| corrupt("table, cut short"))?,
                slice_at(suffix, version_at, 4, "trailer")?,
            ];
            let verified = key.mac.verify_of(&authenticated, &frame.mac);
            verified.map_err(|_| corrupt("table, by its MAC"))?;
        }
        Ok(Self { frame, stored, tags, opening })
    }

    /// cladefs: whether its reader opens its parts: they are under a key it holds, or authenticated
    /// alone, read as stored (TASK-411.2).
    pub fn opens(&self) -> bool {
        !matches!(self.opening, Opening::Locked(_))
    }

    /// The footer in `bytes`, checked against its CRC, and against its MAC if its reader holds the
    /// object's key ([`Frame::check_footer`]).
    pub fn check_footer<'a>(&self, bytes: &'a [u8]) -> Result<&'a [u8], Corrupt> {
        self.frame.check_footer_opened(bytes, &self.opening)
    }

    /// Part `part`, as its reader reads it.
    pub fn part(&self, part: u64) -> Result<Part, Corrupt> {
        let stored = self.stored.get(part as usize).ok_or_else(|| corrupt("part, past its end"))?;
        Ok(Part {
            index: part,
            place: self.frame.part(part),
            stored: *stored,
            tag: self.tags[part as usize],
            opening: self.opening.clone(),
        })
    }

    /// The bytes `range` of the data, reading only the stored bytes of the parts it touches, by
    /// `read`, which gives the bytes of a range of the object.
    pub fn read<E: From<Error>>(
        &self,
        range: Range<u64>,
        mut read: impl FnMut(Range<u64>) -> Result<Vec<u8>, E>,
    ) -> Result<Vec<u8>, E> {
        if range.start > range.end || range.end > self.frame.data_len {
            return Err(Error::from(corrupt("data, read past its end")).into());
        }
        let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
        for part in self.frame.parts_of(range.clone()) {
            let part = self.part(part).map_err(Error::from)?;
            let unpacked = part.unpack(&read(part.stored.range())?)?;
            let from = range.start.max(part.place.start) - part.place.start;
            let to = range.end.min(part.place.end) - part.place.start;
            bytes.extend_from_slice(&unpacked[from as usize..to as usize]);
        }
        Ok(bytes)
    }
}

/// The data of a whole object [`framed`], each part checked against its CRC, and opened, if
/// encrypted, under a key `keys` holds.
pub fn data(bytes: &[u8], keys: &dyn Keys) -> Result<Vec<u8>, Error> {
    let parts = Parts::of(bytes, keys)?;
    parts.read(0..parts.frame.data_len, |range| {
        Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
    })
}

/// Checks a whole object [`framed`]: its trailer, table and footer, and each part's stored bytes
/// against its CRC, unpacking none. cladefs: what a host checks of an object it sends on as it is,
/// as an uploader does a cell's closed segments (TASK-153.11). With no key: an object's MACs and
/// tags are its readers' to check (TASK-411.2); and what tells a whole object from one cut short,
/// as a cell's recovery of its write-ahead log does (TASK-411.3).
pub fn check(bytes: &[u8]) -> Result<(), Corrupt> {
    let parts = Parts::of(bytes, &NoKeys).map_err(|err| match err {
        Error::Corrupt(corrupt) => corrupt,
        Error::Format(refused) => Corrupt(refused.to_string()),
        Error::NoKey(_) => unreachable!("a reader holding no key refuses nothing"),
    })?;
    let footer = parts.frame.footer();
    parts.check_footer(&bytes[footer.start as usize..footer.end as usize])?;
    for stored in &parts.stored {
        stored.check(&bytes[stored.offset as usize..])?;
    }
    Ok(())
}

/// cladefs: whether `suffix`, an object's last bytes, its trailer at their end, is a frame of this
/// format whose encrypt says it is in the clear, kind 0, as objects were before TASK-411.28: what a
/// checker of a cell's files, holding no key, tells one left so by, which every reader refuses as
/// corrupt ([`Frame::of_trailer`]).
pub fn in_the_clear(suffix: &[u8]) -> bool {
    let Some(at) = suffix.len().checked_sub(TRAILER as usize) else { return false };
    suffix.ends_with(&MAGIC) && suffix[at + at::ENCRYPTION] == 0
}

/// The footer of a whole object [`framed`], checked against its CRC, and its MAC if it is encrypted
/// under a key `keys` holds.
pub fn footer<'a>(bytes: &'a [u8], keys: &dyn Keys) -> Result<&'a [u8], Error> {
    let parts = Parts::of(bytes, keys)?;
    let footer = parts.frame.footer();
    Ok(parts.check_footer(&bytes[footer.start as usize..footer.end as usize])?)
}

#[cfg(all(test, feature = "crypto"))]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The DEK of the tests' encrypted objects, and which it is.
    const ID: KeyId = KeyId { cell: 0x7b, generation: 2 };

    fn dek() -> Secret {
        Secret::from_bytes(&[0x5a; extent_keys::KEY_LEN])
    }

    /// A reader holding `dek` as [`ID`]'s.
    fn holding(dek: Secret) -> HashMap<KeyId, Secret> {
        HashMap::from([(ID, dek)])
    }

    /// Data that compresses: a pattern, which differs between parts.
    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ (i / PART) as u8).collect()
    }

    /// Data that does not: from a xorshift generator.
    fn random(len: u64) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// A packer of an object encrypted under [`dek`] with `salt`, as no packer outside the tests
    /// is: so that two pack alike.
    fn encrypted_with(salt: Salt) -> Packer {
        Packer::with(Arc::new(ObjectKey::new(&dek(), ID, salt, Keyed::Encrypted)))
    }

    /// Recomputes the CRCs of `bytes`, an object's, its footer's and its table's, as one who
    /// changes an object holding no key does.
    fn recrc(bytes: &mut [u8]) {
        let trailer = bytes.len() - TRAILER as usize;
        let stored_len = u64_at(bytes, trailer + at::STORED_LEN) as usize;
        let footer = stored_len..stored_len + u64_at(bytes, trailer + at::FOOTER_LEN) as usize;
        let footer_crc = crc(&bytes[footer.clone()]);
        bytes[trailer + at::FOOTER_CRC..][..4].copy_from_slice(&footer_crc.to_le_bytes());
        let table_crc = crc(&bytes[footer.end..trailer + at::TABLE_CRC]);
        bytes[trailer + at::TABLE_CRC..][..4].copy_from_slice(&table_crc.to_le_bytes());
    }

    /// The corruption `result` failed with, if it failed so: what tests of corrupt objects expect.
    fn corrupt_of<T>(result: Result<T, Error>) -> Result<T, Corrupt> {
        result.map_err(|err| match err {
            Error::Corrupt(corrupt) => corrupt,
            err => panic!("not corrupt: {err}"),
        })
    }

    /// Where part `part`'s entry is in the object of `frame`.
    fn entry(frame: &Frame, part: u64) -> usize {
        (frame.table().start + part * ENTRY) as usize
    }

    /// Whether `bytes` hold `piece` anywhere.
    fn holds(bytes: &[u8], piece: &[u8]) -> bool {
        bytes.windows(piece.len()).any(|window| window == piece)
    }

    /// cladefs: parts packed one at a time pack as `push` packs them all (TASK-336), encrypted and
    /// authenticated (TASK-411.2, TASK-411.3).
    #[test]
    fn parts_packed_one_at_a_time_pack_as_all_at_once() {
        let mut data: Vec<u8> = (0..5 * PART as usize).map(|i| (i / 4096 % 7) as u8).collect();
        // A part that doesn't compress, and a last part cut short.
        let mut state = 1u64;
        for byte in &mut data[PART as usize..2 * PART as usize] {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *byte = (state >> 56) as u8;
        }
        data.truncate(data.len() - 1000);
        let salt = Salt([3; SALT_LEN]);
        let authenticated = |salt| {
            let key = ObjectKey::new(&dek(), ID, salt, Keyed::Authenticated);
            Packer::with(Arc::new(key))
        };
        for (mut all, mut one) in [
            (encrypted_with(salt), encrypted_with(salt)),
            (authenticated(salt), authenticated(salt)),
        ] {
            let pushed = all.push(&data);
            let (mut bytes, mut packer) = (Vec::new(), PartPacker::new());
            for (index, part) in data.chunks(PART as usize).enumerate() {
                let packed = packer.pack(index as u64, part, one.key());
                one.push_part(&mut bytes, part, &packed);
            }
            assert_eq!(bytes, pushed);
            assert!(!one.stored_raw(), "parts compressed");
            assert_eq!(one.finish(b"footer"), all.finish(b"footer"));
        }
    }

    /// TASK-411.2: a part packed for another index than its object's next, or compressed alone
    /// (TASK-421), or keyed otherwise than its object, is no part of it.
    #[test]
    #[should_panic(expected = "a part packed as the object's next")]
    fn parts_packed_out_of_order_are_refused() {
        let mut packer = encrypted_with(Salt([3; SALT_LEN]));
        let packed = PartPacker::new().pack(1, b"part", packer.key());
        packer.push_part(&mut Vec::new(), b"part", &packed);
    }

    #[test]
    #[should_panic(expected = "a part encrypted as its object is")]
    fn parts_compressed_alone_are_refused() {
        let packed = PartPacker::new().compress(0, b"part");
        encrypted_with(Salt([3; SALT_LEN])).push_part(&mut Vec::new(), b"part", &packed);
    }

    #[test]
    #[should_panic(expected = "a part encrypted as its object is")]
    fn parts_authenticated_into_encrypted_objects_are_refused() {
        let key = ObjectKey::new(&dek(), ID, Salt([3; SALT_LEN]), Keyed::Authenticated);
        let packed = PartPacker::new().pack(0, b"part", &key);
        encrypted_with(Salt([3; SALT_LEN])).push_part(&mut Vec::new(), b"part", &packed);
    }

    /// TASK-411.2: an encrypted object's bytes are never its data as they are.
    #[test]
    #[should_panic(expected = "an encrypted object's bytes are not its data")]
    fn encrypted_objects_store_no_parts_as_they_are() {
        encrypted_with(Salt([3; SALT_LEN])).push_raw_parts([]);
    }

    /// Objects hold their data in parts, compressed, each where the table says, and their footers;
    /// cladefs: encrypted, their trailers saying so (TASK-411.2, TASK-411.28).
    #[test]
    fn objects_hold_their_parts() {
        let data = pattern(3 * PART + 7);
        let keys = holding(dek());
        let bytes = framed(&data, b"footer", (&dek(), ID));
        let parts = Parts::of(&bytes, &keys).unwrap();
        let frame = parts.frame;
        assert_eq!((frame.data_len, frame.footer_len, frame.parts()), (3 * PART + 7, 6, 4));
        assert!(matches!(frame.encryption, Encryption::Encrypted { key: ID, .. }));
        assert!(parts.opens());
        assert_eq!(frame.part(3), 3 * PART..3 * PART + 7);
        assert_eq!(frame.size(), bytes.len() as u64);
        assert_eq!(
            parts.stored.iter().map(|stored| stored.encoding).collect::<Vec<_>>(),
            // The last part, of 7 bytes, does not shrink.
            [Encoding::Zstd, Encoding::Zstd, Encoding::Zstd, Encoding::Raw]
        );
        assert!(frame.stored_len < PART / 10, "{} bytes stored", frame.stored_len);
        for index in 0..frame.parts() {
            let part = parts.part(index).unwrap();
            assert_eq!(part.stored.offset % ALIGN, 0);
            let unpacked = part.unpack(&bytes[part.stored.offset as usize..]);
            assert_eq!(
                unpacked.unwrap(),
                &data[part.place.start as usize..part.place.end as usize]
            );
        }
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(footer(&bytes, &keys).unwrap(), b"footer");
        // The table is found from a suffix holding it.
        let suffix = &bytes[frame.table().start as usize - 3..];
        assert_eq!(Parts::of_suffix(suffix, &keys).unwrap(), parts);
        assert_eq!(Frame::of_trailer(&bytes[bytes.len() - TRAILER as usize..]).unwrap(), frame);
        assert!(frame.stored_in(&bytes[frame.table().start as usize + 1..]).is_err());

        let empty = framed(&[], &[], (&dek(), ID));
        assert_eq!(Frame::of(&empty).unwrap().data_len, 0);
        assert_eq!(empty.len() as u64, TRAILER);
        assert_eq!(super::data(&empty, &keys).unwrap(), Vec::<u8>::new());

        // Written in pieces, front to back, it is the same object.
        let Encryption::Encrypted { salt, .. } = frame.encryption else {
            unreachable!("encrypted")
        };
        let mut packer = encrypted_with(salt);
        let mut pieces = packer.push(&data[..2 * PART as usize]);
        assert_eq!(packer.end(), pieces.len() as u64);
        pieces.extend(packer.push(&data[2 * PART as usize..]));
        let (tail, written) = packer.finish_parts(b"footer");
        pieces.extend(tail);
        assert_eq!(pieces, bytes);
        assert_eq!(written, parts);
    }

    /// TASK-411.2: an encrypted object holds none of its data's bytes, nor those its parts compress
    /// to; its parts are as long as compressing them makes them, encrypting adding nothing but
    /// their tags; its trailer names its key and salt; it reads back, whole and by parts, under its
    /// key, and its footer with it; written in pieces it is the same object.
    #[test]
    fn encrypted_objects_hold_no_plaintext_and_read_back_under_their_key() {
        let data = [pattern(2 * PART + 5), random(PART + 9)].concat();
        let salt = Salt([9; SALT_LEN]);
        let mut packer = encrypted_with(salt);
        let mut bytes = packer.push(&data[..PART as usize]);
        bytes.extend(packer.push(&data[PART as usize..]));
        bytes.extend(packer.finish(b"footer"));
        let mut once = encrypted_with(salt);
        let mut whole = once.push(&data);
        let (tail, written) = once.finish_parts(b"footer");
        whole.extend(tail);
        assert_eq!(bytes, whole);

        let keys = holding(dek());
        let encrypted = Parts::of(&bytes, &keys).unwrap();
        // Its writer reads it as a reader holding its key does.
        assert_eq!(written, encrypted);
        assert_eq!(encrypted.frame.encryption, Encryption::Encrypted { key: ID, salt });
        assert!(encrypted.opens());
        // Each part as long as compressing it makes it: encrypting adds nothing but its tag.
        let mut compressing = PartPacker::new();
        let compressed: Vec<PackedPart> = (data.chunks(PART as usize).zip(0..))
            .map(|(part, index)| compressing.compress(index, part))
            .collect();
        let encoded: Vec<(Encoding, u64)> = (compressed.iter().zip(data.chunks(PART as usize)))
            .map(|(packed, part)| (packed.encoding(), packed.stored(part).len() as u64))
            .collect();
        let encodings: Vec<(Encoding, u64)> =
            encrypted.stored.iter().map(|stored| (stored.encoding, stored.len)).collect();
        assert_eq!(encodings, encoded);
        assert_eq!(
            encoded.iter().map(|(encoding, _)| *encoding).collect::<Vec<_>>(),
            [Encoding::Zstd, Encoding::Zstd, Encoding::Raw, Encoding::Raw]
        );
        for (index, (packed, part)) in compressed.iter().zip(data.chunks(PART as usize)).enumerate()
        {
            // The first 64 bytes of what the part compresses to, and of its data.
            let stored = packed.stored(part);
            assert!(!holds(&bytes, &stored[..stored.len().min(64)]), "part {index}");
            assert!(!holds(&bytes, &part[..part.len().min(64)]), "part {index}'s data");
        }
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(footer(&bytes, &keys).unwrap(), b"footer");
        for index in 0..encrypted.frame.parts() {
            let part = encrypted.part(index).unwrap();
            assert!(!part.as_is());
            let expected = &data[part.place.start as usize..part.place.end as usize];
            let stored = &bytes[part.stored.offset as usize..];
            assert_eq!(part.unpack(stored).unwrap(), expected, "part {index}");
            // Opened in place, then unpacked.
            let (mut opened, mut out) = (stored.to_vec(), vec![0; part.len() as usize]);
            part.unpack_into(&mut opened, &mut out).unwrap();
            assert_eq!(out, expected, "part {index}, opened in place");
        }
        // Its table is found from a suffix holding it, checked against its MAC.
        let suffix = &bytes[encrypted.frame.table().start as usize - 1..];
        assert_eq!(Parts::of_suffix(suffix, &keys).unwrap(), encrypted);
        let read = encrypted.read(PART - 3..2 * PART + 2, |range| {
            Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
        });
        assert_eq!(read.unwrap(), &data[PART as usize - 3..2 * PART as usize + 2]);

        // An encrypted object of no data: its trailer alone, encrypted.
        let empty = framed(&[], b"", (&dek(), ID));
        assert_eq!(empty.len() as u64, TRAILER);
        assert_eq!(super::data(&empty, &keys).unwrap(), Vec::<u8>::new());
        // Each object's salt its own.
        let (one, other) = (framed(b"x", b"", (&dek(), ID)), framed(b"x", b"", (&dek(), ID)));
        assert_ne!(Frame::of(&one).unwrap().encryption, Frame::of(&other).unwrap().encryption);
    }

    /// TASK-411.2: an encrypted object read by a reader not holding its key: its footer and table
    /// read and checked against their CRCs, its parts' stored bytes checked, as the collector reads
    /// them; its data read by none, refused naming its key, never given as it is stored; restamped
    /// by none. Under another key of the same name: refused, by its MAC first, by its parts' tags
    /// past it.
    #[test]
    fn encrypted_objects_open_under_their_key_alone() {
        let data = [random(PART), pattern(PART / 2)].concat();
        let bytes = framed(&data, b"footer", (&dek(), ID));
        let locked = Parts::of(&bytes, &NoKeys).unwrap();
        assert!(!locked.opens());
        assert_eq!(footer(&bytes, &NoKeys).unwrap(), b"footer");
        assert_eq!(check(&bytes), Ok(()));
        let no_key = Error::NoKey(ID);
        assert_eq!(
            no_key.to_string(),
            "an object encrypted under cell 3v's key of generation 2, which its reader doesn't hold"
        );
        assert_eq!(super::data(&bytes, &NoKeys), Err(no_key.clone()));
        let other_generation = HashMap::from([(KeyId { generation: 3, ..ID }, dek())]);
        assert_eq!(super::data(&bytes, &other_generation), Err(no_key.clone()));
        for index in 0..2 {
            let part = locked.part(index).unwrap();
            let stored = &bytes[part.stored.offset as usize..];
            assert_eq!(part.stored.check(stored).map(<[u8]>::len), Ok(part.stored.len as usize));
            assert_eq!(part.unpack(stored), Err(no_key.clone()), "part {index}");
            let mut opened = stored.to_vec();
            assert_eq!(part.open(&mut opened), Err(no_key.clone()), "part {index}");
        }
        assert_eq!(restamp(&bytes, b"another", &NoKeys), Err(no_key));

        // Another key under its key's name.
        let wrong = holding(Secret::from_bytes(&[0xa5; extent_keys::KEY_LEN]));
        let by_mac = Corrupt("an object's table, by its MAC".into());
        assert_eq!(corrupt_of(Parts::of(&bytes, &wrong)), Err(by_mac.clone()));
        assert_eq!(super::data(&bytes, &wrong), Err(by_mac.clone().into()));
        assert_eq!(corrupt_of(footer(&bytes, &wrong)), Err(by_mac.clone()));
        assert_eq!(restamp(&bytes, b"another", &wrong), Err(by_mac.into()));
        let Encryption::Encrypted { salt, .. } = locked.frame.encryption else {
            panic!("encrypted")
        };
        let keys = holding(dek());
        let opened = Parts::of(&bytes, &keys).unwrap();
        let wrong_key =
            Opening::Open(Arc::new(ObjectKey::new(&wrong[&ID], ID, salt, Keyed::Encrypted)));
        let by_tag = Error::Corrupt(Corrupt("an object's part, by its tag".into()));
        for index in 0..2 {
            let part = Part { opening: wrong_key.clone(), ..opened.part(index).unwrap() };
            assert_eq!(part.unpack(&bytes[part.stored.offset as usize..]), Err(by_tag.clone()));
        }
        // Its footer, checked with its trailer alone, against its MAC.
        let frame = opened.frame;
        let footer_bytes = &bytes[frame.footer().start as usize..frame.footer().end as usize];
        assert_eq!(frame.check_footer(footer_bytes, &holding(dek())), Ok(&b"footer"[..]));
        let by_footer_mac = Err(Corrupt("an object's footer, by its MAC".into()));
        assert_eq!(corrupt_of(frame.check_footer(footer_bytes, &wrong)), by_footer_mac);
    }

    /// A reader holding `keys`, which holds the objects it reads encrypted: a reader of a cell
    /// whose record has a DEK (TASK-411.3).
    struct Encrypting(HashMap<KeyId, Secret>);

    impl Keys for Encrypting {
        fn dek(&self, id: KeyId) -> Option<&Secret> {
            self.0.get(&id)
        }

        fn encrypted(&self) -> bool {
            true
        }
    }

    /// TASK-411.3: an authenticated object holds its parts as compressing them stores them,
    /// unencrypted, tagged: read alike by a reader holding no key, which checks their CRCs alone,
    /// as the collector reads a checkpoint, and by one holding its key, which checks their tags and
    /// its MACs too, and refuses a part changed, its CRC made to match. Restamped under its key
    /// alone. A reader holding its objects encrypted refuses one authenticated under a key it
    /// doesn't hold.
    #[test]
    fn authenticated_objects_read_keyless_and_check_under_their_key() {
        let data = [pattern(PART + 3), random(PART / 2)].concat();
        let keys = holding(dek());
        let bytes = framed_keyed(&data, b"footer", (&dek(), ID, Keyed::Authenticated));
        let parts = Parts::of(&bytes, &keys).unwrap();
        assert!(matches!(parts.frame.encryption, Encryption::Authenticated { key: ID, .. }));
        let mut compressing = PartPacker::new();
        for ((part, index), stored) in data.chunks(PART as usize).zip(0..).zip(&parts.stored) {
            let compressed = compressing.compress(index, part);
            let at = &bytes[stored.range().start as usize..stored.range().end as usize];
            assert_eq!(at, compressed.stored(part), "part {index}, as compressed");
        }
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(super::data(&bytes, &NoKeys).unwrap(), data);
        assert_eq!(footer(&bytes, &NoKeys).unwrap(), b"footer");
        assert_eq!(footer(&bytes, &keys).unwrap(), b"footer");
        assert_eq!(check(&bytes), Ok(()));
        assert!(!parts.part(1).unwrap().as_is(), "its tag is checked");
        assert!(Parts::of(&bytes, &NoKeys).unwrap().part(1).unwrap().as_is());

        // Part 1's stored bytes changed, their CRC made to match.
        let part = parts.part(1).unwrap();
        let mut changed =
            bytes[part.stored.range().start as usize..][..part.stored.len as usize].to_vec();
        changed[10] ^= 1;
        let matching =
            Part { stored: Stored { crc: crc(&changed), ..part.stored }, ..part.clone() };
        let by_tag = Error::Corrupt(Corrupt("an object's part, by its tag".into()));
        assert_eq!(matching.unpack(&changed), Err(by_tag.clone()));
        let mut opened = changed.clone();
        assert_eq!(matching.open(&mut opened), Err(by_tag));
        let unchecked = Parts::of(&bytes, &NoKeys).unwrap().part(1).unwrap();
        let unchecked = Part { stored: matching.stored, ..unchecked };
        assert_eq!(
            unchecked.unpack(&changed).map(|unpacked| unpacked.len()),
            Ok(PART as usize / 2 + 3)
        );

        // Restamped under its key, as an encrypted one is; by no reader not holding it.
        let restamped = restamp(&bytes, b"another", &keys).unwrap();
        assert_eq!(footer(&restamped, &keys).unwrap(), b"another");
        assert_eq!(super::data(&restamped, &keys).unwrap(), data);
        assert_eq!(restamp(&bytes, b"another", &NoKeys), Err(Error::NoKey(ID)));

        // A reader holding its objects encrypted refuses one authenticated under a key it lacks.
        let encrypting = Encrypting(HashMap::from([(ID, dek())]));
        assert_eq!(super::data(&bytes, &encrypting).unwrap(), data);
        let other = KeyId { generation: 9, ..ID };
        let authenticated = framed_keyed(&data, b"footer", (&dek(), other, Keyed::Authenticated));
        assert_eq!(super::data(&authenticated, &encrypting), Err(Error::NoKey(other)));
        assert_eq!(super::data(&authenticated, &NoKeys).unwrap(), data);
    }

    /// TASK-411.28: no object is in the clear. One whose encryption, MACs and tags were zeroed, its
    /// CRCs made to match, as one who can write the bucket, holding no key, would, to have it read
    /// in the clear (a downgrade), is corrupt to every reader, holding its key or none, by its
    /// trailer: whole, by its suffix, its footer, its data, a restamping, the keyless check.
    #[test]
    fn objects_in_the_clear_are_corrupt() {
        let data = pattern(PART + 1);
        let encrypting = Encrypting(HashMap::from([(ID, dek())]));
        let encrypted = framed(&data, b"footer", (&dek(), ID));
        assert_eq!(super::data(&encrypted, &encrypting).unwrap(), data);
        let mut zeroed = encrypted.clone();
        let frame = Frame::of(&zeroed).unwrap();
        let trailer = zeroed.len() - TRAILER as usize;
        zeroed[trailer + at::ENCRYPTION..trailer + at::TABLE_CRC].fill(0);
        for part in 0..frame.parts() {
            let tag = entry(&frame, part) + 17;
            zeroed[tag..tag + TAG_LEN].fill(0);
        }
        recrc(&mut zeroed);
        let clear = corrupt("trailer: an encryption in the clear, which no object has");
        assert_eq!(Frame::of(&zeroed), Err(clear.clone()));
        assert_eq!(check(&zeroed), Err(clear.clone()));
        for keys in [&encrypting as &dyn Keys, &holding(dek()), &NoKeys] {
            assert_eq!(corrupt_of(Parts::of(&zeroed, keys)), Err(clear.clone()));
            let suffix = &zeroed[frame.table().start as usize..];
            assert_eq!(corrupt_of(Parts::of_suffix(suffix, keys)), Err(clear.clone()));
            assert_eq!(super::data(&zeroed, keys), Err(clear.clone().into()));
            assert_eq!(footer(&zeroed, keys), Err(clear.clone().into()));
            assert_eq!(restamp(&zeroed, b"another", keys), Err(clear.clone().into()));
        }
    }

    /// TASK-411.2: an encrypted part changed, its CRC with it or not, its tag changed, or moved to
    /// another index or another object of the same key, never reads as data: the CRC finds a part
    /// changed by itself; its tag, one whose CRC was made to match; the table's MAC, a table made
    /// to say any of it; each layer alone.
    #[test]
    fn encrypted_parts_changed_or_moved_fail() {
        let data = random(3 * PART);
        let keys = holding(dek());
        let bytes = framed(&data, b"footer", (&dek(), ID));
        let parts = Parts::of(&bytes, &keys).unwrap();
        let by_crc = Error::Corrupt(Corrupt("an object's part, by its CRC".into()));
        let by_tag = Error::Corrupt(Corrupt("an object's part, by its tag".into()));
        let by_mac = Corrupt("an object's table, by its MAC".into());
        let stored = |part: &Part, bytes: &[u8]| {
            bytes[part.stored.range().start as usize..part.stored.range().end as usize].to_vec()
        };
        let part = parts.part(1).unwrap();

        // A byte of its stored bytes flipped: by its CRC; its CRC made to match: by its tag.
        let mut flipped = stored(&part, &bytes);
        flipped[1000] ^= 1;
        assert_eq!(part.unpack(&flipped), Err(by_crc.clone()));
        let matching =
            Part { stored: Stored { crc: crc(&flipped), ..part.stored }, ..part.clone() };
        assert_eq!(matching.unpack(&flipped), Err(by_tag.clone()));
        let mut tampered = bytes.clone();
        tampered[part.stored.offset as usize + 1000] ^= 1;
        assert_eq!(super::data(&tampered, &keys), Err(by_crc.clone()));
        tampered[entry(&parts.frame, 1) + 12..][..4].copy_from_slice(&crc(&flipped).to_le_bytes());
        recrc(&mut tampered);
        assert_eq!(super::data(&tampered, &keys), Err(by_mac.clone().into()));

        // Its tag flipped.
        let mut tag = part.tag;
        tag[5] ^= 1;
        let flipped_tag = Part { tag, ..part.clone() };
        assert_eq!(flipped_tag.unpack(&stored(&part, &bytes)), Err(by_tag.clone()));
        let mut tampered = bytes.clone();
        tampered[entry(&parts.frame, 1) + 17 + 5] ^= 1;
        assert_eq!(
            corrupt_of(Parts::of(&tampered, &keys)),
            Err(Corrupt("an object's table, by its CRC".into()))
        );
        recrc(&mut tampered);
        assert_eq!(corrupt_of(Parts::of(&tampered, &keys)), Err(by_mac.clone()));

        // Part 2, of the same length, at index 1, its tag with it.
        let other = parts.part(2).unwrap();
        let moved = Part { index: 1, place: part.place.clone(), ..other.clone() };
        assert_eq!(moved.unpack(&stored(&other, &bytes)), Err(by_tag.clone()));
        let mut swapped = bytes.clone();
        let (one, two) = (part.stored.range(), other.stored.range());
        swapped[one.start as usize..one.end as usize].copy_from_slice(&stored(&other, &bytes));
        swapped[two.start as usize..two.end as usize].copy_from_slice(&stored(&part, &bytes));
        let (first, second) = (entry(&parts.frame, 1), entry(&parts.frame, 2));
        let entries = bytes[first..second + ENTRY as usize].to_vec();
        // Each entry's CRC and tag, the other's: its offset and length stay.
        swapped[first + 12..first + ENTRY as usize]
            .copy_from_slice(&entries[ENTRY as usize + 12..2 * ENTRY as usize]);
        swapped[second + 12..second + ENTRY as usize].copy_from_slice(&entries[12..ENTRY as usize]);
        recrc(&mut swapped);
        assert_eq!(check(&swapped), Ok(()), "its CRCs match");
        assert_eq!(super::data(&swapped, &keys), Err(by_mac.clone().into()));

        // Part 1 of another object of the same data and key, of another salt, its tag with it.
        let another = framed(&data, b"footer", (&dek(), ID));
        let theirs = Parts::of(&another, &keys).unwrap().part(1).unwrap();
        assert_eq!(theirs.stored.range(), part.stored.range());
        let transplanted = Part { opening: part.opening.clone(), ..theirs.clone() };
        assert_eq!(transplanted.unpack(&stored(&theirs, &another)), Err(by_tag));
        let mut grafted = bytes.clone();
        let range = part.stored.range();
        grafted[range.start as usize..range.end as usize]
            .copy_from_slice(&stored(&theirs, &another));
        let at = entry(&parts.frame, 1);
        grafted[at..at + ENTRY as usize].copy_from_slice(&another[at..at + ENTRY as usize]);
        recrc(&mut grafted);
        assert_eq!(check(&grafted), Ok(()), "its CRCs match");
        assert_eq!(super::data(&grafted, &keys), Err(by_mac.into()));
        // As they were, each reads.
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(super::data(&another, &keys).unwrap(), data);
    }

    /// The key of a raw file of [`dek`]'s, from `salt`, as an encrypted cell's (TASK-411.5).
    fn raw_file_key(salt: Salt) -> Arc<ObjectKey> {
        Arc::new(ObjectKey::sectors(&dek(), ID, salt))
    }

    /// Packs encrypted raw file sectors into `packer` as raw parts.
    fn push_raw_sectors(packer: &mut Packer, stored: &[u8]) {
        let first = packer.next_part();
        let parts: Vec<_> = (stored.chunks(PART as usize).zip(first..))
            .map(|(part, index)| RawPart::of(index, part, packer.key()))
            .collect();
        packer.push_raw_parts(parts);
    }

    /// The bytes of an encrypted raw file holding `data`, whole sectors, under `key`, then its
    /// tail, framed in place, as an encrypted cell frames a segment or a large extent
    /// (TASK-411.23).
    fn sectors_of(data: &[u8], footer: &[u8], key: &Arc<ObjectKey>) -> Vec<u8> {
        let mut bytes = data.to_vec();
        key.encrypt_sectors(0, &mut bytes);
        let mut packer = Packer::in_place(key.clone());
        push_raw_sectors(&mut packer, &bytes);
        assert_eq!(packer.end(), bytes.len() as u64, "its data where it is");
        bytes.extend(packer.finish(footer));
        bytes
    }

    /// TASK-411.23: an object framed in place from an encrypted raw file is the raw file's sectors
    /// as AES-256-XTS encrypted them under the raw file's key, `Xts::derive(dek, salt, "cladefs raw
    /// file")`, the sector's place its tweak, then its tail: its trailer names the DEK and the raw
    /// file's salt; it holds none of its data; it reads back under its key, whole, by parts, by
    /// ranges, and restamped; one holding no key checks it, as the collector does, and reads none
    /// of it; under another DEK of the same name it is refused by its MAC. Its parts are all raw,
    /// as long as the data.
    #[test]
    fn objects_of_raw_files_sectors_read_under_their_key_alone() {
        // Three whole parts, then one of eight sectors.
        let data = random(3 * PART + 8 * SECTOR as u64);
        let salt = Salt([4; SALT_LEN]);
        let key = raw_file_key(salt);
        let bytes = sectors_of(&data, b"footer", &key);
        let mut raw = data.clone();
        Xts::derive(&dek(), &salt, b"cladefs raw file").encrypt(0, &mut raw);
        assert!(bytes[..data.len()] == raw[..], "the raw file's sectors, as they are");
        for sector in data.chunks(SECTOR) {
            assert!(!holds(&bytes, &sector[..32]), "no plaintext");
        }
        let keys = holding(dek());
        let parts = Parts::of(&bytes, &keys).unwrap();
        assert_eq!(parts.frame.encryption, Encryption::Sectors { key: ID, salt });
        assert!(parts.opens());
        let lens: Vec<(Encoding, u64)> =
            parts.stored.iter().map(|stored| (stored.encoding, stored.len)).collect();
        let raw_part = (Encoding::Raw, PART);
        assert_eq!(lens, [raw_part, raw_part, raw_part, (Encoding::Raw, 8 * SECTOR as u64)]);
        assert_eq!(
            bytes.len() as u64,
            data.len() as u64 + parts.frame.tail().end - parts.frame.tail().start
        );
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(footer(&bytes, &keys).unwrap(), b"footer");
        for index in 0..parts.frame.parts() {
            let part = parts.part(index).unwrap();
            assert!(!part.as_is(), "part {index}: ciphertext");
            let expected = &data[part.place.start as usize..part.place.end as usize];
            let stored = &bytes[part.stored.offset as usize..];
            assert_eq!(part.unpack(stored).unwrap(), expected, "part {index}");
            let mut opened = stored[..part.stored.len as usize].to_vec();
            assert_eq!(part.open(&mut opened).unwrap(), expected, "part {index}, opened in place");
        }
        let read = parts.read(PART - 3..3 * PART + 5, |range| {
            Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
        });
        assert_eq!(read.unwrap(), &data[PART as usize - 3..3 * PART as usize + 5]);
        // Its writer reads it as a reader holding its key does.
        let mut packer = Packer::in_place(key.clone());
        push_raw_sectors(&mut packer, &raw);
        assert_eq!(packer.finish_parts(b"footer").1, parts);
        // Restamped under its key, its parts as they are.
        let restamped = restamp(&bytes, b"another", &keys).unwrap();
        assert!(restamped[..data.len()] == bytes[..data.len()]);
        assert_eq!(super::data(&restamped, &keys).unwrap(), data);
        assert_eq!(footer(&restamped, &keys).unwrap(), b"another");
        assert_eq!(restamp(&bytes, b"another", &NoKeys), Err(Error::NoKey(ID)));

        // Holding no key: checked by CRCs, read by none.
        assert_eq!(check(&bytes), Ok(()));
        let locked = Parts::of(&bytes, &NoKeys).unwrap();
        assert!(!locked.opens());
        assert_eq!(super::data(&bytes, &NoKeys), Err(Error::NoKey(ID)));
        assert_eq!(footer(&bytes, &NoKeys).unwrap(), b"footer");
        // Under another DEK of its name.
        let other = holding(Secret::from_bytes(&[1; extent_keys::KEY_LEN]));
        let by_mac = Corrupt("an object's table, by its MAC".into());
        assert_eq!(corrupt_of(super::data(&bytes, &other)), Err(by_mac));
    }

    /// TASK-411.23: a part of an object of a raw file's sectors changed, its CRC made to match, is
    /// refused by its tag before it is decrypted; so is one under another tag, at another index,
    /// or of another raw file of the same data and DEK (another salt); its table changed is refused
    /// by its MAC, which is what binds a part's place, its sectors' tweaks: read at another place,
    /// a part decrypts to other bytes. A table saying a part of sectors is compressed, or not whole
    /// sectors, is corrupt, for a reader holding no key too.
    #[test]
    fn parts_of_raw_files_sectors_changed_or_moved_fail() {
        let data = random(3 * PART);
        let keys = holding(dek());
        let bytes = sectors_of(&data, b"footer", &raw_file_key(Salt([5; SALT_LEN])));
        let parts = Parts::of(&bytes, &keys).unwrap();
        let by_crc = Error::Corrupt(Corrupt("an object's part, by its CRC".into()));
        let by_tag = Error::Corrupt(Corrupt("an object's part, by its tag".into()));
        let by_mac = Corrupt("an object's table, by its MAC".into());
        let stored = |part: &Part, bytes: &[u8]| {
            bytes[part.stored.range().start as usize..part.stored.range().end as usize].to_vec()
        };
        let part = parts.part(1).unwrap();

        // A byte flipped: by its CRC; its CRC made to match: by its tag.
        let mut flipped = stored(&part, &bytes);
        flipped[1000] ^= 1;
        assert_eq!(part.unpack(&flipped), Err(by_crc.clone()));
        let matching =
            Part { stored: Stored { crc: crc(&flipped), ..part.stored }, ..part.clone() };
        assert_eq!(matching.unpack(&flipped), Err(by_tag.clone()));
        assert_eq!(matching.open(&mut flipped.clone()), Err(by_tag.clone()));
        let mut tampered = bytes.clone();
        tampered[part.stored.offset as usize + 1000] ^= 1;
        assert_eq!(super::data(&tampered, &keys), Err(by_crc));
        tampered[entry(&parts.frame, 1) + 12..][..4].copy_from_slice(&crc(&flipped).to_le_bytes());
        recrc(&mut tampered);
        assert_eq!(super::data(&tampered, &keys), Err(by_mac.clone().into()));

        // Its tag flipped.
        let mut tag = part.tag;
        tag[15] ^= 1;
        assert_eq!(
            Part { tag, ..part.clone() }.unpack(&stored(&part, &bytes)),
            Err(by_tag.clone())
        );
        // Part 2 at index 1, its tag with it; or at part 1's place, its index its own.
        let other = parts.part(2).unwrap();
        let moved = Part { index: 1, place: part.place.clone(), ..other.clone() };
        assert_eq!(moved.unpack(&stored(&other, &bytes)), Err(by_tag.clone()));
        let elsewhere =
            Part { stored: Stored { offset: part.stored.offset, ..other.stored }, ..other.clone() };
        let decrypted = elsewhere.unpack(&stored(&other, &bytes)).unwrap();
        assert!(decrypted[..] != data[2 * PART as usize..], "another tweak, other bytes");
        // Part 1 of another raw file of the same data and DEK, of another salt, its tag with it.
        let another = sectors_of(&data, b"footer", &raw_file_key(Salt([6; SALT_LEN])));
        let theirs = Parts::of(&another, &keys).unwrap().part(1).unwrap();
        let transplanted = Part { opening: part.opening.clone(), ..theirs.clone() };
        assert_eq!(transplanted.unpack(&stored(&theirs, &another)), Err(by_tag));

        // A part said compressed, or of a length not whole sectors.
        let compressed = {
            let mut changed = bytes.clone();
            changed[entry(&parts.frame, 0) + 16] = 1;
            changed[entry(&parts.frame, 0) + 8..][..4]
                .copy_from_slice(&(PART as u32 - 1).to_le_bytes());
            recrc(&mut changed);
            changed
        };
        let not_sectors =
            Err(Corrupt("an object's part 0, not whole sectors of a raw file".into()));
        assert_eq!(corrupt_of(Parts::of(&compressed, &NoKeys)).map(|_| ()), not_sectors);
        let short = sectors_of(&data[..PART as usize], b"", &raw_file_key(Salt([7; SALT_LEN])));
        let mut cut = short.clone();
        let frame = Frame::of(&short).unwrap();
        // Data of a part less 512 bytes: its entry's length and the trailer's data length.
        let trailer = cut.len() - TRAILER as usize;
        cut[entry(&frame, 0) + 8..][..4].copy_from_slice(&(PART as u32 - 512).to_le_bytes());
        cut[trailer + at::DATA_LEN..][..8].copy_from_slice(&(PART - 512).to_le_bytes());
        recrc(&mut cut);
        assert_eq!(corrupt_of(Parts::of(&cut, &NoKeys)).map(|_| ()), not_sectors);
        // As they were, each reads.
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(super::data(&another, &keys).unwrap(), data);
    }

    /// TASK-421: a part compressed alone, as a cell holds one packed ahead in memory, encrypted
    /// under an object's key, is the part packed under that key, byte for byte, compressed or raw.
    #[test]
    fn parts_compressed_alone_encrypt_as_parts_packed_encrypted() {
        let salt = Salt([8; SALT_LEN]);
        let encrypting = encrypted_with(salt);
        let key = encrypting.key();
        let mut packer = PartPacker::new();
        for (index, part) in [pattern(PART), random(PART), random(5000)].iter().enumerate() {
            let index = index as u64;
            let compressed = packer.compress(index, part);
            let stored = compressed.stored(part).to_vec();
            let encrypted = compressed.encrypted(part.len() as u64, &stored, key);
            assert_eq!(encrypted, packer.pack(index, part, key), "part {index}");
        }
    }

    /// TASK-411.23: an object framed in place under a key other than a raw file's is no such
    /// object.
    #[test]
    #[should_panic(expected = "framed in place under a raw file's sectors key")]
    fn objects_framed_in_place_under_aes_gcm_keys_are_refused() {
        let key = Arc::new(ObjectKey::new(&dek(), ID, Salt([3; SALT_LEN]), Keyed::Encrypted));
        Packer::in_place(key);
    }

    /// TASK-411.2: an encrypted object's footer, table or trailer changed, their CRCs made to
    /// match, as one holding no key would: its readers holding its key refuse it, by its MAC; one
    /// holding none reads it, as the collector would, as doc-16 accepts.
    #[test]
    fn encrypted_metadata_changed_fails_by_its_mac() {
        let bytes = framed(&pattern(PART + 1), b"footer", (&dek(), ID));
        let frame = Frame::of(&bytes).unwrap();
        let trailer = bytes.len() - TRAILER as usize;
        let changed = |at: usize| {
            let mut changed = bytes.clone();
            changed[at] ^= 0x20;
            recrc(&mut changed);
            changed
        };
        // Its footer's first byte; part 0's CRC; its salt's last byte.
        let footer_changed = changed(frame.footer().start as usize);
        let table_changed = changed(entry(&frame, 0) + 12);
        let salt_changed = changed(trailer + at::FOOTER_MAC - 1);
        let read = |bytes: &[u8]| corrupt_of(Parts::of(bytes, &holding(dek())).map(|_| ()));
        let unread = |bytes: &[u8]| corrupt_of(Parts::of(bytes, &NoKeys).map(|_| ()));
        let by_mac = Err(Corrupt("an object's table, by its MAC".into()));
        let read = [read(&footer_changed), read(&table_changed), read(&salt_changed)];
        assert_eq!(read, [by_mac.clone(), by_mac.clone(), by_mac]);
        let unread = [unread(&footer_changed), unread(&table_changed), unread(&salt_changed)];
        assert_eq!(unread, [Ok(()), Ok(()), Ok(())]);
        assert_eq!(footer(&footer_changed, &NoKeys), Ok(&b"Footer"[..]));
        // The footer alone, its CRC made to match, its trailer's MAC of the footer as it was.
        let frame = Frame::of(&bytes).unwrap();
        let tampered = Frame { footer_crc: crc(b"Footer"), ..frame };
        let by_mac = Err(Corrupt("an object's footer, by its MAC".into()));
        assert_eq!(corrupt_of(tampered.check_footer(b"Footer", &holding(dek()))), by_mac);
        assert_eq!(tampered.check_footer(b"Footer", &NoKeys), Ok(&b"Footer"[..]));
    }

    /// A read of a range of the data reads and decompresses only the parts the range touches.
    #[test]
    fn ranges_read_only_their_parts() {
        let data = pattern(4 * PART);
        let mut bytes = framed(&data, b"", (&dek(), ID));
        let parts = Parts::of(&bytes, &holding(dek())).unwrap();
        // Parts the reads below don't touch are corrupt.
        for part in [0, 3] {
            bytes[parts.stored[part].offset as usize] ^= 1;
        }
        let reads = RefCell::new(Vec::new());
        let read = |range: Range<u64>| {
            reads.borrow_mut().push(range.clone());
            Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
        };
        let range = PART + 5..2 * PART + 9;
        let got = parts.read(range.clone(), read).unwrap();
        assert_eq!(got, &data[range.start as usize..range.end as usize]);
        assert_eq!(*reads.borrow(), [parts.stored[1].range(), parts.stored[2].range()]);
        reads.borrow_mut().clear();
        assert_eq!(parts.read(2 * PART + 1..2 * PART + 1, read).unwrap(), Vec::<u8>::new());
        assert!(reads.borrow().is_empty());
        assert_eq!(
            parts.read(3 * PART..3 * PART + 1, read),
            Err(Corrupt("an object's part, by its CRC".into()).into())
        );
        assert_eq!(
            parts.read(4 * PART..4 * PART + 1, read),
            Err(Corrupt("an object's data, read past its end".into()).into())
        );
    }

    /// cladefs: an object restamped holds its parts where they were, read as they were, and the
    /// footer it was given; what isn't a whole object is refused (TASK-165.9). Its salt and its
    /// parts' tags stay, and its new footer and table are authenticated under its subkeys
    /// (TASK-411.2).
    #[test]
    fn restamped_objects_keep_their_parts() {
        let data = [pattern(2 * PART), random(PART / 3)].concat();
        let keys = holding(dek());
        let salt = Salt([4; SALT_LEN]);
        let empty = |footer: &[u8]| {
            let packer = encrypted_with(salt);
            packer.finish(footer)
        };
        assert_eq!(restamp(&empty(b"one"), b"two", &keys).unwrap(), empty(b"two"));
        let object = framed(&data, b"footer of the first writer", (&dek(), ID));
        let cut = Err(Corrupt("an object's trailer, by its magic".into()).into());
        assert_eq!(restamp(&object[..object.len() - 1], b"cut short", &keys), cut);

        let mut packer = encrypted_with(salt);
        let mut object = packer.push(&data);
        object.extend(packer.finish(b"footer of the first writer"));
        let restamped = restamp(&object, b"a later writer's", &keys).unwrap();
        let mut packer = encrypted_with(salt);
        let mut expected = packer.push(&data);
        expected.extend(packer.finish(b"a later writer's"));
        assert_eq!(restamped, expected);
        assert_eq!(footer(&restamped, &keys).unwrap(), b"a later writer's");
        let (before, after) =
            (Parts::of(&object, &keys).unwrap(), Parts::of(&restamped, &keys).unwrap());
        assert_eq!((after.stored, after.tags), (before.stored, before.tags));
        assert_eq!(after.frame.encryption, Encryption::Encrypted { key: ID, salt });
        assert_eq!(super::data(&restamped, &keys).unwrap(), data);
        assert_eq!(
            &restamped[..before.frame.stored_len as usize],
            &object[..before.frame.stored_len as usize]
        );
    }

    /// cladefs: a part mostly incompressible, a sixteenth of it compressing, is stored compressed,
    /// saving less than an eighth: what a cell frames in place nonetheless (TASK-310).
    #[test]
    fn parts_compressing_little_are_stored_compressed() {
        let mut data = random(128 << 10);
        data[..8 << 10].copy_from_slice(&pattern(8 << 10));
        let mut packer = encrypted_with(Salt([3; SALT_LEN]));
        let packed = packer.push(&data);
        assert!(!packer.stored_raw(), "stored raw");
        assert!(packed.len() > data.len() - data.len() / 8, "{} of {}", packed.len(), data.len());
    }

    /// Parts that zstd does not shrink are stored raw: incompressible data costs the table, and
    /// nothing more; cladefs: encrypted, the same (TASK-411.2).
    #[test]
    fn incompressible_data_costs_its_table() {
        let data = random(2 * PART + 4096);
        let bytes = framed(&data, b"footer", (&dek(), ID));
        let parts = Parts::of(&bytes, &holding(dek())).unwrap();
        assert!(parts.stored.iter().all(|stored| stored.encoding == Encoding::Raw));
        assert_eq!(parts.frame.stored_len, data.len() as u64);
        assert_eq!(bytes.len() as u64, data.len() as u64 + 6 + 3 * ENTRY + TRAILER);
        assert_eq!(super::data(&bytes, &holding(dek())).unwrap(), data);
        assert_eq!((ENTRY, TRAILER), (33, 153));
    }

    /// A frame says its version, last but its magic: one of a version the reader doesn't read is
    /// refused, naming it, rather than called corrupt (TASK-141.2, doc-28); cladefs: frames of 1
    /// and 2, which said theirs as the magic's last digit, among them, of any length, the shortest
    /// as long as 1's trailer was, shorter than a trailer is now (TASK-411.2).
    #[test]
    fn frames_of_other_versions_are_named() {
        let bytes = framed(b"data", b"footer", (&dek(), ID));
        assert_eq!(&bytes[bytes.len() - 12..], b"\x03\0\0\0cladefs3");
        let refused = |version: u32| {
            let said = format!(
                "an object's trailer: extent frame version {version}: this build reads version 3 \
                 alone"
            );
            Corrupt(said)
        };
        for version in [0, 2, 4, 10, u32::MAX] {
            let mut other = bytes.clone();
            let at = other.len() - TRAILER as usize + at::VERSION;
            other[at..at + 4].copy_from_slice(&version.to_le_bytes());
            assert_eq!(corrupt_of(Parts::of(&other, &NoKeys)), Err(refused(version)));
            assert_eq!(check(&other), Err(refused(version)));
        }
        let mut other = bytes.clone();
        *other.last_mut().unwrap() = b'x';
        assert_eq!(
            corrupt_of(Parts::of(&other, &NoKeys)),
            Err(Corrupt("an object's trailer, by its magic".into()))
        );

        // Format 1's: a part raw, 17 bytes of table each, 40 of trailer.
        let format_1 = |data: &[u8], footer: &[u8]| {
            let mut bytes = data.to_vec();
            bytes.extend_from_slice(footer);
            let table = bytes.len();
            if !data.is_empty() {
                bytes.extend_from_slice(&0u64.to_le_bytes());
                bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&crc(data).to_le_bytes());
                bytes.push(0);
            }
            for len in [data.len(), data.len(), footer.len()] {
                bytes.extend_from_slice(&(len as u64).to_le_bytes());
            }
            bytes.extend_from_slice(&crc(footer).to_le_bytes());
            bytes.extend_from_slice(&crc(&bytes[table..]).to_le_bytes());
            bytes.extend_from_slice(b"cladefs1");
            bytes
        };
        // Format 2's: as 3's, but the version, its magic's last digit.
        let mut format_2 = bytes.clone();
        format_2.drain(bytes.len() - 12..bytes.len() - 8);
        *format_2.last_mut().unwrap() = b'2';
        for (old, version) in
            [(format_1(&random(4096), b"footer"), 1), (format_1(&[], b""), 1), (format_2, 2)]
        {
            let refused = refused(version);
            assert_eq!(
                corrupt_of(Parts::of(&old, &NoKeys)),
                Err(refused.clone()),
                "{} bytes",
                old.len()
            );
            assert_eq!(Frame::of_trailer(&old), Err(refused.clone()));
            assert_eq!(check(&old), Err(refused.clone()));
            assert_eq!(super::data(&old, &holding(dek())), Err(refused.clone().into()));
            assert_eq!(corrupt_of(footer(&old, &NoKeys)), Err(refused.clone()));
        }
        assert_eq!(format_1(&[], b"").len(), 40);
    }

    /// The table's MAC covers the frame's version, which follows it: a frame of one version
    /// relabelled another is unauthentic to a reader holding its key, were both read, and so is
    /// never read as the other (doc-28). The version is checked by its MAC alone here: a reader
    /// reading 3 alone refuses any other before it.
    #[test]
    fn a_frames_version_is_authenticated() {
        let bytes = framed(&random(4096), b"footer", (&dek(), ID));
        let parts = Parts::of(&bytes, &holding(dek())).unwrap();
        let trailer = bytes.len() - TRAILER as usize;
        let table = parts.frame.table().start as usize;
        let key = ObjectKey::new(&dek(), ID, parts.frame.encryption.keyed().1, Keyed::Encrypted);
        let mac: [u8; MAC_LEN] = bytes[trailer + at::MAC..][..MAC_LEN].try_into().unwrap();
        let signed = |version: u32| {
            let version = version.to_le_bytes();
            key.mac.verify_of(&[&bytes[table..trailer + at::MAC], &version], &mac)
        };
        assert_eq!(signed(3), Ok(()));
        assert!(signed(4).is_err());
        assert!(key.mac.verify(&bytes[table..trailer + at::MAC], &mac).is_err());
    }

    /// TASK-411.2: a trailer's encryption is encrypted, authenticated or a raw file's sectors, as
    /// its kind says; of another kind, or in the clear (TASK-411.28), it is corrupt, to a reader
    /// holding no key too.
    #[test]
    fn encrypts_are_as_their_kinds_say() {
        let bytes = framed(&random(4096), b"footer", (&dek(), ID));
        let trailer = bytes.len() - TRAILER as usize;
        let refused = |at: usize, value: u8| {
            let mut changed = bytes.clone();
            changed[at] = value;
            recrc(&mut changed);
            corrupt_of(Parts::of(&changed, &NoKeys).map(|_| ()))
        };
        let corrupt = |what: &str| Err(Corrupt(format!("an object's {what}")));
        assert_eq!(
            refused(trailer + at::ENCRYPTION, 4),
            corrupt("trailer: an encryption of kind 4")
        );
        assert_eq!(
            refused(trailer + at::ENCRYPTION, 0),
            corrupt("trailer: an encryption in the clear, which no object has")
        );
        for kind in 1..=3 {
            assert_eq!(refused(trailer + at::ENCRYPTION, kind), Ok(()), "kind {kind}");
        }
    }

    /// Objects cut short, or with any byte of them changed, fail to read, without panicking:
    /// cladefs: encrypted and authenticated (TASK-411.2, TASK-411.3).
    #[test]
    fn corrupt_objects_fail() {
        let data = [pattern(PART + 100), random(PART / 2)].concat();
        let keys = holding(dek());
        let encrypted = framed(&data, b"footer", (&dek(), ID));
        let authenticated = framed_keyed(&data, b"footer", (&dek(), ID, Keyed::Authenticated));
        for bytes in [encrypted, authenticated] {
            let read = |bytes: &[u8]| -> Result<(Vec<u8>, Vec<u8>), Error> {
                let parts = Parts::of(bytes, &keys)?;
                let data = parts.read(0..parts.frame.data_len, |range| {
                    bytes
                        .get(range.start as usize..range.end as usize)
                        .map(<[u8]>::to_vec)
                        .ok_or_else(|| Error::from(Corrupt("past the end".into())))
                })?;
                Ok((data, footer(bytes, &keys)?.to_vec()))
            };
            assert_eq!(read(&bytes).unwrap(), (data.clone(), b"footer".to_vec()));
            assert_eq!(check(&bytes), Ok(()));
            // What reading finds, checking finds too, unpacking nothing.
            for len in 0..bytes.len() {
                assert!(read(&bytes[..len]).is_err(), "cut to {len}");
                assert!(check(&bytes[..len]).is_err(), "cut to {len}");
            }
            for start in 1..bytes.len() {
                assert!(read(&bytes[start..]).is_err(), "from {start}");
                assert!(check(&bytes[start..]).is_err(), "from {start}");
            }
            // Each byte of the tail, and of every few of the parts' stored bytes, changed, is
            // found.
            let parts = Parts::of(&bytes, &keys).unwrap();
            let stored = parts.stored.iter().flat_map(|stored| stored.range().step_by(509));
            for at in stored.chain(parts.frame.tail()) {
                let mut flipped = bytes.clone();
                flipped[at as usize] ^= 0x10;
                assert!(read(&flipped).is_err(), "byte {at} flipped");
                assert!(check(&flipped).is_err(), "byte {at} flipped");
            }
            // A part's stored bytes, cut short, or of another part.
            let part = parts.part(0).unwrap();
            let first =
                &bytes[part.stored.range().start as usize..part.stored.range().end as usize];
            assert!(part.unpack(&first[..first.len() - 1]).is_err());
            let second = Part { place: part.place.clone(), ..parts.part(1).unwrap() };
            assert!(second.unpack(first).is_err());
            assert!(parts.part(2).is_err());
        }
        // Data that is no zstd frame, though its CRC matches.
        let garbage = vec![7; 100];
        let lying = Part {
            index: 0,
            place: 0..PART,
            stored: Stored { offset: 0, len: 100, crc: crc(&garbage), encoding: Encoding::Zstd },
            tag: [0; TAG_LEN],
            opening: Opening::Unchecked(ID),
        };
        assert!(lying.unpack(&garbage).is_err());
        // A raw part unpacked as longer or shorter than it is.
        let raw = Part {
            place: 0..100,
            stored: Stored { encoding: Encoding::Raw, ..lying.stored },
            ..lying.clone()
        };
        assert_eq!(raw.unpack(&garbage).unwrap(), garbage);
        for len in [99, 101] {
            let wrong = Err(Corrupt("an object's raw part, of the wrong length".into()).into());
            assert_eq!(Part { place: 0..len, ..raw.clone() }.unpack(&garbage), wrong, "{len}");
        }
    }

    /// TASK-153.11: what checking an object's parts costs, as a host checks a part it reads and a
    /// segment it uploads ([`check`]): the time a MiB of stored bytes takes to check, of data
    /// stored raw and compressed, the median of 9 rounds over 64 MiB. `cargo nextest run
    /// -p extent_frame --release --run-ignored only bench_checking_parts --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-153.11): --run-ignored only, in release"]
    fn bench_checking_parts() {
        let len = 64 * PART;
        for (kind, data) in [("raw", random(len)), ("compressed", pattern(len))] {
            let bytes = framed(&data, b"footer", (&dek(), ID));
            let stored = Parts::of(&bytes, &NoKeys).unwrap().frame.stored_len;
            let mut rounds: Vec<f64> = (0..9)
                .map(|_| {
                    let started = std::time::Instant::now();
                    check(std::hint::black_box(&bytes)).unwrap();
                    started.elapsed().as_secs_f64() * 1e6 / (stored as f64 / PART as f64)
                })
                .collect();
            rounds.sort_by(f64::total_cmp);
            println!("{kind:<10} {:>6.1} µs a MiB stored, {stored} bytes stored", rounds[4]);
        }
    }

    /// A table whose CRC matches, as its writer's would, but that puts a part where none can be,
    /// fails: each way of it alone.
    #[test]
    fn tables_that_lie_fail() {
        // Two parts, of PART bytes and of 8192, written at 0 and at 4096, 4096 + 100 bytes stored.
        let zstd = |offset, len| Stored { offset, len, crc: 0, encoding: Encoding::Zstd };
        let raw = |offset, len| Stored { offset, len, crc: 0, encoding: Encoding::Raw };
        let stored_in = |stored: Vec<Stored>| {
            let tags = vec![[0; TAG_LEN]; stored.len()];
            let packer = Packer {
                stored,
                tags,
                data_len: PART + 8192,
                end: 4096 + 8192,
                ..encrypted_with(Salt([3; SALT_LEN]))
            };
            let tail = packer.finish(b"f");
            Frame::of_trailer(&tail).unwrap().stored_in(&tail).map(|(stored, ..)| stored)
        };
        let place = Err(Corrupt("an object's part 1's place".into()));
        assert_eq!(stored_in(vec![zstd(0, 40), raw(4096, 8192)]).unwrap().len(), 2);
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(4096, 100)]).unwrap().len(), 2);
        // A raw part not as long as it is.
        assert_eq!(stored_in(vec![zstd(0, 40), raw(4096, 8191)]), place);
        // A compressed part of nothing, or no smaller than it is.
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(4096, 0)]), place);
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(4096, 8192)]), place);
        // Not aligned; over the part before it; past the parts stored.
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(4097, 100)]), place);
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(0, 100)]), place);
        assert_eq!(stored_in(vec![zstd(0, 40), zstd(8192, 8192 - 1)]), place);
    }

    /// board 11647: untrusted trailers at `u64::MAX` / `u64::MAX - TRAILER` boundaries (and part
    /// entries whose `offset + len` would overflow `u64`) must return `Err(Corrupt)` cleanly rather
    /// than panicking on arithmetic overflow in `Frame::stored_in` or `Frame::of_trailer`.
    #[test]
    fn boundary_trailers_at_u64_max_do_not_overflow_or_panic() {
        let make_suffix = |data_len: u64,
                           stored_len: u64,
                           footer_len: u64,
                           prefix_len: usize,
                           fix_crc: bool| {
            let mut suffix = vec![0u8; prefix_len + TRAILER as usize];
            let trailer = &mut suffix[prefix_len..];
            trailer[at::DATA_LEN..at::DATA_LEN + 8].copy_from_slice(&data_len.to_le_bytes());
            trailer[at::STORED_LEN..at::STORED_LEN + 8].copy_from_slice(&stored_len.to_le_bytes());
            trailer[at::FOOTER_LEN..at::FOOTER_LEN + 8].copy_from_slice(&footer_len.to_le_bytes());
            trailer[at::ENCRYPTION] = 1;
            trailer[at::VERSION..at::VERSION + 4].copy_from_slice(&FORMAT.current.to_le_bytes());
            trailer[at::MAGIC..].copy_from_slice(&MAGIC);
            if fix_crc {
                let table_crc = crc(&suffix[prefix_len..prefix_len + at::TABLE_CRC]);
                suffix[prefix_len + at::TABLE_CRC..prefix_len + at::TABLE_CRC + 4]
                    .copy_from_slice(&table_crc.to_le_bytes());
            }
            suffix
        };

        // Exact reproducer from board 11647: `size() == u64::MAX`, `table().start == u64::MAX - TRAILER`,
        // `suffix.len() == TRAILER + 10` previously overflowed `suffix.len() as u64 + self.table().start`.
        let bad_crc = make_suffix(0, u64::MAX - TRAILER - 10, 10, 10, false);
        let frame = Frame::of_trailer(&bad_crc).expect("boundary trailer fits in u64");
        assert_eq!(frame.size(), u64::MAX);
        assert!(frame.stored_in(&bad_crc).is_err());

        let valid_crc = make_suffix(0, u64::MAX - TRAILER - 10, 10, 10, true);
        let frame = Frame::of_trailer(&valid_crc).expect("boundary trailer fits in u64");
        let (stored, tags, at) = frame.stored_in(&valid_crc).expect("empty table at u64::MAX");
        assert!(stored.is_empty() && tags.is_empty() && at == 10);

        // Overflowing length combinations must be refused by `of_trailer`.
        for (data_len, stored_len, footer_len) in [
            (0, u64::MAX, 0),
            (0, u64::MAX - TRAILER, 1),
            (u64::MAX, 0, 0),
            (u64::MAX - PART + 1, 0, 0),
            (PART, u64::MAX - TRAILER - ENTRY + 1, 0),
        ] {
            let suffix = make_suffix(data_len, stored_len, footer_len, 0, false);
            assert!(
                Frame::of_trailer(&suffix).is_err(),
                "expected refusal for data_len={data_len}, stored_len={stored_len}, footer_len={footer_len}"
            );
        }

        // Part entry with `offset = u64::MAX - ALIGN + 1` (or aligned near `u64::MAX`) must not
        // overflow `offset + len` in `stored_in`.
        let mut suffix = vec![0u8; ENTRY as usize + TRAILER as usize];
        let huge_offset = u64::MAX - (u64::MAX % ALIGN);
        suffix[0..8].copy_from_slice(&huge_offset.to_le_bytes());
        suffix[8..12].copy_from_slice(&(4096u32).to_le_bytes());
        suffix[16] = 0; // Raw
        let trailer_at = ENTRY as usize;
        suffix[trailer_at + at::DATA_LEN..trailer_at + at::DATA_LEN + 8]
            .copy_from_slice(&4096u64.to_le_bytes());
        suffix[trailer_at + at::STORED_LEN..trailer_at + at::STORED_LEN + 8]
            .copy_from_slice(&(u64::MAX - TRAILER - ENTRY).to_le_bytes());
        suffix[trailer_at + at::ENCRYPTION] = 1;
        suffix[trailer_at + at::VERSION..trailer_at + at::VERSION + 4]
            .copy_from_slice(&FORMAT.current.to_le_bytes());
        suffix[trailer_at + at::MAGIC..].copy_from_slice(&MAGIC);
        let table_crc = crc(&suffix[..trailer_at + at::TABLE_CRC]);
        suffix[trailer_at + at::TABLE_CRC..trailer_at + at::TABLE_CRC + 4]
            .copy_from_slice(&table_crc.to_le_bytes());
        let frame = Frame::of_trailer(&suffix).unwrap();
        assert!(frame.stored_in(&suffix).is_err());
    }

    /// cladefs: the pure-Rust Castagnoli CRC32C used in keyless `wasm32` builds matches
    /// `crc_fast` across edge lengths and arbitrary incremental splits.
    #[test]
    fn pure_crc32c_matches_crc_fast() {
        for len in [0usize, 1, 2, 7, 15, 63, 64, 65, 1024, 4096, 65536] {
            let data = random(len as u64);
            let fast = crc(&data);
            let pure = !crc32c_pure_update(!0u32, &data);
            assert_eq!(pure, fast, "whole length {len}");

            let mut state = !0u32;
            let mut offset = 0;
            for step in [1usize, 3, 7, 13, 29, 64, 511, 4097] {
                if offset >= data.len() {
                    break;
                }
                let end = (offset + step).min(data.len());
                state = crc32c_pure_update(state, &data[offset..end]);
                offset = end;
            }
            if offset < data.len() {
                state = crc32c_pure_update(state, &data[offset..]);
            }
            assert_eq!(!state, fast, "incremental length {len}");
        }
    }

    /// cladefs: the pure-Rust zstd decoder (`ruzstd`) matches C `zstd` decompression and rejects
    /// wrong output lengths.
    #[cfg(feature = "pure-zstd")]
    #[test]
    fn pure_zstd_matches_c_zstd() {
        let data = pattern(PART);
        let compressed = zstd::bulk::compress(&data, 3).unwrap();
        let mut out = vec![0u8; data.len()];
        decode_zstd_pure(&compressed, &mut out).unwrap();
        assert_eq!(out, data);

        let mut short = vec![0u8; data.len() - 1];
        assert!(decode_zstd_pure(&compressed, &mut short).is_err());
        let mut long = vec![0u8; data.len() + 1];
        assert!(decode_zstd_pure(&compressed, &mut long).is_err());
    }

    #[test]
    fn every_frozen_version_is_read() {
        formats::golden::check::<Frames>(env!("CARGO_MANIFEST_DIR"));
    }

    /// Frames of each encryption and encoding, under [`dek`], each of a salt of its own.
    struct Frames;

    /// What a frame decodes into: its trailer, its table, its footer, and its data, by its length
    /// and CRC.
    #[derive(Debug)]
    #[allow(dead_code, reason = "read by its Debug alone")]
    struct Decoded {
        frame: Frame,
        stored: Vec<Stored>,
        footer: String,
        data: String,
    }

    impl formats::golden::Family for Frames {
        const FORMAT: formats::Format = FORMAT;

        fn write() -> Vec<(&'static str, Vec<u8>)> {
            let keyed = |salt: u8, keyed: Keyed, data: &[u8]| {
                let salt = Salt([salt; SALT_LEN]);
                let mut packer = Packer::with(Arc::new(ObjectKey::new(&dek(), ID, salt, keyed)));
                let mut bytes = packer.push(data);
                bytes.extend(packer.finish(b"footer"));
                bytes
            };
            // Zeros compress, as zstd's RLE blocks: what a zstd of another release writes alike.
            let zeros = vec![0; 8192];
            let sectors = sectors_of(
                &random(2 * SECTOR as u64),
                b"footer",
                &raw_file_key(Salt([5; SALT_LEN])),
            );
            vec![
                ("encrypted-zstd", keyed(1, Keyed::Encrypted, &zeros)),
                ("encrypted-raw", keyed(2, Keyed::Encrypted, &random(512))),
                ("encrypted-empty", keyed(3, Keyed::Encrypted, &[])),
                ("authenticated", keyed(4, Keyed::Authenticated, &zeros)),
                ("sectors", sectors),
            ]
        }

        fn written(version: u32, case: &str, bytes: &[u8]) -> Result<String, String> {
            // Version 3's decoder is this build's.
            if version != 3 {
                return Err(format!("no decoder of version {version}"));
            }
            Self::decoded(case, bytes).map(|decoded| format!("{decoded:#?}"))
        }

        fn read(case: &str, bytes: &[u8]) -> Result<(), String> {
            Self::decoded(case, bytes).map(drop)
        }
    }

    impl Frames {
        /// `bytes` read by this build's reader holding [`dek`], and checked by one holding none.
        fn decoded(_: &str, bytes: &[u8]) -> Result<Decoded, String> {
            let keys = holding(dek());
            let parts = Parts::of(bytes, &keys).map_err(|err| err.to_string())?;
            let footer = super::footer(bytes, &keys).map_err(|err| err.to_string())?;
            let data = super::data(bytes, &keys).map_err(|err| err.to_string())?;
            check(bytes).map_err(|err| err.to_string())?;
            Ok(Decoded {
                frame: parts.frame,
                stored: parts.stored,
                footer: footer.escape_ascii().to_string(),
                data: format!("{} bytes, CRC32C {:#010x}", data.len(), crc(&data)),
            })
        }
    }

    /// board 12313: the per-thread zstd decompressor recovers cleanly after a corrupt frame and
    /// decodes subsequent valid parts on the same thread.
    #[test]
    fn thread_local_decompressor_recovers_after_corrupt_frame() {
        let data = vec![0xab; 16 * 1024];
        let mut out = vec![0u8; data.len()];
        assert!(decode(Encoding::Zstd, b"not a valid zstd frame", &mut out).is_err());
        let compressed = zstd::bulk::compress(&data, 1).unwrap();
        decode(Encoding::Zstd, &compressed, &mut out).unwrap();
        assert_eq!(out, data);
    }
}
