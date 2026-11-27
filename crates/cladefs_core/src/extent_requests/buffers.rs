// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A request's buffers: the memory its descriptors name, in their order, as this process maps it:
//! a guest's, which the VMM's adapter walks a descriptor chain of; or what a local process shares
//! with the extent daemon, a ring's buffers (board 14378). Each descriptor's memory is given to
//! [`Buffers`], readable or writable as the descriptor says; nothing here knows of any queue, nor
//! whose memory it is.

use crate::Errno;
use zerocopy::{FromBytes, IntoBytes};

use super::protocol::RequestHeader;

/// A part of a request's buffers in host memory: guest memory a descriptor names.
#[derive(Clone, Copy, Debug)]
struct Segment {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: Guest memory, mapped while the device is active, and so while its requests are served;
// each request's buffers are its future's alone (`Buffers::push_readable`).
unsafe impl Send for Segment {}
// SAFETY: As for `Send`: the memory is the request's alone, which only its future reads or writes.
unsafe impl Sync for Segment {}

/// Why a descriptor chain is no request's buffers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuffersError {
    /// cladefs TASK-79.25: more bytes to write than the used ring's length holds, a u32, as a chain
    /// of descriptors naming the same memory again may give: the device writes them whole, and
    /// says how much it wrote there.
    #[error(
        "a descriptor chain gives the device {0} bytes to write, more than its used length holds"
    )]
    TooLong(u64),
}

/// A request's buffers: those the driver wrote, and those it gave the device to write, whose last
/// bytes are the response.
#[derive(Debug, Default)]
pub struct Buffers {
    readable: Vec<Segment>,
    writable: Vec<Segment>,
    /// The writable segments' bytes.
    writable_len: u64,
}

impl Buffers {
    /// Buffers with no segments yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the `len` bytes at `ptr`, a descriptor's that the driver wrote, after those added
    /// before.
    ///
    /// # Safety
    ///
    /// The `len` bytes at `ptr` are host memory mapping the guest's, readable until the request
    /// is served ([`handle`](super::handle)'s future completes or is dropped). The guest may
    /// change them meanwhile: they are only copied from.
    pub unsafe fn push_readable(&mut self, ptr: *mut u8, len: usize) {
        self.readable.push(Segment { ptr, len });
    }

    /// Adds the `len` bytes at `ptr`, a descriptor's the driver gave the device to write, after
    /// those added before. The device writes every writable byte, as drivers read them whole: the
    /// VMM marks them dirty, as it tracks guest memory's. Refused if the writable bytes come to
    /// more than a u32, the used ring's length, holds.
    ///
    /// # Safety
    ///
    /// The `len` bytes at `ptr` are host memory mapping the guest's, writable until the request
    /// is served ([`handle`](super::handle)'s future completes or is dropped), which the host
    /// accesses nowhere else meanwhile. The guest may access them meanwhile: they are only copied
    /// to.
    pub unsafe fn push_writable(&mut self, ptr: *mut u8, len: usize) -> Result<(), BuffersError> {
        self.writable_len += len as u64;
        if self.writable_len > u64::from(u32::MAX) {
            return Err(BuffersError::TooLong(self.writable_len));
        }
        self.writable.push(Segment { ptr, len });
        Ok(())
    }

    /// The request's header, its first bytes, if the driver wrote that many.
    pub fn header(&self) -> Option<RequestHeader> {
        let mut reader = Reader::new(self);
        // Fewer bytes than a header: none, as the doc says.
        reader.obj().ok()
    }

    /// The bytes the device writes: every writable one.
    pub fn writable_len(&self) -> usize {
        // At most a u32's (`push_writable`).
        self.writable_len as usize
    }
}

/// The `len` bytes at `offset` in `segments`, as parts of the segments, if they are there.
fn range(segments: &[Segment], mut offset: usize, mut len: usize) -> Option<Vec<Segment>> {
    let mut parts = Vec::new();
    for segment in segments {
        if len == 0 {
            break;
        }
        if offset >= segment.len {
            offset -= segment.len;
            continue;
        }
        let part = (segment.len - offset).min(len);
        // SAFETY: Within the segment.
        let ptr = unsafe { segment.ptr.add(offset) };
        parts.push(Segment { ptr, len: part });
        offset = 0;
        len -= part;
    }
    (len == 0).then_some(parts)
}

/// Reads what the driver wrote, in order.
pub struct Reader<'a> {
    segments: &'a [Segment],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buffers: &'a Buffers) -> Self {
        Self { segments: &buffers.readable, offset: 0 }
    }

    /// The next `len` bytes, as the cell copies them out of guest memory: as long as the request
    /// lives, so that what comes after them can be read too.
    pub fn slices(&mut self, len: usize) -> Result<Vec<crate::VolatileSlice<'a>>, Errno> {
        let parts = range(self.segments, self.offset, len).ok_or(Errno::EINVAL)?;
        self.offset += len;
        Ok(parts
            .iter()
            // SAFETY: Guest memory the request names, which outlives it, and which the guest may
            // change as it is copied (`Buffers::push_readable`).
            .map(|part| unsafe { crate::VolatileSlice::from_raw_parts(part.ptr, part.len) })
            .collect())
    }

    /// The next `len` bytes, copied.
    pub fn bytes(&mut self, len: usize) -> Result<Vec<u8>, Errno> {
        let mut bytes = vec![0; len];
        let mut at = 0;
        for slice in self.slices(len)? {
            at += slice.copy_to(&mut bytes[at..]);
        }
        Ok(bytes)
    }

    /// How many bytes are left to read.
    pub fn available(&self) -> usize {
        let len: usize = self.segments.iter().map(|segment| segment.len).sum();
        len.saturating_sub(self.offset)
    }

    /// The next object, copied.
    pub fn obj<T: FromBytes>(&mut self) -> Result<T, Errno> {
        let bytes = self.bytes(size_of::<T>())?;
        Ok(T::read_from_bytes(&bytes).expect("as many bytes as the object has"))
    }
}

/// Writes what the device returns into the writable buffers, from their start.
pub struct Writer<'a> {
    segments: &'a [Segment],
}

impl Writer<'_> {
    pub fn new(buffers: &Buffers) -> Writer<'_> {
        Writer { segments: &buffers.writable }
    }

    /// `len` bytes at `offset`, as the cell copies into guest memory.
    pub fn slices(&self, offset: usize, len: usize) -> Option<Vec<crate::VolatileSliceMut<'_>>> {
        Some(
            range(self.segments, offset, len)?
                .iter()
                // SAFETY: Guest memory the request gives the device to write, which outlives it,
                // and which the guest may access as it is copied (`Buffers::push_writable`).
                .map(|part| unsafe { crate::VolatileSliceMut::from_raw_parts(part.ptr, part.len) })
                .collect(),
        )
    }

    /// Writes `bytes` at `offset`, which must be within the buffers.
    pub fn write(&self, offset: usize, bytes: &[u8]) {
        let mut at = 0;
        for mut slice in
            self.slices(offset, bytes.len()).expect("a write past the request's buffers")
        {
            at += slice.copy_from(&bytes[at..]);
        }
    }

    /// Writes `obj` at `offset`, which must be within the buffers.
    pub fn obj<T: IntoBytes + zerocopy::Immutable>(&self, offset: usize, obj: &T) {
        self.write(offset, obj.as_bytes());
    }

    /// Zeroes `len` bytes at `offset`, which must be within the buffers.
    pub fn zero(&self, offset: usize, len: usize) {
        for part in range(self.segments, offset, len).expect("zeroing past the request's buffers") {
            // SAFETY: Guest memory the request gives the device to write.
            unsafe { std::ptr::write_bytes(part.ptr, 0, part.len) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cladefs TASK-79.25: a chain whose descriptors give the device more bytes to write than a
    /// u32 holds, naming the same memory again and again, is refused, not served: its used
    /// length couldn't say what the device wrote, which aborted the VMM.
    #[test]
    fn chains_writing_past_u32_are_refused() {
        const LEN: usize = 0x6000_0000;
        let mut memory = vec![0u8; 4096];
        let mut buffers = Buffers::new();
        for _ in 0..2 {
            // SAFETY: Never accessed: refused before the request is served. Its length is the
            // chain's, naming the memory again and again.
            unsafe { buffers.push_writable(memory.as_mut_ptr(), LEN) }.unwrap();
        }
        // SAFETY: As above.
        let third = unsafe { buffers.push_writable(memory.as_mut_ptr(), LEN) };
        assert_eq!(third, Err(BuffersError::TooLong(0x1_2000_0000)));
    }

    /// What the driver wrote reads in order across its descriptors, whatever their sizes; and
    /// what the device writes lands across the writable ones, in order.
    #[test]
    fn requests_read_and_write_across_descriptors() {
        let mut first = *b"abc";
        let mut second = *b"defgh";
        let mut out = [[0u8; 2]; 3];
        let mut buffers = Buffers::new();
        // SAFETY: The test's memory, which outlives the buffers.
        unsafe {
            buffers.push_readable(first.as_mut_ptr(), first.len());
            buffers.push_readable(second.as_mut_ptr(), second.len());
            for part in &mut out {
                buffers.push_writable(part.as_mut_ptr(), part.len()).unwrap();
            }
        }
        assert_eq!(buffers.writable_len(), 6);
        let mut reader = Reader::new(&buffers);
        assert_eq!(reader.available(), 8);
        assert_eq!(reader.bytes(2).unwrap(), b"ab");
        assert_eq!(reader.bytes(4).unwrap(), b"cdef");
        assert_eq!(reader.bytes(3), Err(Errno::EINVAL));
        let writer = Writer::new(&buffers);
        writer.write(1, b"wxyz");
        writer.zero(5, 1);
        assert_eq!(out, [*b"\0w", *b"xy", *b"z\0"]);
    }
}
