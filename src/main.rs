#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let (mut listen, mut upstream, mut window, mut config_flag) = (None, None, true, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => listen = Some(args.next().with_context(|| format!("flag {flag} needs a value"))?),
            "--upstream" => upstream = Some(args.next().with_context(|| format!("flag {flag} needs a value"))?),
            "--config" => config_flag = Some(args.next().with_context(|| format!("flag {flag} needs a value"))?),
            "--no-window" => window = false,
            other => bail!("unknown argument {other:?}; usage: magpie-gateway [--listen <addr>] [--upstream <url>] [--config <path>] [--no-window]"),
        }
    }

    // the app opens its window no matter what; a provider given on the command
    // line is remembered, so the next launch can be a plain double-click
    let config_path = config_path(config_flag.as_deref())?;
    let (config_listen, config_upstream) = load_config(&config_path);
    if let Some(given) = &upstream {
        save_config(&config_path, listen.as_deref().or(config_listen.as_deref()), Some(given))?;
        tracing::info!("provider saved to {}", config_path.display());
    }
    let listen: SocketAddr = listen
        .or(config_listen)
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
        .context("invalid --listen address")?;
    let upstream = upstream.or(config_upstream).unwrap_or_default();
    if !upstream.is_empty() {
        reqwest::Url::parse(&upstream).context("invalid --upstream URL")?;
    }

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
    let role = if app.upstream.is_empty() {
        "no provider configured yet — pass --upstream once and it is remembered".to_owned()
    } else {
        format!("everything else passes through to {}", app.upstream)
    };
    tracing::info!("gateway listening on http://{listen} — UI at that address, {role}");
    let router = Router::new()
        .route("/api/state", get(api_state))
        .route("/api/providers", get(api_providers))
        .route("/api/gateway/trace", get(api_trace))
        .route("/api/update", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/window/quit", post(quit))
        .fallback(entry)
        .layer(DefaultBodyLimit::disable())
        .with_state(app.clone());

    // the gateway serves from background threads; the desktop window owns the
    // main thread (a requirement on macOS), and closing it ends the process
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind(listen))?;
    let server = runtime.spawn(async move { axum::serve(listener, router).await });
    if window {
        run_window(&app.gateway.url);
    }
    runtime.block_on(server)??;
    Ok(())
}

/// The desktop shell: one webview window pointed at the gateway's own UI.
fn run_window(url: &str) {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoop::new();
    let app_window = WindowBuilder::new()
        .with_title("magpie")
        .with_inner_size(LogicalSize::new(1120.0, 780.0))
        .build(&event_loop)
        .expect("window");
    let _webview = WebViewBuilder::new()
        .with_url(format!("{url}/?shell=1"))
        .build(&app_window)
        .expect("webview");
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
    // if the event loop ever hands control back, the window is gone: end the app
    std::process::exit(0)
}

/// The UI when the path is one of its files, a JSON 404 for the shell's
/// reserved namespaces (never forwarded upstream), and lossless pass-through
/// for the rest.
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
    if name == "api" || name.starts_with("api/") || name == "wails" || name.starts_with("wails/") {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "reserved by the shell" }))).into_response();
    }
    proxy(State(app), req).await
}

/// Whatever arrives goes out unchanged: same method, path, query, headers and
/// body bytes (streamed both ways, so SSE and big payloads never buffer).
async fn proxy(State(app): State<Arc<App>>, req: Request) -> Response {
    if app.upstream.is_empty() {
        let msg = "magpie: no provider configured yet — pass --upstream <url> once, it is remembered";
        return (StatusCode::BAD_GATEWAY, msg.to_owned()).into_response();
    }
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

/// the UI footer's Quit: end the whole app, window and gateway together
async fn quit() -> Response {
    std::process::exit(0)
}

/// Where the provider and port settings live; `--config` moves the file
/// (the E2E suite keeps its own in a temp dir).
fn config_path(flag: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.into());
    }
    let base = match std::env::consts::OS {
        "windows" => std::env::var_os("APPDATA").map(PathBuf::from).context("APPDATA is not set")?,
        "macos" => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join("Library/Application Support"),
        _ => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?,
    };
    Ok(base.join("magpie").join("config.json"))
}

fn load_config(path: &Path) -> (Option<String>, Option<String>) {
    let saved = std::fs::read(path).ok().map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap_or_default());
    let field = |key: &str| {
        saved
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    (field("listen"), field("upstream"))
}

fn save_config(path: &Path, listen: Option<&str>, upstream: Option<&str>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&json!({ "listen": listen, "upstream": upstream }))?)?;
    Ok(())
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
