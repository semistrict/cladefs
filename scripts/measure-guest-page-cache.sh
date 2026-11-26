#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# What a cladefs guest's page cache holds once a build has written fresh outputs, and what each way
# of dropping it frees (doc-30). Run inside a guest whose root is a cladefs mount, with passwordless
# sudo; sample the VM process's RSS on the host beside it, by the phase times this prints.
#
#   measure-guest-page-cache.sh OUT CRATE TOUCH
#
# Builds CRATE into a new target directory under OUT (every output a file created in this mount),
# then in turn: syncs; drops the outputs' cached pages with POSIX_FADV_DONTNEED (their inodes stay,
# in the page cache); drops every cache (drop_caches 3, which evicts the inodes, so they read in
# again as DAX); compacts free memory. After each it holds 40 s for free page reporting, and after
# the build and each drop it times a rebuild of CRATE with TOUCH touched.

set -euo pipefail

out=${1:?usage: measure-guest-page-cache.sh OUT CRATE TOUCH}
crate=${2:?usage: measure-guest-page-cache.sh OUT CRATE TOUCH}
touch_file=${3:?usage: measure-guest-page-cache.sh OUT CRATE TOUCH}
target=$out/target
mkdir -p "$target"
export CARGO_TARGET_DIR=$target CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}

sudo -n true

# Per regular file under each directory given: S_DAX or not (statx), and its page-cache pages
# (cachestat), summed; with --by DEPTH, summed per path prefix of DEPTH components; with --drop,
# POSIX_FADV_DONTNEED on each non-DAX file first.
files_py=$(cat <<'EOF'
import ctypes, os, platform, struct, sys

libc = ctypes.CDLL(None, use_errno=True)
SYS_STATX = {"x86_64": 332, "aarch64": 291}[platform.machine()]
SYS_CACHESTAT = 451
AT_EMPTY_PATH = 0x1000
STATX_ATTR_DAX = 0x00200000
page = os.sysconf("SC_PAGE_SIZE")

def is_dax(fd):
    buf = ctypes.create_string_buffer(256)
    if libc.syscall(SYS_STATX, fd, b"", AT_EMPTY_PATH, 0x7FF, buf) != 0:
        raise OSError(ctypes.get_errno(), "statx")
    return bool(struct.unpack_from("<Q", buf.raw, 8)[0] & STATX_ATTR_DAX)

def cachestat(fd):
    rng = ctypes.create_string_buffer(struct.pack("<QQ", 0, 0))
    cs = ctypes.create_string_buffer(40)
    if libc.syscall(SYS_CACHESTAT, fd, rng, cs, 0) != 0:
        raise OSError(ctypes.get_errno(), "cachestat")
    return struct.unpack("<5Q", cs.raw)

args = sys.argv[1:]
drop = "--drop" in args
by = None
if "--by" in args:
    i = args.index("--by")
    by = int(args[i + 1])
    del args[i:i + 2]
roots = [a for a in args if a != "--drop"]
keys = ("files", "bytes", "dax_files", "dax_bytes", "cached", "dirty", "writeback")
sums = {}
seen = set()
for root in roots:
    dev = os.lstat(root).st_dev
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames
                       if os.lstat(os.path.join(dirpath, d)).st_dev == dev]
        for name in filenames:
            path = os.path.join(dirpath, name)
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_NOATIME)
            except OSError:
                continue
            try:
                st = os.fstat(fd)
                # Once per inode: cargo hard-links its outputs (deps/ and the profile's directory).
                if not (st.st_mode & 0o170000 == 0o100000) or (st.st_dev, st.st_ino) in seen:
                    continue
                seen.add((st.st_dev, st.st_ino))
                key = "/".join(path.split("/")[:by + 1]) if by else root
                s = sums.setdefault(key, dict.fromkeys(keys, 0))
                s["files"] += 1
                s["bytes"] += st.st_size
                if is_dax(fd):
                    s["dax_files"] += 1
                    s["dax_bytes"] += st.st_size
                    continue
                if drop:
                    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
                cached, dirty, writeback, _, _ = cachestat(fd)
                s["cached"] += cached * page
                s["dirty"] += dirty * page
                s["writeback"] += writeback * page
            finally:
                os.close(fd)
mib = lambda n: f"{n / 2**20:.1f}"
for key, s in sorted(sums.items(), key=lambda kv: -kv[1]["cached"]):
    if by and s["cached"] < 2**20:
        continue
    print(f"files {key}: {s['files']} files {mib(s['bytes'])} MiB, dax {s['dax_files']} files "
          f"{mib(s['dax_bytes'])} MiB, cached {mib(s['cached'])} MiB dirty {mib(s['dirty'])} MiB "
          f"writeback {mib(s['writeback'])} MiB")
EOF
)
# As root: cachestat needs write access, and O_NOATIME ownership, to each file.
files() { sudo -n python3 -c "$files_py" "$@"; }

meminfo() {
    awk '/^(MemFree|Cached|Dirty|Writeback|Mapped|Shmem|AnonPages|Active\(file\)|Inactive\(file\)|SReclaimable):/ {
        printf "%s %.1f MiB  ", $1, $2 / 1024 } END { print "" }' /proc/meminfo
}

# Free pages per order, summed over zones, in MiB: what free page reporting (at its order, from
# /sys/module/page_reporting) could hand the host.
buddy() {
    awk -v page="$(getconf PAGESIZE)" '{ for (o = 0; o + 5 <= NF; o++) free[o] += $(o + 5) * 2 ^ o * page }
        END { for (o = 0; o < 11; o++) printf "o%d %.0f  ", o, free[o] / 2^20; print "MiB" }' \
        /proc/buddyinfo
}

phase() {
    echo "=== $1 $(date +%s.%N)"
    echo "mem $(meminfo)"
    echo "buddy $(buddy)"
    files "$target"
    for t in /proc/fs/cladefs/*; do
        { sudo -n grep -E '^(dax_unsealed_inodes|dax_private_pages|dax_seals|window_free_bytes) ' "$t" || true; } |
            tr '\n' ' ' | sed "s|^|cladefs $(basename "$t"): |"
        echo
    done
}

# Holds 40 s, printing free memory every 2 s, for free page reporting to hand the host what was
# freed (its delay is 2 s a pass).
hold() {
    echo "--- hold $1 $(date +%s.%N)"
    vmstat -t -S M 2 21
}

rebuild() {
    touch "$touch_file"
    local start end
    start=$(date +%s.%N)
    cargo build -q -p "$crate"
    end=$(date +%s.%N)
    echo "--- rebuild $1 $(awk -v s="$start" -v e="$end" 'BEGIN { printf "%.1f", e - s }') s"
}

echo "uname $(uname -r) page_reporting_order $(cat /sys/module/page_reporting/parameters/page_reporting_order)"
grep -E ' (cladefs|tmpfs) ' /proc/mounts
echo "dirty_background_bytes $(cat /proc/sys/vm/dirty_background_bytes) dirty_expire_centisecs $(cat /proc/sys/vm/dirty_expire_centisecs)"
phase start

start=$(date +%s.%N)
cargo build -q -p "$crate"
echo "--- build $(awk -v s="$start" -v e="$(date +%s.%N)" 'BEGIN { printf "%.1f", e - s }') s"
phase built
rebuild warm
phase rebuilt-warm

sync
phase synced
echo "--- what the whole root's cache holds, by path"
# Opening every file reads its inode in, so the walk's own memory shows in the next phase.
timeout 300 sudo -n python3 -c "$files_py" --by 4 / || echo "walk exit $?"
phase walked
hold idle

files --drop "$target" >/dev/null
phase fadvised
hold fadvised
rebuild after-fadvise
phase rebuilt-after-fadvise

sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
phase dropped
hold dropped

sudo sh -c 'echo 1 > /proc/sys/vm/compact_memory'
phase compacted
hold compacted

rebuild after-drop
phase rebuilt-after-drop
echo "=== end $(date +%s.%N)"
