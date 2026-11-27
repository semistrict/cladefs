// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Two extent daemons sharing a bucket, as two hosts do (TASK-406.2): a version made on one, forked
//! and read on the other, whose fetchers ask the first for the parts it owns. The second fetches
//! from the bucket only the parts it owns itself, and the first serves the rest from its part
//! cache, which its uploads left them in, fetching nothing; without peers, the second fetches
//! every part.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::proto::{
    ForkCellRequest, MakeVersionRequest, SetPeersRequest, StatCellRequest, SyncCellRequest,
};
use cladefs::testing::Control;
use cladefs::{CellClient, CellId, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CAPACITY: u64 = 1 << 30;
const EXTENTS: u64 = 16;
const EXTENT: usize = 1 << 20;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon of a host sharing a bucket: its process, its VM socket, a client of its control API,
/// its stats file, and the address it serves its metrics at.
struct Daemon {
    _process: KillOnDrop,
    vms: PathBuf,
    control: Control,
    stats: PathBuf,
    metrics: String,
}

/// A daemon serving in `dir`, keeping its cells in the file bucket at `bucket`, and its
/// part cache in `dir`; serving its peers at `peers`' first address, and asking them all, if given.
fn daemon(dir: &Path, bucket: &Path, peers: Option<&[String]>) -> Daemon {
    let disk = dir.join("disk0");
    std::fs::create_dir_all(&disk).unwrap();
    let (vms, control, stats) =
        (dir.join("cladefsd.sock"), dir.join("control.sock"), dir.join("stats"));
    let [metrics, _] = addresses();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(&vms)
        .env("CLADEFSD_METRICS", &metrics)
        .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_ROOTS", &disk)
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_STATS", &stats)
        .env("CLADEFSD_GRPC", &control);
    if let Some(peers) = peers {
        command.env("CLADEFSD_PEER_ADDRESS", &peers[0]);
        command.env("CLADEFSD_PEERS", peers.join(","));
    }
    let process = KillOnDrop(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let control = Control::connect(&control).unwrap();
    Daemon { _process: process, vms, control, stats, metrics }
}

/// Two loopback addresses no one listens at yet, for two daemons' peers.
fn addresses() -> [String; 2] {
    let listeners = [(); 2].map(|()| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    listeners.map(|listener| listener.local_addr().unwrap().to_string())
}

/// Bytes of extent `sequence` that don't compress.
fn data(sequence: u64) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ sequence;
    (0..EXTENT)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// The count `name` of the line of `what` in the stats file `file`, as last written.
fn stat(file: &Path, what: &str, name: &str) -> u64 {
    let text = std::fs::read_to_string(file).unwrap();
    let line = text.lines().find(|line| line.starts_with(&format!("{what}:"))).unwrap();
    let field = line.split(' ').find_map(|field| field.strip_prefix(&format!("{name}=")));
    field.unwrap_or_else(|| panic!("no {name} in {line}")).parse().unwrap()
}

/// The value of the series `series` (its name and labels) in the metrics the daemon serves at
/// `address`.
fn metric(address: &str, series: &str) -> u64 {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream.write_all(b"GET /metrics HTTP/1.0\r\n\r\n").unwrap();
    let mut page = String::new();
    stream.read_to_string(&mut page).unwrap();
    let value = page.lines().find_map(|line| line.strip_prefix(series)?.strip_prefix(' '));
    value.unwrap_or_else(|| panic!("no {series} in {page}")).parse().unwrap()
}

/// Waits for what the stats file `file` says of the bucket and the peers to stop changing: the
/// daemon writes it within 200 ms of each change. Its other lines change as it writes it, counting
/// its own writes.
fn settled(file: &Path) {
    let counts = || {
        let text = std::fs::read_to_string(file).unwrap();
        let read = |line: &&str| {
            ["object-store:", "peers:", "fetches:"].iter().any(|what| line.starts_with(what))
        };
        text.lines().filter(read).collect::<Vec<_>>().join("\n")
    };
    let (mut last, deadline) = (String::new(), Instant::now() + Duration::from_secs(30));
    loop {
        assert!(Instant::now() < deadline, "{} kept changing: {last}", file.display());
        std::thread::sleep(Duration::from_millis(500));
        let now = counts();
        if now == last {
            return;
        }
        last = now;
    }
}

/// What a run came to, of each host: its GETs of ranges, and its parts found at the bucket and at
/// peers, each a count and their stored bytes; and the parts the second's peers gave it, and the
/// first served, each a count and their stored bytes.
struct Ran {
    gets: [u64; 2],
    from_bucket: [(u64, u64); 2],
    from_peer: [(u64, u64); 2],
    given: (u64, u64),
    served: (u64, u64),
}

/// What a run came to ([`Ran`]): a version made on the first host, of [`EXTENTS`] extents, forked
/// and read whole on the second; each asking the other if `peered`. What the stats files say of the
/// peers and the fetches, the metrics say too.
fn run(peered: bool) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("first"), dir.path().join("second")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [first_at, second_at] = addresses();
    let peers = |me: &String, other: &String| vec![me.clone(), other.clone()];
    let (first_peers, second_peers) = (peers(&first_at, &second_at), peers(&second_at, &first_at));
    let first = daemon(&hosts[0], &bucket, peered.then_some(&first_peers[..]));
    let second = daemon(&hosts[1], &bucket, peered.then_some(&second_peers[..]));

    let client = CellClient::connect_new(&first.vms, 0, CAPACITY).unwrap();
    let cell = client.cell();
    for sequence in 1..=EXTENTS {
        let extent = ExtentId { cell, sequence };
        client.write(extent, 0, &[VolatileSlice::from(&data(sequence)[..])]).unwrap();
    }
    client.commit(None).unwrap();
    drop(client);
    first
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { tenant: 0, cell }).await)
        .map(drop)
        .unwrap();
    let version = first
        .control
        .call(async |client| {
            client.make_version(MakeVersionRequest { tenant: 0, cell, keep_last: 0 }).await
        })
        .map(|response| response.version)
        .unwrap();
    first
        .control
        .call(async |client| client.sync_cell(SyncCellRequest { tenant: 0, cell }).await)
        .map(drop)
        .unwrap();
    let stated = first
        .control
        .call(async |client| client.stat_cell(StatCellRequest { tenant: 0, cell }).await)
        .unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };

    let forked = second
        .control
        .call(async |client| {
            let cladefs::CellVersion { tenant, cell, version } = from;
            client.fork_cell(ForkCellRequest { tenant, cell, version, ..Default::default() }).await
        })
        .map(|response| CellId { tenant: response.tenant, id: response.cell })
        .unwrap();
    let client = CellClient::connect_cell(&second.vms, forked).unwrap();
    for sequence in 1..=EXTENTS {
        let mut read = vec![0; EXTENT];
        let extent = ExtentId { cell, sequence };
        client.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
        assert!(read == data(sequence), "extent {sequence} read otherwise");
    }
    drop(client);
    settled(&second.stats);
    settled(&first.stats);
    let hosts = [&second, &first];
    let gets = hosts.map(|daemon| stat(&daemon.stats, "object-store", "get-range"));
    let found = |source: &str| {
        hosts.map(|daemon| {
            let (count, bytes) = (
                stat(&daemon.stats, "fetches", source),
                stat(&daemon.stats, "fetches", &format!("{source}-bytes")),
            );
            let label = source.replace('-', "_");
            let series = |name| format!("cladefsd_{name}_total{{source=\"{label}\"}}");
            assert_eq!(metric(&daemon.metrics, &series("part_fetches")), count, "{source}");
            assert_eq!(metric(&daemon.metrics, &series("part_fetch_bytes")), bytes, "{source}");
            (count, bytes)
        })
    };
    let (from_bucket, from_peer) = (found("bucket"), found("peer"));
    let (given, served) = match peered {
        false => ((0, 0), (0, 0)),
        true => {
            let given = (
                stat(&second.stats, "peers", "given"),
                stat(&second.stats, "peers", "given-bytes"),
            );
            let served = (
                stat(&first.stats, "peers", "served"),
                stat(&first.stats, "peers", "served-bytes"),
            );
            let asked = stat(&second.stats, "peers", "asked");
            assert_eq!(metric(&second.metrics, "cladefsd_peer_asked_parts_total"), asked);
            assert_eq!(asked, given.0, "parts asked of the first host, which has them all");
            assert_eq!(metric(&second.metrics, "cladefsd_peer_given_bytes_total"), given.1);
            assert_eq!(metric(&first.metrics, "cladefsd_peer_served_bytes_total"), served.1);
            (given, served)
        }
    };
    Ran { gets, from_bucket, from_peer, given, served }
}

/// The second host fetches from the bucket only the parts it owns: the first gives it the rest,
/// from its part cache, fetching none of them: no GET more than it makes alone. A version's base
/// read on another host comes from the bucket once, not once on each host that reads it.
#[test]
fn a_copy_read_on_another_host_comes_from_its_peer() {
    let alone = run(false);
    let peered = run(true);
    let (given, given_bytes) = peered.given;
    assert!(given > 0, "the first host owned none of the parts read");
    assert_eq!(peered.served, peered.given, "parts served, and those given, with their bytes");
    assert_eq!(peered.from_peer[0], peered.given, "parts found at a peer, and given");
    assert_eq!(alone.from_peer, [(0, 0); 2], "parts found at a peer, with none");
    // The second host's parts, from the bucket alone, or from it and its peer: the same parts.
    let (bucket, bucket_bytes) = peered.from_bucket[0];
    assert_eq!(alone.from_bucket[0], (bucket + given, bucket_bytes + given_bytes));
    assert_eq!(peered.from_bucket[1], alone.from_bucket[1], "the first host fetched to serve");
    let (alone, first_alone, second, first) =
        (alone.gets[0], alone.gets[1], peered.gets[0], peered.gets[1]);
    assert_eq!(second, alone - given, "the second host's GETs of ranges, alone {alone}");
    assert_eq!(first, first_alone, "the first host fetched what it served");
    println!(
        "{EXTENTS} MiB read on a second host: {alone} ranged GETs alone, {second} with peers, \
         {given} parts given by the first host ({given_bytes} bytes), which made {first} ranged \
         GETs either way"
    );
}

/// The control API replaces a daemon's peers at any time (`SetPeers`): an address that is no
/// `HOST:PORT` is refused (EINVAL), and a daemon that serves no peers refuses them all (ENOTSUP).
#[test]
fn peers_are_set_at_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("peered"), dir.path().join("alone")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [me, other] = addresses();
    let peered = daemon(&hosts[0], &bucket, Some(&[me.clone()]));
    peered
        .control
        .call(async |client| {
            client
                .set_peers(SetPeersRequest {
                    addresses: (&[&me, &other])
                        .iter()
                        .map(|address| (*address).to_owned())
                        .collect(),
                })
                .await
        })
        .map(drop)
        .unwrap();
    let refused = peered
        .control
        .call(async |client| {
            client
                .set_peers(SetPeersRequest {
                    addresses: (&[&me, "nowhere"])
                        .iter()
                        .map(|address| (*address).to_owned())
                        .collect(),
                })
                .await
        })
        .map(drop)
        .unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EINVAL));
    let alone = daemon(&hosts[1], &bucket, None);
    let refused = alone
        .control
        .call(async |client| {
            client
                .set_peers(SetPeersRequest {
                    addresses: (&[&me]).iter().map(|address| (*address).to_owned()).collect(),
                })
                .await
        })
        .map(drop)
        .unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::ENOTSUP));
}

/// A part cache budget that is no positive number of bytes stops the daemon at its start, naming
/// the variable (TASK-406.1).
#[test]
fn a_cache_budget_the_daemon_cant_take_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(dir.path().join("cladefsd.sock"))
        .env("CLADEFSD_BUCKET", format!("file://{}/", dir.path().display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_ROOTS", dir.path())
        .env("CLADEFSD_PART_CACHE", dir.path().join("cache"))
        .env("CLADEFSD_PART_CACHE_BYTES", "1G")
        .output()
        .unwrap();
    assert!(!output.status.success(), "the daemon started");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("CLADEFSD_PART_CACHE_BYTES must be a positive number of bytes: \"1G\""),
        "{said}"
    );
}
