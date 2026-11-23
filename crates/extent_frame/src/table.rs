// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tables of where a cell's extents are, which its checkpoints are, locally and in object
//! storage (doc-7, Checkpoints): entries sorted by extent ID, in blocks of [`TABLE_BLOCK`] bytes,
//! each with a CRC32C; after them, cladefs: an appendix, what its writer keeps that only a reader of
//! it whole reads ([`Appendix`]); then a footer of the rest of the state, the keys of the objects
//! the entries name, and an index of the blocks; framed as objects are ([`crate::framed`]). A
//! reader after every extent reads the table whole; one after a single extent reads the footer,
//! and one block. cladefs: under its cell's DEK, if it has one, authenticated, not encrypted
//! ([`crate::Encryption::Authenticated`], TASK-411.3): metadata, which the collector reads holding
//! no key (doc-16, What is encrypted).

use crate::{Corrupt, Error, KeyId, Keyed, Keys, Secret, crc};
use serde::{Deserialize, Serialize};
use storage_device::ExtentId;

/// The most bytes of entries a block holds.
pub const TABLE_BLOCK: usize = 64 * 1024;

/// An entry: the extent's cell and sequence, the index of its object's key, its offset and
/// length there.
const ENTRY: usize = 8 + 8 + 4 + 8 + 8;

/// cladefs: the longest key a table's footer names (board 8545): an object's, which S3 and GCS
/// take of 1,024 bytes at most. A footer, from a bucket, is untrusted until its MAC is checked,
/// which the keyless collector checks none of: a key longer, which front-coding would let it grow
/// by what each shares with the one before, is refused.
pub const MAX_KEY: usize = 1024;

/// Where an extent is: in the object at `key`, `len` bytes at `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: ExtentId,
    pub key: String,
    pub offset: u64,
    pub len: u64,
}

/// What follows a table's blocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableFooter {
    /// The rest of the state, as its writer encoded it.
    pub rest: Vec<u8>,
    /// The keys of the objects the entries name: stored each as what it shares with the one before
    /// it and the rest ([`front_coded`]), as a cell's keys share their cell's prefix and their
    /// kind's, and most their cell's (TASK-364).
    #[serde(with = "front_coded")]
    pub keys: Vec<String>,
    pub blocks: Vec<Block>,
    /// cladefs: where its appendix is.
    pub appendix: Appendix,
}

/// cladefs: where in a table's data its appendix is, after its blocks, and its CRC32C: what its
/// writer keeps that a reader of the table whole reads, and one after a single extent never does,
/// so that the footer that one reads stays as small as the entries' index (doc-27: a checkpoint's
/// lengths of its segments' data).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appendix {
    pub offset: u64,
    pub len: u64,
    pub crc: u32,
}

/// A table read whole ([`decode`]): its footer, its entries and its appendix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub footer: TableFooter,
    pub entries: Vec<Entry>,
    pub appendix: Vec<u8>,
}

/// Where a block is, the first extent it holds, and its CRC32C.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub first: ExtentId,
    pub offset: u64,
    pub len: u32,
    pub crc: u32,
}

/// The table of `entries`, which are in their IDs' order ([`ExtentId`]'s: by sequence number, then
/// cell, the order [`TableFooter::block`] finds them by), `rest`, and cladefs: `appendix`
/// ([`Appendix`]); authenticated under the DEK `key` gives, and which it is
/// ([`crate::Packer::keyed`], TASK-411.3).
pub fn encode<'a>(
    entries: impl IntoIterator<Item = (ExtentId, &'a str, u64, u64)>,
    rest: Vec<u8>,
    appendix: &[u8],
    (dek, id): (&Secret, KeyId),
) -> Vec<u8> {
    let mut keys = Vec::new();
    let mut indexes = std::collections::HashMap::new();
    let mut data = Vec::new();
    let mut blocks = Vec::new();
    let mut block = Vec::with_capacity(TABLE_BLOCK);
    let mut first = None;
    let mut flush = |block: &mut Vec<u8>, first: &mut Option<ExtentId>, data: &mut Vec<u8>| {
        if let Some(first) = first.take() {
            let len = block.len() as u32;
            blocks.push(Block { first, offset: data.len() as u64, len, crc: crc(block) });
            data.append(block);
        }
    };
    for (id, key, offset, len) in entries {
        if block.len() + ENTRY > TABLE_BLOCK {
            flush(&mut block, &mut first, &mut data);
        }
        first.get_or_insert(id);
        let key = *indexes.entry(key).or_insert_with(|| {
            keys.push(key.to_owned());
            keys.len() as u32 - 1
        });
        block.extend_from_slice(&id.cell.to_le_bytes());
        block.extend_from_slice(&id.sequence.to_le_bytes());
        block.extend_from_slice(&key.to_le_bytes());
        block.extend_from_slice(&offset.to_le_bytes());
        block.extend_from_slice(&len.to_le_bytes());
    }
    flush(&mut block, &mut first, &mut data);
    let at = data.len() as u64;
    data.extend_from_slice(appendix);
    let appendix = Appendix { offset: at, len: appendix.len() as u64, crc: crc(appendix) };
    let footer = TableFooter { rest, keys, blocks, appendix };
    let footer = bincode::serialize(&footer).expect("footers serialize");
    crate::framed_keyed(&data, &footer, (dek, id, Keyed::Authenticated))
}

/// cladefs: keys stored front-coded: each as the length of what it shares with the one before it, and
/// the rest of it.
mod front_coded {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(keys: &[String], serializer: S) -> Result<S::Ok, S::Error> {
        let mut before = "";
        let coded: Vec<(u32, &str)> = keys
            .iter()
            .map(|key| {
                let shared = shared(before, key);
                before = key;
                (shared as u32, &key[shared..])
            })
            .collect();
        coded.serialize(serializer)
    }

    /// cladefs: each key at most [`super::MAX_KEY`] long (board 8545), so that the keys decoded
    /// are no more than that many bytes for each of the 12 or more each takes coded.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<String>, D::Error> {
        use serde::de::Error as _;
        let coded = Vec::<(u32, String)>::deserialize(deserializer)?;
        let mut keys: Vec<String> = Vec::with_capacity(coded.len());
        for (shared, rest) in coded {
            let before = keys.last().map_or("", String::as_str);
            // Past what this host addresses: more than the key before has, refused below.
            let shared = usize::try_from(shared).ok().and_then(|shared| before.get(..shared));
            let Some(shared) = shared else {
                return Err(D::Error::custom("a key sharing more than the one before it"));
            };
            if shared.len().checked_add(rest.len()).is_none_or(|len| len > super::MAX_KEY) {
                return Err(D::Error::custom(format!("a key past {} bytes", super::MAX_KEY)));
            }
            keys.push(format!("{shared}{rest}"));
        }
        Ok(keys)
    }

    /// The length of what `one` and `other` start with alike, on a character's boundary.
    fn shared(one: &str, other: &str) -> usize {
        let alike = one.bytes().zip(other.bytes()).take_while(|(a, b)| a == b).count();
        (0..=alike).rev().find(|&at| other.is_char_boundary(at)).unwrap_or(0)
    }
}

fn corrupt(what: &str) -> Corrupt {
    Corrupt(format!("a table's {what}"))
}

impl TableFooter {
    /// The footer whose bytes are `bytes`. cladefs: refused unless its blocks are as its encoder
    /// lays them (board 8545): one after another from the data's start, none overlapping another,
    /// none past [`TABLE_BLOCK`], their first extents ascending, its appendix right after them; so
    /// that a table decodes each byte of its data at most once, and a forged footer can't have
    /// many blocks name one, each entry of it decoded, its key cloned, as many times.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn decode(bytes: &[u8]) -> Result<Self, Corrupt> {
        let footer: Self =
            bincode::deserialize(bytes).map_err(|err| corrupt(&format!("footer: {err}")))?;
        let mut end = 0u64;
        let mut first = None;
        for block in &footer.blocks {
            // Past what this host addresses: not sized, refused below.
            let len = usize::try_from(block.len).ok();
            let sized = len.is_some_and(|len| len > 0 && len <= TABLE_BLOCK);
            if block.offset != end || !sized || first.is_some_and(|first| first >= block.first) {
                return Err(corrupt("blocks, not one after another"));
            }
            end = end.checked_add(u64::from(block.len)).ok_or_else(|| corrupt("blocks"))?;
            first = Some(block.first);
        }
        if footer.appendix.offset != end {
            return Err(corrupt("appendix, not after its blocks"));
        }
        Ok(footer)
    }

    /// The block that holds `id`, if any does.
    pub fn block(&self, id: ExtentId) -> Option<&Block> {
        let at = self.blocks.partition_point(|block| block.first <= id).checked_sub(1)?;
        self.blocks.get(at)
    }

    /// The entries of the block `index` describes, whose bytes are `bytes`, checked against its
    /// CRC.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn entries(&self, bytes: &[u8], index: &Block) -> Result<Vec<Entry>, Corrupt> {
        // Past what this host addresses: not the block's length, refused.
        if usize::try_from(index.len).ok() != Some(bytes.len())
            || !bytes.len().is_multiple_of(ENTRY)
            || crc(bytes) != index.crc
        {
            return Err(corrupt("block"));
        }
        let entry = |entry: &[u8; ENTRY]| {
            let (cell, rest) = entry.split_first_chunk::<8>().ok_or_else(|| corrupt("entry"))?;
            let (sequence, rest) = rest.split_first_chunk::<8>().ok_or_else(|| corrupt("entry"))?;
            let (key, rest) = rest.split_first_chunk::<4>().ok_or_else(|| corrupt("entry"))?;
            let (offset, rest) = rest.split_first_chunk::<8>().ok_or_else(|| corrupt("entry"))?;
            let (len, _) = rest.split_first_chunk::<8>().ok_or_else(|| corrupt("entry"))?;
            // Past what this host addresses: no key of its, refused below.
            let key = usize::try_from(u32::from_le_bytes(*key)).ok();
            let key = key.and_then(|key| self.keys.get(key)).ok_or_else(|| corrupt("key"))?;
            Ok(Entry {
                id: ExtentId::new(u64::from_le_bytes(*cell), u64::from_le_bytes(*sequence)),
                key: key.clone(),
                offset: u64::from_le_bytes(*offset),
                len: u64::from_le_bytes(*len),
            })
        };
        bytes.as_chunks::<ENTRY>().0.iter().map(entry).collect()
    }
}

/// The table whose bytes are `bytes` whole: its footer, its entries and its appendix, read by a
/// reader holding `keys`.
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub fn decode(bytes: &[u8], keys: &dyn Keys) -> Result<Decoded, Error> {
    let footer = TableFooter::decode(crate::footer(bytes, keys)?)?;
    let data = crate::data(bytes, keys)?;
    let mut entries = Vec::new();
    for index in &footer.blocks {
        // cladefs: checked, a forged offset refused (board 7011), as the appendix's is below.
        let range = usize::try_from(index.offset).ok().zip(usize::try_from(index.len).ok());
        let range = range.and_then(|(offset, len)| Some(offset..offset.checked_add(len)?));
        let block =
            range.and_then(|range| data.get(range)).ok_or_else(|| corrupt("block's place"))?;
        entries.extend(footer.entries(block, index)?);
    }
    let Appendix { offset, len, crc: expected } = footer.appendix;
    // Past what this host addresses: no range, refused below as corrupt.
    let range = usize::try_from(offset).ok().zip(usize::try_from(len).ok());
    let range = range.and_then(|(offset, len)| Some(offset..offset.checked_add(len)?));
    let appendix = range.and_then(|range| data.get(range)).ok_or_else(|| corrupt("appendix"))?;
    if crc(appendix) != expected {
        return Err(corrupt("appendix").into());
    }
    let appendix = appendix.to_vec();
    Ok(Decoded { footer, entries, appendix })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(count: u64) -> Vec<Entry> {
        (0..count)
            .map(|sequence| Entry {
                id: ExtentId::new(3, sequence),
                key: format!("segments/{}", sequence / 1000),
                offset: sequence * 4096,
                len: 4096,
            })
            .collect()
    }

    /// The DEK of the tests' tables, and which it is.
    const ID: KeyId = KeyId { cell: 3, generation: 0 };

    fn dek() -> Secret {
        Secret::from_bytes(&[1; 32])
    }

    fn encoded(entries: &[Entry]) -> Vec<u8> {
        encrypted(entries, (&dek(), ID))
    }

    /// The table of `entries`, authenticated under `key`.
    fn encrypted(entries: &[Entry], key: (&Secret, KeyId)) -> Vec<u8> {
        let rows =
            entries.iter().map(|entry| (entry.id, entry.key.as_str(), entry.offset, entry.len));
        encode(rows, b"rest".to_vec(), b"appendix", key)
    }

    /// A reader holding `keys`, which holds its objects encrypted: a reader of a cell with a DEK.
    struct Holding<'a>(&'a std::collections::HashMap<KeyId, Secret>);

    impl Keys for Holding<'_> {
        fn dek(&self, id: KeyId) -> Option<&Secret> {
            self.0.get(&id)
        }

        fn encrypted(&self) -> bool {
            true
        }
    }

    /// Tables hold their entries in blocks as full as `BLOCK` allows: 1,820 entries each; cladefs:
    /// authenticated under a key, read alike by readers holding it or none, and refused, changed,
    /// by one holding it; a reader holding its objects encrypted reads it under the key, and refuses
    /// it without (TASK-411.3).
    #[test]
    fn tables_hold_their_entries() {
        let id = ID;
        let keys = std::collections::HashMap::from([(id, dek())]);
        let key = (&keys[&id], id);
        for (count, blocks) in [(0, 0), (1, 1), (1819, 1), (1820, 1), (1821, 2), (10_000, 6)] {
            let entries = entries(count);
            let bytes = encrypted(&entries, key);
            let table = decode(&bytes, &keys).unwrap();
            let footer = table.footer.clone();
            let read = (footer.rest.as_slice(), &table.entries, table.appendix.as_slice());
            assert_eq!(read, (&b"rest"[..], &entries, &b"appendix"[..]), "{count}");
            assert_eq!(decode(&bytes, &crate::NoKeys), Ok(table));
            let firsts: Vec<ExtentId> = footer.blocks.iter().map(|block| block.first).collect();
            let expected: Vec<ExtentId> =
                (0..blocks).map(|block| ExtentId::new(3, block * 1820)).collect();
            assert_eq!(firsts, expected, "{count}");
        }
        let mut bytes = encrypted(&entries(3000), key);
        bytes[100] ^= 1;
        assert!(decode(&bytes, &keys).is_err());
        // Authenticated: its entries unencrypted, as stored; a part's tag changed, its table's CRC
        // made to match, read by a reader holding no key, refused by one holding it.
        let bytes = encrypted(&entries(10), key);
        let frame = crate::Frame::of(&bytes).unwrap();
        assert!(
            matches!(frame.encryption, crate::Encryption::Authenticated { key, .. } if key == id)
        );
        let mut tampered = bytes.clone();
        tampered[(frame.table().start + 17) as usize] ^= 1;
        let trailer = tampered.len() - crate::TRAILER as usize;
        let table_crc = crc(&tampered[frame.footer().end as usize..trailer + crate::at::TABLE_CRC]);
        tampered[trailer + crate::at::TABLE_CRC..][..4].copy_from_slice(&table_crc.to_le_bytes());
        assert!(decode(&tampered, &crate::NoKeys).is_ok());
        assert_eq!(
            decode(&tampered, &keys).map(|_| ()),
            Err(Corrupt("an object's table, by its MAC".into()).into())
        );
        // A reader holding its objects encrypted reads it under the key, and refuses it without.
        assert_eq!(decode(&bytes, &Holding(&keys)).map(|table| table.entries), Ok(entries(10)));
        let none = std::collections::HashMap::new();
        assert_eq!(decode(&bytes, &Holding(&none)).map(|_| ()), Err(Error::NoKey(id)));
    }

    /// cladefs: a footer's keys read back as they were, whatever they share, a character split or
    /// not; stored each as the rest of what the one before it has, they cost little more than what
    /// sets them apart (TASK-364). A footer naming a key sharing more than the one before it has
    /// is refused.
    #[test]
    fn footers_keys_are_stored_front_coded() {
        let prefix = "tenants/0000000000001/cells/0000000000002/segments/0000000000003/";
        let keys = (0..1000).map(|n| format!("{prefix}{}", base32_id::Padded(n)));
        let mut keys: Vec<String> = keys.collect();
        keys.extend(["".into(), "é1".into(), "é2".into(), "e".into(), "ée".into(), prefix.into()]);
        let footer = TableFooter {
            rest: b"rest".to_vec(),
            keys: keys.clone(),
            blocks: Vec::new(),
            appendix: Appendix::default(),
        };
        let bytes = bincode::serialize(&footer).unwrap();
        assert_eq!(TableFooter::decode(&bytes), Ok(footer));
        let plain: usize = keys.iter().map(String::len).sum();
        assert!(bytes.len() < plain / 4, "{} bytes of {plain}", bytes.len());
        let lying = (Vec::<u8>::new(), vec![(0u32, "ab".to_owned()), (3, "c".to_owned())]);
        let lying = (lying.0, lying.1, Vec::<Block>::new(), Appendix::default());
        let lying = bincode::serialize(&lying).unwrap();
        let refused = "a table's footer: a key sharing more than the one before it";
        assert_eq!(TableFooter::decode(&lying), Err(Corrupt(refused.into())));
    }

    /// One entry is found by the footer and its block alone.
    #[test]
    fn entries_are_found_by_their_blocks() {
        let entries = entries(100_000);
        let bytes = encoded(&entries);
        let footer = TableFooter::decode(crate::footer(&bytes, &crate::NoKeys).unwrap()).unwrap();
        let data = crate::data(&bytes, &crate::NoKeys).unwrap();
        for sequence in [0, 1, 50_000, 99_999] {
            let block = footer.block(ExtentId::new(3, sequence)).unwrap();
            let start = block.offset as usize;
            let found = footer.entries(&data[start..start + block.len as usize], block).unwrap();
            assert!(found.contains(&entries[sequence as usize]));
            assert!(block.len as usize <= TABLE_BLOCK);
        }
        assert_eq!(footer.block(ExtentId::new(2, 0)), None);
    }

    /// A block is read only if it is as long as its index says, whole entries, and of its CRC:
    /// each alone is found.
    #[test]
    fn blocks_unlike_their_index_fail() {
        let bytes = encoded(&entries(2));
        let footer = TableFooter::decode(crate::footer(&bytes, &crate::NoKeys).unwrap()).unwrap();
        let data = crate::data(&bytes, &crate::NoKeys).unwrap();
        let index = &footer.blocks[0];
        // cladefs: the appendix after it (`Appendix`).
        let data = &data[..index.len as usize];
        assert_eq!(footer.entries(data, index).unwrap(), entries(2));
        let block = Err(Corrupt("a table's block".into()));
        let longer = Block { len: index.len + ENTRY as u32, ..index.clone() };
        assert_eq!(footer.entries(data, &longer), block);
        let part = &data[..ENTRY + 1];
        let partial = Block { len: part.len() as u32, crc: crc(part), ..index.clone() };
        assert_eq!(footer.entries(part, &partial), block);
        let other = Block { crc: index.crc ^ 1, ..index.clone() };
        assert_eq!(footer.entries(data, &other), block);
    }

    /// board 8545: a footer's keys, forged, are refused past [`MAX_KEY`] bytes each, whatever they
    /// share: one key of 256 KiB, then 20,000 sharing all of it, which decoded would take 5 GB
    /// from 500 KB, is refused before any of the 20,000; a key of exactly the most is read, one
    /// past it not, nor one sharing `u32::MAX` bytes.
    #[test]
    fn footers_keys_past_the_longest_are_refused() {
        let footer = |coded: Vec<(u32, String)>| {
            let footer = (Vec::<u8>::new(), coded, Vec::<Block>::new(), Appendix::default());
            TableFooter::decode(&bincode::serialize(&footer).unwrap())
        };
        let long = "k".repeat(256 << 10);
        let mut amplifying = vec![(0, long)];
        amplifying.extend((0..20_000).map(|_| (256 << 10, String::new())));
        let past = format!("a table's footer: a key past {MAX_KEY} bytes");
        assert_eq!(footer(amplifying), Err(Corrupt(past.clone())));
        let most = "k".repeat(MAX_KEY);
        let read = footer(vec![(0, most.clone()), (MAX_KEY as u32, String::new())]);
        assert_eq!(read.map(|footer| footer.keys), Ok(vec![most.clone(), most.clone()]));
        assert_eq!(
            footer(vec![(0, most.clone()), (MAX_KEY as u32, "k".into())]),
            Err(Corrupt(past))
        );
        let sharing = "a table's footer: a key sharing more than the one before it";
        assert_eq!(
            footer(vec![(0, most), (u32::MAX, String::new())]),
            Err(Corrupt(sharing.into()))
        );
    }

    /// board 8545, board 7011: a footer's blocks, forged, are refused unless they lie as its
    /// encoder lays them, one after another from the data's start, their first extents ascending,
    /// the appendix right after: many naming one block, which decoded each entry of it, its key
    /// cloned, as many times; one out of order; one past the data, or at an offset whose range
    /// overflows; one empty or longer than a block; its appendix elsewhere.
    #[test]
    fn footers_whose_blocks_overlap_are_refused() {
        let bytes = encoded(&entries(3000));
        let footer = TableFooter::decode(crate::footer(&bytes, &crate::NoKeys).unwrap()).unwrap();
        assert_eq!(footer.blocks.len(), 2);
        let (one, two) = (footer.blocks[0].clone(), footer.blocks[1].clone());
        let forged = |blocks: Vec<Block>, appendix: u64| {
            let forged = TableFooter {
                blocks,
                appendix: Appendix { offset: appendix, ..footer.appendix.clone() },
                ..footer.clone()
            };
            TableFooter::decode(&bincode::serialize(&forged).unwrap())
        };
        let end = u64::from(one.len) + u64::from(two.len);
        assert_eq!(forged(vec![one.clone(), two.clone()], end), Ok(footer.clone()));
        let blocks = Err(Corrupt("a table's blocks, not one after another".into()));
        let many = vec![one.clone(); 20_000];
        assert_eq!(forged(many, u64::from(one.len)), blocks);
        assert_eq!(forged(vec![two.clone(), one.clone()], end), blocks);
        let after = Block { first: one.first, ..two.clone() };
        assert_eq!(forged(vec![one.clone(), after], end), blocks);
        for offset in [1, u64::from(one.len) - 1, u64::MAX - 1, u64::MAX] {
            let moved = Block { offset, ..two.clone() };
            assert_eq!(forged(vec![one.clone(), moved], end), blocks, "{offset}");
        }
        for len in [0, TABLE_BLOCK as u32 + 1, u32::MAX] {
            let sized = Block { len, ..one.clone() };
            assert_eq!(forged(vec![sized], u64::from(len)), blocks, "{len}");
        }
        let appendix = Err(Corrupt("a table's appendix, not after its blocks".into()));
        for offset in [0, end - 1, end + 1, u64::MAX] {
            assert_eq!(forged(vec![one.clone(), two.clone()], offset), appendix, "{offset}");
        }
    }
}
