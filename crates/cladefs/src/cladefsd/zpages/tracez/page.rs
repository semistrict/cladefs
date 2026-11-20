// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: `/z/tracez`, the [`Recorder`]'s spans as HTML (TASK-672.1): with no query, each span
//! name's counts by latency bucket, failed and running, each linking to what it counts; with
//! `name`, that name's samples, whole traces, of a latency bucket (`bucket`, its index in
//! [`BOUNDS`]), of those that failed (`failed`), or of all, the slowest first, those whose trace has
//! a span with the attribute `tag` (`KEY=VALUE`) alone, if given; with `name` and `running`, its
//! spans running now.

use super::{BOUNDS, Recorder, Running, Sample, Which, recorder};
use crate::cladefsd::http::Response;
use crate::cladefsd::zpages::{HTML, escape, head};
use bucket_cell::trace::{BACKTRACE, SOURCE};
use opentelemetry::trace::{SpanId, Status};
use opentelemetry_sdk::trace::SpanData;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

/// What `/z/tracez?QUERY` asks for. See the module docs.
#[derive(Debug, Default, PartialEq, Eq)]
struct Query {
    name: Option<String>,
    which: Option<Which>,
    running: bool,
    tag: Option<(String, String)>,
}

impl Query {
    fn parse(query: &str) -> Self {
        let mut parsed = Self::default();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "name" => parsed.name = Some(value.into_owned()),
                "bucket" => parsed.which = value.parse().ok().map(Which::Latency),
                "failed" => parsed.which = Some(Which::Failed),
                "running" => parsed.running = true,
                "tag" => {
                    parsed.tag = (value.split_once('='))
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                }
                _ => {}
            }
        }
        parsed
    }
}

/// `/z/tracez?QUERY`, of the process's [`recorder`].
pub(in crate::cladefsd) fn page(query: &str) -> Response {
    Response::ok(HTML, render(recorder(), &Query::parse(query)))
}

/// The page `query` asks for, of `recorder`'s spans.
fn render(recorder: &Recorder, query: &Query) -> String {
    let mut html = head("tracez");
    match &query.name {
        None => summary(recorder, &mut html),
        Some(name) if query.running => running(&recorder.running(name), name, &mut html),
        Some(name) => samples(recorder, name, query, &mut html),
    }
    html.push_str("</body></html>\n");
    html
}

/// A latency bucket's name: `>` its lower bound.
fn bucket_name(bucket: usize) -> String {
    format!(">{}", short(BOUNDS[bucket]))
}

/// `duration`, short: `0`, `10µs`, `1.5ms`, `2s`.
fn short(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    match nanos {
        0 => "0".to_owned(),
        ..1_000 => format!("{nanos}ns"),
        ..1_000_000 => trimmed(nanos as f64 / 1e3, "µs"),
        ..1_000_000_000 => trimmed(nanos as f64 / 1e6, "ms"),
        _ => trimmed(nanos as f64 / 1e9, "s"),
    }
}

/// `value` to 3 significant figures at most, its trailing zeros dropped, then `unit`.
fn trimmed(value: f64, unit: &str) -> String {
    let figures = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.figures$}");
    let text =
        if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.') } else { &text };
    format!("{text}{unit}")
}

/// A link to `/z/tracez` with `query`, its text `text`, escaped.
fn link(query: &str, text: &str) -> String {
    format!("<a href=\"/z/tracez?{}\">{}</a>", escape(query), escape(text))
}

/// `value` as a query's value.
fn encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Each span name's counts, by latency bucket, failed and running, each linking to what it counts.
fn summary(recorder: &Recorder, html: &mut String) {
    html.push_str("<h1>tracez</h1>\n");
    // cladefs: none made, CLADEFSD_TRACES=off (TASK-707): an empty table says nothing else.
    if !bucket_cell::trace::enabled() {
        html.push_str("<p>Spans are off: <code>CLADEFSD_TRACES=off</code>.</p>\n");
    }
    html.push_str("<p>Spans by latency, wall clock, since the daemon started. ");
    let bucket = format!("name=cache.page_in&tag={}", encoded(&format!("{SOURCE}=bucket")));
    let _ = write!(
        html,
        "{} &middot; <a href=\"/z/\">all pages</a></p>\n",
        link(&bucket, "page-ins that went to the bucket")
    );
    html.push_str("<table>\n<tr><th>span</th><th>running</th>");
    for bucket in 0..BOUNDS.len() {
        let _ = write!(html, "<th>{}</th>", escape(&bucket_name(bucket)));
    }
    html.push_str("<th>failed</th></tr>\n");
    for counts in recorder.counts() {
        let name = encoded(&counts.name);
        let cell = |count: u64, query: String| match count {
            0 => "<td class=\"zero\">0</td>".to_owned(),
            _ => format!("<td>{}</td>", link(&query, &count.to_string())),
        };
        let _ = write!(html, "<tr><td>{}</td>", link(&format!("name={name}"), &counts.name));
        html.push_str(&cell(counts.running, format!("name={name}&running")));
        for (bucket, ended) in counts.ended.iter().enumerate() {
            html.push_str(&cell(*ended, format!("name={name}&bucket={bucket}")));
        }
        html.push_str(&cell(counts.failed, format!("name={name}&failed")));
        html.push_str("</tr>\n");
    }
    html.push_str("</table>\n");
}

/// The spans named `name` running now, the longest running first.
fn running(running: &[Running], name: &str, html: &mut String) {
    let _ =
        write!(html, "<h1>{} running</h1>\n<p>{}</p>\n<table>", escape(name), link("", "tracez"));
    html.push_str("<tr><th>for</th><th>trace</th><th>span</th><th>parent</th></tr>\n");
    let now = SystemTime::now();
    for span in running {
        let _ = writeln!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            short(now.duration_since(span.start).unwrap_or_default()),
            span.trace,
            span.span,
            span.parent
        );
    }
    html.push_str("</table>\n");
}

/// The samples of spans named `name` that `query` asks for, each its whole trace.
fn samples(recorder: &Recorder, name: &str, query: &Query, html: &mut String) {
    let of = match query.which {
        Some(Which::Latency(bucket)) if bucket < BOUNDS.len() => bucket_name(bucket),
        Some(Which::Latency(_)) => "no such bucket".to_owned(),
        Some(Which::Failed) => "failed".to_owned(),
        None => "every latency".to_owned(),
    };
    let _ =
        write!(html, "<h1>{}: {}</h1>\n<p>{}</p>\n", escape(name), escape(&of), link("", "tracez"));
    let tag = query.tag.as_ref().map(|(key, value)| format!("{key}={value}"));
    let _ = write!(
        html,
        "<form><input type=\"hidden\" name=\"name\" value=\"{}\">{}tag \
         <input name=\"tag\" size=\"40\" placeholder=\"{SOURCE}=bucket\" value=\"{}\"> \
         <button>filter</button></form>\n",
        escape(name),
        match query.which {
            Some(Which::Latency(bucket)) => {
                format!("<input type=\"hidden\" name=\"bucket\" value=\"{bucket}\">")
            }
            Some(Which::Failed) => "<input type=\"hidden\" name=\"failed\">".to_owned(),
            None => String::new(),
        },
        escape(tag.as_deref().unwrap_or(""))
    );
    let kept = recorder.samples(name, query.which);
    let shown: Vec<&Sample> = (kept.iter())
        .filter(|sample| query.tag.as_ref().is_none_or(|(key, value)| tagged(sample, key, value)))
        .collect();
    if shown.is_empty() {
        html.push_str("<p>No samples.</p>\n");
    }
    for sample in shown {
        trace(sample, html);
    }
}

/// Whether a span of `sample`'s trace has the attribute `key`, `value`.
fn tagged(sample: &Sample, key: &str, value: &str) -> bool {
    (sample.trace.spans.iter()).any(|span| {
        span.attributes.iter().any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
    })
}

/// `sample`'s trace, its spans as a tree, each under its parent, by when they started; the
/// sample's span marked, and each span's backtrace resolved under it.
fn trace(sample: &Sample, html: &mut String) {
    let spans = &sample.trace.spans;
    let span = sample.span();
    let latency = span.end_time.duration_since(span.start_time).unwrap_or_default();
    let ended = humantime::format_rfc3339_millis(span.end_time);
    let _ = write!(
        html,
        "<div class=\"sample\"><h3>{} &middot; ended {ended} &middot; trace {}</h3>\n<div class=\"tree\">",
        short(latency),
        span.span_context.trace_id()
    );
    let ids: HashMap<SpanId, usize> =
        spans.iter().enumerate().map(|(at, span)| (span.span_context.span_id(), at)).collect();
    let mut children: HashMap<SpanId, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (at, span) in spans.iter().enumerate() {
        match ids.contains_key(&span.parent_span_id) {
            true => children.entry(span.parent_span_id).or_default().push(at),
            false => roots.push(at),
        }
    }
    let by_start = |list: &mut Vec<usize>| list.sort_by_key(|at| spans[*at].start_time);
    by_start(&mut roots);
    children.values_mut().for_each(by_start);
    let began = roots.iter().map(|at| spans[*at].start_time).min().unwrap_or(span.start_time);
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|at| (*at, 0)).collect();
    while let Some((at, depth)) = stack.pop() {
        line(&spans[at], depth, began, at == sample.at, html);
        let of = children.get(&spans[at].span_context.span_id());
        stack.extend(of.into_iter().flatten().rev().map(|child| (*child, depth + 1)));
    }
    if sample.trace.dropped > 0 {
        let _ = writeln!(html, "<div>… {} more spans, dropped</div>", sample.trace.dropped);
    }
    html.push_str("</div></div>\n");
}

/// A line of a trace: `span`, `depth` deep, when it started after `began` and how long it took,
/// its attributes and its error, if it failed; marked if `sampled`; its backtrace, if it has one,
/// resolved under it.
fn line(span: &SpanData, depth: usize, began: SystemTime, sampled: bool, html: &mut String) {
    let after = span.start_time.duration_since(began).unwrap_or_default();
    let took = span.end_time.duration_since(span.start_time).unwrap_or_default();
    let class = if sampled { " sampled" } else { "" };
    let _ = write!(
        html,
        "<div class=\"span{class}\" style=\"padding-left:{}em\"><b>{}</b> +{} <span class=\"took\">{}</span>",
        depth * 2,
        escape(&span.name),
        short(after),
        short(took)
    );
    let mut backtrace = None;
    for kv in &span.attributes {
        if kv.key.as_str() == BACKTRACE {
            backtrace = Some(kv.value.as_str());
            continue;
        }
        let _ = write!(html, " <i>{}</i>={}", escape(kv.key.as_str()), escape(&kv.value.as_str()));
    }
    if let Status::Error { description } = &span.status {
        let _ = write!(html, " <span class=\"error\">{}</span>", escape(description));
    }
    html.push_str("</div>\n");
    if let Some(frames) = backtrace {
        let _ = writeln!(
            html,
            "<pre style=\"margin-left:{}em\">{}</pre>",
            depth * 2 + 2,
            escape(&resolved(&frames))
        );
    }
}

/// The frames of a backtrace (`bucket_cell::trace::record_backtrace`), resolved in this process: a
/// line of each function, an inlined one each a line of its own, innermost first, those of the
/// capture itself left out.
fn resolved(frames: &str) -> String {
    let mut lines = Vec::new();
    for frame in frames.split(' ').filter_map(|frame| usize::from_str_radix(frame, 16).ok()) {
        let mut names = Vec::new();
        // A return address, past its call: resolved at the call.
        backtrace::resolve(frame.saturating_sub(1) as *mut std::ffi::c_void, |symbol| {
            let name =
                symbol.name().map_or_else(|| format!("{frame:#x}"), |name| format!("{name:#}"));
            let at = match (symbol.filename(), symbol.lineno()) {
                (Some(file), Some(line)) => format!("\n      at {}:{line}", file.display()),
                _ => String::new(),
            };
            names.push(format!("{name}{at}"));
        });
        if names.is_empty() {
            names.push(format!("{frame:#x}"));
        }
        lines.extend(names);
    }
    // The capture's own frames, up to record_backtrace's.
    match lines.iter().position(|line| line.contains("record_backtrace")) {
        Some(capture) => lines.drain(..=capture).for_each(drop),
        None => {}
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_say_what_they_ask_for() {
        assert_eq!(Query::parse(""), Query::default());
        assert_eq!(
            Query::parse("name=cache.page_in&bucket=4&tag=cladefs.source%3Dbucket"),
            Query {
                name: Some("cache.page_in".to_owned()),
                which: Some(Which::Latency(4)),
                running: false,
                tag: Some(("cladefs.source".to_owned(), "bucket".to_owned())),
            }
        );
        assert_eq!(
            Query::parse("name=bucket.get_range&failed&running"),
            Query {
                name: Some("bucket.get_range".to_owned()),
                which: Some(Which::Failed),
                running: true,
                tag: None,
            }
        );
    }

    #[test]
    fn durations_are_short() {
        let shown: Vec<String> = [
            Duration::ZERO,
            Duration::from_nanos(700),
            Duration::from_micros(10),
            Duration::from_micros(1500),
            Duration::from_nanos(12_345_678),
            Duration::from_secs(2),
            Duration::from_secs(100),
        ]
        .into_iter()
        .map(short)
        .collect();
        assert_eq!(shown, ["0", "700ns", "10µs", "1.5ms", "12.3ms", "2s", "100s"]);
        assert_eq!(bucket_name(3), ">1ms");
    }

    /// With spans off (`CLADEFSD_TRACES=off`, as in a test's process, which turns none on), the
    /// summary says so, its table empty (TASK-707).
    #[test]
    fn the_summary_says_spans_are_off() {
        assert!(!bucket_cell::trace::enabled());
        let html = render(&Recorder::default(), &Query::parse(""));
        assert!(html.contains("<p>Spans are off: <code>CLADEFSD_TRACES=off</code>.</p>"), "{html}");
    }
}
