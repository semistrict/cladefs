// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's log, on standard error: what is as grave as information or more, each record a
//! line, its key-values after its message (TASK-107), as `fxfs::log::error!(error:?; "...")`
//! gives the cause of what it says went wrong there alone. The daemon's tests log so too, which
//! nextest shows of a test that fails.
//!
//! cladefs: a record about a guest request, the work on its thread being for one
//! (`crate::request::current`), names it: `request_id`, the guest's number of it, and `store`, the
//! cell whose VM process asked, which tells one VM's requests from another's (TASK-260). A record
//! logged while a span is current names it too, `trace_id` and `span_id`, where it is to be found
//! in its trace (`bucket_cell::trace::current_ids`, board 8571).
//!
//! Two variables set it, read as the log is set up:
//!
//! - `CLADEFS_LOG_FORMAT`: `text`, the default, a line of the record's time (RFC 3339, UTC, to the
//!   microsecond), level, target and message, then its key-values, `key=value`, its control
//!   characters escaped (`\n`, `\r`, `\t`, `\x1b`, `\u{85}`), so that no message or value, as a
//!   path a VM asked for, forges a record of its own (board 8571); or `json`: each record a JSON
//!   object, on a line of its own, of its `timestamp` (RFC 3339, UTC, to the microsecond), `level`,
//!   `target` and `message`, its request's `request_id` and `store` if it is about one, its span's
//!   `trace_id` and `span_id` if one is current, and then each of its key-values as a field of its
//!   own: a number or boolean as such, anything else as its text. A key-value named as one of those
//!   fields is `kv_` and its name (TASK-259).
//! - `CLADEFS_LOG_LEVEL`: the gravest records logged, `info` by default: `off`, `error`, `warn`,
//!   `info`, `debug` or `trace`, then, after commas, `TARGET=LEVEL`s for the targets (module paths)
//!   at or under TARGET, the longest such that names one deciding: as
//!   `info,bucket_cell::object_bucket=debug`, which logs each request of object storage too.
//!   cladefs: changed while the process runs by [`set_levels`], the daemon's `POST /z/log`
//!   (`super::zpages`, board 8571), and put back.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::sync::{OnceLock, PoisonError, RwLock};
use std::time::SystemTime;

/// How the log writes its records: `CLADEFS_LOG_FORMAT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// A line of its level, target, message and key-values, `key=value`.
    Text,
    /// A JSON object, of a field for each of those, and the time.
    Json,
}

/// What the log takes and how it writes it: the variables the module says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// How it writes its records.
    pub format: Format,
    /// The gravest records logged of any target not named in `targets`.
    pub level: log::LevelFilter,
    /// Targets' own, each of what is at or under its target.
    pub targets: Vec<(String, log::LevelFilter)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { format: Format::Text, level: log::LevelFilter::Info, targets: Vec::new() }
    }
}

impl Settings {
    /// What this process's environment says.
    pub fn from_env() -> Result<Self, String> {
        // Unset, or not UTF-8, which no value it takes is: none.
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// What the variables `var` gives say.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut settings = Self::default();
        match var("CLADEFS_LOG_FORMAT").as_deref() {
            None | Some("" | "text") => {}
            Some("json") => settings.format = Format::Json,
            Some(other) => {
                return Err(format!("CLADEFS_LOG_FORMAT is text or json, not {other:?}"));
            }
        }
        settings.set_levels(&var("CLADEFS_LOG_LEVEL").unwrap_or_default())?;
        Ok(settings)
    }

    /// Has it log what `levels` says, as `CLADEFS_LOG_LEVEL` says it: the gravest records logged,
    /// unless given, the default's, then `TARGET=LEVEL`s, after commas. Why not, if it says nothing
    /// that is.
    fn set_levels(&mut self, levels: &str) -> Result<(), String> {
        let level = |text: &str| {
            text.parse::<log::LevelFilter>()
                .map_err(|_| format!("CLADEFS_LOG_LEVEL: {text:?} is no level (off to trace)"))
        };
        let (mut default, mut targets) = (Self::default().level, Vec::new());
        for part in levels.split(',').map(str::trim).filter(|part| !part.is_empty()) {
            match part.split_once('=') {
                Some((target, of)) => targets.push((target.trim().to_owned(), level(of.trim())?)),
                None => default = level(part)?,
            }
        }
        (self.level, self.targets) = (default, targets);
        Ok(())
    }

    /// cladefs: what it logs, as `CLADEFS_LOG_LEVEL` would say it.
    fn levels(&self) -> String {
        let mut levels = self.level.as_str().to_ascii_lowercase();
        for (target, level) in &self.targets {
            let _ = write!(levels, ",{target}={}", level.as_str().to_ascii_lowercase());
        }
        levels
    }

    /// The gravest records of `target` logged.
    fn level_of(&self, target: &str) -> log::LevelFilter {
        let under = |of: &str| {
            target.strip_prefix(of).is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        };
        (self.targets.iter().filter(|(of, _)| under(of)))
            .max_by_key(|(of, _)| of.len())
            .map_or(self.level, |&(_, level)| level)
    }

    /// The gravest records of any target logged.
    fn max_level(&self) -> log::LevelFilter {
        self.targets.iter().map(|&(_, level)| level).fold(self.level, std::cmp::max)
    }
}

/// Logs to standard error from now on, as the environment says ([`Settings::from_env`]), unless the
/// process has a log already.
pub fn to_stderr() -> anyhow::Result<()> {
    install(Settings::from_env().map_err(anyhow::Error::msg)?)?;
    Ok(())
}

/// Logs to standard error from now on, as `settings` says, unless the process has a log already.
pub fn install(settings: Settings) -> Result<(), log::SetLoggerError> {
    install_to(settings, Sink::Stderr)
}

fn install_to(settings: Settings, sink: Sink) -> Result<(), log::SetLoggerError> {
    let max = settings.max_level();
    let format = settings.format;
    let logger =
        Logger { format, started: settings.clone(), settings: RwLock::new(settings), sink };
    // The process's for as long as it runs, as `log` keeps it: leaked, once.
    let logger: &'static Logger = Box::leak(Box::new(logger));
    log::set_logger(logger)?;
    log::set_max_level(max);
    // Only the first to set the process's log gets here.
    let _ = INSTALLED.set(logger);
    Ok(())
}

/// cladefs: the log this module installed, if it did ([`install`]), whose levels [`set_levels`]
/// changes.
// The process's log, as `log`'s is: every simulated host's, as their records go to one stderr.
static INSTALLED: OnceLock<&'static Logger> = OnceLock::new();

/// cladefs: what the process logs, as `CLADEFS_LOG_LEVEL` would say it; why not, if it logs through
/// another log than this module's.
pub fn levels() -> Result<String, String> {
    Ok(installed()?.read().levels())
}

/// cladefs: has the process log what `levels` says from now on, as `CLADEFS_LOG_LEVEL` says it;
/// or, if none, what it was installed with (board 8571): what it logs then, or why not.
pub fn set_levels(levels: Option<&str>) -> Result<String, String> {
    let logger = installed()?;
    let mut settings = logger.settings.write().unwrap_or_else(PoisonError::into_inner);
    let mut changed = settings.clone();
    match levels {
        Some(levels) => changed.set_levels(levels)?,
        None => changed = logger.started.clone(),
    }
    *settings = changed;
    log::set_max_level(settings.max_level());
    Ok(settings.levels())
}

/// cladefs: the log this module installed, or why there is none.
fn installed() -> Result<&'static Logger, String> {
    INSTALLED.get().copied().ok_or_else(|| "the process logs through another log".to_owned())
}

/// Logs into what it returns from now on, each record a line, as the defaults say, but for its
/// level, debug: what tests of what is logged read. Once a process, as a log is.
#[cfg(any(test, feature = "testing"))]
pub fn capture() -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let lines = std::sync::Arc::default();
    let settings = Settings { level: log::LevelFilter::Debug, ..Settings::default() };
    install_to(settings, Sink::Captured(std::sync::Arc::clone(&lines)))
        .expect("the test's process has no log yet");
    lines
}

struct Logger {
    /// How it writes its records, whatever its levels.
    format: Format,
    /// What it logs now, which [`set_levels`] changes.
    settings: RwLock<Settings>,
    /// What it was installed with, which [`set_levels`] puts back.
    started: Settings,
    sink: Sink,
}

impl Logger {
    /// What it logs now.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Settings> {
        // Replaced whole, which no panic leaves half done: poisoned, as it is.
        self.settings.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where a [`Logger`] writes its lines.
enum Sink {
    Stderr,
    #[cfg(any(test, feature = "testing"))]
    Captured(std::sync::Arc<std::sync::Mutex<Vec<String>>>),
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.read().level_of(metadata.target())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let trace = bucket_cell::trace::current_ids();
        let request = cladefs_core::request::current();
        let mut line = line(self.format, record, request, trace, SystemTime::now());
        match &self.sink {
            Sink::Stderr => {
                line.push('\n');
                // One write of the line whole, which no other thread's comes between.
                drop(std::io::stderr().lock().write_all(line.as_bytes()));
            }
            #[cfg(any(test, feature = "testing"))]
            Sink::Captured(lines) => lines.lock().unwrap().push(line),
        }
    }

    fn flush(&self) {}
}

/// `record`'s line, in `format`, about `request`, if any, within the span of `trace`, its trace's
/// ID and its own, if any, logged at `now`.
pub fn line(
    format: Format,
    record: &log::Record<'_>,
    request: Option<cladefs_core::request::RequestId>,
    trace: Option<(String, String)>,
    now: SystemTime,
) -> String {
    match format {
        Format::Text => Text { record, request, trace, now }.to_string(),
        Format::Json => json(record, request, trace, now),
    }
}

/// `record`'s text: its time, level, target and message, then each key-value, `key=value`, its
/// request's, if any, and its span's; what it was given escaped ([`Escaped`]).
struct Text<'a> {
    record: &'a log::Record<'a>,
    request: Option<cladefs_core::request::RequestId>,
    trace: Option<(String, String)>,
    now: SystemTime,
}

/// Formats `SystemTime` as UTC RFC 3339 with six fractional digits (`YYYY-MM-DDTHH:MM:SS.ffffffZ`).
struct Rfc3339Micros(SystemTime);

impl fmt::Display for Rfc3339Micros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        bucket_cell::clock::write_rfc3339_utc(f, self.0, false)
    }
}

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.record;
        let now = Rfc3339Micros(self.now);
        write!(f, "{now} {} {}: ", record.level(), record.target())?;
        write!(Escaped(&mut *f), "{}", record.args())?;
        record.key_values().visit(&mut KeyValues(Escaped(&mut *f))).map_err(|_| fmt::Error)?;
        if let Some(request) = &self.request {
            write!(f, " request_id={}", request.guest)?;
            if let Some(open_cell) = &request.open_cell {
                write!(Escaped(&mut *f), " store={open_cell}")?;
            }
        }
        if let Some((trace, span)) = &self.trace {
            write!(f, " trace_id={trace} span_id={span}")?;
        }
        Ok(())
    }
}

/// cladefs: writes what it is written into what it holds, each control character escaped: `\n`,
/// `\r` and `\t`, ASCII's others and DEL as `\xNN`, C1's as `\u{NN}` (board 8571). A message or a
/// value of a text record holds no line break then, nor a terminal's escape: none forges a record,
/// as a path a VM asked for with a newline and a record's text in it did.
struct Escaped<'a>(&'a mut dyn fmt::Write);

impl fmt::Write for Escaped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut plain = 0;
        for (at, c) in text.char_indices().filter(|(_, c)| c.is_control()) {
            self.0.write_str(&text[plain..at])?;
            match c {
                '\n' => self.0.write_str("\\n")?,
                '\r' => self.0.write_str("\\r")?,
                '\t' => self.0.write_str("\\t")?,
                c if c.is_ascii() => write!(self.0, "\\x{:02x}", u32::from(c))?,
                c => write!(self.0, "\\u{{{:x}}}", u32::from(c))?,
            }
            plain = at + c.len_utf8();
        }
        self.0.write_str(&text[plain..])
    }
}

/// Writes the key-values it visits, each after a space.
struct KeyValues<'a>(Escaped<'a>);

impl<'kvs> log::kv::VisitSource<'kvs> for KeyValues<'_> {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        write!(self.0, " {key}={value}").map_err(log::kv::Error::from)
    }
}

/// The fields every JSON record has, or may, which a key-value of the same name is not.
const FIELDS: [&str; 8] =
    ["timestamp", "level", "target", "message", "request_id", "store", "trace_id", "span_id"];

/// `record` as a JSON object, logged at `now`, about `request`, if any, within the span of `trace`,
/// if any.
fn json(
    record: &log::Record<'_>,
    request: Option<cladefs_core::request::RequestId>,
    trace: Option<(String, String)>,
    now: SystemTime,
) -> String {
    let mut out = String::from("{\"timestamp\":");
    string(&mut out, &Rfc3339Micros(now).to_string());
    out.push_str(",\"level\":");
    string(&mut out, record.level().as_str());
    out.push_str(",\"target\":");
    string(&mut out, record.target());
    out.push_str(",\"message\":");
    string(&mut out, &record.args().to_string());
    if let Some(request) = request {
        write!(out, ",\"request_id\":{}", request.guest).expect("a String takes what is written");
        if let Some(open_cell) = &request.open_cell {
            out.push_str(",\"store\":");
            string(&mut out, open_cell);
        }
    }
    if let Some((trace, span)) = trace {
        out.push_str(",\"trace_id\":");
        string(&mut out, &trace);
        out.push_str(",\"span_id\":");
        string(&mut out, &span);
    }
    // A String takes every key-value, which none fails.
    drop(record.key_values().visit(&mut JsonFields(&mut out)));
    out.push('}');
    out
}

/// Appends `text` to `out`, a JSON string.
fn string(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).expect("a str is a JSON string"));
}

/// Appends the key-values it visits to a JSON object, each a field.
struct JsonFields<'a>(&'a mut String);

impl<'kvs> log::kv::VisitSource<'kvs> for JsonFields<'_> {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        let out = &mut *self.0;
        out.push(',');
        match FIELDS.contains(&key.as_str()) {
            true => string(out, &format!("kv_{key}")),
            false => string(out, key.as_str()),
        }
        out.push(':');
        if let Some(value) = value.to_bool() {
            write!(out, "{value}")
        } else if let Some(value) = value.to_u64() {
            write!(out, "{value}")
        } else if let Some(value) = value.to_i64() {
            write!(out, "{value}")
        } else if let Some(value) = value.to_f64().filter(|value| value.is_finite()) {
            write!(out, "{value}")
        } else {
            string(out, &value.to_string());
            Ok(())
        }
        .map_err(log::kv::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{Format, Settings, line};
    use cladefs_core::request::RequestId;
    use std::num::NonZeroU64;
    use std::time::{Duration, SystemTime};

    /// 2026-11-08T12:34:56.789012Z.
    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_micros(1_794_141_296_789_012)
    }

    /// A span's trace's ID and its own, as exporters write them.
    fn trace() -> Option<(String, String)> {
        Some(("4bf92f3577b34da6a3ce929d0e0e4736".to_owned(), "00f067aa0ba902b7".to_owned()))
    }

    fn request(guest: u64, open_cell: Option<&str>) -> Option<RequestId> {
        Some(RequestId {
            guest: NonZeroU64::new(guest).unwrap(),
            open_cell: open_cell.map(Into::into),
        })
    }

    /// A JSON record: its time, level, target and message, its request's, and each key-value by
    /// its type, every string escaped, a key-value named as a field renamed.
    #[test]
    fn a_json_record_has_a_field_for_each_of_its_parts() {
        let error = std::io::Error::from_raw_os_error(libc::ENOMEM);
        let pairs: [(&str, log::kv::Value<'_>); 7] = [
            ("len", log::kv::Value::from(4096u64)),
            ("delta", log::kv::Value::from(-3i64)),
            ("ms", log::kv::Value::from(1.5f64)),
            ("sealed", log::kv::Value::from(true)),
            ("path", log::kv::Value::from("a \"b\"\n\tc\u{1}")),
            ("error", log::kv::Value::from_display(&error)),
            ("level", log::kv::Value::from("mine")),
        ];
        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("cladefs::cladefsd::server")
            .args(format_args!("cladefsd: a \"quoted\" message\\ with\na line"))
            .key_values(&pairs)
            .build();
        assert_eq!(
            line(Format::Json, &record, request(7, Some("/stores/vm \"2\"")), trace(), at()),
            r#"{"timestamp":"2026-11-08T12:34:56.789012Z","level":"WARN","target":"cladefs::cladefsd::server","message":"cladefsd: a \"quoted\" message\\ with\na line","request_id":7,"store":"/stores/vm \"2\"","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7","len":4096,"delta":-3,"ms":1.5,"sealed":true,"path":"a \"b\"\n\tc\u0001","error":"Cannot allocate memory (os error 12)","kv_level":"mine"}"#
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&line(Format::Json, &record, None, None, at())).unwrap();
        assert_eq!(parsed["level"], "WARN");
        assert_eq!(parsed["request_id"], serde_json::Value::Null, "no request, no field");
        assert_eq!(parsed["trace_id"], serde_json::Value::Null, "no span, no field");
        assert_eq!(parsed["path"], "a \"b\"\n\tc\u{1}");
    }

    /// CLADEFS_LOG_FORMAT switches the log's records from text to JSON; anything but the two is
    /// refused.
    #[test]
    fn the_format_is_the_variables() {
        let vars = |format: Option<&'static str>, level: Option<&'static str>| {
            move |name: &str| match name {
                "CLADEFS_LOG_FORMAT" => format.map(str::to_owned),
                "CLADEFS_LOG_LEVEL" => level.map(str::to_owned),
                _ => None,
            }
        };
        let record = log::Record::builder()
            .level(log::Level::Info)
            .target("cladefs::cladefsd")
            .args(format_args!("cladefsd: serving"))
            .build();
        let written = |format, level| {
            let settings = Settings::from_vars(vars(format, level)).unwrap();
            line(settings.format, &record, None, None, at())
        };
        let text = "2026-11-08T12:34:56.789012Z INFO cladefs::cladefsd: cladefsd: serving";
        assert_eq!(written(None, None), text);
        assert_eq!(written(Some("text"), None), text);
        assert_eq!(
            written(Some("json"), None),
            r#"{"timestamp":"2026-11-08T12:34:56.789012Z","level":"INFO","target":"cladefs::cladefsd","message":"cladefsd: serving"}"#
        );
        assert_eq!(
            Settings::from_vars(vars(Some("yaml"), None)),
            Err("CLADEFS_LOG_FORMAT is text or json, not \"yaml\"".to_owned())
        );
        let levels = Settings::from_vars(vars(
            None,
            Some("warn, bucket_cell=debug,bucket_cell::cache=error"),
        ))
        .unwrap();
        assert_eq!(levels.level_of("cladefs::cladefsd::server"), log::LevelFilter::Warn);
        assert_eq!(levels.level_of("bucket_cell::object_bucket"), log::LevelFilter::Debug);
        assert_eq!(levels.level_of("bucket_cell::cache"), log::LevelFilter::Error);
        assert_eq!(levels.level_of("bucket_cell_other"), log::LevelFilter::Warn);
        assert_eq!(levels.max_level(), log::LevelFilter::Debug);
        assert_eq!(
            Settings::from_vars(vars(None, Some("loud"))),
            Err("CLADEFS_LOG_LEVEL: \"loud\" is no level (off to trace)".to_owned())
        );
    }

    /// board 8571: a text record escapes every control character of its message, its key-values
    /// and its request's cell, as a VM's path may hold them: one line whatever they hold, which
    /// none forges another record in, nor moves a terminal's cursor; the rest, non-ASCII too, as
    /// it is. Its span's IDs after the rest.
    #[test]
    fn a_text_record_is_one_line_whatever_it_says() {
        let pairs: [(&str, log::kv::Value<'_>); 2] = [
            ("path", log::kv::Value::from("/a\nINFO cladefs: forged\r\x1b[2J")),
            ("name", log::kv::Value::from("caf\u{e9}\u{85}\u{7f}\t")),
        ];
        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("cladefs::cladefsd::server")
            .args(format_args!("opening x\n2026-11-08T00:00:00Z ERROR cladefs: forged"))
            .key_values(&pairs)
            .build();
        assert_eq!(
            line(Format::Text, &record, request(7, Some("/stores/\n")), trace(), at()),
            r"2026-11-08T12:34:56.789012Z WARN cladefs::cladefsd::server: opening x\n2026-11-08T00:00:00Z ERROR cladefs: forged path=/a\nINFO cladefs: forged\r\x1b[2J name=café\u{85}\x7f\t request_id=7 store=/stores/\n trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7"
        );
    }

    #[test]
    fn rfc3339_micros_handles_epoch_and_leap_dates() {
        use super::Rfc3339Micros;
        let fmt = |secs, nanos| {
            Rfc3339Micros(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)).to_string()
        };
        assert_eq!(fmt(0, 0), "1970-01-01T00:00:00.000000Z");
        // 2024-02-29T23:59:59.999999Z (leap year).
        assert_eq!(fmt(1_709_251_199, 999_999_999), "2024-02-29T23:59:59.999999Z");
        // 2100-03-01T00:00:00.000000Z (non-leap century year).
        assert_eq!(fmt(4_107_542_400, 0), "2100-03-01T00:00:00.000000Z");
    }
}
