// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! fxdax: a closed segment's parts, read from its raw file and looked at on the framer's thread,
//! then packed on threads, each part in a buffer kept for the framings after, as are the
//! compressors (TASK-336): what a framing does with its data before it writes it
//! (`segments::frame_segment`). And a look at each part as the segment fills, once it is written
//! whole ([`see`], TASK-400), which its framing reads no more if it frames the segment in place.
//!
//! A framing read the whole of its data, 32 MiB, into one buffer taken afresh, every page faulted
//! in, 8,192 faults, and its packing threads took compressors afresh, their tables faulted in too.
//! On a host whose CPUs its VMs' vCPUs keep busy, a thread gets its share of them and no more: so
//! the faults, and the copying and looking on the framer's thread, took most of a framing's time,
//! up to half a second each of 0.8 s beside 28 spinning threads on ubu1's 8 CPUs (store-bench
//! framing); and on ubu1's sandboxes run, its slow framings read for 2.5 s, compressed for 2.3 s
//! and wrote for 0.6 s of 5.3 s, 1.1 s of it waiting to run. Each part in a buffer kept
//! ([`Buffer`]), as are the packing threads' compressors, a framing faults nothing in. Reading the
//! parts on threads too took the VMs' CPUs from them: dead-ends/framing-reads-on-threads.md.
//!
//! A framing read its segment's data seconds after the guest wrote it, the segment closing: on a
//! host short of memory, the long mode's 16 GiB at its limit, the host had let its pages go, clean
//! once written back behind the guest's writes (TASK-221), and the framing read them back from a
//! disk eight VMs and their bucket kept busy, 0.7-5.7 s (TASK-400). Most segments are framed in
//! place, their parts raw, whose framing needs no more of their data than each part's CRC and a
//! look at it: those a look ahead took, as each part was written whole, the page cache holding it
//! still, the framing has and reads not.

use crate::dir::File;
use crate::timings::runnable_ns;
use extent_frame::{PART, PackedPart, PartPacker};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// fxdax: the most part buffers ([`Buffer`]) and compressors ([`PartPacker`]) kept for framings to
/// come: two segments' worth of parts, 64 MiB, and as many compressors. Taken
/// afresh, each framing's 32 MiB of buffers were faulted in page by page, 8,192 faults, and each
/// thread's compressor's tables too, every framing: as much of its reading's time as the copying
/// (store-bench framing, perf stat on ubu1). More framings at once than that take theirs afresh,
/// and keep them only while there is room.
const KEPT: usize = 2 * 32;

/// fxdax: what [`KEPT`] keeps, taken by framings and given back.
struct Kept<T> {
    kept: Mutex<Vec<T>>,
}

impl<T> Kept<T> {
    const fn new() -> Self {
        Self { kept: Mutex::new(Vec::new()) }
    }

    /// One kept, or `fresh`'s.
    fn take(&self, fresh: impl FnOnce() -> T) -> T {
        let kept = self.kept.lock().unwrap().pop();
        kept.unwrap_or_else(fresh)
    }

    /// `it`, kept if there is room.
    fn give(&self, it: T) {
        let mut kept = self.kept.lock().unwrap();
        if kept.len() < KEPT {
            kept.push(it);
        }
    }
}

static BUFFERS: Kept<Vec<u8>> = Kept::new();
static PACKERS: Kept<PartPacker> = Kept::new();

/// fxdax: the bytes of a part of a segment's data, read, in a buffer of a part's bytes kept for
/// the framings after it once it goes, if there is room ([`KEPT`]).
pub(crate) struct Buffer(Vec<u8>);

impl Buffer {
    /// One of `len` bytes, a part's at most.
    fn of(len: usize) -> Self {
        let mut bytes = BUFFERS.take(|| Vec::with_capacity(PART as usize));
        bytes.resize(len, 0);
        Self(bytes)
    }
}

impl std::ops::Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        BUFFERS.give(std::mem::take(&mut self.0));
    }
}

/// fxdax: what of a segment's data compressing its parts must save for it to be framed in a copy,
/// compressed, rather than in place, every part raw (`segments::frame_segment`, TASK-310): an
/// eighth. Writing the data again to save less of it, in the bucket and in the host's cache, costs
/// the host's disk more than it saves, on a disk VMs share with their bucket above all. Framed in
/// place only if none compressed, segments a guest's filesystem fills, a large file's data with its
/// metadata in a few of their parts, were all framed in copies but for a few.
pub(crate) const IN_PLACE_SAVING: u64 = 8;

/// fxdax: the bytes of each part of a segment's data its sample compresses ([`Look`]): an eighth,
/// its first, so that every part has its share sampled, spread over all of it (TASK-343).
pub(crate) const SAMPLE: usize = (PART / 8) as usize;

/// fxdax: the blocks of a segment's data [`Look`] finds zeros in, every one of them.
const ZERO_BLOCK: usize = 4096;

/// fxdax: a look at parts of a segment's data short of compressing them: their bytes, those of
/// their blocks of zeros, and of their samples, as they are and compressed (TASK-310).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Look {
    len: u64,
    zeros: u64,
    sampled: u64,
    packed: u64,
}

impl Look {
    /// A look at `part`, a part of a segment's data, its sample compressed by `packer`.
    pub fn at(part: &[u8], packer: &mut PartPacker) -> Self {
        let zeros = (part.chunks(ZERO_BLOCK))
            .filter(|block| block.iter().all(|&byte| byte == 0))
            .map(|block| block.len() as u64)
            .sum();
        let sample = &part[..part.len().min(SAMPLE)];
        let (sampled, packed) = (sample.len() as u64, packer.packed_len(sample));
        Self { len: part.len() as u64, zeros, sampled, packed }
    }

    /// The look at these parts and `other`'s.
    pub fn plus(self, other: Self) -> Self {
        Self {
            len: self.len + other.len,
            zeros: self.zeros + other.zeros,
            sampled: self.sampled + other.sampled,
            packed: self.packed + other.packed,
        }
    }

    /// Whether the parts looked at, a segment's data, save too little compressed for a framing in
    /// a copy: their blocks of zeros under half of what a copy must save ([`IN_PLACE_SAVING`]),
    /// and their samples compressing by under that too; what a segment of a large file's data,
    /// which doesn't compress, holds. One saving more, or near the bound, is compressed whole to
    /// tell (TASK-310).
    ///
    /// The zeros counted exactly, as the store writes them of its own: an extent's holes, sealed in
    /// one run with it, which no guest's write wrote, nor a tenant's disk admitted (TASK-94).
    /// Sampled a part in eight, 1 MiB each, the first part alone of a segment under 9 MiB, the
    /// sample took a segment of a hostile VM process's 3 MiB of noise, then 2.6 MB of its extents'
    /// holes, for one that saves nothing: framed in place, raw, the holes took its tenant's disk
    /// past its limit by what a copy, compressed, takes none of (TASK-343, Hunt run 42949975683).
    pub fn saves_little(&self) -> bool {
        if self.zeros >= self.len / (2 * IN_PLACE_SAVING) {
            return false;
        }
        self.sampled - self.packed < self.sampled / (2 * IN_PLACE_SAVING)
    }
}

/// fxdax: what a look at a part of a segment's data that may be framed in place saw of it: the CRC
/// of its bytes, as a raw part's, and a look at them (TASK-310).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    pub crc: u32,
    pub look: Look,
}

impl Seen {
    /// What a look at `part`, a part's bytes, sees, its sample compressed by `packer`.
    fn of(part: &[u8], packer: &mut PartPacker) -> Self {
        Self { crc: PackedPart::raw(part).crc, look: Look::at(part, packer) }
    }
}

/// fxdax: a look ahead at part `part` of a segment's data, whole, in its raw file `raw`, as the
/// segment fills ([`Seen`], TASK-400): read through the host's page cache, which holds it, just
/// written, still. Its bytes change no more: the writes to a segment write each of its bytes once,
/// and the part is whole once the last write to it ended.
pub(crate) fn see<F: File>(raw: &F, part: u64) -> io::Result<Seen> {
    let mut data = Buffer::of(PART as usize);
    raw.read_cached(&mut data, part * PART)?;
    SEEN_AHEAD.fetch_add(PART, Ordering::Relaxed);
    let mut packer = PACKERS.take(PartPacker::new);
    let seen = Seen::of(&data, &mut packer);
    PACKERS.give(packer);
    Ok(seen)
}

/// fxdax: the bytes of segments' parts [`see`] read, this process's stores'.
static SEEN_AHEAD: AtomicU64 = AtomicU64::new(0);

/// fxdax: the bytes of segments' parts looks ahead read since this process started ([`see`]).
pub(crate) fn seen_ahead() -> u64 {
    SEEN_AHEAD.load(Ordering::Relaxed)
}

/// fxdax: a part of a segment's data, being framed ([`read`]): its bytes, once read, each in a
/// buffer of its own ([`Buffer`]); if its segment may be framed in place, what a look at them saw,
/// ahead or not.
pub(crate) struct Read {
    pub data: Option<Buffer>,
    pub seen: Seen,
}

impl Read {
    /// Its bytes, read.
    pub fn data(&self) -> &[u8] {
        self.data.as_deref().expect("a part's bytes, read")
    }
}

/// fxdax: the parts of the `len` bytes of a segment's data its raw file `raw` holds, on the
/// calling thread; and, if `in_place`, the segment may be framed in place, what a look at each saw:
/// of those `ahead` saw, ahead ([`see`]), that, their bytes not read; of the others, a look at their
/// bytes, read. Those whose bytes it didn't read [`read_rest`] reads, if the framing needs them.
pub(crate) fn read<F: File>(
    raw: &F,
    len: u64,
    in_place: bool,
    ahead: &[Option<Seen>],
) -> io::Result<Vec<Read>> {
    let mut packer = PACKERS.take(PartPacker::new);
    let parts = (0..len.div_ceil(PART)).map(|part| {
        let seen = ahead.get(part as usize).copied().flatten().filter(|_| in_place);
        if let Some(seen) = seen {
            // Its bytes as they were seen: else a look ahead raced a write to it.
            #[cfg(any(test, feature = "audit"))]
            assert_eq!(see(raw, part)?, seen, "part {part}, seen ahead, changed since");
            return Ok(Read { data: None, seen });
        }
        let data = read_part(raw, len, part)?;
        let seen = if in_place { Seen::of(&data, &mut packer) } else { Seen::default() };
        Ok(Read { data: Some(data), seen })
    });
    let parts = parts.collect();
    PACKERS.give(packer);
    parts
}

/// fxdax: reads the bytes of those of `parts`, of the `len` bytes of a segment's data its raw file
/// `raw` holds, that [`read`] didn't: for a framing in a copy, which packs them all.
pub(crate) fn read_rest<F: File>(raw: &F, len: u64, parts: &mut [Read]) -> io::Result<()> {
    for (part, read) in (0..).zip(parts) {
        if read.data.is_none() {
            read.data = Some(read_part(raw, len, part)?);
        }
    }
    Ok(())
}

/// fxdax: part `part` of the `len` bytes of a segment's data its raw file `raw` holds, read.
fn read_part<F: File>(raw: &F, len: u64, part: u64) -> io::Result<Buffer> {
    let at = part * PART;
    let mut data = Buffer::of((len - at).min(PART) as usize);
    raw.read_cached(&mut data, at)?;
    Ok(data)
}

/// fxdax: `parts`, a segment's data, read ([`read`]), packed, on threads ([`on_threads`]); and the
/// longest any thread waited to run, in nanoseconds.
pub(crate) fn pack(parts: &[Read]) -> (Vec<PackedPart>, u64) {
    let (packed, waited) =
        on_threads(parts.len(), |part, packer| Ok(packer.pack(parts[part].data())));
    (packed.expect("packing fails not"), waited)
}

/// fxdax: `each` of `count` parts, on as many threads as the host has CPUs for this process, but
/// no more than there are parts, each thread taking those after the last's, with a packer of its
/// own, kept for the threads after ([`KEPT`]): their results, in order, or the first error; and the longest any of the threads waited to
/// run, runnable, in nanoseconds ([`runnable_ns`]). On the calling thread, if one is all it takes.
fn on_threads<T: Send>(
    count: usize,
    each: impl Fn(usize, &mut PartPacker) -> io::Result<T> + Sync,
) -> (io::Result<Vec<T>>, u64) {
    let threads = std::thread::available_parallelism().map_or(1, |threads| threads.get());
    let per = count.div_ceil(threads.min(count).max(1));
    let run = |parts: std::ops::Range<usize>| {
        let since = runnable_ns();
        let mut packer = PACKERS.take(PartPacker::new);
        let done: io::Result<Vec<T>> = parts.map(|part| each(part, &mut packer)).collect();
        PACKERS.give(packer);
        (done, runnable_ns().saturating_sub(since))
    };
    if per >= count {
        return run(0..count);
    }
    std::thread::scope(|scope| {
        let running: Vec<_> = (0..count)
            .step_by(per)
            .map(|first| scope.spawn(move || run(first..(first + per).min(count))))
            .collect();
        let (mut all, mut waited) = (Ok(Vec::with_capacity(count)), 0);
        for thread in running {
            let (done, thread_waited) = thread.join().expect("a framing's thread panicked");
            waited = waited.max(thread_waited);
            all = all.and_then(|mut all: Vec<T>| {
                all.extend(done?);
                Ok(all)
            });
        }
        (all, waited)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dir, MemDir};

    const MIB: usize = 1 << 20;

    /// `len` bytes that don't compress, of `seed`'s.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// Whether `data`, a segment's, saves too little compressed, by a look at each of its parts.
    fn saves_little(data: &[u8]) -> bool {
        let mut packer = PartPacker::new();
        let parts = data.chunks(PART as usize).map(|part| Look::at(part, &mut packer));
        parts.fold(Look::default(), Look::plus).saves_little()
    }

    /// TASK-343: a segment's look short of compressing it sees all of it: a segment of 3 MiB of
    /// noise, then holes, an extent's zeros, as Hunt's hostile VM process made one, saves much
    /// compressed, as do zeros at every offset no sample piece reaches; one of noise alone, or with
    /// a few blocks of zeros, saves little. Red with every eighth MiB part sampled: the first only
    /// of a segment under 9 MiB, noise, so it saved little, and was framed in place, raw.
    #[test]
    fn segments_holding_holes_save_much_compressed() {
        let mut holes = noise(1, 3 * MIB);
        holes.resize(6 * MIB, 0);
        assert!(!saves_little(&holes), "3 MiB of noise, then 3 MiB of zeros");
        // Zeros past each part's sampled piece, where no sample reaches.
        let mut unsampled = noise(2, 8 * MIB);
        for part in unsampled.chunks_mut(MIB) {
            part[SAMPLE..SAMPLE + MIB / 4].fill(0);
        }
        assert!(!saves_little(&unsampled), "a quarter of each part zeros, unsampled");
        assert!(saves_little(&noise(3, 6 * MIB)), "noise");
        let mut few = noise(4, 6 * MIB);
        few[..4 * ZERO_BLOCK].fill(0);
        assert!(saves_little(&few), "noise, but for four blocks of zeros");
    }

    /// TASK-336: a segment's parts, read, then packed on threads, are its data's, in order, each
    /// packed as one packer packs it, a last part cut short among them; each checksummed and
    /// looked at as a raw part if it may be framed in place, and not otherwise.
    #[test]
    fn parts_read_are_the_segments() {
        let len = 9 * MIB + 1000;
        let mut data = noise(5, len);
        data[2 * MIB..3 * MIB].fill(7);
        let raw = MemDir::new().create("raw", true).unwrap();
        raw.write_at(&data, 0).unwrap();
        let parts = read(&raw, len as u64, true, &[]).unwrap();
        assert_eq!(parts.len(), 10);
        let mut packer = PartPacker::new();
        for (read, part) in parts.iter().zip(data.chunks(PART as usize)) {
            assert!(read.data() == part, "a part's bytes");
            assert_eq!(read.seen.crc, PackedPart::raw(part).crc);
            assert_eq!(read.seen.look, Look::at(part, &mut packer));
        }
        let (packed, _) = pack(&parts);
        let each: Vec<PackedPart> =
            data.chunks(PART as usize).map(|part| packer.pack(part)).collect();
        assert_eq!(packed, each);
        assert!(packed[2].compressed.is_some() && packed[0].compressed.is_none());
        let parts = read(&raw, len as u64, false, &[]).unwrap();
        assert!(parts.iter().all(|read| read.seen == Seen::default()));
        assert!(read(&raw, 0, true, &[]).unwrap().is_empty(), "no data, no parts");
    }

    /// TASK-400: the parts of a segment that may be framed in place a look ahead saw are not read
    /// again, what it saw theirs, until a framing in a copy needs their bytes; what a look ahead
    /// sees of a part is what a look at it framing sees; and the parts of one that may not be are
    /// read, what was seen of them not taken.
    #[test]
    fn parts_seen_ahead_are_read_only_for_a_copy() {
        let len = 3 * MIB + 1000;
        let data = noise(6, len);
        let raw = MemDir::new().create("raw", true).unwrap();
        raw.write_at(&data, 0).unwrap();
        let ahead = [Some(see(&raw, 0).unwrap()), None, Some(see(&raw, 2).unwrap())];
        let mut parts = read(&raw, len as u64, true, &ahead).unwrap();
        let unread: Vec<bool> = parts.iter().map(|read| read.data.is_none()).collect();
        assert_eq!(unread, [true, false, true, false]);
        let fresh = read(&raw, len as u64, true, &[]).unwrap();
        let seen = |parts: &[Read]| parts.iter().map(|read| read.seen).collect::<Vec<_>>();
        assert_eq!(seen(&parts), seen(&fresh), "seen ahead as seen framing");
        read_rest(&raw, len as u64, &mut parts).unwrap();
        for (read, part) in parts.iter().zip(data.chunks(PART as usize)) {
            assert!(read.data() == part, "a part's bytes, read for a copy");
        }
        let parts = read(&raw, len as u64, false, &ahead).unwrap();
        assert!(parts.iter().all(|read| read.data.is_some() && read.seen == Seen::default()));
    }

    /// TASK-400, under the `audit` feature: a part seen ahead that changed since fails its
    /// framing loudly, a look ahead having raced a write to it, rather than frame a CRC that isn't
    /// its bytes'.
    #[test]
    #[should_panic(expected = "part 1, seen ahead, changed since")]
    fn a_part_changed_since_it_was_seen_panics() {
        let raw = MemDir::new().create("raw", true).unwrap();
        raw.write_at(&noise(7, 2 * MIB), 0).unwrap();
        let ahead = [None, Some(see(&raw, 1).unwrap())];
        raw.write_at(&[1; 4096], MIB as u64).unwrap();
        let _ = read(&raw, 2 * MIB as u64, true, &ahead);
    }

    /// TASK-336: a part that fails to read fails them all.
    #[test]
    fn a_part_failing_to_read_fails_the_framing() {
        struct Failing;
        impl File for Failing {
            fn read_at(&self, _: &mut [u8], at: u64) -> io::Result<()> {
                match at == 3 * PART {
                    true => Err(io::Error::other("failing")),
                    false => Ok(()),
                }
            }
            fn write_at(&self, _: &[u8], _: u64) -> io::Result<()> {
                unreachable!()
            }
            fn size(&self) -> io::Result<u64> {
                unreachable!()
            }
            fn sync(&self) -> io::Result<()> {
                unreachable!()
            }
            fn sync_cached(&self) -> io::Result<()> {
                unreachable!()
            }
        }
        let error = read(&Failing, 8 * PART, true, &[]).err().expect("a part failed");
        assert_eq!(error.to_string(), "failing");
    }
}
