// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the daemon's z-pages, under `/z/` on the address `CLADEFSD_METRICS` names
//! (`super::http`), as Census's were at Google: pages of what a running daemon does, for whoever
//! looks into it (TASK-672). `/z/` lists them:
//!
//! - `/z/tracez`: the daemon's spans by latency, kept in its memory whatever OTLP does, with
//!   samples of each, whole traces, a page-in's saying where its data came from and, if from the
//!   bucket, the stack that waited for it ([`tracez`]).
//! - `/z/pprof/profile?seconds=N` and `/z/pprof/flamegraph?seconds=N`: the daemon's CPU, sampled
//!   for N seconds, as pprof's protobuf or an SVG flamegraph ([`pprof`]).
//! - cladefs: `/z/log`: what the daemon logs, as `CLADEFS_LOG_LEVEL` says it (`super::logging`);
//!   `POST /z/log?level=LEVELS` changes it while it runs, and `POST /z/log` puts back what the
//!   environment said (board 8571): a target raised to debug on a daemon serving its VMs, which a
//!   restart would crash (hotbed question 503).
//!
//! cladefs: `CLADEFSD_ZPAGES` serves them apart from the metrics, or nowhere (`super::http`,
//! board 8559).

pub mod pprof;
pub mod tracez;

use super::http::{Client, Response};

/// The content type of the pages.
const HTML: &str = "text/html; charset=utf-8";

/// The z-pages: each one's path under `/z/` and what it shows.
const PAGES: [(&str, &str); 4] = [
    (
        "tracez",
        "Spans by latency, wall clock, with samples of each: whole traces, their attributes, and \
         the stacks that waited for the bucket.",
    ),
    (
        "pprof/profile?seconds=30",
        "The daemon's CPU, sampled for 30 seconds, as pprof's protobuf: go tool pprof URL.",
    ),
    ("pprof/flamegraph?seconds=30", "The same, as an SVG flamegraph."),
    (
        "log",
        "What the daemon logs, by target: POST /z/log?level=LEVELS changes it, POST /z/log puts \
         back CLADEFS_LOG_LEVEL's.",
    ),
];

/// The page at `/z{page}`, asked by `method` with `query`, of `client`: none, if it hung up before
/// the page was made.
pub(super) fn serve(
    method: &str,
    page: &str,
    query: &str,
    client: &Client<'_>,
) -> Option<Response> {
    Some(match (method, page) {
        ("GET", "" | "/") => Response::ok(HTML, index()),
        ("GET", "/tracez") => tracez::page(query),
        ("GET", page) if let Some(page) = page.strip_prefix("/pprof") => {
            return pprof::serve(page, query, client);
        }
        ("GET", "/log") => log_levels(super::logging::levels()),
        ("POST", "/log") => set_log_levels(query),
        _ => Response::not_found(),
    })
}

/// cladefs: `POST /z/log`: what the daemon logs from now on, its query's `level`, as
/// `CLADEFS_LOG_LEVEL` says it; without one, what that said as the daemon started (board 8571).
fn set_log_levels(query: &str) -> Response {
    let level = url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "level")
        .map(|(_, value)| value.into_owned());
    match super::logging::set_levels(level.as_deref()) {
        Ok(levels) => {
            log::warn!("cladefsd: logging {levels} from now on, as /z/log was told");
            log_levels(Ok(levels))
        }
        Err(why) => Response::plain("400 Bad Request", why),
    }
}

/// cladefs: `levels`, what the daemon logs, as `CLADEFS_LOG_LEVEL` says it, in plain text; or why
/// it can't say.
fn log_levels(levels: Result<String, String>) -> Response {
    match levels {
        Ok(levels) => Response::plain("200 OK", levels),
        Err(why) => Response::plain("409 Conflict", why),
    }
}

/// `/z/`: each page, linked, and what it shows.
fn index() -> String {
    let mut html = head("z-pages");
    html.push_str("<h1>cladefsd z-pages</h1>\n<ul>\n");
    for (path, shows) in PAGES {
        html.push_str(&format!(
            "<li><a href=\"/z/{path}\">/z/{path}</a>: {}</li>\n",
            escape(shows)
        ));
    }
    html.push_str("</ul>\n<p><a href=\"/metrics\">/metrics</a>: the daemon's metrics.</p>\n");
    html.push_str("</body></html>\n");
    html
}

/// The start of a page titled `title`, through its body's opening tag: light or dark as its viewer
/// prefers, monospaced.
fn head(title: &str) -> String {
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>cladefsd {}</title><style>\n{STYLE}</style></head><body>\n",
        escape(title)
    )
}

/// The pages' style.
const STYLE: &str = ":root{color-scheme:light dark;--line:#8885;--mark:#fd06;--error:#d33}
body{font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;margin:16px;max-width:100%}
table{border-collapse:collapse;display:block;overflow-x:auto}
th,td{padding:2px 10px;text-align:right;border-bottom:1px solid var(--line);white-space:nowrap}
th:first-child,td:first-child{text-align:left}
td.zero{opacity:.35}
.sample{border-top:1px solid var(--line);margin-top:12px}
.tree{overflow-x:auto}
.span{white-space:nowrap}
.sampled{background:var(--mark)}
.took{font-weight:bold}
.error{color:var(--error)}
pre{margin:2px 0 8px;opacity:.85}
";

/// `text`, safe in HTML's text and its attributes' quoted values.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}
