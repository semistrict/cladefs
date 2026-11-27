// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: keyless WebAssembly bindings of [`bucket_cell::collector::Collector`] for Cloudflare
//! Workers (native `R2Bucket` binding + Queue fan-out per cell).
//!
//! All 64-bit IDs (`tenant`, `cell`, `holder`) cross the JS boundary as strings (base32 for
//! tenant/cell IDs as `cladefs-gc` uses, decimal for `holder`) so IEEE-754 numbers never truncate
//! them. Timestamps and byte counts stay well under `2^53 - 1`.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

use base32_id::Short;
use bucket_cell::gc_metrics::{Merge as _, SweepReport};
use bucket_cell::layout::{self, CellId};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// One cell listed in the bucket.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CellEntry {
    pub tenant: String,
    pub cell: String,
    pub id: String,
}

impl From<CellId> for CellEntry {
    fn from(cell: CellId) -> Self {
        Self {
            tenant: Short(cell.tenant).to_string(),
            cell: Short(cell.id).to_string(),
            id: cell.to_string(),
        }
    }
}

/// Outcome of one [`bucket_cell::collector::Collector::collect_cell`] pass.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollectResult {
    pub cell: String,
    pub deleted: usize,
    pub deleted_bytes: u64,
    pub retired: usize,
    pub skipped: usize,
    pub removed: usize,
    /// Present when another pass holds the cell (`"held"`) or swept it since `sweep_since_ms`
    /// (`"swept"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<&'static str>,
    /// Per-cell GC metrics snapshot (formats, histograms, per-tenant accounting), mergeable
    /// across fan-out workers with [`merge_reports`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<SweepReport>,
}

/// One object a dry run would delete.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DoomedEntry {
    pub key: String,
    pub kind: &'static str,
    pub size: u64,
}

/// Outcome of [`bucket_cell::collector::Collector::dry_run_cell`].
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DryRunResult {
    pub cell: String,
    pub doomed: Vec<DoomedEntry>,
    pub doomed_bytes: u64,
    pub retired: Vec<String>,
    pub fenced: bool,
}

/// Objects and bytes of one object kind in a cell's recorded usage.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KindUsage {
    pub objects: u64,
    pub bytes: u64,
}

/// Recorded usage from a cell's collector lease (`<C>gc/lease`).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CellUsageResult {
    pub tenant: String,
    pub cell: String,
    pub measured_unix_ms: Option<u64>,
    pub objects: Option<u64>,
    pub bytes: Option<u64>,
    pub kinds: Option<BTreeMap<String, KindUsage>>,
}

/// Live-cell head status for one cell.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LiveCellResult {
    pub cell: String,
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_at: Option<String>,
}

/// Parses a `TENANT/CELL` base32 cell ID.
pub fn parse_cell_id(text: &str) -> Result<CellId, String> {
    text.parse::<CellId>().map_err(|err| format!("invalid cell id {text:?}: {err}"))
}

/// Parses a JSON array of base32 tenant IDs into a shredded-tenant set.
pub fn parse_shredded(json: &str) -> Result<BTreeSet<u64>, String> {
    if json.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    let names: Vec<String> =
        serde_json::from_str(json).map_err(|err| format!("invalid shredded JSON: {err}"))?;
    let mut set = BTreeSet::new();
    for name in names {
        let tenant =
            base32_id::parse(&name).ok_or_else(|| format!("invalid base32 tenant id {name:?}"))?;
        if tenant != layout::DEFAULT_TENANT {
            set.insert(tenant);
        }
    }
    Ok(set)
}

/// Parses a non-zero decimal `u64` collector holder ID.
pub fn parse_holder(text: &str) -> Result<u64, String> {
    let holder: u64 = text.parse().map_err(|err| format!("invalid holder id {text:?}: {err}"))?;
    if holder == 0 {
        return Err("holder id must be non-zero".into());
    }
    Ok(holder)
}

/// Returns `true` if `cell` belongs to another bucket than `own_number` (a mirrored copy).
pub fn is_foreign_bucket(own_number: Option<u16>, cell: CellId) -> bool {
    let Some(own) = own_number else { return false };
    layout::routed_to(&cell.prefix()).is_some_and(|of| of != own)
}

/// Folds a JSON array of per-cell or partial [`SweepReport`] values into a single merged JSON
/// [`SweepReport`] (for fan-out/fan-in Cloudflare Workers sweeps).
pub fn merge_reports(reports_json: &str) -> Result<String, String> {
    let parts: Vec<SweepReport> = serde_json::from_str(reports_json)
        .map_err(|err| format!("invalid reports JSON array: {err}"))?;
    let mut merged = SweepReport::default();
    for part in parts {
        merged.merge(part);
    }
    serde_json::to_string(&merged).map_err(|err| format!("serializing merged report: {err}"))
}

#[cfg(target_arch = "wasm32")]
mod wasm_api {
    use super::*;
    use bucket_cell::bucket::{
        Bucket, ByteRange, Error as BucketError, Listing, Meta, Object, Page, PutMode, Refusal,
        Version,
    };
    use bucket_cell::collector::{Collector, Holding};
    use bucket_cell::{Error, Left, cell, events, head, identity, passed};
    use bytes::Bytes;
    use futures::StreamExt as _;
    use js_sys::{Array, Promise, Reflect, Uint8Array};
    use std::sync::Arc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    #[wasm_bindgen]
    extern "C" {
        /// Duck-typed JS bridge wrapping a Cloudflare Worker `R2Bucket` binding.
        #[wasm_bindgen(typescript_type = "R2Bridge")]
        pub type R2Bridge;

        #[wasm_bindgen(method, catch)]
        fn get(this: &R2Bridge, key: &str) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch, js_name = "getRange")]
        fn get_range(
            this: &R2Bridge,
            key: &str,
            start: Option<f64>,
            end: Option<f64>,
            suffix: Option<f64>,
        ) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch)]
        fn head(this: &R2Bridge, key: &str) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch)]
        fn list(this: &R2Bridge, prefix: &str) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch, js_name = "listDelimited")]
        fn list_delimited(
            this: &R2Bridge,
            prefix: &str,
            after: Option<&str>,
            max: u32,
        ) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch)]
        fn put(
            this: &R2Bridge,
            key: &str,
            bytes: Uint8Array,
            mode: &str,
            etag: Option<&str>,
        ) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch)]
        fn delete(this: &R2Bridge, key: &str) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method, catch, js_name = "deleteBatch")]
        fn delete_batch(this: &R2Bridge, keys: Array) -> Result<Promise, JsValue>;

        #[wasm_bindgen(method)]
        fn now(this: &R2Bridge) -> f64;

        #[wasm_bindgen(method, catch)]
        fn sleep(this: &R2Bridge, ms: u32) -> Result<Promise, JsValue>;
    }

    struct JsBucket(R2Bridge);

    fn classify_js_error(err: JsValue) -> BucketError {
        if let Ok(code) = Reflect::get(&err, &JsValue::from_str("code"))
            && let Some(code) = code.as_string()
        {
            return match code.as_str() {
                "NotFound" => BucketError::NotFound,
                "AlreadyExists" => BucketError::AlreadyExists,
                "Precondition" => BucketError::Precondition,
                "Denied" => BucketError::Refused(Refusal::Denied),
                "Request" => BucketError::Refused(Refusal::Request),
                "Unsupported" => BucketError::Unsupported,
                _ => BucketError::Unavailable,
            };
        }
        BucketError::Unavailable
    }

    async fn await_promise(res: Result<Promise, JsValue>) -> Result<JsValue, BucketError> {
        let promise = res.map_err(classify_js_error)?;
        JsFuture::from(promise).await.map_err(classify_js_error)
    }

    fn prop(obj: &JsValue, name: &str) -> Result<JsValue, BucketError> {
        Reflect::get(obj, &JsValue::from_str(name))
            .map_err(|_| BucketError::Refused(Refusal::Unreadable))
    }

    fn prop_str(obj: &JsValue, name: &str) -> Result<String, BucketError> {
        prop(obj, name)?.as_string().ok_or(BucketError::Refused(Refusal::Unreadable))
    }

    fn prop_opt_str(obj: &JsValue, name: &str) -> Result<Option<String>, BucketError> {
        let v = prop(obj, name)?;
        if v.is_null() || v.is_undefined() {
            return Ok(None);
        }
        v.as_string().map(Some).ok_or(BucketError::Refused(Refusal::Unreadable))
    }

    fn prop_u64(obj: &JsValue, name: &str) -> Result<u64, BucketError> {
        let n = prop(obj, name)?.as_f64().ok_or(BucketError::Refused(Refusal::Unreadable))?;
        if !n.is_finite() || n < 0.0 {
            return Err(BucketError::Refused(Refusal::Unreadable));
        }
        Ok(n as u64)
    }

    fn parse_meta(obj: &JsValue) -> Result<Meta, BucketError> {
        let key = prop_str(obj, "key")?;
        let e_tag = prop_opt_str(obj, "etag")?;
        let version = prop_opt_str(obj, "version")?;
        let size = prop_u64(obj, "size")?;
        let modified = prop_u64(obj, "modifiedMs")?;
        Ok(Meta { key, version: Version { e_tag, version }, size, modified })
    }

    fn parse_version(obj: &JsValue) -> Result<Version, BucketError> {
        let e_tag = prop_opt_str(obj, "etag")?;
        let version = prop_opt_str(obj, "version")?;
        Ok(Version { e_tag, version })
    }

    impl Bucket for JsBucket {
        async fn get(&self, key: &str) -> Result<Object, BucketError> {
            let val = await_promise(self.0.get(key)).await?;
            if val.is_null() || val.is_undefined() {
                return Err(BucketError::NotFound);
            }
            let meta = parse_meta(&val)?;
            let bytes_val = prop(&val, "bytes")?;
            let bytes = Uint8Array::new(&bytes_val).to_vec();
            Ok(Object { bytes, meta })
        }

        async fn get_range(&self, key: &str, range: ByteRange) -> Result<Vec<u8>, BucketError> {
            let (start, end, suffix) = match range {
                ByteRange::Bounded(r) => (Some(r.start as f64), Some(r.end as f64), None),
                ByteRange::Suffix(n) => (None, None, Some(n as f64)),
            };
            let val = await_promise(self.0.get_range(key, start, end, suffix)).await?;
            if val.is_null() || val.is_undefined() {
                return Err(BucketError::NotFound);
            }
            Ok(Uint8Array::new(&val).to_vec())
        }

        async fn head(&self, key: &str) -> Result<Meta, BucketError> {
            let val = await_promise(self.0.head(key)).await?;
            if val.is_null() || val.is_undefined() {
                return Err(BucketError::NotFound);
            }
            parse_meta(&val)
        }

        async fn list(&self, prefix: &str) -> Result<Vec<Meta>, BucketError> {
            let val = await_promise(self.0.list(prefix)).await?;
            let arr = Array::from(&val);
            let mut out = Vec::with_capacity(arr.length() as usize);
            for item in arr.iter() {
                out.push(parse_meta(&item)?);
            }
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        }

        async fn list_delimited(
            &self,
            prefix: &str,
            page: Page<'_>,
        ) -> Result<Listing, BucketError> {
            let val =
                await_promise(self.0.list_delimited(prefix, page.after, page.max as u32)).await?;
            let prefixes_arr = Array::from(&prop(&val, "prefixes")?);
            let mut prefixes = Vec::with_capacity(prefixes_arr.length() as usize);
            for p in prefixes_arr.iter() {
                prefixes.push(p.as_string().ok_or(BucketError::Refused(Refusal::Unreadable))?);
            }
            let objects_arr = Array::from(&prop(&val, "objects")?);
            let mut objects = Vec::with_capacity(objects_arr.length() as usize);
            for item in objects_arr.iter() {
                objects.push(parse_meta(&item)?);
            }
            let next = prop_opt_str(&val, "next")?;
            Ok(Listing { prefixes, objects, next })
        }

        async fn put(
            &self,
            key: &str,
            bytes: Bytes,
            mode: PutMode,
        ) -> Result<Version, BucketError> {
            let (mode_str, etag) = match &mode {
                PutMode::Overwrite => ("overwrite", None),
                PutMode::Create => ("create", None),
                PutMode::Update(v) => ("update", v.e_tag.as_deref()),
            };
            let arr = Uint8Array::from(bytes.as_ref());
            let val = await_promise(self.0.put(key, arr, mode_str, etag)).await?;
            if val.is_null() || val.is_undefined() {
                return Err(match mode {
                    PutMode::Create => BucketError::AlreadyExists,
                    PutMode::Update(_) => BucketError::Precondition,
                    PutMode::Overwrite => BucketError::Unavailable,
                });
            }
            parse_version(&val)
        }

        async fn delete(&self, key: &str) -> Result<(), BucketError> {
            let _ = await_promise(self.0.delete(key)).await?;
            Ok(())
        }

        async fn delete_batch(&self, keys: &[&str]) -> Result<(), BucketError> {
            if keys.is_empty() {
                return Ok(());
            }
            let arr = Array::new_with_length(keys.len() as u32);
            for (i, key) in keys.iter().enumerate() {
                arr.set(i as u32, JsValue::from_str(key));
            }
            let _ = await_promise(self.0.delete_batch(arr)).await?;
            Ok(())
        }

        fn now(&self) -> u64 {
            let ms = self.0.now();
            if ms.is_finite() && ms >= 0.0 { ms as u64 } else { 0 }
        }

        async fn pause(&self, failures: u32) {
            // Exponential backoff: 100ms, 200ms, 400ms, ..., capped at 5s.
            let shift = failures.saturating_sub(1).min(6);
            let ms = (100u32 << shift).min(5_000);
            // Backoff sleep failure is non-fatal; retry proceeds immediately if JS timer fails.
            if let Ok(promise) = self.0.sleep(ms) {
                let _ = JsFuture::from(promise).await;
            }
        }
    }

    fn err_to_js(err: impl std::fmt::Display) -> JsValue {
        JsValue::from_str(&err.to_string())
    }

    fn to_json(val: &impl Serialize) -> Result<String, JsValue> {
        serde_json::to_string(val).map_err(err_to_js)
    }

    async fn own_number(bucket: &JsBucket) -> Result<Option<u16>, JsValue> {
        identity::get(bucket).await.map(|id| id.map(|id| id.number)).map_err(err_to_js)
    }

    /// Lists all cells in the bucket (excluding mirrored copies of foreign buckets), returning a
    /// JSON array of [`CellEntry`].
    #[wasm_bindgen(js_name = "listCells")]
    pub async fn list_cells(bridge: R2Bridge) -> Result<String, JsValue> {
        let bucket = JsBucket(bridge);
        let own = own_number(&bucket).await?;
        let mut cells = std::pin::pin!(cell::listed(&bucket));
        let mut out = Vec::new();
        while let Some(cell) = cells.next().await {
            let cell = cell.map_err(err_to_js)?;
            if !is_foreign_bucket(own, cell) {
                out.push(CellEntry::from(cell));
            }
        }
        to_json(&out)
    }

    /// Reads the durable shredded-tenant set from `events`, returning a JSON array of base32
    /// tenant IDs.
    #[wasm_bindgen(js_name = "readShreddedTenants")]
    pub async fn read_shredded_tenants(bridge: R2Bridge) -> Result<String, JsValue> {
        let bucket = JsBucket(bridge);
        let shredded: Vec<String> = match events::get(&bucket).await.map_err(err_to_js)? {
            Some(log) => log.shredded_set().into_iter().map(|t| Short(t).to_string()).collect(),
            None => Vec::new(),
        };
        to_json(&shredded)
    }

    /// Runs one GC pass over a single cell, returning JSON [`CollectResult`].
    #[wasm_bindgen(js_name = "collectCell")]
    pub async fn collect_cell(
        bridge: R2Bridge,
        cell_id: &str,
        lease_ms: f64,
        sweep_since_ms: Option<f64>,
        shredded_json: &str,
        holder_str: &str,
    ) -> Result<String, JsValue> {
        let cell = parse_cell_id(cell_id).map_err(err_to_js)?;
        let shredded = Arc::new(parse_shredded(shredded_json).map_err(err_to_js)?);
        let holder = parse_holder(holder_str).map_err(err_to_js)?;
        if !lease_ms.is_finite() || lease_ms < 0.0 {
            return Err(JsValue::from_str("leaseMs must be a finite non-negative number"));
        }
        let since = match sweep_since_ms {
            Some(ms) if ms.is_finite() && ms >= 0.0 => Some(ms as u64),
            Some(_) => return Err(JsValue::from_str("sweepSinceMs must be finite non-negative")),
            None => None,
        };
        let bucket = JsBucket(bridge);
        let holding = Holding { lease: lease_ms as u64, since };
        let shared = Arc::new(bucket_cell::gc_metrics::Shared::<SweepReport>::default());
        let collector =
            Collector::new().holding(holding).shredded(shredded).observer(Some(shared.clone()));
        let result = match collector.collect_cell(&bucket, cell, holder).await {
            Ok(report) => CollectResult {
                cell: cell.to_string(),
                deleted: report.deleted,
                deleted_bytes: report.deleted_bytes,
                retired: report.retired,
                skipped: report.skipped,
                removed: report.removed,
                left: None,
                metrics: Some(shared.take()),
            },
            Err(Error::Left(left)) => CollectResult {
                cell: cell.to_string(),
                left: Some(match left {
                    Left::Held => "held",
                    Left::Swept => "swept",
                }),
                ..CollectResult::default()
            },
            Err(err) => return Err(err_to_js(err)),
        };
        to_json(&result)
    }

    /// Merges a JSON array of [`SweepReport`] objects (from fan-out `collectCell` calls) into a
    /// single aggregated JSON [`SweepReport`].
    #[wasm_bindgen(js_name = "mergeMetrics")]
    pub fn merge_metrics(reports_json: &str) -> Result<String, JsValue> {
        merge_reports(reports_json).map_err(err_to_js)
    }

    /// Runs a read-only dry-run GC pass over one cell, returning JSON [`DryRunResult`].
    #[wasm_bindgen(js_name = "dryRunCell")]
    pub async fn dry_run_cell(
        bridge: R2Bridge,
        cell_id: &str,
        shredded_json: &str,
    ) -> Result<String, JsValue> {
        let cell = parse_cell_id(cell_id).map_err(err_to_js)?;
        let shredded = Arc::new(parse_shredded(shredded_json).map_err(err_to_js)?);
        let bucket = JsBucket(bridge);
        let collector = Collector::new().shredded(shredded);
        let dry = collector.dry_run_cell(&bucket, cell).await.map_err(err_to_js)?;
        let doomed_bytes = dry.doomed.iter().map(|m| m.size).sum();
        let doomed = dry
            .doomed
            .into_iter()
            .map(|m| DoomedEntry { kind: layout::kind(&m.key).name(), key: m.key, size: m.size })
            .collect();
        to_json(&DryRunResult {
            cell: cell.to_string(),
            doomed,
            doomed_bytes,
            retired: dry.retired,
            fenced: dry.fenced,
        })
    }

    /// Reads recorded usage from a cell's collector lease (`<C>gc/lease`), returning JSON
    /// [`CellUsageResult`].
    #[wasm_bindgen(js_name = "cellUsage")]
    pub async fn cell_usage(bridge: R2Bridge, cell_id: &str) -> Result<String, JsValue> {
        let cell = parse_cell_id(cell_id).map_err(err_to_js)?;
        let bucket = JsBucket(bridge);
        let lease = passed::read(&bucket, cell).await.map_err(err_to_js)?;
        let usage = lease.as_ref().and_then(|l| l.passed.as_ref()).map(|p| &p.usage);
        let total = usage.map(passed::Usage::total);
        let kinds = usage.map(|u| {
            u.kinds
                .iter()
                .map(|(k, v)| (k.clone(), KindUsage { objects: v.objects, bytes: v.bytes }))
                .collect()
        });
        to_json(&CellUsageResult {
            tenant: Short(cell.tenant).to_string(),
            cell: cell.to_string(),
            measured_unix_ms: usage.map(|u| u.measured),
            objects: total.map(|t| t.objects),
            bytes: total.map(|t| t.bytes),
            kinds,
        })
    }

    /// Checks whether a cell is live by reading its head, returning JSON [`LiveCellResult`].
    #[wasm_bindgen(js_name = "liveCell")]
    pub async fn live_cell(bridge: R2Bridge, cell_id: &str) -> Result<String, JsValue> {
        let cell = parse_cell_id(cell_id).map_err(err_to_js)?;
        let bucket = JsBucket(bridge);
        let record = head::live(&bucket, cell).await.map_err(err_to_js)?;
        let (live, origin_parent, origin_at) = match record {
            Some(head) => match head.origin() {
                Some(o) => (true, Some(o.parent.to_string()), Some(Short(o.at).to_string())),
                None => (true, None, None),
            },
            None => (false, None, None),
        };
        to_json(&LiveCellResult { cell: cell.to_string(), live, origin_parent, origin_at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bucket_cell::gc_metrics::{CellSample, Metric as _, ObjectSample};

    #[test]
    fn merge_reports_combines_fan_out_cell_metrics() {
        let mut r1 = SweepReport::default();
        let kept1 = [ObjectSample { kind: "segment", size: 4096, modified: 1 }];
        r1.observe(&CellSample {
            cell: CellId { tenant: 2, id: 1 },
            foreign_copy: false,
            skipped: false,
            removed: false,
            now: 10,
            head_format: Some(layout::FORMAT.current),
            lease_format: Some(layout::FORMAT.current),
            live: Some(true),
            versions: Some(1),
            is_fork: Some(false),
            replicated: Some(false),
            references: 0,
            kept: &kept1,
            deleted: &[],
        });

        let mut r2 = SweepReport::default();
        let kept2 = [ObjectSample { kind: "segment", size: 8192, modified: 2 }];
        r2.observe(&CellSample {
            cell: CellId { tenant: 2, id: 2 },
            foreign_copy: false,
            skipped: true,
            removed: false,
            now: 10,
            head_format: None,
            lease_format: Some(layout::FORMAT.current),
            live: None,
            versions: None,
            is_fork: None,
            replicated: None,
            references: 0,
            kept: &kept2,
            deleted: &[],
        });

        let input = serde_json::to_string(&[r1, r2]).unwrap();
        let merged_json = merge_reports(&input).unwrap();
        let merged: SweepReport = serde_json::from_str(&merged_json).unwrap();
        assert_eq!(merged.cells.total, 2);
        assert_eq!(merged.cells.live, 1);
        assert_eq!(merged.cells.skipped, 1);
        assert_eq!(merged.stored.objects, 2);
        assert_eq!(merged.stored.bytes, 12288);
        assert_eq!(merged.tenants.len(), 1);
    }
}
