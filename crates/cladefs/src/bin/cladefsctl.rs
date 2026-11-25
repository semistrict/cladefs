// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's host tool for VMs' disks, the cells cladefsd serves: makes them, of cells kept
//! in object storage (backlog doc-7) or of container images, places, forks, deletes and resizes
//! them, makes them public bases, and waits for their uploads:
//!
//! ```sh
//! cladefsctl take [--force] DIR CAPACITY CELL [TOKEN]
//! cladefsctl create DIR CAPACITY
//! cladefsctl place [--force] DIR TOKEN
//! cladefsctl sync DIR
//! cladefsctl import DIR CAPACITY IMAGE [REF]
//! cladefsctl import-memory [--version] DIR FILE
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
//! cladefsctl stats [--json] [--wait SECONDS] [--leave-out histograms|guests|vms]...
//! cladefsctl cells [--json]
//! cladefsctl counters [--json]
//! cladefsctl set-peers [ADDRESS...]
//! cladefsctl migrate DIR PEER TO
//! cladefsctl hand-over [--token TOKEN] DIR
//! cladefsctl abort-migration DIR
//! cladefsctl memory-limit [--tenant TENANT] BYTES|none
//! cladefsctl memory-weight TENANT WEIGHT
//! cladefsctl in-flight [--tenant TENANT] BYTES|default
//! cladefsctl shrink-memory [--tenant TENANT] TARGET
//! cladefsctl disk-limit [--tenant TENANT] BYTES|none
//! cladefsctl disk-weight TENANT WEIGHT
//! cladefsctl tenant-disk [--tenant TENANT] [--json]
//! cladefsctl grant TENANT DIR [UID [GID]]
//! cladefsctl revoke TENANT
//! cladefsctl shred-cell TENANT/CELL
//! cladefsctl forget-tenant-keys TENANT
//! cladefsctl shred-tenant TENANT
//! cladefsctl kms reload
//! cladefsctl decommission-host [--none-ok] INCARNATION
//! cladefsctl ephemeral-hosts
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
//! Any command may be preceded by `--timeout SECONDS` (or `CLADEFSCTL_TIMEOUT` set to SECONDS),
//! which bounds connecting to the daemon and its call as a deadline (board 8585): one that takes
//! longer fails with `ETIMEDOUT`, the daemon going on with it, which may still complete it, as it
//! completes every call whether or not its caller waits. No command has one unless given, `sync`,
//! `import` and `fork` taking minutes, others as long as their cells or VMs take. It bounds the
//! control API's calls alone: not `mount`'s, nor what cladefsctl does without the daemon.
//!
//! With `CLADEFSD_GRPC` naming the control socket of the extent daemon (TASK-43.5), every
//! command goes through its control API (`cladefs::cladefsd::control`), and acts on cells as the
//! daemon serves them: on a cell a VM has open, live, or not at all (`EBUSY`). Without, cladefsctl
//! acts on cells itself, on cells nothing has open, as their lock says, which an extent daemon
//! serving them holds (it waits a while for it, then refuses). `create`, `take` and `import`, which
//! make new cells, work so anywhere, as when building base images where no daemon runs. The others
//! change existing cells, which are in the bucket each cell's description names: they need
//! this host's (`CLADEFSD_BUCKET`) and its placement's fencing token (`CLADEFSD_TOKEN`),
//! and refuse otherwise (`cladefs::bucket::Host`, TASK-45).
//!
//! `create` makes an empty cell at DIR, of CAPACITY bytes, of a new cell, a new cell's of the
//! default tenant's (doc-20), its ID its bucket's to give (TASK-364), and prints the cell. `take`
//! makes an empty cell at DIR, of CAPACITY bytes, of CELL (`TENANT/CELL`, each in base 32, its
//! cell's, TASK-364), which its bucket has (`ENOENT` otherwise), its taking of a replicated cell
//! forced with `--force`, as `place --force` places a cell (TASK-681): once cladefsd opens it, with
//! `CLADEFSD_BUCKET` naming the cell's bucket, it takes what the cell holds there, whose data it
//! fetches as it is read; placed at TOKEN, if given, as `place` places it. A version, a VM
//! snapshot's or another, it reads, taking no head, as any number of hosts do at once. `place`
//! places the cell at DIR at the placement whose fencing token, its boot ID, is TOKEN (TASK-39.7):
//! its writer takes its cell in object storage with that token from then on, which a later
//! placement's exceeds, so that a placement cut off takes no cell back from the one after it
//! (`ESTALE`), and its commits fail once a later one takes it. `--force`d, that placement's takings
//! of a replicated cell go on with the members of the configuration its head names that answer,
//! fewer than a take needs, as when f+1 of them are gone for good (doc-21, TASK-681): commits told
//! replicated that only the others held are lost, and the cell's status says past which commit
//! (`forced_after`). `sync` returns once every commit of the cell at DIR is durable and in object
//! storage, as before a clean stop: through the daemon, a cell a VM has open as of then. `import`
//! makes a cell at DIR, of CAPACITY bytes, holding an Fxfs filesystem of the container image IMAGE
//! (TASK-39.5), a base for VMs to boot forks of: IMAGE is an OCI image layout, as `skopeo copy`
//! makes, or `docker save` (whose tarball is one, once extracted), holding the image REF names, or
//! its only image; or a tar archive of one layer, gzipped or not, as `docker export` makes. Files
//! keep their owners, permissions, times and extended attributes; see `cladefs::import`. `fork`
//! copies the cell at SRC to a new cell at DST, of a new cell, which it prints: through the daemon,
//! live, if a VM has SRC open, once nothing is open on it, as with the guest's filesystem frozen
//! (fsfreeze), and refused otherwise (TASK-39.3). The new cell is a fork of SRC's in its bucket,
//! made once SRC's commits are uploaded, holding its extents without uploading them again; DST is a
//! cell of its own, which reads them there (TASK-165.7). `version` makes a version of the cell of
//! the cell at DIR, once nothing is open on it: one commit here, uploaded behind, which it prints
//! the ID of, in hexadecimal. `fork-version` forks the version VERSION of CELL (TENANT/CELL) to
//! DST, on any host, as many hosts at once as wanted, the version uploaded first. `versions` lists
//! the cell's versions, `drop-versions` drops some, what forks of them inherited staying until they
//! go. `share` shares a version (TASK-395.7): it forks into cells of other tenants' than its own,
//! as a base image many tenants fork, which a version not shared refuses (`EACCES`); `unshare`
//! stops such forks, those made living on. The cell's head says it, for every host. `delete`
//! deletes a cell, a fork or not: its cell in its bucket, its forks' living on, and its directory
//! (TASK-39.4). `drop` deletes the cell at DIR as `delete` does, this host's copy of it, its cell
//! staying in the bucket (TASK-154): what `take` made, once this host is done with it. `resize`
//! sets the cell's capacity, the size of the disk its guest sees, to CAPACITY bytes: through the
//! daemon, a cell its VM has open grows at once, the guest seeing the new size while mounted, and
//! shrinks if the guest's filesystem agrees, no lower than what it holds; otherwise, it grows, and
//! does not shrink. `public` makes the cell at DIR a public base, whose pages every tenant's forks
//! of it share (TASK-43). `stat` prints what the cell at DIR is and holds, one `name value` a line:
//! its cell (base 32), capacity, used and available bytes, and of those used, its own (its cell
//! wrote them) and inherited (from the cells it was forked from), what billing it alone charges it
//! and what it shares (TASK-49); how many extents it holds, own and inherited (`own_extents`,
//! `inherited_extents`), which what an open cell takes of the daemon's memory grows with
//! (TASK-443.2, TASK-454); whether it is public, and, through the daemon, open. A cell no VM has
//! open, it opens, locally or through the daemon, as a VM's would, to say: time, and memory till it
//! is done, as its extents. `--version` prints the commit it was built of (`cladefs::BUILD_COMMIT`,
//! TASK-111.7).
//!
//! cladefs: on Linux, it logs on standard error what is as grave as a warning.
//!
//! `stats`, through the daemon alone, prints what the daemon, its VM processes and their guests
//! counted, as of one moment, each with the commit it was built of (the control API's
//! `GetStats`, TASK-111.4): with `--json`, as one JSON object, of `taken_unix_ns`, `cladefsd`
//! (`build`, `counters`, `gauges`, and `incarnation`, its ephemeral cells' directory's, in base
//! 32, null if it keeps none) and `vms`, each VM process's `connection`, `cell` (base 32),
//! `path`, `vmm` (`build`, `counters`, `gauges`), `window`, `answered`, `answer_ns` and
//! `guest` (`build`, `counters`, `histograms`), null where it has none; otherwise as lines of a
//! path through that object and a value, `vms.0.guest.counters.seals 12`. Counters only grow: a
//! measurement is the difference of two. `--wait` waits for the VM processes' answers SECONDS at
//! most (2 unless given, 60 at most); `--leave-out` leaves the guests' histograms out, or their
//! stats, or the VM processes, the daemon's own stats alone, as a host of many VMs needs once its
//! stats are past the most the daemon answers (board 8581).
//!
//! The commands below, through the daemon alone too, are the control API's calls of the same
//! names (board 8585), for operators and scripts; each prints what its call answers, as `stats`
//! does, one `name value` a line, or with `--json` where it takes it, one JSON object; those that
//! answer nothing print nothing. TENANT is in base 32; `--tenant` names one, its absence every
//! tenant, or, for `shrink-memory`, the daemon's memory whole. `cells` lists the cells VM processes
//! have open (`ListCells`), each's `path` and what `StatCell` says of it, by path, under `cells`
//! (`cells.0.path`). `counters` prints the daemon's counters since it started (`GetCounters`),
//! with its bucket's under `object_storage`. `set-peers` has the hosts sharing the daemon's bucket
//! be those at the ADDRESSes, each `HOST:PORT`, none without any (`SetPeers`). `migrate` begins the
//! live migration of the ephemeral cell at DIR to the daemon whose peer address is PEER, its cell
//! going at TO there, and prints `migration ID` (`MigrateCell`); `hand-over` hands it over to its
//! target (`HandOverCell`); `abort-migration` ends its migration, the cell then free to migrate
//! again (`AbortMigration`). `memory-limit` limits the memory the daemon holds extents' pages in
//! to BYTES, or lifts the limit with `none` (`SetMemoryLimit`); `memory-weight` weighs a tenant's
//! memory against the others' (`SetMemoryWeight`); `in-flight` sets the bytes in flight a tenant's
//! connections share, or the default with `default` (`SetTenantInFlight`); `shrink-memory` evicts
//! pages down to TARGET bytes, and prints `bytes`, what is held then (`ShrinkMemory`).
//! `disk-limit`, `disk-weight` and `tenant-disk` are the disk's (`SetDiskLimit`, `SetDiskWeight`,
//! `GetTenantDisk`): `tenant-disk` prints `bytes`, `limit_bytes` (null without one) and
//! `reserved_bytes`.
//!
//! `grant` and `revoke`, through the daemon alone, are the control API's `Grant` and `Revoke`
//! (TASK-43.4), for tests and operators (TASK-71): `grant` gives the tenant TENANT, its ID in the
//! bucket, in base 32 and not 0, the default tenant's (TASK-364), whose cells are in the
//! directory DIR, a VM socket of its own, `TENANT.sock`, its VM processes naming cells in DIR
//! alone, of its own cells, the socket given to the user UID and the group GID if named, the
//! daemon's own otherwise, and prints its path; granting a tenant again, of the same directory,
//! prints it again. `revoke` takes the tenant TENANT's socket away (base 32): its VM processes
//! connected keep their cells, and no more connect.
//!
//! `shred-cell` and `forget-tenant-keys`, through the daemon alone, are the control API's
//! `ShredCell` and `ForgetTenantKeys` (TASK-411.7, backlog doc-16, Shredding). `shred-cell` shreds
//! the cell TENANT/CELL (each in base 32): takes its wrapped KEKs out of its object in the daemon's
//! bucket, so that its keys unwrap no more, and has the daemon forget what it holds of them,
//! halting its cells open there (`EACCES`); run it against every host's daemon, the first
//! shredding, each forgetting. A bucket that keeps object versions keeps the KEKs in the cell
//! object's older versions until a lifecycle rule expires them. `forget-tenant-keys` has the daemon
//! forget the keys of the tenant TENANT (base 32), once its KEK is destroyed, or revoked, in the
//! KMS: its cells there are halted, their pages leave the cache, its fetchers' and KMS's keys of it
//! go. `shred-tenant`, through the daemon alone too, is the control API's `ShredTenant`
//! (TASK-411.27, doc-16 The bucket KMS): under the bucket KMS (a master key, no `CLADEFS_KMS`), it
//! replaces the tenant TENANT's (base 32) KEK object in the daemon's bucket by a tombstone, so that
//! no key its KEK wrapped unwraps anywhere and none is made it again, and has the daemon forget its
//! keys; run it against every host's daemon, the first shredding, each forgetting, and printing
//! `shredded_now 1` or `0`. A bucket that keeps object versions keeps the KEK in the object's older
//! versions until a lifecycle rule expires them. A daemon of another KMS refuses it: destroy the
//! tenant's KEK there, then `forget-tenant-keys`. Each prints what the daemon forgot, one `name
//! value` a line.
//!
//! `kms reload`, through the daemon alone, is the control API's `ReloadKms` (board 8487, doc-16
//! What reloads): the daemon reads its KMS's key overrides file (`keys=FILE`) and its master keys
//! (`CLADEFS_MASTER_KEY_FILE`) again, applying them whole or refusing, changing nothing, naming
//! the file and line of what is not one, or the tenant or master key in use a change would lose.
//! Run it against every host's daemon. It prints `renamed TENANT` for each tenant whose KEK's name
//! changed, then `master_key_added BUCKET ID` and `master_key_removed BUCKET ID` for each master key
//! of a bucket KMS, one a line.
//!
//! `decommission-host` and `ephemeral-hosts`, through the daemon alone, are the control API's
//! `DecommissionHost` and `ListEphemeralHosts` (doc-25, boards 8517, 8519, 8521).
//! `decommission-host` decommissions the host of incarnation INCARNATION (base 32, as the daemon's
//! log, `stats` and `ephemeral-hosts` print it), gone for good, its disk lost: no host of it forks
//! an ephemeral cell, or starts, from then on, and the references of its ephemeral cells, in
//! every bucket, are retired, so that the collector reclaims what they kept. It calls again while
//! the daemon says it left some, and prints `retired N`; having retired none, it says
//! `retired 0: no references of that incarnation` and fails, a wrong INCARNATION the likely cause,
//! unless `--none-ok`. The daemon refuses its own incarnation (`EINVAL`). `ephemeral-hosts` prints
//! every incarnation whose ephemeral cells have references in the buckets, or that was
//! decommissioned, a line each, `INCARNATION REFERENCES`, then `decommissioned` after one that
//! was: a lost host's incarnation, found without its disk or its log.
//!
//! `format-bucket`, `add-bucket` and `buckets` act on buckets themselves, with or without the
//! daemon (TASK-633.9, backlog doc-23). A bucket's number is in the high bits of every cell ID it
//! gives, so that no two buckets give one ID. `format-bucket` makes the bucket at URL, unless it
//! was formatted or added, the root of buckets of its own, number 1, and prints its number; a
//! bucket never formatted is made a root so by the first host that makes a cell in it. `add-bucket`
//! adds the bucket at URL, never formatted, to the buckets of the root bucket at ROOT: the next
//! number, which it prints, given by a conditional write of the root's record of every bucket,
//! which then goes to the root of every bucket. Refused if URL is a bucket already (`EEXIST`), or
//! ROOT is not the root (`EINVAL`). DETAILS, recorded of the bucket, are any of `--region REGION`,
//! `--endpoint URL`, `--kms URI` (its `CLADEFS_KMS`) and `--master-key-id ID` (under the bucket
//! KMS): never a key or a credential. Its provider and URL are recorded too. `buckets` prints the
//! record in the root of the bucket at URL: its version, then each bucket, a line each, its number
//! and its details, `name=value`.
//!
//! `mount`, through the daemon alone, calls the Mount service of its FUSE mount of tenants' cells
//! (`CLADEFSD_MOUNT`, TASK-475.2; `proto/cladefs/mount/v1/mount.proto`), TENANT and CELL in base 32
//! (`base32_id`).
//! `attach` takes CELL from the daemon's bucket, at the placement whose fencing token is TOKEN if
//! given, the daemon's otherwise, and shows it, if an Fxfs cell, at `MOUNT/TENANT/CELL`; it
//! prints the cell. `create` makes a new Fxfs cell of TENANT's, its filesystem empty, of CAPACITY
//! bytes, attached so, and prints the same.
//! `detach` takes the cell away, and returns once its cell is unmounted and written
//! back: once nothing has a file of it open, or a directory of it as its current one; with
//! `--force`, at once, what is open of it failing from then on. `list` prints the cells attached,
//! a line each, `TENANT CELL`, and `detaching` after one being detached.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]
// cladefs: its commands' futures, which hold the cells' (TASK-685.3), nest deeper than the default.
#![recursion_limit = "256"]

// cladefs: Linux release binaries of cladefsctl are static musl builds only (with mimalloc).
#[cfg(all(target_os = "linux", not(target_env = "musl"), not(debug_assertions)))]
compile_error!(
    "Linux release builds of cladefsctl must target musl (use scripts/build-musl.sh or \
     --target <arch>-unknown-linux-musl)"
);

// cladefs: musl's default mallocng allocator contends heavily under multi-threaded allocation;
// mimalloc uses lock-free per-thread heaps and detects OS page size at runtime.
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(madsim))]
fn main() -> anyhow::Result<()> {
    cli::main()
}

#[cfg(madsim)]
fn main() {
    eprintln!("cladefsctl does not run under madsim");
    std::process::exit(2);
}

#[cfg(not(madsim))]
mod cli {
    use anyhow::{Context as _, bail};
    use cladefs::OpenCell;
    use std::path::{Path, PathBuf};

    /// The ID `given` writes, in base 32 (`base32_id`): `what`, as a refusal names it.
    fn id(what: &str, given: &str) -> anyhow::Result<u64> {
        base32_id::parse(given).with_context(|| format!("{what} must be in base 32"))
    }

    /// cladefs: a host's incarnation, in base 32, as the daemon's log, `stats` and `ephemeral-hosts`
    /// print it (board 8521, hotbed question 480).
    fn incarnation(given: &str) -> anyhow::Result<u64> {
        id("INCARNATION", given)
    }

    /// cladefs: what `ephemeral-hosts` prints of `hosts`: a line each, `INCARNATION REFERENCES`, in
    /// base 32 and decimal, then `decommissioned` after one that was (board 8521).
    #[cfg(target_os = "linux")]
    fn ephemeral_hosts_lines(hosts: &[cladefs::cladefsd::control::proto::EphemeralHost]) -> String {
        let line = |host: &cladefs::cladefsd::control::proto::EphemeralHost| {
            let decommissioned = if host.decommissioned { " decommissioned" } else { "" };
            let incarnation = base32_id::Short(host.incarnation);
            format!("{incarnation} {}{decommissioned}\n", host.references)
        };
        hosts.iter().map(line).collect()
    }

    /// cladefs: the number `given` writes, in decimal: `what`, as a refusal names it.
    fn number(what: &str, given: &str) -> anyhow::Result<u64> {
        given.parse().with_context(|| format!("{what} must be a number"))
    }

    /// cladefs: the tenant `args` name, `--tenant TENANT` first, if they do, and the one word after
    /// (board 8585).
    fn of_tenant<'a>(args: &[&'a str]) -> anyhow::Result<(Option<u64>, &'a str)> {
        match *args {
            ["--tenant", tenant, word] => Ok((Some(id("TENANT", tenant)?), word)),
            [word] => Ok((None, word)),
            _ => bail!(usage()),
        }
    }

    /// cladefs: the bytes `given` writes, or none if it is `word` (board 8585).
    fn bytes_or(given: &str, word: &str) -> anyhow::Result<Option<u64>> {
        match given == word {
            true => Ok(None),
            false => number(&format!("BYTES (or {word})"), given).map(Some),
        }
    }

    /// cladefs: the `stats` command `args` say, the words after `stats` (TASK-111.4, board 8581).
    fn stats(args: &[&str]) -> anyhow::Result<Command> {
        let (mut json, mut wait, mut leave_out) = (false, None, [false; 3]);
        let mut rest = args;
        loop {
            rest = match *rest {
                [] => break,
                ["--json", ref rest @ ..] => {
                    json = true;
                    rest
                }
                ["--wait", given, ref rest @ ..] => {
                    wait = Some(seconds("--wait", given)?);
                    rest
                }
                ["--leave-out", what, ref rest @ ..] => {
                    let named =
                        ["histograms", "guests", "vms"].iter().position(|&named| named == what);
                    let Some(left_out) = named else { bail!(usage()) };
                    leave_out[left_out] = true;
                    rest
                }
                _ => bail!(usage()),
            };
        }
        Ok(Command::Stats { json, wait, leave_out })
    }

    /// cladefs: the time `given` writes, a positive number of seconds: `what`, as a refusal names it
    /// (board 8585).
    fn seconds(what: &str, given: &str) -> anyhow::Result<std::time::Duration> {
        // Not a number, or not one a time is: refused below.
        let seconds = given.parse::<f64>().ok().filter(|seconds| *seconds > 0.0);
        // Past what a time holds: refused below.
        let duration =
            seconds.and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok());
        duration.with_context(|| format!("{what} must be a positive number of seconds: {given:?}"))
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
         cladefsctl sync DIR | cladefsctl import DIR CAPACITY IMAGE [REF] | \
         cladefsctl import-memory [--version] DIR FILE | cladefsctl fork SRC DST | \
         cladefsctl version DIR | cladefsctl versions DIR | \
         cladefsctl drop-versions DIR VERSION... | cladefsctl share DIR VERSION | \
         cladefsctl unshare DIR VERSION | cladefsctl fork-version CELL VERSION DST | \
         cladefsctl delete DIR | cladefsctl drop DIR | \
         cladefsctl resize DIR CAPACITY | cladefsctl public DIR | \
         cladefsctl public-attach-only DIR | cladefsctl attach DIR BASE | cladefsctl stat DIR | \
         cladefsctl stats [--json] [--wait SECONDS] [--leave-out histograms|guests|vms]... | \
         cladefsctl cells [--json] | cladefsctl counters [--json] | \
         cladefsctl set-peers [--bucket URL] [ADDRESS...] | cladefsctl migrate DIR PEER TO | \
         cladefsctl hand-over [--token TOKEN] DIR | cladefsctl abort-migration DIR | \
         cladefsctl memory-limit [--tenant TENANT] BYTES|none | \
         cladefsctl memory-weight TENANT WEIGHT | \
         cladefsctl in-flight [--tenant TENANT] BYTES|default | \
         cladefsctl shrink-memory [--tenant TENANT] TARGET | \
         cladefsctl disk-limit [--tenant TENANT] BYTES|none | cladefsctl disk-weight TENANT WEIGHT | \
         cladefsctl tenant-disk [--tenant TENANT] [--json] | \
         cladefsctl grant TENANT DIR [UID [GID]] | \
         cladefsctl revoke TENANT | cladefsctl shred-cell [--bucket URL] TENANT/CELL | \
         cladefsctl forget-tenant-keys TENANT | cladefsctl shred-tenant [--bucket URL] TENANT | \
         cladefsctl kms reload | \
         cladefsctl decommission-host [--none-ok] INCARNATION | cladefsctl ephemeral-hosts | \
         cladefsctl mount attach TENANT CELL [TOKEN] | \
         cladefsctl mount create TENANT CAPACITY [TOKEN] | \
         cladefsctl mount detach [--force] TENANT CELL | cladefsctl mount list | \
         cladefsctl format-bucket URL [DETAILS] | cladefsctl add-bucket ROOT URL [DETAILS] | \
         cladefsctl buckets URL | cladefsctl --version; \
         any command after --timeout SECONDS"
        )
    }

    /// cladefs: what a daemon forgot of a tenant's or a cell's keys, one `name value` a line
    /// (TASK-411.7).
    #[cfg(target_os = "linux")]
    fn forgotten_lines(forgotten: &cladefs::cladefsd::control::proto::Forgotten) -> String {
        let cladefs::cladefsd::control::proto::Forgotten { stores, rings, kept, extents, bytes } =
            forgotten;
        format!(
            "stores_halted {stores}\nrings_forgotten {rings}\nkept_dropped {kept}\n\
         extents_evicted {extents}\nbytes_evicted {bytes}\n"
        )
    }

    /// cladefs: what a reload of the daemon's KMS changed, one `name values` a line, tenants in base 32
    /// (board 8487).
    #[cfg(target_os = "linux")]
    fn reloaded_lines(reloaded: &cladefs::cladefsd::control::proto::ReloadKmsResponse) -> String {
        let renamed = (reloaded.renamed.iter())
            .map(|&tenant| format!("renamed {}\n", base32_id::Short(tenant)));
        let keys = reloaded.master_keys.iter().flat_map(|keys| {
            let added =
                keys.added.iter().map(|id| format!("master_key_added {} {id}\n", keys.bucket));
            let removed =
                keys.removed.iter().map(|id| format!("master_key_removed {} {id}\n", keys.bucket));
            added.chain(removed)
        });
        renamed.chain(keys).collect()
    }

    /// What cladefsctl was asked to do. cladefs: what only the daemon does is read by `through`
    /// alone, which is Linux's alone.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    enum Command {
        /// cladefs: a new cell of a new cell, its ID its bucket's to give (TASK-364).
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
        /// cladefs: a new memory cell holding a snapshot memory file (board 11222).
        ImportMemory {
            dir: PathBuf,
            file: PathBuf,
            make_version: bool,
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
        /// (TASK-111.4), the VM processes' answers waited for `wait` at most, the daemon's default
        /// without, and what `leave_out` says left out: the guests' histograms, their stats, the VM
        /// processes (board 8581).
        Stats {
            json: bool,
            wait: Option<std::time::Duration>,
            leave_out: [bool; 3],
        },
        /// cladefs: the cells VM processes have open, and what each is and holds (board 8585).
        Cells {
            json: bool,
        },
        /// cladefs: the daemon's counters (board 8585).
        Counters {
            json: bool,
        },
        /// cladefs: the hosts sharing the daemon's bucket (board 8585), or published cluster-wide
        /// to `events` when `bucket` is given.
        SetPeers {
            bucket: Option<String>,
            addresses: Vec<String>,
        },
        /// cladefs: a cell's live migration begun, to `peer`, at `to` there (board 8585, board 11369).
        Migrate {
            dir: PathBuf,
            peer: String,
            to: PathBuf,
        },
        /// cladefs: a cell migrating handed over to its target, placing a bucket-backed cell at
        /// `token` if given (board 8585, board 11369).
        HandOver {
            dir: PathBuf,
            token: Option<u64>,
        },
        /// cladefs: an ephemeral cell's migration ended (board 8577).
        AbortMigration {
            dir: PathBuf,
        },
        /// cladefs: a tenant's memory limited, or every tenant's without one, or lifted without
        /// `bytes` (board 8585).
        MemoryLimit {
            tenant: Option<u64>,
            bytes: Option<u64>,
        },
        /// cladefs: a tenant's memory weighed against the others' (board 8585).
        MemoryWeight {
            tenant: u64,
            weight: u64,
        },
        /// cladefs: the bytes in flight a tenant's connections share, or every tenant's without one,
        /// the default without `bytes` (board 8585).
        InFlight {
            tenant: Option<u64>,
            bytes: Option<u64>,
        },
        /// cladefs: a tenant's pages evicted, or the daemon's without one, down to `target` bytes
        /// (board 8585).
        ShrinkMemory {
            tenant: Option<u64>,
            target: u64,
        },
        /// cladefs: a tenant's disk limited, or every tenant's without one, or lifted without `bytes`
        /// (board 8585).
        DiskLimit {
            tenant: Option<u64>,
            bytes: Option<u64>,
        },
        /// cladefs: a tenant's disk weighed against the others' (board 8585).
        DiskWeight {
            tenant: u64,
            weight: u64,
        },
        /// cladefs: what a tenant's disk takes, or every tenant's without one (board 8585).
        TenantDisk {
            tenant: Option<u64>,
            json: bool,
        },
        /// cladefs: a tenant, of the ID `tenant` in the bucket (TASK-364), granted a VM socket of its
        /// own, for its cells in `dir`, given to `uid` and `gid` if named (TASK-71). Read by
        /// `through`, which is Linux's alone.
        Grant {
            tenant: u64,
            dir: PathBuf,
            uid: Option<u32>,
            gid: Option<u32>,
        },
        /// cladefs: a tenant's grant revoked (TASK-71).
        Revoke {
            tenant: u64,
        },
        /// cladefs: a cell shredded (TASK-411.7), through the daemon or directly against `bucket`.
        ShredCell {
            bucket: Option<String>,
            cell: cladefs::CellId,
        },
        /// cladefs: a tenant's keys forgotten, its KEK revoked or destroyed (TASK-411.7).
        ForgetTenantKeys {
            tenant: u64,
        },
        /// cladefs: a tenant shredded under the bucket KMS (TASK-411.27), through the daemon or
        /// keylessly against `bucket`.
        ShredTenant {
            bucket: Option<String>,
            tenant: u64,
        },
        /// cladefs: the daemon's KMS configuration reloaded (board 8487).
        KmsReload,
        /// cladefs: a host of incarnation `incarnation` decommissioned, retiring none refused unless
        /// `none_ok` (boards 8519, 8521).
        DecommissionHost {
            incarnation: u64,
            none_ok: bool,
        },
        /// cladefs: the incarnations of hosts with ephemeral cells' references (board 8521).
        EphemeralHosts,
        /// cladefs: the daemon's FUSE mount of cells (TASK-475.2).
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
        async fn through(
            self,
            socket: &Path,
            timeout: Option<std::time::Duration>,
        ) -> anyhow::Result<()> {
            use cladefs::fuse::control::proto::{
                AttachRequest, DetachRequest, ListRequest, mount_client::MountClient,
            };
            let channel =
                connect_within(timeout, socket, cladefs::cladefsd::control::channel(socket))
                    .await?;
            let mut mount = MountClient::new(channel);
            match self {
                Self::Attach { cell, capacity, token } => {
                    let request = AttachRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        token,
                        capacity,
                        upload_interval_ms: None,
                    };
                    let attached =
                        rpc(timeout, format!("attaching cell {cell}"), mount.attach(request))
                            .await?;
                    println!("{}", base32_id::Padded(attached.cell));
                }
                Self::Detach { cell, force } => {
                    let request = DetachRequest { tenant: cell.tenant, cell: cell.id, force };
                    rpc(timeout, format!("detaching cell {cell}"), mount.detach(request)).await?;
                }
                Self::List => {
                    let listed =
                        rpc(timeout, "listing the cells", mount.list(ListRequest {})).await?;
                    for cell in listed.cells {
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

    #[cfg(target_os = "linux")]
    async fn connect_within<T>(
        timeout: Option<std::time::Duration>,
        socket: &Path,
        call: impl std::future::Future<Output = Result<T, tonic::transport::Error>>,
    ) -> anyhow::Result<T> {
        let what =
            format!("connecting to the extent daemon's control socket at {}", socket.display());
        within(timeout, &what, call).await?.context(what)
    }

    #[cfg(target_os = "linux")]
    async fn within<T>(
        timeout: Option<std::time::Duration>,
        what: &str,
        call: impl std::future::Future<Output = T>,
    ) -> anyhow::Result<T> {
        match timeout {
        Some(timeout) => tokio::time::timeout(timeout, call).await.map_err(|_| {
            let said = format!(
                "no answer within {timeout:?}: the extent daemon may still complete the call, as it \
                 completes every call whether or not its caller waits"
            );
            anyhow::Error::new(cladefs::Errno::ETIMEDOUT).context(said).context(what.to_owned())
        }),
        None => Ok(call.await),
    }
    }

    #[cfg(target_os = "linux")]
    async fn rpc<T>(
        timeout: Option<std::time::Duration>,
        what: impl Into<String>,
        call: impl std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> anyhow::Result<T> {
        let what = what.into();
        // Keep the daemon's errno and the explanation it sent.
        let answered = within(timeout, &what, call).await?;
        Ok(answered
            .map_err(|status| cladefs::cladefsd::control::failed(what, status))?
            .into_inner())
    }

    #[cfg(target_os = "linux")]
    fn name(path: &Path) -> Result<String, cladefs::Error> {
        path.to_str().map(str::to_owned).ok_or_else(|| cladefs::Errno::EINVAL.into())
    }

    #[cfg(target_os = "linux")]
    fn whom(tenant: Option<u64>, none: &str) -> String {
        tenant.map_or_else(
            || none.to_owned(),
            |tenant| format!("tenant {}", base32_id::Short(tenant)),
        )
    }

    impl Command {
        fn parse(args: &[String]) -> anyhow::Result<Self> {
            let capacity = |capacity: &str| capacity.parse().context("CAPACITY must be a number");
            let token = |token: &str| token.parse().context("TOKEN must be a number");
            let id =
                |id: &str, what| id.parse().with_context(|| format!("{what} must be a number"));
            let tenant = |tenant: &str| self::id("TENANT", tenant);
            Ok(match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["take", "--force", dir, size, cell, ref rest @ ..] if rest.len() <= 1 => {
                    Self::Take {
                        dir: dir.into(),
                        capacity: capacity(size)?,
                        cell: cell.parse().map_err(anyhow::Error::msg).context("CELL")?,
                        token: rest.first().map(|rest| token(rest)).transpose()?,
                        force: true,
                    }
                }
                ["take", dir, size, cell, ref rest @ ..] if rest.len() <= 1 => Self::Take {
                    dir: dir.into(),
                    capacity: capacity(size)?,
                    cell: cell.parse().map_err(anyhow::Error::msg).context("CELL")?,
                    token: rest.first().map(|rest| token(rest)).transpose()?,
                    force: false,
                },
                ["create", dir, size] => {
                    Self::Create { dir: dir.into(), capacity: capacity(size)? }
                }
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
                ["import-memory", "--version", dir, file] => {
                    Self::ImportMemory { dir: dir.into(), file: file.into(), make_version: true }
                }
                ["import-memory", dir, file] => {
                    Self::ImportMemory { dir: dir.into(), file: file.into(), make_version: false }
                }
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
                ["resize", dir, size] => {
                    Self::Resize { dir: dir.into(), capacity: capacity(size)? }
                }
                ["public", dir] => Self::Public { dir: dir.into() },
                ["public-attach-only", dir] => Self::PublicAttachOnly { dir: dir.into() },
                ["attach", dir, base] => Self::Attach { dir: dir.into(), base: base.into() },
                ["stat", dir] => Self::Stat { dir: dir.into() },
                ["stats", ref rest @ ..] => stats(rest)?,
                ["cells"] => Self::Cells { json: false },
                ["cells", "--json"] => Self::Cells { json: true },
                ["counters"] => Self::Counters { json: false },
                ["counters", "--json"] => Self::Counters { json: true },
                ["set-peers", "--bucket", url, ref addresses @ ..] => Self::SetPeers {
                    bucket: Some((*url).to_owned()),
                    addresses: addresses.iter().map(|&address| address.into()).collect(),
                },
                ["set-peers", ref addresses @ ..] => Self::SetPeers {
                    bucket: None,
                    addresses: addresses.iter().map(|&address| address.into()).collect(),
                },
                ["migrate", dir, peer, to] => {
                    Self::Migrate { dir: dir.into(), peer: peer.to_owned(), to: to.into() }
                }
                ["hand-over", dir] => Self::HandOver { dir: dir.into(), token: None },
                ["hand-over", "--token", token, dir] => {
                    Self::HandOver { dir: dir.into(), token: Some(number("TOKEN", token)?) }
                }
                ["abort-migration", dir] => Self::AbortMigration { dir: dir.into() },
                ["memory-limit", ref rest @ ..] => {
                    let (tenant, bytes) = of_tenant(rest)?;
                    Self::MemoryLimit { tenant, bytes: bytes_or(bytes, "none")? }
                }
                ["memory-weight", given, weight] => {
                    Self::MemoryWeight { tenant: tenant(given)?, weight: number("WEIGHT", weight)? }
                }
                ["in-flight", ref rest @ ..] => {
                    let (tenant, bytes) = of_tenant(rest)?;
                    Self::InFlight { tenant, bytes: bytes_or(bytes, "default")? }
                }
                ["shrink-memory", ref rest @ ..] => {
                    let (tenant, target) = of_tenant(rest)?;
                    Self::ShrinkMemory { tenant, target: number("TARGET", target)? }
                }
                ["disk-limit", ref rest @ ..] => {
                    let (tenant, bytes) = of_tenant(rest)?;
                    Self::DiskLimit { tenant, bytes: bytes_or(bytes, "none")? }
                }
                ["disk-weight", given, weight] => {
                    Self::DiskWeight { tenant: tenant(given)?, weight: number("WEIGHT", weight)? }
                }
                ["tenant-disk", ref rest @ ..] => {
                    let (json, rest) = match rest {
                        [rest @ .., "--json"] => (true, rest),
                        rest => (false, rest),
                    };
                    let tenant = match rest {
                        [] => None,
                        ["--tenant", given] => Some(tenant(given)?),
                        _ => bail!(usage()),
                    };
                    Self::TenantDisk { tenant, json }
                }
                ["grant", given, dir, ref ids @ ..] if ids.len() <= 2 => Self::Grant {
                    tenant: tenant(given)?,
                    dir: dir.into(),
                    uid: ids.first().map(|uid| id(uid, "UID")).transpose()?,
                    gid: ids.get(1).map(|gid| id(gid, "GID")).transpose()?,
                },
                ["revoke", given] => Self::Revoke { tenant: tenant(given)? },
                ["shred-cell", "--bucket", url, cell] => {
                    let (tenant, id) = cell.split_once('/').context("CELL must be TENANT/CELL")?;
                    let cell = cladefs::CellId {
                        tenant: self::id("TENANT", tenant)?,
                        id: self::id("CELL", id)?,
                    };
                    Self::ShredCell { bucket: Some((*url).to_owned()), cell }
                }
                ["shred-cell", cell] => {
                    let (tenant, id) = cell.split_once('/').context("CELL must be TENANT/CELL")?;
                    let cell = cladefs::CellId {
                        tenant: self::id("TENANT", tenant)?,
                        id: self::id("CELL", id)?,
                    };
                    Self::ShredCell { bucket: None, cell }
                }
                ["forget-tenant-keys", given] => Self::ForgetTenantKeys { tenant: tenant(given)? },
                ["shred-tenant", "--bucket", url, given] => {
                    Self::ShredTenant { bucket: Some((*url).to_owned()), tenant: tenant(given)? }
                }
                ["shred-tenant", given] => {
                    Self::ShredTenant { bucket: None, tenant: tenant(given)? }
                }
                ["kms", "reload"] => Self::KmsReload,
                ["decommission-host", given] => {
                    Self::DecommissionHost { incarnation: incarnation(given)?, none_ok: false }
                }
                ["decommission-host", "--none-ok", given] => {
                    Self::DecommissionHost { incarnation: incarnation(given)?, none_ok: true }
                }
                ["ephemeral-hosts"] => Self::EphemeralHosts,
                ["mount", ref rest @ ..] => Self::Mount(Mount::parse(rest)?),
                ["format-bucket", url, ref rest @ ..] => {
                    Self::FormatBucket { url: url.to_owned(), details: details(rest)? }
                }
                ["add-bucket", root, url, ref rest @ ..] => {
                    let details = details(rest)?;
                    Self::AddBucket { root: root.to_owned(), url: url.to_owned(), details }
                }
                ["buckets", url] => Self::Buckets { url: url.to_owned() },
                _ => bail!(usage()),
            })
        }

        /// Does it through the control API of the daemon whose control socket is at `socket`, within
        /// `timeout` if given (board 8585).
        #[cfg(target_os = "linux")]
        async fn through(
            self,
            socket: &Path,
            timeout: Option<std::time::Duration>,
        ) -> anyhow::Result<()> {
            use cladefs::cladefsd::control::{self, proto::*};

            if let Self::Mount(mount) = self {
                return mount.through(socket, timeout).await;
            }
            let mut daemon = connect_within(timeout, socket, control::connect(socket)).await?;
            macro_rules! call {
                ($what:expr, $method:ident, $request:expr) => {
                    rpc(timeout, $what, daemon.$method($request)).await?
                };
            }
            match self {
                Self::Create { dir, capacity } => {
                    let request = CreateCellRequest {
                        path: name(&dir)?,
                        capacity,
                        kind: CellKind::Fxfs.into(),
                        ..Default::default()
                    };
                    let made = call!(
                        format!("making the cell at {}", dir.display()),
                        create_cell,
                        request
                    );
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Take { dir, capacity, cell, token, force } => {
                    let request = CreateCellRequest {
                        path: name(&dir)?,
                        capacity,
                        tenant: cell.tenant,
                        cell: cell.id,
                        token,
                        force,
                        kind: CellKind::Fxfs.into(),
                        ..Default::default()
                    };
                    call!(format!("making the cell at {}", dir.display()), create_cell, request);
                }
                Self::Place { dir, token, force } => {
                    let request = PlaceCellRequest { path: name(&dir)?, token, force };
                    call!(format!("placing the cell at {}", dir.display()), place_cell, request);
                }
                Self::Sync { dir } => {
                    let request = SyncCellRequest { path: name(&dir)? };
                    call!(
                        format!("waiting for the cell at {} to be uploaded", dir.display()),
                        sync_cell,
                        request
                    );
                }
                Self::Import { dir, capacity, image, reference } => {
                    let request = ImportImageRequest {
                        path: name(&dir)?,
                        capacity,
                        image: name(&image)?,
                        reference: reference.unwrap_or_default(),
                        tenant: bucket_cell::layout::DEFAULT_TENANT,
                    };
                    call!(format!("importing {}", image.display()), import_image, request);
                }
                Self::ImportMemory { dir, file, make_version } => {
                    let request = ImportMemoryRequest {
                        path: name(&dir)?,
                        file: name(&file)?,
                        tenant: bucket_cell::layout::DEFAULT_TENANT,
                        make_version,
                        ephemeral: false,
                    };
                    let imported = call!(
                        format!("importing memory {}", file.display()),
                        import_memory,
                        request
                    );
                    let cell = cladefs::CellId { tenant: imported.tenant, id: imported.cell };
                    if imported.version != 0 {
                        println!("{cell} {:x}", imported.version);
                    } else {
                        println!("{cell}");
                    }
                }
                Self::Fork { src, dst } => {
                    let request = ForkCellRequest {
                        from: name(&src)?,
                        to: name(&dst)?,
                        ..Default::default()
                    };
                    let made = call!(
                        format!("forking the cell at {} to {}", src.display(), dst.display()),
                        fork_cell,
                        request
                    );
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Version { dir } => {
                    let request = MakeVersionRequest { path: name(&dir)?, keep_last: 0 };
                    let made = call!(
                        format!("making a version of the cell at {}", dir.display()),
                        make_version,
                        request
                    );
                    println!("{:x}", made.version);
                }
                Self::Versions { dir } => {
                    let request = ListVersionsRequest { path: name(&dir)? };
                    let listed = call!(
                        format!("listing the versions of the cell at {}", dir.display()),
                        list_versions,
                        request
                    );
                    let versions: Vec<_> = listed
                        .versions
                        .into_iter()
                        .map(|Version { id, uploaded, shared }| cladefs::VersionInfo {
                            id,
                            uploaded,
                            shared,
                        })
                        .collect();
                    print_versions(&versions);
                }
                Self::DropVersions { dir, versions } => {
                    let request = DropVersionsRequest { path: name(&dir)?, versions };
                    call!(
                        format!("dropping versions of the cell at {}", dir.display()),
                        drop_versions,
                        request
                    );
                }
                Self::Share { dir, version, shared } => {
                    let request = ShareVersionRequest { path: name(&dir)?, version, shared };
                    call!(
                        format!("sharing version {version} of the cell at {}", dir.display()),
                        share_version,
                        request
                    );
                }
                Self::ForkVersion { from, dst } => {
                    let cladefs::CellVersion { tenant, cell, version } = from;
                    let request = ForkCellRequest {
                        to: name(&dst)?,
                        version,
                        tenant,
                        cell,
                        ..Default::default()
                    };
                    let made = call!(
                        format!("forking version {version} of cell {cell} to {}", dst.display()),
                        fork_cell,
                        request
                    );
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Delete { dir } => {
                    let request = DeleteCellRequest { path: name(&dir)? };
                    call!(format!("deleting the cell at {}", dir.display()), delete_cell, request);
                }
                Self::Drop { dir } => {
                    let request = DropCellRequest { path: name(&dir)? };
                    call!(format!("dropping the cell at {}", dir.display()), drop_cell, request);
                }
                Self::Resize { dir, capacity } => {
                    let request = ResizeCellRequest { path: name(&dir)?, capacity };
                    call!(
                        format!("resizing the cell at {} to {capacity} bytes", dir.display()),
                        resize_cell,
                        request
                    );
                }
                Self::Public { ref dir } | Self::PublicAttachOnly { ref dir } => {
                    let attach_only = matches!(self, Self::PublicAttachOnly { .. });
                    let request = MakePublicRequest { path: name(dir)?, attach_only };
                    let purpose = if attach_only { " public to attach" } else { " public" };
                    call!(
                        format!("making the cell at {}{purpose}", dir.display()),
                        make_public,
                        request
                    );
                }
                Self::Attach { dir, base } => {
                    let request = AttachBaseRequest { path: name(&dir)?, base: name(&base)? };
                    call!(
                        format!("attaching {} to {}", base.display(), dir.display()),
                        attach_base,
                        request
                    );
                }
                Self::Stat { dir } => {
                    let request = StatCellRequest { path: name(&dir)? };
                    let stat = call!(
                        format!("asking what the cell at {} holds", dir.display()),
                        stat_cell,
                        request
                    );
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
                Self::Stats { json, wait, leave_out } => {
                    // At least a millisecond: 0 asks for the daemon's default (hotbed review 67).
                    let wait_ms = wait.map_or(Ok(0), |wait| u32::try_from(wait.as_millis().max(1)));
                    let [leave_out_histograms, leave_out_guests, leave_out_vms] = leave_out;
                    let request = GetStatsRequest {
                        wait_ms: wait_ms.context("--wait: past what the daemon waits")?,
                        leave_out_histograms,
                        leave_out_guests,
                        leave_out_vms,
                    };
                    let stats = call!("asking the extent daemon for its stats", get_stats, request);
                    print_value(&stats_json(&stats), json)?;
                }
                Self::Cells { json } => {
                    let mut cells =
                        call!("listing the daemon's cells", list_cells, ListCellsRequest {}).stores;
                    cells.sort_by(|one, other| one.path.cmp(&other.path));
                    let cells = cells.iter().map(|cell| {
                        let mut listed = serde_json::json!({ "path": cell.path });
                        if let Some(stat) = &cell.stat {
                            listed["stat"] = stat_json(stat);
                        }
                        listed
                    });
                    print_value(&serde_json::json!({ "cells": cells.collect::<Vec<_>>() }), json)?;
                }
                Self::Counters { json } => {
                    let counters = call!(
                        "asking the extent daemon for its counters",
                        get_counters,
                        GetCountersRequest {}
                    );
                    print_value(&counters_json(&counters), json)?;
                }
                Self::SetPeers { addresses, bucket: None } => {
                    call!("setting the daemon's peers", set_peers, SetPeersRequest { addresses });
                }
                Self::SetPeers { bucket: Some(_), .. } => unreachable!("served above"),
                Self::Migrate { dir, peer, to } => {
                    let what = format!("migrating the cell at {} to {peer}", dir.display());
                    let request = MigrateCellRequest { path: name(&dir)?, peer, to: name(&to)? };
                    println!("migration {}", call!(what, migrate_cell, request).migration);
                }
                Self::HandOver { dir, token } => {
                    let request = HandOverCellRequest { path: name(&dir)?, token };
                    call!(
                        format!("handing the cell at {} over", dir.display()),
                        hand_over_cell,
                        request
                    );
                }
                Self::AbortMigration { dir } => {
                    let request = AbortMigrationRequest { path: name(&dir)? };
                    call!(
                        format!("aborting the migration of the cell at {}", dir.display()),
                        abort_migration,
                        request
                    );
                }
                Self::MemoryLimit { tenant, bytes } => {
                    call!(
                        format!("limiting the memory of {}", whom(tenant, "every tenant")),
                        set_memory_limit,
                        SetMemoryLimitRequest { tenant, bytes }
                    );
                }
                Self::MemoryWeight { tenant, weight } => {
                    call!(
                        format!("weighing the memory of {}", whom(Some(tenant), "")),
                        set_memory_weight,
                        SetMemoryWeightRequest { tenant, weight }
                    );
                }
                Self::InFlight { tenant, bytes } => {
                    call!(
                        format!("setting the bytes in flight of {}", whom(tenant, "every tenant")),
                        set_tenant_in_flight,
                        SetTenantInFlightRequest { tenant, bytes }
                    );
                }
                Self::ShrinkMemory { tenant, target } => {
                    let request = ShrinkMemoryRequest { tenant, target_bytes: target };
                    let shrunk = call!(
                        format!("shrinking the memory of {}", whom(tenant, "the daemon")),
                        shrink_memory,
                        request
                    );
                    println!("bytes {}", shrunk.bytes);
                }
                Self::DiskLimit { tenant, bytes } => {
                    call!(
                        format!("limiting the disk of {}", whom(tenant, "every tenant")),
                        set_disk_limit,
                        SetDiskLimitRequest { tenant, bytes }
                    );
                }
                Self::DiskWeight { tenant, weight } => {
                    call!(
                        format!("weighing the disk of {}", whom(Some(tenant), "")),
                        set_disk_weight,
                        SetDiskWeightRequest { tenant, weight }
                    );
                }
                Self::TenantDisk { tenant, json } => {
                    let disk = call!(
                        format!("counting the disk of {}", whom(tenant, "every tenant")),
                        get_tenant_disk,
                        GetTenantDiskRequest { tenant }
                    );
                    let disk = serde_json::json!({
                        "bytes": disk.bytes,
                        "limit_bytes": disk.limit_bytes,
                        "reserved_bytes": disk.reserved_bytes,
                    });
                    print_value(&disk, json)?;
                }
                Self::Grant { tenant, dir, uid, gid } => {
                    let parts: Vec<PathBuf> =
                        std::env::split_paths(&dir).filter(|p| !p.as_os_str().is_empty()).collect();
                    let (first, rest) = match parts.as_slice() {
                        [only] => (name(only)?, Vec::new()),
                        [first, rest @ ..] => {
                            let mut extra = Vec::with_capacity(rest.len());
                            for p in rest {
                                extra.push(name(p)?);
                            }
                            (name(first)?, extra)
                        }
                        [] => (name(&dir)?, Vec::new()),
                    };
                    let request = GrantRequest { tenant, dir: first, dirs: rest, uid, gid };
                    let grant =
                        call!(format!("granting {}", whom(Some(tenant), "")), grant, request);
                    println!("{}", grant.socket);
                }
                Self::Revoke { tenant } => {
                    call!(
                        format!("revoking {}", whom(Some(tenant), "")),
                        revoke,
                        RevokeRequest { tenant }
                    );
                }
                Self::ShredCell { cell, bucket: None } => {
                    let request = ShredCellRequest { tenant: cell.tenant, cell: cell.id };
                    let answer = call!(format!("shredding cell {cell}"), shred_cell, request);
                    let removed = answer.keks_removed;
                    let forgotten = answer.forgotten.unwrap_or_default();
                    print!("keks_removed {removed}\n{}", forgotten_lines(&forgotten));
                }
                Self::ShredCell { bucket: Some(_), .. } => unreachable!("served above"),
                Self::ForgetTenantKeys { tenant } => {
                    let why = format!(
                        "tenant {tenant}: its KEK is revoked or destroyed, cladefsctl says"
                    );
                    let request = ForgetTenantKeysRequest { tenant, why };
                    let answer = call!(
                        format!("having the daemon forget tenant {tenant}'s keys"),
                        forget_tenant_keys,
                        request
                    );
                    print!("{}", forgotten_lines(&answer.forgotten.unwrap_or_default()));
                }
                Self::ShredTenant { tenant, bucket: None } => {
                    let answer = call!(
                        format!("shredding tenant {tenant}"),
                        shred_tenant,
                        ShredTenantRequest { tenant }
                    );
                    let now = answer.shredded_now;
                    let forgotten = answer.forgotten.unwrap_or_default();
                    print!("shredded_now {}\n{}", u8::from(now), forgotten_lines(&forgotten));
                }
                Self::ShredTenant { bucket: Some(_), .. } => unreachable!("served above"),
                Self::KmsReload => {
                    let reloaded =
                        call!("reloading the daemon's KMS", reload_kms, ReloadKmsRequest {});
                    print!("{}", reloaded_lines(&reloaded));
                }
                Self::DecommissionHost { incarnation, none_ok } => {
                    // Called again while the daemon left some, each call retiring its most (board
                    // 8523); one that retired none, of a listing it says is not empty, would never end.
                    let mut retired = 0;
                    loop {
                        let request = DecommissionHostRequest { incarnation, most: 0 };
                        let done = call!(
                            format!(
                                "decommissioning incarnation {}",
                                base32_id::Short(incarnation)
                            ),
                            decommission_host,
                            request
                        );
                        retired += done.retired;
                        if done.left == 0 || done.retired == 0 {
                            break;
                        }
                        eprintln!("retired {retired}, {} left", done.left);
                    }
                    match retired {
                        0 => {
                            println!("retired 0: no references of that incarnation");
                            if !none_ok {
                                bail!(
                                    "incarnation {}: nothing retired, a wrong INCARNATION the likely \
                                 cause (cladefsctl ephemeral-hosts lists them; --none-ok takes it)",
                                    base32_id::Short(incarnation)
                                );
                            }
                        }
                        retired => println!("retired {retired}"),
                    }
                }
                Self::EphemeralHosts => {
                    let listed = call!(
                        "listing the ephemeral cells' hosts",
                        list_ephemeral_hosts,
                        ListEphemeralHostsRequest {}
                    );
                    print!("{}", ephemeral_hosts_lines(&listed.hosts));
                }
                Self::Mount(_)
                | Self::FormatBucket { .. }
                | Self::AddBucket { .. }
                | Self::Buckets { .. } => unreachable!("served above"),
            }
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        fn through(
            self,
            _socket: &Path,
            _timeout: Option<std::time::Duration>,
        ) -> anyhow::Result<()> {
            bail!("the extent daemon runs on Linux only")
        }

        /// cladefs: whether it acts on buckets themselves, with or without the daemon (TASK-633.9).
        fn is_on_buckets(&self) -> bool {
            matches!(
                self,
                Self::FormatBucket { .. }
                    | Self::AddBucket { .. }
                    | Self::Buckets { .. }
                    | Self::ShredTenant { bucket: Some(_), .. }
                    | Self::ShredCell { bucket: Some(_), .. }
                    | Self::SetPeers { bucket: Some(_), .. }
            )
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
                    let identity =
                        cladefs::bucket::add_bucket(&root, &url, details).map_err(printable);
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
                Self::ShredTenant { bucket: Some(url), tenant } => {
                    let destroyed =
                        cladefs::bucket::shred_tenant_bucket(&url, tenant).map_err(printable)?;
                    let now = destroyed == bucket_cell::bucket_kms::Destroyed::Now;
                    println!("shredded_now {}", u8::from(now));
                }
                Self::ShredCell { bucket: Some(url), cell } => {
                    let removed =
                        cladefs::bucket::shred_cell_bucket(&url, cell).map_err(printable)?;
                    println!("keks_removed {removed}");
                }
                Self::SetPeers { bucket: Some(url), addresses } => {
                    let generation =
                        cladefs::bucket::set_peers_bucket(&url, addresses).map_err(printable)?;
                    println!("generation {generation}");
                }
                _ => unreachable!("acts on no bucket"),
            }
            Ok(())
        }

        /// Does it here, on cells nothing has open: new ones anywhere; existing ones with the bucket
        /// and a placement token.
        fn locally(self) -> anyhow::Result<()> {
            if self.is_on_buckets() {
                return self.on_buckets();
            }
            // Standalone keyless fallback when CLADEFSD_GRPC is unset and CLADEFSD_BUCKET is set.
            if let Some(url) = std::env::var("CLADEFSD_BUCKET").ok().filter(|v| !v.is_empty()) {
                match self {
                    Self::ShredTenant { bucket: None, tenant } => {
                        return Self::ShredTenant { bucket: Some(url), tenant }.on_buckets();
                    }
                    Self::ShredCell { bucket: None, cell } => {
                        return Self::ShredCell { bucket: Some(url), cell }.on_buckets();
                    }
                    _ => {}
                }
            }
            let daemons = match self {
                Self::Stats { .. } => Some("stats are"),
                Self::Grant { .. } | Self::Revoke { .. } => Some("tenants' grants are"),
                Self::ShredCell { .. }
                | Self::ForgetTenantKeys { .. }
                | Self::ShredTenant { .. } => Some("shredding and forgetting keys are"),
                Self::KmsReload => Some("reloading the KMS is"),
                Self::DecommissionHost { .. } | Self::EphemeralHosts => {
                    Some("decommissioning hosts, and listing them, are")
                }
                Self::Mount(_) => Some("its FUSE mount of cells is"),
                Self::Cells { .. } => Some("the cells VM processes have open are"),
                Self::Counters { .. } => Some("its counters are"),
                Self::SetPeers { .. } => Some("its peers are"),
                Self::Migrate { .. } | Self::HandOver { .. } | Self::AbortMigration { .. } => {
                    Some("migrations are")
                }
                Self::MemoryLimit { .. }
                | Self::MemoryWeight { .. }
                | Self::InFlight { .. }
                | Self::ShrinkMemory { .. } => Some("its memory is"),
                Self::DiskLimit { .. } | Self::DiskWeight { .. } | Self::TenantDisk { .. } => {
                    Some("its disks are")
                }
                _ => None,
            };
            if let Some(what) = daemons {
                bail!("{what} the extent daemon's: name its control socket (CLADEFSD_GRPC)");
            }
            // cladefs: every cell it opens here, or deletes the cell of, is encrypted under the host's
            // KMS: none chosen, it refuses before it touches any (TASK-411.28).
            cladefs::bucket::require_kms().map_err(anyhow::Error::msg)?;
            // What stat reads needs no cell changed, nor does a copy dropped, its cell left be.
            let existing = !matches!(
                self,
                Self::Create { .. }
                    | Self::Take { .. }
                    | Self::Import { .. }
                    | Self::ImportMemory { .. }
                    | Self::Stat { .. }
                    | Self::Drop { .. }
            );
            if let Some(Err(why)) = existing.then(cladefs::bucket::Host::from_env) {
                bail!(
                    "without the extent daemon (CLADEFSD_GRPC), cladefsctl changes no existing \
                 cell here: {why}"
                );
            }
            // cladefs: outside the daemon, cladefsctl blocks on what the cells do, at its one entry
            // here; each cell it opens it closes, its uploads done (TASK-685.3).
            // An import's walk of its image makes its cell through FUSE storage, which blocks on the
            // bucket runtime itself: on no runtime's thread, as it always did.
            if let Self::Import { dir, capacity, image, reference } = self {
                let made = cladefs::import::make_base(&dir, capacity, &image, reference.as_deref());
                return futures::executor::block_on(made)
                    .with_context(|| format!("importing {}", image.display()));
            }
            if let Self::ImportMemory { dir, file, make_version } = self {
                let made = cladefs::import::import_memory(&dir, &file, make_version);
                let (cell, version) = futures::executor::block_on(made)
                    .with_context(|| format!("importing memory {}", file.display()))?;
                if let Some(version) = version {
                    println!("{cell} {version:x}");
                } else {
                    println!("{cell}");
                }
                return Ok(());
            }
            local_cell::upload::wait_on(self.locally_async())
        }

        /// cladefs: what [`Command::locally`] does, once its checks pass.
        async fn locally_async(self) -> anyhow::Result<()> {
            match self {
                Self::Create { dir, capacity } => {
                    let cell = OpenCell::create(&dir, capacity).await?;
                    println!("{}", cell.cell());
                    cell.close().await;
                }
                Self::Take { dir, capacity, cell, token, force } => {
                    cladefs::Opening::here()?
                        .force(force)
                        .take(&dir, capacity, cell, token)
                        .await?
                        .close()
                        .await;
                }
                Self::Place { dir, token, force: false } => {
                    local_cell::upload::blocking(|| OpenCell::place(&dir, token))?
                }
                Self::Place { dir, token, force: true } => {
                    local_cell::upload::blocking(|| OpenCell::place_forced(&dir, token))?
                }
                Self::Sync { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    let synced = cell.wait_uploaded().await;
                    cell.close().await;
                    synced?;
                }
                Self::Import { .. } | Self::ImportMemory { .. } => {
                    unreachable!("imported above, on no runtime's thread")
                }
                Self::Fork { src, dst } => {
                    let cell = OpenCell::open(&src).await?;
                    // A new cell of the cell's tenant's (TASK-364).
                    let tenant = cell.cell().tenant;
                    let forked = cell.fork(&dst, tenant).await;
                    cell.close().await;
                    println!("{}", forked?);
                }
                Self::Version { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    let made = cell.make_version(None).await;
                    cell.close().await;
                    println!("{:x}", made?);
                }
                Self::Versions { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    print_versions(&cell.versions().await);
                    cell.close().await;
                }
                Self::DropVersions { dir, versions } => {
                    let cell = OpenCell::open(&dir).await?;
                    let dropped = cell.drop_versions(&versions.into_iter().collect()).await;
                    cell.close().await;
                    dropped?;
                }
                Self::Share { dir, version, shared } => {
                    let cell = OpenCell::open(&dir).await?;
                    let shared = cell.share_version(version, shared).await;
                    cell.close().await;
                    shared?;
                }
                Self::ForkVersion { from, dst } => bail!(
                    "forking version {:x} of cell {:x} to {} needs the extent daemon (CLADEFSD_GRPC)",
                    from.version,
                    from.cell,
                    dst.display()
                ),
                Self::Delete { dir } => {
                    let bucket = local_cell::upload::blocking(|| OpenCell::bucket_at(&dir))?;
                    OpenCell::delete(&dir, bucket.as_deref()).await?
                }
                Self::Drop { dir } => OpenCell::delete(&dir, None).await?,
                Self::Resize { dir, capacity } => {
                    let cell = OpenCell::open(&dir).await?;
                    // Opened, it says whether it is a version, whose capacity changes nowhere
                    // (TASK-160.10).
                    let resized = match cell.info() {
                        Ok(info) if capacity < info.capacity => Err(anyhow::anyhow!(
                            "shrinking a store needs its guest to agree: resize it through the \
                         extent daemon serving its VM (CLADEFSD_GRPC)"
                        )),
                        Ok(_) => cell.set_capacity(capacity).await.map_err(anyhow::Error::from),
                        Err(err) => Err(err.into()),
                    };
                    cell.close().await;
                    resized?;
                }
                Self::Public { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    let made = local_cell::upload::blocking(|| cell.make_public());
                    cell.close().await;
                    made?;
                }
                Self::PublicAttachOnly { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    let made = local_cell::upload::blocking(|| cell.make_public_attach_only());
                    cell.close().await;
                    made?;
                }
                Self::Attach { dir, base } => {
                    let based = OpenCell::open(&base).await?;
                    let ancestry = based.ancestry();
                    based.close().await;
                    let cell = OpenCell::open(&dir).await?;
                    let attached = local_cell::upload::blocking(|| match base.canonicalize() {
                        Ok(base) => cell.attach_base(&base, &ancestry).map_err(anyhow::Error::from),
                        Err(err) => Err(err.into()),
                    });
                    cell.close().await;
                    attached?;
                }
                Self::Stats { .. }
                | Self::Cells { .. }
                | Self::Counters { .. }
                | Self::SetPeers { .. }
                | Self::Migrate { .. }
                | Self::HandOver { .. }
                | Self::AbortMigration { .. }
                | Self::MemoryLimit { .. }
                | Self::MemoryWeight { .. }
                | Self::InFlight { .. }
                | Self::ShrinkMemory { .. }
                | Self::DiskLimit { .. }
                | Self::DiskWeight { .. }
                | Self::TenantDisk { .. }
                | Self::Grant { .. }
                | Self::Revoke { .. }
                | Self::ShredCell { .. }
                | Self::ForgetTenantKeys { .. }
                | Self::ShredTenant { .. }
                | Self::KmsReload
                | Self::DecommissionHost { .. }
                | Self::EphemeralHosts
                | Self::Mount(_)
                | Self::FormatBucket { .. }
                | Self::AddBucket { .. }
                | Self::Buckets { .. } => unreachable!("refused above"),
                Self::Stat { dir } => {
                    let cell = OpenCell::open(&dir).await?;
                    let stat = || -> anyhow::Result<Stat> {
                        let info = cell.info()?;
                        Ok(Stat {
                            cell: cell.cell(),
                            capacity: info.capacity,
                            used: info.used,
                            available: info.available,
                            holding: cell.holding()?,
                            public: cell.is_public(),
                            open: None,
                        })
                    };
                    let stat = stat();
                    cell.close().await;
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
            let given = args.next().with_context(|| format!("{name} takes a value"))?.to_string();
            let value = Some(given);
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
            let bucket_cell::identity::Details {
                provider,
                region,
                url,
                endpoint,
                kms,
                master_key_id,
            } = &entry.details;
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

    /// What `stat` says of a cell, which it prints one `name value` a line.
    struct Stat {
        cell: cladefs::CellId,
        capacity: u64,
        used: u64,
        available: u64,
        holding: cladefs::CellHolding,
        public: bool,
        /// Whether the cell is open, if known.
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
    fn stats_json(
        stats: &cladefs::cladefsd::control::proto::GetStatsResponse,
    ) -> serde_json::Value {
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
                // cladefs: in base 32, as `decommission-host` takes it (board 8521).
                "incarnation": (stats.incarnation != 0)
                    .then(|| base32_id::Short(stats.incarnation).to_string()),
            },
            "vms": vms.collect::<Vec<_>>(),
        })
    }

    /// cladefs: prints `value`, as one JSON object if `json`, otherwise as its lines ([`flattened`]):
    /// as `stats` and the API's other answers print (board 8585).
    #[cfg(target_os = "linux")]
    fn print_value(value: &serde_json::Value, json: bool) -> anyhow::Result<()> {
        match json {
            true => println!("{}", serde_json::to_string_pretty(value)?),
            false => print!("{}", flattened(value)),
        }
        Ok(())
    }

    /// cladefs: what `StatCell` says of a cell, `stat`, every field by its name, as `cells` prints it
    /// (board 8585): its tenant and cell in base 32, its fsync level by name, `forced_after` null
    /// without one.
    #[cfg(target_os = "linux")]
    fn stat_json(stat: &cladefs::cladefsd::control::proto::StatCellResponse) -> serde_json::Value {
        let cladefs::cladefsd::control::proto::StatCellResponse {
            tenant,
            cell,
            capacity,
            used,
            available,
            public,
            open,
            own,
            inherited,
            own_extents,
            inherited_extents,
            committed,
            uploading,
            taken_token,
            taken_epoch,
            taken_unix_ms,
            uploaded,
            pending_upload_bytes,
            oldest_pending_ms,
            replicas,
            fsync: _,
            configuration,
            degraded,
            members_up,
            active_replicas,
            forced_after,
            fast_wal,
            fast_wal_bucket,
            fast_wal_degraded,
            ephemeral,
        } = stat;
        serde_json::json!({
            "tenant": base32_id::Short(*tenant).to_string(),
            "cell": base32_id::Short(*cell).to_string(),
            "capacity": capacity,
            "used": used,
            "available": available,
            "public": public,
            "open": open,
            "own": own,
            "inherited": inherited,
            "own_extents": own_extents,
            "inherited_extents": inherited_extents,
            "committed": committed,
            "uploading": uploading,
            "taken_token": taken_token,
            "taken_epoch": taken_epoch,
            "taken_unix_ms": taken_unix_ms,
            "uploaded": uploaded,
            "pending_upload_bytes": pending_upload_bytes,
            "oldest_pending_ms": oldest_pending_ms,
            "replicas": replicas,
            "fsync": stat.fsync().as_str_name(),
            "configuration": configuration,
            "degraded": degraded,
            "members_up": members_up,
            "active_replicas": active_replicas,
            "forced_after": forced_after,
            "fast_wal": fast_wal,
            "fast_wal_bucket": fast_wal_bucket,
            "fast_wal_degraded": fast_wal_degraded,
            "ephemeral": ephemeral,
        })
    }

    /// cladefs: what `GetCounters` answers, `counters`, every field by its name, its bucket's under
    /// `object_storage`, as `counters` prints it (board 8585).
    #[cfg(target_os = "linux")]
    fn counters_json(
        counters: &cladefs::cladefsd::control::proto::GetCountersResponse,
    ) -> serde_json::Value {
        let cladefs::cladefsd::control::proto::GetCountersResponse {
            maps,
            map_ns,
            faults,
            fault_ns,
            page_ins,
            page_in_bytes,
            fetched_bytes,
            reads,
            read_bytes,
            read_stored_bytes,
            prefetches,
            unpacks,
            object_storage,
            commits,
            commit_ns,
            commit_store_ns,
            commit_wait_ns,
            segment_closes,
            segment_close_ns,
        } = counters;
        fn ordered(
            map: &std::collections::HashMap<String, u64>,
        ) -> std::collections::BTreeMap<&str, u64> {
            map.iter().map(|(name, value)| (name.as_str(), *value)).collect()
        }
        let object_storage = object_storage.as_ref().map(|storage| {
            serde_json::json!({
                "requests": ordered(&storage.requests),
                "uploaded_bytes": ordered(&storage.uploaded_bytes),
                "downloaded_bytes": storage.downloaded_bytes,
                "failures": ordered(&storage.failures),
            })
        });
        serde_json::json!({
            "maps": maps,
            "map_ns": map_ns,
            "faults": faults,
            "fault_ns": fault_ns,
            "page_ins": page_ins,
            "page_in_bytes": page_in_bytes,
            "fetched_bytes": fetched_bytes,
            "reads": reads,
            "read_bytes": read_bytes,
            "read_stored_bytes": read_stored_bytes,
            "prefetches": prefetches,
            "unpacks": unpacks,
            "object_storage": object_storage,
            "commits": commits,
            "commit_ns": commit_ns,
            "commit_store_ns": commit_store_ns,
            "commit_wait_ns": commit_wait_ns,
            "segment_closes": segment_closes,
            "segment_close_ns": segment_close_ns,
        })
    }

    /// cladefs: `value`, as lines of a path through it, its keys and indices joined by dots, and a
    /// value there that holds no other: what `stats` prints without `--json`.
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

    pub(super) fn main() -> anyhow::Result<()> {
        // cladefs: its threads' stacks on SIGQUIT, as an import that hung needed (TASK-98, TASK-44.13).
        #[cfg(target_os = "linux")]
        cladefs::stacks::on_sigquit()?;
        // cladefs: warnings, as of reading what the collector marked (TASK-264).
        #[cfg(target_os = "linux")]
        {
            use cladefs::cladefsd::logging;
            let warnings =
                logging::Settings { level: log::LevelFilter::Warn, ..Default::default() };
            logging::install(warnings)?;
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        // cladefs: the commit it was built of (TASK-111.7).
        if args == ["--version"] {
            println!("cladefsctl {}", cladefs::BUILD_COMMIT);
            return Ok(());
        }
        // cladefs: its calls' deadline, if any: `--timeout`, or CLADEFSCTL_TIMEOUT (board 8585).
        let (timeout, args) = match &args[..] {
            [flag, given, rest @ ..] if flag == "--timeout" => {
                (Some(seconds("--timeout", given)?), rest)
            }
            args => match std::env::var("CLADEFSCTL_TIMEOUT") {
                Ok(given) if !given.is_empty() => {
                    (Some(seconds("CLADEFSCTL_TIMEOUT", &given)?), args)
                }
                // Unset, or empty: none. Not UTF-8: refused.
                Err(std::env::VarError::NotUnicode(given)) => {
                    bail!("CLADEFSCTL_TIMEOUT must be a number of seconds: {given:?}")
                }
                _ => (None, args),
            },
        };
        let command = Command::parse(args)?;
        match std::env::var_os("CLADEFSD_GRPC").filter(|socket| !socket.is_empty()) {
            Some(_) if command.is_on_buckets() => command.on_buckets(),
            #[cfg(target_os = "linux")]
            Some(socket) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(command.through(socket.as_ref(), timeout)),
            #[cfg(not(target_os = "linux"))]
            Some(socket) => command.through(socket.as_ref(), timeout),
            None => command.locally(),
        }
    }
}
