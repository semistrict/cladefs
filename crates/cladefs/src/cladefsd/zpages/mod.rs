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

pub mod pprof;
pub mod tracez;

use super::http::Response;

/// The content type of the pages.
const HTML: &str = "text/html; charset=utf-8";

/// The z-pages: each one's path under `/z/` and what it shows.
const PAGES: [(&str, &str); 3] = [
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
];

/// The page at `/z{page}`, asked with `query`.
pub(super) fn serve(page: &str, query: &str) -> Response {
    match page {
        "" | "/" => Response::ok(HTML, index()),
        "/tracez" => tracez::page(query),
        page if let Some(page) = page.strip_prefix("/pprof") => pprof::serve(page, query),
        _ => Response::not_found(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_lists_every_page() {
        let response = serve("/", "");
        assert_eq!((response.status, response.content_type), ("200 OK", HTML));
        let html = String::from_utf8(response.body).unwrap();
        assert!(html.contains("<a href=\"/z/tracez\">/z/tracez</a>"), "{html}");
        assert_eq!(serve("/nothing", "").status, "404 Not Found");
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
