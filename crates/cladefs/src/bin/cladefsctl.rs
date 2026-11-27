// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's host tool for VMs' disks, the cells cladefsd serves: makes them, of cells kept
//! in object storage (backlog doc-7) or of container images, places, forks, deletes and resizes
//! them, makes them public bases, and waits for their uploads:
//!
//! ```sh
//! # Through the daemon (`CLADEFSD_GRPC`, or `--config PATH` naming it): cells by TENANT/CELL.
//! cladefsctl create [--tenant TENANT] CAPACITY
//! cladefsctl take CAPACITY TENANT/CELL [TOKEN]
//! cladefsctl place TENANT/CELL TOKEN
//! cladefsctl sync TENANT/CELL
//! cladefsctl import [--tenant TENANT] CAPACITY IMAGE [REF]
//! cladefsctl import-memory [--version] [--tenant TENANT] FILE
//! cladefsctl fork [--tenant TENANT] TENANT/CELL
//! cladefsctl fork-version [--ephemeral] [--tenant TENANT] TENANT/CELL VERSION
//! cladefsctl version TENANT/CELL
//! cladefsctl versions TENANT/CELL
//! cladefsctl drop-versions TENANT/CELL VERSION...
//! cladefsctl share TENANT/CELL VERSION
//! cladefsctl unshare TENANT/CELL VERSION
//! cladefsctl delete TENANT/CELL
//! cladefsctl drop TENANT/CELL
//! cladefsctl resize TENANT/CELL CAPACITY
//! cladefsctl public TENANT/CELL
//! cladefsctl public-attach-only TENANT/CELL
//! cladefsctl attach TENANT/CELL BASE_TENANT/BASE_CELL
//! cladefsctl stat TENANT/CELL
//! cladefsctl stats [--json] [--wait SECONDS] [--leave-out histograms|guests|vms]...
//! cladefsctl cells [--json]
//! cladefsctl counters [--json]
//! cladefsctl disks [--json]
//! cladefsctl set-disk ID [--cap BYTES|none] [--cache-cap BYTES|none] [--headroom SPEC|none]
//!     [--dirty-budget BYTES|none] [--placement read_write|drain|read_only]
//! cladefsctl set-peers [--bucket URL] [ADDRESS...]
//! cladefsctl migrate-ephemeral TENANT/CELL PEER
//! cladefsctl hand-over-ephemeral TENANT/CELL
//! cladefsctl wait-ephemeral-hand-over [--migration ID] TENANT/CELL
//! cladefsctl abort-ephemeral-migration TENANT/CELL
//! cladefsctl memory-limit [--tenant TENANT] BYTES|none
//! cladefsctl memory-weight TENANT WEIGHT
//! cladefsctl in-flight [--tenant TENANT] BYTES|default
//! cladefsctl shrink-memory [--tenant TENANT] TARGET
//! cladefsctl disk-limit [--tenant TENANT] BYTES|none
//! cladefsctl disk-weight TENANT WEIGHT
//! cladefsctl tenant-disk [--tenant TENANT] [--json]
//! cladefsctl grant TENANT [UID [GID]]
//! cladefsctl revoke TENANT
//! cladefsctl shred-cell [--bucket URL] TENANT/CELL
//! cladefsctl forget-tenant-keys TENANT
//! cladefsctl shred-tenant [--bucket URL] TENANT
//! cladefsctl kms reload
//! cladefsctl decommission-host [--none-ok] INCARNATION
//! cladefsctl ephemeral-hosts
//! cladefsctl mount attach TENANT CELL [TOKEN]
//! cladefsctl mount create TENANT CAPACITY [TOKEN]
//! cladefsctl mount detach [--force] TENANT CELL
//! cladefsctl mount list
//!
//! # Without the daemon: bucket administration.
//! cladefsctl shred-cell TENANT/CELL       # with CLADEFSD_BUCKET, as with --bucket
//! cladefsctl shred-tenant TENANT          # with CLADEFSD_BUCKET, as with --bucket
//!
//! # With or without it: on buckets themselves.
//! cladefsctl set-peers --bucket URL [ADDRESS...]
//! cladefsctl shred-cell --bucket URL TENANT/CELL
//! cladefsctl shred-tenant --bucket URL TENANT
//! cladefsctl format-bucket URL [DETAILS]
//! cladefsctl add-bucket ROOT URL [DETAILS]
//! cladefsctl buckets URL
//! cladefsctl formats URL
//! cladefsctl ratchet URL [--storage N] [--fxfs N]
//!
//! cladefsctl --version
//! ```
//!
//! Any command may be preceded by `--config PATH` (`-c PATH`, or `CLADEFS_CONFIG` set to PATH), a
//! TOML file whose settings apply as the environment's would, its daemon's control socket among
//! them, and by `--timeout SECONDS` (or `CLADEFSCTL_TIMEOUT` set to SECONDS),
//! which bounds connecting to the daemon and its call as a deadline (board 8585): one that takes
//! longer fails with `ETIMEDOUT`, the daemon going on with it, which may still complete it, as it
//! completes every call whether or not its caller waits. No command has one unless given, `sync`,
//! `import` and `fork` taking minutes, others as long as their cells or VMs take. It bounds the
//! control API's calls alone: not `mount`'s, nor what cladefsctl does without the daemon.
//!
//! With `CLADEFSD_GRPC` naming the control socket of the extent daemon (TASK-43.5), commands go
//! through its control API (`cladefs::cladefsd::control`) and act on cells as the daemon serves
//! them: on a cell a VM has open, live, or not at all (`EBUSY`). Without `CLADEFSD_GRPC` (board
//! 11705, hotbed question 542), only bucket administration runs (`format-bucket`, `add-bucket`,
//! `buckets`, and keyless `shred-tenant`/`shred-cell`/`set-peers`). Every command on a cell
//! (`create`, `take`, `import`, `import-memory`, `stat`, `fork`, ...) requires `CLADEFSD_GRPC`: a
//! host's one daemon owns its cells, which nothing else opens (the user, 2026-11-26, board 14305).
//!
//! Below, "the cell at DIR" is the cell a command names, TENANT/CELL, the daemon placing a new
//! cell's directory itself, of the default tenant's unless `--tenant` names another.
//!
//! `create` makes an empty cell at DIR, of CAPACITY bytes, of a new cell, a new cell's of the
//! default tenant's (doc-20), its ID its bucket's to give (TASK-364), and prints the cell. `take`
//! makes an empty cell at DIR, of CAPACITY bytes, of CELL (`TENANT/CELL`, each in base 32, its
//! cell's, TASK-364), which its bucket has (`ENOENT` otherwise): once cladefsd opens it, with
//! `CLADEFSD_BUCKET` naming the cell's bucket, it takes what the cell holds there, whose data it
//! fetches as it is read; placed at TOKEN, if given, as `place` places it. A version, a VM
//! snapshot's or another, it reads, taking no head, as any number of hosts do at once. `place`
//! places the cell at DIR at the placement whose fencing token, its boot ID, is TOKEN (TASK-39.7):
//! its writer takes its cell in object storage with that token from then on, which a later
//! placement's exceeds, so that a placement cut off takes no cell back from the one after it
//! (`ESTALE`), and its commits fail once a later one takes it. `sync` returns once every commit of
//! the cell at DIR is durable and in object
//! storage, as before a clean stop: through the daemon, a cell a VM has open as of then. `import`
//! makes a cell at DIR, of CAPACITY bytes, holding an Fxfs filesystem of the container image IMAGE
//! (TASK-39.5), a base for VMs to boot forks of: IMAGE is an OCI image layout, as `skopeo copy`
//! makes, or `docker save` (whose tarball is one, once extracted), holding the image REF names, or
//! its only image; or a tar archive of one layer, gzipped or not, as `docker export` makes. Files
//! keep their owners, permissions, times and extended attributes; see `cladefs::import`. `fork`
//! copies the cell it names (SRC without the daemon) to a new cell (at DST without it; of the
//! tenant `--tenant` names through it, the cell's own otherwise), which it prints: through the
//! daemon, live, if a VM has the cell open, once nothing is open on it, as with the guest's
//! filesystem frozen (fsfreeze), and refused otherwise (TASK-39.3). The new cell is a fork of the
//! cell's in its bucket, made once its commits are uploaded, holding its extents without uploading
//! them again; the fork is a cell of its own, which reads them there (TASK-165.7). `version` makes
//! a version of the cell of the cell at DIR, once nothing is open on it: one commit here, uploaded
//! behind, which it prints the ID of, in hexadecimal. `fork-version`, through the daemon alone,
//! forks the version VERSION of the cell TENANT/CELL to a new cell, which it prints, of the tenant
//! `--tenant` names, the cell's own otherwise, ephemeral with `--ephemeral`, on any host, as many
//! hosts at once as wanted, the version uploaded first. `versions` lists
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
//! of it share (TASK-43); `public-attach-only` makes its branch alone public, not those it
//! inherited, a base to attach to cells, not to fork (`MakePublic`'s `attach_only`). `attach` attaches the cell BASE_TENANT/BASE_CELL (BASE_DIR without the daemon)
//! to the cell at DIR, its extents the cell doesn't hold read through to it, read-only
//! (`AttachBase`). `stat` prints what the cell at DIR is and holds, one `name value` a line:
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
//! with its bucket's under `object_storage`. `disks` lists the host's managed disks and their
//! governance (`ListDisks`); `set-disk` changes disk ID's, at least one flag given
//! (`SetDiskGovernance`). `set-peers` has the hosts sharing the daemon's bucket be those at the
//! ADDRESSes, each `HOST:PORT`, none without any (`SetPeers`); with `--bucket URL`, with or without
//! the daemon, it publishes them to the bucket at URL instead, for every daemon on it to take, and
//! prints `generation N`, the peer list's new generation. `migrate-ephemeral` begins the live
//! migration of the ephemeral cell TENANT/CELL to the daemon whose peer address is PEER, and prints
//! `migration ID` (`MigrateEphemeralCell`); `hand-over-ephemeral` hands the ephemeral cell over to
//! its migration's target (`HandOverEphemeralCell`); `wait-ephemeral-hand-over` waits on the target
//! daemon until an incoming ephemeral hand-over of TENANT/CELL is accepted or fails
//! (`WaitEphemeralHandOver`); `abort-ephemeral-migration` ends its migration, the cell then free to
//! migrate again (`AbortEphemeralMigration`). Bucket-backed cells move
//! through the bucket instead (`sync`, `take`/`place`, `drop`). `memory-limit` limits the memory the daemon holds extents' pages in
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
//! bucket, in base 32 and not 0, the default tenant's (TASK-364), a VM socket of its own,
//! `TENANT.sock`, its VM processes naming cells in the tenant's directory alone, of its own cells,
//! the socket given to the user UID and the group GID if named, the daemon's own otherwise, and
//! prints its path; granting a tenant again prints it again. `revoke` takes the tenant TENANT's socket away (base 32): its VM processes
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
//! value` a line. With `--bucket URL`, or without the daemon with `CLADEFSD_BUCKET` naming the
//! bucket, `shred-cell` and `shred-tenant` shred in the bucket at URL themselves, no daemon
//! forgetting, and print `keks_removed N` and `shredded_now 1` or `0`.
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

    /// Parses a `TENANT/CELL` identifier in Crockford base 32.
    fn parse_cell(given: &str) -> anyhow::Result<cladefs::CellId> {
        given.parse().map_err(anyhow::Error::msg).context("CELL must be TENANT/CELL in base 32")
    }

    /// Parses a `ByteSize` value or `"none"` (to clear a cap/budget).
    fn byte_size_or_none(what: &str, given: &str) -> anyhow::Result<Option<u64>> {
        if given == "none" {
            return Ok(None);
        }
        given
            .parse::<cladefs::config::ByteSize>()
            .map(|b| Some(b.bytes()))
            .map_err(|err| anyhow::anyhow!("{what}: {err}"))
    }

    /// Parses a headroom spec (`"10%"`, `"8GiB"`, `"1073741824"`, or `"none"`).
    #[derive(Clone, Copy)]
    enum HeadroomArg {
        Clear,
        Bytes(u64),
        Percent(u32),
    }

    fn parse_headroom_arg(given: &str) -> anyhow::Result<HeadroomArg> {
        if given == "none" {
            return Ok(HeadroomArg::Clear);
        }
        if let Some(pct) = given.strip_suffix('%') {
            let pct: u32 = pct
                .trim()
                .parse()
                .with_context(|| format!("--headroom percentage invalid: {given:?}"))?;
            if pct >= 100 {
                bail!("--headroom percentage must be 0..100, got {pct}%");
            }
            return Ok(HeadroomArg::Percent(pct));
        }
        let bytes: cladefs::config::ByteSize =
            given.parse().map_err(|err| anyhow::anyhow!("--headroom: {err}"))?;
        Ok(HeadroomArg::Bytes(bytes.bytes()))
    }

    /// Parses a `set-disk` subcommand's flags (`--cap`, `--cache-cap`, `--headroom`,
    /// `--dirty-budget`, `--placement`).
    fn parse_set_disk(id: &str, args: &[&str]) -> anyhow::Result<Command> {
        let mut cap: Option<Option<u64>> = None;
        let mut cache_cap: Option<Option<u64>> = None;
        let mut headroom: Option<HeadroomArg> = None;
        let mut dirty_budget: Option<Option<u64>> = None;
        let mut placement: Option<String> = None;
        let mut rest = args;
        while !rest.is_empty() {
            rest = match *rest {
                ["--cap", val, ref tail @ ..] => {
                    cap = Some(byte_size_or_none("--cap", val)?);
                    tail
                }
                ["--cache-cap", val, ref tail @ ..] => {
                    cache_cap = Some(byte_size_or_none("--cache-cap", val)?);
                    tail
                }
                ["--headroom", val, ref tail @ ..] => {
                    headroom = Some(parse_headroom_arg(val)?);
                    tail
                }
                ["--dirty-budget", val, ref tail @ ..] => {
                    dirty_budget = Some(byte_size_or_none("--dirty-budget", val)?);
                    tail
                }
                ["--placement", val, ref tail @ ..] => {
                    if !matches!(val, "read_write" | "drain" | "read_only") {
                        bail!("--placement must be read_write, drain, or read_only (got {val:?})");
                    }
                    placement = Some(val.to_owned());
                    tail
                }
                _ => bail!(usage()),
            };
        }
        Ok(Command::SetDisk {
            id: id.to_owned(),
            cap,
            cache_cap,
            headroom,
            dirty_budget,
            placement,
        })
    }

    fn usage() -> anyhow::Error {
        anyhow::anyhow!(
            "usage (daemon via --config PATH / CLADEFSD_GRPC): \
         cladefsctl create [--tenant TENANT] CAPACITY | \
         cladefsctl take CAPACITY TENANT/CELL [TOKEN] | \
         cladefsctl place TENANT/CELL TOKEN | \
         cladefsctl sync TENANT/CELL | \
         cladefsctl import [--tenant TENANT] CAPACITY IMAGE [REF] | \
         cladefsctl import-memory [--version] [--tenant TENANT] FILE | \
         cladefsctl fork [--tenant TENANT] TENANT/CELL | \
         cladefsctl fork-version [--ephemeral] [--tenant TENANT] TENANT/CELL VERSION | \
         cladefsctl version TENANT/CELL | cladefsctl versions TENANT/CELL | \
         cladefsctl drop-versions TENANT/CELL VERSION... | \
         cladefsctl share TENANT/CELL VERSION | cladefsctl unshare TENANT/CELL VERSION | \
         cladefsctl delete TENANT/CELL | cladefsctl drop TENANT/CELL | \
         cladefsctl resize TENANT/CELL CAPACITY | \
         cladefsctl public TENANT/CELL | cladefsctl public-attach-only TENANT/CELL | \
         cladefsctl attach TENANT/CELL BASE_TENANT/BASE_CELL | \
         cladefsctl stat TENANT/CELL | \
         cladefsctl stats [--json] [--wait SECONDS] [--leave-out histograms|guests|vms]... | \
         cladefsctl cells [--json] | cladefsctl counters [--json] | \
         cladefsctl disks [--json] | \
         cladefsctl set-disk ID [--cap BYTES|none] [--cache-cap BYTES|none] \
         [--headroom SPEC|none] [--dirty-budget BYTES|none] \
         [--placement read_write|drain|read_only] | \
         cladefsctl set-peers [--bucket URL] [ADDRESS...] | \
         cladefsctl migrate-ephemeral TENANT/CELL PEER | \
         cladefsctl hand-over-ephemeral TENANT/CELL | \
         cladefsctl wait-ephemeral-hand-over [--migration ID] TENANT/CELL | \
         cladefsctl abort-ephemeral-migration TENANT/CELL | \
         cladefsctl memory-limit [--tenant TENANT] BYTES|none | \
         cladefsctl memory-weight TENANT WEIGHT | \
         cladefsctl in-flight [--tenant TENANT] BYTES|default | \
         cladefsctl shrink-memory [--tenant TENANT] TARGET | \
         cladefsctl disk-limit [--tenant TENANT] BYTES|none | cladefsctl disk-weight TENANT WEIGHT | \
         cladefsctl tenant-disk [--tenant TENANT] [--json] | \
         cladefsctl grant TENANT [UID [GID]] | \
         cladefsctl revoke TENANT | cladefsctl shred-cell [--bucket URL] TENANT/CELL | \
         cladefsctl forget-tenant-keys TENANT | cladefsctl shred-tenant [--bucket URL] TENANT | \
         cladefsctl kms reload | \
         cladefsctl decommission-host [--none-ok] INCARNATION | cladefsctl ephemeral-hosts | \
         cladefsctl mount attach TENANT CELL [TOKEN] | \
         cladefsctl mount create TENANT CAPACITY [TOKEN] | \
         cladefsctl mount detach [--force] TENANT CELL | cladefsctl mount list | \
         cladefsctl format-bucket URL [DETAILS] | cladefsctl add-bucket ROOT URL [DETAILS] | \
         cladefsctl buckets URL | cladefsctl formats URL | \
         cladefsctl ratchet URL [--storage N] [--fxfs N] | cladefsctl --version; \
         without CLADEFSD_GRPC, bucket administration alone; \
         global flags: [--config PATH] [--timeout SECONDS]"
        )
    }

    /// cladefs: what a daemon forgot of a tenant's or a cell's keys, one `name value` a line
    /// (TASK-411.7).
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
    /// alone.
    enum Command {
        /// cladefs: a new cell, its ID its bucket's to give (TASK-364, hotbed question 547).
        Create {
            tenant: u64,
            capacity: u64,
        },
        Take {
            capacity: u64,
            cell: cladefs::CellId,
            token: Option<u64>,
        },
        Place {
            target: cladefs::CellId,
            token: u64,
        },
        Sync {
            target: cladefs::CellId,
        },
        Import {
            tenant: u64,
            capacity: u64,
            image: PathBuf,
            reference: Option<String>,
        },
        /// cladefs: a new memory cell holding a snapshot memory file (board 11222).
        ImportMemory {
            tenant: u64,
            file: PathBuf,
            make_version: bool,
        },
        Fork {
            src: cladefs::CellId,
            target_tenant: Option<u64>,
        },
        /// cladefs: a version made (doc-20).
        Version {
            target: cladefs::CellId,
        },
        Versions {
            target: cladefs::CellId,
        },
        DropVersions {
            target: cladefs::CellId,
            versions: Vec<u64>,
        },
        /// cladefs: a version shared, or no longer (TASK-395.7).
        Share {
            target: cladefs::CellId,
            version: u64,
            shared: bool,
        },
        /// cladefs: a version forked, on any host.
        ForkVersion {
            from: cladefs::CellVersion,
            ephemeral: bool,
            target_tenant: Option<u64>,
        },
        Delete {
            target: cladefs::CellId,
        },
        Drop {
            target: cladefs::CellId,
        },
        Resize {
            target: cladefs::CellId,
            capacity: u64,
        },
        Public {
            target: cladefs::CellId,
        },
        PublicAttachOnly {
            target: cladefs::CellId,
        },
        Attach {
            target: cladefs::CellId,
            base: cladefs::CellId,
        },
        Stat {
            target: cladefs::CellId,
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
        /// cladefs: all managed host disks and their live governance state (hotbed question 547).
        Disks {
            json: bool,
        },
        /// cladefs: live runtime governance update for one managed disk (hotbed question 547).
        SetDisk {
            id: String,
            cap: Option<Option<u64>>,
            cache_cap: Option<Option<u64>>,
            headroom: Option<HeadroomArg>,
            dirty_budget: Option<Option<u64>>,
            placement: Option<String>,
        },
        /// cladefs: the hosts sharing the daemon's bucket (board 8585), or published cluster-wide
        /// to `events` when `bucket` is given.
        SetPeers {
            bucket: Option<String>,
            addresses: Vec<String>,
        },
        /// cladefs: an ephemeral cell's live migration begun, to `peer` (board 8585, hotbed
        /// question 547).
        Migrate {
            cell: cladefs::CellId,
            peer: String,
        },
        /// cladefs: an ephemeral cell migrating handed over to its target (board 8585).
        HandOver {
            cell: cladefs::CellId,
        },
        /// cladefs: waits on the target daemon until an incoming hand-over of `cell` is accepted
        /// or fails (`WaitHandOver`).
        WaitHandOver {
            cell: cladefs::CellId,
            migration: u64,
        },
        /// cladefs: a cell's migration ended (board 8577).
        AbortMigration {
            cell: cladefs::CellId,
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
        /// own across all managed JBOD disks, given to `uid` and `gid` if named (hotbed question
        /// 547).
        Grant {
            tenant: u64,
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
        /// cladefs: the storage-version ratchet gates in a bucket's root (doc-28).
        Formats {
            url: String,
        },
        /// cladefs: monotonically advance a bucket's storage-version ratchet (doc-28).
        Ratchet {
            url: String,
            storage: Option<u32>,
            fxfs: Option<u32>,
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

    /// cladefs: the daemon's FUSE mount of cells is Linux's (`cladefsd::fuse_control`).
    #[cfg(not(target_os = "linux"))]
    impl Mount {
        async fn through(
            self,
            _socket: &Path,
            _timeout: Option<std::time::Duration>,
        ) -> anyhow::Result<()> {
            bail!("the daemon's FUSE mount of cells is Linux's")
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
            use cladefs::fuse::control::proto::{AttachRequest, DetachRequest, ListRequest};
            let channel =
                connect_within(timeout, socket, cladefs::cladefsd::control::channel(socket))
                    .await?;
            // Taking answers as large as the daemon sends (board 10297).
            let mut mount = cladefs::fuse::control::client(channel);
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

    async fn connect_within<T>(
        timeout: Option<std::time::Duration>,
        socket: &Path,
        call: impl std::future::Future<Output = Result<T, tonic::transport::Error>>,
    ) -> anyhow::Result<T> {
        let what =
            format!("connecting to the extent daemon's control socket at {}", socket.display());
        within(timeout, &what, call).await?.context(what)
    }

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

    fn name(path: &Path) -> Result<String, cladefs::Error> {
        path.to_str().map(str::to_owned).ok_or_else(|| cladefs::Errno::EINVAL.into())
    }

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
            let target = parse_cell;
            let words: Vec<&str> = args.iter().map(String::as_str).collect();
            Ok(match words[..] {
                ["take", size, cell, ref rest @ ..] if rest.len() <= 1 => Self::Take {
                    capacity: capacity(size)?,
                    cell: parse_cell(cell)?,
                    token: rest.first().map(|rest| token(rest)).transpose()?,
                },
                ["create", "--tenant", t, size] => {
                    Self::Create { tenant: tenant(t)?, capacity: capacity(size)? }
                }
                ["create", size] => Self::Create {
                    tenant: bucket_cell::layout::DEFAULT_TENANT,
                    capacity: capacity(size)?,
                },
                ["place", cell, given] => {
                    Self::Place { target: target(cell)?, token: token(given)? }
                }
                ["sync", cell] => Self::Sync { target: target(cell)? },
                ["import", "--tenant", t, size, image, ref rest @ ..] if rest.len() <= 1 => {
                    Self::Import {
                        tenant: tenant(t)?,
                        capacity: capacity(size)?,
                        image: image.into(),
                        reference: rest.first().map(|reference| reference.to_string()),
                    }
                }
                ["import", size, image, ref rest @ ..] if rest.len() <= 1 => Self::Import {
                    tenant: bucket_cell::layout::DEFAULT_TENANT,
                    capacity: capacity(size)?,
                    image: image.into(),
                    reference: rest.first().map(|reference| reference.to_string()),
                },
                ["import-memory", ref rest @ ..] => {
                    let mut make_version = false;
                    let mut t = bucket_cell::layout::DEFAULT_TENANT;
                    let mut r = rest;
                    loop {
                        r = match *r {
                            ["--version", ref tail @ ..] => {
                                make_version = true;
                                tail
                            }
                            ["--tenant", given, ref tail @ ..] => {
                                t = tenant(given)?;
                                tail
                            }
                            _ => break,
                        };
                    }
                    let [file] = *r else { bail!(usage()) };
                    Self::ImportMemory { tenant: t, file: file.into(), make_version }
                }
                ["fork", "--tenant", t, src] => {
                    Self::Fork { src: parse_cell(src)?, target_tenant: Some(tenant(t)?) }
                }
                ["fork", src] => Self::Fork { src: parse_cell(src)?, target_tenant: None },
                ["version", cell] => Self::Version { target: target(cell)? },
                ["versions", cell] => Self::Versions { target: target(cell)? },
                ["drop-versions", cell, ref ids @ ..] if !ids.is_empty() => Self::DropVersions {
                    target: target(cell)?,
                    versions: ids.iter().map(|id| version(id)).collect::<anyhow::Result<_>>()?,
                },
                ["share", cell, id] => {
                    Self::Share { target: target(cell)?, version: version(id)?, shared: true }
                }
                ["unshare", cell, id] => {
                    Self::Share { target: target(cell)?, version: version(id)?, shared: false }
                }
                ["fork-version", ref rest @ ..] => {
                    let mut ephemeral = false;
                    let mut target_tenant = None;
                    let mut r = rest;
                    loop {
                        r = match *r {
                            ["--ephemeral", ref tail @ ..] => {
                                ephemeral = true;
                                tail
                            }
                            ["--tenant", given, ref tail @ ..] => {
                                target_tenant = Some(tenant(given)?);
                                tail
                            }
                            _ => break,
                        };
                    }
                    let [cell, id] = *r else { bail!(usage()) };
                    let cell = parse_cell(cell)?;
                    let from = cladefs::CellVersion {
                        tenant: cell.tenant,
                        cell: cell.id,
                        version: version(id)?,
                    };
                    Self::ForkVersion { from, ephemeral, target_tenant }
                }
                ["delete", cell] => Self::Delete { target: target(cell)? },
                ["drop", cell] => Self::Drop { target: target(cell)? },
                ["resize", cell, size] => {
                    Self::Resize { target: target(cell)?, capacity: capacity(size)? }
                }
                ["public", cell] => Self::Public { target: target(cell)? },
                ["public-attach-only", cell] => Self::PublicAttachOnly { target: target(cell)? },
                ["attach", cell, base] => {
                    Self::Attach { target: target(cell)?, base: target(base)? }
                }
                ["stat", cell] => Self::Stat { target: target(cell)? },
                ["stats", ref rest @ ..] => stats(rest)?,
                ["cells"] => Self::Cells { json: false },
                ["cells", "--json"] => Self::Cells { json: true },
                ["counters"] => Self::Counters { json: false },
                ["counters", "--json"] => Self::Counters { json: true },
                ["disks"] => Self::Disks { json: false },
                ["disks", "--json"] => Self::Disks { json: true },
                ["set-disk", disk_id, ref rest @ ..] if !rest.is_empty() => {
                    parse_set_disk(disk_id, rest)?
                }
                ["set-peers", "--bucket", url, ref addresses @ ..] => Self::SetPeers {
                    bucket: Some((*url).to_owned()),
                    addresses: addresses.iter().map(|&address| address.into()).collect(),
                },
                ["set-peers", ref addresses @ ..] => Self::SetPeers {
                    bucket: None,
                    addresses: addresses.iter().map(|&address| address.into()).collect(),
                },
                ["migrate-ephemeral", cell, peer] => {
                    Self::Migrate { cell: parse_cell(cell)?, peer: peer.to_owned() }
                }
                ["hand-over-ephemeral", cell] => Self::HandOver { cell: parse_cell(cell)? },
                ["wait-ephemeral-hand-over", cell] => {
                    Self::WaitHandOver { cell: parse_cell(cell)?, migration: 0 }
                }
                ["wait-ephemeral-hand-over", "--migration", migration, cell] => {
                    Self::WaitHandOver {
                        cell: parse_cell(cell)?,
                        migration: number("MIGRATION", migration)?,
                    }
                }
                ["abort-ephemeral-migration", cell] => {
                    Self::AbortMigration { cell: parse_cell(cell)? }
                }
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
                ["grant", given, ref ids @ ..] if ids.len() <= 2 => Self::Grant {
                    tenant: tenant(given)?,
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
                ["formats", url] => Self::Formats { url: url.to_owned() },
                ["ratchet", url, ref rest @ ..] => {
                    let (storage, fxfs) = parse_ratchet(rest)?;
                    Self::Ratchet { url: url.to_owned(), storage, fxfs }
                }
                _ => bail!(usage()),
            })
        }

        /// Does it through the control API of the daemon whose control socket is at `socket`, within
        /// `timeout` if given (board 8585).
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
                Self::Create { tenant, capacity, .. } => {
                    let request = CreateCellRequest {
                        capacity,
                        tenant,
                        kind: CellKind::Fxfs.into(),
                        ..Default::default()
                    };
                    let made = call!("making a cell", create_cell, request);
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Take { capacity, cell, token, .. } => {
                    let request = CreateCellRequest {
                        capacity,
                        tenant: cell.tenant,
                        cell: cell.id,
                        token,
                        kind: CellKind::Fxfs.into(),
                        ..Default::default()
                    };
                    call!(format!("taking cell {cell}"), create_cell, request);
                }
                Self::Place { target, token } => {
                    let cell = target;
                    let request = PlaceCellRequest { tenant: cell.tenant, cell: cell.id, token };
                    call!(format!("placing cell {cell}"), place_cell, request);
                }
                Self::Sync { target } => {
                    let cell = target;
                    let request = SyncCellRequest { tenant: cell.tenant, cell: cell.id };
                    call!(format!("waiting for cell {cell} to be uploaded"), sync_cell, request);
                }
                Self::Import { tenant, capacity, image, reference, .. } => {
                    let request = ImportImageRequest {
                        capacity,
                        image: name(&image)?,
                        reference: reference.unwrap_or_default(),
                        tenant,
                    };
                    let made =
                        call!(format!("importing {}", image.display()), import_image, request);
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::ImportMemory { tenant, file, make_version, .. } => {
                    let request = ImportMemoryRequest {
                        file: name(&file)?,
                        tenant,
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
                Self::Fork { src, target_tenant, .. } => {
                    let cell = src;
                    let request = ForkCellRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        target_tenant,
                        ..Default::default()
                    };
                    let made = call!(format!("forking cell {cell}"), fork_cell, request);
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Version { target } => {
                    let cell = target;
                    let request =
                        MakeVersionRequest { tenant: cell.tenant, cell: cell.id, keep_last: 0 };
                    let made =
                        call!(format!("making a version of cell {cell}"), make_version, request);
                    println!("{:x}", made.version);
                }
                Self::Versions { target } => {
                    let cell = target;
                    let request = ListVersionsRequest { tenant: cell.tenant, cell: cell.id };
                    let listed = call!(
                        format!("listing the versions of cell {cell}"),
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
                Self::DropVersions { target, versions } => {
                    let cell = target;
                    let request =
                        DropVersionsRequest { tenant: cell.tenant, cell: cell.id, versions };
                    call!(format!("dropping versions of cell {cell}"), drop_versions, request);
                }
                Self::Share { target, version, shared } => {
                    let cell = target;
                    let request =
                        ShareVersionRequest { tenant: cell.tenant, cell: cell.id, version, shared };
                    call!(
                        format!("sharing version {version} of cell {cell}"),
                        share_version,
                        request
                    );
                }
                Self::ForkVersion { from, ephemeral, target_tenant } => {
                    let cladefs::CellVersion { tenant, cell, version } = from;
                    let request = ForkCellRequest {
                        version,
                        tenant,
                        cell,
                        target_tenant,
                        persistent: Some(!ephemeral),
                        ..Default::default()
                    };
                    let made = call!(
                        format!("forking version {version:x} of cell {cell:x}"),
                        fork_cell,
                        request
                    );
                    println!("{}", cladefs::CellId { tenant: made.tenant, id: made.cell });
                }
                Self::Delete { target } => {
                    let cell = target;
                    let request = DeleteCellRequest { tenant: cell.tenant, cell: cell.id };
                    call!(format!("deleting cell {cell}"), delete_cell, request);
                }
                Self::Drop { target } => {
                    let cell = target;
                    let request = DropCellRequest { tenant: cell.tenant, cell: cell.id };
                    call!(format!("dropping cell {cell}"), drop_cell, request);
                }
                Self::Resize { target, capacity } => {
                    let cell = target;
                    let request =
                        ResizeCellRequest { tenant: cell.tenant, cell: cell.id, capacity };
                    call!(
                        format!("resizing cell {cell} to {capacity} bytes"),
                        resize_cell,
                        request
                    );
                }
                Self::Public { target } => {
                    let cell = target;
                    let request = MakePublicRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        attach_only: false,
                    };
                    call!(format!("making cell {cell} public"), make_public, request);
                }
                Self::PublicAttachOnly { target } => {
                    let cell = target;
                    let request =
                        MakePublicRequest { tenant: cell.tenant, cell: cell.id, attach_only: true };
                    call!(format!("making cell {cell} public to attach"), make_public, request);
                }
                Self::Attach { target, base } => {
                    let cell = target;
                    let request = AttachBaseRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        base_tenant: base.tenant,
                        base_cell: base.id,
                    };
                    call!(format!("attaching {base} to {cell}"), attach_base, request);
                }
                Self::Stat { target } => {
                    let cell = target;
                    let request = StatCellRequest { tenant: cell.tenant, cell: cell.id };
                    let stat = call!(format!("asking what cell {cell} holds"), stat_cell, request);
                    let cell = cladefs::CellId { tenant: stat.tenant, id: stat.cell };
                    let whose = |own, inherited| cladefs::Whose { own, inherited };
                    print!(
                        "{}",
                        Stat {
                            cell,
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
                    cells.sort_by_key(|c| (c.tenant, c.cell));
                    let cells = cells.iter().map(|cell| {
                        let mut listed = serde_json::json!({
                            "tenant": base32_id::Short(cell.tenant).to_string(),
                            "cell": base32_id::Short(cell.cell).to_string(),
                            "disk_id": cell.disk_id,
                        });
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
                Self::Disks { json } => {
                    let listed = call!("listing managed disks", list_disks, ListDisksRequest {});
                    let disks: Vec<_> = listed.disks.iter().map(disk_json).collect();
                    print_value(&serde_json::json!({ "disks": disks }), json)?;
                }
                Self::SetDisk { id, cap, cache_cap, headroom, dirty_budget, placement } => {
                    let placement = placement.map(|p| match p.as_str() {
                        "drain" => DiskPlacementMode::DiskPlacementDrain as i32,
                        "read_only" => DiskPlacementMode::DiskPlacementReadOnly as i32,
                        _ => DiskPlacementMode::DiskPlacementReadWrite as i32,
                    });
                    let (headroom_bytes, headroom_percent, clear_headroom) = match headroom {
                        None => (None, None, false),
                        Some(HeadroomArg::Clear) => (None, None, true),
                        Some(HeadroomArg::Bytes(b)) => (Some(b), None, false),
                        Some(HeadroomArg::Percent(p)) => (None, Some(p), false),
                    };
                    let request = SetDiskGovernanceRequest {
                        id: id.clone(),
                        cap_bytes: cap.flatten(),
                        clear_cap: matches!(cap, Some(None)),
                        cache_cap_bytes: cache_cap.flatten(),
                        clear_cache_cap: matches!(cache_cap, Some(None)),
                        headroom_bytes,
                        headroom_percent,
                        clear_headroom,
                        dirty_budget_bytes: dirty_budget.flatten(),
                        clear_dirty_budget: matches!(dirty_budget, Some(None)),
                        placement,
                    };
                    call!(
                        format!("updating governance of disk {id}"),
                        set_disk_governance,
                        request
                    );
                    let listed = call!("listing managed disks", list_disks, ListDisksRequest {});
                    if let Some(disk) = listed.disks.iter().find(|d| d.id == id) {
                        print_value(&disk_json(disk), false)?;
                    }
                }
                Self::SetPeers { addresses, bucket: None } => {
                    call!("setting the daemon's peers", set_peers, SetPeersRequest { addresses });
                }
                Self::SetPeers { bucket: Some(_), .. } => unreachable!("served above"),
                Self::Migrate { cell, peer } => {
                    let what = format!("migrating cell {cell} to {peer}");
                    let request =
                        MigrateEphemeralCellRequest { peer, tenant: cell.tenant, cell: cell.id };
                    println!(
                        "migration {}",
                        call!(what, migrate_ephemeral_cell, request).migration
                    );
                }
                Self::HandOver { cell } => {
                    let request =
                        HandOverEphemeralCellRequest { tenant: cell.tenant, cell: cell.id };
                    call!(format!("handing cell {cell} over"), hand_over_ephemeral_cell, request);
                }
                Self::WaitHandOver { cell, migration } => {
                    let request = WaitEphemeralHandOverRequest {
                        tenant: cell.tenant,
                        cell: cell.id,
                        migration,
                    };
                    let ready = call!(
                        format!("waiting for a hand-over of cell {cell}"),
                        wait_ephemeral_hand_over,
                        request
                    );
                    println!("migration {}", ready.migration);
                    println!("{}", cladefs::CellId { tenant: ready.tenant, id: ready.cell });
                }
                Self::AbortMigration { cell } => {
                    let request =
                        AbortEphemeralMigrationRequest { tenant: cell.tenant, cell: cell.id };
                    call!(
                        format!("aborting the migration of cell {cell}"),
                        abort_ephemeral_migration,
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
                Self::Grant { tenant, uid, gid } => {
                    let request = GrantRequest { tenant, uid, gid };
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
                | Self::Buckets { .. }
                | Self::Formats { .. }
                | Self::Ratchet { .. } => unreachable!("served above"),
            }
            Ok(())
        }

        /// cladefs: whether it acts on buckets themselves, with or without the daemon (TASK-633.9).
        fn is_on_buckets(&self) -> bool {
            matches!(
                self,
                Self::FormatBucket { .. }
                    | Self::AddBucket { .. }
                    | Self::Buckets { .. }
                    | Self::Formats { .. }
                    | Self::Ratchet { .. }
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
                Self::Formats { url } => {
                    let gates = cladefs::bucket::bucket_formats(&url).map_err(printable);
                    let gates = gates.with_context(|| format!("reading {url}'s format gates"))?;
                    let gates = gates.with_context(|| format!("{url} records no format gates"))?;
                    print!("{}", formats_lines(gates));
                }
                Self::Ratchet { url, storage, fxfs } => {
                    let (storage, fxfs) = match (storage, fxfs) {
                        (None, None) => (
                            Some(formats::gates::CURRENT_STORAGE),
                            Some(formats::gates::CURRENT_FXFS),
                        ),
                        explicit => explicit,
                    };
                    let (gates, changed) = cladefs::bucket::ratchet_bucket(&url, storage, fxfs)
                        .map_err(printable)
                        .with_context(|| format!("ratcheting {url}'s format gates"))?;
                    print!("{}", formats_lines(gates));
                    println!("changed {}", u8::from(changed));
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

        /// Commands when `CLADEFSD_GRPC` is unset (board 11705, hotbed question 542): bucket
        /// administration alone. cladefs: every command on a cell is the daemon's, which owns its
        /// host's cells, nothing else opening them (the user, 2026-11-26, board 14305): imports and
        /// stats too, which opened cell directories here before.
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
            let what = match self {
                Self::Stats { .. } => "stats are",
                Self::Grant { .. } | Self::Revoke { .. } => "tenants' grants are",
                Self::ShredCell { .. }
                | Self::ForgetTenantKeys { .. }
                | Self::ShredTenant { .. } => "shredding and forgetting keys are",
                Self::KmsReload => "reloading the KMS is",
                Self::DecommissionHost { .. } | Self::EphemeralHosts => {
                    "decommissioning hosts, and listing them, are"
                }
                Self::Mount(_) => "its FUSE mount of cells is",
                Self::Cells { .. } => "the cells VM processes have open are",
                Self::Counters { .. } => "its counters are",
                Self::SetPeers { .. } => "its peers are",
                Self::Migrate { .. }
                | Self::HandOver { .. }
                | Self::WaitHandOver { .. }
                | Self::AbortMigration { .. } => "migrations are",
                Self::MemoryLimit { .. }
                | Self::MemoryWeight { .. }
                | Self::InFlight { .. }
                | Self::ShrinkMemory { .. } => "its memory is",
                Self::DiskLimit { .. }
                | Self::DiskWeight { .. }
                | Self::TenantDisk { .. }
                | Self::Disks { .. }
                | Self::SetDisk { .. } => "its disks are",
                Self::Create { .. }
                | Self::Take { .. }
                | Self::Place { .. }
                | Self::Sync { .. }
                | Self::Fork { .. }
                | Self::Version { .. }
                | Self::Versions { .. }
                | Self::DropVersions { .. }
                | Self::Share { .. }
                | Self::ForkVersion { .. }
                | Self::Delete { .. }
                | Self::Drop { .. }
                | Self::Resize { .. }
                | Self::Public { .. }
                | Self::PublicAttachOnly { .. }
                | Self::Attach { .. } => "live cell commands are",
                Self::Import { .. } | Self::ImportMemory { .. } => "its imports are",
                Self::Stat { .. } => "its cells' stats are",
                Self::FormatBucket { .. }
                | Self::AddBucket { .. }
                | Self::Buckets { .. }
                | Self::Formats { .. }
                | Self::Ratchet { .. } => unreachable!("handled above"),
            };
            bail!(
                "{what} the extent daemon's: name its control socket (CLADEFSD_GRPC), or pass --config"
            );
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

    /// cladefs: parses `[--storage N] [--fxfs N]` for `cladefsctl ratchet` (doc-28).
    fn parse_ratchet(args: &[&str]) -> anyhow::Result<(Option<u32>, Option<u32>)> {
        let (mut storage, mut fxfs) = (None, None);
        let mut iter = args.iter();
        while let Some(&flag) = iter.next() {
            let raw = iter.next().with_context(|| format!("{flag} takes a version number"))?;
            let val: u32 =
                raw.parse().with_context(|| format!("bad version {raw:?} for {flag}"))?;
            match flag {
                "--storage" => storage = Some(val),
                "--fxfs" => fxfs = Some(val),
                _ => bail!(usage()),
            }
        }
        Ok((storage, fxfs))
    }

    /// cladefs: what `formats` and `ratchet` print of a bucket's ratchet gates (doc-28).
    fn formats_lines(gates: formats::Gates) -> String {
        format!(
            "storage {} (build {}..={})\nfxfs {} (build {}..={})\n",
            gates.storage,
            formats::gates::OLDEST_STORAGE,
            formats::gates::CURRENT_STORAGE,
            gates.fxfs,
            formats::gates::OLDEST_FXFS,
            formats::gates::CURRENT_FXFS,
        )
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
                "tenant": base32_id::Short(vm.tenant).to_string(),
                "cell": base32_id::Short(vm.cell).to_string(),
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
    fn print_value(value: &serde_json::Value, json: bool) -> anyhow::Result<()> {
        match json {
            true => println!("{}", serde_json::to_string_pretty(value)?),
            false => print!("{}", flattened(value)),
        }
        Ok(())
    }

    /// cladefs: what `StatCell` says of a cell, `stat`, every field by its name, as `cells` prints it
    /// (board 8585): its tenant and cell in base 32, its fsync level by name.
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
            fsync: _,
            fast_wal,
            fast_wal_bucket,
            fast_wal_degraded,
            ephemeral,
            placement_token,
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
            "placement_token": placement_token,
            "taken_token": taken_token,
            "taken_epoch": taken_epoch,
            "taken_unix_ms": taken_unix_ms,
            "uploaded": uploaded,
            "pending_upload_bytes": pending_upload_bytes,
            "oldest_pending_ms": oldest_pending_ms,
            "fsync": stat.fsync().as_str_name(),
            "fast_wal": fast_wal,
            "fast_wal_bucket": fast_wal_bucket,
            "fast_wal_degraded": fast_wal_degraded,
            "ephemeral": ephemeral,
        })
    }

    /// cladefs: what `GetCounters` answers, `counters`, every field by its name, its bucket's under
    /// `object_storage`, as `counters` prints it (board 8585).
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

    /// Formats a `DiskInfo` for `cladefsctl disks [--json]`.
    fn disk_json(disk: &cladefs::cladefsd::control::proto::DiskInfo) -> serde_json::Value {
        use cladefs::cladefsd::control::proto::{DiskHealthMode, DiskPlacementMode};
        let health = match disk.state() {
            DiskHealthMode::DiskHealthOnline => "online",
            DiskHealthMode::DiskHealthSuspect => "suspect",
            DiskHealthMode::DiskHealthOffline => "offline",
            DiskHealthMode::DiskHealthReturning => "returning",
        };
        let placement = match disk.placement() {
            DiskPlacementMode::DiskPlacementReadWrite => "read_write",
            DiskPlacementMode::DiskPlacementDrain => "drain",
            DiskPlacementMode::DiskPlacementReadOnly => "read_only",
        };
        let opt = |v: u64| (v != 0).then_some(v);
        serde_json::json!({
            "id": disk.id,
            "device": disk.device,
            "health": health,
            "placement": placement,
            "size_bytes": disk.size_bytes,
            "free_bytes": disk.free_bytes,
            "cap_bytes": opt(disk.cap_bytes),
            "headroom_bytes": disk.headroom_bytes,
            "allowance_bytes": disk.allowance_bytes,
            "cells_bytes": disk.cells_bytes,
            "reserved_bytes": disk.reserved_bytes,
            "cache_bytes": disk.cache_bytes,
            "cache_cap_bytes": opt(disk.cache_cap_bytes),
            "spare_bytes": disk.spare_bytes,
            "under_pressure": disk.under_pressure,
            "dirty_budget_bytes": opt(disk.dirty_budget_bytes),
        })
    }

    pub(super) fn main() -> anyhow::Result<()> {
        // cladefs: its threads' stacks on SIGQUIT, as an import that hung needed (TASK-98, TASK-44.13).
        #[cfg(target_os = "linux")]
        cladefs::stacks::on_sigquit()?;
        // cladefs: warnings, as of reading what the collector marked (TASK-264).
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
        // Global flags before the subcommand: `--config PATH` / `-c PATH`, `--timeout SECONDS`.
        let mut config_path: Option<PathBuf> = None;
        let mut timeout_cli: Option<std::time::Duration> = None;
        let mut idx = 0usize;
        while idx < args.len() {
            match args[idx].as_str() {
                "--config" | "-c" => {
                    let Some(path) = args.get(idx + 1) else {
                        bail!("--config requires a TOML file path");
                    };
                    config_path = Some(PathBuf::from(path));
                    idx += 2;
                }
                "--timeout" => {
                    let Some(given) = args.get(idx + 1) else {
                        bail!("--timeout requires a number of seconds");
                    };
                    timeout_cli = Some(seconds("--timeout", given)?);
                    idx += 2;
                }
                _ => break,
            }
        }
        let rest = &args[idx..];
        let config_path = config_path.or_else(|| {
            std::env::var_os("CLADEFS_CONFIG").filter(|v| !v.is_empty()).map(PathBuf::from)
        });
        if let Some(path) = config_path {
            let cfg = cladefs::config::Config::from_toml_file(&path)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("loading {}", path.display()))?;
            // SAFETY: invoked at process startup on the main thread before any tokio runtime or
            // worker threads exist.
            unsafe { cfg.apply_process_env() };
        }
        let timeout = match timeout_cli {
            Some(t) => Some(t),
            None => match std::env::var("CLADEFSCTL_TIMEOUT") {
                Ok(given) if !given.is_empty() => Some(seconds("CLADEFSCTL_TIMEOUT", &given)?),
                // Unset, or empty: none. Not UTF-8: refused.
                Err(std::env::VarError::NotUnicode(given)) => {
                    bail!("CLADEFSCTL_TIMEOUT must be a number of seconds: {given:?}")
                }
                _ => None,
            },
        };
        let control_socket = std::env::var_os("CLADEFSD_GRPC").filter(|socket| !socket.is_empty());
        let command = Command::parse(rest)?;
        match control_socket {
            Some(_) if command.is_on_buckets() => command.on_buckets(),
            Some(socket) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(command.through(socket.as_ref(), timeout)),
            None => command.locally(),
        }
    }
}
