#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Whether a file created in a cladefs DAX mount reads in again as DAX once its writes are sealed
# and its inode evicted (doc-13, TASK-164.3), on a running guest (doc-30): kprobes count the
# inodes cladefs evicts and what cladefs_dax_set_inode answers as each is read in. Needs sudo.
#
#   probe-created-promotion.sh DIR [CRATE [OLD_FILE]]
#
# DIR is a directory on the cladefs mount to create files in; CRATE, one to build into DIR/target;
# OLD_FILE, a file created in an earlier mount, which should read in as DAX at once.

set -euo pipefail

dir=${1:?usage: probe-created-promotion.sh DIR [CRATE [OLD_FILE]]}
crate=${2:-}
old=${3:-}
tracing=/sys/kernel/tracing
mkdir -p "$dir"

is_dax() {
    python3 -c '
import ctypes, platform, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
nr = {"x86_64": 332, "aarch64": 291}[platform.machine()]
buf = ctypes.create_string_buffer(256)
assert libc.syscall(nr, -100, sys.argv[1].encode(), 0, 0x7FF, buf) == 0, ctypes.get_errno()
attributes, = struct.unpack_from("<Q", buf.raw, 8)
mask, = struct.unpack_from("<Q", buf.raw, 56)
assert mask & 0x200000, "the kernel does not report DAX"
print("dax" if attributes & 0x200000 else "page-cache", "ino", struct.unpack_from("<Q", buf.raw, 32)[0])
' "$1"
}

# Regular files under a directory, once per inode, by whether they are DAX (statx by path); with
# --detail, what the page-cache ones have in common, DAX ones beside; with --fsync, each
# page-cache one fsynced.
count_dax() {
    python3 -c '
import collections, ctypes, os, platform, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
nr = {"x86_64": 332, "aarch64": 291}[platform.machine()]
root, flags = sys.argv[1], sys.argv[2:]
seen, kinds, inos = set(), {True: collections.Counter(), False: collections.Counter()}, []
for d, _, names in os.walk(root):
    for n in names:
        p = os.path.join(d, n)
        buf = ctypes.create_string_buffer(256)
        assert libc.syscall(nr, -100, p.encode(), 0x100, 0x7FF, buf) == 0, ctypes.get_errno()
        mode, = struct.unpack_from("<H", buf.raw, 28)
        ino, = struct.unpack_from("<Q", buf.raw, 32)
        if mode & 0o170000 != 0o100000 or ino in seen:
            continue
        seen.add(ino)
        dax = bool(struct.unpack_from("<Q", buf.raw, 8)[0] & 0x200000)
        nlink, = struct.unpack_from("<I", buf.raw, 16)
        size, = struct.unpack_from("<Q", buf.raw, 40)
        # struct statx_timestamp {s64 tv_sec; u32 tv_nsec; s32 pad}: atime 64, mtime 112.
        atime, = struct.unpack_from("<q", buf.raw, 64)
        mtime, = struct.unpack_from("<q", buf.raw, 112)
        ext = os.path.splitext(n)[1] or ("(fingerprint)" if "/.fingerprint/" in p else "(none)")
        c = kinds[dax]
        c["files"] += 1
        c["ext " + ext] += 1
        c["nlink>1"] += nlink > 1
        c["read after written"] += atime > mtime
        c["empty"] += size == 0
        if "--inos" in flags:
            inos.append("%d %s %s" % (ino, "dax" if dax else "page-cache", p))
        if "--fsync" in flags and not dax:
            fd = os.open(p, os.O_RDONLY)
            os.fsync(fd)
            os.close(fd)
print("  %s: %d DAX, %d page-cache inodes" % (root, kinds[True]["files"], kinds[False]["files"]))
if "--inos" in flags:
    open(flags[flags.index("--inos") + 1], "w").write("\n".join(inos) + "\n")
if "--detail" in flags:
    for dax, name in ((False, "page-cache"), (True, "DAX")):
        top = ", ".join("%s %d" % kv for kv in kinds[dax].most_common(14) if kv[0] != "files")
        print("    %s: %s" % (name, top))
' "$@"
}

fadvise_dontneed() {
    python3 -c '
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.fsync(fd)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(fd)' "$1"
}

grep -E ' (cladefs_vfs_evict_inode|cladefs_dax_set_inode|cladefs_dax_evict)$' /proc/kallsyms
sudo -n sh -c "echo > $tracing/kprobe_events
echo 'p:probe/evict cladefs_vfs_evict_inode' >> $tracing/kprobe_events
echo 'r:probe/set_inode cladefs_dax_set_inode ret=\$retval:u8' >> $tracing/kprobe_events
echo 'p:probe/write_inode cladefs_vfs_write_inode' >> $tracing/kprobe_events
echo 8192 > $tracing/buffer_size_kb
echo 1 > $tracing/events/probe/enable
echo > $tracing/trace"
# Each inode iput as a drop prunes its dentries: its state bits (6.18's enum inode_state_flags_t:
# I_DIRTY_* 0x70, I_REFERENCED 0x400, I_LINKABLE 0x800, I_DIRTY_TIME 0x1000), references and pages.
# Needs the kernel's BTF and CONFIG_PROBE_EVENTS_BTF_ARGS.
iput_probe=yes
sudo -n sh -c "echo 'p:why/iput iput ino=inode->i_ino:u64 state=inode->i_state:x32 count=inode->i_count.counter:s32 pages=inode->i_data.nrpages:u64 xa_head=inode->i_data.i_pages.xa_head:x64' >> $tracing/kprobe_events" ||
    { iput_probe=; echo "no BTF field access in kprobes: no iput states"; }
# What the inode shrinker sees of each inode it walks: inode_lru_isolate(item = &inode->i_lru, ...),
# struct inode's offsets read from the kernel's BTF, fetched relative to item.
isolate_args=$(sudo -n python3 -c '
import struct
b = open("/sys/kernel/btf/vmlinux", "rb").read()
magic, _, _, hdr, toff, tlen, soff, _ = struct.unpack_from("<HBBIIIII", b, 0)
assert magic == 0xEB9F
strs = hdr + soff
def name(off):
    return b[strs + off:b.index(b"\0", strs + off)].decode()
types, p, end = [None], hdr + toff, hdr + toff + tlen
fixed = {1: 4, 2: 0, 3: 12, 7: 0, 8: 0, 9: 0, 10: 0, 11: 0, 12: 0, 14: 4, 16: 0, 17: 4, 18: 0}
per = {4: 12, 5: 12, 6: 8, 13: 8, 15: 12, 19: 12}
while p < end:
    noff, info, size = struct.unpack_from("<III", b, p)
    p += 12
    kind, vlen, kflag = (info >> 24) & 0x1F, info & 0xFFFF, info >> 31
    members = []
    if kind in (4, 5):
        for i in range(vlen):
            mn, mt, mo = struct.unpack_from("<III", b, p + 12 * i)
            members.append((name(mn), mt, (mo & 0xFFFFFF) if kflag else mo))
    p += per[kind] * vlen if kind in per else fixed[kind]
    types.append((kind, name(noff), size, members))
def resolve(t):
    while types[t][0] in (8, 9, 10, 11, 18):
        t = types[t][2]
    return t
def find(t, field):
    for mn, mt, mo in types[resolve(t)][3]:
        if mn == field:
            return mo, mt
        if not mn and types[resolve(mt)][0] in (4, 5):
            hit = find(mt, field)
            if hit:
                return mo + hit[0], hit[1]
def offset(t, path):
    bits = 0
    for field in path.split("."):
        mo, t = find(t, field)
        bits += mo
    return bits // 8
inode = next(i for i, ty in enumerate(types) if ty and ty[0] == 4 and ty[1] == "inode")
lru = offset(inode, "i_lru")
fields = (("ino", "i_ino", "u64"), ("state", "i_state", "x32"), ("count", "i_count", "s32"),
          ("pages", "i_data.nrpages", "u64"), ("xa_head", "i_data.i_pages.xa_head", "x64"))
print(" ".join("%s=%+d($arg1):%s" % (n, offset(inode, f) - lru, ty) for n, f, ty in fields))
') && sudo -n sh -c "echo 'p:why/isolate inode_lru_isolate $isolate_args' >> $tracing/kprobe_events" &&
    echo "isolate probe: $isolate_args" || echo "no inode_lru_isolate probe"
# Which memory cgroup's inode list each inode joins as its last reference goes (list_lru_add, item
# = &inode->i_lru for the inode list), whether it joins, and which cgroups' lists the shrinker walks.
ino_from_item=${isolate_args%% *}
ino_from_item=${ino_from_item/\$arg1/\$arg2}
sudo -n sh -c "echo 'p:why/lru_add list_lru_add lru=\$arg1:x64 $ino_from_item memcg=\$arg4:x64' >> $tracing/kprobe_events
echo 'r:why/lru_add_ret list_lru_add ret=\$retval:u8' >> $tracing/kprobe_events
echo 'p:why/prune prune_icache_sb sb=\$arg1:x64 memcg=sc->memcg:x64 scan=sc->nr_to_scan:u64' >> $tracing/kprobe_events
echo 'p:why/count super_cache_count memcg=sc->memcg:x64' >> $tracing/kprobe_events
echo 'r:why/count_ret super_cache_count ret=\$retval:s64' >> $tracing/kprobe_events
echo 'r:why/fs_objects_ret cladefs_vfs_nr_cached_objects ret=\$retval:s64' >> $tracing/kprobe_events
echo 'r:why/fs_freed_ret cladefs_vfs_free_cached_objects ret=\$retval:s64' >> $tracing/kprobe_events" ||
    echo "no list_lru_add or prune_icache_sb probes"
trap 'sudo -n sh -c "echo 0 > $tracing/events/probe/enable; echo > $tracing/kprobe_events"' EXIT

events() {
    echo "  evicted $(sudo -n grep -c ' evict:' $tracing/trace || true)," \
        "set_inode true $(sudo -n grep -c 'set_inode:.*ret=1' $tracing/trace || true)," \
        "false $(sudo -n grep -c 'set_inode:.*ret=0' $tracing/trace || true)," \
        "write_inode $(sudo -n grep -c ' write_inode:' $tracing/trace || true)"
    sudo -n sh -c "echo > $tracing/trace"
}

step() {
    echo "--- $1"
}

step "A: a new file, written, read, fsynced, its pages dropped, drop_caches 2"
a=$dir/a
head -c 8M /dev/urandom > "$a"
cat "$a" > /dev/null
is_dax "$a"
fadvise_dontneed "$a"
sudo -n sh -c 'echo 2 > /proc/sys/vm/drop_caches'
events
is_dax "$a"
events

step "B: a new file and a hard link to it, synced, drop_caches 3"
b=$dir/b
head -c 8M /dev/urandom > "$b"
ln -f "$b" "$b.link"
cat "$b.link" > /dev/null
sync
sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
events
is_dax "$b"
is_dax "$b.link"
events

step "C: as the measurement did: written, synced, fadvised, drop_caches 3"
c=$dir/c
head -c 8M /dev/urandom > "$c"
sync
fadvise_dontneed "$c"
sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
events
is_dax "$c"
events

# Step V: which way of reading a file read in from sealed extents leaves its inode one DAX entry,
# at index 0, as the xarray's head, a value entry (board 13582): each such file read so, closed,
# its dentry pruned, the iput shows its mapping's pages and head.
step "V: blockers: the mapping each way of reading a DAX file leaves"
mkdir -p "$dir/blockers"
blocker() {
    local name=$1 size=$2 how=$3 file=$dir/blockers/$1
    head -c "$size" /dev/urandom > "$file"
    fadvise_dontneed "$file"
    sudo -n sh -c 'echo 2 > /proc/sys/vm/drop_caches'
    python3 -c '
import mmap, os, sys
path, how = sys.argv[1], sys.argv[2]
fd = os.open(path, os.O_RDWR if how == "mmap-rw" else os.O_RDONLY)
if how == "read":
    os.read(fd, 4096)
else:
    flags = mmap.MAP_PRIVATE if how == "mmap-private" else mmap.MAP_SHARED
    prot = mmap.PROT_READ | (mmap.PROT_WRITE if how == "mmap-rw" else 0)
    m = mmap.mmap(fd, 4096, flags=flags, prot=prot)
    m[0]
    m.close()
os.close(fd)' "$file" "$how"
    echo "$name $(is_dax "$file")" >> "$dir/blockers.txt"
}
: > "$dir/blockers.txt"
[[ -n $iput_probe ]] && sudo -n sh -c "echo > $tracing/trace; echo 1 > $tracing/events/why/iput/enable"
blocker 4k-mmap-rw 4096 mmap-rw
blocker 4k-mmap-ro 4096 mmap-ro
blocker 4k-mmap-private 4096 mmap-private
blocker 4k-read 4096 read
blocker 64k-mmap-ro 65536 mmap-ro
blocker 1m-mmap-ro 1048576 mmap-ro
blocker 3m-mmap-ro 3145728 mmap-ro
sudo -n sh -c 'echo 2 > /proc/sys/vm/drop_caches'
[[ -n $iput_probe ]] && sudo -n sh -c "echo 0 > $tracing/events/why/iput/enable"
# shellcheck disable=SC2024 # root reads the trace; the file written is ours
sudo -n cat "$tracing/trace" > "$dir/blocker-trace"
python3 -c '
import re, sys
iputs = {}
for line in open(sys.argv[2]):
    m = re.search(r" iput: .*ino=(\d+) state=(0x[0-9a-f]+) count=(-?\d+) pages=(\d+) xa_head=(0x[0-9a-f]+)", line)
    if m:
        head = int(m[5], 16)
        kind = "empty" if not head else ("value entry" if head & 1 else ("node" if head & 3 == 2 else "pointer"))
        iputs[int(m[1])] = "count %s pages %s head %s" % (m[3], m[4], kind)
for line in open(sys.argv[1]):
    name, dax, _, ino = line.split()
    print("  %-16s %-10s ino %s: %s" % (name, dax, ino, iputs.get(int(ino), "no iput in the drop")))
' "$dir/blockers.txt" "$dir/blocker-trace"
events

# Each way a build makes a file, alone: made, synced, drop_caches 3, read in again.
variant() {
    local name=$1 file=$2
    sync
    sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
    echo "  $name: $(is_dax "$file")"
}

step "F: ways of making a file"
mkdir -p "$dir/v"
head -c 8M /dev/urandom > "$dir/v/renamed.tmp"
mkdir -p "$dir/v/into"
mv "$dir/v/renamed.tmp" "$dir/v/into/renamed"
variant "renamed into another directory" "$dir/v/into/renamed"
head -c 8M /dev/urandom > "$dir/v/touched"
touch -d 2020-01-01 "$dir/v/touched"
variant "times set (utimensat)" "$dir/v/touched"
truncate -s 8M "$dir/v/grown"
dd if=/dev/urandom of="$dir/v/grown" bs=1M count=8 conv=notrunc status=none
variant "grown by truncate, then written" "$dir/v/grown"
python3 -c '
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
fd = os.open(os.path.dirname(sys.argv[1]), os.O_TMPFILE | os.O_WRONLY, 0o644)
os.write(fd, os.urandom(8 << 20))
# os.link calls link(2), which does not follow /proc/self/fd: linkat, AT_SYMLINK_FOLLOW.
src = ("/proc/self/fd/%d" % fd).encode()
assert libc.linkat(-100, src, -100, sys.argv[1].encode(), 0x400) == 0, ctypes.get_errno()
os.close(fd)' "$dir/v/tmpfile"
variant "O_TMPFILE, then linked" "$dir/v/tmpfile"
python3 -c '
import mmap, os, sys
fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 8 << 20)
m = mmap.mmap(fd, 8 << 20)
m[:] = os.urandom(8 << 20)
m.flush()
m.close()
os.close(fd)' "$dir/v/mapped"
variant "written through a shared mapping" "$dir/v/mapped"
head -c 64 /dev/urandom > "$dir/v/small"
variant "64 bytes" "$dir/v/small"
head -c 3000 /dev/urandom > "$dir/v/kib"
variant "3000 bytes" "$dir/v/kib"
events

if [[ -n $crate ]]; then
    step "E: a cargo build's outputs ($crate), synced, drop_caches 3"
    rm -rf "$dir/target"
    cg=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
    echo "  cgroup $cg: memory.high $(cat "$cg"/memory.high) memory.max $(cat "$cg"/memory.max) memory.low $(cat "$cg"/memory.low)"
    reclaim_stats() {
        echo "  $1: cgroup $(grep -E '^(file|anon|pgscan|pgsteal|workingset_refault_file|workingset_nodereclaim) ' "$cg"/memory.stat | tr '\n' ' ')"
        echo "  $1: guest $(grep -E '^(workingset_nodes|pgsteal_kswapd|pgsteal_direct|pgscan_kswapd|pgscan_direct) ' /proc/vmstat | tr '\n' ' ')"
    }
    reclaim_stats "before the build"
    CARGO_TARGET_DIR=$dir/target CARGO_BUILD_JOBS=4 cargo build -q -p "$crate"
    reclaim_stats "after the build"
    count_dax "$dir/target"
    sync
    events
    [[ -n $iput_probe ]] && sudo -n sh -c "echo 1 > $tracing/events/why/enable"
    sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
    [[ -n $iput_probe ]] && sudo -n sh -c "echo 0 > $tracing/events/why/enable"
    # shellcheck disable=SC2024 # root reads the trace; the file written is ours
    sudo -n cat "$tracing/trace" > "$dir/drop-trace"
    events
    count_dax "$dir/target" --detail --inos "$dir/inos"
    events
    python3 -c '
import collections, re, sys
inos = {}
for line in open(sys.argv[1]):
    ino, kind, path = line.split(" ", 2)
    inos[int(ino)] = (kind, path.strip())
iputs = collections.defaultdict(list)
for line in open(sys.argv[2]):
    m = re.search(r" iput: .*ino=(\d+) state=(0x[0-9a-f]+) count=(-?\d+) pages=(\d+)", line)
    if m:
        iputs[int(m[1])].append((m[2], int(m[3]), int(m[4])))
for kind in ("page-cache", "dax"):
    group = [i for i, (k, _) in inos.items() if k == kind]
    seen = collections.Counter()
    for i in group:
        for state, count, pages in iputs.get(i, [("no iput", None, None)]):
            seen["state %s count %s pages>0 %s" % (state, count, pages and pages > 0)] += 1
    print("  %s (%d inodes) at the drop: %s" % (kind, len(group), "; ".join("%s: %d" % kv for kv in seen.most_common(8))))
    for i in group[:3]:
        print("    e.g. %d %s %s" % (i, inos[i][1], iputs.get(i)))
isolates = collections.defaultdict(list)
for line in open(sys.argv[2]):
    m = re.search(r" isolate: .*ino=(\d+) state=(0x[0-9a-f]+) count=(-?\d+) pages=(\d+) xa_head=(0x[0-9a-f]+)", line)
    if m:
        isolates[int(m[1])].append("state %s count %s pages %s xa_head %s" % (m[2], m[3], m[4], "0" if m[5] == "0x0" else "set"))
for kind in ("page-cache", "dax"):
    group = [i for i, (k, _) in inos.items() if k == kind]
    seen = collections.Counter()
    for i in group:
        seen[" -> ".join(isolates.get(i, ["never walked"]))] += 1
    print("  %s: what the inode shrinker saw: %s" % (kind, "; ".join("%s: %d" % kv for kv in seen.most_common(6))))
print("  isolate events in all: %d" % sum(len(v) for v in isolates.values()))
# The inodes the walk met most: a head inode it retries (LRU_RETRY) takes the whole budget.
heads = collections.Counter()
last = {}
for line in open(sys.argv[2]):
    m = re.search(r" isolate: .*ino=(\d+) state=(0x[0-9a-f]+) count=(-?\d+) pages=(\d+) xa_head=(0x[0-9a-f]+)", line)
    if m:
        heads[int(m[1])] += 1
        last[int(m[1])] = (m[2], m[3], m[4], m[5], "value entry" if int(m[5], 16) & 1 else ("node" if int(m[5], 16) & 3 == 2 else "pointer or empty"))
for ino, n in heads.most_common(5):
    print("    walked %d times: ino %d state %s count %s pages %s xa_head %s (%s)" % ((n, ino) + last[ino]))
adds, pending, prunes = collections.defaultdict(list), {}, collections.Counter()
for line in open(sys.argv[2]):
    cpu = re.search(r"\[(\d+)\]", line)
    m = re.search(r" lru_add: .*lru=(0x[0-9a-f]+) ino=(\d+) memcg=(0x[0-9a-f]+)", line)
    if m:
        pending[cpu[1]] = (int(m[2]), m[1], m[3])
        continue
    m = re.search(r" lru_add_ret: .*ret=(\d+)", line)
    if m and cpu[1] in pending:
        ino, lru, memcg = pending.pop(cpu[1])
        adds[ino].append("lru %s memcg %s added %s" % (lru[-6:], memcg, m[1]))
        continue
    m = re.search(r" prune: .*memcg=(0x[0-9a-f]+) scan=(\d+)", line)
    if m:
        prunes[m[1]] += 1
for kind in ("page-cache", "dax"):
    group = [i for i, (k, _) in inos.items() if k == kind]
    seen = collections.Counter(" -> ".join(adds.get(i, ["no list_lru_add"])) for i in group)
    print("  %s: joined the LRU: %s" % (kind, "; ".join("%s: %d" % kv for kv in seen.most_common(6))))
print("  prune_icache_sb calls by memcg: %s" % ", ".join("%s: %d" % kv for kv in prunes.most_common(12)))
lru_memcg = collections.Counter(a.split()[3] for v in adds.values() for a in v).most_common(1)[0][0]
scans, counts, fs_objects, fs_freed, pend = [], [], [], [], {}
for line in open(sys.argv[2]):
    cpu = re.search(r"\[(\d+)\]", line)
    if not cpu:
        continue
    cpu = cpu[1]
    m = re.search(r" prune: .*memcg=(0x[0-9a-f]+) scan=(\d+)", line)
    if m and m[1] == lru_memcg:
        scans.append(int(m[2]))
    m = re.search(r" count: .*memcg=(0x[0-9a-f]+)", line)
    if m:
        pend[cpu] = m[1]
    m = re.search(r" count_ret: .*ret=(-?\d+)", line)
    if m and pend.pop(cpu, None) == lru_memcg:
        counts.append(int(m[1]))
    m = re.search(r" fs_objects_ret: .*ret=(-?\d+)", line)
    if m:
        fs_objects.append(int(m[1]))
    m = re.search(r" fs_freed_ret: .*ret=(-?\d+)", line)
    if m:
        fs_freed.append(int(m[1]))
print("  the memcg of the inodes %s: prune_icache_sb nr_to_scan %s" % (lru_memcg, scans[:40]))
print("  its super_cache_count %s" % counts[:20])
print("  cladefs nr_cached_objects: %d calls, first %s, last %s" % (len(fs_objects), fs_objects[:6], fs_objects[-6:]))
print("  cladefs free_cached_objects: %d calls, freed in all %d, first %s" % (len(fs_freed), sum(x for x in fs_freed if x > 0), fs_freed[:8]))
' "$dir/inos" "$dir/drop-trace"
    for round in 1 2 3 4 5; do
        sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
        echo "  drop $round more: $(count_dax "$dir/target")"
    done
    echo "  shadow entries (cachestat nr_evicted) left in each kind's mappings:"
    sudo -n python3 -c '
import collections, ctypes, os, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
sums = collections.defaultdict(lambda: [0, 0, 0])
for line in open(sys.argv[1]):
    ino, kind, path = line.split(" ", 2)
    fd = os.open(path.strip(), os.O_RDONLY | os.O_NOATIME)
    rng = ctypes.create_string_buffer(struct.pack("<QQ", 0, 0))
    cs = ctypes.create_string_buffer(40)
    assert libc.syscall(451, fd, rng, cs, 0) == 0, ctypes.get_errno()
    os.close(fd)
    cache, dirty, wb, evicted, recent = struct.unpack("<5Q", cs.raw)
    s = sums[kind]
    s[0] += 1
    s[1] += evicted > 0
    s[2] += evicted
for kind, (n, with_shadows, shadows) in sums.items():
    print("    %s: %d of %d inodes hold shadow entries, %d in all" % (kind, with_shadows, n, shadows))
' "$dir/inos"
fi

if [[ -n $old ]]; then
    step "D: a file created in an earlier mount, read in"
    is_dax "$old"
    events
fi

grep -E '^(dax_unsealed_inodes|dax_seals) ' /proc/fs/cladefs/rootfs || sudo -n grep -E '^(dax_unsealed_inodes|dax_seals) ' /proc/fs/cladefs/rootfs
rm -rf "$a" "$b" "$b.link" "$c" "$dir/target" "$dir/v" "$dir/inos" "$dir/drop-trace" "$dir/blockers" "$dir/blockers.txt" "$dir/blocker-trace"
