// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tables of where a branch's extents are, which its checkpoints are, locally and in object
//! storage (doc-7, Checkpoints): entries sorted by extent ID, in blocks of [`BLOCK`] bytes, each
//! with a CRC32C; after them, a footer of the rest of the state, the keys of the objects the
//! entries name, and an index of the blocks; framed as objects are ([`crate::framed`]). A reader
//! after every extent reads the table whole; one after a single extent reads the footer, and one
//! block. cladefs: under its branch's DEK, if it has one, authenticated, not sealed
//! ([`crate::Seal::Authenticated`], TASK-411.3): metadata, which the collector reads holding no
//! key (doc-16, What is encrypted).

use crate::{Corrupt, Error, KeyId, Keyed, Keys, Secret, crc};
use serde::{Deserialize, Serialize};

/// The most bytes of entries a block holds.
pub const BLOCK: usize = 64 * 1024;

/// An entry: the extent's branch and sequence, the index of its object's key, its offset and
/// length there.
const ENTRY: usize = 8 + 8 + 4 + 8 + 8;

/// An extent's ID: its branch and its sequence number.
pub type Id = (u64, u64);

/// Where an extent is: in the object at `key`, `len` bytes at `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: Id,
    pub key: String,
    pub offset: u64,
    pub len: u64,
}

/// What follows a table's blocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footer {
    /// The rest of the state, as its writer encoded it.
    pub rest: Vec<u8>,
    /// The keys of the objects the entries name: stored each as what it shares with the one before
    /// it and the rest ([`front_coded`]), as a branch's keys share their cell's prefix and their
    /// kind's, and most their branch's (TASK-364).
    #[serde(with = "front_coded")]
    pub keys: Vec<String>,
    pub blocks: Vec<Block>,
}

/// Where a block is, the first extent it holds, and its CRC32C.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub first: Id,
    pub offset: u64,
    pub len: u32,
    pub crc: u32,
}

/// The table of `entries`, which are in their IDs' order, and `rest`: cladefs: authenticated, if
/// `key` gives a DEK and which it is ([`crate::Packer::keyed`], TASK-411.3).
pub fn encode<'a>(
    entries: impl IntoIterator<Item = (Id, &'a str, u64, u64)>,
    rest: Vec<u8>,
    key: Option<(&Secret, KeyId)>,
) -> Vec<u8> {
    let mut keys = Vec::new();
    let mut indexes = std::collections::HashMap::new();
    let mut data = Vec::new();
    let mut blocks = Vec::new();
    let mut block = Vec::with_capacity(BLOCK);
    let mut first = None;
    let mut flush = |block: &mut Vec<u8>, first: &mut Option<Id>, data: &mut Vec<u8>| {
        if let Some(first) = first.take() {
            let len = block.len() as u32;
            blocks.push(Block { first, offset: data.len() as u64, len, crc: crc(block) });
            data.append(block);
        }
    };
    for ((branch, sequence), key, offset, len) in entries {
        if block.len() + ENTRY > BLOCK {
            flush(&mut block, &mut first, &mut data);
        }
        first.get_or_insert((branch, sequence));
        let key = *indexes.entry(key).or_insert_with(|| {
            keys.push(key.to_owned());
            keys.len() as u32 - 1
        });
        block.extend_from_slice(&branch.to_le_bytes());
        block.extend_from_slice(&sequence.to_le_bytes());
        block.extend_from_slice(&key.to_le_bytes());
        block.extend_from_slice(&offset.to_le_bytes());
        block.extend_from_slice(&len.to_le_bytes());
    }
    flush(&mut block, &mut first, &mut data);
    let footer = bincode::serialize(&Footer { rest, keys, blocks }).expect("footers serialize");
    crate::framed_keyed(&data, &footer, key.map(|(dek, id)| (dek, id, Keyed::Authenticated)))
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

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<String>, D::Error> {
        let coded = Vec::<(u32, String)>::deserialize(deserializer)?;
        let mut keys: Vec<String> = Vec::with_capacity(coded.len());
        for (shared, rest) in coded {
            let before = keys.last().map_or("", String::as_str);
            let Some(shared) = before.get(..shared as usize) else {
                return Err(serde::de::Error::custom("a key sharing more than the one before it"));
            };
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

impl Footer {
    /// The footer whose bytes are `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Self, Corrupt> {
        bincode::deserialize(bytes).map_err(|err| corrupt(&format!("footer: {err}")))
    }

    /// The block that holds `id`, if any does.
    pub fn block(&self, id: Id) -> Option<&Block> {
        let at = self.blocks.partition_point(|block| block.first <= id).checked_sub(1)?;
        Some(&self.blocks[at])
    }

    /// The entries of the block `index` describes, whose bytes are `bytes`, checked against its
    /// CRC.
    pub fn entries(&self, bytes: &[u8], index: &Block) -> Result<Vec<Entry>, Corrupt> {
        if bytes.len() != index.len as usize
            || !bytes.len().is_multiple_of(ENTRY)
            || crc(bytes) != index.crc
        {
            return Err(corrupt("block"));
        }
        let entry = |entry: &[u8; ENTRY]| {
            let u64_at = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
            let key = u32::from_le_bytes(entry[16..20].try_into().unwrap()) as usize;
            let key = self.keys.get(key).ok_or_else(|| corrupt("key"))?.clone();
            Ok(Entry { id: (u64_at(0), u64_at(8)), key, offset: u64_at(20), len: u64_at(28) })
        };
        bytes.as_chunks::<ENTRY>().0.iter().map(entry).collect()
    }
}

/// The footer of the table whose bytes are `bytes`, and its entries, read by a reader holding
/// `keys`.
pub fn decode(bytes: &[u8], keys: &dyn Keys) -> Result<(Footer, Vec<Entry>), Error> {
    let footer = Footer::decode(crate::footer(bytes, keys)?)?;
    let data = crate::data(bytes, keys)?;
    let mut entries = Vec::new();
    for index in &footer.blocks {
        let range = index.offset as usize..index.offset as usize + index.len as usize;
        let block = data.get(range).ok_or_else(|| corrupt("block's place"))?;
        entries.extend(footer.entries(block, index)?);
    }
    Ok((footer, entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(count: u64) -> Vec<Entry> {
        (0..count)
            .map(|sequence| Entry {
                id: (3, sequence),
                key: format!("segments/{}", sequence / 1000),
                offset: sequence * 4096,
                len: 4096,
            })
            .collect()
    }

    fn encoded(entries: &[Entry]) -> Vec<u8> {
        sealed(entries, None)
    }

    /// The table of `entries`, authenticated under `key` if given.
    fn sealed(entries: &[Entry], key: Option<(&Secret, KeyId)>) -> Vec<u8> {
        let rows =
            entries.iter().map(|entry| (entry.id, entry.key.as_str(), entry.offset, entry.len));
        encode(rows, b"rest".to_vec(), key)
    }

    /// A reader holding `keys`, which holds its objects sealed: a reader of a branch with a DEK.
    struct Holding<'a>(&'a std::collections::HashMap<KeyId, Secret>);

    impl Keys for Holding<'_> {
        fn dek(&self, id: KeyId) -> Option<&Secret> {
            self.0.get(&id)
        }

        fn sealed(&self) -> bool {
            true
        }
    }

    /// Tables hold their entries in blocks as full as `BLOCK` allows: 1,820 entries each; cladefs:
    /// in the clear, and under a key, authenticated, read alike by readers holding it or none, and
    /// refused, changed, by one holding it; one in the clear refused by a reader holding its
    /// objects sealed (TASK-411.3).
    #[test]
    fn tables_hold_their_entries() {
        let id = KeyId { branch: 3, generation: 0 };
        let keys = std::collections::HashMap::from([(id, Secret::from_bytes(&[1; 32]))]);
        for key in [None, Some((&keys[&id], id))] {
            for (count, blocks) in [(0, 0), (1, 1), (1819, 1), (1820, 1), (1821, 2), (10_000, 6)] {
                let entries = entries(count);
                let bytes = sealed(&entries, key);
                let (footer, decoded) = decode(&bytes, &keys).unwrap();
                assert_eq!((footer.rest.as_slice(), &decoded), (&b"rest"[..], &entries), "{count}");
                assert_eq!(decode(&bytes, &crate::NoKeys), Ok((footer.clone(), entries)));
                let firsts: Vec<Id> = footer.blocks.iter().map(|block| block.first).collect();
                let expected: Vec<Id> = (0..blocks).map(|block| (3, block * 1820)).collect();
                assert_eq!(firsts, expected, "{count}");
            }
            let mut bytes = sealed(&entries(3000), key);
            bytes[100] ^= 1;
            assert!(decode(&bytes, &keys).is_err());
        }
        // Authenticated: its entries in the clear, as stored; a part's tag changed, its table's
        // CRC made to match, read by a reader holding no key, refused by one holding it.
        let bytes = sealed(&entries(10), Some((&keys[&id], id)));
        let frame = crate::Frame::of(&bytes).unwrap();
        assert!(matches!(frame.seal, crate::Seal::Authenticated { key, .. } if key == id));
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
        // In the clear, refused by a reader holding its objects sealed.
        let clear = sealed(&entries(10), None);
        assert_eq!(decode(&clear, &Holding(&keys)).map(|_| ()), Err(Error::Clear));
        assert_eq!(decode(&bytes, &Holding(&keys)).map(|(_, entries)| entries), Ok(entries(10)));
    }

    /// cladefs: a footer's keys read back as they were, whatever they share, a character split or
    /// not; stored each as the rest of what the one before it has, they cost little more than what
    /// sets them apart (TASK-364). A footer naming a key sharing more than the one before it has
    /// is refused.
    #[test]
    fn footers_keys_are_stored_front_coded() {
        let prefix = "tenants/0000000000000001/cells/0000000000000002/segments/0000000000000003/";
        let mut keys: Vec<String> = (0..1000).map(|n| format!("{prefix}{n:016x}")).collect();
        keys.extend(["".into(), "é1".into(), "é2".into(), "e".into(), "ée".into(), prefix.into()]);
        let footer = Footer { rest: b"rest".to_vec(), keys: keys.clone(), blocks: Vec::new() };
        let bytes = bincode::serialize(&footer).unwrap();
        assert_eq!(Footer::decode(&bytes), Ok(footer));
        let plain: usize = keys.iter().map(String::len).sum();
        assert!(bytes.len() < plain / 4, "{} bytes of {plain}", bytes.len());
        let lying = (Vec::<u8>::new(), vec![(0u32, "ab".to_owned()), (3, "c".to_owned())]);
        let lying = bincode::serialize(&(lying.0, lying.1, Vec::<Block>::new())).unwrap();
        let refused = "a table's footer: a key sharing more than the one before it";
        assert_eq!(Footer::decode(&lying), Err(Corrupt(refused.into())));
    }

    /// One entry is found by the footer and its block alone.
    #[test]
    fn entries_are_found_by_their_blocks() {
        let entries = entries(100_000);
        let bytes = encoded(&entries);
        let footer = Footer::decode(crate::footer(&bytes, &crate::NoKeys).unwrap()).unwrap();
        let data = crate::data(&bytes, &crate::NoKeys).unwrap();
        for sequence in [0, 1, 50_000, 99_999] {
            let block = footer.block((3, sequence)).unwrap();
            let start = block.offset as usize;
            let found = footer.entries(&data[start..start + block.len as usize], block).unwrap();
            assert!(found.contains(&entries[sequence as usize]));
            assert!(block.len as usize <= BLOCK);
        }
        assert_eq!(footer.block((2, 5)), None);
    }

    /// A block is read only if it is as long as its index says, whole entries, and of its CRC:
    /// each alone is found.
    #[test]
    fn blocks_unlike_their_index_fail() {
        let bytes = encoded(&entries(2));
        let footer = Footer::decode(crate::footer(&bytes, &crate::NoKeys).unwrap()).unwrap();
        let data = crate::data(&bytes, &crate::NoKeys).unwrap();
        let index = &footer.blocks[0];
        assert_eq!(footer.entries(&data, index).unwrap(), entries(2));
        let block = Err(Corrupt("a table's block".into()));
        let longer = Block { len: index.len + ENTRY as u32, ..index.clone() };
        assert_eq!(footer.entries(&data, &longer), block);
        let part = &data[..ENTRY + 1];
        let partial = Block { len: part.len() as u32, crc: crc(part), ..index.clone() };
        assert_eq!(footer.entries(part, &partial), block);
        let other = Block { crc: index.crc ^ 1, ..index.clone() };
        assert_eq!(footer.entries(&data, &other), block);
    }
}
