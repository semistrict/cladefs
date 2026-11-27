# libmachine

An embeddable virtual machine monitor (a "library VMM") for cladefs's guests on an Apple Silicon
Mac, on Hypervisor.framework (arm64, macOS 15 and later; board 13063): link it into your process
and run Linux guests, DAX included (board 13054). Elsewhere the crate is empty: Firecracker is
cladefs's production VMM on Linux. libmachine is here for two users: `crates/libmachine-firecracker`,
a Firecracker-compatible VMM on it, which runs cladefs's guest tests on a Mac, and `crates/machined`,
the multi-VM server of hotbed's Mac worker. Its architecture is doc-30 (`docs/design/`), its
decisions before it came into this repo doc-31; its tasks are on the board
(`hotbed board task ls --label tag.libmachine=yes`).

**Status:** a `Machine` boots an arm64 `Image` on the board libmachine describes with a device tree:
the framework's GICv3 and timers, PSCI, a 16550 console and virtio-PCI devices (entropy, vsock, a
network with an egress policy, a balloon, block devices, persistent memory, and cladefs's extent
store device with its DAX window). A bare machine (`Machine::new`) remains for instruction-level
tests. A process has one Hypervisor.framework VM: one machine at a time.

## Booting Linux

```rust
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig};

let boot = LinuxBoot {
    kernel: "target/arm64-guest/Image".into(),
    initrd: Some("target/arm64-guest/initramfs.cpio".into()),
    cmdline: "quiet".into(),                       // after "console=ttyS0 panic=-1"
};
let mut m = Machine::boot_linux(MachineConfig { mem_size: 256 << 20, ..Default::default() }, &boot)?;
m.set_console(std::io::stdout());                 // stream the console as it runs
assert_eq!(m.run()?, ExitReason::Poweroff);       // `reboot` gives ExitReason::Reset
```

The kernel is cladefs's arm64 `Image`, the one Firecracker boots: Linux with Fxfs and the virtio
extent-store driver, built by `scripts/build-guest-kernel.sh arm64`. It needs `CONFIG_PCI` with
MSI-X, `CONFIG_VIRTIO_PCI` and a console on the 16550 (`console=ttyS0`).

### Running commands in the guest

Every board machine has a virtio-vsock device (guest CID 3) whose host end lives in this process
(`Machine::vsock()`), and libmachine's guest agent (`crates/libmachine-agent`, a static binary in
the test initramfs, started by `libmachine_action=agent`) serves exec and file copy over it. The
agent answers only while the machine runs, so run it on another thread:

```rust
let mut m = Machine::boot_linux(config, &boot)?;      // cmdline "quiet libmachine_action=agent"
let agent = m.agent().expect("a board machine has an agent");
let stop = m.stop_handle();
let runner = std::thread::spawn(move || m.run());   // the agent answers while it runs
agent.wait_ready(Duration::from_secs(30))?;
let out = agent.exec(Command::new("sh").args(["-c", "echo hi"]))?;
agent.write_file("/tmp/x", b"data", 0o644)?;
stop.stop();
runner.join().unwrap()?;
```

The protocol is versioned (both sides refuse another version; it is at version 3) and follows
cladefs's reconnect rules: one exchange per connection, a reset means connect again, and only what
is safe to see twice is resent. Each exec carries a request ID the agent runs once, so an exec is
resent after any reset, and a running process whose connection is reset takes its command up again
on a new one, from the output it has, resending the stdin and signals the agent had not taken. What
cannot be taken up (the agent restarted, or forgot the command a minute after its connection went)
fails with `Error::ConnectionReset`. When the guest powers off or resets, every vsock stream,
connect and agent call fails at once (`Error::GuestGone` for the agent) instead of waiting.

The agent is also how `examples/linux_test_env.rs` runs cladefs's tests that need Linux and KVM
on a Mac: it boots a Linux machine with nested virtualization (`MachineConfig::nested_virtualization`,
Apple M3 and later) whose root is a cell of cladefs's daemon, and runs `scripts/test.sh` in it.

### Networking

A machine gets a network card only if `MachineConfig::network` asks for one. Its other end is a
userspace TCP/IP stack ([smoltcp](https://crates.io/crates/smoltcp)) in this process: the guest
(`10.0.2.2/30`, configured by the kernel command line) routes through the VMM (`10.0.2.1`), which
makes each of its TCP connections again from the host, with an ordinary socket, if the egress policy
allows the destination. No privileges; denied connections are refused at once. The gateway address
stands for the host's loopback, and the VMM answers DNS for names the policy could allow. The
default policy allows nothing, and `*` and name rules never reach loopback, private or link-local
addresses (an explicit IP or CIDR rule can). A machine with a network is not snapshotted.

```rust
let policy = EgressPolicy::deny_all()
    .allow(HostPattern::name("*.pypi.org")?, 443)
    .allow(Ipv4Addr::LOCALHOST, 8080); // the guest's 10.0.2.1:8080
let config = MachineConfig { network: Some(NetworkConfig::new(policy)), ..Default::default() };
let m = Machine::boot_linux(config, &boot)?;
m.network().unwrap().set_policy(EgressPolicy::allow_all()); // from any thread
```

Guest kernels need `CONFIG_INET`, `CONFIG_VIRTIO_NET` and `CONFIG_IP_PNP`. The guest-facing network
code has cargo-fuzz targets in `crates/libmachine/fuzz` (`cargo +nightly fuzz run net_frames`).

### Stopping a running machine

`run()` blocks its thread while the guest runs. `run_for(timeout)` returns `ExitReason::TimedOut`
after the timeout, and a `StopHandle` (from `machine.stop_handle()`, cheap to clone, usable from any
thread) makes `run()` return `ExitReason::Stopped`. Either way the machine keeps its state and
`run()` continues it. A stop requested while the machine is not running is kept.

### Snapshots

`Machine::snapshot` writes a stopped machine to a file, `Snapshot::open` checks one, and
`Machine::restore` makes a machine from it, its RAM a copy-on-write mapping of the file (or of RAM its
caller keeps: `restore_onto`), its guest's virtual counter going on from the snapshot's. A guest that
has its agent reseeds its RNG before anything runs. Snapshot an idle guest: a command running at the
snapshot runs on, unreachable, in every clone. Machines with a network are not snapshotted.
`libmachine-firecracker` serves Firecracker's snapshot API with them.

## The test guest

`crates/libmachine/scripts/arm64-guest.sh` builds cladefs's arm64 kernel and the test initramfs on an
arm64 Linux node of hotbed's, and fetches them into `target/arm64-guest`. The initramfs's `init`
(`guest/init.c`) prints a marker and powers off; by the kernel command line it also does one of
`libmachine_action=`: `reboot`, `hang` (idle forever), `sh` (run the shell script
`libmachine_sh`, then power off) or `agent` (run the guest agent); `libmachine_cat=PATH,...` has it
print those files (or list those directories) from /proc or /sys first. Tests that need the guest skip
themselves, saying so, when it is missing; `scripts/test.sh`'s `hvf` part fails on any such note.

## Layout

| Path | What |
|---|---|
| `crates/libmachine` | Rust core: the `Machine` API and the Hypervisor.framework backend |
| `crates/libmachine/guest` | The test guest's init |
| `crates/libmachine/fuzz` | cargo-fuzz targets for the guest-facing parsers |
| `crates/libmachine-agent` | The guest agent, and the wire protocol both ends build |
| `crates/libmachine-firecracker` | A Firecracker-compatible VMM on libmachine |
| `crates/machined` | The multi-VM server |

## Requirements

- An Apple Silicon Mac, macOS 15 or later (`kern.hv_support`), and the workspace's Rust
  (`rust-toolchain.toml`). Every macOS executable is signed with the hypervisor entitlement as it
  links (`scripts/macos-link.sh`).

## Tests

`CLADEFS_TEST_PARTS=hvf scripts/test.sh` runs libmachine's, libmachine-firecracker's and machined's
tests on the Mac, niced, failing on any test that skipped itself. Tests write their bare machines'
guest programs with [dynasm](https://crates.io/crates/dynasm), never as raw bytes.
