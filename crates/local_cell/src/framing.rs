// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a closed segment's parts, read from its raw file and looked at on the framer's thread,
//! then packed on threads, each part in a buffer kept for the framings after, as are the
//! compressors (TASK-336): what a framing does with its data before it writes it
//! (`segments::frame_segment`). And a look at each part as the segment fills, once it is written
//! whole ([`see`], TASK-400), which its framing reads no more if it frames the segment in place.
//!
//! A framing read the whole of its data, 32 MiB, into one buffer taken afresh, every page faulted
//! in, 8,192 faults, and its packing threads took compressors afresh, their tables faulted in too.
//! On a host whose CPUs its VMs' vCPUs keep busy, a thread gets its share of them and no more: so
//! the faults, and the copying and looking on the framer's thread, took most of a framing's time,
//! up to half a second each of 0.8 s beside 28 spinning threads on ubu1's 8 CPUs (cell-bench
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
//! still, the framing has and reads not. A segment framed in a copy, which compresses, needs each
//! part packed: the look ahead packs it too, while the parts seen say the segment will be framed
//! so, and holds it packed in memory ([`crate::held`]), which its framing takes; a part let go of
//! meanwhile, the host's memory short, its framing reads back and packs, as before.
//!
//! cladefs: a store's segments, encrypted, are looked at ahead and held as clear stores' were
//! (TASK-411.23, TASK-421): what a look sees of a part is its stored bytes' CRC and tag, the raw
//! file's XTS sectors, as a framing in place stores them ([`extent_frame::RawPart`]), and a look at
//! them decrypted; a part packed ahead is packed in the clear and held in memory, which holds
//! plaintext anyway (the extent cache, doc-16), and encrypted once its framing in a copy draws its
//! object's key ([`pack`]): so that no key is drawn before the framing, nor a part's nonce under it
//! used twice. Nothing of it reaches the disk in the clear.

use crate::dir::File;
use crate::held::{Hold, Pending};
use crate::raw::RawFile;
use crate::timings::runnable_ns;
use extent_frame::{ObjectKey, PART, PackedPart, PartPacker, RawPart};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

/// cladefs: the most part buffers ([`Buffer`]) and compressors ([`PartPacker`]) kept for framings to
/// come: two segments' worth of parts, 64 MiB, and as many compressors. Taken
/// afresh, each framing's 32 MiB of buffers were faulted in page by page, 8,192 faults, and each
/// thread's compressor's tables too, every framing: as much of its reading's time as the copying
/// (cell-bench framing, perf stat on ubu1). More framings at once than that take theirs afresh,
/// and keep them only while there is room.
const KEPT: usize = 2 * 32;

/// cladefs: what [`KEPT`] keeps, taken by framings and given back.
struct Pool<T> {
    kept: Mutex<Vec<T>>,
}

impl<T> Pool<T> {
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

static BUFFERS: Pool<Vec<u8>> = Pool::new();
static PACKERS: Pool<PartPacker> = Pool::new();

/// cladefs: the bytes of a part of a segment's data, read, in a buffer of a part's bytes kept for
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

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Buffer({} bytes)", self.0.len())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        BUFFERS.give(std::mem::take(&mut self.0));
    }
}

/// cladefs: what of a segment's data compressing its parts must save for it to be framed in a copy,
/// compressed, rather than in place, every part raw (`segments::frame_segment`, TASK-310): an
/// eighth. Writing the data again to save less of it, in the bucket and in the host's cache, costs
/// the host's disk more than it saves, on a disk VMs share with their bucket above all. Framed in
/// place only if none compressed, segments a guest's filesystem fills, a large file's data with its
/// metadata in a few of their parts, were all framed in copies but for a few.
pub(crate) const IN_PLACE_SAVING: u64 = 8;

/// cladefs: the bytes of each part of a segment's data its sample compresses ([`Look`]): an eighth,
/// its first, so that every part has its share sampled, spread over all of it (TASK-343).
pub(crate) const SAMPLE: usize = (PART / 8) as usize;

/// cladefs: the blocks of a segment's data [`Look`] finds zeros in, every one of them.
const ZERO_BLOCK: usize = 4096;

/// cladefs: a look at parts of a segment's data short of compressing them: their bytes, those of
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

/// cladefs: what a look at a part of a segment's data that may be framed in place saw of it: its
/// bytes as stored, as a raw part's, their CRC and, of an encrypted raw file's sectors, their tag
/// (TASK-411.23); and a look at its data (TASK-310).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    pub part: RawPart,
    pub look: Look,
}

/// cladefs: part `part` of a segment's data, of `len` bytes, read from its raw file `raw` through the
/// host's page cache, as stored, then decrypted, if encrypted; and, if `seen`, what a look at it sees,
/// its sample compressed by `packer`: its stored bytes as a framing in place stores them, the raw
/// file's sectors, and its data (TASK-411.23).
fn read_seen<F: File>(
    raw: &RawFile<F>,
    (part, len): (u64, u64),
    seen: bool,
    packer: &mut PartPacker,
) -> io::Result<(Buffer, Seen)> {
    let mut data = Buffer::of(len as usize);
    let at = part * PART;
    if !seen {
        raw.read_cached(&mut data, at)?;
        return Ok((data, Seen::default()));
    }
    raw.file().read_cached(&mut data, at)?;
    let stored = RawPart::of(part, &data, raw.sectors());
    raw.decrypt(at, &mut data);
    let look = Look::at(&data, packer);
    Ok((data, Seen { part: stored, look }))
}

/// cladefs: what a look ahead at a part of a segment's data took of it, as the segment filled
/// ([`see`], TASK-400): what it saw, and the part packed, held, if its segment looked as if it would
/// be framed in a copy then.
#[derive(Debug)]
pub(crate) struct Looked {
    pub seen: Seen,
    pub held: Option<Hold>,
}

/// cladefs: a part of a segment's data packed ([`PartPacker::pack`]), as a framing in a copy stores
/// it: compressed (or encrypted, framing), or raw, then its bytes in `raw` if they are held with it
/// ([`see`]).
#[derive(Debug)]
pub(crate) struct Stored {
    pub part: PackedPart,
    pub raw: Option<Buffer>,
}

impl Stored {
    /// The bytes of memory it takes.
    pub fn bytes(&self) -> u64 {
        let raw = self.raw.as_ref().map(|raw| raw.0.capacity());
        (self.part.stored_capacity() + raw.unwrap_or(0)) as u64
    }
}

#[cfg(any(test, feature = "testing"))]
impl Stored {
    /// For the tests of what holds parts: one taking `bytes` bytes of memory, of no part's data.
    pub(crate) fn of_bytes(bytes: usize) -> Self {
        let part = PartPacker::new().compress(0, &[]);
        Self { part, raw: Some(Buffer(vec![0; bytes])) }
    }
}

/// cladefs: a look ahead at part `part` of a segment's data, whole, in its raw file `raw`, as the
/// segment fills ([`look_ahead`]), and the part packed then and there ([`stored`]), if it is to be:
/// as the store's framer packs it on a directory whose I/O is its callers' alone.
pub(crate) fn see<F: File>(
    raw: &RawFile<F>,
    part: u64,
    before: Look,
) -> io::Result<(Seen, Option<Stored>)> {
    let (seen, data) = look_ahead(raw, part, before)?;
    let packed = data.map(|data| {
        let mut packer = PACKERS.take(PartPacker::new);
        let packed = stored(part, data, &mut packer);
        PACKERS.give(packer);
        packed
    });
    Ok((seen, packed))
}

/// cladefs: a look ahead at part `part` of a segment's data, whole, in its raw file `raw`, as the
/// segment fills ([`Seen`], TASK-400), on the framer's thread, in order with the framings: read
/// through the host's page cache, which holds it, just written, still, and looked at. Its bytes
/// change no more: the writes to a segment write each of its bytes once, and the part is whole once
/// the last write to it ended. And its bytes, to pack ahead ([`pack_ahead`]), if `before`, the look
/// at the segment's parts seen before it, with this one's, doesn't say the segment will be framed
/// in place ([`Look::saves_little`]): so that its framing in a copy reads it no more, the part held
/// ([`crate::held`]). cladefs: what it sees of an encrypted raw file's part, its stored bytes' CRC and
/// tag, and a look at them decrypted; and its bytes decrypted, to pack ahead in the clear, held in
/// memory alone (TASK-411.23, TASK-421).
pub(crate) fn look_ahead<F: File>(
    raw: &RawFile<F>,
    part: u64,
    before: Look,
) -> io::Result<(Seen, Option<Buffer>)> {
    let mut packer = PACKERS.take(PartPacker::new);
    let seen = read_seen(raw, (part, PART), true, &mut packer);
    PACKERS.give(packer);
    let (data, seen) = seen?;
    SEEN_AHEAD.fetch_add(PART, Ordering::Relaxed);
    let packs = !before.plus(seen.look).saves_little();
    Ok((seen, packs.then_some(data)))
}

/// cladefs: `data`, part `index` of a segment's data, compressed alone by `packer`, as a framing in a
/// copy compresses it before it encrypts it ([`pack`], TASK-421): holding only what of the buffer
/// compressing took the compressed bytes need, or raw, its bytes kept.
fn stored(index: u64, data: Buffer, packer: &mut PartPacker) -> Stored {
    let mut part = packer.compress(index, &data);
    let raw = match part.stored_bytes() {
        // What it holds of the buffer compressing took, a part's room.
        Some(_) => {
            part.shrink_to_fit();
            None
        }
        None => Some(data),
    };
    Stored { part, raw }
}

/// cladefs: how many threads pack parts ahead ([`pack_ahead`]), the process's stores': at the host's
/// lowest priority, so that they take only what CPU nothing else wants.
const PACKING_AHEAD: usize = 2;

/// cladefs: the most parts waiting to be packed ahead, a MiB each: past them, the packing is too far
/// behind for a part's framing to find it packed, and it skips them ([`Pending::skip`]).
const WAITING_MOST: usize = 64;

/// cladefs: the parts waiting to be packed ahead, and the threads packing them, started as there are
/// parts for them, and kept for the process's life.
struct Ahead {
    waiting: Mutex<(std::collections::VecDeque<Waiting>, usize)>,
    ready: Condvar,
}

/// cladefs: a part waiting to be packed ahead ([`pack_ahead`]): its index in its segment, which its
/// packing takes (`PartPacker::pack`, TASK-411.2), its bytes, and where it is to be held.
type Waiting = (u64, Buffer, Pending);

static AHEAD: Ahead =
    Ahead { waiting: Mutex::new((std::collections::VecDeque::new(), 0)), ready: Condvar::new() };

/// cladefs: packs `data`, part `part` of a segment's data a look ahead took ([`look_ahead`]), on a thread
/// of [`PACKING_AHEAD`], at the host's lowest priority, and holds it as `pending` says, if it is
/// still to be held then: off the framer's thread, so that a look ahead starved of CPU, the host's
/// vCPUs and flushes taking it, leaves its parts unheld, which their framings read and pack, at
/// their own priority, and holds up no framing. Packing ahead on the framer's thread, then on
/// threads of the framer's priority, all the host's CPUs' worth, while the guests wrote and
/// committed, took from the daemon's flushes (sandboxes short mode: durable lag within 50 ms down
/// by 10-30 versions). Skipped, if too many wait already ([`WAITING_MOST`]).
pub(crate) fn pack_ahead(part: u64, data: Buffer, pending: Pending) {
    let mut waiting = AHEAD.waiting.lock().unwrap();
    if waiting.0.len() >= WAITING_MOST {
        drop(waiting);
        pending.skip();
        return;
    }
    waiting.0.push_back((part, data, pending));
    if waiting.1 < PACKING_AHEAD {
        let spawned = std::thread::Builder::new().name("store packing ahead".into()).spawn(packing);
        match spawned {
            Ok(_) => waiting.1 += 1,
            Err(err) if waiting.1 == 0 => {
                log::warn!("extent store: a thread to pack parts ahead: {err}");
                let refused: Vec<_> = waiting.0.drain(..).collect();
                drop(waiting);
                refused.into_iter().for_each(|(_, _, pending)| pending.skip());
                return;
            }
            Err(_) => {}
        }
    }
    AHEAD.ready.notify_one();
}

/// cladefs: a thread packing parts ahead ([`pack_ahead`]), as they wait, for the process's life.
fn packing() {
    lowest_priority();
    let mut packer = PartPacker::new();
    loop {
        let (part, data, pending) = {
            let mut waiting = AHEAD.waiting.lock().unwrap();
            loop {
                if let Some(next) = waiting.0.pop_front() {
                    break next;
                }
                waiting = AHEAD.ready.wait(waiting).unwrap();
            }
        };
        if pending.wanted() {
            pending.fill(stored(part, data, &mut packer));
        }
    }
}

/// cladefs: has the calling thread run at the host's lowest priority: Linux's idle scheduling class,
/// which runs it only on CPUs nothing else wants, or else its lowest niceness; macOS's background
/// band. Logged if refused: it runs at its priority then.
fn lowest_priority() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: all zeros is a sched_param of priority 0, the idle class's only (musl's has
        // fields of its own, which zeros leave unused); then sets the calling thread's scheduling
        // class (0: the calling thread).
        let set = unsafe {
            let param: libc::sched_param = std::mem::zeroed();
            libc::sched_setscheduler(0, libc::SCHED_IDLE, &param)
        };
        if set == 0 {
            return;
        }
        // SAFETY: sets the calling thread's niceness: Linux's are a thread's, by its ID.
        let set = unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::setpriority(libc::PRIO_PROCESS, tid, 19)
        };
        if set != 0 {
            log::warn!(
                "extent store: packing ahead at its priority: {}",
                io::Error::last_os_error()
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: moves the calling thread into the background band (0: the calling thread).
        if unsafe { libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG) } != 0 {
            log::warn!(
                "extent store: packing ahead at its priority: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// cladefs: what a look at part `part` of a segment's data, whole, in its raw file `raw`, sees: what
/// a look ahead at it saw, if it changed not since.
#[cfg(any(test, feature = "audit"))]
fn seen_again<F: File>(raw: &RawFile<F>, part: u64) -> io::Result<Seen> {
    let mut packer = PACKERS.take(PartPacker::new);
    let seen = read_seen(raw, (part, PART), true, &mut packer);
    PACKERS.give(packer);
    Ok(seen?.1)
}

/// cladefs: the bytes of segments' parts [`see`] read, this process's stores'.
static SEEN_AHEAD: AtomicU64 = AtomicU64::new(0);

/// cladefs: the bytes of segments' parts looks ahead read since this process started ([`see`]).
pub(crate) fn seen_ahead() -> u64 {
    SEEN_AHEAD.load(Ordering::Relaxed)
}

/// cladefs: a part of a segment's data, being framed ([`read`]), of `len` bytes: its bytes, once
/// read, each in a buffer of its own ([`Buffer`]); packed, once it is, ahead or framing; held, if a
/// look ahead packed it, until the framing takes it ([`fill`]); and if its segment may be framed in
/// place, what a look at it saw, ahead or not.
pub(crate) struct Read {
    pub data: Option<Buffer>,
    pub packed: Option<Stored>,
    pub held: Option<Hold>,
    pub seen: Seen,
    pub len: u64,
}

impl Read {
    /// Its bytes, read.
    pub fn data(&self) -> &[u8] {
        self.data.as_deref().expect("a part's bytes, read")
    }

    /// Its bytes as a framing in a copy stores them, packed ([`pack`]), and how.
    pub fn stored(&self) -> (&[u8], &PackedPart) {
        let packed = self.packed.as_ref().expect("a part packed");
        let raw = || packed.raw.as_deref().unwrap_or_else(|| self.data());
        (packed.part.stored_bytes().unwrap_or_else(raw), &packed.part)
    }
}

/// cladefs: the parts of the `len` bytes of a segment's data its raw file `raw` holds, on the
/// calling thread; and, if `in_place`, the segment may be framed in place, what a look at each saw:
/// of those `ahead` saw, ahead ([`see`]), that, their bytes not read; of the others, a look at their
/// bytes, read. Those packed ahead, held, it reads not either way. Those whose bytes it didn't read
/// [`fill`] packs or reads, if the framing needs them. cladefs: the bytes read are the data, decrypted
/// if encrypted; what is seen of them, their stored bytes' and their data's ([`read_seen`],
/// TASK-411.23).
pub(crate) fn read<F: File>(
    raw: &RawFile<F>,
    len: u64,
    in_place: bool,
    ahead: Vec<Option<Looked>>,
) -> io::Result<Vec<Read>> {
    let mut packer = PACKERS.take(PartPacker::new);
    let mut ahead = ahead.into_iter();
    let parts = (0..len.div_ceil(PART)).map(|part| {
        let part_len = (len - part * PART).min(PART);
        let looked = ahead.next().flatten().filter(|looked| in_place || looked.held.is_some());
        if let Some(Looked { seen, held }) = looked {
            let seen = match in_place {
                true => {
                    // Its bytes as they were seen: else a look ahead raced a write to it.
                    #[cfg(any(test, feature = "audit"))]
                    assert_eq!(
                        seen_again(raw, part)?,
                        seen,
                        "part {part}, seen ahead, changed since"
                    );
                    seen
                }
                false => Seen::default(),
            };
            return Ok(Read { data: None, packed: None, held, seen, len: part_len });
        }
        let (data, seen) = read_seen(raw, (part, part_len), in_place, &mut packer)?;
        Ok(Read { data: Some(data), packed: None, held: None, seen, len: part_len })
    });
    let parts = parts.collect();
    PACKERS.give(packer);
    parts
}

/// cladefs: for a framing in a copy, which packs all of `parts`, of the `len` bytes of a segment's
/// data its raw file `raw` holds, those [`read`] didn't read: each held, packed ahead, taken; the
/// others, those not packed ahead, or let go of since, read.
pub(crate) fn fill<F: File>(raw: &RawFile<F>, len: u64, parts: &mut [Read]) -> io::Result<()> {
    for (part, read) in (0..).zip(parts) {
        if read.data.is_some() || read.packed.is_some() {
            continue;
        }
        match read.held.take().and_then(Hold::take) {
            Some(packed) => {
                // As the part is now, packed: else a look ahead raced a write to it.
                #[cfg(any(test, feature = "audit"))]
                {
                    let data = read_part(raw, len, part)?;
                    let mut packer = PACKERS.take(PartPacker::new);
                    let again = packer.compress(part, &data);
                    PACKERS.give(packer);
                    assert_eq!(again, packed.part, "part {part}, packed ahead, changed since");
                    let raw = packed.raw.as_deref();
                    assert!(raw.is_none_or(|raw| raw == &data[..]), "part {part}, held raw");
                }
                read.packed = Some(packed);
            }
            None => read.data = Some(read_part(raw, len, part)?),
        }
    }
    Ok(())
}

/// cladefs: part `part` of the `len` bytes of a segment's data its raw file `raw` holds, read,
/// decrypted if encrypted.
fn read_part<F: File>(raw: &RawFile<F>, len: u64, part: u64) -> io::Result<Buffer> {
    let at = part * PART;
    let mut data = Buffer::of((len - at).min(PART) as usize);
    raw.read_cached(&mut data, at)?;
    Ok(data)
}

/// cladefs: packs `parts`, a segment's data, read ([`read`], [`fill`]), on threads
/// ([`on_threads`]), encrypted under `key`, its framed file's (TASK-411.2); and returns the longest
/// any thread waited to run, in nanoseconds. Those packed ahead, held, compressed alone, are encrypted
/// here under its key, drawn as its framing began, as packing them here would (TASK-421).
pub(crate) fn pack(parts: &mut [Read], key: &ObjectKey) -> u64 {
    let (packed, waited) = {
        let parts = &*parts;
        on_threads(parts.len(), |index, packer| {
            let read = &parts[index];
            Ok(match &read.packed {
                None => packer.pack(index as u64, read.data(), key),
                Some(_) => {
                    let (stored, held) = read.stored();
                    held.encrypted(read.len, stored, key)
                }
            })
        })
    };
    for (at, part) in packed.expect("packing fails not").into_iter().enumerate() {
        parts[at].packed = Some(Stored { part, raw: None });
    }
    waited
}

/// cladefs: `each` of `count` parts, on as many threads as the host has CPUs for this process, but
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
    use crate::{Dir, HeldParts, MemDir};
    use std::sync::Arc;

    const MIB: usize = 1 << 20;

    /// The key the tests' encrypted segments are encrypted under.
    const KEY: extent_frame::KeyId = extent_frame::KeyId { branch: 1, generation: 0 };

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

    /// `len` bytes that compress, of `seed`'s: letters at random, sixteen of them.
    fn text(seed: u64, len: usize) -> Vec<u8> {
        noise(seed, len).into_iter().map(|byte| b'a' + byte % 16).collect()
    }

    /// The tests' DEK, which [`KEY`] names.
    fn dek() -> extent_frame::Secret {
        extent_frame::Secret::from_bytes(&[0x3c; 32])
    }

    /// An encryption of a raw file under [`dek`], from a salt of its own.
    fn encryption() -> Arc<crate::raw::RawEncryption> {
        Arc::new(crate::raw::RawEncryption::new(&dek(), KEY))
    }

    /// A raw file holding `data`, whole sectors, encrypted (TASK-411.5, TASK-411.28).
    fn raw_of(data: &[u8]) -> RawFile<crate::MemFile> {
        encrypted_raw_of(data, encryption())
    }

    /// A raw file holding `data`, whole sectors, encrypted by `encryption` (TASK-411.5).
    fn encrypted_raw_of(
        data: &[u8],
        encryption: Arc<crate::raw::RawEncryption>,
    ) -> RawFile<crate::MemFile> {
        let raw = RawFile::new(MemDir::new().create("raw", true).unwrap(), encryption);
        raw.write_at(data, 0).unwrap();
        raw
    }

    /// A packer of a segment's framed file, encrypted under [`dek`], from a salt drawn for it.
    fn encrypting() -> extent_frame::Packer {
        extent_frame::Packer::new((&dek(), KEY))
    }

    /// The bytes `parts` store, framed in a copy, in order, and where each is stored: as packed
    /// ([`pack`]), each encrypted under the key it was given, which a framing's packer of the same
    /// kind takes, whatever its salt.
    fn stored(parts: &[Read]) -> (Vec<u8>, Vec<extent_frame::Stored>) {
        let mut packer = encrypting();
        let mut bytes = Vec::new();
        for part in parts {
            let (stored, how) = part.stored();
            packer.push_stored(&mut bytes, part.len, stored, how);
        }
        let (_, parts) = packer.finish_parts(b"footer");
        (bytes, parts.stored)
    }

    /// TASK-336: a segment's parts, read, then packed on threads, are its data's, in order, each
    /// packed as one packer packs it, a last part cut short among them; each checksummed and
    /// looked at as a raw part if it may be framed in place, and not otherwise.
    #[test]
    fn parts_read_are_the_segments() {
        let len = 9 * MIB + 4096;
        let mut data = noise(5, len);
        data[2 * MIB..3 * MIB].fill(7);
        let raw = raw_of(&data);
        let mut parts = read(&raw, len as u64, true, Vec::new()).unwrap();
        assert_eq!(parts.len(), 10);
        let mut packer = PartPacker::new();
        let mut stored = vec![0; len];
        raw.file().read_at(&mut stored, 0).unwrap();
        for ((index, read), part) in parts.iter().enumerate().zip(data.chunks(PART as usize)) {
            assert!(read.data() == part, "a part's bytes");
            let at = &stored[index * MIB..][..part.len()];
            assert_eq!(read.seen.part, RawPart::of(index as u64, at, raw.sectors()));
            assert_eq!(read.seen.look, Look::at(part, &mut packer));
        }
        // TASK-411.2: encrypted under a segment's key, each as one packer encrypts it.
        let encrypting = encrypting();
        pack(&mut parts, encrypting.key());
        let packed: Vec<PackedPart> =
            parts.iter().map(|read| read.packed.as_ref().unwrap().part.clone()).collect();
        let each: Vec<PackedPart> = (data.chunks(PART as usize).enumerate())
            .map(|(index, part)| packer.pack(index as u64, part, encrypting.key()))
            .collect();
        assert_eq!(packed, each);
        let encodings = (packed[2].encoding(), packed[0].encoding());
        assert_eq!(encodings, (extent_frame::Encoding::Zstd, extent_frame::Encoding::Raw));
        // Not the bytes compressing alone stores: encrypted.
        let compressed: Vec<PackedPart> = (data.chunks(PART as usize).enumerate())
            .map(|(index, part)| packer.compress(index as u64, part))
            .collect();
        let crcs = |packed: &[PackedPart]| packed.iter().map(PackedPart::crc).collect::<Vec<_>>();
        assert!(
            (crcs(&packed).iter().zip(crcs(&compressed)))
                .all(|(encrypted, alone)| *encrypted != alone)
        );
        let parts = read(&raw, len as u64, false, Vec::new()).unwrap();
        assert!(parts.iter().all(|read| read.seen == Seen::default()));
        assert!(read(&raw, 0, true, Vec::new()).unwrap().is_empty(), "no data, no parts");
    }

    /// Looks ahead at `parts` of `raw`, as the store's framer does, each with the look at those
    /// before it, holding in `held` those it packs.
    fn ahead(
        raw: &RawFile<crate::MemFile>,
        parts: &[u64],
        held: &Arc<HeldParts>,
    ) -> Vec<Option<Looked>> {
        let mut look = Look::default();
        let mut looked: Vec<Option<Looked>> = Vec::new();
        for &part in parts {
            let (seen, packed) = see(raw, part, look).unwrap();
            look = look.plus(seen.look);
            looked.resize_with(looked.len().max(part as usize + 1), || None);
            looked[part as usize] = Some(Looked { seen, held: packed.map(|it| held.hold(it)) });
        }
        looked
    }

    /// TASK-400: the parts of a segment that may be framed in place a look ahead saw are not read
    /// again, what it saw theirs, until a framing in a copy needs their bytes; what a look ahead
    /// sees of a part is what a look at it framing sees; and the parts of one that may not be are
    /// read, what was seen of them not taken.
    #[test]
    fn parts_seen_ahead_are_read_only_for_a_copy() {
        let len = 3 * MIB + 4096;
        let data = noise(6, len);
        let raw = raw_of(&data);
        let held = HeldParts::new();
        let looked = ahead(&raw, &[0, 2], &held);
        assert_eq!(held.bytes(), 0, "noise, packed not");
        let mut parts = read(&raw, len as u64, true, looked).unwrap();
        let unread: Vec<bool> = parts.iter().map(|read| read.data.is_none()).collect();
        assert_eq!(unread, [true, false, true, false]);
        let fresh = read(&raw, len as u64, true, Vec::new()).unwrap();
        let seen = |parts: &[Read]| parts.iter().map(|read| read.seen).collect::<Vec<_>>();
        assert_eq!(seen(&parts), seen(&fresh), "seen ahead as seen framing");
        fill(&raw, len as u64, &mut parts).unwrap();
        for (read, part) in parts.iter().zip(data.chunks(PART as usize)) {
            assert!(read.data() == part, "a part's bytes, read for a copy");
        }
        let looked = ahead(&raw, &[0, 2], &held);
        let parts = read(&raw, len as u64, false, looked).unwrap();
        assert!(parts.iter().all(|read| read.data.is_some() && read.seen == Seen::default()));
    }

    /// TASK-400: the parts of a segment that compresses are packed as a look ahead sees them, and
    /// held, compressed, or raw for a part that doesn't compress, its bytes with it: its framing in
    /// a copy reads none of them, and stores the bytes a framing reading them all stores, whether
    /// all are held, some let go of to make room, newest first, or none. Before, a framing in a
    /// copy read its whole raw file back as it closed.
    #[test]
    fn parts_held_frame_as_parts_read() {
        let len = 4 * MIB + 4096;
        let mut data = text(8, len);
        // A part of noise among them, which is held raw.
        data[MIB..2 * MIB].copy_from_slice(&noise(9, MIB));
        let raw = raw_of(&data);
        // Each framing's parts encrypted under one key, so that their bytes compare.
        let key = encrypting();
        let framed = |looked: Vec<Option<Looked>>, quiet: bool| {
            let mut parts = read(&raw, len as u64, quiet, looked).unwrap();
            fill(&raw, len as u64, &mut parts).unwrap();
            let read: Vec<bool> = parts.iter().map(|read| read.data.is_some()).collect();
            pack(&mut parts, key.key());
            (stored(&parts), read)
        };
        let (expected, read) = framed(Vec::new(), true);
        assert_eq!(read, [true; 5], "nothing held, all read");

        let held = HeldParts::new();
        let looked = ahead(&raw, &[0, 1, 2, 3], &held);
        let counts = held.counts();
        let raw_part = PART;
        assert!(counts.bytes > raw_part && counts.bytes < raw_part + 3 * MIB as u64, "{counts:?}");
        for quiet in [true, false] {
            let looked = match quiet {
                true => std::mem::take(&mut ahead(&raw, &[0, 1, 2, 3], &held)),
                false => ahead(&raw, &[0, 1, 2, 3], &held),
            };
            let (bytes, read) = framed(looked, quiet);
            assert!(bytes == expected, "held, quiet {quiet}: the bytes framed reading them all");
            assert_eq!(read, [false, false, false, false, true], "the last part alone read");
        }
        drop(looked);
        assert_eq!(held.bytes(), 0, "taken, or dropped, held no more");

        // Some let go of to make room: the last held first.
        let looked = ahead(&raw, &[0, 1, 2, 3], &held);
        let two = held.bytes() - held.evict_to(held.bytes() / 2);
        assert!(two > 0 && held.counts().evicted_parts >= 1);
        let (bytes, read) = framed(looked, true);
        assert!(bytes == expected, "partly held: the bytes framed reading them all");
        assert_eq!(read.last(), Some(&true));
        assert!(read[0] == false && read[3] == true, "the first kept, the last let go: {read:?}");

        // All let go of.
        let looked = ahead(&raw, &[0, 1, 2, 3], &held);
        held.evict_to(0);
        let (bytes, read) = framed(looked, true);
        assert!(bytes == expected, "none held: the bytes framed reading them all");
        assert_eq!(read, [true; 5]);
        assert_eq!(held.bytes(), 0);
    }

    /// TASK-400: a look ahead packs a part only while the parts seen, it among them, don't say
    /// their segment will be framed in place: a part of text, which compresses by a little under
    /// half, then noise, packed until the parts seen save less than a sixteenth: seven, then not.
    #[test]
    fn parts_are_packed_while_the_parts_seen_say_a_copy() {
        let mut data = text(10, MIB);
        data.extend(noise(11, 11 * MIB));
        let raw = raw_of(&data);
        let mut look = Look::default();
        let mut packed = Vec::new();
        for part in 0..12 {
            let (seen, part) = see(&raw, part, look).unwrap();
            look = look.plus(seen.look);
            packed.push(part.is_some());
        }
        let expected: Vec<bool> = (0..12).map(|part| part < 7).collect();
        assert_eq!(packed, expected);
    }

    /// TASK-411.23, TASK-421: an encrypted raw file's parts, read or looked at ahead, are seen as a
    /// framing in place stores them, their stored bytes, the raw file's XTS sectors, checksummed
    /// and tagged under its key; looked at, and read, as their data, decrypted; and packed ahead in
    /// the clear, held, then encrypted as its framing in a copy packs them, under its object's key,
    /// byte for byte as the parts read and packed then.
    #[test]
    fn encrypted_raw_files_parts_are_seen_as_stored_and_held_as_data() {
        let len = 4 * MIB + 8 * 4096;
        let mut data = text(12, len);
        data[MIB..2 * MIB].copy_from_slice(&noise(13, MIB));
        let raw = encrypted_raw_of(&data, encryption());
        let sectors = raw.sectors().clone();
        let mut stored = vec![0; len];
        raw.file().read_at(&mut stored, 0).unwrap();
        assert!(stored != data, "ciphertext");
        let parts = read(&raw, len as u64, true, Vec::new()).unwrap();
        let mut packer = PartPacker::new();
        for (index, read) in parts.iter().enumerate() {
            let range = index * MIB..(len.min((index + 1) * MIB));
            assert!(read.data() == &data[range.clone()], "part {index}'s data");
            let expected = RawPart::of(index as u64, &stored[range.clone()], &sectors);
            assert_eq!(read.seen.part, expected, "part {index}, seen as stored");
            assert_ne!(expected.tag, [0; 16]);
            assert_eq!(read.seen.look, Look::at(&data[range], &mut packer), "part {index}");
        }
        let held = HeldParts::new();
        let looked = ahead(&raw, &[0, 1, 2, 3], &held);
        let seen: Vec<Seen> = looked.iter().map(|looked| looked.as_ref().unwrap().seen).collect();
        assert_eq!(seen, parts[..4].iter().map(|read| read.seen).collect::<Vec<_>>());
        assert!(held.bytes() > MIB as u64, "held, compressed alone, in memory");
        let mut framed = |looked: Vec<Option<Looked>>| {
            let encrypting = encrypting();
            let mut parts = read(&raw, len as u64, false, looked).unwrap();
            fill(&raw, len as u64, &mut parts).unwrap();
            let read: Vec<bool> = parts.iter().map(|read| read.data.is_some()).collect();
            pack(&mut parts, encrypting.key());
            let packed: Vec<PackedPart> =
                parts.iter().map(|read| read.packed.as_ref().unwrap().part.clone()).collect();
            let fresh: Vec<PackedPart> = (data.chunks(MIB).enumerate())
                .map(|(index, part)| packer.pack(index as u64, part, encrypting.key()))
                .collect();
            assert_eq!(packed, fresh, "encrypted as packed encrypted");
            read
        };
        assert_eq!(framed(looked), [false, false, false, false, true], "the last alone read");
        assert_eq!(framed(Vec::new()), [true; 5], "none held, all read");
        assert_eq!(held.bytes(), 0);
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
        let failing = RawFile::new(Failing, encryption());
        let error = read(&failing, 8 * PART, true, Vec::new()).err().expect("a part failed");
        assert_eq!(error.to_string(), "failing");
    }
}
