# machined

The server libmachine is for (board 13467): one process running many machines, each a Linux
guest on libmachine (`crates/libmachine`, doc-30), managed over a gRPC API of its own,
`machined.v1.Machined` ([`proto/machined/v1/machined.proto`](proto/machined/v1/machined.proto)).
It is to embed cladefs's extent daemon too, one per process, shared by its machines (libcladefs,
board 13471), and serve that daemon's `cladefs.cladefsd.v1.Cladefsd` on the same endpoint, so
that one endpoint manages both cells and machines: both wait on boards 13067 and 13471.

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
- `--socket PATH`: serve on a Unix socket, and/or
- `--tcp ADDR --tls-cert PEM --tls-key PEM --tls-client-ca PEM`: serve on TCP with mutual TLS.

SIGTERM or SIGINT stops it: it stops serving, stops every machine, and exits. Machines are its
process's: they outlive their callers' connections (a VM manager that restarts lists what runs
and takes it up), not machined (board 13492).

On macOS the binary must be signed with the hypervisor entitlement, as every macOS executable of
this workspace is as it links (`scripts/macos-link.sh`). There, until board 13460 runs each
machine in a helper process, one machine runs at a time (Hypervisor.framework's one VM per
process: a second start fails with RESOURCE_EXHAUSTED), and machines have no network and can't be
snapshotted or suspended yet (board 13399: UNIMPLEMENTED).

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
| `src/vmm.rs` | The one seam over libmachine: a machine's run thread; board 13460's helper processes slot in here |
| `src/vsock.rs` | The vsock socket, Firecracker's protocol |
| `src/console.rs` | The console kept with offsets |
| `tests/machined.rs` | End to end over gRPC: KVM (`scripts/test.sh`'s libmachine part) and Hypervisor.framework (its hvf part) |

## What a hotbed node on a Mac still needs

hotbed's worker runs each workspace as a Firecracker VM today (`internal/vm` in hotbed); machined
is to run them on a Mac. Beyond what machined offers: roots on cells (boards 13067, 13471), a
network on Hypervisor.framework (board 13399), port forwards into the guest (board 13486), a
balloon (board 13490), machines past one per macOS process (board 13460), and machines that
outlive a machined restart (board 13492). hotbed's guestd boots from a read-only tools disk under
Firecracker; under machined it comes in the initramfs (`Boot.initramfs`).
