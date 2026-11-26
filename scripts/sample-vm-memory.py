#!/usr/bin/env python3
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""What hotbed's build VMs hold, sampled every INTERVAL seconds (board 13454, doc-30).

Each sample, for each HOST (over ssh): the host's available memory, swap in use and memory
pressure; for each Firecracker VM, its workspace, the guest kernel it booted (by the kernel
artifact's checksum, named with --kernel SHA16=NAME), its guest RAM, and the host RSS and swap it
holds. For each running hotbed command on those hosts (`hotbed inspect cgroup`), its guest
cgroup's file memory (less shmem, which reclaim can't drop), shmem and anon, joined to its VM by
workspace. Appends one JSON object a sample to
OUT and prints a summary line a sample, VMs grouped by kernel.

Usage: scripts/sample-vm-memory.py --out FILE [--interval S] [--count N]
                                   [--kernel SHA16=NAME ...] HOST...
"""

import argparse
import json
import subprocess
import sys
import time

REMOTE = r"""
import glob, json, os
def kib(path, keys):
    out = {}
    for line in open(path):
        k, _, v = line.partition(":")
        if k in keys:
            out[k] = int(v.split()[0])
    return out
mem = kib("/proc/meminfo", {"MemAvailable", "SwapTotal", "SwapFree"})
psi = open("/proc/pressure/memory").readline().split()
vms = []
for vm in glob.glob("/var/lib/hotbed/worker/vms/*/vm.json"):
    ws = vm.split("/")[-2]
    try:
        cfg = json.load(open(vm))
    except (OSError, ValueError):
        continue
    kernel = cfg["boot-source"]["kernel_image_path"].split("/")[-2]
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
        except OSError:
            continue
        if cmd and cmd[0].endswith(b"firecracker") and vm.encode() in cmd:
            st = kib(f"/proc/{pid}/status", {"VmRSS", "RssAnon", "RssShmem", "VmSwap"})
            vms.append({"ws": ws, "kernel": kernel, "guest_mib": cfg["machine-config"]["mem_size_mib"],
                        "rss_mib": st.get("VmRSS", 0) // 1024, "anon_mib": st.get("RssAnon", 0) // 1024,
                        "shmem_mib": st.get("RssShmem", 0) // 1024, "swap_mib": st.get("VmSwap", 0) // 1024})
print(json.dumps({"avail_mib": mem["MemAvailable"] // 1024,
                  "swap_used_mib": (mem["SwapTotal"] - mem["SwapFree"]) // 1024,
                  "psi_some_avg10": float(psi[1].split("=")[1]), "vms": vms}))
"""


def hotbed_json(*args):
    out = subprocess.run(["hotbed", *args, "--json"], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)["data"]


def sample(hosts, kernels):
    nodes = {n["id"]: n["hostname"] for n in hotbed_json("nodes")["nodes"]}
    running = [c for c in hotbed_json("ps", "--all")["commands"]
               if c.get("state") == "COMMAND_STATE_RUNNING" and nodes.get(c.get("nodeId")) in hosts]
    guest = {}
    for c in running:
        try:
            st = hotbed_json("inspect", "cgroup", c["id"])["cgroupStats"]
        except (subprocess.CalledProcessError, KeyError, ValueError):
            continue  # Ended between the listing and the inspection: no guest numbers this sample.
        # File memory less shmem (tmpfs, /dev/shm): the page cache a reclaim can drop.
        shmem = int(st.get("memoryShmemBytes", 0))
        guest[c["workspaceId"]] = {"cmd": c["id"], "guest_file_mib": (int(st.get("memoryFileBytes", 0)) - shmem) >> 20,
                                   "guest_shmem_mib": shmem >> 20,
                                   "guest_anon_mib": int(st.get("memoryAnonBytes", 0)) >> 20}
    record = {"t": int(time.time()), "hosts": {}}
    for host in hosts:
        # The script on stdin: ssh hands its arguments to the remote shell, which would split it.
        out = subprocess.run(["ssh", host, "python3", "-"], input=REMOTE, capture_output=True, text=True)
        if out.returncode:
            record["hosts"][host] = {"error": out.stderr.strip()[-200:]}
            continue
        h = json.loads(out.stdout)
        for vm in h["vms"]:
            vm["kernel"] = kernels.get(vm["kernel"], vm["kernel"])
            vm.update(guest.get(vm["ws"], {}))
        record["hosts"][host] = h
    return record


def summary(record):
    parts = [time.strftime("%H:%M:%S", time.localtime(record["t"]))]
    for host, h in record["hosts"].items():
        if "error" in h:
            parts.append(f"{host}: {h['error']}")
            continue
        groups = {}
        for vm in h["vms"]:
            g = groups.setdefault(vm["kernel"], {"n": 0, "held": 0, "file": 0, "shmem": 0, "anon": 0, "with_guest": 0})
            g["n"] += 1
            g["held"] += vm["rss_mib"] + vm["swap_mib"]
            if "guest_file_mib" in vm:
                g["with_guest"] += 1
                g["file"] += vm["guest_file_mib"]
                g["shmem"] += vm.get("guest_shmem_mib", 0)
                g["anon"] += vm["guest_anon_mib"]
        by = "; ".join(f"{k}: {g['n']} VMs hold {g['held'] / 1024:.1f} GiB, guest file {g['file'] / 1024:.1f}"
                       f" shmem {g['shmem'] / 1024:.1f} anon {g['anon'] / 1024:.1f} GiB ({g['with_guest']} with a command)"
                       for k, g in sorted(groups.items()))
        parts.append(f"{host}: avail {h['avail_mib'] / 1024:.1f} GiB swap {h['swap_used_mib'] / 1024:.1f} GiB"
                     f" psi {h['psi_some_avg10']:.1f}% | {by or 'no VMs'}")
    return " || ".join(parts)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", required=True)
    p.add_argument("--interval", type=int, default=300)
    p.add_argument("--count", type=int, default=0, help="samples to take (0: until stopped)")
    p.add_argument("--kernel", action="append", default=[], help="SHA16=NAME")
    p.add_argument("hosts", nargs="+")
    a = p.parse_args()
    kernels = dict(k.split("=", 1) for k in a.kernel)
    taken = 0
    while True:
        record = sample(a.hosts, kernels)
        with open(a.out, "a") as f:
            f.write(json.dumps(record) + "\n")
        print(summary(record), flush=True)
        taken += 1
        if a.count and taken >= a.count:
            return
        time.sleep(a.interval)


if __name__ == "__main__":
    sys.exit(main())
