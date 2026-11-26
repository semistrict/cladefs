# machined

The server libmachine is for (board 13467): one process running many machines, each a Linux
guest on libmachine (`crates/libmachine`, doc-30), managed over a gRPC API of its own,
`machined.v1.Machined` ([`proto/machined/v1/machined.proto`](proto/machined/v1/machined.proto)).
It embeds cladefs's extent daemon, one per process, shared by its machines (libcladefs, board
13471), and serves that daemon's `cladefs.cladefsd.v1.Cladefsd` (and its health service) on the
same endpoint, so that one endpoint manages both cells and machines. Every machine boots from a
cell (board 13992): its `root`, Fxfs on its extent store device, which its kernel mounts as `/`
(`root=root rootfstype=cladefs rw`); `CreateCell`, `ImportImage` (a container image) and
`ForkCell` of the Cladefsd service make one, and a cell is one machine's at a time.

## Running it

```sh
cargo build --release -p machined
machined --images /var/lib/machined/images --state /var/lib/machined/state \
    --socket /run/machined.sock
```

- `--images DIR`: where kernels and initramfses are. A machine's `Boot` names them relative to
  it; nothing outside it boots (no `..`, no absolute path, no symbolic link out of it).
- `--state DIR`: machined's own directory, made mode 0700: its machines' vsock sockets, its
  snapshots and its suspensions' files. On a disk (suspensions refuse tmpfs), and short: the vsock
  sockets' paths under it must fit a Unix socket's (about 100 bytes); machined refuses to start
  otherwise. What an earlier machined left there goes as it starts.
- `--cladefs-config FILE`: the daemon's configuration, cladefsd's TOML (`cladefsd --config`):
  its disks, bucket, KMS and memory budget. Unless given, one local disk in `<state>/cladefs`
  with a bucket of files there and a master key of its own (`<state>/cladefs/master.key`, made
  once): a developer's machine's. `<state>/cladefs` outlives machined's restarts, as cells do.
- `--socket PATH`: serve on a Unix socket, and/or
- `--tcp ADDR --tls-cert PEM --tls-key PEM --tls-client-ca PEM`: serve on TCP with mutual TLS.

SIGTERM or SIGINT stops it: it stops serving, stops every machine, and exits. Machines are its
process's: they outlive their callers' connections (a VM manager that restarts lists what runs
and takes it up), not machined (board 13492).

Each machine's VM runs in a helper process of its own, `machined-vm` (board 13460): machined, the
long-lived daemon with the API, starts one per machine start, from beside its own binary
(`--helper` names another), and speaks to it over the helper's standard input
(`src/helper/wire.rs`, this build's alone). machined keeps no VM itself: as many machines run as
the host takes, on Hypervisor.framework too (one VM per process); a helper that dies ends its
machine (FAILED, saying how its process ended) while machined and the others run on; and helpers
end with machined. `Machine.pid` is a running machine's helper.

On macOS `machined-vm` must be signed with the hypervisor entitlement, as every macOS executable
of this workspace is as it links (`scripts/macos-link.sh`). There, machines can't be snapshotted
or suspended yet (board 13399: UNIMPLEMENTED).

## Who may call it

- **The Unix socket** is served as cladefsd's control API is: mode 0600 from the moment it
  exists, whatever the umask, so only machined's user can connect; and each caller's peer
  credentials are checked as it connects (`SO_PEERCRED` on Linux, `getpeereid` on macOS): a
  caller that is neither machined's user nor root is disconnected at once. A path already there
  fails the start.
- **TCP** is served with mutual TLS only: callers present a certificate of the client CA given
  (`--tls-client-ca`), or the handshake fails. There is no TCP in the clear.
- Every caller may do everything: the API is for the host's VM manager. What a request asks is
  checked once, as it enters (`src/checks.rs`): names, paths, sizes, counts and limits, refused
  with INVALID_ARGUMENT; a guest's answers (its agent's files, its output) are bounded as well.

## The API

| Call | What |
|---|---|
| `CreateMachine` | A machine from a spec: vCPUs, memory, kernel, initramfs and command line, its network's egress rules, its root's cell, vsock ports the guest may connect out to, labels. CREATED. |
| `StartMachine` | Boots it; RUNNING once its vCPUs run. A stopped machine boots afresh. |
| `StopMachine` | Stops it, its guest's filesystems synced first through its agent if asked (`sync_filesystems`; left running if the sync fails). STOPPED. |
| `DeleteMachine` | Deletes a stopped machine, or any with `force`. |
| `GetMachine`, `ListMachines` | Its spec, state, last exit, vsock socket. |
| `WatchMachine` | A stream of its status: CREATED, STARTING, RUNNING, SUSPENDED, STOPPING, STOPPED, with how it last ended (STOPPED, POWEROFF, RESET, SHUTDOWN, BOOT_FAILED, FAILED). |
| `StreamConsole` | Its console from an offset (the last 1 MiB is kept, across starts), and, with `follow`, what comes after. |
| `Exec` | A command in its guest through libmachine's agent: a bidirectional stream of stdin, its close and signals in; its pid, stdout, stderr and exit out. A caller gone kills it. |
| `ReadFile`, `WriteFile` | A guest's file, out or in, in chunks, 64 MiB at most. |
| `SnapshotMachine`, `RestoreMachine`, `DeleteSnapshot` | KVM: a running machine's snapshot, by name, in machined's state directory; a new machine from it, RUNNING in milliseconds. |
| `SuspendMachine`, `ResumeMachine` | KVM: an idle guest's RAM to a file until a device has something for it, or a resume. |
| `Balloon` | A machine with `balloon` in its spec: its guest's free memory reported and given back to the host as it frees it, and a target set, which the guest's driver inflates to. |

Exec, ReadFile, WriteFile and the sync need libmachine's agent in the guest
(`libmachine_action=agent` on the test initramfs's command line). A guest with an agent of its own
(a VM manager's, as hotbed's guestd) reaches it through the machine's **vsock socket**
(`Machine.vsock_socket`), which speaks Firecracker's vsock protocol, so that a client written for
Firecracker's works unchanged: connect, send `CONNECT <port>\n`, read `OK <port>\n`, and the stream
is the guest's port's; the guest's connections to a port of the spec's `vsock_listen_ports` go to
the Unix socket `<vsock_socket>_<port>`, if one listens there. A Unix socket, not a gRPC stream:
the clients VM managers have for Firecracker work as they are, nothing frames each byte, a
connection lives apart from any call, and its access is the API socket's (mode 0600 in a mode-0700
directory, peer credentials checked).

Errors are gRPC statuses (the .proto's header says which); the API only grows (`go/compat.sh`
holds it to the last build's with `buf breaking`).

## Clients

- **Go:** `github.com/semistrict/cladefs/go/machined/v1` (`go/machined/v1`), generated by buf
  (`go/generate.sh`, which lints the .proto first with `crates/machined/proto/buf.yaml`).
- **Rust:** `machined::client` (`connect_unix`, or `MachinedClient` over any tonic channel).

## Layout

| File | What |
|---|---|
| `src/listen.rs` | The Unix socket and TCP with mutual TLS; the server's start and shutdown |
| `src/checks.rs` | Every request's checks, once, where it enters |
| `src/service.rs` | The gRPC service |
| `src/machines.rs` | Machines by name: their lifecycle, state and console |
| `src/vmm.rs` | The one seam over a machine's VM: its helper process, started and spoken to |
| `src/helper/` | The helper process (`src/bin/machined-vm.rs`): a machine's VM, its run thread, its vsock socket |
| `src/vsock.rs` | The vsock socket, Firecracker's protocol |
| `src/console.rs` | The console kept with offsets |
| `tests/machined.rs` | End to end over gRPC: KVM (`scripts/test.sh`'s libmachine part) and Hypervisor.framework (its hvf part) |

## A node of hotbed

With `--hotbed-controller URL --hotbed-token-file PATH --hotbed-node NAME --hotbed-kernel KERNEL`
(and `--hotbed-initramfs`, `--hotbed-command-line`, `--hotbed-vcpus`, `--hotbed-memory-mib` for
each workspace's machine), machined is a node of hotbed (`src/hotbed`, board 13896), ported from
hotbed's worker: it joins once (a join token, `hotbed org join-token create`, traded for the
node's worker token, kept in `<state>/hotbed-node.json`), heartbeats every 5 s, and keeps hotbed's
`/ws/worker` WebSocket to the controller, down which the controller calls WorkspaceService: a
workspace is a machine, a command an agent exec in it, its output followed and tailed. hotbed's
proto is copied in `hotbed/` (its README says from which commit).

Not served yet (PLATFORM_NOT_SUPPORTED, hotbed's unimplemented): Sync, files, shells, pause and
resume, output queries, input, inspection and profiles, daemon logs, upgrades. The node reports
no runtime and is not on hotbed's build, so hotbed's placement lists it but sends it no runs until
hotbed places on machined's runtime.

## What a hotbed node on a Mac still needs

hotbed's worker runs each workspace as a Firecracker VM today (`internal/vm` in hotbed); machined
is to run them on a Mac. Beyond what machined offers: a
port forwards into the guest (board 13486), and machines that
outlive a machined restart (board 13492). hotbed's guestd boots from a read-only tools disk under
Firecracker; under machined it comes in the initramfs (`Boot.initramfs`).
