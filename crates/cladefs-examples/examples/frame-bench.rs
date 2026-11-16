// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what framing an object costs in the clear and encrypted (TASK-411.2, backlog
//! doc-16): packing N MiB (256) of data into one object, part by part on one thread, as a store's
//! framing thread packs each, and unpacking it again, each part checked, opened if encrypted, and
//! decompressed. Say:
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example frame-bench -- [N]
//! ```
//!
//! For each of `text`, sixteen letters at random, which compresses about by half, as a build's
//! files do, and `noise`, which doesn't compress, it prints the MiB/s of packing and of unpacking,
//! in the clear and encrypted, the best of five rounds.

use extent_frame::{Keys, NoKeys, PART, Packer, Parts};
use extent_keys::{KeyId, Secret};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const ROUNDS: usize = 5;

/// `len` bytes of `kind`'s data, from a xorshift generator seeded alike every run.
fn data(kind: &str, len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    match kind {
        "text" => (0..len).map(|_| b'a' + (next() % 16) as u8).collect(),
        "noise" => (0..len.div_ceil(8)).flat_map(|_| next().to_le_bytes()).take(len).collect(),
        other => panic!("no data {other}"),
    }
}

/// `data` framed as one object, encrypted if `encryption` says: its bytes, and how long packing
/// took.
fn pack(data: &[u8], encryption: Option<(&Secret, KeyId)>) -> (Vec<u8>, Duration) {
    let began = Instant::now();
    let mut packer = Packer::new(encryption);
    let mut object = Vec::with_capacity(data.len() + (1 << 20));
    for part in data.chunks(PART as usize) {
        object.extend_from_slice(&packer.push(part));
    }
    object.extend_from_slice(&packer.finish(b"footer"));
    (object, began.elapsed())
}

/// How long unpacking every part of `object` took, as a reader holding `keys` unpacks it.
fn unpack(object: &[u8], keys: &dyn Keys, data: &[u8]) -> Duration {
    let began = Instant::now();
    let parts = Parts::of(object, keys).expect("the object's table");
    let mut stored = vec![0; PART as usize];
    let mut out = vec![0; PART as usize];
    for index in 0..parts.frame.parts() {
        let part = parts.part(index).expect("a part of the object");
        let range = part.stored.range();
        let stored = &mut stored[..range.end as usize - range.start as usize];
        stored.copy_from_slice(&object[range.start as usize..range.end as usize]);
        let out = &mut out[..part.len() as usize];
        part.unpack_into(stored, out).expect("the part, unpacked");
        assert_eq!(out, &data[part.place.start as usize..part.place.end as usize]);
    }
    began.elapsed()
}

fn rate(bytes: usize, took: Duration) -> f64 {
    bytes as f64 / (1 << 20) as f64 / took.as_secs_f64()
}

fn main() {
    let mib: usize = std::env::args().nth(1).map_or(256, |n| n.parse().expect("N, in MiB"));
    let id = KeyId { branch: 1, generation: 1 };
    let mut keys = HashMap::new();
    keys.insert(id, Secret::random());
    for kind in ["text", "noise"] {
        let data = data(kind, mib << 20);
        for (mode, encrypted) in [("clear", false), ("encrypted", true)] {
            let encryption = encrypted.then(|| (&keys[&id], id));
            let keys: &dyn Keys = if encrypted { &keys } else { &NoKeys };
            let (mut packing, mut unpacking) = (Duration::MAX, Duration::MAX);
            let mut stored = 0;
            for _ in 0..ROUNDS {
                let (object, took) = pack(&data, encryption);
                packing = packing.min(took);
                unpacking = unpacking.min(unpack(&object, keys, &data));
                stored = object.len();
            }
            println!(
                "{kind:5} {mode:9} pack {:7.1} MiB/s  unpack {:7.1} MiB/s  stored {:.2} of the data",
                rate(data.len(), packing),
                rate(data.len(), unpacking),
                stored as f64 / data.len() as f64,
            );
        }
    }
}
