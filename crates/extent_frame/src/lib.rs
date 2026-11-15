// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: how an object holding extents' data is framed (backlog doc-7, Segments). Its data
//! is in parts of [`PART`] bytes, each stored on its own, compressed with zstd, or raw if that is no
//! smaller, at an offset that is a multiple of [`ALIGN`]; then its footer; then a table of where
//! each part is stored, its stored length, the CRC32C of its stored bytes, its encoding and its
//! tag; then a [`TRAILER`] of the lengths of the data, the parts stored and the footer, the CRC of
//! the footer, the object's [`Seal`], the MACs of its footer and of its table, and the CRC of its
//! table. A store's closed segments and sealed extents are framed so, as are the objects a branch
//! keeps in object storage: a reader reads and decompresses only the parts it needs, each checked
//! against its CRC before it is decompressed.
//!
//! cladefs: FORMAT 2 (TASK-411.2, doc-16, What is encrypted). An object is sealed or in the clear,
//! as its trailer says. Sealed, each of its parts is compressed first, as in the clear, then sealed
//! with AES-256-GCM under a subkey of the object's own: its branch's DEK's, of the generation its
//! trailer names, derived with a salt drawn at random for the object and kept in its trailer
//! ([`Secret::derive`]); the nonce is the part's index, and the associated data its index,
//! encoding and lengths. The table's entry holds the part's tag; the CRC stays, over the bytes
//! stored, sealed, so that a reader holding no key, the collector, checks them as before, and a
//! part corrupt is told from one under another key. The footer, the table and the trailer stay in
//! the clear, authenticated by HMAC-SHA256 under a second subkey from the same salt, which a
//! reader holding the key checks ([`Keys`]), and one holding none does not.
//!
//! Parts are bound to their object by its salt, not by its key in object storage, as doc-16 first
//! had it: a segment's framed file is uploaded as it is, or with its footer stamped anew
//! ([`restamp`]), at the key of the number and epoch its writer's chain gives it then, which a
//! takeover may change after it was framed (doc-14; `wal::rebase`), and the extents stored on their
//! own its footer names go to keys naming that number and epoch too. A part moved to another
//! object is opened under that object's salt, another subkey, and fails; one moved to another
//! index fails by its nonce and associated data. A whole object copied to another key is authentic
//! there: what it is, a reader tells by its footer, as it does now (a segment's names its branch,
//! number and epoch, `segment::Footer`).
//!
//! A branch's checkpoints, locally and in object storage, are such objects: tables of where its
//! extents are ([`table`]). cladefs: authenticated, not sealed, under their branch's DEK (TASK-411.3,
//! [`Seal::Authenticated`]): metadata, which the collector reads holding no key (doc-16, What is
//! encrypted), each part's tag a GMAC of its stored bytes, which a reader holding the key checks.
//!
//! cladefs: a reader of a branch whose objects are sealed refuses one in the clear ([`Keys::sealed`],
//! [`Error::Clear`], TASK-411.3): its trailer's seal is not authenticated, and one who can write the
//! bucket could otherwise zero an object's seal, MACs and tags, and have it read as its branch's.

pub mod segment;
pub mod spent;
pub mod table;

pub use extent_keys::{KeyId, Salt, Secret};

use extent_keys::{Aead, MAC_LEN, SALT_LEN, TAG_LEN};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Each this many bytes of an object's data, the last fewer, is a part, stored on its own, with
/// its own CRC32C: what a reader reads, checks and decompresses at once.
pub const PART: u64 = 1 << 20;

/// Parts are stored at multiples of this, so that a host reads them bypassing its page cache.
pub const ALIGN: u64 = 4096;

/// The bytes of the table for each part: its offset, stored length, CRC32C and encoding, and its
/// AES-256-GCM tag, zeros in an object in the clear. cladefs: one size for both kinds of object
/// (TASK-411.2), so that where a table is, and how a reader of a suffix finds it, never depends on
/// the seal its trailer names: a table read the same way whatever it frames, for 16 bytes a part
/// of a MiB, a 65,536th, in an object in the clear.
pub const ENTRY: u64 = 8 + 4 + 4 + 1 + TAG_LEN as u64;

/// A sealed part's AES-256-GCM tag, which its table's entry holds: zeros in the clear.
type Tag = [u8; TAG_LEN];

/// The bytes of a trailer's [`Seal`]: its kind, a byte, 0 in the clear, 1 sealed; the branch and
/// generation of the DEK its subkeys came from; and its salt. Zeros but for its kind in the clear.
const SEAL_LEN: usize = 1 + 8 + 4 + SALT_LEN;

/// Where each piece of a trailer is in it.
mod at {
    use super::MAC_LEN;

    pub const DATA_LEN: usize = 0;
    pub const STORED_LEN: usize = 8;
    pub const FOOTER_LEN: usize = 16;
    pub const FOOTER_CRC: usize = 24;
    pub const SEAL: usize = 28;
    pub const FOOTER_MAC: usize = SEAL + super::SEAL_LEN;
    /// The MAC of the table and the trailer before it.
    pub const MAC: usize = FOOTER_MAC + MAC_LEN;
    /// The CRC of the table and the trailer before it.
    pub const TABLE_CRC: usize = MAC + MAC_LEN;
    pub const MAGIC: usize = TABLE_CRC + 4;
}

/// The bytes after an object's table: the lengths of its data, its parts stored and its footer;
/// the CRC32C of its footer; its [`Seal`]; the MACs of its footer and of its table and trailer
/// before that MAC, zeros in the clear; the CRC32C of its table and trailer before it; and
/// [`MAGIC`].
pub const TRAILER: u64 = (at::MAGIC + MAGIC.len()) as u64;

/// The format of the frames this crate writes and reads, the last byte of [`MAGIC`], as a digit:
/// a frame says its format, and a reader of another names it rather than call it corrupt (doc-11,
/// rule 1; TASK-141.2). cladefs: 2 since objects may be sealed (TASK-411.2); a reader of 2 reads no
/// frame of 1, nothing having been kept that needs it (the user's decision of 2026-11-14).
pub const FORMAT: u8 = 2;

/// An object's last bytes: the frame's [`FORMAT`] after them.
const MAGIC_OF: [u8; 7] = *b"cladefs";

/// An object's last bytes.
const MAGIC: [u8; 8] = {
    let mut magic = [b'0' + FORMAT; 8];
    let mut at = 0;
    while at < MAGIC_OF.len() {
        magic[at] = MAGIC_OF[at];
        at += 1;
    }
    magic
};

/// The zstd level parts are compressed at, unless `CLADEFS_ZSTD_LEVEL` says another, to measure it
/// against (TASK-29.4): the fastest, as sealing and closing segments wait for it. At 0, parts are
/// stored raw, as before compression.
const LEVEL: i32 = 1;

/// The zstd level parts are compressed at: `CLADEFS_ZSTD_LEVEL`'s, or [`LEVEL`].
fn level() -> i32 {
    static CHOSEN: std::sync::LazyLock<i32> =
        std::sync::LazyLock::new(|| match std::env::var("CLADEFS_ZSTD_LEVEL") {
            Ok(level) => {
                level.parse().unwrap_or_else(|_| panic!("CLADEFS_ZSTD_LEVEL is {level:?}"))
            }
            Err(_) => LEVEL,
        });
    *CHOSEN
}

/// The CRC32C of `bytes`.
pub fn crc(bytes: &[u8]) -> u32 {
    crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, bytes) as u32
}

/// An object that is not as its frame says: corrupt, or, sealed, tampered with or opened under
/// another key than it was sealed under.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Corrupt(pub String);

fn corrupt(what: impl std::fmt::Display) -> Corrupt {
    Corrupt(format!("an object's {what}"))
}

/// cladefs: why an object's data can't be read (TASK-411.2): it is corrupt, or sealed under a key
/// its reader doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Corrupt(#[from] Corrupt),
    #[error(
        "an object sealed under branch {:x}'s key of generation {}, which its reader doesn't hold",
        .0.branch,
        .0.generation
    )]
    NoKey(KeyId),
    /// cladefs: an object in the clear, of a branch whose objects its reader holds sealed
    /// ([`Keys::sealed`], TASK-411.3): a downgrade, its seal, MACs and tags zeroed by one who can
    /// write where it is, or another's object put there.
    #[error("an object in the clear, of a branch whose objects are sealed")]
    Clear,
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// cladefs: the keys a reader of objects holds: the DEK a [`KeyId`] names, if it holds it
/// (TASK-411.2). A reader holding none of an object's reads its footer and table, checked against
/// their CRCs, and checks its parts' stored bytes, but unpacks none of them ([`Error::NoKey`]).
pub trait Keys {
    fn dek(&self, id: KeyId) -> Option<&Secret>;

    /// cladefs: whether its reader holds the objects it reads sealed, or authenticated, under their
    /// branch's DEK, as a reader of a branch whose record has a DEK does (TASK-411.3): one in the
    /// clear is refused ([`Error::Clear`]), and one authenticated under a key it doesn't hold
    /// unread ([`Error::NoKey`]). None by default: a reader of objects in the clear, or of
    /// metadata alone, as the collector is.
    fn sealed(&self) -> bool {
        false
    }
}

/// cladefs: no keys: a reader of objects in the clear, or of objects' metadata alone, as the
/// collector's marking is.
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

/// cladefs: whether an object's parts are sealed, as its trailer says, and if so, under which DEK's
/// subkeys, from which salt (TASK-411.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Seal {
    /// In the clear: an object of a cell with no key (tenant 0, tests).
    Clear,
    /// Its parts sealed with AES-256-GCM, its footer and table authenticated with HMAC-SHA256,
    /// under subkeys HKDF-SHA256 derives from the DEK `key` names and `salt`.
    Sealed { key: KeyId, salt: Salt },
    /// cladefs: its parts in the clear, as stored, each tagged with AES-256-GCM's GMAC of them, its
    /// footer and table authenticated as a sealed object's are (TASK-411.3): metadata, a branch's
    /// checkpoints, which a reader holding no key reads, checked against their CRCs, as the
    /// collector does, and one holding the key checks against their tags and MACs.
    Authenticated { key: KeyId, salt: Salt },
}

impl Seal {
    /// The key its subkeys came from, and its salt, unless it is in the clear.
    pub fn keyed(&self) -> Option<(KeyId, Salt)> {
        match *self {
            Self::Clear => None,
            Self::Sealed { key, salt } | Self::Authenticated { key, salt } => Some((key, salt)),
        }
    }
}

/// cladefs: how a writer frames an object under its branch's DEK, the key it names (TASK-411.3):
/// its parts sealed, as data is, or authenticated alone, as metadata the collector reads is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyed {
    Sealed,
    Authenticated,
}

/// What HKDF derives an object's subkey that seals its parts with, as its info.
const PARTS_INFO: &[u8] = b"cladefs frame parts";

/// What HKDF derives an object's subkey that authenticates its footer and table with, as its info.
const MAC_INFO: &[u8] = b"cladefs frame mac";

/// cladefs: an object's subkeys, derived from its DEK and its salt (TASK-411.2): one that seals and
/// opens its parts, and one that authenticates its footer and table. A packer draws a salt of its
/// own for each object it packs ([`Packer::new`]), so that no two objects' parts are sealed under
/// one subkey, as nonces, the parts' indices, repeat across objects. Shared by the threads that
/// pack an object's parts ([`PartPacker::pack`]) and by its readers.
pub struct ObjectKey {
    id: KeyId,
    salt: Salt,
    /// cladefs: whether its parts are sealed, or authenticated alone (TASK-411.3).
    keyed: Keyed,
    parts: Aead,
    mac: Secret,
}

impl ObjectKey {
    fn new(dek: &Secret, id: KeyId, salt: Salt, keyed: Keyed) -> Self {
        let parts = Aead::new(&dek.derive(&salt, &[PARTS_INFO]));
        Self { id, salt, keyed, parts, mac: dek.derive(&salt, &[MAC_INFO]) }
    }

    fn seal(&self) -> Seal {
        let (key, salt) = (self.id, self.salt);
        match self.keyed {
            Keyed::Sealed => Seal::Sealed { key, salt },
            Keyed::Authenticated => Seal::Authenticated { key, salt },
        }
    }

    /// cladefs: whether its parts are sealed, not authenticated alone.
    fn seals(&self) -> bool {
        self.keyed == Keyed::Sealed
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

    /// Seals `bytes` in place, part `index`, of `len` bytes, packed `encoding`: its tag.
    fn seal_part(&self, index: u64, encoding: Encoding, len: u64, bytes: &mut [u8]) -> Tag {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        self.parts.seal(index, &aad, bytes)
    }

    /// cladefs: the tag of `bytes`, part `index`, of `len` bytes, packed `encoding`, authenticated
    /// alone: AES-256-GCM's GMAC, of nothing sealed, its associated data the part's and its bytes
    /// (TASK-411.3).
    fn tag_part(&self, index: u64, encoding: Encoding, len: u64, bytes: &[u8]) -> Tag {
        let aad = [&Self::aad(index, encoding, bytes.len(), len)[..], bytes].concat();
        self.parts.seal(index, &aad, &mut [])
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
        let aad = [&Self::aad(index, encoding, bytes.len(), len)[..], bytes].concat();
        (self.parts.open(index, &aad, &mut [], tag)).map_err(|_| corrupt("part, by its tag"))
    }

    /// Opens in place what [`ObjectKey::seal_part`] sealed, given its tag.
    fn open_part(
        &self,
        index: u64,
        encoding: Encoding,
        len: u64,
        bytes: &mut [u8],
        tag: &Tag,
    ) -> Result<(), Corrupt> {
        let aad = Self::aad(index, encoding, bytes.len(), len);
        (self.parts.open(index, &aad, bytes, tag)).map_err(|_| corrupt("part, by its tag"))
    }
}

impl std::fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ObjectKey");
        debug.field("id", &self.id).field("salt", &self.salt).field("keyed", &self.keyed).finish()
    }
}

/// cladefs: how a reader opens an object's parts and checks its footer and table (TASK-411.2).
#[derive(Clone, Debug)]
enum Opening {
    /// The object is in the clear.
    Clear,
    /// It is sealed, or authenticated, under a key the reader holds: its subkeys.
    Open(Arc<ObjectKey>),
    /// It is sealed under a key the reader doesn't hold: its parts are checked, not opened.
    Locked(KeyId),
    /// cladefs: it is authenticated under a key the reader doesn't hold, which holds none sealed
    /// ([`Keys::sealed`]): its parts are read as they are stored, checked against their CRCs alone,
    /// as the collector reads a checkpoint (TASK-411.3).
    Unchecked(KeyId),
}

impl Opening {
    /// How a reader holding `keys` opens an object sealed as `seal` says: refused, if it is in
    /// the clear, or authenticated under a key the reader doesn't hold, and the reader holds its
    /// objects sealed (TASK-411.3).
    fn of(seal: Seal, keys: &dyn Keys) -> Result<Self, Error> {
        Ok(match seal {
            Seal::Clear if keys.sealed() => return Err(Error::Clear),
            Seal::Clear => Self::Clear,
            Seal::Sealed { key, salt } => match keys.dek(key) {
                Some(dek) => Self::Open(Arc::new(ObjectKey::new(dek, key, salt, Keyed::Sealed))),
                None => Self::Locked(key),
            },
            Seal::Authenticated { key, salt } => match keys.dek(key) {
                Some(dek) => {
                    Self::Open(Arc::new(ObjectKey::new(dek, key, salt, Keyed::Authenticated)))
                }
                None if keys.sealed() => return Err(Error::NoKey(key)),
                None => Self::Unchecked(key),
            },
        })
    }

    /// The object's subkeys, if the reader holds them.
    fn key(&self) -> Option<&ObjectKey> {
        match self {
            Self::Open(key) => Some(key),
            Self::Clear | Self::Locked(_) | Self::Unchecked(_) => None,
        }
    }
}

/// Openings are alike if they open alike: under the same subkeys, as those of one DEK and salt are.
impl PartialEq for Opening {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Clear, Self::Clear) => true,
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
    fn byte(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Zstd => 1,
        }
    }
}

/// Where and how a part is stored: its stored bytes, which a reader holding no key checks
/// ([`Stored::check`]), and a peer or a cache keeps, as they are, sealed or not.
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

/// Decodes `packed`, a part's packed bytes, opened if it was sealed, stored `encoding`, into `out`,
/// which is as long as the part.
fn decode(encoding: Encoding, packed: &[u8], out: &mut [u8]) -> Result<(), Corrupt> {
    match encoding {
        Encoding::Raw if packed.len() == out.len() => out.copy_from_slice(packed),
        Encoding::Raw => return Err(corrupt("raw part, of the wrong length")),
        Encoding::Zstd => {
            let unpacked = zstd::bulk::decompress_to_buffer(packed, out)
                .map_err(|err| corrupt(format_args!("part, by zstd: {err}")))?;
            if unpacked != out.len() {
                return Err(corrupt("compressed part, of the wrong length"));
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
    /// stored raw, in the clear; cladefs: or authenticated under a key its reader doesn't hold
    /// (TASK-411.3).
    pub fn as_is(&self) -> bool {
        self.stored.encoding == Encoding::Raw
            && matches!(self.opening, Opening::Clear | Opening::Unchecked(_))
    }

    /// Opens `bytes` in place, its stored bytes checked against its CRC, if it is sealed; cladefs:
    /// checks them against its tag, if it is authenticated under a key its reader holds
    /// (TASK-411.3).
    fn open_in_place(&self, bytes: &mut [u8]) -> Result<(), Error> {
        let (index, encoding) = (self.index, self.stored.encoding);
        match &self.opening {
            Opening::Clear | Opening::Unchecked(_) => Ok(()),
            Opening::Locked(key) => Err(Error::NoKey(*key)),
            Opening::Open(key) if key.seals() => {
                Ok(key.open_part(index, encoding, self.len(), bytes, &self.tag)?)
            }
            Opening::Open(key) => {
                Ok(key.check_part(index, encoding, self.len(), bytes, &self.tag)?)
            }
        }
    }

    /// Its packed bytes, from `stored`, its stored bytes and any after them: checked against its
    /// CRC, and opened in place, if it is sealed. Raw, they are its data; else compressed.
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
    /// sealed ([`Part::open`]), into `out`, which is as long as it is.
    pub fn unpack_into(&self, stored: &mut [u8], out: &mut [u8]) -> Result<(), Error> {
        let packed = self.open(stored)?;
        Ok(decode(self.stored.encoding, packed, out)?)
    }

    /// It, unpacked from `stored`, its stored bytes and any after them, checked against its CRC,
    /// and opened, if sealed, in a copy of them.
    pub fn unpack(&self, stored: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = vec![0; self.len() as usize];
        let checked = self.stored.check(stored)?;
        match (&self.opening, self.stored.encoding) {
            (Opening::Clear | Opening::Unchecked(_), encoding) => {
                decode(encoding, checked, &mut out)?
            }
            // cladefs: checked where it is, then unpacked (TASK-411.3).
            (Opening::Open(key), encoding) if !key.seals() => {
                key.check_part(self.index, encoding, self.len(), checked, &self.tag)?;
                decode(encoding, checked, &mut out)?;
            }
            // Opened where it is unpacked to.
            (_, Encoding::Raw) => {
                if checked.len() != out.len() {
                    return Err(corrupt("raw part, of the wrong length").into());
                }
                out.copy_from_slice(checked);
                self.open_in_place(&mut out)?;
            }
            (_, Encoding::Zstd) => {
                let mut packed = checked.to_vec();
                self.open_in_place(&mut packed)?;
                decode(Encoding::Zstd, &packed, &mut out)?;
            }
        }
        Ok(out)
    }
}

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
/// that shrinks it, or raw; sealed, if its object is; the CRC of its bytes as stored, and its tag
/// (TASK-336, TASK-411.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedPart {
    /// Its index in its object, which its nonce is.
    index: u64,
    encoding: Encoding,
    /// Its bytes as stored, unless they are the part's, as they are: raw, in the clear.
    stored: Option<Vec<u8>>,
    crc: u32,
    tag: Tag,
    /// cladefs: how its object is keyed, if it is (TASK-411.3).
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
    fn stored<'a>(&'a self, part: &'a [u8]) -> &'a [u8] {
        self.stored.as_deref().unwrap_or(part)
    }

    /// cladefs: its bytes as stored, unless they are the part's own, raw in the clear: compressed,
    /// or sealed. What a store holds of a part packed as its segment fills, with the part's bytes
    /// if these are none (TASK-400).
    pub fn stored_bytes(&self) -> Option<&[u8]> {
        self.stored.as_deref()
    }

    /// cladefs: the bytes of memory [`PackedPart::stored_bytes`] take.
    pub fn stored_capacity(&self) -> usize {
        self.stored.as_ref().map_or(0, Vec::capacity)
    }

    /// cladefs: lets go of what the buffer of [`PackedPart::stored_bytes`] holds past them: a part's
    /// room, which compressing took (TASK-400).
    pub fn shrink_to_fit(&mut self) {
        if let Some(stored) = &mut self.stored {
            stored.shrink_to_fit();
        }
    }
}

/// cladefs: packs parts of an object's data one at a time, as [`Packer::push`] packs each: so that a
/// store packs a segment's parts on threads of its own, each part read there, rather than all its
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

    /// `part`, part `index` of an object's data, packed, as [`Packer::push`] packs it: sealed under
    /// `key`, its object's ([`Packer::key`]), if it is sealed (TASK-411.2); cladefs: tagged under it,
    /// if it is authenticated (TASK-411.3).
    pub fn pack(&mut self, index: u64, part: &[u8], key: Option<&ObjectKey>) -> PackedPart {
        let compressed =
            self.compressor.as_mut().and_then(|compressor| pack_part(part, compressor));
        let encoding = if compressed.is_some() { Encoding::Zstd } else { Encoding::Raw };
        let len = part.len() as u64;
        let (stored, tag) = match key {
            None => (compressed, [0; TAG_LEN]),
            Some(key) if key.seals() => {
                let mut stored = compressed.unwrap_or_else(|| part.to_vec());
                let tag = key.seal_part(index, encoding, len, &mut stored);
                (Some(stored), tag)
            }
            Some(key) => {
                let tag = key.tag_part(index, encoding, len, compressed.as_deref().unwrap_or(part));
                (compressed, tag)
            }
        };
        let crc = crc(stored.as_deref().unwrap_or(part));
        PackedPart { index, encoding, stored, crc, tag, keyed: key.map(|key| key.keyed) }
    }

    /// cladefs: the bytes `part`, a part of an object's data, or a piece of one, is stored in:
    /// compressed, or raw if that is no smaller. What a store samples a segment's parts by, to tell
    /// whether compressing them is worth writing it again (TASK-310). Sealing adds no byte.
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

/// `data`'s parts, packed, the first part `first` of its object, sealed under `key` if given: on
/// threads of their own if there are several.
fn pack_parts(data: &[u8], first: u64, key: Option<&ObjectKey>) -> Vec<PackedPart> {
    let parts: Vec<&[u8]> = data.chunks(PART as usize).collect();
    let threads =
        std::thread::available_parallelism().map_or(1, |threads| threads.get()).min(parts.len());
    let pack = |first: u64, parts: &[&[u8]]| {
        let mut packer = PartPacker::new();
        let parts = parts.iter().zip(first..);
        parts.map(|(part, index)| packer.pack(index, part, key)).collect::<Vec<_>>()
    };
    if threads <= 1 || (level() == 0 && key.is_none()) {
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
    stored: Vec<Stored>,
    tags: Vec<Tag>,
    data_len: u64,
    /// The bytes of the object so far.
    end: u64,
    /// cladefs: its subkeys, if it is sealed (TASK-411.2).
    key: Option<ObjectKey>,
}

impl Packer {
    /// A packer of an object in the clear, or, given its branch's DEK and which it is, `seal`,
    /// sealed under subkeys of the DEK from a salt drawn for it alone (TASK-411.2).
    pub fn new(seal: Option<(&Secret, KeyId)>) -> Self {
        Self::keyed(seal.map(|(dek, id)| (dek, id, Keyed::Sealed)))
    }

    /// cladefs: a packer of an object in the clear, or, given its branch's DEK, which it is, and
    /// how, `keyed` under subkeys of the DEK from a salt drawn for it alone: sealed, or
    /// authenticated alone (TASK-411.3).
    pub fn keyed(keyed: Option<(&Secret, KeyId, Keyed)>) -> Self {
        let key = keyed.map(|(dek, id, keyed)| ObjectKey::new(dek, id, Salt::random(), keyed));
        Self { stored: Vec::new(), tags: Vec::new(), data_len: 0, end: 0, key }
    }

    /// The bytes of the object so far.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// cladefs: its subkeys, if it is sealed: what the threads that pack its parts seal them under
    /// ([`PartPacker::pack`], TASK-411.2).
    pub fn key(&self) -> Option<&ObjectKey> {
        self.key.as_ref()
    }

    /// cladefs: how the object is sealed, as its trailer will say (TASK-411.2).
    pub fn seal(&self) -> Seal {
        self.key.as_ref().map_or(Seal::Clear, ObjectKey::seal)
    }

    /// cladefs: the index of the object's next part (TASK-411.2).
    pub fn next_part(&self) -> u64 {
        self.stored.len() as u64
    }

    /// cladefs: whether every part so far is stored raw, none compressing, in the clear: the
    /// object's bytes so far are its data then, as they are (TASK-310).
    pub fn raw(&self) -> bool {
        self.key.is_none() && self.stored.iter().all(|stored| stored.encoding == Encoding::Raw)
    }

    /// cladefs: packs `data`, the object's first parts, each stored raw, compressing none, as
    /// [`Packer::push`] stores those that don't compress: the object's bytes are `data` itself, as
    /// it is, where it is, which this doesn't return (TASK-310). Whole parts, but for the last. An
    /// object in the clear's alone: a sealed one's bytes are never its data.
    pub fn push_raw(&mut self, data: &[u8]) {
        let parts = data.chunks(PART as usize);
        self.push_raw_parts(parts.map(|part| (part.len() as u64, crc(part))));
    }

    /// cladefs: [`Packer::push_raw`], of parts given by their lengths and CRCs, as a store reads and
    /// checksums them on threads of its own (TASK-336).
    pub fn push_raw_parts(&mut self, parts: impl IntoIterator<Item = (u64, u32)>) {
        assert_eq!(self.end, 0, "raw parts first");
        assert!(self.key.is_none(), "a sealed object's bytes are not its data");
        for (len, crc) in parts {
            assert!(self.data_len.is_multiple_of(PART), "parts follow a part cut short");
            let offset = self.end;
            self.stored.push(Stored { offset, len, crc, encoding: Encoding::Raw });
            self.tags.push([0; TAG_LEN]);
            self.data_len += len;
            self.end += len;
        }
    }

    /// Packs `data`, the object's next parts: whole parts, but for the object's last. Returns the
    /// object's next bytes, which follow those returned before.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let packed = pack_parts(data, self.next_part(), self.key.as_ref());
        for (part, packed) in data.chunks(PART as usize).zip(packed) {
            self.push_part(&mut bytes, part, &packed);
        }
        bytes
    }

    /// cladefs: packs `part`, the object's next part, as `packed`, [`PartPacker::pack`]'s of it,
    /// says, appending the object's next bytes to `bytes` (TASK-336). A whole part, but for the
    /// object's last; packed as the object's next, sealed under its key if it is sealed.
    pub fn push_part(&mut self, bytes: &mut Vec<u8>, part: &[u8], packed: &PackedPart) {
        self.push_stored(bytes, part.len() as u64, packed.stored(part), packed);
    }

    /// cladefs: [`Packer::push_part`], of a part of `len` bytes of data given by its bytes as
    /// stored, `stored`: compressed or sealed if `packed` is, else the part's own. So that a store packs a
    /// part as its segment fills, and holds it compressed, its data gone, until the segment's
    /// framing (TASK-400).
    pub fn push_stored(
        &mut self,
        bytes: &mut Vec<u8>,
        len: u64,
        stored: &[u8],
        packed: &PackedPart,
    ) {
        assert!(self.data_len.is_multiple_of(PART), "parts follow a part cut short");
        assert_eq!(packed.index, self.next_part(), "a part packed as the object's next");
        let keyed = self.key.as_ref().map(|key| key.keyed);
        assert_eq!(packed.keyed, keyed, "a part sealed as its object is");
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

    /// cladefs: [`Packer::finish`]'s bytes, and the object's parts, as its writer reads them: opened
    /// under its key, if it is sealed, which its writer holds, needing no [`Keys`] to find it
    /// (TASK-411.2).
    pub fn finish_parts(self, footer: &[u8]) -> (Vec<u8>, Parts) {
        let table = Table { stored: &self.stored, tags: &self.tags };
        let tail = tail(table, self.data_len, self.end, footer, self.key.as_ref());
        let frame = Frame::of_trailer(&tail).expect("a trailer as packed");
        let opening = self.key.map_or(Opening::Clear, |key| Opening::Open(Arc::new(key)));
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
/// at `end`, sealed under `key` if given: `footer`, the table, and the trailer.
fn tail(
    table: Table<'_>,
    data_len: u64,
    end: u64,
    footer: &[u8],
    key: Option<&ObjectKey>,
) -> Vec<u8> {
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
    let mut seal = [0; SEAL_LEN];
    if let Some(key) = key {
        seal[0] = match key.keyed {
            Keyed::Sealed => 1,
            Keyed::Authenticated => 2,
        };
        seal[1..9].copy_from_slice(&key.id.branch.to_le_bytes());
        seal[9..13].copy_from_slice(&key.id.generation.to_le_bytes());
        seal[13..].copy_from_slice(&key.salt.0);
    }
    bytes.extend_from_slice(&seal);
    bytes.extend_from_slice(&key.map_or([0; MAC_LEN], |key| key.mac.mac(footer)));
    // The table's MAC covers the trailer before it: the footer's MAC, the seal, the lengths.
    bytes.extend_from_slice(&key.map_or([0; MAC_LEN], |key| key.mac.mac(&bytes[start..])));
    // And its CRC, the MAC too.
    bytes.extend_from_slice(&crc(&bytes[start..]).to_le_bytes());
    bytes.extend_from_slice(&MAGIC);
    bytes
}

/// cladefs: the whole object `object`, of `footer` in place of its own: its parts as they are, stored
/// where they are, then a tail anew, so that its data reads as it did, nothing unpacked or packed
/// again. What a host's write-ahead log uploads of a segment its writer's chain stamped anew after
/// a takeover (doc-14, Opening a store; TASK-165.9). Fails if `object` is not a whole object. A
/// sealed object's new footer and table are authenticated under its own subkeys, from its salt,
/// which its parts stay sealed under: so `keys` must hold its DEK, its table checked against its
/// MAC first, or it fails ([`Error::NoKey`], TASK-411.2); cladefs: an authenticated one's alike
/// (TASK-411.3).
pub fn restamp(object: &[u8], footer: &[u8], keys: &dyn Keys) -> Result<Vec<u8>, Error> {
    let parts = Parts::of(object, keys)?;
    let key = match &parts.opening {
        Opening::Locked(key) | Opening::Unchecked(key) => return Err(Error::NoKey(*key)),
        opening => opening.key(),
    };
    let (frame, table) = (parts.frame, Table { stored: &parts.stored, tags: &parts.tags });
    let mut bytes = object[..frame.stored_len as usize].to_vec();
    bytes.extend_from_slice(&tail(table, frame.data_len, frame.stored_len, footer, key));
    Ok(bytes)
}

/// The bytes of an object of `data` and `footer`: sealed, if `seal` gives a DEK and which it is
/// ([`Packer::new`]).
pub fn framed(data: &[u8], footer: &[u8], seal: Option<(&Secret, KeyId)>) -> Vec<u8> {
    framed_keyed(data, footer, seal.map(|(dek, id)| (dek, id, Keyed::Sealed)))
}

/// cladefs: the bytes of an object of `data` and `footer`: keyed as `keyed` says, if it gives a DEK,
/// which it is, and how ([`Packer::keyed`], TASK-411.3).
pub fn framed_keyed(data: &[u8], footer: &[u8], keyed: Option<(&Secret, KeyId, Keyed)>) -> Vec<u8> {
    let mut packer = Packer::keyed(keyed);
    let mut bytes = packer.push(data);
    bytes.extend_from_slice(&packer.finish(footer));
    bytes
}

/// Where the pieces of an object [`framed`] are, as its trailer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub data_len: u64,
    /// The bytes of the parts stored, which the footer follows.
    pub stored_len: u64,
    pub footer_len: u64,
    footer_crc: u32,
    /// cladefs: whether its parts are sealed, and how (TASK-411.2).
    pub seal: Seal,
    footer_mac: [u8; MAC_LEN],
    mac: [u8; MAC_LEN],
    table_crc: u32,
}

impl Frame {
    /// The frame an object's last bytes, its [`TRAILER`] at their end, give.
    pub fn of_trailer(bytes: &[u8]) -> Result<Self, Corrupt> {
        // Its magic first: a frame of another format, its trailer of another length, is named.
        let magic = bytes.len().checked_sub(MAGIC.len()).ok_or_else(|| corrupt("trailer"))?;
        let magic = &bytes[magic..];
        if magic != MAGIC {
            let (of, format) = magic.split_at(MAGIC_OF.len());
            return Err(match (of == MAGIC_OF, format[0].checked_sub(b'0')) {
                (true, Some(format @ 0..=9)) => corrupt(format_args!(
                    "trailer: a frame of format {format}, which a reader of format {FORMAT} \
                     doesn't read"
                )),
                _ => corrupt("trailer, by its magic"),
            });
        }
        let at = bytes.len().checked_sub(TRAILER as usize).ok_or_else(|| corrupt("trailer"))?;
        let trailer = &bytes[at..];
        let mac = |at: usize| -> [u8; MAC_LEN] { trailer[at..at + MAC_LEN].try_into().unwrap() };
        let frame = Self {
            data_len: u64_at(trailer, at::DATA_LEN),
            stored_len: u64_at(trailer, at::STORED_LEN),
            footer_len: u64_at(trailer, at::FOOTER_LEN),
            footer_crc: u32_at(trailer, at::FOOTER_CRC),
            seal: Self::seal_of(&trailer[at::SEAL..at::SEAL + SEAL_LEN])?,
            footer_mac: mac(at::FOOTER_MAC),
            mac: mac(at::MAC),
            table_crc: u32_at(trailer, at::TABLE_CRC),
        };
        if frame.seal == Seal::Clear
            && (frame.footer_mac, frame.mac) != ([0; MAC_LEN], [0; MAC_LEN])
        {
            return Err(corrupt("trailer: MACs, in the clear"));
        }
        // Lengths so large that the object's would overflow are corrupt.
        (frame.data_len.checked_add(PART))
            .and_then(|_| frame.stored_len.checked_add(frame.footer_len))
            .and_then(|end| end.checked_add(frame.parts().checked_mul(ENTRY)?))
            .and_then(|end| end.checked_add(TRAILER))
            .ok_or_else(|| corrupt("trailer, by its lengths"))?;
        Ok(frame)
    }

    /// The seal a trailer's [`SEAL_LEN`] bytes say.
    fn seal_of(seal: &[u8]) -> Result<Seal, Corrupt> {
        match seal[0] {
            0 if seal[1..].iter().all(|&byte| byte == 0) => Ok(Seal::Clear),
            0 => Err(corrupt("trailer: a seal in the clear naming a key")),
            kind @ (1 | 2) => {
                let key = KeyId { branch: u64_at(seal, 1), generation: u32_at(seal, 9) };
                let salt = Salt(seal[13..].try_into().unwrap());
                Ok(match kind {
                    1 => Seal::Sealed { key, salt },
                    _ => Seal::Authenticated { key, salt },
                })
            }
            kind => Err(corrupt(format_args!("trailer: a seal of kind {kind}"))),
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
    /// sealed under a key `keys` holds (TASK-411.2). A footer read with its trailer alone, not its
    /// table, is checked so: its MAC, under the object's subkeys, says the object's writer wrote it
    /// with that trailer. cladefs: refused as [`Opening`] refuses an object (TASK-411.3).
    pub fn check_footer<'a>(&self, bytes: &'a [u8], keys: &dyn Keys) -> Result<&'a [u8], Error> {
        Ok(self.check_footer_opened(bytes, &Opening::of(self.seal, keys)?)?)
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
        if let Some(key) = opening.key() {
            key.mac.verify(bytes, &self.footer_mac).map_err(|_| corrupt("footer, by its MAC"))?;
        }
        Ok(bytes)
    }

    /// Where its parts are stored, and their tags, as the table in `suffix`, the object's last
    /// bytes, which must hold it, says, checked against its CRC; and where in `suffix` the table
    /// begins.
    fn stored_in(&self, suffix: &[u8]) -> Result<(Vec<Stored>, Vec<Tag>, usize), Corrupt> {
        let at = (suffix.len() as u64 + self.table().start)
            .checked_sub(self.size())
            .ok_or_else(|| corrupt("table, cut short"))? as usize;
        let trailer = suffix.len() - TRAILER as usize;
        if crc(&suffix[at..trailer + at::TABLE_CRC]) != self.table_crc {
            return Err(corrupt("table, by its CRC"));
        }
        let table = &suffix[at..trailer];
        let mut stored = Vec::with_capacity(self.parts() as usize);
        let mut tags = Vec::with_capacity(self.parts() as usize);
        let mut end = 0;
        for (part, entry) in table.chunks(ENTRY as usize).enumerate() {
            let encoding = match entry[16] {
                0 => Encoding::Raw,
                1 => Encoding::Zstd,
                other => return Err(corrupt(format_args!("part {part}'s encoding {other}"))),
            };
            let entry_stored = Stored {
                offset: u64_at(entry, 0),
                len: u32_at(entry, 8) as u64,
                crc: u32_at(entry, 12),
                encoding,
            };
            let tag: Tag = entry[17..].try_into().unwrap();
            if self.seal == Seal::Clear && tag != [0; TAG_LEN] {
                return Err(corrupt(format_args!("part {part}'s tag, in the clear")));
            }
            let len = self.part(part as u64).end - self.part(part as u64).start;
            let fits = match encoding {
                Encoding::Raw => entry_stored.len == len,
                Encoding::Zstd => entry_stored.len > 0 && entry_stored.len < len,
            };
            if !fits
                || !entry_stored.offset.is_multiple_of(ALIGN)
                || entry_stored.offset < end
                || entry_stored.range().end > self.stored_len
            {
                return Err(corrupt(format_args!("part {part}'s place")));
            }
            end = entry_stored.range().end;
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
    /// sealed under a key `keys` holds (TASK-411.2). cladefs: refused if it is in the clear and its
    /// reader holds its objects sealed ([`Error::Clear`], TASK-411.3).
    pub fn of_suffix(suffix: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        Self::in_suffix(Frame::of_trailer(suffix)?, suffix, keys)
    }

    /// The parts of a whole object, read by a reader holding `keys`, as [`Parts::of_suffix`].
    pub fn of(bytes: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        Self::in_suffix(Frame::of(bytes)?, bytes, keys)
    }

    fn in_suffix(frame: Frame, suffix: &[u8], keys: &dyn Keys) -> Result<Self, Error> {
        let (stored, tags, table) = frame.stored_in(suffix)?;
        let opening = Opening::of(frame.seal, keys)?;
        if let Some(key) = opening.key() {
            let trailer = suffix.len() - TRAILER as usize;
            let authenticated = &suffix[table..trailer + at::MAC];
            key.mac.verify(authenticated, &frame.mac).map_err(|_| corrupt("table, by its MAC"))?;
        }
        Ok(Self { frame, stored, tags, opening })
    }

    /// cladefs: whether its reader opens its parts: they are in the clear, or sealed under a key it
    /// holds (TASK-411.2).
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

/// The data of a whole object [`framed`], each part checked against its CRC, and opened, if sealed,
/// under a key `keys` holds.
pub fn data(bytes: &[u8], keys: &dyn Keys) -> Result<Vec<u8>, Error> {
    let parts = Parts::of(bytes, keys)?;
    parts.read(0..parts.frame.data_len, |range| {
        Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
    })
}

/// Checks a whole object [`framed`]: its trailer, table and footer, and each part's stored bytes
/// against its CRC, unpacking none. cladefs: what a host checks of an object it sends on as it is,
/// as an uploader does a store's closed segments (TASK-153.11). With no key: a sealed object's
/// MACs and tags are its readers' to check (TASK-411.2); and what tells a whole object from one
/// cut short, sealed or not, as a store's recovery of its write-ahead log does (TASK-411.3).
pub fn check(bytes: &[u8]) -> Result<(), Corrupt> {
    let parts = Parts::of(bytes, &NoKeys).map_err(|err| match err {
        Error::Corrupt(corrupt) => corrupt,
        Error::NoKey(_) | Error::Clear => unreachable!("a reader holding no key refuses nothing"),
    })?;
    let footer = parts.frame.footer();
    parts.check_footer(&bytes[footer.start as usize..footer.end as usize])?;
    for stored in &parts.stored {
        stored.check(&bytes[stored.offset as usize..])?;
    }
    Ok(())
}

/// The footer of a whole object [`framed`], checked against its CRC, and its MAC if it is sealed
/// under a key `keys` holds.
pub fn footer<'a>(bytes: &'a [u8], keys: &dyn Keys) -> Result<&'a [u8], Error> {
    let parts = Parts::of(bytes, keys)?;
    let footer = parts.frame.footer();
    Ok(parts.check_footer(&bytes[footer.start as usize..footer.end as usize])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The DEK of the tests' sealed objects, and which it is.
    const ID: KeyId = KeyId { branch: 0x7b, generation: 2 };

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

    /// A packer of an object sealed under [`dek`] with `salt`, as no packer outside the tests is:
    /// so that two pack alike.
    fn sealed_with(salt: Salt) -> Packer {
        Packer { key: Some(ObjectKey::new(&dek(), ID, salt, Keyed::Sealed)), ..Packer::new(None) }
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

    /// cladefs: parts packed one at a time pack as `push` packs them all (TASK-336), in the clear
    /// and sealed (TASK-411.2).
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
        for (mut all, mut one) in
            [(Packer::new(None), Packer::new(None)), (sealed_with(salt), sealed_with(salt))]
        {
            let pushed = all.push(&data);
            let (mut bytes, mut packer) = (Vec::new(), PartPacker::new());
            for (index, part) in data.chunks(PART as usize).enumerate() {
                let packed = packer.pack(index as u64, part, one.key());
                one.push_part(&mut bytes, part, &packed);
            }
            assert_eq!(bytes, pushed);
            assert!(!one.raw(), "parts compressed, or sealed");
            assert_eq!(one.finish(b"footer"), all.finish(b"footer"));
        }
        let mut raw = Packer::new(None);
        raw.push_raw(&data);
        let mut given = Packer::new(None);
        let parts = data.chunks(PART as usize);
        given.push_raw_parts(parts.map(|part| (part.len() as u64, crc(part))));
        assert_eq!(given.finish(b"footer"), raw.finish(b"footer"));
    }

    /// TASK-411.2: a part packed for another index than its object's next, or sealed where its
    /// object is not, is no part of it.
    #[test]
    #[should_panic(expected = "a part packed as the object's next")]
    fn parts_packed_out_of_order_are_refused() {
        let mut packer = Packer::new(None);
        let packed = PartPacker::new().pack(1, b"part", None);
        packer.push_part(&mut Vec::new(), b"part", &packed);
    }

    #[test]
    #[should_panic(expected = "a part sealed as its object is")]
    fn parts_sealed_into_objects_in_the_clear_are_refused() {
        let sealing = sealed_with(Salt([3; SALT_LEN]));
        let packed = PartPacker::new().pack(0, b"part", sealing.key());
        Packer::new(None).push_part(&mut Vec::new(), b"part", &packed);
    }

    /// TASK-411.2: a sealed object's bytes are never its data as they are.
    #[test]
    #[should_panic(expected = "a sealed object's bytes are not its data")]
    fn sealed_objects_store_no_parts_as_they_are() {
        sealed_with(Salt([3; SALT_LEN])).push_raw(b"data");
    }

    /// Objects hold their data in parts, compressed, each where the table says, and their footers;
    /// cladefs: in the clear, their trailers saying so (TASK-411.2).
    #[test]
    fn objects_hold_their_parts() {
        let data = pattern(3 * PART + 7);
        let bytes = framed(&data, b"footer", None);
        let parts = Parts::of(&bytes, &NoKeys).unwrap();
        let frame = parts.frame;
        assert_eq!((frame.data_len, frame.footer_len, frame.parts()), (3 * PART + 7, 6, 4));
        assert_eq!(frame.seal, Seal::Clear);
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
        assert_eq!(super::data(&bytes, &NoKeys).unwrap(), data);
        assert_eq!(footer(&bytes, &NoKeys).unwrap(), b"footer");
        // The table is found from a suffix holding it.
        let suffix = &bytes[frame.table().start as usize - 3..];
        assert_eq!(Parts::of_suffix(suffix, &NoKeys).unwrap(), parts);
        assert_eq!(Frame::of_trailer(&bytes[bytes.len() - TRAILER as usize..]).unwrap(), frame);
        assert!(frame.stored_in(&bytes[frame.table().start as usize + 1..]).is_err());
        // A reader holding keys reads it as one holding none.
        assert_eq!(Parts::of(&bytes, &holding(dek())).unwrap(), parts);
        assert_eq!(super::data(&bytes, &holding(dek())).unwrap(), data);

        let empty = framed(&[], &[], None);
        assert_eq!(Frame::of(&empty).unwrap().data_len, 0);
        assert_eq!(empty.len() as u64, TRAILER);
        assert_eq!(super::data(&empty, &NoKeys).unwrap(), Vec::<u8>::new());

        // cladefs: its parts stored raw, its bytes its data, then its tail (TASK-310).
        let mut raw = Packer::new(None);
        raw.push_raw(&data);
        assert!(raw.raw());
        let mut whole = data.clone();
        whole.extend_from_slice(&raw.finish(b"footer"));
        assert_eq!(super::data(&whole, &NoKeys).unwrap(), data);
        assert_eq!(Frame::of(&whole).unwrap().data_len, data.len() as u64);

        // Written in pieces, front to back, it is the same object.
        let mut packer = Packer::new(None);
        let mut pieces = packer.push(&data[..2 * PART as usize]);
        assert_eq!(packer.end(), pieces.len() as u64);
        pieces.extend(packer.push(&data[2 * PART as usize..]));
        let (tail, written) = packer.finish_parts(b"footer");
        pieces.extend(tail);
        assert_eq!(pieces, bytes);
        assert_eq!(written, parts);
    }

    /// TASK-411.2: a sealed object holds none of its data's bytes, nor those its parts compress
    /// to, where its twin in the clear holds them; its trailer names its key and salt; it reads
    /// back, whole and by parts, under its key, and its footer with it; written in pieces it is
    /// the same object.
    #[test]
    fn sealed_objects_hold_no_plaintext_and_read_back_under_their_key() {
        let data = [pattern(2 * PART + 5), random(PART + 9)].concat();
        let clear = framed(&data, b"footer", None);
        let salt = Salt([9; SALT_LEN]);
        let mut packer = sealed_with(salt);
        let mut bytes = packer.push(&data[..PART as usize]);
        bytes.extend(packer.push(&data[PART as usize..]));
        bytes.extend(packer.finish(b"footer"));
        let mut once = sealed_with(salt);
        let mut whole = once.push(&data);
        let (tail, written) = once.finish_parts(b"footer");
        whole.extend(tail);
        assert_eq!(bytes, whole);

        let keys = holding(dek());
        let (sealed, plain) =
            (Parts::of(&bytes, &keys).unwrap(), Parts::of(&clear, &NoKeys).unwrap());
        // Its writer reads it as a reader holding its key does.
        assert_eq!(written, sealed);
        assert_eq!(sealed.frame.seal, Seal::Sealed { key: ID, salt });
        assert!(sealed.opens());
        // Compressed as in the clear: as long, each part, sealing adding nothing but its tag.
        let encodings = |parts: &Parts| -> Vec<(Encoding, u64)> {
            parts.stored.iter().map(|stored| (stored.encoding, stored.len)).collect()
        };
        let encoded = encodings(&plain);
        assert_eq!(encodings(&sealed), encoded);
        assert_eq!(
            encoded.iter().map(|(encoding, _)| *encoding).collect::<Vec<_>>(),
            [Encoding::Zstd, Encoding::Zstd, Encoding::Raw, Encoding::Raw]
        );
        assert_eq!(bytes.len(), clear.len());
        for (index, stored) in plain.stored.iter().enumerate() {
            let range = stored.offset as usize..(stored.offset + stored.len) as usize;
            // The first 64 bytes of what the part is stored as in the clear, and of its data.
            let clear = &clear[range.start..range.end.min(range.start + 64)];
            assert!(!holds(&bytes, clear), "part {index}");
            let place = plain.frame.part(index as u64);
            let data = &data[place.start as usize..place.end as usize];
            assert!(!holds(&bytes, &data[..data.len().min(64)]), "part {index}'s data");
        }
        assert_eq!(super::data(&bytes, &keys).unwrap(), data);
        assert_eq!(footer(&bytes, &keys).unwrap(), b"footer");
        for index in 0..sealed.frame.parts() {
            let part = sealed.part(index).unwrap();
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
        let suffix = &bytes[sealed.frame.table().start as usize - 1..];
        assert_eq!(Parts::of_suffix(suffix, &keys).unwrap(), sealed);
        let read = sealed.read(PART - 3..2 * PART + 2, |range| {
            Ok::<_, Error>(bytes[range.start as usize..range.end as usize].to_vec())
        });
        assert_eq!(read.unwrap(), &data[PART as usize - 3..2 * PART as usize + 2]);

        // A sealed object of no data: its trailer alone, sealed.
        let empty = framed(&[], b"", Some((&dek(), ID)));
        assert_eq!(empty.len() as u64, TRAILER);
        assert_eq!(super::data(&empty, &keys).unwrap(), Vec::<u8>::new());
        // Each object's salt its own.
        let (one, other) =
            (framed(b"x", b"", Some((&dek(), ID))), framed(b"x", b"", Some((&dek(), ID))));
        assert_ne!(Frame::of(&one).unwrap().seal, Frame::of(&other).unwrap().seal);
    }

    /// TASK-411.2: a sealed object read by a reader not holding its key: its footer and table read
    /// and checked against their CRCs, its parts' stored bytes checked, as the collector reads
    /// them; its data read by none, refused naming its key, never given as it is stored; restamped
    /// by none. Under another key of the same name: refused, by its MAC first, by its parts' tags
    /// past it.
    #[test]
    fn sealed_objects_open_under_their_key_alone() {
        let data = [random(PART), pattern(PART / 2)].concat();
        let bytes = framed(&data, b"footer", Some((&dek(), ID)));
        let locked = Parts::of(&bytes, &NoKeys).unwrap();
        assert!(!locked.opens());
        assert_eq!(footer(&bytes, &NoKeys).unwrap(), b"footer");
        assert_eq!(check(&bytes), Ok(()));
        let no_key = Error::NoKey(ID);
        assert_eq!(
            no_key.to_string(),
            "an object sealed under branch 7b's key of generation 2, which its reader doesn't hold"
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
        let Seal::Sealed { salt, .. } = locked.frame.seal else { panic!("sealed") };
        let keys = holding(dek());
        let opened = Parts::of(&bytes, &keys).unwrap();
        let wrong_key =
            Opening::Open(Arc::new(ObjectKey::new(&wrong[&ID], ID, salt, Keyed::Sealed)));
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

    /// A reader holding `keys`, which holds the objects it reads sealed: a reader of a branch whose
    /// record has a DEK (TASK-411.3).
    struct Sealing(HashMap<KeyId, Secret>);

    impl Keys for Sealing {
        fn dek(&self, id: KeyId) -> Option<&Secret> {
            self.0.get(&id)
        }

        fn sealed(&self) -> bool {
            true
        }
    }

    /// TASK-411.3: an authenticated object holds its parts as its twin in the clear stores them,
    /// tagged: read alike by a reader holding no key, which checks their CRCs alone, as the
    /// collector reads a checkpoint, and by one holding its key, which checks their tags and its
    /// MACs too, and refuses a part changed, its CRC made to match. Restamped under its key alone.
    #[test]
    fn authenticated_objects_read_keyless_and_check_under_their_key() {
        let data = [pattern(PART + 3), random(PART / 2)].concat();
        let keys = holding(dek());
        let bytes = framed_keyed(&data, b"footer", Some((&dek(), ID, Keyed::Authenticated)));
        let clear = framed(&data, b"footer", None);
        let parts = Parts::of(&bytes, &keys).unwrap();
        assert!(matches!(parts.frame.seal, Seal::Authenticated { key: ID, .. }));
        let stored_len = parts.frame.stored_len as usize;
        assert_eq!(&bytes[..stored_len], &clear[..stored_len]);
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

        // Restamped under its key, as a sealed one is; by no reader not holding it.
        let restamped = restamp(&bytes, b"another", &keys).unwrap();
        assert_eq!(footer(&restamped, &keys).unwrap(), b"another");
        assert_eq!(super::data(&restamped, &keys).unwrap(), data);
        assert_eq!(restamp(&bytes, b"another", &NoKeys), Err(Error::NoKey(ID)));
    }

    /// TASK-411.3: a reader holding the objects it reads sealed refuses one in the clear, whole,
    /// by its suffix, by its footer alone, its data or a restamping of it; and one authenticated
    /// under a key it doesn't hold. One sealed or authenticated under a key it holds it reads.
    #[test]
    fn readers_holding_objects_sealed_refuse_them_in_the_clear() {
        let data = pattern(PART + 1);
        let sealing = Sealing(HashMap::from([(ID, dek())]));
        let clear = framed(&data, b"footer", None);
        assert_eq!(Parts::of(&clear, &sealing), Err(Error::Clear));
        let suffix = &clear[Frame::of(&clear).unwrap().table().start as usize..];
        assert_eq!(Parts::of_suffix(suffix, &sealing), Err(Error::Clear));
        assert_eq!(super::data(&clear, &sealing), Err(Error::Clear));
        assert_eq!(footer(&clear, &sealing), Err(Error::Clear));
        assert_eq!(restamp(&clear, b"another", &sealing), Err(Error::Clear));
        let frame = Frame::of(&clear).unwrap();
        let footer_bytes = &clear[frame.footer().start as usize..frame.footer().end as usize];
        assert_eq!(frame.check_footer(footer_bytes, &sealing), Err(Error::Clear));
        assert_eq!(
            Error::Clear.to_string(),
            "an object in the clear, of a branch whose objects are sealed"
        );

        let sealed = framed(&data, b"footer", Some((&dek(), ID)));
        assert_eq!(super::data(&sealed, &sealing).unwrap(), data);

        // A downgrade: a sealed object's seal, MACs and tags zeroed, its CRCs made to match, as
        // one who can write the bucket, holding no key, would: refused by the branch's readers;
        // a reader holding no key, as the collector, finds its parts no zstd: ciphertext.
        let mut zeroed = sealed.clone();
        let frame = Frame::of(&zeroed).unwrap();
        let trailer = zeroed.len() - TRAILER as usize;
        zeroed[trailer + at::SEAL..trailer + at::TABLE_CRC].fill(0);
        for part in 0..frame.parts() {
            let tag = entry(&frame, part) + 17;
            zeroed[tag..tag + TAG_LEN].fill(0);
        }
        recrc(&mut zeroed);
        assert_eq!(Frame::of(&zeroed).unwrap().seal, Seal::Clear);
        assert_eq!(check(&zeroed), Ok(()));
        assert_eq!(Parts::of(&zeroed, &sealing), Err(Error::Clear));
        assert_eq!(super::data(&zeroed, &sealing), Err(Error::Clear));
        let ciphertext = corrupt("part, by zstd: Unknown frame descriptor");
        assert_eq!(super::data(&zeroed, &NoKeys), Err(ciphertext.into()));
        let other = KeyId { generation: 9, ..ID };
        let authenticated =
            framed_keyed(&data, b"footer", Some((&dek(), other, Keyed::Authenticated)));
        assert_eq!(super::data(&authenticated, &sealing), Err(Error::NoKey(other)));
        assert_eq!(super::data(&authenticated, &NoKeys).unwrap(), data);
    }

    /// TASK-411.2: a sealed part changed, its CRC with it or not, its tag changed, or moved to
    /// another index or another object of the same key, never reads as data: the CRC finds a part
    /// changed by itself; its tag, one whose CRC was made to match; the table's MAC, a table made
    /// to say any of it; each layer alone.
    #[test]
    fn sealed_parts_changed_or_moved_fail() {
        let data = random(3 * PART);
        let keys = holding(dek());
        let bytes = framed(&data, b"footer", Some((&dek(), ID)));
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
        let another = framed(&data, b"footer", Some((&dek(), ID)));
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

    /// TASK-411.2: a sealed object's footer, table or trailer changed, their CRCs made to match,
    /// as one holding no key would: its readers holding its key refuse it, by its MAC; one holding
    /// none reads it, as the collector would, as doc-16 accepts. Its plaintext twin's readers read
    /// it changed so, but for its trailer's seal.
    #[test]
    fn sealed_metadata_changed_fails_by_its_mac() {
        for seal in [Some((&dek(), ID)), None] {
            let bytes = framed(&pattern(PART + 1), b"footer", seal);
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
            let named =
                Err(Corrupt("an object's trailer: a seal in the clear naming a key".into()));
            let expected = match seal {
                Some(_) => [by_mac.clone(), by_mac.clone(), by_mac],
                None => [Ok(()), Ok(()), named.clone()],
            };
            let read = [read(&footer_changed), read(&table_changed), read(&salt_changed)];
            assert_eq!(read, expected, "sealed: {}", seal.is_some());
            let expected = match seal {
                Some(_) => [Ok(()), Ok(()), Ok(())],
                None => [Ok(()), Ok(()), named],
            };
            let unread = [unread(&footer_changed), unread(&table_changed), unread(&salt_changed)];
            assert_eq!(unread, expected, "sealed: {}", seal.is_some());
            assert_eq!(footer(&footer_changed, &NoKeys), Ok(&b"Footer"[..]));
        }
        // The footer alone, its CRC made to match, its trailer's MAC of the footer as it was.
        let bytes = framed(&pattern(PART + 1), b"footer", Some((&dek(), ID)));
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
        let mut bytes = framed(&data, b"", None);
        let parts = Parts::of(&bytes, &NoKeys).unwrap();
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
    /// footer it was given; what isn't a whole object is refused (TASK-165.9). Sealed, its salt and
    /// its parts' tags stay, and its new footer and table are authenticated under its subkeys
    /// (TASK-411.2).
    #[test]
    fn restamped_objects_keep_their_parts() {
        let data = [pattern(2 * PART), random(PART / 3)].concat();
        let object = framed(&data, b"footer of the first writer", None);
        let restamped = restamp(&object, b"a later writer's", &NoKeys).unwrap();
        assert_eq!(restamped, framed(&data, b"a later writer's", None));
        assert_eq!(footer(&restamped, &NoKeys).unwrap(), b"a later writer's");
        let (before, after) =
            (Parts::of(&object, &NoKeys).unwrap(), Parts::of(&restamped, &NoKeys).unwrap());
        assert_eq!((after.stored, after.frame.data_len), (before.stored, before.frame.data_len));
        assert_eq!(super::data(&restamped, &NoKeys).unwrap(), data);
        let empty = framed(&[], b"one", None);
        assert_eq!(restamp(&empty, b"two", &NoKeys).unwrap(), framed(&[], b"two", None));
        let cut = Err(Corrupt("an object's trailer, by its magic".into()).into());
        assert_eq!(restamp(&object[..object.len() - 1], b"cut short", &NoKeys), cut);

        // Sealed.
        let keys = holding(dek());
        let salt = Salt([4; SALT_LEN]);
        let mut packer = sealed_with(salt);
        let mut object = packer.push(&data);
        object.extend(packer.finish(b"footer of the first writer"));
        let restamped = restamp(&object, b"a later writer's", &keys).unwrap();
        let mut packer = sealed_with(salt);
        let mut expected = packer.push(&data);
        expected.extend(packer.finish(b"a later writer's"));
        assert_eq!(restamped, expected);
        assert_eq!(footer(&restamped, &keys).unwrap(), b"a later writer's");
        let (before, after) =
            (Parts::of(&object, &keys).unwrap(), Parts::of(&restamped, &keys).unwrap());
        assert_eq!((after.stored, after.tags), (before.stored, before.tags));
        assert_eq!(after.frame.seal, Seal::Sealed { key: ID, salt });
        assert_eq!(super::data(&restamped, &keys).unwrap(), data);
        assert_eq!(
            &restamped[..before.frame.stored_len as usize],
            &object[..before.frame.stored_len as usize]
        );
    }

    /// cladefs: a part mostly incompressible, a sixteenth of it compressing, is stored compressed,
    /// saving less than an eighth: what a store frames in place nonetheless (TASK-310).
    #[test]
    fn parts_compressing_little_are_stored_compressed() {
        let mut data = random(128 << 10);
        data[..8 << 10].copy_from_slice(&pattern(8 << 10));
        let mut packer = Packer::new(None);
        let packed = packer.push(&data);
        assert!(!packer.raw(), "stored raw");
        assert!(packed.len() > data.len() - data.len() / 8, "{} of {}", packed.len(), data.len());
    }

    /// Parts that zstd does not shrink are stored raw: incompressible data costs the table, and
    /// nothing more; cladefs: sealed, the same (TASK-411.2).
    #[test]
    fn incompressible_data_costs_its_table() {
        let data = random(2 * PART + 4096);
        for seal in [None, Some((&dek(), ID))] {
            let bytes = framed(&data, b"footer", seal);
            let parts = Parts::of(&bytes, &holding(dek())).unwrap();
            assert!(parts.stored.iter().all(|stored| stored.encoding == Encoding::Raw));
            assert_eq!(parts.frame.stored_len, data.len() as u64);
            assert_eq!(bytes.len() as u64, data.len() as u64 + 6 + 3 * ENTRY + TRAILER);
            assert_eq!(super::data(&bytes, &holding(dek())).unwrap(), data);
        }
        let clear = framed(&data, b"footer", None);
        assert_eq!(&clear[..data.len()], &data[..]);
        assert_eq!((ENTRY, TRAILER), (33, 149));
    }

    /// A frame says its format, last: one of another format than the reader's is refused, naming
    /// it, rather than called corrupt (TASK-141.2); cladefs: a frame of format 1, the last before
    /// objects were sealed, among them, of any length, the shortest as long as its trailer was,
    /// shorter than a trailer is now (TASK-411.2).
    #[test]
    fn frames_of_other_formats_are_named() {
        let bytes = framed(b"data", b"footer", None);
        assert_eq!(&bytes[bytes.len() - 8..], b"cladefs2");
        let mut newer = bytes.clone();
        *newer.last_mut().unwrap() = b'0' + FORMAT + 1;
        let refused =
            "an object's trailer: a frame of format 3, which a reader of format 2 doesn't read";
        assert_eq!(corrupt_of(Parts::of(&newer, &NoKeys)), Err(Corrupt(refused.into())));
        assert_eq!(check(&newer), Err(Corrupt(refused.into())));
        *newer.last_mut().unwrap() = b'x';
        assert_eq!(
            corrupt_of(Parts::of(&newer, &NoKeys)),
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
        let refused =
            "an object's trailer: a frame of format 1, which a reader of format 2 doesn't read";
        let refused = Corrupt(refused.into());
        for old in [format_1(&random(4096), b"footer"), format_1(&[], b"")] {
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

    /// TASK-411.2: a trailer's seal is in the clear, all zeros but its kind, or sealed; of another
    /// kind, or in the clear but naming a key or carrying MACs, or tags, it is corrupt.
    #[test]
    fn seals_are_as_their_kinds_say() {
        let bytes = framed(&random(4096), b"footer", None);
        let trailer = bytes.len() - TRAILER as usize;
        let refused = |at: usize, value: u8| {
            let mut changed = bytes.clone();
            changed[at] = value;
            recrc(&mut changed);
            corrupt_of(Parts::of(&changed, &NoKeys).map(|_| ()))
        };
        let corrupt = |what: &str| Err(Corrupt(format!("an object's {what}")));
        assert_eq!(refused(trailer + at::SEAL, 3), corrupt("trailer: a seal of kind 3"));
        assert_eq!(
            refused(trailer + at::SEAL + 3, 1),
            corrupt("trailer: a seal in the clear naming a key")
        );
        assert_eq!(refused(trailer + at::MAC, 1), corrupt("trailer: MACs, in the clear"));
        assert_eq!(
            refused(trailer + at::FOOTER_MAC + 31, 1),
            corrupt("trailer: MACs, in the clear")
        );
        let frame = Frame::of(&bytes).unwrap();
        assert_eq!(refused(entry(&frame, 0) + 20, 1), corrupt("part 0's tag, in the clear"));
    }

    /// Objects cut short, or with any byte of them changed, fail to read, without panicking:
    /// cladefs: in the clear and sealed (TASK-411.2).
    #[test]
    fn corrupt_objects_fail() {
        let data = [pattern(PART + 100), random(PART / 2)].concat();
        let keys = holding(dek());
        for seal in [None, Some((&dek(), ID))] {
            let bytes = framed(&data, b"footer", seal);
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
            opening: Opening::Clear,
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
            let bytes = framed(&data, b"footer", None);
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
            let packer =
                Packer { stored, tags, data_len: PART + 8192, end: 4096 + 8192, key: None };
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
}
