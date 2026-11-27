// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Project-wide TOML configuration for `cladefsd`, `cladefsctl`, and `cladefs-gc`.
//!
//! A single `cladefs.toml` (or per-service TOML file) configures the daemon, its managed JBOD
//! `[[disks]]` with per-disk caps and governance, bucket/KMS settings, peers, FUSE mount,
//! telemetry, CLI client defaults, and garbage collection. Unknown fields are rejected at load
//! time (`#[serde(deny_unknown_fields)]`) so typos fail hard on boot rather than being ignored.

use bucket_cell::host_disk::{DiskHeadroom, DiskPlacement, DiskPolicy};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Default environment variable pointing to a TOML configuration file when `--config` is omitted.
pub const CONFIG_ENV: &str = "CLADEFS_CONFIG";

/// Top-level project configuration loaded from TOML.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Daemon sockets, placement token, and drain policy (`cladefsd`).
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Home object-storage bucket and upload settings.
    #[serde(default)]
    pub bucket: BucketConfig,
    /// Optional per-bucket overrides keyed by bucket number (`[buckets.2]`, etc.).
    #[serde(default)]
    pub buckets: BTreeMap<u16, BucketOverrideConfig>,
    /// Key management configuration (external KMS URL or bucket KMS master key).
    #[serde(default)]
    pub kms: KmsConfig,
    /// Daemon memory cache and memory governor settings.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Global disk governance defaults and ephemeral metadata journal location.
    #[serde(default)]
    pub disk: DiskDefaultsConfig,
    /// Managed local disks (JBOD pool), each with independent caps and governance.
    #[serde(default)]
    pub disks: Vec<DiskEntryConfig>,
    /// Peer-to-peer part sharing, replication, and live migration settings.
    #[serde(default)]
    pub peers: PeersConfig,
    /// Optional host FUSE multi-cell mount served by `cladefsd`.
    #[serde(default)]
    pub mount: Option<MountConfig>,
    /// Metrics, z-pages, tracing, stats file, and logging settings.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// `cladefsctl` client defaults.
    #[serde(default)]
    pub ctl: CtlConfig,
    /// `cladefs-gc` garbage collector settings.
    #[serde(default)]
    pub gc: GcConfig,
}

/// `[daemon]` section: sockets, placement token, and drain behaviour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Default VM socket path (`cladefsd` own socket, mode 0600).
    #[serde(default)]
    pub vm_socket: Option<PathBuf>,
    /// Control gRPC Unix socket path (mode 0600).
    #[serde(default)]
    pub control_socket: Option<PathBuf>,
    /// Directory where granted tenant VM sockets are created (defaults to `tenants` beside
    /// `control_socket`).
    #[serde(default)]
    pub tenants_dir: Option<PathBuf>,
    /// Placement fencing token from the control plane (must be non-zero for `cladefsd`).
    #[serde(default)]
    pub token: Option<u64>,
    /// Maximum seconds to wait for clean drain on SIGTERM/SIGINT/SIGHUP (default 60).
    #[serde(default)]
    pub drain_secs: Option<u64>,
    /// Polling interval in milliseconds for global control-plane bucket events.
    #[serde(default)]
    pub events_poll_ms: Option<u64>,
}

/// `[bucket]` section: home bucket URL and uploader tuning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfig {
    /// Home bucket URL (`s3://...`, `gs://...`, `az://...`, `file:///...`, `memory:///...`).
    #[serde(default)]
    pub url: Option<String>,
    /// Maximum bytes of in-flight upload buffers across all open cells (default 512 MiB).
    #[serde(default)]
    pub upload_memory: Option<ByteSize>,
    /// Optional zstd compression level for framed segments.
    #[serde(default)]
    pub zstd_level: Option<i32>,
}

/// `[buckets.<N>]` per-bucket credential and KMS overrides (replaces `CLADEFSD_BUCKET_<N>_*`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketOverrideConfig {
    /// Additional key-value environment overrides applied for bucket `N` (e.g. `AWS_REGION`,
    /// `GOOGLE_SERVICE_ACCOUNT`, `CLADEFS_KMS`, `CLADEFS_MASTER_KEY_FILE`).
    #[serde(default, flatten)]
    pub env: BTreeMap<String, String>,
}

/// `[kms]` section: encryption key management.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmsConfig {
    /// External KMS URL (`aws-kms://...`, `gcp-kms://...`, `openbao://...`, `local:///...`).
    #[serde(default)]
    pub url: Option<String>,
    /// Inline master key(s) for bucket KMS (prefer `master_key_file` in production).
    #[serde(default)]
    pub master_key: Option<String>,
    /// File holding bucket KMS master key(s).
    #[serde(default)]
    pub master_key_file: Option<PathBuf>,
    /// Allowed KMS hostnames (`CLADEFSD_KMS_HOSTS`).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Allow `local://` KMS URLs (for development/tests; `CLADEFSD_KMS_LOCAL`).
    #[serde(default)]
    pub allow_local: Option<bool>,
}

/// `[memory]` section: extent page cache and memory governor settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Fixed memory limit in bytes; when omitted, the memory governor sizes dynamically.
    #[serde(default)]
    pub bytes: Option<ByteSize>,
    /// Free memory headroom left for VMs and host (`default: max(host/16, 1GiB)`).
    #[serde(default)]
    pub headroom: Option<ByteSize>,
    /// Minimum memory floor kept by the governor before declaring the host full (default:
    /// `limit / 8`).
    #[serde(default)]
    pub floor: Option<ByteSize>,
    /// Allow proactive VM `memory.reclaim` without `CAP_DAC_OVERRIDE` when VM cgroups are
    /// delegated to `cladefsd`'s user (`CLADEFSD_VM_CGROUP_DELEGATED`).
    #[serde(default)]
    pub vm_cgroup_delegated: Option<bool>,
}

/// `[disk]` section: global disk defaults shared across `[[disks]]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskDefaultsConfig {
    /// Optional aggregate hard cap across all managed disks combined.
    #[serde(default)]
    pub cap: Option<ByteSize>,
    /// Default headroom left free per disk (`bytes` or `"N%"`; default `max(5%, 4GiB)`).
    #[serde(default)]
    pub headroom: Option<HeadroomSpec>,
    /// Optional aggregate part-cache cap across all shards.
    #[serde(default)]
    pub cache_cap: Option<ByteSize>,
    /// Optional default per-disk `PageCache` dirty-page hard ceiling.
    #[serde(default)]
    pub dirty_budget: Option<ByteSize>,
    /// Small metadata journal directory for ephemeral cells (`ephemeral-host` format v2).
    #[serde(default)]
    pub ephemeral_journal: Option<PathBuf>,
    /// Whether the daemon opens a part-cache shard on each managed disk (default `true`).
    #[serde(default = "default_true")]
    pub part_cache: bool,
}

impl Default for DiskDefaultsConfig {
    fn default() -> Self {
        Self {
            cap: None,
            headroom: None,
            cache_cap: None,
            dirty_budget: None,
            ephemeral_journal: None,
            part_cache: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// One `[[disks]]` entry describing a managed local SSD / filesystem in the daemon's JBOD pool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskEntryConfig {
    /// Stable operator-facing identifier for this disk (defaults to `disk-<index>`).
    #[serde(default)]
    pub id: Option<String>,
    /// Root directory owned by `cladefsd` on this filesystem (`cells/` and `part-cache/`
    /// are managed automatically inside it).
    pub path: PathBuf,
    /// Per-disk hard byte cap (`cells + reserve + pledged + cache`). Unset = uncapped (governed
    /// by filesystem free space minus headroom).
    #[serde(default)]
    pub cap: Option<ByteSize>,
    /// Per-disk headroom override (`bytes` or `"N%"`), overriding `[disk].headroom`.
    #[serde(default)]
    pub headroom: Option<HeadroomSpec>,
    /// Per-disk hard byte cap for the part-cache shard on this disk.
    #[serde(default)]
    pub cache_cap: Option<ByteSize>,
    /// Per-disk `PageCache` dirty-page hard ceiling override.
    #[serde(default)]
    pub dirty_budget: Option<ByteSize>,
    /// Placement/write policy for this disk (`"read_write"`, `"drain"`, or `"read_only"`).
    #[serde(default)]
    pub placement: PlacementSpec,
}

/// `[peers]` section: peer-to-peer part sharing and live migration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeersConfig {
    /// This host's `HOST:PORT` peer service bind/advertised address.
    #[serde(default)]
    pub address: Option<String>,
    /// Initial peer `HOST:PORT` list.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Directory containing mTLS credentials (`ca.crt`, `peer.crt`, `peer.key`).
    #[serde(default)]
    pub tls_dir: Option<PathBuf>,
    /// Seconds an idle incoming migration is kept before timing out.
    #[serde(default)]
    pub migration_idle_secs: Option<u64>,
}

/// `[mount]` section: host FUSE multi-cell mount served by `cladefsd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    /// Host directory where attached cells appear at `<mountpoint>/<TENANT>/<CELL>`.
    pub mountpoint: PathBuf,
    /// Enable FUSE `allow_other` mount option.
    #[serde(default)]
    pub allow_other: bool,
}

/// `[telemetry]` section: observability, tracing, stats, and logging.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// HTTP bind address (`HOST:PORT`) for `/metrics` (and `/z/` unless `zpages` overrides).
    #[serde(default)]
    pub metrics: Option<String>,
    /// Separate `/z/` bind address, `"off"`, or omitted to share `metrics`.
    #[serde(default)]
    pub zpages: Option<String>,
    /// Optional periodic text stats file path.
    #[serde(default)]
    pub stats_file: Option<PathBuf>,
    /// Optional directory for per-VM boot read traces.
    #[serde(default)]
    pub read_traces_dir: Option<PathBuf>,
    /// Export spans over OTLP (`OTEL_EXPORTER_OTLP_ENDPOINT`).
    #[serde(default)]
    pub otlp: bool,
    /// Log level filter (e.g. `"info"`, `"info,bucket_cell::object_bucket=debug"`).
    #[serde(default)]
    pub log_level: Option<String>,
    /// Log output format (`"text"` or `"json"`).
    #[serde(default)]
    pub log_format: Option<String>,
}

/// `[ctl]` section: `cladefsctl` client defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtlConfig {
    /// Control gRPC socket (falls back to `[daemon].control_socket`).
    #[serde(default)]
    pub control_socket: Option<PathBuf>,
    /// Default RPC timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `[gc]` section: `cladefs-gc` garbage collector configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcConfig {
    /// Bucket URL to collect (falls back to `[bucket].url`).
    #[serde(default)]
    pub bucket: Option<String>,
    /// Continuous sweep cadence in seconds (`None` = single pass).
    #[serde(default)]
    pub every_secs: Option<u64>,
    /// Concurrent cell passes (default 16).
    #[serde(default)]
    pub at_once: Option<usize>,
    /// Per-cell collector lease duration in seconds (default 600).
    #[serde(default)]
    pub lease_secs: Option<u64>,
    /// Prometheus textfile metrics output path.
    #[serde(default)]
    pub metrics_file: Option<PathBuf>,
    /// Maximum objects deleted per sweep.
    #[serde(default)]
    pub max_delete_objects: Option<u64>,
    /// Maximum bytes deleted per sweep.
    #[serde(default)]
    pub max_delete_bytes: Option<ByteSize>,
    /// Maximum bucket requests per second.
    #[serde(default)]
    pub max_requests_per_second: Option<u64>,
    /// Maximum consecutive refused passes before aborting sweep.
    #[serde(default)]
    pub max_refused_passes: Option<u64>,
}

/// Byte count deserialized from either an integer (`1073741824`) or a suffixed string
/// (`"500GiB"`, `"1.5TiB"`, `"256MiB"`, `"64KiB"`, `"100GB"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// Raw byte count.
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Parses a byte size string with optional binary (`KiB`/`MiB`/`GiB`/`TiB`) or decimal
    /// (`KB`/`MB`/`GB`/`TB`/`B`) suffix.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim();
        if s.is_empty() {
            return Err("byte size must not be empty".to_owned());
        }
        if let Ok(n) = s.parse::<u64>() {
            return Ok(Self(n));
        }
        let split = s
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
            .ok_or_else(|| format!("invalid byte size {raw:?}"))?;
        let num_str = s[..split].replace('_', "");
        let unit = s[split..].trim();
        let mult: u64 = match unit.to_ascii_lowercase().as_str() {
            "b" => 1,
            "k" | "kb" => 1_000,
            "m" | "mb" => 1_000_000,
            "g" | "gb" => 1_000_000_000,
            "t" | "tb" => 1_000_000_000_000,
            "kib" | "ki" => 1 << 10,
            "mib" | "mi" => 1 << 20,
            "gib" | "gi" => 1 << 30,
            "tib" | "ti" => 1 << 40,
            _ => return Err(format!("unknown byte size unit {unit:?} in {raw:?}")),
        };
        if let Ok(int) = num_str.parse::<u64>() {
            return int
                .checked_mul(mult)
                .map(Self)
                .ok_or_else(|| format!("byte size overflow in {raw:?}"));
        }
        let val: f64 = num_str.parse().map_err(|_| format!("invalid byte size {raw:?}"))?;
        if !val.is_finite() || val < 0.0 {
            return Err(format!("invalid byte size {raw:?}"));
        }
        let scaled = val * (mult as f64);
        if scaled > (u64::MAX as f64) {
            return Err(format!("byte size overflow in {raw:?}"));
        }
        Ok(Self(scaled.round() as u64))
    }
}

impl std::str::FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ByteSize;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a non-negative byte integer or size string like \"500GiB\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(ByteSize(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::custom("byte size must be non-negative"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                ByteSize::parse(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Per-disk or global headroom specification in TOML (`"5%"`, `"4GiB"`, or integer bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadroomSpec(pub DiskHeadroom);

impl HeadroomSpec {
    /// Parses a headroom string (`"5%"` or byte size like `"4GiB"`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim();
        if let Some(pct) = s.strip_suffix('%') {
            let pct: u64 = pct.parse().map_err(|_| format!("invalid headroom percent {raw:?}"))?;
            if pct >= 100 {
                return Err(format!("headroom percent must be < 100%, got {raw:?}"));
            }
            return Ok(Self(DiskHeadroom::Percent(pct)));
        }
        ByteSize::parse(s).map(|b| Self(DiskHeadroom::Bytes(b.0)))
    }
}

impl Serialize for HeadroomSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            DiskHeadroom::Bytes(b) => serializer.serialize_u64(b),
            DiskHeadroom::Percent(p) => serializer.serialize_str(&format!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for HeadroomSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = HeadroomSpec;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bytes (integer or \"4GiB\") or percentage string (\"5%\")")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(HeadroomSpec(DiskHeadroom::Bytes(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(|b| HeadroomSpec(DiskHeadroom::Bytes(b)))
                    .map_err(|_| E::custom("headroom must be non-negative"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                HeadroomSpec::parse(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// TOML spelling of [`DiskPlacement`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementSpec {
    /// Normal read/write placement.
    #[default]
    ReadWrite,
    /// Stop placing new cells here while existing cells keep running.
    Drain,
    /// Refuse new cells, writes, and part-cache fills.
    ReadOnly,
}

impl From<PlacementSpec> for DiskPlacement {
    fn from(spec: PlacementSpec) -> Self {
        match spec {
            PlacementSpec::ReadWrite => Self::ReadWrite,
            PlacementSpec::Drain => Self::Drain,
            PlacementSpec::ReadOnly => Self::ReadOnly,
        }
    }
}

impl DiskEntryConfig {
    /// Effective human-readable ID for this disk entry (`id` if given, else `disk-<index>`).
    pub fn effective_id(&self, index: usize) -> String {
        self.id.clone().unwrap_or_else(|| format!("disk-{index}"))
    }

    /// Converts this entry's governance fields into a [`DiskPolicy`].
    pub fn to_policy(&self, index: usize, defaults: &DiskDefaultsConfig) -> DiskPolicy {
        DiskPolicy {
            id: Some(self.effective_id(index)),
            cap: self.cap.map(ByteSize::bytes),
            headroom: self.headroom.map(|h| h.0),
            cache_cap: self.cache_cap.map(ByteSize::bytes),
            dirty_budget: self.dirty_budget.or(defaults.dirty_budget).map(ByteSize::bytes),
            placement: self.placement.into(),
        }
    }
}

impl Config {
    /// Parses and validates a [`Config`] from a TOML string.
    pub fn from_toml_str(src: &str) -> Result<Self, String> {
        let cfg: Self = toml::from_str(src).map_err(|err| format!("parsing TOML config: {err}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reads, parses, and validates a [`Config`] from `path`.
    pub fn from_toml_file(path: &Path) -> Result<Self, String> {
        let bytes = host_fs::read(path)
            .map_err(|err| format!("reading config {}: {err}", path.display()))?;
        let content = String::from_utf8(bytes)
            .map_err(|err| format!("config {} is not valid UTF-8: {err}", path.display()))?;
        Self::from_toml_str(&content).map_err(|err| format!("in config {}: {err}", path.display()))
    }

    /// Validates cross-field invariants (distinct disk IDs and paths, positive caps, non-zero
    /// placement token when specified, valid log format, etc.).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(0) = self.daemon.token {
            return Err("daemon.token must be non-zero".to_owned());
        }
        if let Some(cap) = self.disk.cap
            && cap.bytes() == 0
        {
            return Err("disk.cap must be positive".to_owned());
        }
        if let Some(cap) = self.disk.cache_cap
            && cap.bytes() == 0
        {
            return Err("disk.cache_cap must be positive".to_owned());
        }
        if let Some(budget) = self.disk.dirty_budget
            && budget.bytes() == 0
        {
            return Err("disk.dirty_budget must be positive".to_owned());
        }
        let mut seen_ids = HashSet::new();
        let mut seen_paths = HashSet::new();
        for (i, disk) in self.disks.iter().enumerate() {
            if disk.path.as_os_str().is_empty() {
                return Err(format!("disks[{i}].path must not be empty"));
            }
            let id = disk.effective_id(i);
            if id.is_empty() {
                return Err(format!("disks[{i}].id must not be empty"));
            }
            if !seen_ids.insert(id.clone()) {
                return Err(format!("duplicate disk id {id:?}"));
            }
            if !seen_paths.insert(disk.path.clone()) {
                return Err(format!("duplicate disk path {}", disk.path.display()));
            }
            if let Some(cap) = disk.cap
                && cap.bytes() == 0
            {
                return Err(format!("disks[{i}] ({id}): cap must be positive"));
            }
            if let Some(cap) = disk.cache_cap
                && cap.bytes() == 0
            {
                return Err(format!("disks[{i}] ({id}): cache_cap must be positive"));
            }
            if let Some(budget) = disk.dirty_budget
                && budget.bytes() == 0
            {
                return Err(format!("disks[{i}] ({id}): dirty_budget must be positive"));
            }
        }
        if !self.peers.peers.is_empty() && self.peers.address.is_none() {
            return Err("peers.peers is set, but peers.address is unset".to_owned());
        }
        if let Some(fmt) = &self.telemetry.log_format
            && !matches!(fmt.as_str(), "text" | "json")
        {
            return Err(format!("telemetry.log_format must be \"text\" or \"json\", got {fmt:?}"));
        }
        Ok(())
    }

    /// Validates that this config has everything `cladefsd` requires to boot (at least one managed
    /// disk, home bucket URL, and non-zero placement token).
    pub fn validate_for_daemon(&self) -> Result<(), String> {
        self.validate()?;
        if self.disks.is_empty() {
            return Err("at least one [[disks]] entry is required for cladefsd".to_owned());
        }
        if self.bucket.url.as_deref().is_none_or(str::is_empty) {
            return Err("bucket.url is required for cladefsd".to_owned());
        }
        if self.daemon.token.is_none_or(|t| t == 0) {
            return Err("daemon.token (non-zero) is required for cladefsd".to_owned());
        }
        Ok(())
    }

    /// Returns the effective control socket path for `cladefsctl`.
    pub fn control_socket(&self) -> Option<&Path> {
        self.ctl.control_socket.as_deref().or(self.daemon.control_socket.as_deref())
    }

    /// cladefs: the environment-backed process settings this configuration gives (bucket URL,
    /// token, KMS, per-bucket overrides, logging, compression, telemetry), by variable: what
    /// [`Config::apply_process_env`] exports, and what a process embedding the daemon reads over
    /// its own environment instead (`node_local::env::set_embedded_vars`, board 13471).
    pub fn process_env(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = Vec::new();
        let mut set = |k: &str, v: &str| vars.push((k.to_owned(), v.to_owned()));
        if let Some(url) = &self.bucket.url {
            set("CLADEFSD_BUCKET", url);
        }
        if let Some(token) = self.daemon.token {
            set("CLADEFSD_TOKEN", &token.to_string());
        }
        if let Some(bytes) = self.bucket.upload_memory {
            set("CLADEFSD_UPLOAD_MEMORY", &bytes.bytes().to_string());
        }
        if let Some(level) = self.bucket.zstd_level {
            set("CLADEFS_ZSTD_LEVEL", &level.to_string());
        }
        for (num, ov) in &self.buckets {
            for (k, v) in &ov.env {
                let key = format!("CLADEFSD_BUCKET_{num}_{}", k.to_ascii_uppercase());
                set(&key, v);
            }
        }
        if let Some(url) = &self.kms.url {
            set("CLADEFS_KMS", url);
        }
        if let Some(key) = &self.kms.master_key {
            set("CLADEFS_MASTER_KEY", key);
        }
        if let Some(file) = &self.kms.master_key_file {
            set("CLADEFS_MASTER_KEY_FILE", &file.to_string_lossy());
        }
        if !self.kms.allowed_hosts.is_empty() {
            set("CLADEFSD_KMS_HOSTS", &self.kms.allowed_hosts.join(","));
        }
        if let Some(allow_local) = self.kms.allow_local {
            set("CLADEFSD_KMS_LOCAL", if allow_local { "1" } else { "0" });
        }
        if let Some(drain) = self.daemon.drain_secs {
            set("CLADEFSD_DRAIN", &drain.to_string());
        }
        if let Some(poll_ms) = self.daemon.events_poll_ms {
            set("CLADEFSD_EVENTS_POLL_MS", &poll_ms.to_string());
        }
        if let Some(headroom) = self.memory.headroom {
            set("CLADEFSD_HEADROOM", &headroom.bytes().to_string());
        }
        if let Some(floor) = self.memory.floor {
            set("CLADEFSD_MEMORY_FLOOR", &floor.bytes().to_string());
        }
        if let Some(delegated) = self.memory.vm_cgroup_delegated {
            set("CLADEFSD_VM_CGROUP_DELEGATED", if delegated { "1" } else { "0" });
        }
        if let Some(idle) = self.peers.migration_idle_secs {
            set("CLADEFSD_MIGRATION_IDLE", &idle.to_string());
        }
        if let Some(tls) = &self.peers.tls_dir {
            set("CLADEFSD_PEER_TLS", &tls.to_string_lossy());
        }
        if let Some(metrics) = &self.telemetry.metrics {
            set("CLADEFSD_METRICS", metrics);
        }
        if let Some(zpages) = &self.telemetry.zpages {
            set("CLADEFSD_ZPAGES", zpages);
        }
        if let Some(stats) = &self.telemetry.stats_file {
            set("CLADEFSD_STATS", &stats.to_string_lossy());
        }
        if let Some(traces) = &self.telemetry.read_traces_dir {
            set("CLADEFSD_READ_TRACES", &traces.to_string_lossy());
        }
        if self.telemetry.otlp {
            set("CLADEFSD_TRACES", "otlp");
        }
        if let Some(level) = &self.telemetry.log_level {
            set("CLADEFS_LOG_LEVEL", level);
        }
        if let Some(format) = &self.telemetry.log_format {
            set("CLADEFS_LOG_FORMAT", format);
        }
        if let Some(ctl) = self.control_socket() {
            set("CLADEFSD_GRPC", &ctl.to_string_lossy());
        }
        if let Some(timeout) = self.ctl.timeout_secs {
            set("CLADEFSCTL_TIMEOUT", &timeout.to_string());
        }
        vars
    }

    /// Exports environment-backed process settings (bucket URL, token, KMS, per-bucket overrides,
    /// logging, compression, telemetry) before threads start in CLI binaries (`cladefsd`,
    /// `cladefs-gc`, `cladefsctl`).
    ///
    /// # Safety
    /// Must only be called during single-threaded process startup before spawning worker threads.
    pub unsafe fn apply_process_env(&self) {
        for (name, value) in self.process_env() {
            // SAFETY: caller guarantees single-threaded process startup before spawning threads.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_toml_with_per_disk_caps_and_governance() {
        let raw = r#"
[daemon]
vm_socket = "/run/cladefsd/vm.sock"
control_socket = "/run/cladefsd/control.sock"
token = 42
drain_secs = 90

[bucket]
url = "s3://prod-cladefs/v1"
upload_memory = "512MiB"

[buckets.2]
aws_region = "eu-west-1"

[kms]
url = "aws-kms://us-east-1"

[memory]
headroom = "2GiB"
floor = "512MiB"

[disk]
cap = "2TiB"
headroom = "5%"
cache_cap = "1TiB"
dirty_budget = "128MiB"
ephemeral_journal = "/var/lib/cladefsd/ephemeral"

[[disks]]
id = "os-nvme"
path = "/mnt/nvme0/cladefsd"
cap = "500GiB"
headroom = "15%"
cache_cap = "200GiB"
dirty_budget = "64MiB"
placement = "read_write"

[[disks]]
id = "scratch-nvme"
path = "/mnt/nvme1/cladefsd"
cap = "1.5TiB"
placement = "drain"

[peers]
address = "10.0.0.1:9185"
peers = ["10.0.0.2:9185"]
tls_dir = "/etc/cladefs/peer-tls"

[mount]
mountpoint = "/mnt/cladefs"
allow_other = true

[telemetry]
metrics = "127.0.0.1:9184"
zpages = "off"
log_level = "info"
log_format = "json"
"#;
        let cfg = Config::from_toml_str(raw).unwrap();
        cfg.validate_for_daemon().unwrap();
        assert_eq!(cfg.daemon.token, Some(42));
        assert_eq!(cfg.bucket.upload_memory, Some(ByteSize(512 << 20)));
        assert_eq!(cfg.disk.cap, Some(ByteSize(2 << 40)));
        assert_eq!(cfg.disk.headroom, Some(HeadroomSpec(DiskHeadroom::Percent(5))));
        assert_eq!(cfg.disks.len(), 2);
        let p0 = cfg.disks[0].to_policy(0, &cfg.disk);
        assert_eq!(p0.id.as_deref(), Some("os-nvme"));
        assert_eq!(p0.cap, Some(500 << 30));
        assert_eq!(p0.headroom, Some(DiskHeadroom::Percent(15)));
        assert_eq!(p0.cache_cap, Some(200 << 30));
        assert_eq!(p0.dirty_budget, Some(64 << 20));
        assert_eq!(p0.placement, DiskPlacement::ReadWrite);
        let p1 = cfg.disks[1].to_policy(1, &cfg.disk);
        assert_eq!(p1.cap, Some(1536 << 30));
        assert_eq!(p1.dirty_budget, Some(128 << 20));
        assert_eq!(p1.placement, DiskPlacement::Drain);
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_disk_configs() {
        assert!(Config::from_toml_str("bogus_key = 1\n").is_err());
        assert!(Config::from_toml_str("[daemon]\ntoken = 0\n").is_err());
        assert!(
            Config::from_toml_str(
                "[[disks]]\nid = \"a\"\npath = \"/d1\"\n[[disks]]\nid = \"a\"\npath = \"/d2\"\n"
            )
            .is_err()
        );
        assert!(Config::from_toml_str("[[disks]]\npath = \"/d1\"\ncap = 0\n").is_err());
        assert!(Config::from_toml_str("[disk]\nheadroom = \"100%\"\n").is_err());
    }

    #[test]
    fn omitting_disk_table_keeps_part_cache_enabled_by_default() {
        let omitted = Config::from_toml_str(
            r#"
[daemon]
vm_socket = "/run/cladefsd.sock"
[bucket]
url = "memory:///omitted"
[[disks]]
path = "/var/lib/cladefs/d0"
"#,
        )
        .unwrap();
        let empty = Config::from_toml_str(
            r#"
[daemon]
vm_socket = "/run/cladefsd.sock"
[bucket]
url = "memory:///empty"
[disk]
[[disks]]
path = "/var/lib/cladefs/d0"
"#,
        )
        .unwrap();
        assert!(omitted.disk.part_cache, "omitting [disk] must default part_cache to true");
        assert_eq!(omitted.disk, empty.disk);
    }
}
