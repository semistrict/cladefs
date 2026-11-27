// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The snapshot state's encoding: little-endian numbers, byte strings as a
//! `u32` length and the bytes, lists as a `u32` count and the items.
//!
//! The decoder checks every length against what is left, so a damaged state
//! fails with an error instead of a huge allocation or a panic.

use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::error::{Error, Result};

#[derive(Default)]
pub(crate) struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.count(b.len());
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// A list's item count.
    pub(crate) fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("snapshot lists hold fewer than 2^32 items"));
    }

    /// A plain-data structure as its bytes, with its length (so that a
    /// structure of another size is caught on decoding).
    pub(crate) fn raw<T: IntoBytes + Immutable + ?Sized>(&mut self, v: &T) {
        self.bytes(v.as_bytes());
    }
}

pub(crate) struct Decoder<'a> {
    buf: &'a [u8],
}

fn damaged(what: &str) -> Error {
    Error::Snapshot(format!("the snapshot's state is damaged: {what}"))
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(damaged("it ends early"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(damaged(&format!("{v} is not a boolean"))),
        }
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub(crate) fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| damaged("a string is not UTF-8"))
    }

    /// A list's item count, checked against what is left: each item takes
    /// at least `min_item` bytes.
    pub(crate) fn count(&mut self, min_item: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item.max(1)) > self.buf.len() {
            return Err(damaged("a list is longer than the state"));
        }
        Ok(n)
    }

    /// A plain-data structure [`Encoder::raw`] wrote.
    pub(crate) fn raw<T: FromBytes>(&mut self) -> Result<T> {
        let bytes = self.bytes()?;
        T::read_from_bytes(bytes).map_err(|_| {
            damaged(&format!(
                "{} bytes for a {}-byte {}",
                bytes.len(),
                size_of::<T>(),
                std::any::type_name::<T>()
            ))
        })
    }

    /// How many bytes are left.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// The state must end here.
    pub(crate) fn end(&self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(damaged(&format!("{} bytes after its end", self.buf.len())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_come_back_as_written() {
        let mut e = Encoder::default();
        e.u8(7);
        e.bool(true);
        e.u16(0xbeef);
        e.u32(0xdead_beef);
        e.u64(u64::MAX - 1);
        e.i64(-5);
        e.bytes(b"abc");
        e.str("snapshot");
        e.raw(&[1u32, 2, 3]);
        let bytes = e.finish();
        let mut d = Decoder::new(&bytes);
        assert_eq!(d.u8().unwrap(), 7);
        assert!(d.bool().unwrap());
        assert_eq!(d.u16().unwrap(), 0xbeef);
        assert_eq!(d.u32().unwrap(), 0xdead_beef);
        assert_eq!(d.u64().unwrap(), u64::MAX - 1);
        assert_eq!(d.i64().unwrap(), -5);
        assert_eq!(d.bytes().unwrap(), b"abc");
        assert_eq!(d.str().unwrap(), "snapshot");
        assert_eq!(d.raw::<[u32; 3]>().unwrap(), [1, 2, 3]);
        d.end().unwrap();
    }

    /// Every prefix of a state, and a state with bytes past its end, is
    /// refused with an error, never a panic.
    #[test]
    fn damaged_states_are_refused() {
        let mut e = Encoder::default();
        e.u64(1);
        e.bytes(&[9; 40]);
        e.raw(&[0u64; 4]);
        let bytes = e.finish();
        let read = |b: &[u8]| -> Result<()> {
            let mut d = Decoder::new(b);
            d.u64()?;
            d.bytes()?;
            d.raw::<[u64; 4]>()?;
            d.end()
        };
        read(&bytes).unwrap();
        for n in 0..bytes.len() {
            assert!(matches!(read(&bytes[..n]), Err(Error::Snapshot(_))), "{n}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(read(&longer).is_err());
        // A structure of another size.
        let mut d = Decoder::new(&bytes[8 + 4 + 40..]);
        assert!(d.raw::<[u64; 3]>().is_err());
        // A count that cannot fit.
        let mut e = Encoder::default();
        e.u32(u32::MAX);
        assert!(Decoder::new(&e.finish()).count(1).is_err());
        assert!(Decoder::new(&[2]).bool().is_err());
    }
}
