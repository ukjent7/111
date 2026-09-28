use std::collections::VecDeque;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

// the prepared UI, embedded so the binary alone is the whole desktop app
static UI: Dir<'_> = include_dir!("ui-source");

struct Gateway {
    url: String,
    requests: AtomicU64,
    errors: AtomicU64,
    calls: Mutex<VecDeque<Value>>,
}

struct App {
    client: reqwest::Client,
    upstream: String,
    gateway: Gateway,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let (mut listen, mut upstream, mut open) = (None, None, true);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => listen = Some(args.next().with_context(|| format!("flag {flag} needs a value"))?),
            "--upstream" => upstream = Some(args.next().with_context(|| format!("flag {flag} needs a value"))?),
            "--no-open" => open = false,
            other => bail!("unknown argument {other:?}; usage: magpie-gateway --listen <addr> --upstream <url> [--no-open]"),
        }
    }
    let listen: SocketAddr = listen
        .or_else(|| std::env::var("MAGPIE_LISTEN").ok())
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
        .context("invalid --listen address")?;
    let upstream = upstream
        .or_else(|| std::env::var("MAGPIE_UPSTREAM").ok())
        .context("no upstream: pass --upstream <url> or set MAGPIE_UPSTREAM")?;
    reqwest::Url::parse(&upstream).context("invalid --upstream URL")?;

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let app = Arc::new(App {
        client,
        upstream,
        gateway: Gateway {
            url: format!("http://{listen}"),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            calls: Mutex::new(VecDeque::new()),
        },
    });
    tracing::info!("gateway listening on http://{listen} — UI at that address, everything else passes through to {}", app.upstream);
    let router = Router::new()
        .route("/api/state", get(api_state))
        .route("/api/providers", get(api_providers))
        .route("/api/gateway/trace", get(api_trace))
        .route("/api/update", get(|| async { StatusCode::NO_CONTENT }))
        .fallback(entry)
        .layer(DefaultBodyLimit::disable())
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    if open {
        open_browser(&app.gateway.url);
    }
    axum::serve(listener, router).await?;
    Ok(())
}

/// The UI when the path is one of its files, a JSON 404 for unknown API
/// calls (never forwarded upstream), and lossless pass-through for the rest.
async fn entry(State(app): State<Arc<App>>, req: Request) -> Response {
    let name = req.uri().path().trim_start_matches('/');
    if let Some(file) = UI.get_file(if name.is_empty() { "index.html" } else { name }) {
        let mime = mime_guess::from_path(file.path()).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.as_ref().to_owned())], file.contents()).into_response();
    }
    if name == "favicon.ico" {
        if let Some(icon) = UI.get_file("icons/magpie.svg") {
            return ([("content-type", "image/svg+xml")], icon.contents()).into_response();
        }
    }
    if name == "api" || name.starts_with("api/") {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "no such api" }))).into_response();
    }
    proxy(State(app), req).await
}

/// Whatever arrives goes out unchanged: same method, path, query, headers and
/// body bytes (streamed both ways, so SSE and big payloads never buffer).
async fn proxy(State(app): State<Arc<App>>, req: Request) -> Response {
    let started = Instant::now();
    let (mut parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".into());
    let method = parts.method.to_string();
    let has_body = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .is_some_and(|v| v != "0")
        || parts.headers.contains_key(header::TRANSFER_ENCODING);
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove(header::HOST);
    parts.headers.remove(header::CONTENT_LENGTH);

    let url = format!("{}{}", app.upstream.trim_end_matches('/'), path);
    let mut sent = app.client.request(parts.method, url).headers(parts.headers);
    if has_body {
        sent = sent.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }

    let upstream_res = match sent.send().await {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!(%method, %path, error = %err, "upstream request failed");
            record(&app, &path, StatusCode::BAD_GATEWAY, started, Some(err.to_string()));
            return (StatusCode::BAD_GATEWAY, format!("magpie: {err}")).into_response();
        }
    };

    let status = upstream_res.status();
    let mut headers = upstream_res.headers().clone();
    strip_hop_by_hop(&mut headers);
    let body = Body::from_stream(upstream_res.bytes_stream());
    let mut res = Response::builder()
        .status(status)
        .body(body)
        .expect("status and stream body are always valid");
    *res.headers_mut() = headers;
    record(&app, &path, status, started, None);
    tracing::info!(%method, %path, status = status.as_u16(), ms = started.elapsed().as_millis() as u64, "passed through");
    res
}

fn record(app: &App, path: &str, status: StatusCode, started: Instant, error: Option<String>) {
    app.gateway.requests.fetch_add(1, Ordering::Relaxed);
    if status.is_client_error() || status.is_server_error() {
        app.gateway.errors.fetch_add(1, Ordering::Relaxed);
    }
    let call = json!({
        "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
        "model": "",
        "from": path,
        "to": path,
        "status": status.as_u16(),
        "ms": started.elapsed().as_millis() as u64,
        "error": error,
    });
    let mut calls = app.gateway.calls.lock().unwrap();
    calls.push_back(call);
    while calls.len() > 50 {
        calls.pop_front();
    }
}

async fn api_state() -> impl IntoResponse {
    Json(json!({ "agents": [], "profiles": [], "settings": {} }))
}

async fn api_providers(State(app): State<Arc<App>>) -> impl IntoResponse {
    let g = &app.gateway;
    let calls: Vec<Value> = g.calls.lock().unwrap().iter().rev().cloned().collect();
    Json(json!({
        "providers": [],
        "presets": [],
        "gateway": {
            "running": true,
            "mine": true,
            "url": g.url,
            "models": 0,
            "groups": [],
            "calls": calls,
        },
    }))
}

async fn api_trace(State(app): State<Arc<App>>, req: Request) -> impl IntoResponse {
    // the routing view long-polls: holding the answer here is its heartbeat
    if req.uri().query().is_some_and(|q| q.contains("wait=1")) {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let g = &app.gateway;
    Json(json!({
        "now": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
        "mine": true,
        "seq": 0,
        "routes": [],
        "totals": {
            "requests": g.requests.load(Ordering::Relaxed),
            "rerouted": 0,
            "errors": g.errors.load(Ordering::Relaxed),
        },
    }))
}

/// Hop-by-hop headers describe one connection, never the message; anything the
/// `Connection` header lists belongs to them too (RFC 9110 §7.6.1).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').filter_map(|name| name.trim().parse().ok()).collect())
        .unwrap_or_default();
    for name in listed {
        headers.remove(&name);
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

fn open_browser(url: &str) {
    let mut cmd = match std::env::consts::OS {
        "windows" => {
            let mut c = Command::new("cmd");
            c.args(["/C", "start", "", url]);
            c
        }
        "macos" => {
            let mut c = Command::new("open");
            c.arg(url);
            c
        }
        _ => {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        }
    };
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}
