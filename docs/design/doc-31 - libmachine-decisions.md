---
id: doc-31
title: libmachine's decisions, as it made them
type: other
created_date: '2026-11-14 20:10'
updated_date: '2026-11-26 12:00'
---
# libmachine's decisions, as it made them

**Removed on 2026-11-27 (board 14519).** Most of what these entries decided no longer exists: the KVM and x86-64 backend, the PC platform (ACPI, PVH, the 16550 on I/O ports), suspension, the `Sandbox` layer, the Python bindings, the network stack's save and restore and the patched smoltcp, and machined's calls for stops, watches, execs, files, snapshots, suspensions and balloon targets. The entries stay as the history of why they were made, in the order they were; what is true of the code now is doc-30's.

libmachine's OPEN_QUESTIONS.md at cbff5b1 (semistrict/libmachine), kept as the record of what it
decided before it came into this repo (board 13054); doc-30 is its architecture. Its rule was to
decide, act, and record each decision here for the user to overturn; in this repo the board and
AGENTS.md's rules hold, and new decisions go where cladefs's go.

## Decisions

- **Guest physical address map (x86_64), fixed for every machine, 2026-11-14:** RAM from 0 up to
  3 GiB; the 32-bit hole 3-4 GiB holds a 512 MiB PCI MMIO window for 32-bit BARs at
  `0xC000_0000-0xE000_0000`, the PCI ECAM (config space) at `0xE000_0000-0xF000_0000`, then the
  I/O APIC, local APIC and KVM's pages; RAM beyond 3 GiB continues at 4 GiB; 64-bit BARs (the
  cladefs DAX window, up to 64 GiB) come from a 1 TiB window at `0x100_0000_0000` (1 TiB). Fixed
  rather than computed from the RAM size, so a snapshot's windows keep their addresses whatever
  the RAM; RAM is therefore limited to under 1 TiB - 1 GiB. Every host CPU in use has at least
  41 physical address bits. Undo: the constants live in `arch/x86_64/layout.rs`.

- **Parallel work, 2026-11-14:** after board 13119 lands, three tasks run at once in their own
  worktrees: board 13111 (PCI bus and virtio-PCI), board 13113 (memory layout, RAM above 4 GiB) and board 13087
  (cladefs's guest kernel build). SMP (board 13109) waits until PCI has landed, because both restructure
  the run loop's device dispatch. Undo: stop a subagent; its branch is discarded.
- **Kick signal, board 13119:** stopping a running vCPU uses `SIGRTMIN` (glibc's first real-time
  signal free for applications), process-wide. An embedding application that also uses `SIGRTMIN`
  would conflict; making the signal configurable is easy if that comes up. Alternative considered:
  a helper thread per machine, which costs a thread and still needs a signal to leave `KVM_RUN`.
- **Python test assembler:** Keystone stays (it works, and is only a test dependency), despite
  being unmaintained since 2020 and, as far as known, GPLv2. Alternative: `llvm-mc` in a
  subprocess, changing only `tests/guest.py`.
- **Backlog remote operations:** turned back on now that `origin` exists, so Backlog sees task
  states across remote branches, which matters with several worktrees.

- **Extent device: extract a shared crate, don't port (board 13115, doc-2):** both cladefs's
  Firecracker fork and libmachine use one crate holding the protocol, request core, window logic
  and snapshot state; each VMM keeps a thin adapter. Why: the protocol is versioned with no
  migrations, so two drifting copies would break guests; the fork's code already splits along this
  line (`worker.rs::handle` takes no Firecracker types). The fork upgrades vm-memory to 0.18 (the
  user's call); instructions for the cladefs team are doc-3. Alternative: port ~3.7k lines into
  libmachine and re-port every cladefs change. Undo: port instead; the libmachine adapter (board 13067)
  would then hold the copied code.

- **Device memory only in the BAR windows (board 13113):** runtime memory slots may only map inside
  the 32-bit BAR window or the 64-bit window, never over RAM, the platform area or gaps.
  Alternative: anywhere outside RAM. Undo: the window check in `SlotTable::claim`.
- **Bare machines use the PC address map (board 13113):** `Machine::new` has the same layout and RAM
  limit as `boot_linux`. Alternative: contiguous RAM from 0. Undo: a layout per platform in
  `Vm::new`.
- **Memory slots are handles that may outlive the Machine (board 13113):** `Machine::memory_slots()`
  returns a handle usable from any thread (like `stop_handle()`); a `MemorySlot` owns its
  `HostMemory` (a lazily backed `MAP_NORESERVE` mapping) and holds the VM handle, removing its KVM
  slot when dropped. If removal fails, the memory is leaked rather than unmapped, so the guest can
  never reach memory the host reuses (reported on stderr until libmachine has logging).
  Alternatives: a borrowing lifetime (devices could not own slots on their own threads) or weak
  references. No constructor from a raw pointer yet: cladefs gets libmachine's pointer.

- **The shipped guest kernel is cladefs's PVH vmlinux, built by wrapping cladefs's own script
  (board 13087):** `scripts/build-cladefs-guest.sh` clones cladefs at a pinned commit (`355147cd`, kernel
  tag commit pinned too), runs cladefs's `build-guest-kernel.sh` unchanged, then merges
  `guest/cladefs-kernel.config` and rebuilds. PVH vmlinux rather than bzImage: it is what cladefs
  builds and boots without decompression. Wrapping rather than reimplementing keeps patches,
  config and Rust identical to cladefs's; the cost is a second partial build (an cladefs hook would
  remove it, see doc-3). Undo: add `bzImage` to the make target, or replace the wrapper.
- **cladefs kernel differs from cladefs's only in `CPU_FREQ`/`SCHED_MC_PRIO` off (board 13087):** a vCPU's
  frequency is the host's, and `amd_pstate` warned on every boot. Narrower alternative: only
  `X86_AMD_PSTATE` off. Undo: the fragment's last section.
- **Test init can print guest files (board 13087):** `libmachine_cat=PATH,...` on the command line
  prints files or lists directories before power-off, so tests can inspect the guest without an
  agent; `/sys` is in the initramfs for it.

- **Guests never get a PMU (board 13121):** KVM's virtual PMU is disabled VM-wide
  (`KVM_CAP_PMU_CAPABILITY`), which makes host Linux 5.18 a hard minimum (cladefs already needs
  6.1). Performance counters are a side channel and snapshot state; profiling inside a sandbox
  uses software events. Undo: drop `disable_pmu` in `backend/kvm.rs`, and expose the PMU as a
  CPU-model option if a use appears.

- **PCI platform (board 13111):** ECAM only (no 0xCF8/0xCFC ports: one exit per config access, where
  Linux would otherwise prefer the ports at two); bus 0 only is described (all 256 cost ~60 ms of
  boot, dead-ends/ecam-all-256-buses.md), with the 256 MiB ECAM still reserved; a conventional
  PNP0A03 root bridge (no PCIe capability, so no `_OSC`/`_PRT`); no host-bridge function at
  00:00.0, devices from 00:01.0; BARs assigned before boot from the windows, live only inside
  them; MSI-X only, no INTx; MSI GSIs from 24 up. Undo: `devices/pci` and `acpi.rs`.
- **Virtio-PCI IDs past type 63 use device ID 0x103f (board 13111):** the last transitional ID, with
  the type as subsystem ID. The task and doc-1 said 0x107f, but upstream Linux reads 0x107f as
  type 63 and binds no ID past it; AC and doc-1 corrected. Undo: `pci_ids()`.
- **Virtio-PCI runtime (board 13111):** one epoll device thread per machine and one lock per device
  (board 13101's tokio API or board 13067's per-vCPU queues may revisit); a masked MSI-X vector has its
  irqfd unassigned and its eventfd holds the pending bit; EVENT_IDX offered for every device; up to
  3 shared memory regions per device (what fits in config space), each aligned to min(size
  rounded to a power of two, 1 GiB); every PC machine has a virtio-rng, with no switch to turn it
  off.
- **BAR memory uses the one slot table (merge of board 13111 and board 13113):** BARs' memory slots go
  through `MemorySlots` like every device mapping, instead of board 13111's own counter from slot
  0x100, so overlaps are refused and BAR mappings appear in `memory_layout()` for snapshots.

- **vCPU threads live with the machine; the caller's thread runs no vCPU (board 13109):** spawned at
  creation, joined on drop; `run()` waits for the first outcome from any vCPU (or a stop, or the
  timeout), kicks every vCPU, and returns once all are parked. Why: KVM, seccomp (board 13099) and
  cladefs's `guest_thread_started` want a vCPU on one thread for good. Timeouts moved from
  per-thread POSIX timers to the waiting caller. Alternatives: threads per run, or vCPU 0 on the
  caller's thread. Undo: `vcpus.rs`.
- **1 to 254 vCPUs, bare machines exactly one (board 13109):** xAPIC MADT entries (8-bit APIC IDs,
  0xFF broadcast, the I/O APIC takes the next ID); x2APIC entries would lift it (board 13127). Bare
  machines have no LAPIC to start more. Topology: one package, a core per vCPU, no SMT; AMD leaf
  0x8000_0026 hidden so Linux uses leaf 0xB.
- **The CPU fingerprint leaves out the vCPU count (board 13109):** it names the CPU model (taken over a
  lone vCPU's table); snapshot restore must compare vCPU counts separately (added to board 13095).
- **`Machine::serial_output()` returns a `Vec<u8>` copy (board 13109):** the UART is shared by all vCPU
  threads behind a lock, so no borrow can be handed out. Public API change.

- **vsock's host end is in-process (board 13089):** `Machine::vsock()` gives `connect`/`listen` and
  `VsockStream`s, not Firecracker-style Unix sockets or vhost-vsock: a library has no socket files
  to manage, CIDs are per machine, and the device resets connections itself. Every PC machine has
  one, guest CID 3, stream only. Limits: 256 KiB credit and send queue per connection, 1024
  connections, backlog 128 (`vsock/state.rs`). A Unix-socket bridge can sit on top if needed.
- **No connections after a vsock reset until the guest has taken it (board 13089):** the guest drops
  every connection it has when it handles the reset event, including ones made in between
  (cladefs's TASK-277 case), so the device refuses new connections until the guest posts its event
  buffer again; clients retry. Risk: a guest driver that never reposts refuses connections forever.
- **The guest agent (board 13089):** Rust, static musl, in a crate shared with the host so the protocol
  cannot drift; vsock port 100 (privileged, so unprivileged guest code cannot impersonate it);
  protocol v1 (`lmag` + version, refused on mismatch), one exchange per connection; ping and file
  reads/writes are resent after a reset, an exec only if the reset came before its request (else
  `Error::ConnectionReset`, "it may have run"); no request IDs yet; the agent kills a command's
  process group if its connection ends first; a command's environment is exactly the caller's
  (default PATH); `write_file` is temp file + rename; `Agent::timeout` defaults to none (connect
  still gives up after 10 s); `reset_connections()` is public for testing reconnect handling.
- **The test guest carries busybox (board 13089):** the host's static busybox goes into the initramfs
  (3 MB; the build needs `busybox-static`), and the kernel gains NET, UNIX, VSOCKETS,
  VIRTIO_VSOCKETS, FUTEX. Quiet boot to power-off went from ~42 to ~48 ms (vmlinux). Alternative:
  build busybox from a pinned source.

- **Snapshots split in two (2026-11-14):** board 13095 is now the snapshot core (RAM, vCPUs, interrupt
  controllers, timers, libmachine's own devices) and depends only on what has landed; the cladefs
  extent device's doc-4 snapshot protocol moved to its own task, which waits for the cladefs crate.
  Why: the core does not need cladefs, and waiting would block the most important feature on an open
  question. Undo: merge the two tasks back.

- **CPUID is an allow list per leaf (board 13125):** `LEAVES` in `cpuid.rs` names every leaf (and which
  subleaves) a guest can see; everything else KVM reports is absent, and feature bits pointing at a
  dropped leaf or XSAVE component are cleared. Per leaf, not per bit: inside a listed leaf the CPU
  model still decides the bits (a per-bit allow list would make the host model a fixed one). MPX
  and leaves 0x15/0x16 left out (kvmclock gives the TSC frequency); AMX/AVX10/APX listed from
  Intel's manual but untested on real hardware (see open questions). The fingerprint version went
  to v2, so every fingerprint changed once.
- **Exit counts are only the exits that reach libmachine (board 13123):** `Machine::exit_counts()` by
  device class; exits KVM handles itself (interrupts, LAPIC, HLT on a PC) are not counted (a
  follow-up task uses KVM's stats). Quiet-boot budgets: 1,450 (test kernel) and 1,500 (cladefs's),
  raised from 1,000/1,100 at the merge because board 13089's vsock adds a second virtio-PCI function
  (~280 boot exits each); the measurements are next to the constants.
- **`Machine::start_run()` returns a `Run` handle (board 13129):** `wait_for(d)` gives back the run if it
  is still going, without touching the vCPUs; every way of finishing consumes the `Run`, and
  dropping an unfinished one stops it. Python's `run()` waits in 50 ms steps on it, so Ctrl-C no
  longer kicks the vCPUs 20 times a second. A `Run` nobody waits on runs past its timeout until the
  next wait (documented). Alternative: a polling callback.

- **Networking is off by default; when on, the VMM is the guest's whole network (board 13097):**
  `MachineConfig::network` is `None` unless asked (no thread, no boot cost). Guest `10.0.2.2/30`
  (hostname `guest`), the VMM at `10.0.2.1` as gateway and DNS server, also standing for the host's
  `127.0.0.1`; set by `ip=` on the kernel command line (costs ~20 ms of boot; alternatives: DHCP,
  or the agent). smoltcp 0.12 (0.13+ needs Rust 1.91); MTU 65520, mergeable buffers, no offloads;
  the guest's SYN waits for the real host connect, so `connect()` reports the true outcome. IPv4
  and TCP only; UDP only to the DNS server (A records, NXDOMAIN for names no rule allows); pings
  only to the gateway. 128 connections x 256 KiB buffers per machine; one network thread each.
- **Egress policy (board 13097):** ordered rules, first match wins, deny by default; hosts are `*`, an
  IP, a CIDR or a DNS name (`name`, `*.domain`) matching the addresses the guest got from the VMM's
  DNS. `*` and names match only public addresses, so loopback, private, link-local and metadata
  addresses need an explicit IP/CIDR rule (no reaching the caller's LAN by DNS tricks). Known
  limits: rules act on IPs, so sites sharing an allowed name's IP (CDNs) are reachable; no TLS SNI
  inspection; policy changes do not affect open connections. (Learnt names now expire with the
  VMM's DNS TTL: board 13157.)

- **Agent commands outlive their connections (board 13131, protocol v2):** each exec carries a random
  host-chosen request ID (random so restored clones in other processes never collide); the agent
  runs a command once per ID, keeps it apart from connections, and a reset is met by taking it up
  again (resend by ID, attach from output offsets, stdin and signals resent from the agent's
  counts). Bounds: output kept 512 KiB behind what was sent and read at most 256 KiB ahead
  (backpressure, never loss); host keeps the last 1 MiB of stdin; orphans killed after 60 s and
  results forgotten after 60 s (forgotten IDs refused, never rerun); 1024 commands, 64 MiB;
  reconnects give up after 30 s. A Process is released explicitly (drop), no longer by its
  connection ending. A restarted agent (new instance in its Hello) fails with ConnectionReset,
  which now means "could not be taken up again". Undo: drop Attach/replay, keep the IDs.
- **A guest that stops fails what waits on it, decided by the run's outcome (board 13133):** after a
  run ends in Poweroff, Reset or Shutdown, vsock connects, accepts, reads and writes fail at once
  (`is_guest_stopped`), agent calls with `Error::GuestGone` (Python ConnectionResetError), until
  the guest's driver starts the device again (libmachine has no reboot yet). FailEntry and errors
  do not stop the vsock (the guest may be resumable). Every way of ending a run passes the same
  hook (`Run::ended`).

- **The async Sandbox (board 13101):** `sandbox` feature, on by default (tokio optional for sync-only
  users); blocking pieces wrapped on threads, not native async vsock I/O (~70 µs per exec and a
  thread per running command; a sans-IO agent core would remove both); per sandbox one run thread
  owning the machine (not tokio's blocking pool, which hundreds of sandboxes would fill), and per
  running command a dedicated output thread; a sandbox's guest runs once (whatever ends the run
  frees the machine; calls then fail with GuestGone; a 64 KiB console tail is kept); dropping an
  exec future or a process SIGKILLs its command; defaults 256 MiB, 30 s ready timeout, no agent
  timeout. Measured: ~29 MiB and 3 threads per idle sandbox, 100 created at once in ~1 s.
- **Python awaitables are coroutines that never complete after `atexit` (board 13101):** bare asyncio
  Futures from pyo3-async-runtimes were refused by `create_task`, and a completion after
  interpreter finalization segfaulted (seen 1 in 3 runs); one `awaitable()` helper wraps every
  Future in a coroutine and holds completions back once `atexit` ran
  (dead-ends/python-awaitables-as-asyncio-futures.md).
- **libmachine's own asyncio bridge replaces pyo3-async-runtimes (board 13191):** the exit guard
  above leaked. Cancellation made the library enter Python from tokio threads, and a flag checked
  before attaching is not atomic with the attach. The result was a segfault at exit in 1 of 2
  runs that cancel calls as `asyncio.run` ends. It was the bridge's third bug, after board 13101 and
  board 13173. Now:
  - `awaitable()` makes the asyncio Future itself, and a done callback aborts the tokio task.
  - Completions go through one gate (`with_python`), which the decider also uses. `atexit`
    closes the gate and waits up to 1 s, without the GIL, for threads inside to leave.
  - A panic fails the awaitable with PanicException.

  Possible instead: pyo3's experimental native async methods (not stable, no tokio story). Undo:
  restore the dependency and `future_into_py` in `awaitable()`
  (dead-ends/pyo3-async-runtimes-guarded-by-an-exit-flag.md).

- **Devices take buffers through `DeviceQueue`, which cannot rewind (board 13149):** the second ring
  rewind bug (net, then vsock) made it structural: no way to hand a buffer back, and `pop()` on an
  empty ring re-arms notifications and looks again. vsock holds an rx buffer too small for data
  for a later packet without data. Undo: give devices `virtio_queue::Queue` again.
- **Snapshots (board 13095):** one file: a 64-byte header (magic, format v1, state length, RAM offset,
  SHA-256 of the state), the state, then RAM 2 MiB-aligned with zero pages as holes; written beside
  its path, synced, renamed, mode 0400. Taken between runs only (every transport lock held), PC
  machines only, without a network (networked machines refused; follow-up) or runtime device
  memory (board 13139's). Restore builds the machine as `boot_linux` does with RAM a MAP_PRIVATE map
  of the file, restores KVM and device state, moves kvmclock and TSCs on by the real time elapsed,
  sets the TSC frequency to the snapshot's (refused if KVM cannot), signals every active queue's
  MSI-X vector once, resets the vsock transport, and reseeds by two mechanisms: VMGENID (ACPI VGEN
  at 0xFF000 plus a Generic Event Device on GSI 16, a pin the legacy PIC never sees:
  dead-ends/ged-on-an-isa-irq.md) and a mandatory agent reseed (protocol v3) before any request.
  `Machine::restore(&snapshot, config)` takes an explicit config that must match; Python has a
  separate `Snapshot` class. Agent instance IDs stay the same in clones (request IDs are random);
  commands running at snapshot time are orphaned in clones: snapshot templates idle. Every run end
  costs one extra KVM_RUN per vCPU to finish a pending I/O access, so state read between runs is
  complete. Measured: restore ~2.5 ms median, first agent answer ~19 ms after restore begins.
- **Host-side settings are not part of a snapshot (merge of board 13095 and board 13163):** e.g.
  `serial_capture_limit` is the restoring caller's; the snapshot check compares only what the
  guest saw (CPU, vCPUs, memory map, no network).

- **Asking the caller is a rule action (board 13151):** `Ask` sits beside allow and deny in the ordered
  rules (put `ask 0.0.0.0/0` last to decide what the rules leave open). A `Decider` callback runs on
  one serialized thread per machine, never the network thread and never joined, so a stuck callback
  costs timeouts, never a hang; the guest's SYN waits for the answer up to 10 s, then the default
  (deny) applies and a late answer is ignored; panics and `None` get the default. Python callbacks
  run with the GIL on that thread and are not called after `atexit`.
- **Connection log is a ring the caller drains (board 13151):** default 1024 records (at most 2^20),
  written when a connection ends (denied ones at once), oldest dropped and counted; live byte
  counters in `NetworkStats`. Not an event stream or callback: bounded memory, no caller code on the
  network thread; long-lived connections show only when they end.
- **DNS TTLs and host entries (board 13157):** every VMM DNS answer carries `dns_ttl` (default 60 s) and
  a learnt name matches name rules for exactly that long (open connections stay); the host's
  resolver (getaddrinfo) reports no upstream TTL, so ours is used. Caller-set host entries are
  answered first, only for names the policy could allow; 127.0.0.1 is answered as the gateway; a
  name rule matches an entry's address even if private or loopback (the caller chose the mapping;
  the public-only restriction guards against untrusted DNS answers).

- **Host connects wait out a vsock reset (board 13177):** a host connect during a transport reset
  waits for the guest to take it, within its own timeout, rather than being refused and polled.
  A guest that never takes it makes connects time out instead of refuse. The guest's own
  connects are still refused during the window. Undo: `Vsock::after_a_reset`.
- **The vsock device asks for the guest's repost to notify it (board 13177):** during a reset it sets
  `avail_event` to the index it last saw. Linux keeps 8 event buffers posted and so never crossed
  the index virtio-queue leaves. It is written directly, since virtio-queue only sets it to its
  next index. Undo: `DeviceQueue::notify_past` and `look_at_events`.
- **Vsock device state version 2 (board 13177):** it carries the next host port, so that a clone never
  reuses a port its template's guest still holds. Version 1 snapshots are refused, no migration.
- **A reconnect's stdin replay runs on a thread of its own (board 13177, board 13193):** one per reconnect
  that has something to send again; later writes wait for it. Both batches found the same
  deadlock, and the merge needs both fixes:
  - the replay thread (board 13177) covers a reconnect the reader starts;
  - board 13193's reader never waits for the writing lock (a try_lock, else a Condvar), because a
    writer waiting for its replay holds that lock.

  Each design alone failed the other's case: board 13193's test hung for 300 s on the replay thread
  alone. Alternative: board 13165's sans-IO client, with a connection driver that always reads.
  Undo: `Replay`, `reset_seen` and `relinked` in `agent.rs`.
- **Templates are snapshots of a running sandbox (board 13167):** `Sandbox::snapshot(path)` pauses the
  run thread between runs, writes the file and runs on. A stop that is not a close no longer ends
  a sandbox. Alternatives: handing back the `Machine`, or templates only through the low-level
  API.
- **`Sandbox::from_snapshot` is a constructor of its own (board 13167)**, not a source of `builder()`;
  Python gets `AsyncSandbox.restore` and `.snapshot`.
- **Only an idle guest is suspended (board 13203):** every vCPU halted (KVM_MP_STATE_HALTED) with no
  interrupt in its local APIC's IRR, every device settled (its queues processed, nothing held for
  the guest), and no doorbell ring since the checks began; otherwise `Error::Busy`, and the
  Sandbox retries in 2 ms runs for up to 2 s. Suspending anywhere else could leave a driver
  polling with its interrupts off, never woken for the frame it had not read. The alternative, a
  guest-side agent call asking the guest to quiesce, would cost a round trip and trust the guest.
- **A suspended guest wakes when a device puts something in one of its queues (board 13203):**
  interrupt or not (a polling driver turned them off). Its own timers do not wake it: they fire
  late, when it wakes. Waking on timers would make every idle guest with a periodic timer
  unsuspendable. Undoing it: a wake deadline from the guest's next timer (KVM gives the LAPIC
  timer's deadline).
- **A suspended guest's clocks run on (board 13203):** kvmclock and the TSC keep counting, as between
  any two runs, so the guest sees the time it waited, as if it had waited awake: its wall clock
  stays right and a read timeout still expires. Each vCPU is marked paused (KVM_KVMCLOCK_CTRL) so
  that watchdogs do not report the gap. Freezing the monotonic clock would hide the wait from
  timeouts, but put the guest's monotonic and wall clocks out of step and change what the
  program observes. Undoing it: save kvmclock at suspension and set it back at the wake.
- **The suspension file is the machine's, updated in place, not flushed (board 13203):** the first
  suspension makes an unnamed file in the caller's directory (tmpfs refused; a clone's gets the
  snapshot's data extents); later ones write only the pages pagemap shows as the process's own,
  into the same file. No fsync and no page cache drop: writeback starts in the background and the
  kernel reclaims the clean pages as it needs memory, so a guest woken soon finds them cached.
  Flushing and dropping made every later suspension read all of RAM back (dead-ends/
  rewrite-all-ram-at-each-suspension.md).
- **A suspension serializes the network's socket state to its file (board 13209/67/68), overturning
  the in-process half of board 13203's design:** the user said, of board 13203, "no socket state should be
  serialized to file", read as "no: socket state should be serialized to the file" (it followed
  their push to serialize TCP state, and opens with disagreement). So a suspension becomes a
  snapshot of the whole machine, guest connections' TCP state included, with the VM freed; only
  host fds and the vsock host end stay. If the reading is wrong (nothing of the sockets should go
  to the file), board 13209 and board 13213 are dropped and board 13203 stands as is.
- **smoltcp is vendored, with a patch exporting a TCP socket's state (board 13209):** smoltcp 0.12.0
  is in `third_party/smoltcp` (0BSD), used through `[patch.crates-io]` in the workspace and the
  fuzz crate. The patch is one new file, `src/socket/tcp/snapshot.rs` (`Socket::snapshot(now)` and
  `Socket::restore(&snapshot, now)`: every field as plain data, times relative to `now`), plus a
  read-only `RingBuffer::peek_unallocated` (22 lines) and the `mod` line;
  `README.libmachine.md` there lists every change. Not carried: the timestamp generator (a
  function) and the congestion controller's internal state (libmachine builds smoltcp without
  Reno or Cubic). Nothing is posted upstream. Alternatives: a git fork, or rebuilding TCP state
  outside smoltcp. Undoing it: drop the patch, and `Stack::save` with it.
- **What a restored network does with what was waiting (board 13209):** a DNS lookup in flight is
  made again under its ID; a connection waiting for the decider is asked about again under its
  ID, with the time it had left (refused if the restoring caller has no decider); a connection
  whose host connection was being made keeps its stream if handed back, or has it made again to
  the same destination with the verdict it had (no new policy check: the guest's SYN was already
  judged); an open connection whose stream is not handed back is reset towards the guest, its RST
  out before it goes. Alternatives: refusing half-made connections in a clone (the guest's
  connect would fail for nothing), or checking them against the clone's policy. Undoing it:
  `Stack::restore` in `net/stack/save.rs`.
- **The network's timers resume with the time they had left (board 13209):** smoltcp's timers, the
  decider's deadlines and learnt names' TTLs do not count the time between a save and a restore;
  the log keeps wall-clock start times, so a connection's logged duration does count it. The
  guest's clocks do run on (board 13203), so its own DNS cache may have expired a name the VMM still
  matches for the TTL it had left: slightly more permissive, never less. Alternative: store the
  save's wall time and subtract the gap. Undoing it: that, in `Stack::restore`.
- **Saving virtio-net keeps the machine's connections (board 13209):** `NetDevice::save` stops the
  network thread, saves its stack and starts it again from the state with its host streams, so a
  template snapshot runs on with its connections; `restore` hands back no streams (a clone's
  connections are reset). board 13213 adds the suspension's hand-over of streams. Device state and
  stack state are each version 1; others are refused. A restored stack learns the guest's MAC
  address from a synthetic ARP request (smoltcp's neighbour cache is not saved) and takes a new
  random seed (a clone's new connections get other initial sequence numbers than its
  template's).
- **DNS replies wait in the stack, one at a time in smoltcp (board 13209):** smoltcp's UDP socket
  cannot be read back, so the stack keeps unsent replies itself (up to 32) and hands smoltcp the
  next once it sent the last; a save carries them. Undoing it: `send_dns`/`flow_dns`.

- **A host end is handed over by detaching it before the snapshot (board 13211):**
  `Machine::restore_with(snapshot, config, Some(vsock))` (crate-internal) attaches the restored
  device to an existing `Vsock`, connections, listeners and generation kept, no transport reset.
  The host end must first be detached (`Vsock::detach`, crate-internal, undone by `reattach`),
  before the snapshot: a detached device serves nothing and its machine's drop leaves the host
  end alone. Detaching after the snapshot would let the old device thread move bytes into the
  old guest's RAM between the snapshot and the drop, lost to the restored guest. Each host end
  has a random identity and counts its device's exchanges with a guest (packets taken or given,
  events, driver starts and resets); vsock device state version 3 saves both, version 2 is
  refused. A restore onto a host end is refused, the host end unchanged, unless it is detached,
  the snapshot's own, and has made no exchange since. A restored device gets its own wake eventfd
  and installs it on the host end. Possible instead: detach inside a snapshot variant (one call,
  but a second snapshot entry point in machine.rs). Undo: `restore_with`, `State::adopt` and the
  gates on `State::serves`. A machine run after its host end was detached could still reset it
  through `guest_stopped`; board 13213 drops it instead.

- **A suspension writes the whole machine to its file and frees it (board 13213), replacing board 13203's
  "keeps the VM and moves only its RAM":** the vCPUs', devices' and network stack's state (every
  guest connection's TCP state) and the RAM go to the machine's file; the hypervisor VM, the
  vCPU, device and network threads and the guest's RAM mapping are freed. Held in the process:
  the host sockets (`HeldStreams`, no read or write access: only their fds, for the watcher, and
  `into_streams` for the restore), the vsock host end (detached), the network's `Shared`, the
  console, the stop state and doorbell, the exit counters, and one watcher thread. A wake
  restores through the same code as `Machine::restore` onto all of those, with no new VMGENID.
  Undo: `Machine::suspend`/`thaw` in machine.rs.
- **One file per machine, the state after the RAM (board 13213):** a 4 KiB header (magic `lmsusp`,
  version, what the file lacks, the state's offset, length and SHA-256), the RAM from 2 MiB laid
  out as in a snapshot, the state after the RAM's end, rewritten and the file truncated at each
  suspension. One fd, one unnamed file, and the RAM's offsets never move however the state
  (up to tens of MiB with a busy network) grows or shrinks. A second file would do as well,
  for a second fd and nothing gained. Undo: `suspend::write_state`/`read_state`.
- **A suspended machine's file is no template (board 13213):** its magic is not a snapshot's, and its
  header says what it lacks (the host sockets, the vsock host end); `Snapshot::open` refuses it
  naming both. A plain `Machine::snapshot` of a networked machine stays refused (board 13175's
  question). Undo: `snapshot::suspended_file`.
- **The network must be quiet to be suspended (board 13213):** besides board 13203's checks, a suspension
  is `Busy` while the stack has a DNS lookup or a decider question in flight, frames for or from
  the guest not yet handled, a DNS reply not yet out, or anything smoltcp would send now. Saved,
  the stack's timers do not run (board 13209: they resume with the time left), and answers from its
  lookup and decider threads would go to no one, so a guest waiting on them would never wake. A
  guest waiting on a decider that takes more than 2 s cannot be suspended (`GuestBusyError`).
  Possible instead: keep the stack's host side (lookup channel, decider thread) alive while
  suspended and let its answers wake the machine. The busy check parks the network thread and
  resumes it whole if busy, so nothing in flight is lost or asked twice. Undo: `Stack::quiet`,
  `NetDevice::hand_over`.
- **What wakes a suspended machine (board 13213):** the stack says, per host socket, what would make
  it act for the guest (`Stack::watch_list`): data or a close where it has read all the host
  sent and smoltcp has room; room where the guest's bytes wait for the host, or a connect is in
  progress. The watcher waits for those (level-triggered epoll) and for the vsock host end owing
  the guest a packet (`State::has_packet`, the same test the idle check uses), and rings the
  doorbell. Not exact in two rare cases, both waking the guest for nothing: data from a host
  whose guest end advertises a zero window (smoltcp would buffer it), and an error or hang-up on
  a socket the stack would not touch (epoll always reports them). Undo: `suspend::Watcher`.
- **A wake refuses host sockets that lost bytes (board 13213):** each held socket's unread byte count
  (SIOCINQ) is recorded at the hand-over; a wake with fewer waiting (something read them) is
  refused, unless TCP_INFO says the peer reset it (the kernel may drop what was waiting). Reading
  SO_ERROR instead would take the error from the stack that needs it. Undo:
  `HeldStreams::check_unread`.
- **The freed VM's file closes on a thread of its own (board 13213):** KVM destroys a VM as its last
  fd closes, 20-27 ms here, which made each suspension 26 ms instead of 5 ms
  (dead-ends/free-the-vm-inline-at-suspension.md). A short-lived `libmachine-vmfree` thread closes
  it; the guest's RAM is unmapped before (it is not KVM's). Undo: `MemorySlots::retire`.
- **What a caller's handles do across a suspension (board 13213):** the stop handle, vsock and agent,
  network, console (serial output, now one `Arc<Mutex<Output>>` the UARTs write to) and exit
  counts carry on. `MemorySlots` handles are the VM's: retired at the suspension, they map
  nothing and say why (take a new one after a wake). While suspended: `read_memory` reads the
  file, `registers` is refused, `load`/`set_entry` restore the machine (it stays suspended), and
  `snapshot` restores it, snapshots and suspends it again. A machine with devices of the
  caller's (`boot_linux_with`) cannot be suspended: only the caller could make them again. A
  wake that fails loses the machine: everything after fails with `GuestGone`, and so does
  everything on its vsock host end.
- **Interrupts that may have been on their way are sent again at the first run after a restore
  (board 13213):** KVM injects an irqfd's interrupt from a workqueue when it cannot at once (always
  for the UART's I/O APIC route, sometimes for an MSI), so one signalled just before the state
  was saved can be lost with the VM. Every active virtio queue, and the UART if its IIR shows an
  interrupt pending, interrupts the guest again; a spare interrupt only makes a driver look.
  Clones already did it for virtio queues at the restore; a woken machine does it when it first
  runs (not at the restore: one restored to be snapshotted is suspended again, and spare
  interrupts would make it busy). Not reproduced (30 runs under 10-way load): this is from
  KVM's code, not an observed loss. Undo: `Live::resend_interrupts`.
- **Wake latency (board 13213):** release build, 256 MiB guest that wrote 32 MiB: first suspension
  19-21 ms (board 13203: 21), later ones 5.7 ms median (board 13203: 5), a call that wakes the guest
  7.1 ms median (board 13203: 4.7; awake 0.6 ms). The 2.5 ms more is the restore: a new KVM VM, its
  vCPUs and interrupt routes. Measured by `kvm_suspend::suspension_timings` (ignored; run it with
  `--release -- --ignored`).

## Open questions (as libmachine left them)

- **May perf be enabled for profiling?** This host has `perf_event_paranoid=4`. Lowering it, or a
  run as root, would let board 13199 confirm why 100 restores at once take 39-45 ms each against
  2.2-4 ms alone. The guess is KVM's SRCU grace periods and the memory-map lock. Only you can
  change the host's setting. I recommend `sysctl kernel.perf_event_paranoid=1` for one session.
  Until then board 13199 waits, and board 13179/60 go on unprofiled.

- **May I report the pyo3-async-runtimes crashes upstream?** It enters Python from tokio threads
  after the interpreter finalized: on completion (board 13101) and on cancellation (board 13191, from a
  core dump). libmachine no longer depends on it (board 13191), so this only helps other users.
  Reporting means posting an issue on its public GitHub repository, which I will not do without
  your go-ahead. Nothing waits on it.

- **Can the CPUID allow list be run on an Intel host with AMX/AVX10?** board 13125 lists Intel's AMX
  (leaves 0x1D/0x1E), AVX10 (0x24) and APX from the manual, but this host is AMD Zen 4, so those
  paths ran only on fake tables. Needed: access to an Intel Sapphire Rapids or newer machine with
  KVM (or a GCE instance like cladefs's kept hosts). Until then those leaves pass as the manual
  describes; nothing else depends on it.

