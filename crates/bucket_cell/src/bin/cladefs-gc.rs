// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's collector (backlog doc-7, Garbage collection): reclaims what no live cell in a bucket
//! can reach. Only it deletes; it coordinates with cells'
//! writers, and with other collectors, through the bucket alone.
//!
//! ```sh
//! cladefs-gc URL [--cell TENANT/CELL] [--every SECONDS] [--at-once PASSES]
//!     [--lease SECONDS] [--metrics FILE] [--max-delete-objects N] [--max-delete-bytes N]
//!     [--max-requests-per-second N] [--max-refused-passes N]
//! cladefs-gc URL [--cell TENANT/CELL] --dry-run
//! cladefs-gc URL --cells [--cell TENANT/CELL] [--held FILE]
//! cladefs-gc URL --delete-cell TENANT/CELL [--force-missing]
//! cladefs-gc URL --usage [--tenant TENANT]
//! cladefs-gc URL --rotate-master-key
//! ```
//!
//! URL names the bucket (`s3://bucket/prefix`, `gs://bucket/prefix`, `az://container/prefix`),
//! reached with credentials from the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables. It
//! collects once, or every SECONDS, a cell at a time (TASK-395.4): every cell of the bucket, found
//! by delimiter listings a page at a time, listing none of their objects, PASSES of them at once
//! (16 by default, TASK-395.5; `bucket_cell::sweep`), or the one `--cell` names, its IDs in base
//! 32. Each cell's pass takes the cell's own lease, lists the cell alone, and deletes there alone,
//! but for the references it retires of its deleted children's under their parents' cells. It holds
//! the lease for `--lease` SECONDS (600 by default), renewing it between its steps and letting it
//! go as it ends: a pass, of this process or of any other collecting the bucket at once, that finds
//! its cell held leaves it and goes on to the next, as it does a cell another pass went over since
//! its sweep began; so processes collecting one bucket at once split its cells. A process gone
//! holds the cells its passes held until their leases expire, and the next sweep after that
//! collects them. A pass that fails says so and stops none of the others; the next pass tries it
//! again. It compacts nothing, and holds no key (doc-27): writers relocate out of their cells'
//! sparse segments themselves, and a pass decides by metadata alone, authenticated, not encrypted.
//! `--rotate-master-key` alone needs keys: the master keys.
//!
//! SECONDS are finite, from a millisecond to a year: anything else is a usage error (board 8511),
//! as a count of 0 is.
//!
//! cladefs: a sweep may be bounded (board 8529, `bucket_cell::sweep::Limits`): `--max-delete-objects`
//! and `--max-delete-bytes` bound what its passes delete together, objects of its cells and
//! references retired, bytes as listed, each pass taking what it would delete from them first, all
//! or none: once one would go past either, it deletes nothing, nor does any pass after it, and the
//! sweep starts no more, saying so; none by default. `--max-requests-per-second` spaces its
//! requests, its passes' and its listing's together, each waiting its turn (from 0.01 to a
//! million); none by default. After `--max-refused-passes` passes in a row the bucket refused (10
//! by default), as a bucket whose credentials or policy no longer let them in refuses every one,
//! it starts no more, saying so; a bucket out of reach fails no pass, each request of it going
//! again, backing off. SIGTERM or SIGINT has it start no more passes, let those running end,
//! letting their cells go, write its metrics and exit (board 8513); a second ends it at once, the
//! cells of the passes still running held until their leases expire. A sweep run once that did not
//! do all it was to, a pass failed, a pass deleting nothing past its bounds, or stopped before it
//! went over every cell for any of those reasons or because the bucket refused its listing of the
//! cells, fails the run once it said what it did and wrote its metrics (board 8509); run every so
//! long, it goes on, its metrics saying so, and ends as its last sweep did.
//!
//! Each sweep says on standard error, a line for each pass that deleted or retired something, or
//! failed, what it did, in how long, and the requests it made of each kind; then two lines of all
//! its passes: the objects and bytes deleted, the references retired,
//! the cells gone over, those left to passes holding them and to passes that went over them since,
//! and those failed, and the requests of each kind, listing the cells among them; then how long it
//! took, and its slowest passes. With `--metrics`, it writes the same of its last sweep to FILE
//! after each, in Prometheus' text format, for a node exporter's textfile collector: see
//! [`metrics`].
//!
//! `--dry-run` writes nothing at all, not even the collector's lease, which would fence a
//! pass running meanwhile out of its deletes: it prints each object a pass would delete, `KEY KIND
//! SIZE delete` a line, and each reference it would retire, `KEY reference - retire`, then their
//! counts and bytes. A pass says what it did on standard error, a line for all the cells it ran
//! over, and logs there what warns. The mark-only passes, which wrote what they would delete to
//! the bucket for every reader to watch, went pre-production (board 10545): a dry run's output
//! says as much, touching neither the bucket nor its readers.
//!
//! The collector keeps what every live cell inherited, whether or not a host holds the cell,
//! which only hosts know. `--cells` lists the live cells, of every cell or of the one
//! `--cell` names, one a line, a fork's after the
//! cell and the segment it forked at, each cell as TENANT/CELL, all in base 32, but
//! those FILE names, as `cladefsctl stat` calls them, separated by white space: with FILE naming
//! every cell every host holds, those left are no host's, or newer than FILE. It, and a dry run,
//! say each cell's lines as they read it, holding no more than a page of the cells listed, and a
//! cell's findings, at once (board 8513). `--delete-cell`
//! deletes one, whichever host took it last, which stops writing if there is one, and the next pass
//! frees what only it held (TASK-79.85). A cell of no head it refuses, writing nothing (board
//! 8511), as a typo of an ID would get a deleted head no pass removes, for good, unless
//! `--force-missing` says to delete it all the same.
//!
//! `--usage` says what each cell takes, and each tenant, every cell's or `--tenant`'s alone, as the
//! collector's last pass over each cell to end found it, writing nothing (TASK-394, TASK-395.8): it
//! reads each cell's collector lease, where that pass recorded it, listing none of the cells'
//! objects, the tenants and their cells listed a page at a time, so that what it holds at once is a
//! page of each, however many tenants, cells and objects the bucket holds. It streams a JSON object
//! a line: one for each cell of a tenant, then one for the tenant, IDs in base 32. A cell's:
//! `{"scope":"cell","tenant":"1","cell":"1/2f",
//! "measured_unix_ms":...,"objects":...,"bytes":...,"kinds":{"segment":{"objects":...,
//! "bytes":...},...}}`, `measured_unix_ms` when the listing it is of began, by the bucket's clock,
//! and all four null for a cell no pass ran over to its end yet. A tenant's: `{"scope":"tenant",
//! "tenant":"1","cells":...,"measured_cells":...,"oldest_measured_unix_ms":...,
//! "newest_measured_unix_ms":...,"objects":...,"bytes":...,"kinds":{...}}`, the sum of its cells
//! measured. A cell's objects are those under its prefix, of each kind: its cells', its
//! collector's lease, and the references of forks of its
//! cells; the garbage the collector has yet to reclaim among them; bytes as stored. A pass that
//! skips a cell unchanged since the last measures it not again: its usage is as of the last pass
//! that went over it in full, which nothing under its prefix changed since.
//!
//! `--rotate-master-key` rewraps every tenant's KEK object the bucket KMS keeps in the bucket
//! (`tenants/<tenant>/kek`, TASK-411.27, doc-16 The bucket KMS) under the newest master key, the
//! first of `CLADEFS_MASTER_KEY`'s (or `CLADEFS_MASTER_KEY_FILE`'s), each by a conditional update,
//! the others given unwrapping those under them; the KEKs stay as they are. It says on standard
//! error how many it rewrapped, how many were under the newest already, and how many tenants are
//! shredded (tombstones, left), naming the newest key by its ID. A KEK object under none of the
//! keys given is left as it is, said, and fails the run, which, done again with its key among them,
//! finishes. Refusing none, it then records the newest key's ID in the bucket's entry of the
//! registry of buckets, if that entry records a master key (`cladefsctl add-bucket` records it), in
//! the root bucket's registry, then in every bucket's copy, each reached as the bucket's hosts
//! reach it (`CLADEFSD_BUCKET_<N>_*`), saying so (board 8483): hosts of other buckets choose this
//! bucket's KMS by it. A copy out of reach is said, and fails the run, which, done again, finishes
//! it. URL may be any bucket, the root or another. Once it ran refusing none and failing no copy,
//! hosts need the newest key alone. Done again, it rewraps nothing.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

use base32_id::Short;
use bucket_cell::bucket::{Bucket, Meta};
use bucket_cell::collector::{Collector, DryRun, Report};
use bucket_cell::layout;
use bucket_cell::layout::CellId;
use bucket_cell::layout::Head;
use bucket_cell::object_bucket::{ObjectBucket, Request, RoutedBucket, probe};
use bucket_cell::passed::{self, Sum, Usage};
use bucket_cell::sweep::{self, Limits, Outcome, Ran, Requests, Stop, Stopped, Sweep, Swept};
use bucket_cell::{cell, lifecycle};
use futures::{Stream, StreamExt as _};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn usage() -> String {
    "usage: cladefs-gc URL [--cell TENANT/CELL] [--every SECONDS] \
     [--at-once PASSES] [--lease SECONDS] [--metrics FILE] \
     [--max-delete-objects N] [--max-delete-bytes N] [--max-requests-per-second N] \
     [--max-refused-passes N] | \
     cladefs-gc URL [--cell TENANT/CELL] --dry-run | \
     cladefs-gc URL --cells [--cell TENANT/CELL] [--held FILE] | \
     cladefs-gc URL --delete-cell TENANT/CELL [--force-missing] | \
     cladefs-gc URL --delete-tenant TENANT | \
     cladefs-gc URL --usage [--tenant TENANT] | \
     cladefs-gc URL --rotate-master-key (every cell, or the one --cell names; SECONDS from a \
     millisecond to a year; --dry-run writes nothing)"
        .into()
}

/// What the command line asks for.
#[derive(Debug, PartialEq)]
struct Args {
    url: String,
    command: Command,
}

/// What it does to the bucket.
#[derive(Debug, PartialEq)]
enum Command {
    /// Collects, every so long if more than once, every cell or one.
    Collect { every: Option<Duration>, mode: Mode, cell: Option<CellId>, run: Run },
    /// Lists the live cells, of every cell or one, but those a file names.
    Cells { held: Option<PathBuf>, cell: Option<CellId> },
    /// Deletes a cell: one of no head only if `missing` says so (board 8511).
    DeleteCell { cell: CellId, missing: bool },
    /// Marks a tenant shredded in `events` and deletes all its live cells in this bucket.
    DeleteTenant { tenant: u64 },
    /// Says what each cell takes, and each tenant, every tenant's or one's, as recorded (TASK-394,
    /// TASK-395.8).
    Usage { tenant: Option<u64> },
    /// Rewraps tenants' KEK objects under the newest master key (TASK-411.27).
    RotateMasterKey,
}

/// How a sweep runs its passes, and what it writes of them (TASK-395.5).
#[derive(Debug, PartialEq)]
struct Run {
    /// How many at once.
    at_once: usize,
    /// How long each holds its cell, in milliseconds.
    lease: u64,
    /// Where it writes its metrics, if anywhere.
    metrics: Option<PathBuf>,
    /// cladefs: the most objects, and bytes, its passes delete together, the most requests a
    /// second they make, and the passes in a row the bucket refuses after which it starts no
    /// more (board 8529, `bucket_cell::sweep::Limits`).
    max_deleted_objects: Option<u64>,
    max_deleted_bytes: Option<u64>,
    requests_per_second: Option<f64>,
    refused_in_a_row: usize,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            at_once: 16,
            lease: 600_000,
            metrics: None,
            max_deleted_objects: None,
            max_deleted_bytes: None,
            requests_per_second: None,
            refused_in_a_row: REFUSED_IN_A_ROW,
        }
    }
}

/// cladefs: after how many passes in a row the bucket refused a sweep starts no more, unless
/// `--max-refused-passes` says (board 8529, hotbed question 491): passes of cells of
/// their own, of which so many refused in a row is the bucket's refusal, its credentials' or its
/// policy's, not a cell's.
const REFUSED_IN_A_ROW: usize = 10;

/// cladefs: the most seconds `--every` and `--lease` take: a year (board 8511), which
/// leaves each, in milliseconds, far from a u64's end, whatever a pass adds to it.
const MOST_SECONDS: u64 = 365 * 24 * 3600;

/// cladefs: the fewest and the most requests a second `--max-requests-per-second` takes (board
/// 8529).
const REQUESTS_PER_SECOND: std::ops::RangeInclusive<f64> = 0.01..=1_000_000.0;

/// How many of a sweep's slowest passes it says.
const SLOWEST: usize = 5;

/// What a pass does with what no live cell needs (TASK-264).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Delete,
    /// Prints it, writing nothing, once.
    DryRun,
}

/// The command line `args`, after the program's name.
fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let url = args.next().ok_or_else(usage)?;
    let args: Vec<String> = args.collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let command = match args.as_slice() {
        ["--cells", rest @ ..] => parse_cells(rest)?,
        ["--delete-cell", cell] => {
            Command::DeleteCell { cell: cell.parse().map_err(|_| usage())?, missing: false }
        }
        ["--delete-cell", cell, "--force-missing"] => {
            Command::DeleteCell { cell: cell.parse().map_err(|_| usage())?, missing: true }
        }
        ["--delete-tenant", tenant] => {
            let tenant = base32_id::parse(tenant).ok_or_else(usage)?;
            if tenant == layout::DEFAULT_TENANT {
                return Err("--delete-tenant: cannot delete the default tenant (0)".into());
            }
            Command::DeleteTenant { tenant }
        }
        ["--usage"] => Command::Usage { tenant: None },
        ["--rotate-master-key"] => Command::RotateMasterKey,
        ["--usage", "--tenant", tenant] => {
            Command::Usage { tenant: Some(base32_id::parse(tenant).ok_or_else(usage)?) }
        }
        flags => parse_collect(flags)?,
    };
    Ok(Args { url, command })
}

/// `--cells`' `flags`, after it: a cell, a file of those held, each at most once.
fn parse_cells(flags: &[&str]) -> Result<Command, String> {
    let (mut held, mut cell) = (None, None);
    let mut flags = flags.iter();
    while let Some(&flag) = flags.next() {
        let value = flags.next().ok_or_else(usage)?;
        match flag {
            "--held" if held.is_none() => held = Some(value.into()),
            "--cell" if cell.is_none() => cell = Some(value.parse().map_err(|_| usage())?),
            _ => return Err(usage()),
        }
    }
    Ok(Command::Cells { held, cell })
}

/// A pass's `flags`: a dry run's run once.
fn parse_collect(flags: &[&str]) -> Result<Command, String> {
    let mut every = None;
    let mut mode = Mode::Delete;
    let mut cell = None;
    let mut run = Run::default();
    let mut flags = flags.iter().copied();
    while let Some(flag) = flags.next() {
        let mut value = || flags.next().ok_or_else(usage);
        match flag {
            "--dry-run" if mode != Mode::Delete => return Err(usage()),
            "--dry-run" => mode = Mode::DryRun,
            "--cell" if cell.is_none() => cell = Some(value()?.parse().map_err(|_| usage())?),
            "--at-once" => run.at_once = count(value()?)?,
            "--metrics" if run.metrics.is_none() => run.metrics = Some(value()?.into()),
            "--every" => every = Some(seconds(value()?)?),
            "--lease" => run.lease = millis(seconds(value()?)?),
            "--max-delete-objects" => run.max_deleted_objects = Some(count(value()?)?),
            "--max-delete-bytes" => run.max_deleted_bytes = Some(count(value()?)?),
            "--max-requests-per-second" => {
                let rate: f64 = value()?.parse().map_err(|_| usage())?;
                // NaN in no range.
                if !REQUESTS_PER_SECOND.contains(&rate) {
                    return Err(usage());
                }
                run.requests_per_second = Some(rate);
            }
            "--max-refused-passes" => run.refused_in_a_row = count(value()?)?,
            _ => return Err(usage()),
        }
    }
    // A dry run runs once, a pass at a time, holding no lease and writing nothing.
    if mode == Mode::DryRun && (every.is_some() || run != Run::default()) {
        return Err(usage());
    }
    Ok(Command::Collect { every, mode, cell, run })
}

/// cladefs: a count of seconds, `text`, as `--every` and `--lease` take it (board 8511): finite,
/// from a millisecond to [`MOST_SECONDS`]; a usage error otherwise, never a value cast into range:
/// before, `-60` and `NaN` became 0, and `--every` of a negative, NaN or infinite count panicked.
fn seconds(text: &str) -> Result<Duration, String> {
    let seconds: f64 = text.parse().map_err(|_| usage())?;
    // Negative, NaN and infinite counts, and those past a Duration's end, refused.
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| usage())?;
    match Duration::from_millis(1) <= duration && duration <= Duration::from_secs(MOST_SECONDS) {
        true => Ok(duration),
        false => Err(usage()),
    }
}

/// `duration`, of [`seconds`], in milliseconds: at most a year's, which a u64 holds.
fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

/// A count, `text`, of 1 at least.
fn count<T: std::str::FromStr + Default + PartialOrd>(text: &str) -> Result<T, String> {
    let count: T = text.parse().map_err(|_| usage())?;
    match count > T::default() {
        true => Ok(count),
        false => Err(usage()),
    }
}

/// The cells `text` names, in base 32, separated by white space.
fn held_cells(text: &str) -> Result<BTreeSet<layout::CellId>, String> {
    let cell = |word: &str| word.parse();
    text.split_whitespace().map(cell).collect()
}

/// What `--cells` prints of `cell`, live, of `record`.
fn listed(cell: CellId, record: &Head) -> String {
    match record.origin() {
        Some(origin) => format!("{cell} {} {}\n", origin.parent, Short(origin.at)),
        None => format!("{cell}\n"),
    }
}

/// Writes to `out` the live cells of `bucket`, every cell's or `named`'s, but those `held`, a line
/// each as it reads its head (`--cells`): holding a page of the cells listed at most, whatever the
/// bucket holds (board 8513), in the order of their IDs.
async fn say_cells(
    bucket: &impl Bucket,
    named: Option<CellId>,
    held: &BTreeSet<CellId>,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let mut cells = std::pin::pin!(cells(bucket, named));
    while let Some(cell) = cells.next().await {
        let cell = cell?;
        if held.contains(&cell) {
            continue;
        }
        let found = bucket_cell::head::live(bucket, cell).await;
        if let Some(record) = found.map_err(|err| format!("cell {cell}: {err}"))? {
            write!(out, "{}", listed(cell, &record)).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

/// How many cells' leases `--usage` reads at once.
const USAGES_AT_ONCE: usize = 32;

/// `kinds`, as `--usage` says them: each kind's objects and bytes, by its name.
fn kinds_json(kinds: &BTreeMap<String, Sum>) -> Value {
    let kind = |sum: &Sum| json!({ "objects": sum.objects, "bytes": sum.bytes });
    Value::Object(kinds.iter().map(|(name, sum)| (name.clone(), kind(sum))).collect())
}

/// What `--usage` says of `cell`, of `usage`, as its last pass to end recorded it, if one did.
fn cell_line(cell: CellId, usage: Option<&Usage>) -> String {
    let total = usage.map(Usage::total);
    json!({
        "scope": "cell",
        "tenant": Short(cell.tenant).to_string(),
        "cell": cell.to_string(),
        "measured_unix_ms": usage.map(|usage| usage.measured),
        "objects": total.map(|total| total.objects),
        "bytes": total.map(|total| total.bytes),
        "kinds": usage.map(|usage| kinds_json(&usage.kinds)),
    })
    .to_string()
}

/// What a tenant's cells take, as `--usage` reads them one after another.
#[derive(Default)]
struct Tenant {
    cells: u64,
    measured: u64,
    /// The oldest and newest of its cells' measurements.
    when: Option<(u64, u64)>,
    kinds: BTreeMap<String, Sum>,
}

impl Tenant {
    /// Adds one of its cells, of `usage`, if a pass recorded it.
    fn add(&mut self, usage: Option<&Usage>) {
        self.cells += 1;
        let Some(usage) = usage else { return };
        self.measured += 1;
        let at = usage.measured;
        self.when = Some(self.when.map_or((at, at), |(old, new)| (old.min(at), new.max(at))));
        for (name, sum) in &usage.kinds {
            *self.kinds.entry(name.clone()).or_default() += *sum;
        }
    }

    /// What `--usage` says of it, `tenant`.
    fn line(&self, tenant: u64) -> String {
        let mut total = Sum::default();
        self.kinds.values().for_each(|sum| total += *sum);
        json!({
            "scope": "tenant",
            "tenant": Short(tenant).to_string(),
            "cells": self.cells,
            "measured_cells": self.measured,
            "oldest_measured_unix_ms": self.when.map(|(oldest, _)| oldest),
            "newest_measured_unix_ms": self.when.map(|(_, newest)| newest),
            "objects": total.objects,
            "bytes": total.bytes,
            "kinds": kinds_json(&self.kinds),
        })
        .to_string()
    }
}

/// Writes to `out` what each cell of `bucket` takes, and each tenant, every tenant's or `named`'s,
/// a line each as it reads them (`--usage`): reading each cell's collector lease, a few at once,
/// the tenants and each one's cells listed a page at a time, so that it holds a page of each at
/// most, whatever the bucket holds (TASK-395.8).
async fn say_usage(
    bucket: &impl Bucket,
    named: Option<u64>,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let tenants = match named {
        Some(tenant) => futures::stream::iter([Ok(tenant)]).left_stream(),
        None => cell::tenants(bucket).right_stream(),
    };
    let mut tenants = std::pin::pin!(tenants);
    while let Some(tenant) = tenants.next().await {
        let tenant = tenant.map_err(|err| err.to_string())?;
        let reads = cell::of_tenant(bucket, tenant)
            .map(|cell| async move {
                let cell = cell.map_err(|err| err.to_string())?;
                let read = passed::read(bucket, cell).await;
                read.map(|lease| (cell, lease)).map_err(|err| format!("cell {cell}: {err}"))
            })
            .buffered(USAGES_AT_ONCE);
        let mut reads = std::pin::pin!(reads);
        let mut sum = Tenant::default();
        while let Some(read) = reads.next().await {
            let (cell, lease) = read?;
            let usage = lease.and_then(|lease| lease.passed).map(|passed| passed.usage);
            writeln!(out, "{}", cell_line(cell, usage.as_ref())).map_err(|err| err.to_string())?;
            sum.add(usage.as_ref());
        }
        writeln!(out, "{}", sum.line(tenant)).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// The cells a command goes over: the one named, or every cell of `bucket`, found by delimiter
/// listings a page at a time, listing none of their objects (TASK-395.4, TASK-395.5).
fn cells(
    bucket: &impl Bucket,
    named: Option<CellId>,
) -> impl Stream<Item = Result<CellId, String>> + '_ {
    let cells = match named {
        Some(cell) => futures::stream::iter([Ok(cell)]).left_stream(),
        None => cell::listed(bucket).right_stream(),
    };
    cells.map(|cell| cell.map_err(|err| err.to_string()))
}

/// Collects every cell of `bucket`, or the one `named`, a pass over each, once, or every `every`,
/// deleting, as `run` says, until `stop` is asked; or prints what a pass
/// would delete, once. A sweep run once that did not do all it was to fails the run, once it said
/// so and wrote its metrics (board 8509); run every so long, the process goes on, and its metrics
/// say so; stopped, it ends as its last sweep did.
async fn collect(
    bucket: &Arc<RoutedBucket>,
    every: Option<Duration>,
    mode: Mode,
    named: Option<CellId>,
    (run, stop): (Run, Arc<Stop>),
) -> Result<(), String> {
    let swept_mode = match mode {
        Mode::Delete => sweep::Mode::Delete,
        Mode::DryRun => {
            let shredded = shredded_tenants(bucket).await?;
            let mut out = std::io::stdout().lock();
            return dry_run_cells(&**bucket, named, shredded, &mut out).await;
        }
    };
    let sweep = Sweep {
        mode: swept_mode,
        at_once: run.at_once,
        lease: run.lease,
        slowest: SLOWEST,
        // Each pass on the runtime's threads (TASK-395.15).
        spawned: true,
    };
    // cladefs: copies of other buckets' cells' versions here, by its own bucket (TASK-633.8).
    let own = bucket.own();
    let mut sweeps = 0;
    loop {
        let shredded = shredded_tenants(bucket).await?;
        let limits = Limits {
            deleted_objects: run.max_deleted_objects,
            deleted_bytes: run.max_deleted_bytes,
            requests_per_second: run.requests_per_second,
            refused_in_a_row: Some(run.refused_in_a_row),
            stop: Some(stop.clone()),
            shredded: Some(shredded),
        };
        let mut passes = Histogram::default();
        let observe = |ran: &Ran| {
            passes.observe(ran);
            if let Some(line) = passed_line(ran) {
                eprintln!("{line}");
            }
        };
        let swept = sweep.run_limited(bucket, Some(&own), named, &limits, observe).await;
        sweeps += 1;
        let (did, took) = swept_lines(&swept);
        eprintln!("{did}\n{took}");
        if let Some(stopped) = &swept.stopped {
            eprintln!("cladefs-gc: the sweep stopped: {stopped}");
        }
        if let Some(file) = &run.metrics {
            let written = metrics(sweeps, &swept, &passes, peak_resident(), SystemTime::now());
            write_atomically(file, &written).map_err(|err| format!("{}: {err}", file.display()))?;
        }
        let ended = ended(&swept);
        let Some(every) = every else { return ended };
        if stop.asked() {
            return ended;
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            () = stop.wait() => return ended,
        }
    }
}

/// cladefs: what a sweep, `swept`, ends a run with (board 8509): an error, unless it did all it
/// was to, saying what it left undone.
fn ended(swept: &Swept) -> Result<(), String> {
    if swept.complete() {
        return Ok(());
    }
    let mut undone = Vec::new();
    if swept.failed > 0 {
        undone.push(format!("{} passes failed", swept.failed));
    }
    if swept.capped > 0 {
        undone.push(format!("{} passes deleted nothing, past its bounds", swept.capped));
    }
    if let Some(stopped) = &swept.stopped {
        undone.push(format!("it stopped: {stopped}"));
    }
    Err(format!("the sweep did not go through: {}", undone.join("; ")))
}

/// cladefs: blocks SIGTERM and SIGINT in the calling thread, and so in every thread it starts
/// after, which a process does before it starts any; and starts one that waits for them, as an
/// orchestrator or an operator stops a sweep (board 8513): the first asks the returned [`Stop`],
/// so that the sweep starts no more passes and those running end, letting their cells go; the
/// second ends the process at once, the cells of the passes still running held until their leases
/// expire (`--lease`).
fn stop_on_signals() -> Result<Arc<Stop>, String> {
    // SAFETY: A signal set, plain data, filled by the calls below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: Plain calls on a signal set this function owns, and on the calling thread's mask,
    // which the threads it starts from now on inherit.
    let status = unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
    };
    if status != 0 {
        let error = std::io::Error::from_raw_os_error(status);
        return Err(format!("blocking SIGTERM and SIGINT: {error}"));
    }
    let stop = Arc::new(Stop::default());
    let asked = stop.clone();
    let waiting = move || {
        for signals in 1.. {
            let mut signal = 0;
            // SAFETY: Waits for a signal of the set, which every thread of the process blocks.
            let status = unsafe { libc::sigwait(&set, &mut signal) };
            if status != 0 {
                let error = std::io::Error::from_raw_os_error(status);
                eprintln!("cladefs-gc: waiting for SIGTERM and SIGINT: {error}");
                std::process::exit(1);
            }
            let name = if signal == libc::SIGINT { "SIGINT" } else { "SIGTERM" };
            if signals > 1 {
                eprintln!(
                    "cladefs-gc: {name} again: exiting, the cells of the passes running held \
                     until their leases expire"
                );
                std::process::exit(128 + signal);
            }
            eprintln!(
                "cladefs-gc: {name}: starting no more passes, letting those running end; again \
                 to exit at once"
            );
            asked.ask();
        }
    };
    let thread = std::thread::Builder::new().name("cladefs-gc signals".into());
    thread.spawn(waiting).map_err(|err| format!("a thread for signals: {err}"))?;
    Ok(stop)
}

/// Rewraps every tenant's KEK object in `bucket` under the newest of the master keys the
/// environment gives (TASK-411.27), then records it in the bucket's entry of the registry of
/// buckets, the root's and every copy (board 8483, `bucket_cell::bucket_kms::rotate_recorded`),
/// saying what it did on standard error; failing, once it went over every tenant, if it left any
/// under none of them, or a copy of the registry unwritten.
async fn rotate_master_key(bucket: &RoutedBucket) -> Result<(), String> {
    use bucket_cell::bucket_kms::{Recorded, Rotated};
    use extent_keys::kms::master::{MASTER_KEY_FILE_VAR, MASTER_KEY_VAR, MasterKeys};
    let keys = MasterKeys::from_env()?.ok_or_else(|| {
        format!(
            "--rotate-master-key: neither {MASTER_KEY_VAR} nor {MASTER_KEY_FILE_VAR} gives a \
             master key: the newest first, then those to rotate from"
        )
    })?;
    let rotated = bucket_cell::bucket_kms::rotate_recorded(bucket, &keys).await;
    let (rotated, recorded) = rotated.map_err(|err| format!("--rotate-master-key: {err}"))?;
    let Rotated { rewrapped, newest, destroyed, refused } = rotated;
    eprintln!(
        "cladefs-gc: master key {}: rewrapped {rewrapped} tenants' KEKs, {newest} under it already, \
         {destroyed} tenants shredded, {} refused",
        keys.newest().id(),
        refused.len()
    );
    for refusal in &refused {
        eprintln!("cladefs-gc: left as it is: {refusal}");
    }
    match recorded {
        Recorded::Refused => Err(format!(
            "{} tenants' KEK objects left under no master key given, the registry of buckets \
             unchanged: run again with theirs among {MASTER_KEY_VAR}'s",
            refused.len()
        )),
        // No registry, or no master key recorded in it: none to record, nothing said.
        Recorded::Unrecorded => Ok(()),
        Recorded::Recorded { failed } if failed.is_empty() => {
            eprintln!(
                "cladefs-gc: the registry of buckets records master key {} for this bucket",
                keys.newest().id()
            );
            Ok(())
        }
        Recorded::Recorded { failed } => {
            for (number, why) in &failed {
                eprintln!(
                    "cladefs-gc: bucket {number}'s copy of the registry left as it was: {why}"
                );
            }
            Err(format!(
                "{} buckets' copies of the registry still name the old master key: run again, \
                 those buckets in reach, before dropping it from any host",
                failed.len()
            ))
        }
    }
}

/// Reads the shredded tenant set from the root bucket's `events` object.
async fn shredded_tenants(bucket: &RoutedBucket) -> Result<Arc<BTreeSet<u64>>, String> {
    let auth = bucket_cell::events::authority(bucket)
        .await
        .map_err(|err| format!("events authority: {err}"))?;
    let log =
        bucket_cell::events::get(&*auth).await.map_err(|err| format!("reading events: {err}"))?;
    Ok(Arc::new(log.map(|l| l.shredded_set()).unwrap_or_default()))
}

/// Publishes `TenantShredded` to the root bucket's `events` object and ends every live cell head
/// of `tenant` in `bucket` (without creating heads for missing cells).
async fn delete_tenant(bucket: &RoutedBucket, tenant: u64) -> Result<(), String> {
    let auth = bucket_cell::events::authority(bucket)
        .await
        .map_err(|err| format!("--delete-tenant: events authority: {err}"))?;
    bucket_cell::events::publish(&*auth, bucket_cell::events::Publish::TenantShredded(tenant))
        .await
        .map_err(|err| format!("--delete-tenant: publishing events: {err}"))?;
    let holder = rand::random();
    let mut deleted = 0u64;
    let mut listed = std::pin::pin!(cell::of_tenant(bucket, tenant));
    while let Some(cell) = listed.next().await {
        let cell =
            cell.map_err(|err| format!("--delete-tenant: listing tenant {tenant}: {err}"))?;
        if lifecycle::delete_existing(bucket, cell, holder, u64::MAX)
            .await
            .map_err(|err| format!("--delete-tenant: deleting cell {cell}: {err}"))?
        {
            deleted += 1;
        }
    }
    eprintln!(
        "cladefs-gc: tenant {}: recorded shredded in events, deleted {deleted} live cells",
        Short(tenant)
    );
    Ok(())
}

/// Deletes `cell` of `bucket`, whichever placement took it last, as an operator does.
async fn delete_cell(bucket: &impl Bucket, cell: layout::CellId) -> Result<(), String> {
    // Whichever placement took it last: u64::MAX is no lower than any's token.
    let deleted = lifecycle::delete(bucket, cell, rand::random(), u64::MAX);
    deleted.await.map_err(|err| format!("deleting cell {cell}: {err}"))
}

/// cladefs: refuses `--delete-cell` of `cell` if it has no head, writing nothing (board 8511): a
/// typo of an ID would get a deleted head of the cell's own, which no pass of the collector
/// removes ([`Head::removable`]), and every sweep after reads, for good. `--force-missing` deletes
/// it all the same.
async fn refuse_missing(bucket: &impl Bucket, cell: CellId) -> Result<(), String> {
    match bucket_cell::head::get(bucket, cell).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!(
            "--delete-cell: cell {cell} has no head, so no cell to delete: nothing written; \
             --force-missing writes it a deleted head, which no pass of the collector removes"
        )),
        Err(err) => Err(format!("--delete-cell: cell {cell}: {err}")),
    }
}

/// What a dry run over every cell of `bucket`, or the one `named`, finds, written to `out` a cell
/// at a time as it goes, holding one cell's findings at most (board 8513), then counted on
/// standard error.
async fn dry_run_cells(
    bucket: &impl Bucket,
    named: Option<CellId>,
    shredded: Arc<BTreeSet<u64>>,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let collector = Collector::new().shredded(shredded);
    let mut sum = DrySum::default();
    let mut cells = std::pin::pin!(cells(bucket, named));
    while let Some(cell) = cells.next().await {
        let cell = cell?;
        let found = collector.dry_run_cell(bucket, cell).await;
        let found = found.map_err(|err| format!("cell {cell}: {err}"))?;
        write!(out, "{}", dry_lines(&found)).map_err(|err| err.to_string())?;
        sum.add(&found);
    }
    eprintln!("{}", sum.line());
    Ok(())
}

/// What the requests `requests` were, of each kind made: `get 3, list 1`; `none` if none.
fn requests_text(requests: &Requests) -> String {
    let made: Vec<String> = (Request::ALL.into_iter())
        .filter(|&request| requests.of(request) > 0)
        .map(|request| format!("{} {}", request.name(), requests.of(request)))
        .collect();
    if made.is_empty() { "none".to_owned() } else { made.join(", ") }
}

/// What a pass that deleted or retired something, or failed, says of `passed`: none for one that
/// did nothing, skipping its cell or not, or left its cell to another.
fn passed_line(passed: &Ran) -> Option<String> {
    let Ran { cell, outcome, requests, took } = passed;
    let tail = format!("in {:.3} s; requests: {}", took.as_secs_f64(), requests_text(requests));
    let idle = |report: &Report| Report { skipped: 0, ..*report } == Report::default();
    match outcome {
        Outcome::Collected(report) if idle(report) => None,
        // One that skipped its cell, unchanged since its last pass, trimming read evidence
        // (TASK-395.6, TASK-702.2).
        Outcome::Collected(report) if report.skipped > 0 => {
            Some(format!("cladefs-gc: cell {cell}, unchanged: {}, {tail}", did(report)))
        }
        Outcome::Collected(report) => {
            Some(format!("cladefs-gc: cell {cell}: {}, {tail}", did(report)))
        }
        Outcome::Left(_) => None,
        Outcome::Failed(err) => Some(format!("cladefs-gc: collecting cell {cell}: {err}, {tail}")),
    }
}

/// What passes that delete did, as `report` says; and the dead cells they removed whole, if any
/// (TASK-455).
fn did(report: &Report) -> String {
    let Report { deleted, retired, deleted_bytes, removed, .. } = report;
    let removed = match removed {
        0 => String::new(),
        removed => format!(", removed {removed} dead cells"),
    };
    format!("deleted {deleted} ({deleted_bytes} bytes), retired {retired} references{removed}")
}

/// What a sweep says of `swept`: what its passes did, and the requests it made; then how long it
/// took, and its slowest passes.
fn swept_lines(swept: &Swept) -> (String, String) {
    let Swept { visited, held, swept: since, failed, report, requests, slowest, took, .. } = swept;
    let did = did(report);
    let did = format!(
        "cladefs-gc: {did}, over {visited} cells ({} unchanged), {held} held by other passes, \
         {since} swept by them since, {failed} failed; requests: {}",
        report.skipped,
        requests_text(requests)
    );
    let slowest: Vec<String> = (slowest.iter())
        .map(|(cell, took)| format!("{cell} {:.3} s", took.as_secs_f64()))
        .collect();
    let slowest = if slowest.is_empty() { "none".to_owned() } else { slowest.join(", ") };
    (did, format!("cladefs-gc: took {:.3} s; slowest: {slowest}", took.as_secs_f64()))
}

/// The bounds, in seconds, of [`Histogram`]'s buckets.
const BOUNDS: [f64; 7] = [0.001, 0.01, 0.1, 1.0, 10.0, 60.0, 600.0];

/// How long a sweep's passes over cells took, those it ran: how many took no longer than each
/// of [`BOUNDS`], and how long they took together.
#[derive(Debug, Default)]
struct Histogram {
    within: [u64; BOUNDS.len()],
    count: u64,
    sum: f64,
}

impl Histogram {
    fn observe(&mut self, passed: &Ran) {
        if matches!(passed.outcome, Outcome::Left(_)) {
            return;
        }
        let took = passed.took.as_secs_f64();
        for (within, bound) in self.within.iter_mut().zip(BOUNDS) {
            *within += u64::from(took <= bound);
        }
        self.count += 1;
        self.sum += took;
    }
}

/// What `--metrics` writes after the process's `sweeps`th sweep, `swept`, whose passes took as
/// `passes` says, ended at `now`, the process having taken `peak` bytes of memory at most so far:
/// in Prometheus' text format, the last sweep's.
///
/// - `cladefs_gc_sweeps_total`: the sweeps the process ran;
///   `cladefs_gc_sweep_end_timestamp_seconds` and `cladefs_gc_sweep_seconds`: when its last ended,
///   and how long it took.
/// - `cladefs_gc_sweep_cells{outcome}`: its cells, by what came of each: `passed` (in full),
///   `skipped` (unchanged since its last pass, TASK-395.6), `failed`, `held` (left to a pass
///   holding it), `swept` (left to a pass that went over it since).
/// - `cladefs_gc_sweep_stopped{reason}`: 1 for why it stopped before it went over every cell, if
///   it did, 0 for each other reason (`bucket_cell::sweep::Stopped::NAMES`): `unlisted` (the
///   bucket refused its listing of the cells), `unidentified` (its own identity unread),
///   `refusing` (`--max-refused-passes` in a row refused), `capped` (a pass would have deleted
///   past `--max-delete-objects` or `--max-delete-bytes`), `asked` (SIGTERM or SIGINT); and
///   `cladefs_gc_sweep_capped_passes`, the passes that deleted nothing past those bounds (boards
///   8509, 8529, 8513).
/// - `cladefs_gc_sweep_objects{action}` and `cladefs_gc_sweep_bytes{action}`: what its passes
///   `deleted`, as listed; `cladefs_gc_sweep_retired_references`, and
///   `cladefs_gc_sweep_removed_cells`, the dead cells removed whole (TASK-455).
/// - `cladefs_gc_sweep_requests{request}`: the requests it made of each kind
///   (`bucket_cell::object_bucket::Request::name`), listing the cells among them.
/// - `cladefs_gc_pass_seconds`: a histogram of how long its passes took, those it ran;
///   `cladefs_gc_slowest_pass_seconds{cell}`: its slowest passes, by their cells, TENANT/CELL.
/// - `cladefs_gc_peak_resident_bytes`: the most memory the process held so far.
fn metrics(sweeps: u64, swept: &Swept, passes: &Histogram, peak: u64, now: SystemTime) -> String {
    let mut text = String::new();
    let mut family = |name: &str, kind: &str, help: &str, samples: &[(String, String)]| {
        text += &format!("# HELP {name} {help}\n# TYPE {name} {kind}\n");
        for (labels, value) in samples {
            text += &format!("{name}{labels} {value}\n");
        }
    };
    let one = |value: String| [(String::new(), value)];
    let ended = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    family(
        "cladefs_gc_sweeps_total",
        "counter",
        "Sweeps the process ran.",
        &one(sweeps.to_string()),
    );
    family(
        "cladefs_gc_sweep_end_timestamp_seconds",
        "gauge",
        "When its last sweep ended.",
        &one(format!("{ended:.3}")),
    );
    family(
        "cladefs_gc_sweep_seconds",
        "gauge",
        "How long its last sweep took.",
        &one(format!("{:.6}", swept.took.as_secs_f64())),
    );
    // cladefs: each counted as its passes ended, none derived from the others, which a refused
    // listing made underflow (board 8509).
    let cells = [
        ("passed", swept.passed),
        ("skipped", swept.report.skipped),
        ("failed", swept.failed),
        ("held", swept.held),
        ("swept", swept.swept),
    ];
    let labelled = |label: &str, samples: &[(&str, String)]| -> Vec<(String, String)> {
        (samples.iter())
            .map(|(name, value)| (format!("{{{label}=\"{name}\"}}"), value.clone()))
            .collect()
    };
    let cells: Vec<(&str, String)> =
        cells.iter().map(|(name, count)| (*name, count.to_string())).collect();
    family(
        "cladefs_gc_sweep_cells",
        "gauge",
        "The cells of its last sweep, by what came of each.",
        &labelled("outcome", &cells),
    );
    let stopped: Vec<(&str, String)> = (Stopped::NAMES.iter())
        .map(|&name| {
            let this = swept.stopped.as_ref().is_some_and(|stopped| stopped.name() == name);
            (name, u8::from(this).to_string())
        })
        .collect();
    family(
        "cladefs_gc_sweep_stopped",
        "gauge",
        "Why its last sweep stopped before it went over every cell, if it did: 1 for the reason.",
        &labelled("reason", &stopped),
    );
    family(
        "cladefs_gc_sweep_capped_passes",
        "gauge",
        "The passes of its last sweep that deleted nothing, past its bounds.",
        &one(swept.capped.to_string()),
    );
    let report = &swept.report;
    let objects = [("deleted", report.deleted.to_string())];
    family(
        "cladefs_gc_sweep_objects",
        "gauge",
        "What its last sweep's passes did, in objects.",
        &labelled("action", &objects),
    );
    let bytes = [("deleted", report.deleted_bytes.to_string())];
    family(
        "cladefs_gc_sweep_bytes",
        "gauge",
        "What its last sweep's passes did, in bytes, as listed.",
        &labelled("action", &bytes),
    );
    family(
        "cladefs_gc_sweep_retired_references",
        "gauge",
        "The references its last sweep's passes retired.",
        &one(report.retired.to_string()),
    );
    family(
        "cladefs_gc_sweep_removed_cells",
        "gauge",
        "The dead cells its last sweep's passes removed whole.",
        &one(report.removed.to_string()),
    );
    let requests: Vec<(&str, String)> = (Request::ALL.into_iter())
        .map(|request| (request.name(), swept.requests.of(request).to_string()))
        .collect();
    family(
        "cladefs_gc_sweep_requests",
        "gauge",
        "The requests its last sweep made, by kind.",
        &labelled("request", &requests),
    );
    let mut buckets: Vec<(String, String)> = (BOUNDS.iter().zip(passes.within))
        .map(|(bound, within)| (format!("_bucket{{le=\"{bound}\"}}"), within.to_string()))
        .collect();
    buckets.push(("_bucket{le=\"+Inf\"}".to_owned(), passes.count.to_string()));
    buckets.push(("_sum".to_owned(), format!("{:.6}", passes.sum)));
    buckets.push(("_count".to_owned(), passes.count.to_string()));
    family(
        "cladefs_gc_pass_seconds",
        "histogram",
        "How long its last sweep's passes took.",
        &buckets,
    );
    let slowest: Vec<(String, String)> = (swept.slowest.iter())
        .map(|(cell, took)| (format!("{{cell=\"{cell}\"}}"), format!("{:.6}", took.as_secs_f64())))
        .collect();
    family(
        "cladefs_gc_slowest_pass_seconds",
        "gauge",
        "Its last sweep's slowest passes, by cell.",
        &slowest,
    );
    family(
        "cladefs_gc_peak_resident_bytes",
        "gauge",
        "The most memory the process held so far.",
        &one(peak.to_string()),
    );
    text
}

/// The most memory the process held so far, in bytes (`getrusage`'s maximum resident set size).
fn peak_resident() -> u64 {
    // SAFETY: `getrusage` writes the `rusage` it is given, which is plain data, all of it valid
    // zeroed.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let max = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    // In bytes on macOS, in KiB elsewhere.
    if cfg!(target_os = "macos") { max } else { max * 1024 }
}

/// Writes `text` to `file` whole: to a file beside it first, then renamed over it, so that a
/// reader never finds it half written.
fn write_atomically(file: &std::path::Path, text: &str) -> std::io::Result<()> {
    let mut beside = file.as_os_str().to_owned();
    beside.push(".new");
    std::fs::write(&beside, text)?;
    std::fs::rename(&beside, file)
}

/// What a dry run prints of what it found of a cell, `found`: each object a pass would delete, a
/// line of its key, kind, size and which, then each reference it would retire, for standard
/// output.
fn dry_lines(found: &DryRun) -> String {
    let line = |meta: &Meta, which| {
        format!("{} {} {} {which}\n", meta.key, layout::kind(&meta.key).name(), meta.size)
    };
    (found.doomed.iter().map(|meta| line(meta, "delete")))
        .chain(found.retired.iter().map(|key| format!("{key} reference - retire\n")))
        .collect()
}

/// cladefs: what a dry run found of its cells so far, counted, holding none of it (board 8513).
#[derive(Default)]
struct DrySum {
    doomed: Sum,
    retired: u64,
    fenced: bool,
}

impl DrySum {
    /// Counts in what it found of a cell, `found`.
    fn add(&mut self, found: &DryRun) {
        let sum = |metas: &[Meta]| Sum {
            objects: metas.len() as u64,
            bytes: metas.iter().map(|meta| meta.size).sum(),
        };
        self.doomed += sum(&found.doomed);
        self.retired += found.retired.len() as u64;
        self.fenced |= found.fenced;
    }

    /// What a dry run says of them at its end, on standard error: their counts and bytes.
    fn line(&self) -> String {
        let Self { doomed, retired, fenced } = self;
        let mut line = format!(
            "cladefs-gc: dry run: a pass would delete {} objects ({} bytes), and retire {retired} \
             references",
            doomed.objects, doomed.bytes,
        );
        if *fenced {
            line += "; a pass took a cell's collector lease meanwhile, deleting what this read may \
                     name: run it again";
        }
        line
    }
}

/// cladefs: logs what is as grave as a warning on standard error, a line each, its key-values after
/// its message: what readers of the bucket and the collector warn of (TASK-264).
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        struct Pairs(String);
        impl<'kvs> log::kv::VisitSource<'kvs> for Pairs {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'kvs>,
                value: log::kv::Value<'kvs>,
            ) -> Result<(), log::kv::Error> {
                self.0 += &format!(" {key}={value}");
                Ok(())
            }
        }
        if self.enabled(record.metadata()) {
            let mut pairs = Pairs(String::new());
            // Pairs takes every value: the visit fails none.
            let _ = record.key_values().visit(&mut pairs);
            eprintln!("{} {}{}", record.level(), record.args(), pairs.0);
        }
    }

    fn flush(&self) {}
}

fn main() -> Result<(), String> {
    let Args { url, command } = parse(std::env::args().skip(1))?;
    // cladefs: before any thread starts, which would take them else (board 8513). Nothing asks
    // the others' stops: SIGTERM and SIGINT end them as ever, as they hold no lease.
    let stop = match &command {
        Command::Collect { mode: Mode::Delete, .. } => stop_on_signals()?,
        _ => Arc::default(),
    };
    log::set_logger(&Stderr).map_err(|err| format!("a log: {err}"))?;
    log::set_max_level(log::LevelFilter::Warn);
    // cladefs: its passes' requests of other buckets' cells go there (TASK-633.7): a child's pass
    // retires its reference, under its parent's cell, in its parent's bucket, and a parent's pass
    // reads its children's heads in theirs.
    let resolve = Arc::new(bucket_cell::object_bucket::Registered::from_env());
    let bucket = Arc::new(ObjectBucket::from_url(&url)?.routed(resolve));
    // Of a thread a core: a sweep's passes run on them at once (TASK-395.15).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("a runtime: {err}"))?;
    runtime.block_on(async {
        // cladefs: before anything is written, the probe's object too (board 8511).
        if let Command::DeleteCell { cell, missing: false } = command {
            refuse_missing(&*bucket, cell).await?;
        }
        // A dry run and a sum write nothing, and rely on no condition of a write's.
        if !matches!(command, Command::Collect { mode: Mode::DryRun, .. } | Command::Usage { .. }) {
            probe(&*bucket, rand::random()).await?;
        }
        match command {
            Command::Collect { every, mode, cell, run } => {
                collect(&bucket, every, mode, cell, (run, stop)).await
            }
            Command::Cells { held, cell } => {
                let held = match held {
                    Some(file) => std::fs::read_to_string(&file)
                        .map_err(|err| format!("{}: {err}", file.display()))?,
                    None => String::new(),
                };
                let held = held_cells(&held)?;
                say_cells(&*bucket, cell, &held, &mut std::io::stdout().lock()).await
            }
            Command::DeleteCell { cell, .. } => delete_cell(&*bucket, cell).await,
            Command::DeleteTenant { tenant } => delete_tenant(&bucket, tenant).await,
            Command::RotateMasterKey => rotate_master_key(&bucket).await,
            Command::Usage { tenant } => {
                say_usage(&*bucket, tenant, &mut std::io::stdout().lock()).await
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|arg| arg.to_string()))
    }

    /// Each line written, and how many pages of listings by delimiter `bucket` was asked for by
    /// then.
    struct Lines<'a> {
        bucket: &'a ObjectBucket,
        lines: Vec<(String, u64)>,
        line: Vec<u8>,
    }

    impl std::io::Write for Lines<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            for &byte in bytes {
                if byte != b'\n' {
                    self.line.push(byte);
                    continue;
                }
                let line = String::from_utf8(std::mem::take(&mut self.line)).unwrap();
                let listed = self.bucket.stats().requests(Request::ListDelimited);
                self.lines.push((line, listed));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// TASK-395.8's AC #2: `--usage` holds a page of a tenant's cells at a time, whatever their
    /// number: each page listed only as the lines of the cells before it are written, the leases
    /// read at most [`USAGES_AT_ONCE`] ahead of them, a tenant of more cells than two pages among
    /// others; each cell's line once, in order, then its tenant's.
    #[test]
    fn usage_lists_a_page_of_cells_at_a_time() {
        use bucket_cell::Bucket as _;
        use bucket_cell::bucket::{LIST_PAGE, PutMode};
        let bucket = ObjectBucket::from_url("memory:///").unwrap();
        let many = LIST_PAGE as u64 * 2 + 5;
        let mut cells: Vec<CellId> = (0..many).map(|id| CellId { tenant: 2, id }).collect();
        cells.extend([CellId { tenant: 1, id: 7 }, CellId { tenant: 3, id: 1 }]);
        cells.sort();
        let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio.block_on(async {
            for &cell in &cells {
                let key = layout::head(cell);
                bucket.put(&key, vec![1].into(), PutMode::Create).await.unwrap();
            }
        });
        let mut out = Lines { bucket: &bucket, lines: Vec::new(), line: Vec::new() };
        tokio.block_on(say_usage(&bucket, None, &mut out)).unwrap();
        let cell_lines: Vec<String> = (out.lines.iter())
            .filter(|(line, _)| line.contains(r#""scope":"cell""#))
            .map(|(line, _)| line.clone())
            .collect();
        let expected: Vec<String> = cells.iter().map(|&cell| cell_line(cell, None)).collect();
        assert_eq!(cell_lines, expected);
        // The lines of tenant 2's cells: the tenants' page and tenant 1's cells' page listed before
        // them, then a page of tenant 2's for each [`LIST_PAGE`] cells read, [`USAGES_AT_ONCE`]
        // ahead of their lines at most.
        let tenant = Value::from("2");
        let of_tenant: Vec<u64> = (out.lines.iter())
            .filter(|(line, _)| {
                let line: Value = serde_json::from_str(line).unwrap();
                line["scope"] == "cell" && line["tenant"] == tenant
            })
            .map(|&(_, listed)| listed)
            .collect();
        assert_eq!(of_tenant.len() as u64, many);
        for (index, &listed) in of_tenant.iter().enumerate() {
            let read = (index + USAGES_AT_ONCE - 1).min(many as usize - 1);
            let pages = read / LIST_PAGE + 1;
            assert_eq!(listed, 2 + pages as u64, "the line of tenant 2's cell {index}");
        }
        // Tenants, then tenant 1's cells, tenant 2's in three pages, tenant 3's.
        assert_eq!(bucket.stats().requests(Request::ListDelimited), 6);
    }

    /// TASK-395.5: a pass says what it did, in how long and by what requests, if it deleted or
    /// retired something, or failed; nothing if it did nothing, or left its cell to another.
    #[test]
    fn passes_say_what_they_did_if_anything() {
        let mut requests = Requests::default();
        requests.0[Request::List as usize] = 2;
        let passed = |outcome| Ran {
            cell: CellId { tenant: 1, id: 2 },
            outcome,
            requests,
            took: Duration::from_millis(12),
        };
        let report = Report { deleted: 2, deleted_bytes: 8192, ..Report::default() };
        assert_eq!(
            passed_line(&passed(Outcome::Collected(report))).as_deref(),
            Some(
                "cladefs-gc: cell 1/2: deleted 2 (8192 bytes), retired 0 references, in 0.012 s; \
                 requests: list 2"
            )
        );
        let failed = Outcome::Failed(bucket_cell::Error::Corrupt("x".into()));
        assert_eq!(
            passed_line(&passed(failed)).as_deref(),
            Some("cladefs-gc: collecting cell 1/2: corrupt: x, in 0.012 s; requests: list 2")
        );
        let retiring = Report { retired: 1, skipped: 1, ..Report::default() };
        assert_eq!(
            passed_line(&passed(Outcome::Collected(retiring))).as_deref(),
            Some(
                "cladefs-gc: cell 1/2, unchanged: deleted 0 (0 bytes), retired 1 references, in \
                 0.012 s; requests: list 2"
            )
        );
        assert_eq!(passed_line(&passed(Outcome::Collected(Report::default()))), None);
        let skipped = Report { skipped: 1, ..Report::default() };
        assert_eq!(passed_line(&passed(Outcome::Collected(skipped))), None);
        assert_eq!(passed_line(&passed(Outcome::Left(bucket_cell::Left::Held))), None);
    }

    /// TASK-395.5: `--metrics` writes the last sweep's counts in Prometheus' text format: its cells
    /// by what came of each, why it stopped, if it did, what its passes did, its requests by kind,
    /// how long its passes took, its slowest, and the process's peak memory.
    #[test]
    fn metrics_say_what_the_last_sweep_did() {
        let mut requests = Requests::default();
        requests.0[Request::Get as usize] = 40;
        let swept = Swept {
            visited: 3,
            passed: 2,
            held: 1,
            failed: 1,
            report: Report { deleted: 4, deleted_bytes: 4096, ..Report::default() },
            requests,
            slowest: vec![(CellId { tenant: 1, id: 2 }, Duration::from_millis(250))],
            took: Duration::from_millis(1500),
            ..Swept::default()
        };
        let mut passes = Histogram::default();
        for (millis, outcome) in [
            (250, Outcome::Collected(Report::default())),
            (5, Outcome::Collected(Report::default())),
            (20_000, Outcome::Failed(bucket_cell::Error::Corrupt("x".into()))),
            (1, Outcome::Left(bucket_cell::Left::Held)),
        ] {
            let cell = CellId { tenant: 1, id: 2 };
            let took = Duration::from_millis(millis);
            passes.observe(&Ran { cell, outcome, requests: Requests::default(), took });
        }
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_250);
        let text = metrics(2, &swept, &passes, 64 << 20, now);
        let expected = "\
# HELP cladefs_gc_sweeps_total Sweeps the process ran.
# TYPE cladefs_gc_sweeps_total counter
cladefs_gc_sweeps_total 2
# HELP cladefs_gc_sweep_end_timestamp_seconds When its last sweep ended.
# TYPE cladefs_gc_sweep_end_timestamp_seconds gauge
cladefs_gc_sweep_end_timestamp_seconds 1700000000.250
# HELP cladefs_gc_sweep_seconds How long its last sweep took.
# TYPE cladefs_gc_sweep_seconds gauge
cladefs_gc_sweep_seconds 1.500000
# HELP cladefs_gc_sweep_cells The cells of its last sweep, by what came of each.
# TYPE cladefs_gc_sweep_cells gauge
cladefs_gc_sweep_cells{outcome=\"passed\"} 2
cladefs_gc_sweep_cells{outcome=\"skipped\"} 0
cladefs_gc_sweep_cells{outcome=\"failed\"} 1
cladefs_gc_sweep_cells{outcome=\"held\"} 1
cladefs_gc_sweep_cells{outcome=\"swept\"} 0
# HELP cladefs_gc_sweep_stopped Why its last sweep stopped before it went over every cell, if it did: 1 for the reason.
# TYPE cladefs_gc_sweep_stopped gauge
cladefs_gc_sweep_stopped{reason=\"unlisted\"} 0
cladefs_gc_sweep_stopped{reason=\"unidentified\"} 0
cladefs_gc_sweep_stopped{reason=\"refusing\"} 0
cladefs_gc_sweep_stopped{reason=\"capped\"} 0
cladefs_gc_sweep_stopped{reason=\"asked\"} 0
# HELP cladefs_gc_sweep_capped_passes The passes of its last sweep that deleted nothing, past its bounds.
# TYPE cladefs_gc_sweep_capped_passes gauge
cladefs_gc_sweep_capped_passes 0
# HELP cladefs_gc_sweep_objects What its last sweep's passes did, in objects.
# TYPE cladefs_gc_sweep_objects gauge
cladefs_gc_sweep_objects{action=\"deleted\"} 4
# HELP cladefs_gc_sweep_bytes What its last sweep's passes did, in bytes, as listed.
# TYPE cladefs_gc_sweep_bytes gauge
cladefs_gc_sweep_bytes{action=\"deleted\"} 4096
# HELP cladefs_gc_sweep_retired_references The references its last sweep's passes retired.
# TYPE cladefs_gc_sweep_retired_references gauge
cladefs_gc_sweep_retired_references 0
# HELP cladefs_gc_sweep_removed_cells The dead cells its last sweep's passes removed whole.
# TYPE cladefs_gc_sweep_removed_cells gauge
cladefs_gc_sweep_removed_cells 0
# HELP cladefs_gc_sweep_requests The requests its last sweep made, by kind.
# TYPE cladefs_gc_sweep_requests gauge
cladefs_gc_sweep_requests{request=\"get\"} 40
cladefs_gc_sweep_requests{request=\"get-range\"} 0
cladefs_gc_sweep_requests{request=\"head\"} 0
cladefs_gc_sweep_requests{request=\"list\"} 0
cladefs_gc_sweep_requests{request=\"create\"} 0
cladefs_gc_sweep_requests{request=\"update\"} 0
cladefs_gc_sweep_requests{request=\"overwrite\"} 0
cladefs_gc_sweep_requests{request=\"delete\"} 0
cladefs_gc_sweep_requests{request=\"list-delimited\"} 0
cladefs_gc_sweep_requests{request=\"delete-batch\"} 0
# HELP cladefs_gc_pass_seconds How long its last sweep's passes took.
# TYPE cladefs_gc_pass_seconds histogram
cladefs_gc_pass_seconds_bucket{le=\"0.001\"} 0
cladefs_gc_pass_seconds_bucket{le=\"0.01\"} 1
cladefs_gc_pass_seconds_bucket{le=\"0.1\"} 1
cladefs_gc_pass_seconds_bucket{le=\"1\"} 2
cladefs_gc_pass_seconds_bucket{le=\"10\"} 2
cladefs_gc_pass_seconds_bucket{le=\"60\"} 3
cladefs_gc_pass_seconds_bucket{le=\"600\"} 3
cladefs_gc_pass_seconds_bucket{le=\"+Inf\"} 3
cladefs_gc_pass_seconds_sum 20.255000
cladefs_gc_pass_seconds_count 3
# HELP cladefs_gc_slowest_pass_seconds Its last sweep's slowest passes, by cell.
# TYPE cladefs_gc_slowest_pass_seconds gauge
cladefs_gc_slowest_pass_seconds{cell=\"1/2\"} 0.250000
# HELP cladefs_gc_peak_resident_bytes The most memory the process held so far.
# TYPE cladefs_gc_peak_resident_bytes gauge
cladefs_gc_peak_resident_bytes 67108864
";
        assert_eq!(text, expected);
    }

    /// Command lines of no command, or of flags their commands take not, get the usage.
    #[test]
    fn other_command_lines_get_the_usage() {
        let lines: [&[&str]; 32] = [
            &[],
            &["u", "--every"],
            &["u", "--every", "soon"],
            &["u", "--fast", "1"],
            &["u", "--delete-cell", "cell"],
            &["u", "--delete-cell", "ff"],
            &["u", "--cells", "--held"],
            &["u", "--mark-only", "--dry-run"],
            &["u", "--mark-only", "--mark-only"],
            &["u", "--dry-run", "--every", "30"],
            &["u", "--mark-only", "1"],
            &["u", "--usage", "--dry-run"],
            &["u", "--dry-run", "--usage"],
            &["u", "--cell"],
            &["u", "--cell", "1"],
            &["u", "--cell", "1/2/3"],
            &["u", "--cells", "--cell", "X/2"],
            &["u", "--cells", "--cell", "1/2", "--cell", "1/2"],
            &["u", "--usage", "--cell", "1/2"],
            &["u", "--usage", "--tenant"],
            &["u", "--usage", "--tenant", "X"],
            &["u", "--usage", "--tenant", "1/2"],
            &["u", "--usage", "--tenant", "10000000000000000"],
            &["u", "--tenant", "1", "--usage"],
            &["u", "--at-once", "many"],
            &["u", "--lease"],
            &["u", "--metrics"],
            &["u", "--metrics", "a", "--metrics", "b"],
            &["u", "--dry-run", "--at-once", "2"],
            &["u", "--dry-run", "--metrics", "a"],
            &["u", "--rotate-master-key", "now"],
            &["u", "--rotate-master-key", "--dry-run"],
        ];
        for args in lines {
            assert_eq!(parsed(args), Err(usage()), "{args:?}");
        }
    }

    /// board 8511's AC #1: `--lease` and `--every` take finite counts of seconds, from a
    /// millisecond to a year: anything else is a usage error, never a panic or a count cast into
    /// range; and the counts and rates of the other flags likewise.
    #[test]
    fn counts_out_of_range_are_usage_errors() {
        let parsed = |flags: &[&str]| {
            let args = std::iter::once("URL").chain(flags.iter().copied()).map(String::from);
            parse(args).map(|args| args.command)
        };
        let refused = ["-1", "-inf", "NaN", "inf", "1e30", "31536001", "one", ""];
        for flag in ["--lease", "--every"] {
            for value in refused {
                assert_eq!(parsed(&[flag, value]), Err(usage()), "{flag} {value}");
            }
        }
        for flags in [
            &["--lease", "0"][..],
            &["--every", "0"],
            &["--lease", "0.0001"],
            &["--every", "0.0001"],
            &["--at-once", "0"],
            &["--max-delete-objects", "0"],
            &["--max-delete-objects", "-1"],
            &["--max-delete-bytes", "0"],
            &["--max-requests-per-second", "0"],
            &["--max-requests-per-second", "NaN"],
            &["--max-requests-per-second", "inf"],
            &["--max-requests-per-second", "1e7"],
            &["--max-refused-passes", "0"],
            &["--dry-run", "--max-delete-objects", "1"],
            &["--cell", "1/1", "--cell", "1/2"],
        ] {
            assert_eq!(parsed(flags), Err(usage()), "{flags:?}");
        }
        let collect = |flags: &[&str]| match parsed(flags) {
            Ok(Command::Collect { every, run, .. }) => (every, run),
            parsed => panic!("{flags:?}: {parsed:?}"),
        };
        let (every, run) = collect(&["--every", "1.5", "--lease", "0.001"]);
        assert_eq!((every, run.lease), (Some(Duration::from_millis(1500)), 1));
        let year = MOST_SECONDS.to_string();
        let (every, run) = collect(&["--every", &year, "--lease", &year]);
        let year = Duration::from_secs(MOST_SECONDS);
        assert_eq!((every, run.lease), (Some(year), millis(year)));
        let (_, run) = collect(&[
            "--max-delete-objects",
            "5",
            "--max-delete-bytes",
            "1000",
            "--max-requests-per-second",
            "0.5",
            "--max-refused-passes",
            "3",
        ]);
        let bounds = (run.max_deleted_objects, run.max_deleted_bytes, run.requests_per_second);
        assert_eq!((bounds, run.refused_in_a_row), ((Some(5), Some(1000), Some(0.5)), 3));
        assert_eq!(collect(&[]).1, Run::default());
    }

    /// board 8509's AC #1 and #2: `--metrics` of a sweep whose listing the bucket refused, which
    /// went over no cell, says no cell passed, none failed, and why it stopped; of counts no sweep
    /// gives, failed passes past those it ran, it says them as they are: nothing is derived from
    /// them, to underflow, as `passed` was, `visited - failed - skipped`.
    #[test]
    fn metrics_say_each_outcome_as_counted() {
        use bucket_cell::bucket::{Refusal, Refused};
        let refused = Refused { key: "tenants/".into(), refusal: Refusal::Denied };
        let unlisted = Swept { stopped: Some(Stopped::Unlisted(refused)), ..Swept::default() };
        let odd = Swept {
            visited: 1,
            failed: 3,
            report: Report { skipped: 2, ..Report::default() },
            capped: 4,
            stopped: Some(Stopped::Capped),
            ..Swept::default()
        };
        let cells = |outcomes: [usize; 5]| {
            let names = ["passed", "skipped", "failed", "held", "swept"];
            (names.iter().zip(outcomes))
                .map(|(name, count)| {
                    format!("cladefs_gc_sweep_cells{{outcome=\"{name}\"}} {count}")
                })
                .collect::<Vec<_>>()
        };
        let stopped = |reason: &str| {
            (Stopped::NAMES.iter())
                .map(|name| {
                    let this = u8::from(*name == reason);
                    format!("cladefs_gc_sweep_stopped{{reason=\"{name}\"}} {this}")
                })
                .collect::<Vec<_>>()
        };
        let cases = [
            (unlisted, cells([0, 0, 0, 0, 0]), stopped("unlisted"), 0),
            (odd, cells([0, 2, 3, 0, 0]), stopped("capped"), 4),
        ];
        for (swept, cells, stopped, capped) in cases {
            let text = metrics(1, &swept, &Histogram::default(), 0, SystemTime::UNIX_EPOCH);
            let capped = format!("cladefs_gc_sweep_capped_passes {capped}");
            for line in cells.iter().chain(&stopped).chain([&capped]) {
                assert!(text.lines().any(|had| had == line), "{line} in {text}");
            }
            assert!(ended(&swept).is_err(), "{swept:?}");
        }
    }

    /// board 8513's AC #2: `--cells` says each live cell as it reads its head, a page of cells
    /// listed at a time: a cell's line written before the page after its own is listed, whatever
    /// the cells' number; but those held, whose heads it reads not.
    #[test]
    fn cells_say_each_cell_as_they_read_it() {
        use bucket_cell::bucket::LIST_PAGE;
        let bucket = ObjectBucket::from_url("memory:///").unwrap();
        let many = LIST_PAGE as u64 + 5;
        let cells: Vec<CellId> = (0..many).map(|id| CellId { tenant: 2, id }).collect();
        let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio.block_on(async {
            for &cell in &cells {
                lifecycle::create(&bucket, cell, 0).await.unwrap();
            }
        });
        let held: BTreeSet<CellId> = [cells[1], cells[LIST_PAGE + 1]].into();
        let mut out = Lines { bucket: &bucket, lines: Vec::new(), line: Vec::new() };
        tokio.block_on(say_cells(&bucket, None, &held, &mut out)).unwrap();
        let expected: Vec<(String, u64)> = (cells.iter().enumerate())
            .filter(|(_, cell)| !held.contains(*cell))
            .map(|(index, cell)| (format!("{cell}"), 1 + (index / LIST_PAGE) as u64 + 1))
            .collect();
        assert_eq!(out.lines, expected);
    }
}
