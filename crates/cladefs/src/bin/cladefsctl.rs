// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's host tool for VMs' disks, the extent stores extentd serves: makes them, of branches kept
//! in object storage (backlog doc-7) or of container images, places, forks, deletes and resizes
//! them, makes them public bases, and waits for their uploads:
//!
//! ```sh
//! cladefsctl take DIR CAPACITY BRANCH [TOKEN]
//! cladefsctl create DIR CAPACITY
//! cladefsctl place DIR TOKEN
//! cladefsctl sync DIR
//! cladefsctl import DIR CAPACITY IMAGE [REF]
//! cladefsctl fork SRC DST
//! cladefsctl point SRC DST [--shared]
//! cladefsctl share DIR
//! cladefsctl unshare DIR
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
//! cladefsctl --version
//! ```
//!
//! With `CLADEFS_EXTENTD_GRPC` naming the control socket of the extent daemon (TASK-43.5), every
//! command goes through its control API (`cladefs::extentd::control`), and acts on stores as the
//! daemon serves them: on a store a VM has open, live, or not at all (`EBUSY`). Without, cladefsctl
//! acts on stores itself, on stores nothing has open, as their lock says, which an extent daemon
//! serving them holds (it waits a while for it, then refuses). `create`, `take` and `import`, which
//! make new stores, work so anywhere, as when building base images where no daemon runs. The others change
//! existing stores' branches, which are in the bucket each store's description names: they need
//! this host's (`CLADEFS_EXTENTD_REMOTE`) and its placement's fencing token (`CLADEFS_EXTENTD_TOKEN`),
//! and refuse otherwise (`cladefs::remote::Host`, TASK-45).
//!
//! `create` makes an empty store at DIR, of CAPACITY bytes, of a new branch, the one volume of a
//! new cell of the default tenant's, its ID its bucket's to give (TASK-364), and prints the branch.
//! `take` makes an empty store at DIR, of CAPACITY bytes, of BRANCH (`TENANT/CELL/ID`, each in hex,
//! a volume of its cell named as DIR is, TASK-364), which its bucket has (`ENOENT` otherwise): once
//! extentd opens it,
//! with `CLADEFS_EXTENTD_REMOTE` naming the branch's bucket, it takes what the branch holds there,
//! whose data it fetches as it is read; placed at TOKEN, if given, as `place` places it. A
//! point, a VM snapshot's or another, it reads, taking no head, as any number of hosts do at once.
//! `place` places the store at DIR at the placement whose fencing token,
//! its boot ID, is TOKEN (TASK-39.7): its writer takes its branch in object storage with that token
//! from then on, which a later placement's exceeds, so that a placement cut off takes no branch
//! back from the one after it (`ESTALE`), and its commits fail once a later one takes it. `sync`
//! returns once every commit of the store at DIR is durable and in object storage, as before a
//! clean stop: through the daemon, a store a VM has open as of then. `import` makes a store at DIR, of CAPACITY bytes,
//! holding an Fxfs filesystem of the container image IMAGE (TASK-39.5), a base for VMs to boot
//! forks of: IMAGE is an OCI image layout, as `skopeo copy` makes, or `docker save` (whose tarball
//! is one, once extracted), holding the image REF names, or its only image; or a tar archive of one
//! layer, gzipped or not, as `docker export` makes. Files keep their owners, permissions, times
//! and extended attributes; see `cladefs::import`. `fork` copies the store at SRC to a new store at
//! DST, of a new branch, which it prints: through the daemon, live, if a VM has SRC open, once
//! nothing is open on it, as with the guest's filesystem frozen (fsfreeze), and refused otherwise
//! (TASK-39.3). The new branch is a fork of SRC's in its bucket, made once SRC's commits are
//! uploaded, holding its extents without uploading them again; DST is a store of its own, which
//! reads them there (TASK-165.7). `point` forks so, once nothing is open, to a point (TASK-158): a name of SRC's
//! state as it is, written by none, which any number of hosts fork at once, each from a copy `take`
//! makes of its branch, as a VM snapshot's restores do; for a base forked on several hosts; with
//! `--shared`, shared once made, as `share` shares it. `share` shares the point the store at DIR
//! is of (TASK-395.7): it forks into cells of other tenants' than its own, as a base image or a
//! VM snapshot's point many tenants fork, which a point not shared refuses (`EACCES`); `unshare`
//! stops such forks, those made living on. The point's record in its bucket says it, for every
//! host. `delete` deletes a store, a fork or not: its branch in its bucket, its forks' living on,
//! and its directory (TASK-39.4). `drop` deletes the store at DIR as `delete` does, this
//! host's copy of it, its branch staying in the bucket (TASK-154): what `take` made, once this host
//! is done with it. `resize` sets the store's capacity, the size of the disk
//! its guest sees, to CAPACITY bytes: through the daemon, a store its VM has open grows at once,
//! the guest seeing the new size while mounted, and shrinks if the guest's filesystem agrees, no
//! lower than what it holds; otherwise, it grows, and does not shrink. `public` makes the store at
//! DIR a public base, whose pages every tenant's forks of it share (TASK-43). `stat` prints what the
//! store at DIR is and holds, one `name value` a line: its branch (hex), capacity, used and
//! available bytes, and of those used, its own (its branch wrote them) and inherited (from the
//! branches it was forked from), what billing it alone charges it and what it shares (TASK-49);
//! how many extents it holds, own and inherited (`own_extents`, `inherited_extents`), which what an
//! open store takes of the daemon's memory grows with (TASK-443.2, TASK-454); whether it is public,
//! and, through the daemon, open. A store no VM has open, it opens, locally or through the daemon,
//! as a VM's would, to say: time, and memory till it is done, as its extents. `--version` prints the commit it was built
//! of (`cladefs::BUILD_COMMIT`, TASK-111.7).
//!
//! cladefs: on Linux, it logs on standard error what is as grave as a warning: as a read of an object
//! the collector's newest mark-only pass marked, a bug in the collector (`extent_remote::marks`,
//! TASK-264).
//!
//! `stats`, through the daemon alone, prints what the daemon, its VM processes and their guests
//! counted, as of one moment, each with the commit it was built of (the control API's
//! `GetStats`, TASK-111.4): with `--json`, as one JSON object, of `taken_unix_ns`, `extentd`
//! (`build`, `counters`, `gauges`) and `vms`, each VM process's `connection`, `branch` (hex),
//! `path`, `follower`, `vmm` (`build`, `counters`, `gauges`), `window`, `answered`, `answer_ns` and
//! `guest` (`build`, `counters`, `histograms`), null where it has none; otherwise as lines of a
//! path through that object and a value, `vms.0.guest.counters.seals 12`. Counters only grow: a
//! measurement is the difference of two.
//!
//! `grant` and `revoke`, through the daemon alone, are the control API's `Grant` and `Revoke`
//! (TASK-43.4), for tests and operators (TASK-71): `grant` gives the tenant TENANT, its ID in the
//! bucket, in hexadecimal and not 0, the default tenant's (TASK-364), whose stores are in the
//! directory DIR, a VM socket of its own, `TENANT.sock`, its VM processes naming stores in DIR
//! alone, of its own cells, the socket given to the user UID and the group GID if named, the
//! daemon's own otherwise, and prints its path; granting a tenant again, of the same directory,
//! prints it again. `revoke` takes the tenant TENANT's socket away (hexadecimal): its VM processes
//! connected keep their stores, and no more connect.
//!
//! `shred-cell` and `forget-tenant-keys`, through the daemon alone, are the control API's
//! `ShredCell` and `ForgetTenantKeys` (TASK-411.7, backlog doc-16, Shredding). `shred-cell` shreds
//! the cell TENANT/CELL (each in hex): takes its wrapped KEKs out of its object in the daemon's
//! bucket, so that its volumes' keys unwrap no more, and has the daemon forget what it holds of
//! them, fencing its stores open there (`EACCES`); run it against every host's daemon, the first
//! shredding, each forgetting. A bucket that keeps object versions keeps the KEKs in the cell
//! object's older versions until a lifecycle rule expires them. `forget-tenant-keys` has the daemon
//! forget the keys of the tenant TENANT (hex), once its KEK is destroyed, or revoked, in the KMS:
//! its stores there are fenced, their pages leave the cache, its fetchers' and KMS's keys of it
//! go. Each prints what the daemon forgot, one `name value` a line.

use anyhow::{Context as _, bail};
use cladefs::ExtentStore;
use std::path::{Path, PathBuf};

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "usage: cladefsctl take DIR CAPACITY BRANCH [TOKEN] | cladefsctl create DIR CAPACITY | \
         cladefsctl place DIR TOKEN | \
         cladefsctl sync DIR | cladefsctl import DIR CAPACITY IMAGE [REF] | cladefsctl fork SRC DST | \
         cladefsctl point SRC DST [--shared] | cladefsctl share DIR | cladefsctl unshare DIR | \
         cladefsctl delete DIR | cladefsctl drop DIR | \
         cladefsctl resize DIR CAPACITY | cladefsctl public DIR | \
         cladefsctl public-attach-only DIR | cladefsctl attach DIR BASE | cladefsctl stat DIR | \
         cladefsctl stats [--json] | cladefsctl grant TENANT DIR [UID [GID]] | \
         cladefsctl revoke TENANT | cladefsctl shred-cell TENANT/CELL | \
         cladefsctl forget-tenant-keys TENANT | cladefsctl --version"
    )
}

/// cladefs: what a daemon forgot of a tenant's or a cell's keys, one `name value` a line (TASK-411.7).
#[cfg(target_os = "linux")]
fn forgotten_lines(forgotten: &cladefs::extentd::control::proto::Forgotten) -> String {
    let cladefs::extentd::control::proto::Forgotten { stores, rings, kept, extents, bytes } =
        forgotten;
    format!(
        "stores_fenced {stores}\nrings_forgotten {rings}\nkept_dropped {kept}\n\
         extents_evicted {extents}\nbytes_evicted {bytes}\n"
    )
}

/// What cladefsctl was asked to do.
enum Command {
    /// cladefs: a new store of a new branch, its ID its bucket's to give (TASK-364).
    Create {
        dir: PathBuf,
        capacity: u64,
    },
    Take {
        dir: PathBuf,
        capacity: u64,
        branch: cladefs::Branch,
        token: Option<u64>,
    },
    Place {
        dir: PathBuf,
        token: u64,
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
    /// A fork, a point if `point`, shared if `shared` (TASK-395.7).
    Fork {
        src: PathBuf,
        dst: PathBuf,
        point: bool,
        shared: bool,
    },
    /// cladefs: a point shared, or no longer (TASK-395.7).
    Share {
        dir: PathBuf,
        shared: bool,
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
        cell: cladefs::Cell,
    },
    /// cladefs: a tenant's keys forgotten, its KEK revoked or destroyed (TASK-411.7).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ForgetTenantKeys {
        tenant: u64,
    },
}

impl Command {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let capacity = |capacity: &str| capacity.parse().context("CAPACITY must be a number");
        let token = |token: &str| token.parse().context("TOKEN must be a number");
        let id = |id: &str, what| id.parse().with_context(|| format!("{what} must be a number"));
        let tenant =
            |tenant: &str| u64::from_str_radix(tenant, 16).context("TENANT must be hexadecimal");
        Ok(match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["take", dir, size, branch, ref rest @ ..] if rest.len() <= 1 => Self::Take {
                dir: dir.into(),
                capacity: capacity(size)?,
                branch: branch.parse().map_err(anyhow::Error::msg).context("BRANCH")?,
                token: rest.first().map(|rest| token(rest)).transpose()?,
            },
            ["create", dir, size] => Self::Create { dir: dir.into(), capacity: capacity(size)? },
            ["place", dir, given] => Self::Place { dir: dir.into(), token: token(given)? },
            ["sync", dir] => Self::Sync { dir: dir.into() },
            ["import", dir, size, image, ref rest @ ..] if rest.len() <= 1 => Self::Import {
                dir: dir.into(),
                capacity: capacity(size)?,
                image: image.into(),
                reference: rest.first().map(|reference| reference.to_string()),
            },
            ["fork", src, dst] => {
                Self::Fork { src: src.into(), dst: dst.into(), point: false, shared: false }
            }
            ["point", src, dst] => {
                Self::Fork { src: src.into(), dst: dst.into(), point: true, shared: false }
            }
            ["point", src, dst, "--shared"] => {
                Self::Fork { src: src.into(), dst: dst.into(), point: true, shared: true }
            }
            ["share", dir] => Self::Share { dir: dir.into(), shared: true },
            ["unshare", dir] => Self::Share { dir: dir.into(), shared: false },
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
                let hex = |hex| u64::from_str_radix(hex, 16).context("TENANT/CELL must be hex");
                Self::ShredCell { cell: cladefs::Cell { tenant: hex(tenant)?, id: hex(id)? } }
            }
            ["forget-tenant-keys", given] => Self::ForgetTenantKeys { tenant: tenant(given)? },
            _ => bail!(usage()),
        })
    }

    /// Does it through the control API of the daemon whose control socket is at `socket`.
    #[cfg(target_os = "linux")]
    fn through(self, socket: &Path) -> anyhow::Result<()> {
        let daemon = cladefs::extentd::control::Client::connect(socket)?;
        match self {
            Self::Create { dir, capacity } => {
                let new = cladefs::Branch { cell: cladefs::Cell { tenant: 0, id: 0 }, id: 0 };
                println!("{}", daemon.create_store(&dir, capacity, new, None)?);
            }
            Self::Take { dir, capacity, branch, token } => {
                daemon.create_store(&dir, capacity, branch, token)?;
            }
            Self::Place { dir, token } => daemon.place_store(&dir, token)?,
            Self::Sync { dir } => daemon.sync_store(&dir)?,
            Self::Import { dir, capacity, image, reference } => {
                daemon.import_image(&dir, capacity, &image, reference.as_deref())?;
            }
            Self::Fork { src, dst, point: false, .. } => {
                println!("{}", daemon.fork_store(&src, &dst)?);
            }
            Self::Fork { src, dst, point: true, shared } => {
                println!("{}", daemon.point_store(&src, &dst, shared)?);
            }
            Self::Share { dir, shared } => daemon.share_point(&dir, shared)?,
            Self::Delete { dir } => daemon.delete_store(&dir)?,
            Self::Drop { dir } => daemon.drop_store(&dir)?,
            Self::Resize { dir, capacity } => daemon.resize_store(&dir, capacity)?,
            Self::Public { dir } => daemon.make_public(&dir)?,
            Self::PublicAttachOnly { dir } => daemon.make_public_attach_only(&dir)?,
            Self::Attach { dir, base } => daemon.attach_base(&dir, &base)?,
            Self::Stat { dir } => {
                let stat = daemon.stat_store(&dir)?;
                let cell = cladefs::Cell { tenant: stat.tenant, id: stat.cell };
                let whose = |own, inherited| cladefs::Whose { own, inherited };
                print!(
                    "{}",
                    Stat {
                        branch: cladefs::Branch { cell, id: stat.branch },
                        capacity: stat.capacity,
                        used: stat.used,
                        available: stat.available,
                        holding: cladefs::StoreHolding {
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
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn through(self, _socket: &Path) -> anyhow::Result<()> {
        bail!("the extent daemon runs on Linux only")
    }

    /// Does it here, on stores nothing has open: new ones anywhere; existing ones with the bucket
    /// and a placement token.
    fn locally(self) -> anyhow::Result<()> {
        let daemons = match self {
            Self::Stats { .. } => Some("stats are"),
            Self::Grant { .. } | Self::Revoke { .. } => Some("tenants' grants are"),
            Self::ShredCell { .. } | Self::ForgetTenantKeys { .. } => {
                Some("shredding and forgetting keys are")
            }
            _ => None,
        };
        if let Some(what) = daemons {
            bail!("{what} the extent daemon's: name its control socket (CLADEFS_EXTENTD_GRPC)");
        }
        // What stat reads needs no branch changed, nor does a copy dropped, its branch left be.
        let existing = !matches!(
            self,
            Self::Create { .. }
                | Self::Take { .. }
                | Self::Import { .. }
                | Self::Stat { .. }
                | Self::Drop { .. }
        );
        if let Some(Err(why)) = existing.then(cladefs::remote::Host::from_env) {
            bail!(
                "without the extent daemon (CLADEFS_EXTENTD_GRPC), cladefsctl changes no existing \
                 store's branch here: {why}"
            );
        }
        match self {
            Self::Create { dir, capacity } => {
                println!("{}", ExtentStore::create(&dir, capacity)?.branch());
            }
            Self::Take { dir, capacity, branch, token } => {
                ExtentStore::take(&dir, capacity, branch, token)?;
            }
            Self::Place { dir, token } => ExtentStore::place(&dir, token)?,
            Self::Sync { dir } => {
                let store = ExtentStore::open(&dir)?;
                store.wait_uploaded()?;
            }
            Self::Import { dir, capacity, image, reference } => {
                let made = cladefs::import::store(&dir, capacity, &image, reference.as_deref());
                futures::executor::block_on(made)
                    .with_context(|| format!("importing {}", image.display()))?;
            }
            Self::Fork { src, dst, point, shared } => {
                let store = ExtentStore::open(&src)?;
                // The one volume of a new cell of the store's tenant's (TASK-364).
                let cell = cladefs::new_cell(store.branch().cell.tenant);
                let branch = match point {
                    true => store.point(&dst, cell)?,
                    false => store.fork(&dst, cell)?,
                };
                if shared {
                    ExtentStore::share(&dst, true)?;
                }
                println!("{branch}");
            }
            Self::Share { dir, shared } => ExtentStore::share(&dir, shared)?,
            Self::Delete { dir } => {
                let bucket = ExtentStore::bucket_at(&dir)?;
                ExtentStore::delete(&dir, bucket.as_deref())?
            }
            Self::Drop { dir } => ExtentStore::delete(&dir, None)?,
            Self::Resize { dir, capacity } => {
                let store = ExtentStore::open(&dir)?;
                // Opened, it says whether it is a point, whose capacity changes nowhere
                // (TASK-160.10).
                if capacity < store.info()?.capacity {
                    bail!(
                        "shrinking a store needs its guest to agree: resize it through the \
                         extent daemon serving its VM (CLADEFS_EXTENTD_GRPC)"
                    );
                }
                store.set_capacity(capacity)?;
            }
            Self::Public { dir } => ExtentStore::open(&dir)?.make_public()?,
            Self::PublicAttachOnly { dir } => ExtentStore::open(&dir)?.make_public_attach_only()?,
            Self::Attach { dir, base } => {
                let ancestry = ExtentStore::open(&base)?.ancestry();
                ExtentStore::open(&dir)?.attach_base(&base.canonicalize()?, &ancestry)?;
            }
            Self::Stats { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::ShredCell { .. }
            | Self::ForgetTenantKeys { .. } => unreachable!("refused above"),
            Self::Stat { dir } => {
                let store = ExtentStore::open(&dir)?;
                let info = store.info()?;
                print!(
                    "{}",
                    Stat {
                        branch: store.branch(),
                        capacity: info.capacity,
                        used: info.used,
                        available: info.available,
                        holding: store.holding()?,
                        public: store.is_public(),
                        open: None,
                    }
                );
            }
        }
        Ok(())
    }
}

/// What `stat` says of a store, which it prints one `name value` a line.
struct Stat {
    branch: cladefs::Branch,
    capacity: u64,
    used: u64,
    available: u64,
    holding: cladefs::StoreHolding,
    public: bool,
    /// Whether the store is open, if known.
    open: Option<bool>,
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { branch, capacity, used, available, holding, public, open } = self;
        let cladefs::StoreHolding { bytes, extents } = holding;
        writeln!(f, "branch {branch}\ncapacity {capacity}\nused {used}\navailable {available}")?;
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
fn stats_json(stats: &cladefs::extentd::control::proto::GetStatsResponse) -> serde_json::Value {
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
            "branch": format!("{:x}", vm.branch),
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
        "extentd": {
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
    // cladefs: its threads' stacks on SIGQUIT, as an import that hung needed (TASK-98, TASK-44.13).
    #[cfg(target_os = "linux")]
    cladefs::stacks::on_sigquit()?;
    // cladefs: warnings, as of reading what the collector marked (TASK-264).
    #[cfg(target_os = "linux")]
    {
        use cladefs::extentd::logging;
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
    match std::env::var_os("CLADEFS_EXTENTD_GRPC").filter(|socket| !socket.is_empty()) {
        Some(socket) => command.through(socket.as_ref()),
        None => command.locally(),
    }
}
