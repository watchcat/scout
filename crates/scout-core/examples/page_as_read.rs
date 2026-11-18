//! Reads a page the way `fetch_page` would and prints, as one line of JSON,
//! what the model would have been handed. For `scripts/shop-reach.mjs`.
//!
//!     page_as_read get <url> [--proxy http://127.0.0.1:8118] [--save body.html]
//!     page_as_read file <page.html> <url>
//!     page_as_read headers
//!
//! `get` makes the plain request with Scout's own client stack and headers,
//! so a shop that tells clients apart by their handshake sees the one it
//! would see from Scout. `--proxy` is an HTTP proxy to leave by: the
//! measurement points it at a tunnel to the production node, whose address
//! is the one shops see. `file` reads HTML already on disk, such as what a
//! headless render dumped.
//!
//! It does not go through `net_guard`: the address comes from whoever runs
//! it, not from a model.

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = match args.first().map(String::as_str) {
        Some("get") if args.len() >= 2 => get(&args[1], flag(&args, "--proxy"), flag(&args, "--save")),
        Some("file") if args.len() == 3 => file(&args[1], &args[2]),
        // What the plain request says of itself, for a browser that should say the same.
        Some("headers") => scout_core::measure::request_headers().iter().map(|(k, v)| (k.to_string(), serde_json::Value::from(*v))).collect::<serde_json::Map<_, _>>().into(),
        _ => {
            eprintln!("usage: page_as_read get <url> [--proxy <http proxy>] [--save <file>] | page_as_read file <page.html> <url> | page_as_read headers");
            std::process::exit(2);
        }
    };
    println!("{out}");
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn file(path: &str, url: &str) -> serde_json::Value {
    match std::fs::read_to_string(path) {
        Ok(html) => read("file", &html, url, None, url, None),
        Err(e) => serde_json::json!({"way": "file", "error": format!("cannot read {path}: {e}")}),
    }
}

fn get(url: &str, proxy: Option<&str>, save: Option<&str>) -> serde_json::Value {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        // The timeout `Core` gives its client.
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if let Some(proxy) = proxy {
            match reqwest::Proxy::all(proxy) {
                Ok(p) => builder = builder.proxy(p),
                Err(e) => return serde_json::json!({"way": "get", "error": format!("bad proxy: {e}")}),
            }
        }
        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => return serde_json::json!({"way": "get", "error": format!("no client: {e}")}),
        };
        let mut request = client.get(url);
        for (name, value) in scout_core::measure::request_headers() {
            request = request.header(name, value);
        }
        let started = std::time::Instant::now();
        let response = match request.send().await {
            Ok(r) => r,
            // No answer at all. `fetch_page` hands that back as the error it
            // is, without trying the render.
            Err(e) => {
                return serde_json::json!({
                    "way": "get", "error": format!("{e}"), "status": null, "bytes": 0, "landed": url,
                    "would_render": false, "unrendered": true, "challenge": false,
                    "ms": started.elapsed().as_millis() as u64,
                });
            }
        };
        let status = response.status();
        let landed = response.url().to_string();
        // As `fetch_page` does: a body is read only from a success.
        let body = if status.is_success() { response.text().await.unwrap_or_default() } else { String::new() };
        if let Some(path) = save {
            let _ = std::fs::write(path, &body);
        }
        read("get", &body, url, Some(status.as_u16()), &landed, Some(started.elapsed()))
    })
}

fn read(way: &str, html: &str, url: &str, status: Option<u16>, landed: &str, took: Option<Duration>) -> serde_json::Value {
    let mut out = match scout_core::measure::page_as_read(html, url, status) {
        Ok(v) => v,
        Err(e) => serde_json::json!({"error": e.to_string()}),
    };
    out["way"] = way.into();
    out["status"] = status.into();
    out["bytes"] = html.len().into();
    out["landed"] = landed.into();
    out["ms"] = took.map(|d| d.as_millis() as u64).into();
    out
}
