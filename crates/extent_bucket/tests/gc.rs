// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs-gc, the program: a pass over a bucket, and its usage. cladefs: every store encrypted
//! (TASK-411.28), the program given a master key, its bucket KMS keeping the tests' tenants' KEKs
//! in the bucket ([`MASTER_KEY`]), as their writers' are ([`host_kms`]).

use std::process::Command;
use std::sync::Arc;

/// The master key the program and the tests' writers are given, in hexadecimal.
const MASTER_KEY: &str = "7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e";

/// A host's KMS over `bucket`, as the program's environment chooses it: the bucket KMS, given
/// [`MASTER_KEY`], which the tests' writers encrypt under.
fn host_kms<B: extent_bucket::Bucket>(bucket: &Arc<B>) -> Arc<extent_bucket::keys::CachingKms> {
    use extent_keys::kms::master::MasterKeys;
    let key = extent_keys::Secret::from_bytes(&[0x7e; 32]);
    let keys = MasterKeys::new(vec![key]).unwrap();
    let kms = extent_bucket::bucket_kms::BucketKms::new(bucket.clone(), keys);
    extent_bucket::keys::CachingKms::new(Arc::new(kms))
}

/// The tests' cell `id`, of tenant 1.
fn test_branch(id: u64) -> extent_bucket::layout::CellId {
    extent_bucket::layout::CellId { tenant: 1, id }
}

fn gc(args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
    command.env_remove("CLADEFS_KMS").env_remove("CLADEFS_MASTER_KEY_FILE");
    command.env("CLADEFS_MASTER_KEY", MASTER_KEY).args(args).output().unwrap()
}

/// What `out` said on standard error, its times, which vary, as `T`, and the requests it says it
/// made, which its unit tests check, as `R`.
fn said(out: &std::process::Output) -> String {
    let words = |line: &str| -> String {
        let timed =
            line.split(' ').map(|word| match word.contains('.') && word.parse::<f64>().is_ok() {
                true => "T",
                false => word,
            });
        timed.collect::<Vec<_>>().join(" ")
    };
    (String::from_utf8_lossy(&out.stderr).lines())
        .map(|line| match line.find("; requests: ") {
            Some(at) => format!("{}; requests: R\n", words(&line[..at])),
            None => format!("{}\n", words(line)),
        })
        .collect()
}

/// What a sweep that deleted `deleted` objects of `bytes` bytes says of its passes over `cells`
/// cells, none unchanged since a pass before, none failed, none left to others; then how long it
/// took, and its slowest, `slowest`.
fn swept(deleted: usize, bytes: u64, cells: usize, slowest: &str) -> String {
    format!(
        "cladefs-gc: deleted {deleted} ({bytes} bytes), compacted 0 (0 bytes), retired 0 references, \
         over {cells} cells (0 unchanged), 0 held by other passes, 0 swept by them since, 0 \
         failed; requests: R\ncladefs-gc: took T s; slowest: {slowest}\n"
    )
}

/// A pass over a bucket (in memory: empty) reports what it did, over no cell, and exits.
#[test]
fn collects_a_bucket_once() {
    let out = gc(&["memory:///", "--grace", "1"]);
    assert_eq!(said(&out), swept(0, 0, 0, "none"));
    assert!(out.status.success());
}

/// TASK-411.28: with no KMS, neither `CLADEFS_KMS` nor a master key given, it refuses to collect or
/// to delete a branch, before it touches the bucket, naming both variables: every cell is encrypted,
/// and compacting needs its KEK. What reads no data, a dry run, `--branches`, `--usage`, needs
/// none.
#[test]
fn refuses_to_collect_without_a_kms() {
    let keyless = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
        command.env_remove("CLADEFS_KMS").env_remove("CLADEFS_MASTER_KEY");
        command.env_remove("CLADEFS_MASTER_KEY_FILE").args(args).output().unwrap()
    };
    let refused = format!("Error: {:?}\n", extent_bucket::bucket_kms::no_kms());
    for args in [
        &["memory:///"][..],
        &["memory:///", "--mark-only"],
        &["memory:///", "--delete-branch", "1/1/1"],
    ] {
        let out = keyless(args);
        assert_eq!(String::from_utf8_lossy(&out.stderr), refused, "{args:?}");
        assert_eq!(out.status.code(), Some(1), "{args:?}");
    }
    assert!(refused.contains("CLADEFS_KMS") && refused.contains("CLADEFS_MASTER_KEY"));
    for args in [
        &["memory:///", "--dry-run"][..],
        &["memory:///", "--branches"],
        &["memory:///", "--usage"],
    ] {
        assert!(keyless(args).status.success(), "{args:?}");
    }
}

/// TASK-395.5: `--metrics` writes what the sweep did, in Prometheus' text format, to its file.
#[test]
fn sweeps_write_their_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("gc.prom");
    let out = gc(&["memory:///", "--at-once", "2", "--metrics", file.to_str().unwrap()]);
    assert_eq!(said(&out), swept(0, 0, 0, "none"));
    assert!(out.status.success());
    let metrics = std::fs::read_to_string(&file).unwrap();
    for line in [
        "cladefs_gc_sweeps_total 1",
        "cladefs_gc_sweep_cells{outcome=\"passed\"} 0",
        "cladefs_gc_sweep_cells{outcome=\"skipped\"} 0",
        "cladefs_gc_sweep_requests{request=\"list-delimited\"} 1",
        "cladefs_gc_pass_seconds_count 0",
    ] {
        assert!(metrics.lines().any(|had| had == line), "{line} in {metrics}");
    }
    let peak =
        metrics.lines().find_map(|line| line.strip_prefix("cladefs_gc_peak_resident_bytes "));
    let peak: u64 = peak.unwrap().parse().unwrap();
    assert!((1 << 20..1 << 34).contains(&peak), "a process's memory, at most: {peak}");
}

/// TASK-79.85: a bucket (in memory: empty) has no live branches, and a branch is deleted, as an
/// operator's commands do them.
#[test]
fn lists_and_deletes_branches() {
    let out = gc(&["memory:///", "--branches"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(out.status.success());
    let out = gc(&["memory:///", "--delete-branch", "1/1/ff"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert!(out.status.success());
}

/// cladefs: a pass over a bucket on the host's filesystem (TASK-165.1), from a process of its own:
/// it reclaims the branch's segments its checkpoint covers, which hold no live extent (the one
/// holding what it freed, the one freeing it), and lists the branch.
#[test]
fn collects_a_file_bucket() {
    use extent_bucket::batch::{Commit, SegmentBuilder};
    use extent_bucket::object_bucket::ObjectBucket;
    use extent_bucket::{Writer, branch};
    use storage_device::ExtentId;

    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::from_directory_path(dir.path()).unwrap().to_string();
    futures::executor::block_on(async {
        let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let kms = host_kms(&bucket);
        let writer = Writer::open_under(bucket, test_branch(1), (10, 1), &kms).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let written =
            Commit { tag: 1, extents: vec![(extent, vec![7; 4096])], ..Commit::default() };
        let freed = Commit { tag: 2, frees: vec![extent], ..Commit::default() };
        for commit in [written, freed] {
            let framed = SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            let segment = writer.add_segment(framed).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
        }
        writer.checkpoint().await.unwrap();
    });
    // The two segments a pass deletes, and their bytes.
    let at = "tenants/0000000000001/cells/0000000000001";
    let doomed = [0, 1].map(|segment| {
        format!(
            "{at}/segments/0000000000001/0000000000001.{segment}",
            segment = base32_id::Padded(segment)
        )
    });
    let bytes: u64 = futures::executor::block_on(async {
        use extent_bucket::Bucket as _;
        let listed = ObjectBucket::from_url(&url).unwrap().list("").await.unwrap();
        listed.iter().filter(|meta| doomed.contains(&meta.key)).map(|meta| meta.size).sum()
    });
    // TASK-395.4: a pass over another cell, named, leaves this one be.
    let out = gc(&[&url, "--cell", "1/2", "--grace", "0"]);
    assert_eq!(said(&out), swept(0, 0, 1, "1/2 T s"));
    // Over the one cell there: the other's pass, of a cell holding nothing, took no lease there.
    let out = gc(&[&url, "--grace", "0"]);
    assert_eq!(
        said(&out),
        format!(
            "cladefs-gc: cell 1/1: deleted 2 ({bytes} bytes), compacted 0 (0 bytes), retired 0 \
             references, in T s; requests: R\n{}",
            swept(2, bytes, 1, "1/1 T s")
        )
    );
    assert!(out.status.success());
    let left = futures::executor::block_on(async {
        use extent_bucket::Bucket as _;
        let bucket = ObjectBucket::from_url(&url).unwrap();
        let listed = bucket.list("").await.unwrap();
        listed
            .into_iter()
            .map(|meta| meta.key)
            .filter(|key| !key.ends_with("/gc/lease"))
            .collect::<Vec<_>>()
    });
    let left: Vec<&str> = left.iter().map(String::as_str).collect();
    let at = "tenants/0000000000001/cells/0000000000001";
    let branch = "0000000000001";
    // Its head, which names its checkpoint (TASK-407); cladefs: its cell's object, keeping its
    // KEK, and its tenant's KEK object, the bucket KMS's (TASK-411.28).
    let kept = [
        format!("{at}/branches/{branch}"),
        format!("{at}/cell"),
        format!("{at}/checkpoints/{branch}/0000000000001.0000000000001"),
        format!("{at}/heads/{branch}"),
        "tenants/0000000000001/kek".to_owned(),
    ];
    assert_eq!(left, kept);
    let out = gc(&[&url, "--branches"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1/1/1\n");
    let out = gc(&[&url, "--branches", "--cell", "1/1"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1/1/1\n");
    let out = gc(&[&url, "--branches", "--cell", "1/2"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

/// TASK-264: a dry run over a bucket on the host's filesystem lists what a pass would delete,
/// writing nothing, the collector's lease neither, nor the probe of its conditional writes; a
/// mark-only pass marks it, deleting nothing; a pass after deletes it, leaving the marks.
#[test]
fn dry_runs_and_mark_only_passes_delete_nothing() {
    use extent_bucket::batch::{Commit, SegmentBuilder};
    use extent_bucket::object_bucket::ObjectBucket;
    use extent_bucket::{Writer, branch};
    use storage_device::ExtentId;

    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::from_directory_path(dir.path()).unwrap().to_string();
    futures::executor::block_on(async {
        let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let kms = host_kms(&bucket);
        let writer = Writer::open_under(bucket, test_branch(1), (10, 1), &kms).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let written =
            Commit { tag: 1, extents: vec![(extent, vec![7; 4096])], ..Commit::default() };
        let freed = Commit { tag: 2, frees: vec![extent], ..Commit::default() };
        for commit in [written, freed] {
            let framed = SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            let segment = writer.add_segment(framed).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
        }
        writer.checkpoint().await.unwrap();
    });
    let files = || -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = walk(dir.path())
            .into_iter()
            .map(|path| {
                let name = path.strip_prefix(dir.path()).unwrap().display().to_string();
                (name, std::fs::read(&path).unwrap())
            })
            .collect();
        files.sort();
        files
    };
    let before = files();
    let out = gc(&[&url, "--dry-run", "--grace", "0"]);
    let branch = "0000000000001";
    let segment = |number: u64| {
        format!(
            "tenants/{branch}/cells/{branch}/segments/{branch}/0000000000001.{number}",
            number = base32_id::Padded(number)
        )
    };
    let (holding, freeing) = (segment(0), segment(1));
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let sizes: Vec<&str> = listed.lines().map(|line| line.split(' ').nth(2).unwrap()).collect();
    let expected: String = [&holding, &freeing]
        .iter()
        .zip(&sizes)
        .map(|(key, size)| format!("{key} segment {size} delete\n"))
        .collect();
    assert_eq!(listed, expected);
    let bytes: u64 = sizes.iter().map(|size| size.parse::<u64>().unwrap()).sum();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "cladefs-gc: dry run: a pass would delete 2 objects ({bytes} bytes), and more: it would \
             compact 0 (0 bytes), and delete what it compacts a grace period after, which a dry \
             run, compacting nothing, doesn't count; and retire 0 references\n"
        )
    );
    assert!(out.status.success());
    assert_eq!(files(), before);

    let out = gc(&[&url, "--mark-only", "--grace", "0"]);
    let at = "tenants/0000000000001/cells/0000000000001";
    let marks = format!("{at}/gc/marks/0000000000001");
    assert_eq!(
        said(&out),
        format!(
            "cladefs-gc: cell 1/1: marked 2 ({bytes} bytes) in {marks}, deleted 0, compacted 0, \
             resurrected 0, in T s; requests: R\n\
             cladefs-gc: marked 2 ({bytes} bytes), compacted 0 (0 bytes), resurrected 0, over 1 \
             cells (0 unchanged), 0 held by other passes, 0 swept by them since, 0 failed; \
             requests: R\n\
             cladefs-gc: took T s; slowest: 1/1 T s\n"
        )
    );
    let names: Vec<String> = files().into_iter().map(|(name, _)| name).collect();
    for key in [&holding, &freeing] {
        assert!(names.contains(key), "{key} marked, deleted");
    }
    assert!(names.contains(&marks), "{names:?}");

    let out = gc(&[&url, "--grace", "0"]);
    assert_eq!(
        said(&out),
        format!(
            "cladefs-gc: cell 1/1: deleted 2 ({bytes} bytes), compacted 0 (0 bytes), retired 0 \
             references, in T s; requests: R\n{}",
            swept(2, bytes, 1, "1/1 T s")
        )
    );
    // The bucket's locks, no objects, aside (TASK-395.15: one a directory's, local.rs's tests);
    // the objects, the collector's under the cell's prefix.
    let names: Vec<String> = (files().into_iter())
        .map(|(name, _)| name)
        .filter(|name| !name.starts_with(extent_bucket::local::LOCK))
        .collect();
    let kept = [
        // The word that the bucket is marked, which an operator removes with the marks.
        extent_bucket::layout::MARKING.to_owned(),
        format!("{at}/branches/{branch}"),
        format!("{at}/cell"),
        format!("{at}/checkpoints/{branch}/0000000000001.0000000000001"),
        format!("{at}/gc/lease"),
        marks,
        format!("{at}/heads/{branch}"),
        "tenants/0000000000001/kek".to_owned(),
    ];
    assert_eq!(names, kept);
}

/// The files under `dir`, at every depth.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => files.extend(walk(&path)),
            false => files.push(path),
        }
    }
    files
}

/// TASK-394, TASK-395.8: `--usage` over a bucket on the host's filesystem says what each cell
/// takes, by kind, as the last pass over it found it, which matches a listing of the cell after it:
/// what the branches wrote, less what the pass deleted; when the pass listed it; nothing of a cell
/// no pass went over; and each tenant's sum after its cells. `--tenant` says one tenant's. It
/// writes nothing, the probe of conditional writes neither.
#[test]
fn usage_says_what_each_files_cell_and_tenant_takes_as_passes_found() {
    use extent_bucket::Bucket as _;
    use extent_bucket::batch::{Commit, SegmentBuilder};
    use extent_bucket::layout::{self, CellId};
    use extent_bucket::object_bucket::ObjectBucket;
    use extent_bucket::{Writer, branch};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use storage_device::ExtentId;

    let dir = tempfile::tempdir().unwrap();
    let url = url::Url::from_directory_path(dir.path()).unwrap().to_string();
    let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
    let cells = [
        CellId { tenant: 1, id: 1 },
        CellId { tenant: 1, id: 0x2f },
        CellId { tenant: 0xb, id: 1 },
    ];
    let unpassed = CellId { tenant: 1, id: 0x30 };
    futures::executor::block_on(async {
        for (id, cell) in cells.into_iter().enumerate() {
            branch::create(&*bucket, CellId { tenant: cell.tenant, id: id as u64 + 1 }, 0)
                .await
                .unwrap();
        }
        // In the first cell, a segment written and one freeing it: what the pass deletes.
        let kms = host_kms(&bucket);
        let branch = CellId { tenant: (cells[0]).tenant, id: 1 };
        let writer = Writer::open_under(bucket.clone(), branch, (10, 1), &kms).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let written =
            Commit { tag: 1, extents: vec![(extent, vec![7; 4096])], ..Commit::default() };
        let freed = Commit { tag: 2, frees: vec![extent], ..Commit::default() };
        for commit in [written, freed] {
            let framed = SegmentBuilder::from(commit)
                .close(&mut writer.chain())
                .frame(writer.keys().encrypting());
            let segment = writer.add_segment(framed).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
        }
        writer.checkpoint().await.unwrap();
    });
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let before = now().as_millis() as u64;
    let out = gc(&[&url, "--grace", "0"]);
    let said = said(&out);
    // The slowest passes, last, in an order of their times.
    let (did, slowest) = said.split_at(said.find("cladefs-gc: took").unwrap());
    assert_eq!(
        did,
        "cladefs-gc: cell 1/1: deleted 2 (612 bytes), compacted 0 (0 bytes), retired 0 references, \
         in T s; requests: R\ncladefs-gc: deleted 2 (612 bytes), compacted 0 (0 bytes), retired 0 \
         references, over 3 cells (0 unchanged), 0 held by other passes, 0 swept by them since, 0 \
         failed; requests: R\n"
    );
    assert!(slowest.starts_with("cladefs-gc: took T s; slowest: "), "{slowest}");
    let after = now().as_millis() as u64;
    // A cell made after the pass, which no pass went over.
    futures::executor::block_on(branch::create(
        &*bucket,
        CellId { tenant: unpassed.tenant, id: 4 },
        0,
    ))
    .unwrap();
    let listed = futures::executor::block_on(bucket.list("")).unwrap();

    // Each cell's objects and bytes by kind, as the bucket lists them now.
    let sums = |cell: CellId| {
        let mut kinds: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for meta in listed.iter().filter(|meta| meta.key.starts_with(&cell.prefix())) {
            let sum = kinds.entry(layout::kind(&meta.key).name()).or_default();
            *sum = (sum.0 + 1, sum.1 + meta.size);
        }
        kinds
    };
    let kinds_json = |kinds: &BTreeMap<&str, (u64, u64)>| -> Value {
        let kind = |&(objects, bytes): &(u64, u64)| json!({ "objects": objects, "bytes": bytes });
        kinds.iter().map(|(name, sum)| (name.to_string(), kind(sum))).collect()
    };
    let total = |kinds: &BTreeMap<&str, (u64, u64)>| {
        kinds.values().fold((0, 0), |(objects, bytes), sum| (objects + sum.0, bytes + sum.1))
    };
    let lines = |out: &std::process::Output| -> Vec<Value> {
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        stdout.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    };
    let out = lines(&gc(&[&url, "--usage"]));
    // What each line says when it was measured: the pass's listing's start, which it went over.
    let measured: Vec<Value> = out.iter().map(|line| line["measured_unix_ms"].clone()).collect();
    for when in [&measured[0], &measured[1], &measured[4]] {
        let when = when.as_u64().unwrap();
        assert!(before <= when && when <= after, "{before} {when} {after}");
    }
    let cell_line = |cell: CellId, when: &Value| {
        let kinds = sums(cell);
        let (objects, bytes) = total(&kinds);
        json!({
            "scope": "cell",
            "tenant": base32_id::Short(cell.tenant).to_string(),
            "cell": cell.to_string(),
            "measured_unix_ms": when,
            "objects": objects,
            "bytes": bytes,
            "kinds": kinds_json(&kinds),
        })
    };
    let tenant_line = |tenant: u64, of: &[CellId], when: &[&Value], unmeasured: u64| {
        let mut kinds: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for cell in of {
            for (name, sum) in sums(*cell) {
                let summed = kinds.entry(name).or_default();
                *summed = (summed.0 + sum.0, summed.1 + sum.1);
            }
        }
        let (objects, bytes) = total(&kinds);
        let when: Vec<u64> = when.iter().map(|when| when.as_u64().unwrap()).collect();
        json!({
            "scope": "tenant",
            "tenant": base32_id::Short(tenant).to_string(),
            "cells": of.len() as u64 + unmeasured,
            "measured_cells": of.len(),
            "oldest_measured_unix_ms": when.iter().min(),
            "newest_measured_unix_ms": when.iter().max(),
            "objects": objects,
            "bytes": bytes,
            "kinds": kinds_json(&kinds),
        })
    };
    let nothing = json!({
        "scope": "cell",
        "tenant": "1",
        "cell": "1/1g",
        "measured_unix_ms": null,
        "objects": null,
        "bytes": null,
        "kinds": null,
    });
    let tenant_b =
        [cell_line(cells[2], &measured[4]), tenant_line(0xb, &cells[2..], &[&measured[4]], 0)];
    let expected = [
        cell_line(cells[0], &measured[0]),
        cell_line(cells[1], &measured[1]),
        nothing,
        tenant_line(1, &cells[..2], &[&measured[0], &measured[1]], 1),
        tenant_b[0].clone(),
        tenant_b[1].clone(),
    ];
    assert_eq!(out, expected);
    // What the pass deleted is gone from it: the first cell's segments the checkpoint covers, all
    // of them, its head naming none after it (TASK-407).
    assert_eq!(out[0]["kinds"]["segment"], Value::Null, "{}", out[0]);
    assert_eq!(out[0]["kinds"]["head"]["objects"], 1, "{}", out[0]);
    assert_eq!(out[0]["kinds"]["gc-lease"]["objects"], 1, "{}", out[0]);
    assert_eq!(lines(&gc(&[&url, "--usage", "--tenant", "b"])), tenant_b);
    let after = futures::executor::block_on(bucket.list("")).unwrap();
    assert_eq!(after, listed);
}

/// TASK-411.14: `--delete-branch` of an encrypted branch whose fork into another cell has no record
/// yet, its writer dead between the fork's acknowledgement and recording it, records the fork, its
/// checkpoint at the origin authenticated under the branch's DEK, its cell's KEK unwrapped by
/// the KMS `CLADEFS_KMS` names. TASK-411.28: without a KMS, it refuses to start, naming
/// `CLADEFS_KMS` and `CLADEFS_MASTER_KEY`, the branch as it was; run again with one, it deletes it.
#[test]
fn deletes_encrypted_branches_recording_their_forks_into_other_cells_under_the_kms() {
    use extent_bucket::batch::{Commit, SegmentBuilder};
    use extent_bucket::layout::{self, CellId, Child, Origin};
    use extent_bucket::object_bucket::ObjectBucket;
    use extent_bucket::{Bucket as _, Writer, branch, cell, keys};
    use extent_frame::{Encryption, Frame, KeyId};
    use extent_keys::kms::LocalKms;
    use futures::executor::block_on;
    use std::sync::Arc;
    use storage_device::ExtentId;

    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("bucket");
    std::fs::create_dir(&files).unwrap();
    let url = url::Url::from_directory_path(&files).unwrap().to_string();
    let kms_dir = dir.path().join("kms");
    let local = Arc::new(LocalKms::open(&kms_dir).unwrap());
    local.create(1).unwrap();
    let parent = test_branch(1);
    let child = CellId { tenant: 1, id: 9 };
    let at = block_on(async {
        let bucket = Arc::new(ObjectBucket::from_url(&url).unwrap());
        let shared: extent_keys::kms::Shared = local.clone();
        let host = keys::CachingKms::new(shared);
        let kek = keys::new_cell_kek(&host, 1).await.unwrap();
        let making = Some(&kek.1);
        let (_, object) = cell::hold(&*bucket, parent, (5, 5), making, None).await.unwrap();
        let cell_keys = keys::held(&host, parent, object, Some(kek));
        branch::create(&*bucket, parent, 0).await.unwrap();
        let record = branch::record(&*bucket, parent).await.unwrap().unwrap();
        let record = keys::with_dek(&*bucket, &cell_keys, parent, record).await.unwrap();
        let ring = keys::ring(&*bucket, &cell_keys, parent, &record, true).await.unwrap();
        let writer = Writer::open_keyed(bucket.clone(), parent, (10, 1), ring.clone());
        let writer = writer.await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let written =
            Commit { tag: 1, extents: vec![(extent, vec![7; 4096])], ..Commit::default() };
        let written = SegmentBuilder::from(written).close(&mut writer.chain());
        let segment = writer.add_segment(written.frame(ring.encrypting())).unwrap();
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
        // The fork's segment uploaded, and the head swapped to name it, as its acknowledgement
        // swaps it (TASK-407); its writer dies before it records the child.
        let forking = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let forked = Child { branch: child, owner: 0x109 };
        let forking = forking.fork(forked, &mut writer.chain());
        let at = writer.add_segment(forking.frame(ring.encrypting())).unwrap();
        writer.upload(at).await.unwrap();
        let head = extent_bucket::head::get(&*bucket, parent).await.unwrap().unwrap();
        let named = layout::encode(&head.holding(writer.epoch(), at + 1));
        let overwrite = extent_bucket::bucket::PutMode::Overwrite;
        bucket.put(&layout::head(parent), named.into(), overwrite).await.unwrap();
        at
    });
    let bucket = ObjectBucket::from_url(&url).unwrap();
    let record = |branch| block_on(branch::record(&bucket, branch)).unwrap();
    let checkpoints = || {
        let listed = block_on(bucket.list(&format!("{}checkpoints/", parent.prefix())));
        listed.unwrap().into_iter().map(|meta| meta.key).collect::<Vec<_>>()
    };
    assert_eq!(record(child), None);
    let delete = |kms: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
        command.args([url.as_str(), "--delete-branch", "1/1/1"]).env_remove("CLADEFS_KMS");
        command.env_remove("CLADEFS_MASTER_KEY").env_remove("CLADEFS_MASTER_KEY_FILE");
        if let Some(kms) = kms {
            command.env("CLADEFS_KMS", format!("local://{}", kms.display()));
        }
        command.output().unwrap()
    };

    let out = delete(None);
    let refused = extent_bucket::bucket_kms::no_kms();
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("Error: {refused:?}\n"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(record(parent).map(|record| record.deleted), Some(false));
    assert_eq!(record(child), None);
    assert_eq!(checkpoints(), Vec::<String>::new());

    let out = delete(Some(&kms_dir));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert!(out.status.success());
    assert_eq!(record(parent).map(|record| record.deleted), Some(true));
    assert_eq!(record(child).and_then(|record| record.origin()), Some(Origin { parent, at }));
    let checkpoint = layout::checkpoint(parent, at, 1);
    assert_eq!(checkpoints(), std::slice::from_ref(&checkpoint));
    let bytes = block_on(bucket.get(&checkpoint)).unwrap().bytes;
    let encryption = Frame::of(&bytes).unwrap().encryption;
    let Encryption::Authenticated { key, .. } = encryption else {
        panic!("{checkpoint}: {encryption:?}")
    };
    assert_eq!(key, KeyId { branch: 1, generation: keys::FIRST });
}

/// TASK-411.27: `--rotate-master-key` rewraps every tenant's KEK object under the newest master
/// key `CLADEFS_MASTER_KEY` gives, the first, saying what it did; a host given the newest alone then
/// unwraps what was wrapped before. Done again, it rewraps nothing. Given keys none of which opens
/// an object, it leaves it, says so, and fails; given none, it fails naming the variables.
#[test]
fn rotates_master_keys_rewrapping_every_tenants_kek() {
    use extent_bucket::bucket_kms::{self, BucketKms};
    use extent_bucket::object_bucket::ObjectBucket as Store;
    use extent_keys::Secret;
    use extent_keys::kms::Kms as _;
    use extent_keys::kms::master::MasterKeys;
    use futures::executor::block_on;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("bucket");
    std::fs::create_dir(&files).unwrap();
    let url = url::Url::from_directory_path(&files).unwrap().to_string();
    let hex =
        |key: &Secret| key.expose().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let (old, new) = (Secret::random(), Secret::random());
    let keys = |given: &[&Secret]| {
        MasterKeys::new(given.iter().map(|key| Secret::from_bytes(key.expose())).collect()).unwrap()
    };
    let bucket = Arc::new(Store::from_url(&url).unwrap());
    let before = BucketKms::new(bucket.clone(), keys(&[&old]));
    let key = Secret::random();
    let wrapped: Vec<_> =
        (1..=3).map(|tenant| block_on(before.wrap(tenant, &key)).unwrap()).collect();
    block_on(bucket_kms::destroy(&*bucket, 3));
    let rotate = |given: Option<String>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
        command.args([url.as_str(), "--rotate-master-key"]);
        command.env_remove("CLADEFS_MASTER_KEY_FILE").env_remove("CLADEFS_KMS");
        match given {
            Some(given) => command.env("CLADEFS_MASTER_KEY", given),
            None => command.env_remove("CLADEFS_MASTER_KEY"),
        };
        command.output().unwrap()
    };
    let new_id = keys(&[&new]).newest().id();

    let out = rotate(Some(format!("{},{}", hex(&new), hex(&old))));
    let expected = format!(
        "cladefs-gc: master key {new_id}: rewrapped 2 tenants' KEKs, 0 under it already, 1 tenants \
         shredded, 0 refused\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), expected);
    assert!(out.status.success());
    let after = BucketKms::new(bucket.clone(), keys(&[&new]));
    for (tenant, wrapped) in (1..=2).zip(&wrapped) {
        assert_eq!(block_on(after.unwrap(tenant, wrapped)).unwrap().expose(), key.expose());
    }
    let out = rotate(Some(format!("{}\n{}\n", hex(&new), hex(&old))));
    let expected = format!(
        "cladefs-gc: master key {new_id}: rewrapped 0 tenants' KEKs, 2 under it already, 1 tenants \
         shredded, 0 refused\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), expected);

    let other = Secret::random();
    let other_id = keys(&[&other]).newest().id();
    let out = rotate(Some(hex(&other)));
    let left = |tenant| {
        format!(
            "cladefs-gc: left as it is: tenant {tenant}: its KEK is wrapped by master key {new_id}, \
             which this host has not (it has {other_id}): CLADEFS_MASTER_KEY\n"
        )
    };
    let expected = format!(
        "cladefs-gc: master key {other_id}: rewrapped 0 tenants' KEKs, 0 under it already, 1 \
         tenants shredded, 2 refused\n{}{}Error: \"2 tenants' KEK objects left under no master key \
         given: run again with theirs among CLADEFS_MASTER_KEY's\"\n",
        left(1),
        left(2)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), expected);
    assert_eq!(out.status.code(), Some(1));
    for out in [rotate(Some(format!("{},{}", hex(&new), hex(&old)))), rotate(Some(hex(&other)))] {
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        for key in [&old, &new, &other] {
            assert!(!said.contains(&hex(key)), "{said}");
        }
    }

    let out = rotate(None);
    let refused = "--rotate-master-key: neither CLADEFS_MASTER_KEY nor CLADEFS_MASTER_KEY_FILE gives \
                   a master key: the newest first, then those to rotate from";
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("Error: {refused:?}\n"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn without_a_bucket_prints_its_usage() {
    let out = gc(&[]);
    let usage = "usage: cladefs-gc URL [--cell TENANT/CELL] [--mark-only] [--every SECONDS] \
                 [--grace SECONDS] [--compact-below FRACTION] [--at-once PASSES] [--lease SECONDS] \
                 [--metrics FILE] | cladefs-gc URL [--cell TENANT/CELL] \
                 --dry-run [--grace SECONDS] [--compact-below FRACTION] | cladefs-gc URL --branches \
                 [--cell TENANT/CELL] [--held FILE] | cladefs-gc URL --delete-branch \
                 TENANT/CELL/ID | cladefs-gc URL --usage [--tenant TENANT] | cladefs-gc URL \
                 --rotate-master-key (every cell, or the one --cell names; \
                 --mark-only deletes nothing, writing each cell's gc/marks/EPOCH: the bucket \
                 grows; --dry-run writes nothing, and finds less than a pass deletes)";
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("Error: {usage:?}\n"));
    assert_eq!(out.status.code(), Some(1));
}
