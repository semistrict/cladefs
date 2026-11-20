// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's host tool for VMs' disks, the extent stores cladefsd serves: makes them, of cells kept
//! in object storage (backlog doc-7) or of container images, places, forks, deletes and resizes
//! them, makes them public bases, and waits for their uploads:
//!
//! ```sh
//! cladefsctl take [--force] DIR CAPACITY CELL [TOKEN]
//! cladefsctl create DIR CAPACITY
//! cladefsctl place [--force] DIR TOKEN
//! cladefsctl sync DIR
//! cladefsctl import DIR CAPACITY IMAGE [REF]
//! cladefsctl fork SRC DST
//! cladefsctl version DIR
//! cladefsctl versions DIR
//! cladefsctl drop-versions DIR VERSION...
//! cladefsctl share DIR VERSION
//! cladefsctl unshare DIR VERSION
//! cladefsctl fork-version CELL VERSION DST
//! cladefsctl delete DIR
//! cladefsctl drop DIR
//! cladefsctl resize DIR CAPACITY
//! cladefsctl public DIR
//! cladefsctl stat DIR
//! cladefsctl stats [--json]
//! cladefsctl grant TENANT DIR [UID [GID]]
//! cladefsctl revoke TENANT
//! cladefsctl shred-cell TENANT/CELL
//! cladefsctl forget-tenant-keys TENANT
//! cladefsctl shred-tenant TENANT
//! cladefsctl mount attach TENANT CELL [TOKEN]
//! cladefsctl mount create TENANT CAPACITY [TOKEN]
//! cladefsctl mount detach [--force] TENANT CELL
//! cladefsctl mount list
//! cladefsctl format-bucket URL [DETAILS]
//! cladefsctl add-bucket ROOT URL [DETAILS]
//! cladefsctl buckets URL
//! cladefsctl --version
//! ```
//!
//! With `CLADEFSD_GRPC` naming the control socket of the extent daemon (TASK-43.5), every
//! command goes through its control API (`cladefs::cladefsd::control`), and acts on stores as the
//! daemon serves them: on a store a VM has open, live, or not at all (`EBUSY`). Without, cladefsctl
//! acts on stores itself, on stores nothing has open, as their lock says, which an extent daemon
//! serving them holds (it waits a while for it, then refuses). `create`, `take` and `import`, which
//! make new stores, work so anywhere, as when building base images where no daemon runs. The others change
//! existing stores' cells, which are in the bucket each store's description names: they need
//! this host's (`CLADEFSD_BUCKET`) and its placement's fencing token (`CLADEFSD_TOKEN`),
//! and refuse otherwise (`cladefs::bucket::Host`, TASK-45).
//!
//! `create` makes an empty store at DIR, of CAPACITY bytes, of a new cell, a new cell's of the
//! default tenant's (doc-20), its ID its bucket's to give (TASK-364), and prints the cell.
//! `take` makes an empty store at DIR, of CAPACITY bytes, of CELL (`TENANT/CELL`, each in base 32,
//! its cell's, TASK-364), which its bucket has (`ENOENT` otherwise), its taking of a replicated
//! cell forced with `--force`, as `place --force` places a store (TASK-681): once
//! cladefsd opens it,
//! with `CLADEFSD_BUCKET` naming the cell's bucket, it takes what the cell holds there,
//! whose data it fetches as it is read; placed at TOKEN, if given, as `place` places it. A
//! version, a VM snapshot's or another, it reads, taking no head, as any number of hosts do at once.
//! `place` places the store at DIR at the placement whose fencing token,
//! its boot ID, is TOKEN (TASK-39.7): its writer takes its cell in object storage with that token
//! from then on, which a later placement's exceeds, so that a placement cut off takes no cell
//! back from the one after it (`ESTALE`), and its commits fail once a later one takes it.
//! `--force`d, that placement's takings of a replicated cell go on with the members of the
//! configuration its head names that answer, fewer than a take needs, as when f+1 of them are
//! gone for good (doc-21, TASK-681): commits told replicated that only the others held are lost,
//! and the store's status says past which commit (`forced_after`). `sync`
//! returns once every commit of the store at DIR is durable and in object storage, as before a
//! clean stop: through the daemon, a store a VM has open as of then. `import` makes a store at DIR, of CAPACITY bytes,
//! holding an Fxfs filesystem of the container image IMAGE (TASK-39.5), a base for VMs to boot
//! forks of: IMAGE is an OCI image layout, as `skopeo copy` makes, or `docker save` (whose tarball
//! is one, once extracted), holding the image REF names, or its only image; or a tar archive of one
//! layer, gzipped or not, as `docker export` makes. Files keep their owners, permissions, times
//! and extended attributes; see `cladefs::import`. `fork` copies the store at SRC to a new store at
//! DST, of a new cell, which it prints: through the daemon, live, if a VM has SRC open, once
//! nothing is open on it, as with the guest's filesystem frozen (fsfreeze), and refused otherwise
//! (TASK-39.3). The new cell is a fork of SRC's in its bucket, made once SRC's commits are
//! uploaded, holding its extents without uploading them again; DST is a store of its own, which
//! reads them there (TASK-165.7). `version` makes a version of the cell of the store at DIR, once
//! nothing is open on it: one commit here, uploaded behind, which it prints the ID of, in
//! hexadecimal. `fork-version` forks the version VERSION of CELL (TENANT/CELL) to DST, on any
//! host, as many hosts at once as wanted, the version uploaded first. `versions` lists the cell's versions,
//! `drop-versions` drops some, what forks of them inherited staying until they go. `share` shares
//! a version (TASK-395.7): it forks into cells of other tenants' than its own, as a base image
//! many tenants fork, which a version not shared refuses (`EACCES`); `unshare` stops such forks,
//! those made living on. The cell's head says it, for every
//! host. `delete` deletes a store, a fork or not: its cell in its bucket, its forks' living on,
//! and its directory (TASK-39.4). `drop` deletes the store at DIR as `delete` does, this
//! host's copy of it, its cell staying in the bucket (TASK-154): what `take` made, once this host
//! is done with it. `resize` sets the store's capacity, the size of the disk
//! its guest sees, to CAPACITY bytes: through the daemon, a store its VM has open grows at once,
//! the guest seeing the new size while mounted, and shrinks if the guest's filesystem agrees, no
//! lower than what it holds; otherwise, it grows, and does not shrink. `public` makes the store at
//! DIR a public base, whose pages every tenant's forks of it share (TASK-43). `stat` prints what the
//! store at DIR is and holds, one `name value` a line: its cell (base 32), capacity, used and
//! available bytes, and of those used, its own (its cell wrote them) and inherited (from the
//! cells it was forked from), what billing it alone charges it and what it shares (TASK-49);
//! how many extents it holds, own and inherited (`own_extents`, `inherited_extents`), which what an
//! open store takes of the daemon's memory grows with (TASK-443.2, TASK-454); whether it is public,
//! and, through the daemon, open. A store no VM has open, it opens, locally or through the daemon,
//! as a VM's would, to say: time, and memory till it is done, as its extents. `--version` prints the commit it was built
//! of (`cladefs::BUILD_COMMIT`, TASK-111.7).
//!
//! cladefs: on Linux, it logs on standard error what is as grave as a warning: as a read of an object
//! the collector's newest mark-only pass marked, a bug in the collector (`bucket_cell::marks`,
//! TASK-264).
//!
//! `stats`, through the daemon alone, prints what the daemon, its VM processes and their guests
//! counted, as of one moment, each with the commit it was built of (the control API's
//! `GetStats`, TASK-111.4): with `--json`, as one JSON object, of `taken_unix_ns`, `cladefsd`
//! (`build`, `counters`, `gauges`) and `vms`, each VM process's `connection`, `cell` (base 32),
//! `path`, `follower`, `vmm` (`build`, `counters`, `gauges`), `window`, `answered`, `answer_ns` and
//! `guest` (`build`, `counters`, `histograms`), null where it has none; otherwise as lines of a
//! path through that object and a value, `vms.0.guest.counters.seals 12`. Counters only grow: a
//! measurement is the difference of two.
//!
//! `grant` and `revoke`, through the daemon alone, are the control API's `Grant` and `Revoke`
//! (TASK-43.4), for tests and operators (TASK-71): `grant` gives the tenant TENANT, its ID in the
//! bucket, in base 32 and not 0, the default tenant's (TASK-364), whose stores are in the
//! directory DIR, a VM socket of its own, `TENANT.sock`, its VM processes naming stores in DIR
//! alone, of its own cells, the socket given to the user UID and the group GID if named, the
//! daemon's own otherwise, and prints its path; granting a tenant again, of the same directory,
//! prints it again. `revoke` takes the tenant TENANT's socket away (base 32): its VM processes
//! connected keep their stores, and no more connect.
//!
//! `shred-cell` and `forget-tenant-keys`, through the daemon alone, are the control API's
//! `ShredCell` and `ForgetTenantKeys` (TASK-411.7, backlog doc-16, Shredding). `shred-cell` shreds
//! the cell TENANT/CELL (each in base 32): takes its wrapped KEKs out of its object in the daemon's
//! bucket, so that its keys unwrap no more, and has the daemon forget what it holds of
//! them, halting its stores open there (`EACCES`); run it against every host's daemon, the first
//! shredding, each forgetting. A bucket that keeps object versions keeps the KEKs in the cell
//! object's older versions until a lifecycle rule expires them. `forget-tenant-keys` has the daemon
//! forget the keys of the tenant TENANT (base 32), once its KEK is destroyed, or revoked, in the KMS:
//! its stores there are halted, their pages leave the cache, its fetchers' and KMS's keys of it
//! go. `shred-tenant`, through the daemon alone too, is the control API's `ShredTenant`
//! (TASK-411.27, doc-16 The bucket KMS): under the bucket KMS (a master key, no `CLADEFS_KMS`), it
//! replaces the tenant TENANT's (base 32) KEK object in the daemon's bucket by a tombstone, so that no
//! key its KEK wrapped unwraps anywhere and none is made it again, and has the daemon forget its
//! keys; run it against every host's daemon, the first shredding, each forgetting, and printing
//! `shredded_now 1` or `0`. A bucket that keeps object versions keeps the KEK in the object's
//! older versions until a lifecycle rule expires them. A daemon of another KMS refuses it: destroy
//! the tenant's KEK there, then `forget-tenant-keys`. Each prints what the daemon forgot, one `name
//! value` a line.
//!
//! `format-bucket`, `add-bucket` and `buckets` act on buckets themselves, with or without the daemon
//! (TASK-633.9, backlog doc-23). A bucket's number is in the high bits of every cell ID it gives,
//! so that no two buckets give one ID. `format-bucket` makes the bucket at URL, unless it was
//! formatted or added, the root of buckets of its own, number 1, and prints its number; a bucket
//! never formatted is made a root so by the first host that makes a cell in it. `add-bucket` adds
//! the bucket at URL, never formatted, to the buckets of the root bucket at ROOT: the next number,
//! which it prints, given by a conditional write of the root's record of every bucket, which then
//! goes to the root of every bucket. Refused if URL is a bucket already (`EEXIST`), or ROOT is not
//! the root (`EINVAL`). DETAILS, recorded of the bucket, are any of `--region REGION`, `--endpoint
//! URL`, `--kms URI` (its `CLADEFS_KMS`) and `--master-key-id ID` (under the bucket KMS): never a
//! key or a credential. Its provider and URL are recorded too. `buckets` prints the record in
//! the root of the bucket at URL: its version, then each bucket, a line each, its number and its
//! details, `name=value`.
//!
//! `mount`, through the daemon alone, calls the Mount service of its FUSE mount of tenants' cells
//! (`CLADEFSD_MOUNT`, TASK-475.2; `proto/cladefs/mount/v1/mount.proto`), TENANT and CELL in base 32
//! (`base32_id`).
//! `attach` takes CELL from the daemon's bucket, at the placement whose fencing token is TOKEN if
//! given, the daemon's otherwise, and shows it, if an Fxfs cell, at `MOUNT/TENANT/CELL`; it
//! prints the cell. `create` makes a new Fxfs cell of TENANT's, its filesystem empty, of CAPACITY
//! bytes, attached so, and prints the same.
//! `detach` takes the cell away, and returns once its store is unmounted and written
//! back: once nothing has a file of it open, or a directory of it as its current one; with
//! `--force`, at once, what is open of it failing from then on. `list` prints the cells attached,
//! a line each, `TENANT CELL`, and `detaching` after one being detached.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]
// cladefs: its commands' futures, which hold the stores' (TASK-685.3), nest deeper than the default.
#![recursion_limit = "256"]

use anyhow::{Context as _, bail};
use cladefs::OpenCell;
use std::path::{Path, PathBuf};

/// The ID `given` writes, in base 32 (`base32_id`): `what`, as a refusal names it.
fn id(what: &str, given: &str) -> anyhow::Result<u64> {
    base32_id::parse(given).with_context(|| format!("{what} must be in base 32"))
}

/// A version's ID, in hexadecimal, as `version` prints it.
fn version(id: &str) -> anyhow::Result<u64> {
    u64::from_str_radix(id, 16).map_err(|_| anyhow::anyhow!("bad version {id:?}: hexadecimal"))
}

/// Prints `versions`, one a line: its ID, in hexadecimal, and whether it is uploaded and shared.
fn print_versions(versions: &[cladefs::VersionInfo]) {
    for cladefs::VersionInfo { id, uploaded, shared } in versions {
        let uploaded = if *uploaded { "uploaded" } else { "local" };
        let shared = if *shared { " shared" } else { "" };
        println!("{id:x} {uploaded}{shared}");
    }
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "usage: cladefsctl take [--force] DIR CAPACITY CELL [TOKEN] | cladefsctl create DIR CAPACITY | \
         cladefsctl place [--force] DIR TOKEN | \
         cladefsctl sync DIR | cladefsctl import DIR CAPACITY IMAGE [REF] | cladefsctl fork SRC DST | \
         cladefsctl version DIR | cladefsctl versions DIR | \
         cladefsctl drop-versions DIR VERSION... | cladefsctl share DIR VERSION | \
         cladefsctl unshare DIR VERSION | cladefsctl fork-version CELL VERSION DST | \
         cladefsctl delete DIR | cladefsctl drop DIR | \
         cladefsctl resize DIR CAPACITY | cladefsctl public DIR | \
         cladefsctl public-attach-only DIR | cladefsctl attach DIR BASE | cladefsctl stat DIR | \
         cladefsctl stats [--json] | cladefsctl grant TENANT DIR [UID [GID]] | \
         cladefsctl revoke TENANT | cladefsctl shred-cell TENANT/CELL | \
         cladefsctl forget-tenant-keys TENANT | cladefsctl shred-tenant TENANT | \
         cladefsctl mount attach TENANT CELL [TOKEN] | \
         cladefsctl mount create TENANT CAPACITY [TOKEN] | \
         cladefsctl mount detach [--force] TENANT CELL | cladefsctl mount list | \
         cladefsctl format-bucket URL [DETAILS] | cladefsctl add-bucket ROOT URL [DETAILS] | \
         cladefsctl buckets URL | cladefsctl --version"
    )
}

/// cladefs: what a daemon forgot of a tenant's or a cell's keys, one `name value` a line (TASK-411.7).
#[cfg(target_os = "linux")]
fn forgotten_lines(forgotten: &cladefs::cladefsd::control::proto::Forgotten) -> String {
    let cladefs::cladefsd::control::proto::Forgotten { stores, rings, kept, extents, bytes } =
        forgotten;
    format!(
        "stores_halted {stores}\nrings_forgotten {rings}\nkept_dropped {kept}\n\
         extents_evicted {extents}\nbytes_evicted {bytes}\n"
    )
}

/// What cladefsctl was asked to do.
enum Command {
    /// cladefs: a new store of a new cell, its ID its bucket's to give (TASK-364).
    Create {
        dir: PathBuf,
        capacity: u64,
    },
    Take {
        dir: PathBuf,
        capacity: u64,
        cell: cladefs::CellId,
        token: Option<u64>,
        force: bool,
    },
    Place {
        dir: PathBuf,
        token: u64,
        force: bool,
    },
    Sync {
        dir: PathBuf,
    },
    Import {
        dir: PathBuf,
        capacity: u64,
        image: PathBuf,
        reference: Option<String>,
    },
    Fork {
        src: PathBuf,
        dst: PathBuf,
    },
    /// cladefs: a version made (doc-20).
    Version {
        dir: PathBuf,
    },
    Versions {
        dir: PathBuf,
    },
    DropVersions {
        dir: PathBuf,
        versions: Vec<u64>,
    },
    /// cladefs: a version shared, or no longer (TASK-395.7).
    Share {
        dir: PathBuf,
        version: u64,
        shared: bool,
    },
    /// cladefs: a version forked, on any host.
    ForkVersion {
        from: cladefs::CellVersion,
        dst: PathBuf,
    },
    Delete {
        dir: PathBuf,
    },
    Drop {
        dir: PathBuf,
    },
    Resize {
        dir: PathBuf,
        capacity: u64,
    },
    Public {
        dir: PathBuf,
    },
    PublicAttachOnly {
        dir: PathBuf,
    },
    Attach {
        dir: PathBuf,
        base: PathBuf,
    },
    Stat {
        dir: PathBuf,
    },
    /// cladefs: the stats of the daemon, its VM processes and their guests, as JSON if `json`
    /// (TASK-111.4). Read by `through`, which is Linux's alone.
    Stats {
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        json: bool,
    },
    /// cladefs: a tenant, of the ID `tenant` in the bucket (TASK-364), granted a VM socket of its
    /// own, for its stores in `dir`, given to `uid` and `gid` if named (TASK-71). Read by
    /// `through`, which is Linux's alone.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Grant {
        tenant: u64,
        dir: PathBuf,
        uid: Option<u32>,
        gid: Option<u32>,
    },
    /// cladefs: a tenant's grant revoked (TASK-71).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Revoke {
        tenant: u64,
    },
    /// cladefs: a cell shredded (TASK-411.7).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ShredCell {
        cell: cladefs::CellId,
    },
    /// cladefs: a tenant's keys forgotten, its KEK revoked or destroyed (TASK-411.7).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ForgetTenantKeys {
        tenant: u64,
    },
    /// cladefs: a tenant shredded under the bucket KMS (TASK-411.27).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ShredTenant {
        tenant: u64,
    },
    /// cladefs: the daemon's FUSE mount of cells (TASK-475.2).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Mount(Mount),
    /// cladefs: a bucket formatted, a root of its own unless formatted or added (TASK-633.9).
    FormatBucket {
        url: String,
        details: bucket_cell::identity::Details,
    },
    /// cladefs: a bucket added to a root bucket's (TASK-633.9).
    AddBucket {
        root: String,
        url: String,
        details: bucket_cell::identity::Details,
    },
    /// cladefs: the record of every bucket in a bucket's root (TASK-633.9).
    Buckets {
        url: String,
    },
}

/// What cladefsctl was asked to do with the daemon's FUSE mount of cells (`cladefsctl mount`,
/// TASK-475.2): its Mount service's calls.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Mount {
    /// A cell attached, at `token`'s placement if given; or, `capacity` given, a new one of its
    /// tenant's, a new Fxfs cell of `capacity` bytes.
    Attach { cell: cladefs::CellId, capacity: u64, token: Option<u64> },
    /// A cell detached, at once if `force`.
    Detach { cell: cladefs::CellId, force: bool },
    /// The cells attached.
    List,
}

impl Mount {
    /// The `mount` command `args` say, the words after `mount`.
    fn parse(args: &[&str]) -> anyhow::Result<Self> {
        let cell = |tenant: &str, cell: &str| -> anyhow::Result<cladefs::CellId> {
            Ok(cladefs::CellId { tenant: id("TENANT", tenant)?, id: id("CELL", cell)? })
        };
        let token = |rest: &[&str]| -> anyhow::Result<Option<u64>> {
            let parsed = rest.first().map(|token| token.parse()).transpose();
            parsed.context("TOKEN must be a number")
        };
        Ok(match args {
            ["attach", tenant, id, rest @ ..] if rest.len() <= 1 => {
                Self::Attach { cell: cell(tenant, id)?, capacity: 0, token: token(rest)? }
            }
            ["create", tenant, capacity, rest @ ..] if rest.len() <= 1 => Self::Attach {
                cell: cell(tenant, "0")?,
                capacity: capacity.parse().context("CAPACITY must be a number")?,
                token: token(rest)?,
            },
            ["detach", "--force", tenant, id] => {
                Self::Detach { cell: cell(tenant, id)?, force: true }
            }
            ["detach", tenant, id] => Self::Detach { cell: cell(tenant, id)?, force: false },
            ["list"] => Self::List,
            _ => bail!(usage()),
        })
    }
}

#[cfg(target_os = "linux")]
impl Mount {
    /// Calls the Mount service of the daemon whose control socket is at `socket`, and prints what
    /// it answers: `attach` and `create`, the cell; `list`, each cell, `TENANT CELL`, `detaching`
    /// after one being detached. IDs padded, as the mount's paths have them.
    fn through(self, socket: &Path) -> anyhow::Result<()> {
        let failed = |err: cladefs::Error| anyhow::anyhow!("{err:#}");
        let mount = cladefs::fuse::control::Client::connect(socket).map_err(failed)?;
        match self {
            Self::Attach { cell, capacity, token } => {
                let cell = mount.attach(cell, capacity, token).map_err(failed)?;
                println!("{}", base32_id::Padded(cell.id));
            }
            Self::Detach { cell, force } => mount.detach(cell, force).map_err(failed)?,
            Self::List => {
                for cell in mount.list().map_err(failed)? {
                    let detaching = if cell.detaching { " detaching" } else { "" };
                    let (tenant, id) =
                        (base32_id::Padded(cell.tenant), base32_id::Padded(cell.cell));
                    println!("{tenant} {id}{detaching}");
                }
            }
        }
        Ok(())
    }
}

impl Command {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let capacity = |capacity: &str| capacity.parse().context("CAPACITY must be a number");
        let token = |token: &str| token.parse().context("TOKEN must be a number");
        let id = |id: &str, what| id.parse().with_context(|| format!("{what} must be a number"));
        let tenant = |tenant: &str| self::id("TENANT", tenant);
        Ok(match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["take", "--force", dir, size, cell, ref rest @ ..] if rest.len() <= 1 => Self::Take {
                dir: dir.into(),
                capacity: capacity(size)?,
                cell: cell.parse().map_err(anyhow::Error::msg).context("CELL")?,
                token: rest.first().map(|rest| token(rest)).transpose()?,
                force: true,
            },
            ["take", dir, size, cell, ref rest @ ..] if rest.len() <= 1 => Self::Take {
                dir: dir.into(),
                capacity: capacity(size)?,
                cell: cell.parse().map_err(anyhow::Error::msg).context("CELL")?,
                token: rest.first().map(|rest| token(rest)).transpose()?,
                force: false,
            },
            ["create", dir, size] => Self::Create { dir: dir.into(), capacity: capacity(size)? },
            ["place", dir, given] => {
                Self::Place { dir: dir.into(), token: token(given)?, force: false }
            }
            ["place", "--force", dir, given] => {
                Self::Place { dir: dir.into(), token: token(given)?, force: true }
            }
            ["sync", dir] => Self::Sync { dir: dir.into() },
            ["import", dir, size, image, ref rest @ ..] if rest.len() <= 1 => Self::Import {
                dir: dir.into(),
                capacity: capacity(size)?,
                image: image.into(),
                reference: rest.first().map(|reference| reference.to_string()),
            },
            ["fork", src, dst] => Self::Fork { src: src.into(), dst: dst.into() },
            ["version", dir] => Self::Version { dir: dir.into() },
            ["versions", dir] => Self::Versions { dir: dir.into() },
            ["drop-versions", dir, ref ids @ ..] if !ids.is_empty() => Self::DropVersions {
                dir: dir.into(),
                versions: ids.iter().map(|id| version(id)).collect::<anyhow::Result<_>>()?,
            },
            ["share", dir, id] => {
                Self::Share { dir: dir.into(), version: version(id)?, shared: true }
            }
            ["unshare", dir, id] => {
                Self::Share { dir: dir.into(), version: version(id)?, shared: false }
            }
            ["fork-version", cell, id, dst] => {
                let cell: cladefs::CellId = cell.parse().map_err(anyhow::Error::msg)?;
                let from = cladefs::CellVersion {
                    tenant: cell.tenant,
                    cell: cell.id,
                    version: version(id)?,
                };
                Self::ForkVersion { from, dst: dst.into() }
            }
            ["delete", dir] => Self::Delete { dir: dir.into() },
            ["drop", dir] => Self::Drop { dir: dir.into() },
            ["resize", dir, size] => Self::Resize { dir: dir.into(), capacity: capacity(size)? },
            ["public", dir] => Self::Public { dir: dir.into() },
            ["public-attach-only", dir] => Self::PublicAttachOnly { dir: dir.into() },
            ["attach", dir, base] => Self::Attach { dir: dir.into(), base: base.into() },
            ["stat", dir] => Self::Stat { dir: dir.into() },
            ["stats"] => Self::Stats { json: false },
            ["stats", "--json"] => Self::Stats { json: true },
            ["grant", given, dir, ref ids @ ..] if ids.len() <= 2 => Self::Grant {
                tenant: tenant(given)?,
                dir: dir.into(),
                uid: ids.first().map(|uid| id(uid, "UID")).transpose()?,
                gid: ids.get(1).map(|gid| id(gid, "GID")).transpose()?,
            },
            ["revoke", given] => Self::Revoke { tenant: tenant(given)? },
            ["shred-cell", cell] => {
                let (tenant, id) = cell.split_once('/').context("CELL must be TENANT/CELL")?;
                let cell = cladefs::CellId {
                    tenant: self::id("TENANT", tenant)?,
                    id: self::id("CELL", id)?,
                };
                Self::ShredCell { cell }
            }
            ["forget-tenant-keys", given] => Self::ForgetTenantKeys { tenant: tenant(given)? },
            ["shred-tenant", given] => Self::ShredTenant { tenant: tenant(given)? },
            ["mount", ref rest @ ..] => Self::Mount(Mount::parse(rest)?),
            ["format-bucket", url, ref rest @ ..] => {
                Self::FormatBucket { url: url.to_owned(), details: details(rest)? }
            }
            ["add-bucket", root, url, ref rest @ ..] => Self::AddBucket {
                root: root.to_owned(),
                url: url.to_owned(),
                details: details(rest)?,
            },
            ["buckets", url] => Self::Buckets { url: url.to_owned() },
            _ => bail!(usage()),
        })
    }

    /// Does it through the control API of the daemon whose control socket is at `socket`.
    #[cfg(target_os = "linux")]
    fn through(self, socket: &Path) -> anyhow::Result<()> {
        if self.is_on_buckets() {
            return self.on_buckets();
        }
        if let Self::Mount(mount) = self {
            return mount.through(socket);
        }
        let daemon = cladefs::cladefsd::control::Client::connect(socket)?;
        match self {
            Self::Create { dir, capacity } => {
                let new = cladefs::CellId { tenant: 0, id: 0 };
                println!("{}", daemon.create_cell(&dir, capacity, new, (None, false))?);
            }
            Self::Take { dir, capacity, cell, token, force } => {
                daemon.create_cell(&dir, capacity, cell, (token, force))?;
            }
            Self::Place { dir, token, force } => daemon.place_cell(&dir, token, force)?,
            Self::Sync { dir } => daemon.sync_cell(&dir)?,
            Self::Import { dir, capacity, image, reference } => {
                daemon.import_image(&dir, capacity, &image, reference.as_deref())?;
            }
            Self::Fork { src, dst } => println!("{}", daemon.fork_cell(&src, &dst)?),
            Self::Version { dir } => println!("{:x}", daemon.make_version(&dir, None)?),
            Self::Versions { dir } => print_versions(&daemon.list_versions(&dir)?),
            Self::DropVersions { dir, versions } => daemon.drop_versions(&dir, &versions)?,
            Self::Share { dir, version, shared } => daemon.share_version(&dir, version, shared)?,
            Self::ForkVersion { from, dst } => println!("{}", daemon.fork_version(from, &dst)?),
            Self::Delete { dir } => daemon.delete_cell(&dir)?,
            Self::Drop { dir } => daemon.drop_cell(&dir)?,
            Self::Resize { dir, capacity } => daemon.resize_cell(&dir, capacity)?,
            Self::Public { dir } => daemon.make_public(&dir)?,
            Self::PublicAttachOnly { dir } => daemon.make_public_attach_only(&dir)?,
            Self::Attach { dir, base } => daemon.attach_base(&dir, &base)?,
            Self::Stat { dir } => {
                let stat = daemon.stat_cell(&dir)?;
                let cell = cladefs::CellId { tenant: stat.tenant, id: stat.cell };
                let whose = |own, inherited| cladefs::Whose { own, inherited };
                print!(
                    "{}",
                    Stat {
                        cell: cladefs::CellId { tenant: cell.tenant, id: stat.cell },
                        capacity: stat.capacity,
                        used: stat.used,
                        available: stat.available,
                        holding: cladefs::CellHolding {
                            bytes: whose(stat.own, stat.inherited),
                            extents: whose(stat.own_extents, stat.inherited_extents),
                        },
                        public: stat.public,
                        open: Some(stat.open),
                    }
                );
            }
            Self::Stats { json } => {
                let stats = stats_json(&daemon.stats(None)?);
                match json {
                    true => println!("{}", serde_json::to_string_pretty(&stats)?),
                    false => print!("{}", flattened(&stats)),
                }
            }
            Self::Grant { tenant, dir, uid, gid } => {
                println!("{}", daemon.grant(tenant, &dir, uid, gid)?.display());
            }
            Self::Revoke { tenant } => daemon.revoke(tenant)?,
            Self::ShredCell { cell } => {
                let (removed, forgotten) = daemon.shred_cell(cell)?;
                print!("keks_removed {removed}\n{}", forgotten_lines(&forgotten));
            }
            Self::ForgetTenantKeys { tenant } => {
                let why =
                    format!("tenant {tenant}: its KEK is revoked or destroyed, cladefsctl says");
                print!("{}", forgotten_lines(&daemon.forget_tenant_keys(tenant, &why)?));
            }
            Self::ShredTenant { tenant } => {
                let (now, forgotten) = daemon.shred_tenant(tenant)?;
                print!("shredded_now {}\n{}", u8::from(now), forgotten_lines(&forgotten));
            }
            Self::Mount(_)
            | Self::FormatBucket { .. }
            | Self::AddBucket { .. }
            | Self::Buckets { .. } => unreachable!("served above"),
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn through(self, _socket: &Path) -> anyhow::Result<()> {
        if self.is_on_buckets() {
            return self.on_buckets();
        }
        bail!("the extent daemon runs on Linux only")
    }

    /// cladefs: whether it acts on buckets themselves, with or without the daemon (TASK-633.9).
    fn is_on_buckets(&self) -> bool {
        matches!(self, Self::FormatBucket { .. } | Self::AddBucket { .. } | Self::Buckets { .. })
    }

    /// cladefs: does what it does to buckets, and prints what it says (TASK-633.9).
    fn on_buckets(self) -> anyhow::Result<()> {
        let printable = |err: cladefs::Error| anyhow::anyhow!("{err:#}");
        match self {
            Self::FormatBucket { url, details } => {
                let identity = cladefs::bucket::format_bucket(&url, details).map_err(printable);
                println!("{}", identity.with_context(|| format!("formatting {url}"))?.number);
            }
            Self::AddBucket { root, url, details } => {
                let identity = cladefs::bucket::add_bucket(&root, &url, details).map_err(printable);
                let added =
                    identity.with_context(|| format!("adding {url} to {root}'s buckets"))?;
                println!("{}", added.number);
            }
            Self::Buckets { url } => {
                let registry = cladefs::bucket::bucket_registry(&url).map_err(printable);
                let registry = registry.with_context(|| format!("reading {url}'s buckets"))?;
                let registry = registry.with_context(|| format!("{url} records no buckets"))?;
                print!("{}", buckets_lines(&registry));
            }
            _ => unreachable!("acts on no bucket"),
        }
        Ok(())
    }

    /// Does it here, on stores nothing has open: new ones anywhere; existing ones with the bucket
    /// and a placement token.
    fn locally(self) -> anyhow::Result<()> {
        if self.is_on_buckets() {
            return self.on_buckets();
        }
        let daemons = match self {
            Self::Stats { .. } => Some("stats are"),
            Self::Grant { .. } | Self::Revoke { .. } => Some("tenants' grants are"),
            Self::ShredCell { .. } | Self::ForgetTenantKeys { .. } | Self::ShredTenant { .. } => {
                Some("shredding and forgetting keys are")
            }
            Self::Mount(_) => Some("its FUSE mount of cells is"),
            _ => None,
        };
        if let Some(what) = daemons {
            bail!("{what} the extent daemon's: name its control socket (CLADEFSD_GRPC)");
        }
        // cladefs: every store it opens here, or deletes the cell of, is encrypted under the host's
        // KMS: none chosen, it refuses before it touches any (TASK-411.28).
        cladefs::bucket::require_kms().map_err(anyhow::Error::msg)?;
        // What stat reads needs no cell changed, nor does a copy dropped, its cell left be.
        let existing = !matches!(
            self,
            Self::Create { .. }
                | Self::Take { .. }
                | Self::Import { .. }
                | Self::Stat { .. }
                | Self::Drop { .. }
        );
        if let Some(Err(why)) = existing.then(cladefs::bucket::Host::from_env) {
            bail!(
                "without the extent daemon (CLADEFSD_GRPC), cladefsctl changes no existing \
                 store's cell here: {why}"
            );
        }
        // cladefs: outside the daemon, cladefsctl blocks on what the stores do, at its one entry
        // here; each store it opens it closes, its uploads done (TASK-685.3).
        futures::executor::block_on(self.locally_async())
    }

    /// cladefs: what [`Command::locally`] does, once its checks pass.
    async fn locally_async(self) -> anyhow::Result<()> {
        match self {
            Self::Create { dir, capacity } => {
                let store = OpenCell::create(&dir, capacity).await?;
                println!("{}", store.cell());
                store.close().await;
            }
            Self::Take { dir, capacity, cell, token, force } => {
                let mut opening = cladefs::Opening::here()?;
                opening.keeping.force = force;
                OpenCell::take_with(&dir, (capacity, cell, token), opening).await?.close().await;
            }
            Self::Place { dir, token, force: false } => OpenCell::place(&dir, token)?,
            Self::Place { dir, token, force: true } => OpenCell::place_forced(&dir, token)?,
            Self::Sync { dir } => {
                let store = OpenCell::open(&dir).await?;
                let synced = store.wait_uploaded().await;
                store.close().await;
                synced?;
            }
            Self::Import { dir, capacity, image, reference } => {
                let made = cladefs::import::store(&dir, capacity, &image, reference.as_deref());
                made.await.with_context(|| format!("importing {}", image.display()))?;
            }
            Self::Fork { src, dst } => {
                let store = OpenCell::open(&src).await?;
                // A new cell of the store's tenant's (TASK-364).
                let tenant = store.cell().tenant;
                let forked = store.fork(&dst, tenant).await;
                store.close().await;
                println!("{}", forked?);
            }
            Self::Version { dir } => {
                let store = OpenCell::open(&dir).await?;
                let made = store.make_version(None).await;
                store.close().await;
                println!("{:x}", made?);
            }
            Self::Versions { dir } => {
                let store = OpenCell::open(&dir).await?;
                print_versions(&store.versions().await);
                store.close().await;
            }
            Self::DropVersions { dir, versions } => {
                let store = OpenCell::open(&dir).await?;
                let dropped = store.drop_versions(&versions.into_iter().collect()).await;
                store.close().await;
                dropped?;
            }
            Self::Share { dir, version, shared } => {
                let store = OpenCell::open(&dir).await?;
                let shared = store.share_version(version, shared).await;
                store.close().await;
                shared?;
            }
            Self::ForkVersion { from, dst } => bail!(
                "forking version {:x} of cell {:x} to {} needs the extent daemon (CLADEFSD_GRPC)",
                from.version,
                from.cell,
                dst.display()
            ),
            Self::Delete { dir } => {
                let bucket = OpenCell::bucket_at(&dir)?;
                OpenCell::delete(&dir, bucket.as_deref()).await?
            }
            Self::Drop { dir } => OpenCell::delete(&dir, None).await?,
            Self::Resize { dir, capacity } => {
                let store = OpenCell::open(&dir).await?;
                // Opened, it says whether it is a version, whose capacity changes nowhere
                // (TASK-160.10).
                let resized = match store.info() {
                    Ok(info) if capacity < info.capacity => Err(anyhow::anyhow!(
                        "shrinking a store needs its guest to agree: resize it through the \
                         extent daemon serving its VM (CLADEFSD_GRPC)"
                    )),
                    Ok(_) => store.set_capacity(capacity).map_err(anyhow::Error::from),
                    Err(err) => Err(err.into()),
                };
                store.close().await;
                resized?;
            }
            Self::Public { dir } => {
                let store = OpenCell::open(&dir).await?;
                let made = store.make_public();
                store.close().await;
                made?;
            }
            Self::PublicAttachOnly { dir } => {
                let store = OpenCell::open(&dir).await?;
                let made = store.make_public_attach_only();
                store.close().await;
                made?;
            }
            Self::Attach { dir, base } => {
                let based = OpenCell::open(&base).await?;
                let ancestry = based.ancestry();
                based.close().await;
                let store = OpenCell::open(&dir).await?;
                let attached = match base.canonicalize() {
                    Ok(base) => store.attach_base(&base, &ancestry).map_err(anyhow::Error::from),
                    Err(err) => Err(err.into()),
                };
                store.close().await;
                attached?;
            }
            Self::Stats { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::ShredCell { .. }
            | Self::ForgetTenantKeys { .. }
            | Self::ShredTenant { .. }
            | Self::Mount(_)
            | Self::FormatBucket { .. }
            | Self::AddBucket { .. }
            | Self::Buckets { .. } => unreachable!("refused above"),
            Self::Stat { dir } => {
                let store = OpenCell::open(&dir).await?;
                let stat = || -> anyhow::Result<Stat> {
                    let info = store.info()?;
                    Ok(Stat {
                        cell: store.cell(),
                        capacity: info.capacity,
                        used: info.used,
                        available: info.available,
                        holding: store.holding()?,
                        public: store.is_public(),
                        open: None,
                    })
                };
                let stat = stat();
                store.close().await;
                print!("{}", stat?);
            }
        }
        Ok(())
    }
}

/// cladefs: the details `args` say of a bucket, `--region`, `--endpoint`, `--kms` and
/// `--master-key-id`, each with its value (TASK-633.9).
fn details(args: &[&str]) -> anyhow::Result<bucket_cell::identity::Details> {
    let mut details = bucket_cell::identity::Details::default();
    let mut args = args.iter();
    while let Some(&name) = args.next() {
        let value = Some(args.next().with_context(|| format!("{name} takes a value"))?.to_string());
        match name {
            "--region" => details.region = value,
            "--endpoint" => details.endpoint = value,
            "--kms" => details.kms = value,
            "--master-key-id" => details.master_key_id = value,
            _ => bail!(usage()),
        }
    }
    Ok(details)
}

/// cladefs: what `buckets` prints of `registry`: `version VERSION`, then each bucket, a line each,
/// its number and the details recorded, `name=value` (TASK-633.9).
fn buckets_lines(registry: &bucket_cell::identity::Registry) -> String {
    let mut lines = format!("version {}\n", registry.version);
    for entry in &registry.buckets {
        let bucket_cell::identity::Details { provider, region, url, endpoint, kms, master_key_id } =
            &entry.details;
        let named = [
            ("provider", provider),
            ("region", region),
            ("url", url),
            ("endpoint", endpoint),
            ("kms", kms),
            ("master_key_id", master_key_id),
        ];
        lines += &entry.number.to_string();
        for (name, value) in named {
            if let Some(value) = value {
                lines += &format!(" {name}={value}");
            }
        }
        lines += "\n";
    }
    lines
}

/// What `stat` says of a store, which it prints one `name value` a line.
struct Stat {
    cell: cladefs::CellId,
    capacity: u64,
    used: u64,
    available: u64,
    holding: cladefs::CellHolding,
    public: bool,
    /// Whether the store is open, if known.
    open: Option<bool>,
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { cell, capacity, used, available, holding, public, open } = self;
        let cladefs::CellHolding { bytes, extents } = holding;
        writeln!(f, "cell {cell}\ncapacity {capacity}\nused {used}\navailable {available}")?;
        writeln!(f, "own {}\ninherited {}", bytes.own, bytes.inherited)?;
        writeln!(f, "own_extents {}\ninherited_extents {}", extents.own, extents.inherited)?;
        writeln!(f, "public {public}")?;
        if let Some(open) = open {
            writeln!(f, "open {open}")?;
        }
        Ok(())
    }
}

/// cladefs: `stats` as `stats --json` prints it (TASK-111.4): see the module docs. Maps' keys in
/// order.
#[cfg(target_os = "linux")]
fn stats_json(stats: &cladefs::cladefsd::control::proto::GetStatsResponse) -> serde_json::Value {
    use serde_json::{Value, json};
    let named = |map: &std::collections::HashMap<String, u64>| -> Value {
        map.iter().map(|(name, value)| (name.clone(), Value::from(*value))).collect()
    };
    let nonempty = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    let vms = stats.vms.iter().map(|vm| {
        let guest = vm.guest.as_ref().map(|guest| {
            let histograms: serde_json::Map<String, Value> = (guest.histograms.iter())
                .map(|(name, histogram)| (name.clone(), json!(histogram.buckets)))
                .collect();
            json!({
                "build": nonempty(&guest.build),
                "counters": named(&guest.counters),
                "histograms": histograms,
            })
        });
        json!({
            "connection": vm.connection,
            "cell": base32_id::Short(vm.cell).to_string(),
            "path": vm.path,
            "follower": vm.follower,
            "vmm": {
                "build": nonempty(&vm.build),
                "counters": named(&vm.counters),
                "gauges": named(&vm.gauges),
            },
            "window": (!vm.window.is_empty()).then(|| named(&vm.window)),
            "answered": vm.answered,
            "answer_ns": vm.answered.then_some(vm.answer_ns),
            "guest": guest,
        })
    });
    json!({
        "taken_unix_ns": stats.taken_unix_ns,
        "cladefsd": {
            "build": stats.build,
            "counters": named(&stats.counters),
            "gauges": named(&stats.gauges),
        },
        "vms": vms.collect::<Vec<_>>(),
    })
}

/// cladefs: `value`, as lines of a path through it, its keys and indices joined by dots, and a value
/// there that holds no other: what `stats` prints without `--json`.
#[cfg(target_os = "linux")]
fn flattened(value: &serde_json::Value) -> String {
    use serde_json::Value;
    fn lines(at: &str, value: &Value, out: &mut String) {
        let within =
            |name: &str| if at.is_empty() { name.to_owned() } else { format!("{at}.{name}") };
        match value {
            Value::Object(map) => {
                map.iter().for_each(|(name, value)| lines(&within(name), value, out))
            }
            Value::Array(values) => (values.iter().enumerate())
                .for_each(|(index, value)| lines(&within(&index.to_string()), value, out)),
            Value::String(text) => *out += &format!("{at} {text}\n"),
            value => *out += &format!("{at} {value}\n"),
        }
    }
    let mut out = String::new();
    lines("", value, &mut out);
    out
}

fn main() -> anyhow::Result<()> {
    cladefs::renamed::refuse_old_variables().map_err(anyhow::Error::msg)?;
    // cladefs: its threads' stacks on SIGQUIT, as an import that hung needed (TASK-98, TASK-44.13).
    #[cfg(target_os = "linux")]
    cladefs::stacks::on_sigquit()?;
    // cladefs: warnings, as of reading what the collector marked (TASK-264).
    #[cfg(target_os = "linux")]
    {
        use cladefs::cladefsd::logging;
        let warnings = logging::Settings { level: log::LevelFilter::Warn, ..Default::default() };
        logging::install(warnings)?;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    // cladefs: the commit it was built of (TASK-111.7).
    if args == ["--version"] {
        println!("cladefsctl {}", cladefs::BUILD_COMMIT);
        return Ok(());
    }
    let command = Command::parse(&args)?;
    match std::env::var_os("CLADEFSD_GRPC").filter(|socket| !socket.is_empty()) {
        Some(socket) => command.through(socket.as_ref()),
        None => command.locally(),
    }
}
