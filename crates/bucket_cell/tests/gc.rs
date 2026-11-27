// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs-gc, the program: a pass over a bucket, and its usage. cladefs: every cell encrypted
//! (TASK-411.28) by writers holding the bucket KMS over the tests' master key ([`host_kms`]), and
//! the program given no key: it collects by metadata alone (doc-27).

use std::process::Command;
use std::sync::Arc;

/// A host's KMS over `bucket`: the bucket KMS, given the tests' master key, which the tests'
/// writers encrypt under.
fn host_kms<B: bucket_cell::Bucket>(bucket: &Arc<B>) -> Arc<bucket_cell::keys::CachingKms> {
    use extent_keys::kms::master::MasterKeys;
    let key = extent_keys::Secret::from_bytes(&[0x7e; 32]);
    let keys = MasterKeys::new(vec![key]).unwrap();
    let kms = bucket_cell::bucket_kms::BucketKms::new(bucket.clone(), keys);
    bucket_cell::keys::CachingKms::new(Arc::new(kms))
}

/// The tests' cell `id`, of tenant 1.
fn test_cell(id: u64) -> bucket_cell::layout::CellId {
    bucket_cell::layout::CellId { tenant: 1, id }
}

/// Writes to `cell`, made, of the bucket at `url`, a segment holding an extent and one freeing
/// it, then a checkpoint covering both: what a pass deletes, those two segments.
fn write_and_free(url: &str, cell: bucket_cell::layout::CellId) {
    use bucket_cell::Writer;
    use bucket_cell::batch::{Commit, SegmentBuilder};
    use bucket_cell::object_bucket::ObjectBucket;
    use storage_device::ExtentId;
    futures::executor::block_on(async {
        let bucket = Arc::new(ObjectBucket::from_url(url).unwrap());
        let kms = host_kms(&bucket);
        let writer = Writer::open_under(bucket, cell, (10, 1), &kms).await.unwrap();
        let extent = ExtentId::new(cell.id, writer.chain().next_sequence());
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
}

/// A bucket on the host's filesystem, in `dir`: its URL.
fn file_bucket(dir: &tempfile::TempDir) -> String {
    url::Url::from_directory_path(dir.path()).unwrap().to_string()
}

/// Every object of the bucket at `url`, by key, and its bytes.
fn objects(url: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
    use bucket_cell::Bucket as _;
    use bucket_cell::object_bucket::ObjectBucket;
    futures::executor::block_on(async {
        let bucket = ObjectBucket::from_url(url).unwrap();
        let mut objects = std::collections::BTreeMap::new();
        for meta in bucket.list("").await.unwrap() {
            let object = bucket.get(&meta.key).await.unwrap();
            objects.insert(meta.key, object.bytes.to_vec());
        }
        objects
    })
}

/// The program, run with `args`, given no KMS nor master key.
fn gc(args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
    command.env_remove("CLADEFS_KMS").env_remove("CLADEFS_MASTER_KEY");
    command.env_remove("CLADEFS_MASTER_KEY_FILE").args(args).output().unwrap()
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
        "cladefs-gc: deleted {deleted} ({bytes} bytes), retired 0 references, over {cells} cells (0 unchanged), 0 held by other passes, 0 swept by them since, 0 \
         failed; requests: R\ncladefs-gc: took T s; slowest: {slowest}\n"
    )
}

/// A pass over a bucket (in memory: empty) reports what it did, over no cell, and exits.
#[test]
fn collects_a_bucket_once() {
    let out = gc(&["memory:///"]);
    assert_eq!(said(&out), swept(0, 0, 0, "none"));
    assert!(out.status.success());
}

/// doc-27 step 3 (board 6218's AC #2): with no KMS, neither `CLADEFS_KMS` nor a master key given,
/// it collects, dry runs, lists, says usage and deletes cells: nothing it does reads data,
/// every cell's metadata authenticated, not encrypted (doc-16). The tests below collect encrypted
/// cells so.
#[test]
fn collects_without_a_kms() {
    for args in [
        &["memory:///"][..],
        &["memory:///", "--dry-run"],
        &["memory:///", "--cells"],
        &["memory:///", "--usage"],
        &["memory:///", "--delete-cell", "1/1", "--force-missing"],
    ] {
        assert!(gc(args).status.success(), "{args:?}");
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
        "cladefs_gc_sweep_stopped{reason=\"unlisted\"} 0",
        "cladefs_gc_sweep_capped_passes 0",
    ] {
        assert!(metrics.lines().any(|had| had == line), "{line} in {metrics}");
    }
    let peak =
        metrics.lines().find_map(|line| line.strip_prefix("cladefs_gc_peak_resident_bytes "));
    let peak: u64 = peak.unwrap().parse().unwrap();
    assert!((1 << 20..1 << 34).contains(&peak), "a process's memory, at most: {peak}");
}

/// TASK-79.85: a bucket (in memory: empty) has no live cells, as an operator's command lists
/// them; and a cell is deleted, as an operator's command does it. board 8511's AC #2 and #3: a
/// cell of no head, an ID mistyped, it refuses to delete, saying so, writing nothing, not even its
/// probe of the bucket's conditional writes; but with `--force-missing`, whose deleted head stays.
#[test]
fn lists_and_deletes_cells() {
    let out = gc(&["memory:///", "--cells"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(out.status.success());

    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    futures::executor::block_on(bucket_cell::lifecycle::create(&bucket, test_cell(1), 0)).unwrap();
    let files = || walk(dir.path());
    let before = (files(), objects(&url));
    let out = gc(&[&url, "--delete-cell", "1/ff"]);
    let refused = "--delete-cell: cell 1/ff has no head, so no cell to delete: nothing written; \
                   --force-missing writes it a deleted head, which no pass of the collector removes";
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("Error: {refused:?}\n"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!((files(), objects(&url)), before);

    let out = gc(&[&url, "--delete-cell", "1/1"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert!(out.status.success());
    let head = futures::executor::block_on(bucket_cell::head::get(&bucket, test_cell(1))).unwrap();
    assert!(head.as_ref().is_some_and(|head| head.deleted()), "{head:?}");
    let out = gc(&[&url, "--delete-cell", "1/ff", "--force-missing"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert!(out.status.success());
    let missing: bucket_cell::layout::CellId = "1/ff".parse().unwrap();
    let head = bucket_cell::layout::head(missing);
    assert!(objects(&url).contains_key(&head), "{head}: {:?}", objects(&url).keys());
}

/// cladefs: a pass over a bucket on the host's filesystem (TASK-165.1), from a process of its own:
/// it reclaims the cell's segments its checkpoint covers, which hold no live extent (the one
/// holding what it freed, the one freeing it), and lists the cell. board 6218's AC #1: the cell
/// encrypted, the process given no KMS nor master key ([`gc`]), it deletes those two, as a pass
/// holding the cell's keys did before doc-27.
#[test]
fn collects_a_file_bucket() {
    use bucket_cell::lifecycle;
    use bucket_cell::object_bucket::ObjectBucket;

    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = ObjectBucket::from_url(&url).unwrap();
    futures::executor::block_on(lifecycle::create(&bucket, test_cell(1), 0)).unwrap();
    write_and_free(&url, test_cell(1));
    // The two segments a pass deletes, and their bytes.
    let at = "tenants/0000000000001/cells/0000000000001";
    let doomed = [0, 1].map(|segment| {
        format!("{at}/segments/0000000000001.{segment}", segment = base32_id::Padded(segment))
    });
    let bytes: u64 = futures::executor::block_on(async {
        use bucket_cell::Bucket as _;
        let listed = ObjectBucket::from_url(&url).unwrap().list("").await.unwrap();
        listed.iter().filter(|meta| doomed.contains(&meta.key)).map(|meta| meta.size).sum()
    });
    // TASK-395.4: a pass over another cell, named, leaves this one be.
    let out = gc(&[&url, "--cell", "1/2"]);
    assert_eq!(said(&out), swept(0, 0, 1, "1/2 T s"));
    // Over the one cell there: the other's pass, of a cell holding nothing, took no lease there.
    let out = gc(&[&url]);
    assert_eq!(
        said(&out),
        format!(
            "cladefs-gc: cell 1/1: deleted 2 ({bytes} bytes), retired 0 references, in T s; \
             requests: R\n{}",
            swept(2, bytes, 1, "1/1 T s")
        )
    );
    assert!(out.status.success());
    let left = futures::executor::block_on(async {
        use bucket_cell::Bucket as _;
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
    // Its head, which names its checkpoint (TASK-407) and keeps its KEK; and its tenant's KEK
    // object, the bucket KMS's (TASK-411.28).
    let cell = bucket_cell::layout::CellId { tenant: 1, id: 1 };
    let kept: Vec<String> = vec![
        format!("{at}/checkpoints/0000000000001.0000000000001"),
        bucket_cell::layout::head(cell),
        "tenants/0000000000001/kek".to_owned(),
    ];
    assert_eq!(left, kept);
    let out = gc(&[&url, "--cells"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1/1\n");
    let out = gc(&[&url, "--cells", "--cell", "1/1"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1/1\n");
    let out = gc(&[&url, "--cells", "--cell", "1/2"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

/// TASK-264: a dry run over a bucket on the host's filesystem lists what a pass would delete,
/// writing nothing, the collector's lease neither, nor the probe of its conditional writes; a pass
/// after deletes it.
#[test]
fn dry_runs_delete_nothing() {
    use bucket_cell::lifecycle;
    use bucket_cell::object_bucket::ObjectBucket;

    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = ObjectBucket::from_url(&url).unwrap();
    futures::executor::block_on(lifecycle::create(&bucket, test_cell(1), 0)).unwrap();
    write_and_free(&url, test_cell(1));
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
    let out = gc(&[&url, "--dry-run"]);
    let cell = "0000000000001";
    let segment = |number: u64| {
        format!(
            "tenants/{cell}/cells/{cell}/segments/0000000000001.{number}",
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
            "cladefs-gc: dry run: a pass would delete 2 objects ({bytes} bytes), and retire 0 \
             references\n"
        )
    );
    assert!(out.status.success());
    assert_eq!(files(), before);

    let at = "tenants/0000000000001/cells/0000000000001";
    let out = gc(&[&url]);
    assert_eq!(
        said(&out),
        format!(
            "cladefs-gc: cell 1/1: deleted 2 ({bytes} bytes), retired 0 references, in T s; \
             requests: R\n{}",
            swept(2, bytes, 1, "1/1 T s")
        )
    );
    // The bucket's locks, no objects, aside (TASK-395.15: one a directory's, local.rs's tests);
    // the objects, the collector's under the cell's prefix.
    let names: Vec<String> = (files().into_iter())
        .map(|(name, _)| name)
        .filter(|name| !name.starts_with(bucket_cell::local::LOCK))
        .collect();
    let cell = bucket_cell::layout::CellId { tenant: 1, id: 1 };
    let kept: Vec<String> = vec![
        format!("{at}/checkpoints/0000000000001.0000000000001"),
        format!("{at}/gc/lease"),
        bucket_cell::layout::head(cell),
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
/// what the cells wrote, less what the pass deleted; when the pass listed it; nothing of a cell
/// no pass went over; and each tenant's sum after its cells. `--tenant` says one tenant's. It
/// writes nothing, the probe of conditional writes neither.
#[test]
fn usage_says_what_each_files_cell_and_tenant_takes_as_passes_found() {
    use bucket_cell::Bucket as _;
    use bucket_cell::layout::{self, CellId};
    use bucket_cell::lifecycle;
    use bucket_cell::object_bucket::ObjectBucket;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

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
        for cell in cells {
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
        }
    });
    // In the first cell, a segment written and one freeing it: what the pass deletes.
    write_and_free(&url, cells[0]);
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let before = now().as_millis() as u64;
    let out = gc(&[&url]);
    let said = said(&out);
    // The slowest passes, last, in an order of their times.
    let (did, slowest) = said.split_at(said.find("cladefs-gc: took").unwrap());
    assert_eq!(
        did,
        "cladefs-gc: cell 1/1: deleted 2 (596 bytes), retired 0 references, in T s; requests: \
         R\ncladefs-gc: deleted 2 (596 bytes), retired 0 references, over 3 cells (0 unchanged), \
         0 held by other passes, 0 swept by them since, 0 failed; requests: R\n"
    );
    assert!(slowest.starts_with("cladefs-gc: took T s; slowest: "), "{slowest}");
    let after = now().as_millis() as u64;
    // A cell made after the pass, which no pass went over.
    futures::executor::block_on(lifecycle::create(&*bucket, unpassed, 0)).unwrap();
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

/// TASK-411.27: `--rotate-master-key` rewraps every tenant's KEK object under the newest master key
/// `CLADEFS_MASTER_KEY` gives, the first, saying what it did; a host given the newest alone then
/// unwraps what was wrapped before. Done again, it rewraps nothing. Given keys none of which opens
/// an object, it leaves it, says so, and fails; given none, it fails naming the variables.
#[test]
fn rotates_master_keys_rewrapping_every_tenants_kek() {
    use bucket_cell::bucket_kms::{self, BucketKms};
    use bucket_cell::object_bucket::ObjectBucket as Bucket;
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
    let bucket = Arc::new(Bucket::from_url(&url).unwrap());
    let before = BucketKms::new(bucket.clone(), keys(&[&old]));
    let key = Secret::random();
    let wrapped: Vec<_> =
        (1..=3).map(|tenant| block_on(before.wrap(tenant, &key)).unwrap()).collect();
    block_on(bucket_kms::destroy(&*bucket, 3)).unwrap();
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
         given, the registry of buckets unchanged: run again with theirs among \
         CLADEFS_MASTER_KEY's\"\n",
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
    let usage = "usage: cladefs-gc URL [--cell TENANT/CELL] [--every SECONDS] [--at-once PASSES] \
                 [--lease SECONDS] [--metrics FILE] [--report FILE] [--max-delete-objects N] \
                 [--max-delete-bytes N] [--max-requests-per-second N] [--max-refused-passes N] | \
                 cladefs-gc URL [--cell TENANT/CELL] --dry-run | cladefs-gc URL --cells [--cell \
                 TENANT/CELL] [--held FILE] | cladefs-gc URL --delete-cell TENANT/CELL \
                 [--force-missing] | cladefs-gc URL --delete-tenant TENANT | cladefs-gc URL \
                 --usage [--tenant TENANT] | cladefs-gc URL --rotate-master-key (every cell, or \
                 the one --cell names; SECONDS from a millisecond to a year; --dry-run writes \
                 nothing)";
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("Error: {usage:?}\n"));
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn sweeps_write_json_report_and_extended_prometheus_metrics() {
    use bucket_cell::gc_metrics::SweepReport;
    use bucket_cell::object_bucket::ObjectBucket;
    use bucket_cell::{identity, lifecycle};

    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = ObjectBucket::from_url(&url).unwrap();
    futures::executor::block_on(async {
        let _ = identity::format(&bucket, &identity::Details::default()).await.unwrap();
        lifecycle::create(&bucket, test_cell(1), 0).await.unwrap();
        lifecycle::create(&bucket, bucket_cell::layout::CellId { tenant: 2, id: 1 }, 0)
            .await
            .unwrap();
    });

    let out_dir = tempfile::tempdir().unwrap();
    let prom_file = out_dir.path().join("gc.prom");
    let report_file = out_dir.path().join("gc.json");
    let out = gc(&[
        &url,
        "--metrics",
        prom_file.to_str().unwrap(),
        "--report",
        report_file.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let report: SweepReport =
        serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
    assert_eq!(report.cells.total, 2);
    assert_eq!(report.cells.live, 2);
    assert_eq!(report.tenants.len(), 2);
    assert_eq!(
        report.formats.bucket_gates,
        Some(bucket_cell::gc_metrics::BucketGates {
            storage: formats::gates::CURRENT_STORAGE,
            fxfs: formats::gates::CURRENT_FXFS,
        })
    );
    assert_eq!(report.formats.head_versions.get(&bucket_cell::layout::FORMAT.current), Some(&2));
    assert!(report.stored.bytes > 0);
    assert!(report.object_sizes.stats.count > 0);

    let prom = std::fs::read_to_string(&prom_file).unwrap();
    assert!(prom.contains("cladefs_gc_bucket_gate_version{knob=\"storage\"} 1"), "{prom}");
    assert!(prom.contains("cladefs_gc_tenants_observed 2"), "{prom}");
    assert!(prom.contains("cladefs_gc_format_cells{scope=\"head\",version=\"35\"} 2"), "{prom}");
    assert!(prom.contains("cladefs_gc_object_size_bytes_count "), "{prom}");
    assert!(prom.contains("cladefs_gc_cell_size_bytes_count 2"), "{prom}");
    assert!(!prom.contains("tenant="), "high-cardinality tenant label leaked into prom: {prom}");
}

/// The cells' objects of the bucket at `url`, the collector's leases aside, which a pass writes
/// whatever it does.
fn cells_objects(url: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut objects = objects(url);
    objects.retain(|key, _| !key.ends_with("/gc/lease"));
    objects
}

/// board 8509's AC #4: a sweep run once whose pass failed, its cell's head corrupt, says so, and
/// writes its metrics, which say so too; then it fails the run.
#[test]
fn sweeps_run_once_that_fail_fail_the_run() {
    use bucket_cell::Bucket as _;
    use bucket_cell::bucket::PutMode;
    use bucket_cell::object_bucket::ObjectBucket;
    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = ObjectBucket::from_url(&url).unwrap();
    let head = bucket_cell::layout::head(test_cell(1));
    let corrupt = bucket.put(&head, vec![0xff; 7].into(), PutMode::Create);
    futures::executor::block_on(corrupt).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let file = elsewhere.path().join("gc.prom");
    let out = gc(&[&url, "--metrics", file.to_str().unwrap()]);
    let said = said(&out);
    let (failed, rest) = said.split_once('\n').unwrap();
    assert!(failed.starts_with("cladefs-gc: collecting cell 1/1: "), "{said}");
    assert_eq!(
        rest,
        "cladefs-gc: deleted 0 (0 bytes), retired 0 references, over 1 \
         cells (0 unchanged), 0 held by other passes, 0 swept by them since, 1 failed; requests: \
         R\ncladefs-gc: took T s; slowest: 1/1 T s\nError: \"the sweep did not go through: 1 \
         passes failed\"\n"
    );
    assert_eq!(out.status.code(), Some(1));
    let metrics = std::fs::read_to_string(&file).unwrap();
    for line in [
        "cladefs_gc_sweep_cells{outcome=\"passed\"} 0",
        "cladefs_gc_sweep_cells{outcome=\"failed\"} 1",
    ] {
        assert!(metrics.lines().any(|had| had == line), "{line} in {metrics}");
    }
}

/// board 8529's AC #1: a sweep bounded below what its pass would delete deletes nothing, says why
/// it stopped, and fails the run; unbounded after it, it deletes it.
#[test]
fn sweeps_past_their_bounds_delete_nothing_and_fail_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    futures::executor::block_on(bucket_cell::lifecycle::create(&bucket, test_cell(1), 0)).unwrap();
    write_and_free(&url, test_cell(1));
    let before = cells_objects(&url);
    for bound in ["--max-delete-objects", "--max-delete-bytes"] {
        let out = gc(&[&url, bound, "1"]);
        let stopped = "a pass would have deleted past the sweep's bounds";
        assert_eq!(
            said(&out),
            format!(
                "{}cladefs-gc: the sweep stopped: {stopped}\nError: \"the sweep did not go \
                 through: 1 passes deleted nothing, past its bounds; it stopped: {stopped}\"\n",
                swept(0, 0, 1, "1/1 T s")
            ),
            "{bound}"
        );
        assert_eq!(out.status.code(), Some(1), "{bound}");
        assert_eq!(cells_objects(&url), before, "{bound}");
    }
    let out = gc(&[&url]);
    assert!(out.status.success());
    assert_eq!(cells_objects(&url).len(), before.len() - 2);
}

/// board 8513's AC #1: SIGTERM has a process sweeping every so long end between its sweeps, at
/// once, saying so, as its last sweep did, having written its metrics. A sweep stopped as it runs,
/// starting no more passes, is `bucket_cell::sweep`'s test.
#[test]
fn sigterm_ends_sweeping() {
    use std::io::{BufRead as _, Read as _};
    let elsewhere = tempfile::tempdir().unwrap();
    let file = elsewhere.path().join("gc.prom");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
    command.env_remove("CLADEFS_KMS").env_remove("CLADEFS_MASTER_KEY_FILE");
    command.env_remove("CLADEFS_MASTER_KEY");
    command.args(["memory:///", "--every", "3600", "--metrics", file.to_str().unwrap()]);
    let mut child = command.stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("cladefs-gc: took ") {
        assert_ne!(stderr.read_line(&mut said).unwrap(), 0, "{said}");
    }
    // SAFETY: A signal to the child this test started, which it has not waited for.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = child.wait().unwrap();
    stderr.read_to_string(&mut said).unwrap();
    assert!(
        said.ends_with(
            "cladefs-gc: SIGTERM: starting no more passes, letting those running end; again to \
             exit at once\n"
        ),
        "{said}"
    );
    assert!(status.success(), "{status:?}");
    let metrics = std::fs::read_to_string(&file).unwrap();
    assert!(metrics.lines().any(|line| line == "cladefs_gc_sweeps_total 1"), "{metrics}");
}

/// board 8367: `--cells` is read-only and succeeds against a bucket that allows no writes or
/// deletes (no probe object is created or deleted).
#[test]
fn cells_listing_runs_against_a_read_only_bucket() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let url = file_bucket(&dir);
    let bucket = bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap();
    let cell = test_cell(7);
    futures::executor::block_on(bucket_cell::lifecycle::create(&bucket, cell, 0)).unwrap();

    // Remove write permissions from the bucket root and all subdirectories so any PUT/DELETE
    // fails with EACCES. Restore permissions on drop so tempdir cleanup always succeeds.
    struct Restore(Vec<std::path::PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for path in self.0.iter().rev() {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    let mut dirs = vec![dir.path().to_path_buf()];
    let mut i = 0;
    while i < dirs.len() {
        for entry in std::fs::read_dir(&dirs[i]).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        i += 1;
    }
    let _restore = Restore(dirs.clone());
    for path in dirs.iter().rev() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    let out = gc(&[&url, "--cells"]);
    assert!(
        out.status.success(),
        "cladefs-gc --cells must succeed on a read-only bucket; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{cell}\n"));
    assert!(!dir.path().join("probe").exists(), "--cells must not write a probe object");
}

/// board 5532: on bucket B with bucket A's live cell P mirrored in B:
/// 1. `--cells`, `--usage`, and `--dry-run` finish with A offline and send A zero requests.
/// 2. `--delete-cell P` on B refuses (naming P's bucket and B) and leaves P live in A.
/// 3. `cladefs-gc B --cell P` collects P's copy in B via `collect_copy`, leaving P's head and
///    objects in A untouched, while a B-owned child of P still routes to A to retire its reference.
///
/// Requires `testing` feature for `bucket_cell::s3_server` so standalone `cargo test -p bucket_cell`
/// (e.g. `scripts/test-azurite.sh`) compiles without `--features testing` (board 11589).
#[cfg(feature = "testing")]
#[test]
fn bucket_b_commands_stay_on_b_and_collect_copies_locally() {
    use bucket_cell::Bucket as _;
    use bucket_cell::Writer;
    use bucket_cell::batch::{Commit, SegmentBuilder};
    use bucket_cell::identity::{self, Details};
    use bucket_cell::layout::{CellId, NewVersion, Origin};
    use bucket_cell::object_bucket::{ObjectBucket, Registered};
    use bucket_cell::s3_server::{BUCKET, S3Server};
    use storage_device::ExtentId;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let (server_a, server_b, url_a, url_b, p, c_live, c_del, d_live, p_objects_before, ref_in_a) =
        runtime.block_on(async {
            let server_a = S3Server::start(None);
            let server_b = S3Server::start(None);
            let url_a = format!("s3://{BUCKET}/a");
            let url_b = format!("s3://{BUCKET}/b");
            let opts_for = |server: &S3Server| {
                server
                    .options()
                    .into_iter()
                    .map(|(k, v)| (k.to_ascii_uppercase(), v))
                    .collect::<Vec<_>>()
            };
            let a_own = Arc::new(ObjectBucket::new(server_a.objects(), "a"));
            let b_own = Arc::new(ObjectBucket::new(server_b.objects(), "b"));

            let details = |url: &str| Details {
                provider: Some("s3".into()),
                url: Some(url.to_owned()),
                ..Details::default()
            };
            identity::format(&*a_own, &details(&url_a)).await.unwrap();
            let (_, registry) =
                identity::add(&*a_own, &details(&url_a), &*b_own, details(&url_b)).await.unwrap();
            identity::spread(&registry, &*a_own).await.unwrap();
            identity::spread(&registry, &*b_own).await.unwrap();

            // Build a routed client for B that resolves bucket 1 -> server_a and 2 -> server_b.
            let mut env_pairs = Vec::new();
            for (k, v) in opts_for(&server_a) {
                env_pairs.push((format!("CLADEFSD_BUCKET_1_{k}"), v));
            }
            for (k, v) in opts_for(&server_b) {
                env_pairs.push((format!("CLADEFSD_BUCKET_2_{k}"), v));
            }
            for (k, v) in &env_pairs {
                // SAFETY: nextest runs each test in its own process, before spawning cladefs-gc.
                unsafe { std::env::set_var(k, v) };
            }
            let resolve = Arc::new(Registered::from_env());
            let b_routed = Arc::new(ObjectBucket::new(server_b.objects(), "b").routed(resolve));

            // Create live cell P in bucket A (#1) with a freed segment (reclaimable in A) and two
            // versions: v1 (mirrored into B) and v2 (unmirrored, referenced by B-owned child c_del
            // in A).
            let p = CellId { tenant: 1, id: (1u64 << 48) | 1 };
            let kms_a = host_kms(&a_own);
            bucket_cell::lifecycle::create(&*a_own, p, 0).await.unwrap();
            let writer = Writer::open_under(a_own.clone(), p, (10, 1), &kms_a).await.unwrap();
            let ext = ExtentId::new(p.id, writer.chain().next_sequence());
            let mut at_v1 = 0;
            let mut at_v2 = 0;
            for (idx, commit) in [
                Commit { tag: 1, extents: vec![(ext, vec![3; 4096])], ..Commit::default() },
                Commit { tag: 2, frees: vec![ext], ..Commit::default() },
            ]
            .into_iter()
            .enumerate()
            {
                let cut = NewVersion { id: (idx + 1) as u64, keep_last: None };
                let framed = SegmentBuilder::from(commit)
                    .cut(cut, &mut writer.chain())
                    .frame(writer.keys().encrypting());
                let seg = writer.add_segment(framed).unwrap();
                writer.upload(seg).await.unwrap();
                writer.acknowledge().await.unwrap();
                if idx == 0 {
                    at_v1 = seg;
                } else {
                    at_v2 = seg;
                }
            }
            writer.checkpoint().await.unwrap();
            let epoch_p = writer.epoch();
            let p_ring = writer.keys().clone();
            drop(writer);

            // Fork v2 into B-owned child `c_del` BEFORE any mirror exists for v2, so its reference
            // lives in bucket A under P's prefix, then delete `c_del` in B.
            let c_del = CellId { tenant: 1, id: (2u64 << 48) | 2 };
            let origin_v2 = Origin { parent: p, at: at_v2, epoch: epoch_p };
            bucket_cell::lifecycle::fork_version(
                &*b_routed,
                c_del,
                2,
                origin_v2,
                2,
                bucket_cell::keys::Parents::Held(&p_ring),
            )
            .await
            .unwrap();
            let ref_in_a = bucket_cell::layout::reference(p, at_v2, c_del);
            assert!(
                a_own.get(&ref_in_a).await.is_ok(),
                "expected B-owned child's reference to exist in A"
            );
            bucket_cell::lifecycle::delete(&*b_own, c_del, 99, u64::MAX).await.unwrap();

            // Fork v1 into B-owned live child `c_live`, then mirror v1 into B so P has a mirrored
            // copy in B.
            let c_live = CellId { tenant: 1, id: (2u64 << 48) | 3 };
            let origin_v1 = Origin { parent: p, at: at_v1, epoch: epoch_p };
            bucket_cell::lifecycle::fork_version(
                &*b_routed,
                c_live,
                1,
                origin_v1,
                3,
                bucket_cell::keys::Parents::Held(&p_ring),
            )
            .await
            .unwrap();
            let mirrored =
                bucket_cell::mirror::mirror(&*b_routed, &*b_own, 2, p, at_v1).await.unwrap();
            assert!(matches!(mirrored, bucket_cell::mirror::Mirrored::Done { .. }), "{mirrored:?}");

            // Create a standalone live B-owned cell `d_live`.
            let d_live = CellId { tenant: 1, id: (2u64 << 48) | 4 };
            bucket_cell::lifecycle::create(&*b_own, d_live, 0).await.unwrap();

            // Record P's objects in A before running cladefs-gc on B.
            let mut p_objects_before: Vec<String> =
                a_own.list(&p.prefix()).await.unwrap().into_iter().map(|m| m.key).collect();
            p_objects_before.sort();

            (server_a, server_b, url_a, url_b, p, c_live, c_del, d_live, p_objects_before, ref_in_a)
        });

    let gc_b = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cladefs-gc"));
        command
            .env_remove("CLADEFS_KMS")
            .env_remove("CLADEFS_MASTER_KEY")
            .env_remove("CLADEFS_MASTER_KEY_FILE");
        for (k, v) in server_b.options() {
            command.env(k.to_ascii_uppercase(), v);
        }
        for (k, v) in server_a.options() {
            command.env(format!("CLADEFSD_BUCKET_1_{}", k.to_ascii_uppercase()), v);
        }
        for (k, v) in server_b.options() {
            command.env(format!("CLADEFSD_BUCKET_2_{}", k.to_ascii_uppercase()), v);
        }
        command.args(args).output().unwrap()
    };

    // AC #1: With A offline (403 to every request), --cells, --usage, and --dry-run on B finish
    // and send A zero requests.
    server_a.answering(Some(403));
    let a_reqs_before = server_a.requests();

    let out = gc_b(&[&url_b, "--cells"]);
    assert!(out.status.success(), "--cells failed: {}", String::from_utf8_lossy(&out.stderr));
    let cells_out = String::from_utf8_lossy(&out.stdout);
    let listed_cells: Vec<&str> =
        cells_out.lines().map(|line| line.split_whitespace().next().unwrap()).collect();
    assert_eq!(
        listed_cells,
        vec![c_live.to_string(), d_live.to_string()],
        "--cells must list only B's live cells, not foreign P: {cells_out}"
    );

    let out = gc_b(&[&url_b, "--usage"]);
    assert!(out.status.success(), "--usage failed: {}", String::from_utf8_lossy(&out.stderr));
    let usage_out = String::from_utf8_lossy(&out.stdout);
    assert!(!usage_out.contains(&p.to_string()), "--usage must not list foreign P: {usage_out}");
    assert!(usage_out.contains(&d_live.to_string()), "{usage_out}");

    let out = gc_b(&[&url_b, "--dry-run"]);
    assert!(out.status.success(), "--dry-run failed: {}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(
        server_a.requests(),
        a_reqs_before,
        "read-only commands on B sent requests to offline A"
    );

    // AC #2: --delete-cell P on B refuses (naming P's bucket 1 and B's bucket 2), sends A no
    // request, and leaves P live in A.
    server_a.answering(None);
    let a_reqs_before_del = server_a.requests();
    let p_str = p.to_string();
    let out = gc_b(&[&url_b, "--delete-cell", &p_str]);
    assert!(!out.status.success(), "--delete-cell P on B must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bucket 1") && stderr.contains("2"),
        "error should name P's bucket (1) and B (2): {stderr}"
    );
    assert_eq!(server_a.requests(), a_reqs_before_del, "--delete-cell P on B sent requests to A");
    let a_own = ObjectBucket::new(server_a.objects(), "a");
    assert!(
        runtime.block_on(bucket_cell::head::live(&a_own, p)).unwrap().is_some(),
        "P must remain live in A"
    );

    // AC #3: `cladefs-gc B --cell P` runs collect_copy on P's copy in B and leaves P's head and
    // objects in A untouched; collecting deleted B-owned child `c_del` still routes to A to retire
    // its reference there.
    let out = gc_b(&[&url_b, "--cell", &p_str]);
    assert!(
        out.status.success(),
        "cladefs-gc B --cell P failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut p_objects_after: Vec<String> =
        runtime.block_on(a_own.list(&p.prefix())).unwrap().into_iter().map(|m| m.key).collect();
    p_objects_after.sort();
    assert_eq!(
        p_objects_after, p_objects_before,
        "cladefs-gc B --cell P modified P's objects in A"
    );

    let c_del_str = c_del.to_string();
    let out = gc_b(&[&url_b, "--cell", &c_del_str]);
    assert!(
        out.status.success(),
        "cladefs-gc B --cell c_del failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ref_after = runtime.block_on(a_own.get(&ref_in_a));
    assert!(
        ref_after.is_err(),
        "collecting deleted B-owned child must route to A to retire its reference {ref_in_a}"
    );
    let _ = url_a;
}

/// board 11663: `Collector::collect_with_own` on a routed bucket 2+ must read `events` from root
/// bucket 1 (`events::authority`) and observe `TenantShredded` without pre-populating
/// `Collector::shredded`, while an unrouted bucket 2 handle (as used by `--dry-run`, board 5532)
/// stays strictly local to bucket 2 and sends zero requests to bucket 1.
#[cfg(feature = "testing")]
#[test]
fn collect_with_own_on_bucket_2_reads_shredded_tenants_from_root_bucket_1() {
    use bucket_cell::Bucket as _;
    use bucket_cell::batch::{Commit, SegmentBuilder};
    use bucket_cell::collector::Collector;
    use bucket_cell::events::{self, Publish};
    use bucket_cell::identity::{self, Details};
    use bucket_cell::layout::{self, CellId};
    use bucket_cell::object_bucket::{ObjectBucket, Registered};
    use bucket_cell::s3_server::{BUCKET, S3Server};
    use bucket_cell::writer::Writer;
    use storage_device::ExtentId;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let server_a = S3Server::start(None);
        let server_b = S3Server::start(None);
        let url_a = format!("s3://{BUCKET}/a-11663");
        let url_b = format!("s3://{BUCKET}/b-11663");
        let opts_for = |server: &S3Server| {
            server
                .options()
                .into_iter()
                .map(|(k, v)| (k.to_ascii_uppercase(), v))
                .collect::<Vec<_>>()
        };
        let a_own = Arc::new(ObjectBucket::new(server_a.objects(), "a-11663"));
        let b_own = Arc::new(ObjectBucket::new(server_b.objects(), "b-11663"));
        let details = |url: &str| Details {
            provider: Some("s3".into()),
            url: Some(url.to_owned()),
            ..Details::default()
        };
        identity::format(&*a_own, &details(&url_a)).await.unwrap();
        let (_, registry) =
            identity::add(&*a_own, &details(&url_a), &*b_own, details(&url_b)).await.unwrap();
        identity::spread(&registry, &*a_own).await.unwrap();
        identity::spread(&registry, &*b_own).await.unwrap();

        for (k, v) in opts_for(&server_a) {
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var(format!("CLADEFSD_BUCKET_1_{k}"), v) };
        }
        for (k, v) in opts_for(&server_b) {
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var(format!("CLADEFSD_BUCKET_2_{k}"), v) };
        }
        let resolve = Arc::new(Registered::from_env());
        let b_routed = Arc::new(ObjectBucket::new(server_b.objects(), "b-11663").routed(resolve));

        // Create a live cell owned by bucket 2 for tenant 7.
        let tenant = 7;
        let cell = CellId { tenant, id: (2u64 << 48) | 1 };
        let kms_b = host_kms(&b_own);
        bucket_cell::lifecycle::create(&*b_own, cell, 0).await.unwrap();
        let writer = Writer::open_under(b_own.clone(), cell, (10, 1), &kms_b).await.unwrap();
        let ext = ExtentId::new(cell.id, writer.chain().next_sequence());
        let framed = SegmentBuilder::from(Commit {
            tag: 1,
            extents: vec![(ext, vec![0x5a; 4096])],
            ..Commit::default()
        })
        .close(&mut writer.chain())
        .frame(writer.keys().encrypting());
        let seg = writer.add_segment(framed).unwrap();
        writer.upload(seg).await.unwrap();
        writer.acknowledge().await.unwrap();
        drop(writer);

        // Publish `TenantShredded` ONLY to root bucket 1 (`a_own`); bucket 2 has no `events`.
        events::publish(&*a_own, Publish::TenantShredded(tenant)).await.unwrap();
        assert!(events::get(&*b_own).await.unwrap().is_none());

        // Unrouted `b_own` (used by `--dry-run`, board 5532) stays local to bucket 2 and sends
        // zero requests to bucket 1.
        let a_before = server_a.requests();
        assert!(events::get_authoritative(&*b_own).await.unwrap().is_none());
        assert_eq!(server_a.requests(), a_before, "unrouted events read must not touch bucket 1");

        // Direct `Collector::collect_with_own` on routed bucket 2 observes `TenantShredded` from
        // root bucket 1 without pre-populating `Collector::shredded` and collects `cell`.
        let report =
            Collector::new().collect_with_own(&*b_routed, Some(&*b_own), 99).await.unwrap();
        assert!(report.deleted > 0, "collector on bucket 2 must collect shredded tenant's cell");
        let remaining = b_own.list(&cell.prefix()).await.unwrap();
        assert!(
            remaining.iter().all(|m| matches!(layout::kind(&m.key), layout::Kind::Head { .. })),
            "shredded cell data objects in bucket 2 must be deleted; left: {remaining:?}"
        );
    });
}
