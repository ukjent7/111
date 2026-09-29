#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use include_dir::{Dir, include_dir};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

mod providers;
mod settings;
mod usage;

// the prepared UI, embedded so the binary alone is the whole desktop app
pub static UI: Dir<'_> = include_dir!("ui-source");

pub struct Gateway {
    pub url: String,
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub calls: Mutex<VecDeque<Value>>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ConfigState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream: String,
    #[serde(default)]
    pub providers: Vec<providers::Provider>,
    #[serde(default)]
    pub settings: Value,
}

/// a vendor logo's bytes and content type, or its absence
pub type Logo = Option<(Vec<u8>, String)>;

pub struct App {
    pub client: reqwest::Client,
    pub config: tokio::sync::Mutex<ConfigState>,
    pub config_path: PathBuf,
    /// models.dev's vendor catalog, loaded from the disk cache at startup and
    /// refreshed from the network in the background and on Sync
    pub catalog: Mutex<Value>,
    /// the catalog's disk cache, next to the config
    pub catalog_path: PathBuf,
    /// directory where vendor logos are cached on disk
    pub logos_dir: PathBuf,
    /// vendor logos from models.dev, by id; a miss is remembered too
    pub logos: Mutex<HashMap<String, Logo>>,
    /// every call's tokens, from the vendor usage reports in the answers
    pub usage: usage::Store,
    pub gateway: Gateway,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (mut listen, mut upstream, mut window, mut config_flag) = (None, None, true, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => {
                listen = Some(
                    args.next()
                        .with_context(|| format!("flag {flag} needs a value"))?,
                )
            }
            "--upstream" => {
                upstream = Some(
                    args.next()
                        .with_context(|| format!("flag {flag} needs a value"))?,
                )
            }
            "--config" => {
                config_flag = Some(
                    args.next()
                        .with_context(|| format!("flag {flag} needs a value"))?,
                )
            }
            "--no-window" => window = false,
            other => bail!(
                "unknown argument {other:?}; usage: magpie-gateway [--listen <addr>] [--upstream <url>] [--config <path>] [--no-window]"
            ),
        }
    }

    // the app opens its window no matter what; a provider given on the command
    // line is remembered, so the next launch can be a plain double-click
    let config_path = config_path(config_flag.as_deref())?;
    let mut cfg = load_config(&config_path);
    if let Some(given) = &upstream {
        cfg.upstream = given.clone();
        persist(&config_path, &cfg)?;
        tracing::info!("provider saved to {}", config_path.display());
    }
    let listen: SocketAddr = listen
        .or(cfg.listen.take())
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
        .context("invalid --listen address")?;
    if !cfg.upstream.is_empty() {
        reqwest::Url::parse(&cfg.upstream).context("invalid --upstream URL")?;
    }

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let role = if cfg.upstream.is_empty() {
        ", no provider configured yet".to_owned()
    } else {
        format!(", raw upstream {}", cfg.upstream)
    };
    let catalog_path = config_path.with_file_name("catalog.json");
    let logos_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("logos");
    if let Err(e) = std::fs::create_dir_all(&logos_dir) {
        tracing::warn!("failed to create logos dir: {e}");
    }
    let usage_path = config_path.with_file_name("usage.json");
    let app = Arc::new(App {
        client,
        config: tokio::sync::Mutex::new(cfg),
        config_path,
        catalog: Mutex::new(load_catalog(&catalog_path)),
        catalog_path,
        logos_dir,
        logos: Mutex::new(HashMap::new()),
        usage: usage::Store::load(usage_path),
        gateway: Gateway {
            url: format!("http://{listen}"),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            calls: Mutex::new(VecDeque::new()),
        },
    });
    tracing::info!("gateway listening on http://{listen} — UI at that address{role}");

    let router = Router::new()
        .route("/api/state", get(api_state))
        .route("/api/providers", get(providers::list))
        .route("/api/gateway/trace", get(api_trace))
        .route("/api/update", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/window/quit", post(quit))
        .route("/api/sync", post(providers::sync))
        .route("/api/icons/{id}", get(providers::icon))
        .route("/api/provider/save", post(providers::save))
        .route("/api/provider/delete", post(providers::delete))
        .route("/api/provider/key", post(providers::reveal_key))
        .route("/api/provider/models", post(providers::fetch_models))
        .route("/api/provider/test", post(providers::test))
        .route("/api/settings", get(settings::get).post(settings::save))
        .route("/api/settings/quota-left", post(settings::set_quota_left))
        .route("/api/settings/login", post(settings::set_login))
        .route("/api/settings/lan", post(settings::set_lan))
        .route("/api/settings/reveal", post(settings::reveal))
        .route("/api/open", post(settings::open))
        .route("/api/usage", get(settings::usage))
        .route("/api/usage/quotas", get(settings::usage_quotas))
        .route("/api/sessions", get(settings::sessions))
        .route("/api/sessions/stats", get(settings::sessions_stats))
        .route("/api/davsync", get(settings::davsync))
        .route("/api/drift", get(settings::drift))
        .fallback(entry)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn(check_host))
        .with_state(app.clone());

    // the gateway serves from background threads; the desktop window owns the
    // main thread (a requirement on macOS), and closing it ends the process
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    {
        // models.dev in the background: the Providers tab wants its vendor
        // list. From the second launch the disk cache has it before the
        // window even opens; this refresh is for the first launch and for a
        // cache gone stale, so a miss just tries again in a minute.
        let app = app.clone();
        runtime.spawn(async move {
            loop {
                match providers::fetch_catalog(&app.client).await {
                    Ok(v) => {
                        providers::store_catalog(&app, v);
                        tracing::info!("models.dev catalog loaded");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("models.dev not fetched yet, retrying in a minute (Sync retries too): {e}");
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                }
            }
        });
    }
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
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

/// The UI when the path is one of its files, a JSON 404 for the shell's
/// reserved namespaces (never forwarded upstream), and lossless pass-through
/// for the rest.
async fn entry(State(app): State<Arc<App>>, req: Request) -> Response {
    let name = req.uri().path().trim_start_matches('/');
    if let Some(file) = UI.get_file(if name.is_empty() { "index.html" } else { name }) {
        let mime = mime_guess::from_path(file.path()).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref().to_owned())],
            file.contents(),
        )
            .into_response();
    }
    if name == "favicon.ico"
        && let Some(icon) = UI.get_file("icons/magpie.svg")
    {
        return ([("content-type", "image/svg+xml")], icon.contents()).into_response();
    }
    if name == "api" || name.starts_with("api/") {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "reserved by the shell" })),
        )
            .into_response();
    }
    proxy(State(app), req).await
}

/// DNS rebinding would let a web page read the API — and the key behind
/// `/api/provider/key` — as if it were this machine: the browser's Host must
/// name localhost or be a bare IP (a hostname can't be a rebinding target if
/// the page never had one to rebind).
async fn check_host(req: Request, next: Next) -> Response {
    let local = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_name)
        .is_some_and(|name| name == "localhost" || name.parse::<std::net::IpAddr>().is_ok());
    if local {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// The Host header's name, without port and IPv6 brackets.
fn host_name(host: &str) -> String {
    reqwest::Url::parse(&format!("http://{host}"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned()
}

/// Whatever arrives goes out unchanged: same method, path, query, headers and
/// body bytes (streamed both ways, so SSE and big payloads never buffer) —
/// to the provider the UI configured, or to the raw upstream from the
/// command line. Only the key is swapped in.
async fn proxy(State(app): State<Arc<App>>, req: Request) -> Response {
    let started = Instant::now();
    let (mut parts, body) = req.into_parts();
    let mut path = parts
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

    let body_bytes = if has_body {
        to_bytes(body, usize::MAX).await.unwrap_or_default()
    } else {
        Bytes::new()
    };

    let cfg = app.config.lock().await;

    // 1. Check if URL path starts with a provider prefix: /<provider>/...
    let mut path_provider_hint: Option<String> = None;
    let (path_only, query_part) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path.as_str(), None),
    };
    let trimmed_path = path_only.trim_start_matches('/');
    if let Some((first_seg, rest)) = trimmed_path.split_once('/') {
        if let Some(p) = cfg.providers.iter().find(|p| {
            p.id.eq_ignore_ascii_case(first_seg) || p.name.eq_ignore_ascii_case(first_seg)
        }) {
            path_provider_hint = Some(p.id.clone());
            path = match query_part {
                Some(q) => format!("/{rest}?{q}"),
                None => format!("/{rest}"),
            };
        }
    } else if let Some(p) = cfg.providers.iter().find(|p| {
        p.id.eq_ignore_ascii_case(trimmed_path) || p.name.eq_ignore_ascii_case(trimmed_path)
    }) {
        path_provider_hint = Some(p.id.clone());
        path = match query_part {
            Some(q) => format!("/?{q}"),
            None => "/".to_string(),
        };
    }

    // 2. Check if JSON body specifies a provider prefix in "model": "<provider>/<model>" or "<provider>:<model>"
    let mut model_name: Option<String> = None;
    let mut model_provider_hint: Option<String> = None;
    let mut forwarded_body = body_bytes;

    if !forwarded_body.is_empty()
        && let Ok(mut json_val) = serde_json::from_slice::<Value>(&forwarded_body)
        && let Some(obj) = json_val.as_object_mut()
        && let Some(model_val) = obj.get("model").and_then(Value::as_str)
    {
        model_name = Some(model_val.to_owned());
        for sep in ['/', ':'] {
            if let Some((prefix, clean_model)) = model_val.split_once(sep)
                && let Some(p) = cfg.providers.iter().find(|p| {
                    p.id.eq_ignore_ascii_case(prefix) || p.name.eq_ignore_ascii_case(prefix)
                })
            {
                model_provider_hint = Some(p.id.clone());
                obj.insert("model".to_owned(), json!(clean_model));
                if let Ok(new_bytes) = serde_json::to_vec(&json_val) {
                    forwarded_body = Bytes::from(new_bytes);
                }
                break;
            }
        }
    }

    let provider_hint = model_provider_hint
        .as_deref()
        .or(path_provider_hint.as_deref());

    enum Dest {
        Provider(providers::Target),
        Legacy(String),
    }
    let dest = if cfg.providers.is_empty() {
        (!cfg.upstream.is_empty()).then(|| Dest::Legacy(cfg.upstream.clone()))
    } else {
        providers::route_for(&cfg, provider_hint, &path).map(Dest::Provider)
    };
    drop(cfg);

    let to = match dest {
        None => {
            let msg = "magpie: no provider configured yet — add one in the Providers tab, or pass --upstream <url> once";
            return (StatusCode::BAD_GATEWAY, msg.to_owned()).into_response();
        }
        Some(Dest::Legacy(base)) => format!("{}{}", base.trim_end_matches('/'), path),
        Some(Dest::Provider(t)) => {
            parts.headers.remove(header::AUTHORIZATION);
            parts.headers.remove("x-api-key");
            if !t.key.is_empty()
                && let Ok(v) = HeaderValue::from_str(&t.key)
            {
                if t.anthropic {
                    parts
                        .headers
                        .insert(HeaderName::from_static("x-api-key"), v);
                } else if let Ok(bearer) = HeaderValue::from_str(&format!("Bearer {}", t.key)) {
                    parts.headers.insert(header::AUTHORIZATION, bearer);
                }
            }
            for (name, value) in &t.headers {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::try_from(name.as_str()),
                    HeaderValue::try_from(value.as_str().unwrap_or_default()),
                ) {
                    parts.headers.insert(name, value);
                }
            }
            t.url
        }
    };

    let mut sent = app.client.request(parts.method, &to).headers(parts.headers);
    if !forwarded_body.is_empty() {
        sent = sent.body(forwarded_body);
    }

    let recorded_model = model_name.as_deref().unwrap_or("");

    let upstream_res = match sent.send().await {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!(%method, %path, error = %err, "upstream request failed");
            record(
                &app,
                &path,
                &to,
                StatusCode::BAD_GATEWAY,
                started,
                Some(err.to_string()),
                recorded_model,
            );
            return (StatusCode::BAD_GATEWAY, format!("magpie: {err}")).into_response();
        }
    };

    let status = upstream_res.status();
    let mut headers = upstream_res.headers().clone();
    strip_hop_by_hop(&mut headers);

    // the answer streams through untouched; a bounded copy of its head and
    // tail is scanned for the vendor's usage report once the stream ends
    let tee = Arc::new(Mutex::new(usage::Tee::default()));
    let usage_app = app.clone();
    let usage_model = recorded_model.to_owned();
    let body = Body::from_stream(futures::stream::unfold(
        (upstream_res.bytes_stream(), tee, usage_app, usage_model, status, false),
        |(mut stream, tee, app, model, status, mut done)| async move {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    tee.lock().unwrap().push(&chunk);
                    Some((Ok::<_, reqwest::Error>(chunk), (stream, tee, app, model, status, done)))
                }
                Some(Err(e)) => {
                    if !done {
                        done = true;
                        usage::record(&app, &model, status, &tee);
                    }
                    Some((Err(e), (stream, tee, app, model, status, done)))
                }
                None => {
                    if !done {
                        usage::record(&app, &model, status, &tee);
                    }
                    None
                }
            }
        },
    ));
    let mut res = Response::builder()
        .status(status)
        .body(body)
        .expect("status and stream body are always valid");
    *res.headers_mut() = headers;
    record(&app, &path, &to, status, started, None, recorded_model);
    tracing::info!(%method, %path, status = status.as_u16(), ms = started.elapsed().as_millis() as u64, "passed through");
    res
}

fn record(
    app: &App,
    path: &str,
    to: &str,
    status: StatusCode,
    started: Instant,
    error: Option<String>,
    model: &str,
) {
    app.gateway.requests.fetch_add(1, Ordering::Relaxed);
    if status.is_client_error() || status.is_server_error() {
        app.gateway.errors.fetch_add(1, Ordering::Relaxed);
    }
    let call = json!({
        "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
        "model": model,
        "from": path,
        "to": to,
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

/// The shell's boot answer: placeholders the page fills from the other tabs.
pub fn shell_state() -> Response {
    Json(json!({ "agents": [], "profiles": [], "settings": {} })).into_response()
}

async fn api_state(State(app): State<Arc<App>>) -> impl IntoResponse {
    let cfg = app.config.lock().await;
    let s = settings::get_settings(&app, &cfg);
    Json(json!({ "agents": [], "profiles": [], "settings": s }))
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

/// Hop-by-hop headers describe one connection, never the message; anything the
/// `Connection` header lists belongs to them too (RFC 9110 §7.6.1).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .filter_map(|name| name.trim().parse().ok())
                .collect()
        })
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

/// Where the provider and port settings live; `--config` moves the file
/// (the E2E suite keeps its own in a temp dir).
fn config_path(flag: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.into());
    }
    let base = match std::env::consts::OS {
        "windows" => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .context("APPDATA is not set")?,
        "macos" => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join("Library/Application Support"),
        _ => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?,
    };
    Ok(base.join("magpie").join("config.json"))
}

fn load_config(path: &Path) -> ConfigState {
    // a hand-edited or half-written file falls back to a fresh config, same
    // as a missing one
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The catalog from the last successful refresh, so a launch is never a cold
/// one; a missing or corrupt cache costs the background fetch its usual job.
fn load_catalog(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn persist(path: &Path, cfg: &ConfigState) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(cfg)?;
    // the file carries API keys: write it out of line and rename, so a crash
    // mid-write can't truncate the old one, and keep it to the owner
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::File::create(&tmp)?;
    {
        use std::io::Write;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}
