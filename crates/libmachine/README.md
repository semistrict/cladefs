# libmachine

An embeddable virtual machine monitor (a "library VMM") for sandboxing untrusted
code: link it into your process and create many short-lived micro-VMs.

Linux/KVM (x86_64) is the production platform; a macOS backend on Apple
Silicon (Hypervisor.framework, board 13063) is to run cladefs's guests there in
full, DAX included (board 13054). Its architecture is doc-30
(`docs/design/`), its decisions before it came into this repo doc-31; its
tasks are on the board (`hotbed board task ls --label tag.libmachine=yes`).

**Status:** a `Machine` boots Linux: an ELF `vmlinux` through PVH or a
`bzImage` through the 64-bit boot protocol, on a PC platform with KVM's
in-kernel interrupt controllers, ACPI tables (power off and reset end the run),
a 16550 console and virtio-PCI devices (entropy, vsock, a network with an
egress policy). With `quiet`, a minimal kernel boots to init and powers off in
about 30 ms (PVH) to 45 ms (bzImage). On top of it, the async `Sandbox` (Rust)
and `AsyncSandbox` (Python) run commands through libmachine's guest agent, come
from snapshots in milliseconds, and can be suspended while they wait. A bare
real-mode machine (`Machine::new`) remains for instruction-level tests.

## Booting Linux

```rust
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig};

let boot = LinuxBoot {
    kernel: "target/guest/vmlinux".into(),        // or a bzImage
    initrd: Some("target/guest/initramfs.cpio".into()),
    cmdline: "quiet".into(),                       // after "console=ttyS0 panic=-1"
};
let mut m = Machine::boot_linux(MachineConfig { mem_size: 256 << 20, ..Default::default() }, &boot)?;
m.set_console(std::io::stdout());                 // stream the console as it runs
assert_eq!(m.run()?, ExitReason::Poweroff);       // `reboot` gives ExitReason::Reset
```

- **Kernels:** an ELF `vmlinux` with a PVH entry point (`CONFIG_PVH`), which
  boots uncompressed, or a `bzImage`. An ELF without PVH is refused.
- **The platform:** hardware-reduced ACPI (FADT sleep and reset registers, a
  DSDT describing COM1 and soft-off, a MADT), so the kernel needs
  `CONFIG_ACPI`, `CONFIG_PCI` (ACPI cannot load its tables without it) and
  `CONFIG_PNP` with `CONFIG_SERIAL_8250_PNP` (the console UART's interrupt is
  only described through ACPI). `guest/test-kernel.config` is a minimal working
  configuration.
- RAM must currently end below 3 GiB (board 13113 adds RAM above 4 GiB).

### Running commands in the guest

Every PC machine has a virtio-vsock device (guest CID 3) whose host end lives
in this process (`Machine::vsock()`), and libmachine's guest agent
(`crates/libmachine-agent`, a static binary in the test initramfs, started by
`libmachine_action=agent`) serves exec and file copy over it. The agent answers
only while the machine runs, so run it on another thread:

```rust
let mut m = Machine::boot_linux(config, &boot)?;      // cmdline "quiet libmachine_action=agent"
let agent = m.agent().expect("PC machines have an agent");
let stop = m.stop_handle();
let runner = std::thread::spawn(move || m.run());   // the agent answers while it runs
agent.wait_ready(Duration::from_secs(30))?;
let out = agent.exec(Command::new("sh").args(["-c", "echo hi"]))?;
agent.write_file("/tmp/x", b"data", 0o644)?;
stop.stop();
runner.join().unwrap()?;
```

The whole program is `crates/libmachine/examples/agent_exec.rs`
(`cargo run --example agent_exec -- target/guest/vmlinux target/guest/initramfs.cpio`).

```python
m = Machine.boot_linux(kernel, initrd, "quiet libmachine_action=agent")
agent, stop = m.agent(), m.stop_handle()
t = threading.Thread(target=m.run); t.start()
agent.wait_ready(30)
assert agent.exec(["sh", "-c", "echo hi"]).stdout == b"hi\n"
stop.stop(); t.join()
```

The protocol is versioned (both sides refuse another version; it is at
version 3) and follows cladefs's reconnect rules: one exchange per connection, a
reset means connect again, and only what is safe to see twice is resent. Each
exec carries a request ID the agent runs once, so an exec is resent after any
reset, and a running process whose connection is reset takes its command up
again on a new one, from the output it has, resending the stdin and signals the
agent had not taken. What cannot be taken up (the agent restarted, or forgot
the command a minute after its connection went) fails with
`Error::ConnectionReset`. When the guest powers off, resets or triple-faults,
every vsock stream, connect and agent call fails at once (`Error::GuestGone`
for the agent) instead of waiting.

### Networking

A machine gets a network card only if `MachineConfig::network` asks for one
(`network=` in Python). Its other end is a userspace TCP/IP stack
([smoltcp](https://crates.io/crates/smoltcp)) in this process: the guest
(`10.0.2.2/30`, configured by the kernel command line) routes through the VMM
(`10.0.2.1`), which makes each of its TCP connections again from the host,
with an ordinary socket, if the egress policy allows the destination. No TAP
device and no privileges; denied connections are refused at once. The
gateway address stands for the host's loopback, and the VMM answers DNS for
names the policy could allow. The default policy allows nothing, and `*` and
name rules never reach loopback, private or link-local addresses (an
explicit IP or CIDR rule can).

```rust
let policy = EgressPolicy::deny_all()
    .allow(HostPattern::name("*.pypi.org")?, 443)
    .allow(Ipv4Addr::LOCALHOST, 8080); // the guest's 10.0.2.1:8080
let config = MachineConfig { network: Some(NetworkConfig::new(policy)), ..Default::default() };
let m = Machine::boot_linux(config, &boot)?;
m.network().unwrap().set_policy(EgressPolicy::allow_all()); // from any thread
```

```python
m = Machine.boot_linux(kernel, initrd, "quiet libmachine_action=agent",
                       network=EgressPolicy().allow("*.pypi.org", 443).allow("127.0.0.1", 8080))
m.network().set_policy(EgressPolicy.allow_all())
```

Guest kernels need `CONFIG_INET`, `CONFIG_VIRTIO_NET` and `CONFIG_IP_PNP`.
The guest-facing network code has cargo-fuzz targets in
`crates/libmachine/fuzz` (`cargo +nightly fuzz run net_frames`).

### Stopping a running machine

`run()` blocks its thread while the guest runs. `run_for(timeout)` returns
`ExitReason::TimedOut` after the timeout, and a `StopHandle` (from
`machine.stop_handle()`, cheap to clone, usable from any thread) makes `run()`
return `ExitReason::Stopped`. Either way the machine keeps its state and
`run()` continues it. A stop requested while the machine is not running is
kept. Internally a real-time signal (`SIGRTMIN`) kicks the vCPU thread out of
KVM, with KVM's `immediate_exit` closing the race before it enters the guest.

In Python, `run(timeout=None)` does the same, and Ctrl-C stops the guest and
raises `KeyboardInterrupt` within about 50 ms.

`crates/libmachine/scripts/build-test-guest.sh` builds the guest the tests boot into
`target/guest`: kernel 6.18 (longterm, from kernel.org, checksum-pinned) as
both a bzImage and a vmlinux, and an initramfs whose `init`
(`guest/init.c`) prints a marker and powers off; `libmachine_cat=PATH,...`
on the command line has it print those files (or list those directories)
from /proc or /sys first. It needs the usual kernel build tools plus
`musl-gcc` and `zstd`. Tests that need it skip themselves
when it is missing. `cargo run --release --example boot_linux -- KERNEL
[INITRD] [CMDLINE]` boots any kernel and reports the time.

### cladefs's guest kernel

The kernel libmachine's guests run is cladefs's, the one Firecracker boots:
Linux with Fxfs and the virtio extent-store driver, built by
`scripts/build-guest-kernel.sh` (`scripts/guest-kernel.sh` prints the path of
the one built from this tree), a `vmlinux` booted through its PVH entry.
`guest/linux/configs/cladefs.config` sets what libmachine's platform needs
beyond Firecracker's configuration (no CPU frequency scaling: a vCPU's
frequency is the host's), and the build checks every option the platform
needs (board 13061). `tests/kvm_cladefs_linux.rs` boots it
(`CLADEFS_GUEST_KERNEL`) with the test initramfs. With `quiet` it boots to init
and powers off in about 0.3 s (the minimal test kernel: 30 ms).

### Snapshots: sandboxes in milliseconds

Booting is the slow part of making a sandbox. Boot a template once, prepare
it, snapshot it, then create sandboxes from the file: each is its own guest,
its RAM a copy-on-write mapping of the file, its RNG reseeded (VMGENID and
the agent's reseed with host entropy, before any command) and its clock
moved on to the present.

```rust
let template = Sandbox::builder(boot).create().await?;
template.write_file("/etc/motd", b"prepared".to_vec(), 0o644).await?;
let snapshot = Arc::new(template.snapshot("template.snap").await?); // it runs on
template.close().await;
let sandbox = Sandbox::from_snapshot(snapshot.clone()).create().await?;
```

```python
async with await AsyncSandbox.create(kernel, initrd, "quiet libmachine_action=agent") as t:
    snapshot = await t.snapshot("template.snap")
sandbox = await AsyncSandbox.restore(snapshot)
```

Snapshot an idle template: a command running at the snapshot runs on,
unreachable, in every clone. Networked machines are not snapshotted. The
low-level API is `Machine::snapshot`, `Snapshot::open` and
`Machine::restore` (Python `Snapshot.take`, `Snapshot`, `restore`).

Measured on a shared 16-core host (AMD Zen 4, under load from other work),
256 MiB guests with 1 vCPU, release build, `cargo run --release -p
libmachine --example snapshot_clones -- 10 100`:

| | test kernel | cladefs's kernel |
|---|---|---|
| boot a sandbox, ready | 77 ms | 345 ms |
| snapshot it | 53-72 ms | 65 ms |
| `Machine::restore` alone | 2.5 ms | |
| sandbox from the snapshot, ready (agent answered after its reseed), one at a time | 5-6 ms | 7-8 ms |
| ... 10 at once, each | 10.5-12.6 ms (first command 15-19 ms) | 12.6 ms (19 ms) |
| ... 100 at once, each | 190-215 ms (all 100 up and run a command in 0.33 s) | 183 ms (0.31 s) |
| memory per clone: private (what its guest wrote) | 1.5-1.7 MiB | 2.4-2.7 MiB |
| memory per clone: cgroup, page cache included | 2.0-2.3 MiB | 3.2-3.5 MiB |

The snapshot file's page cache (4-7 MiB touched) is shared by all clones.
Restores made at once in one process contend in the host kernel, so 100 at
once take longer each than one alone (all threads stall together in KVM
setup; each restore waits for 11 KVM SRCU grace periods, for its memory slot,
interrupt routes and ioeventfds, the likely cause; not profiled yet).

### Suspended while it waits

An agent's sandbox spends much of its time waiting: for an LLM's answer to
a request it made, for the next call. `suspend` stops it and moves its RAM to
a file, out of the process's memory, until a device has something for it:
the answer, on the same connection, wakes it where it was, as does any call
on the sandbox or `resume`.

```python
call = await sandbox.spawn(["python3", "ask_llm.py"])   # the guest sends its request
await sandbox.suspend("/var/tmp")                       # the guest is idle, waiting
result = await call.wait()                              # the answer woke it
```

- **Everything but the RAM stays:** the hypervisor's VM, its devices, the
  network (each connection's host end is a socket of this process) and the
  vsock. Nothing the guest sees changes but the time.
- **Only an idle guest:** every vCPU halted with no interrupt waiting, and no
  device holding something for it; `suspend` gives the guest up to 2 s to get
  there, then raises `GuestBusyError` (`Error::Busy`).
- **Its clocks run on**, so it sees the time it waited, as if it had waited
  awake (a read timeout still expires); its own timers fire when it wakes.
- **The file:** made, unnamed, in the directory given (on a disk: tmpfs is
  refused), at the first suspension; later ones write only the pages changed
  since. The kernel writes it out in the background and reclaims its page
  cache as it needs the memory; a guest that wakes soon finds its pages there.

The design and its reasons are in `crates/libmachine/src/suspend.rs`.

## CPU model

Every machine's CPUID table is set explicitly, starting from what KVM supports
on the host:

- `host` (default): every supported feature, minus what a sandbox must not
  expose: nested virtualization (VMX/SVM), performance counters, SGX and
  processor trace. The hypervisor bit is set.
- `baseline`: a fixed x86-64-v3 set (AVX2, BMI1/2, FMA, MOVBE, ...; no
  AVX-512), XSAVE limited to x87/SSE/AVX. Hosts that cannot provide it refuse
  to create the machine and name the missing features. Speculation-control
  bits follow the host in both models.

`Machine::cpu_fingerprint()` is a SHA-256 of the CPUID table and KVM's feature
MSRs: equal fingerprints mean the same CPU as far as the guest can tell, which
snapshots will check before restoring.

## Layout

| Path | What |
|---|---|
| `crates/libmachine` | Rust core: `Machine` API and the KVM backend (rust-vmm crates) |
| `crates/libmachine/guest` | The test guest's kernel configuration and init |
| `crates/libmachine/fuzz` | cargo-fuzz targets for the guest-facing parsers |
| `crates/libmachine-agent` | The guest agent, and the wire protocol both ends build |
| `crates/libmachine-py` | Python bindings (PyO3, built with maturin) |
| `vendor/smoltcp` | smoltcp 0.12.0, patched to export and import a TCP socket's state |

## Requirements

- Linux x86_64 with KVM: your user needs read/write access to `/dev/kvm`
  (usually membership in the `kvm` group). Tests that need KVM skip themselves
  when it is unavailable.
- The workspace's Rust (`rust-toolchain.toml`)
- Python 3.10+ and uv for the bindings

## Rust

`scripts/test.sh`'s `libmachine` part builds the test guest, finds cladefs's
kernel and runs libmachine's tests and the bindings', failing on any test that
skipped itself; on its own: `CLADEFS_TEST_PARTS=libmachine scripts/test.sh`.

```rust
use iced_x86::code_asm::*;
use libmachine::{ExitReason, Machine, MachineConfig};

let mut a = CodeAssembler::new(16)?;
a.mov(dx, 0x3f8)?; // COM1
a.mov(al, b'A' as u32)?;
a.out(dx, al)?;
a.hlt()?;
let code = a.assemble(0x1000)?;

let mut m = Machine::new(MachineConfig::default())?;
m.load(0x1000, &code)?;
m.set_entry(0x1000)?;
assert_eq!(m.run()?, ExitReason::Halt);
assert_eq!(m.serial_output(), b"A");
```

This is `crates/libmachine/examples/serial_hello.rs` (`cargo run --example
serial_hello`). Tests and examples write guest code with
[iced-x86](https://crates.io/crates/iced-x86)'s code assembler, a dev-dependency
only.

## Python

The bindings are a [uv](https://docs.astral.sh/uv/) project in
`crates/libmachine-py`. `uv run` builds the extension with maturin, and
rebuilds it whenever the Rust sources change:

```sh
cd crates/libmachine-py
uv sync                 # create .venv with the package and dev tools
uv run pytest           # run the tests against the current Rust code
uv build --wheel        # build a distributable abi3 wheel (CPython 3.10+)
```

The tests write their guest programs in x86 assembly (NASM syntax), assembled
with [Keystone](https://www.keystone-engine.org/); see
`crates/libmachine-py/tests/guest.py`.

```python
from keystone import KS_ARCH_X86, KS_MODE_16, KS_OPT_SYNTAX_NASM, Ks
from libmachine import Machine

ks = Ks(KS_ARCH_X86, KS_MODE_16)
ks.syntax = KS_OPT_SYNTAX_NASM
code, _ = ks.asm("""
    mov dx, 0x3f8   ; COM1
    mov al, 0x41    ; 'A'
    out dx, al
    hlt
""")
m = Machine(cpu_model="host")  # or "baseline"
m.load(0x1000, bytes(code))
m.set_entry(0x1000)
exit = m.run()          # releases the GIL while the guest runs
assert exit.kind == "halt"
assert m.serial_output() == b"A"
```

Booting Linux from Python (`run()` releases the GIL while the guest runs):

```python
m = Machine.boot_linux("target/guest/vmlinux", "target/guest/initramfs.cpio", "quiet")
assert m.run().kind == "poweroff"
print(m.serial_output().decode())
```

Configuration, guest-memory and bad-kernel errors raise `ValueError`; a
missing file raises `FileNotFoundError` (an `OSError`); hypervisor failures
raise `libmachine.MachineError`.
