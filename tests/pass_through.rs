//! E2E: the real gateway binary against a mock upstream.
//! Every check proves one facet of lossless pass-through; the run leaves a
//! machine-readable report at the manifest root for CI to upload.

use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Router;
use futures::StreamExt;

const SSE_EXPECTED: &[u8] = b"data: {\"chunk\":1}\n\ndata: {\"chunk\":2}\n\ndata: [DONE]\n\n";

#[derive(Clone, Debug, Default)]
struct Captured {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    headers: HeaderMap,
}

type Shared = Arc<Mutex<Captured>>;

#[derive(Debug)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(name: &'static str, result: Result<String, String>) -> Check {
    let ok = result.is_ok();
    Check { name, ok, detail: result.unwrap_or_else(|e| e) }
}

struct Gateway(Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn gateway_passes_everything_through_losslessly() {
    let run = tokio::time::timeout(Duration::from_secs(60), run_checks());
    let checks = match run.await {
        Ok(checks) => checks,
        Err(_) => vec![check("the whole suite finishes in 60s", Err("timed out".into()))],
    };
    write_report(&checks);

    let failed: Vec<&Check> = checks.iter().filter(|c| !c.ok).collect();
    assert!(failed.is_empty(), "failed checks: {failed:#?}");
}

async fn run_checks() -> Vec<Check> {
    let captured: Shared = Arc::default();
    let mock_addr = spawn_mock(captured.clone()).await;
    let gateway_addr = free_port();
    let config = std::env::temp_dir().join(format!("magpie-e2e-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&config);
    let guard = spawn_gateway(gateway_addr, mock_addr, &config, true);
    let gateway = format!("http://{gateway_addr}");
    let client = reqwest::Client::new();

    let mut checks = Vec::new();
    checks.extend(sse_scenario(&client, &gateway, &mock_addr.to_string(), &captured).await);
    checks.extend(binary_scenario(&client, &gateway).await);
    checks.extend(status_scenario(&client, &gateway, &mock_addr.to_string(), &captured).await);
    checks.extend(ui_scenario(&client, &gateway).await);
    checks.extend(provider_scenario(&client, &gateway, &mock_addr.to_string(), &captured).await);
    checks.extend(config_scenario(&config, &mock_addr.to_string()));
    checks.extend(no_provider_scenario(&client).await);
    drop(guard); // keep the child alive until every check ran
    checks
}

/// A provider given on the command line lands in the config file, so the next
/// launch can be a plain double-click.
fn config_scenario(config: &Path, mock_addr: &str) -> Vec<Check> {
    let saved = std::fs::read(config)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| v["upstream"].as_str().map(str::to_owned));
    let expected = format!("http://{mock_addr}");
    vec![check("the provider setting is remembered for the next launch", (saved.as_deref() == Some(expected.as_str()))
        .then(|| format!("config.json now names {saved:?}"))
        .ok_or_else(|| format!("config.json has {saved:?}, expected {expected:?}")))]
}

/// A provider added through the UI's editor: listed back with its models,
/// then actually used as the pass-through target — path mapped, key swapped
/// in, extra headers attached — and gone again after Remove.
async fn provider_scenario(client: &reqwest::Client, gateway: &str, mock_addr: &str, captured: &Shared) -> Vec<Check> {
    let post = |path: &str, body: String| {
        let client = client.clone();
        let url = format!("{gateway}{path}");
        async move { client.post(url).header("content-type", "application/json").body(body).send().await }
    };
    let get_json = |url: String| {
        let client = client.clone();
        async move {
            let body = client.get(url).send().await?.text().await?;
            Ok::<_, reqwest::Error>(serde_json::from_str::<serde_json::Value>(&body).expect("api answer is json"))
        }
    };
    let save_body = serde_json::json!({
        "id": "e2e", "new": true, "name": "E2E Vendor", "api": "openai",
        "chat": format!("http://{mock_addr}/v1"), "key": "sk-test-1234",
        "models": ["alpha", "beta"], "headers": { "x-extra": "1" },
    });
    let saved = get_json_raw(&post("/api/provider/save", save_body.to_string()).await.unwrap());
    let listed = get_json(format!("{gateway}/api/providers")).await.unwrap();

    let sent_body = r#"{"model":"alpha","messages":[{"role":"user","content":"hi"}]}"#;
    client
        .post(format!("{gateway}/v1/chat/completions"))
        .header("authorization", "Bearer magpie")
        .header("x-extra", "client")
        .body(sent_body)
        .send()
        .await
        .unwrap();
    let c = captured.lock().unwrap().clone();

    let fetched = get_json_raw(&post("/api/provider/models", serde_json::json!({"id": "e2e"}).to_string()).await.unwrap());
    let after_fetch = get_json(format!("{gateway}/api/providers")).await.unwrap();
    let deleted = get_json_raw(&post("/api/provider/delete", serde_json::json!({"id": "e2e"}).to_string()).await.unwrap());

    vec![
        check("a provider added in the UI is listed back", (saved["providers"][0]["id"] == serde_json::json!("e2e")
            && saved["providers"][0]["models"].as_array().map(|m| m.len()) == Some(2)
            && saved["providers"][0]["key"]["set"] == serde_json::json!(true)
            && saved["gateway"]["models"] == serde_json::json!(2)
            && listed["presets"].is_array())
        .then(|| format!("{} preset tiles, gateway serves {} models", listed["presets"].as_array().map(|p| p.len()).unwrap_or(0), saved["gateway"]["models"]))
        .ok_or_else(|| format!("saved: {saved}, listed: {listed}"))),
        check("requests route through the provider, with its key", (c.path == "/chat/completions"
            && c.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer sk-test-1234")
            && c.headers.get("x-extra").and_then(|v| v.to_str().ok()) == Some("1")
            && c.body == sent_body.as_bytes())
        .then(|| format!("{} {} with the provider's key and headers", c.method, c.path))
        .ok_or_else(|| format!("captured {c:#?}"))),
        check("the vendor's model list lands in the provider", (fetched["count"] == serde_json::json!(3)
            && after_fetch["gateway"]["models"] == serde_json::json!(3))
        .then(|| "3 models fetched, all served".into())
        .ok_or_else(|| format!("fetched {fetched}, after {after_fetch}"))),
        check("a provider removed in the UI is gone", (deleted["providers"].as_array().map(|p| p.is_empty()).unwrap_or(false)
            && deleted["gateway"]["models"] == serde_json::json!(0))
        .then(|| "no providers left, no models served".into())
        .ok_or_else(|| format!("deleted: {deleted}"))),
    ]
}

/// api answers arrive as json bodies (or json errors the page can show)
fn get_json_raw(res: reqwest::Response) -> serde_json::Value {
    let status = res.status();
    let text = res.text().unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "error": text, "status": status.as_u16() }))
}

/// The double-click experience before any provider was ever set: the app
/// still runs and answers with a clear hint instead of refusing to start.
async fn no_provider_scenario(client: &reqwest::Client) -> Vec<Check> {
    let addr = free_port();
    let config = std::env::temp_dir().join(format!("magpie-e2e-empty-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&config);
    let guard = spawn_gateway(addr, "0.0.0.0:1".parse().unwrap(), &config, false);
    let res = client.get(format!("http://{addr}/v1/chat/completions")).send().await.unwrap();
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    drop(guard);

    vec![check("with no provider configured, the app still runs and answers clearly", (status == StatusCode::BAD_GATEWAY && body.contains("provider"))
        .then(|| format!("{status}: {body}"))
        .ok_or_else(|| format!("{status}: {body}")))]
}

/// An OpenAI-style POST whose answer is an SSE stream with a quiet gap in the
/// middle: proves bytes arrive live (not buffered) and arrive complete.
async fn sse_scenario(client: &reqwest::Client, gateway: &str, mock_addr: &str, captured: &Shared) -> Vec<Check> {
    let body = r#"{"model":"m","messages":[{"role":"user","content":"你好 magpie 🎉"}]}"#;
    let request = client
        .post(format!("{gateway}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer magpie")
        .header("x-api-key", "magpie")
        .header("anthropic-version", "2023-06-01")
        .header("x-magpie-probe", "ping")
        .header("connection", "x-magpie-hop")
        .header("x-magpie-hop", "secret")
        .body(body);

    let t0 = Instant::now();
    let res = request.send().await.expect("SSE request through the gateway");
    let res_headers = res.headers().clone();
    let mut stream = res.bytes_stream();
    let first = stream.next().await.expect("at least one SSE chunk").expect("first SSE chunk");
    let ttfb = t0.elapsed();
    let mut received = first.to_vec();
    while let Some(chunk) = stream.next().await {
        received.extend(chunk.expect("SSE chunk"));
    }
    let total = t0.elapsed();

    vec![
        check("SSE body arrives byte for byte", (received == SSE_EXPECTED)
            .then(|| format!("{} bytes", received.len()))
            .ok_or_else(|| format!("expected {SSE_EXPECTED:?}, got {received:?}"))),
        check("SSE streams live instead of buffering", (total >= Duration::from_millis(350) && ttfb < Duration::from_millis(250))
            .then(|| format!("first byte after {ttfb:?}, done after {total:?}"))
            .ok_or_else(|| format!("first byte after {ttfb:?}, done after {total:?} — the gap was swallowed"))),
        check("response headers reach the client", {
            let got = res_headers;
            (got.get("x-request-id").and_then(|v| v.to_str().ok()) == Some("upstream-42")
                && got.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").starts_with("text/event-stream"))
            .then(|| "x-request-id and content-type survived".into())
            .ok_or_else(|| format!("got {:#?}", got))
        }),
        {
            let c = captured.lock().unwrap().clone();
            check("the upstream saw the request intact", (c.method == "POST"
                && c.path == "/v1/chat/completions"
                && c.body == body.as_bytes()
                && c.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer magpie")
                && c.headers.get("anthropic-version").and_then(|v| v.to_str().ok()) == Some("2023-06-01")
                && c.headers.get("x-magpie-probe").and_then(|v| v.to_str().ok()) == Some("ping")
                && !c.headers.contains_key("x-magpie-hop")
                && c.headers.get(header::HOST).and_then(|v| v.to_str().ok()) == Some(mock_addr))
            .then(|| format!("{} {} ({} bytes of body)", c.method, c.path, c.body.len()))
            .ok_or_else(|| format!("captured {c:#?}")))
        },
    ]
}

/// One mebibyte of noise tagged with `content-encoding: gzip` the gateway must
/// neither touch nor decode: round-trips byte for byte.
async fn binary_scenario(client: &reqwest::Client, gateway: &str) -> Vec<Check> {
    let payload: Vec<u8> = (0..(1024 * 1024))
        .map(|i| (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_shr(33) as u8)
        .collect();
    let res = client
        .post(format!("{gateway}/echo"))
        .header("content-encoding", "gzip")
        .body(payload.clone())
        .send()
        .await
        .expect("binary request through the gateway");
    let content_encoding = res.headers().get("content-encoding").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let got = res.bytes().await.expect("binary response body");

    vec![check("a 1 MiB encoded body round-trips untouched", (got == payload
        && content_encoding.as_deref() == Some("gzip"))
    .then(|| format!("{} bytes back", got.len()))
    .ok_or_else(|| format!("{} bytes back, content-encoding {content_encoding:?}", got.len())))]
}

/// A 429 with a query string: status codes, response headers and the query all
/// survive the hop.
async fn status_scenario(client: &reqwest::Client, gateway: &str, mock_addr: &str, captured: &Shared) -> Vec<Check> {
    let res = client
        .get(format!("{gateway}/status?code=429"))
        .send()
        .await
        .expect("status request through the gateway");
    let status = res.status();
    let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let c = captured.lock().unwrap().clone();

    vec![
        check("status codes pass through", (status == StatusCode::TOO_MANY_REQUESTS)
            .then(|| format!("got {status}"))
            .ok_or_else(|| format!("got {status}"))),
        check("response headers pass through", (retry_after.as_deref() == Some("7"))
            .then(|| "retry-after survived".into())
            .ok_or_else(|| format!("retry-after was {retry_after:?}"))),
        check("the query string and method pass through", (c.method == "GET"
            && c.query == "code=429"
            && c.path == "/status"
            && c.headers.get(header::HOST).and_then(|v| v.to_str().ok()) == Some(mock_addr))
        .then(|| format!("{} {}?{}", c.method, c.path, c.query))
        .ok_or_else(|| format!("captured {c:#?}"))),
    ]
}

/// The UI at the gateway root and the handful of /api calls the page needs to
/// boot — plus proof that unknown api calls never leak to the upstream.
async fn ui_scenario(client: &reqwest::Client, gateway: &str) -> Vec<Check> {
    let index = client.get(gateway).send().await.unwrap();
    let index_headers = index.headers().clone();
    let html = index.text().await.unwrap();
    let boot = client.get(format!("{gateway}/boot.js")).send().await.unwrap();
    let boot_headers = boot.headers().clone();
    let get_json = |url: String| {
        let client = client.clone();
        async move {
            let body = client.get(url).send().await?.text().await?;
            Ok::<_, reqwest::Error>(serde_json::from_str::<serde_json::Value>(&body).expect("api answer is json"))
        }
    };
    let state = get_json(format!("{gateway}/api/state")).await.unwrap();
    let providers = get_json(format!("{gateway}/api/providers")).await.unwrap();
    let trace = get_json(format!("{gateway}/api/gateway/trace?after=0")).await.unwrap();
    let no_api = client.get(format!("{gateway}/api/nope")).send().await.unwrap();
    let no_api_headers = no_api.headers().clone();
    let wails = client.get(format!("{gateway}/wails/runtime.js")).send().await.unwrap();
    let wails_headers = wails.headers().clone();

    vec![
        check("the UI is served at the gateway root", (html.contains("<title>magpie</title>")
            && index_headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").starts_with("text/html"))
        .then(|| format!("index.html, {} bytes", html.len()))
        .ok_or_else(|| format!("content-type {index_headers:?}, {} bytes", html.len()))),
        check("the UI's assets are served", (boot.status().is_success()
            && boot_headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").contains("javascript"))
        .then(|| format!("boot.js, {}", boot_headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("")))
        .ok_or_else(|| format!("status {}, headers {boot_headers:?}", boot.status()))),
        check("the UI boots from the api", (state["agents"].is_array()
            && state["settings"].is_object()
            && providers["gateway"]["running"] == serde_json::json!(true)
            && providers["gateway"]["url"] == serde_json::json!(gateway)
            && providers["gateway"]["calls"].is_array())
        .then(|| "state and providers answer in the shapes the page reads".into())
        .ok_or_else(|| format!("state {state}, providers {providers}"))),
        check("the gateway's pass-through calls show in the UI", {
            let calls = providers["gateway"]["calls"].as_array().unwrap();
            (calls.len() >= 3 && calls.iter().any(|c| c["status"] == serde_json::json!(429)))
                .then(|| format!("{} calls recorded", calls.len()))
                .ok_or_else(|| format!("calls: {calls:?}"))
        }),
        check("the routing view hears the gateway", (trace["mine"] == serde_json::json!(true)
            && trace["routes"].is_array()
            && trace["totals"]["requests"].as_u64().unwrap_or(0) >= 3)
        .then(|| format!("totals: {}", trace["totals"]))
        .ok_or_else(|| format!("trace: {trace}"))),
        check("unknown api calls never reach the upstream", (no_api.status() == StatusCode::NOT_FOUND
            && !no_api_headers.contains_key("x-mock-upstream"))
        .then(|| "404 from magpie itself".into())
        .ok_or_else(|| format!("status {}, mock header {:?}", no_api.status(), no_api_headers.get("x-mock-upstream")))),
        check("the shell's wails namespace never reaches the upstream", (wails.status() == StatusCode::NOT_FOUND
            && !wails_headers.contains_key("x-mock-upstream"))
        .then(|| "404 from magpie itself".into())
        .ok_or_else(|| format!("status {}, mock header {:?}", wails.status(), wails_headers.get("x-mock-upstream")))),
    ]
}

async fn spawn_mock(captured: Shared) -> SocketAddr {    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock upstream");
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(mock).with_state(captured);
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock upstream serves") });
    addr
}

/// Records whatever the gateway sent, then answers from a handful of shapes.
async fn mock(State(captured): State<Shared>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("mock reads request body");
    *captured.lock().unwrap() = Captured {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        body: bytes.to_vec(),
        headers: parts.headers.clone(),
    };

    // every mock answer is marked, so a check can prove a request never got here
    let mut res = match parts.uri.path() {
        "/v1/chat/completions" => Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-request-id", "upstream-42")
            .body(sse_body())
            .unwrap(),
        "/v1/models" => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"data":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#))
            .unwrap(),
        "/echo" => Response::builder()
            .header("content-type", "application/octet-stream")
            .header("content-encoding", "gzip")
            .body(Body::from(bytes))
            .unwrap(),
        "/status" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", "7")
            .body(Body::from("slow down"))
            .unwrap(),
        _ => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
    };
    res.headers_mut().insert("x-mock-upstream", HeaderValue::from_static("1"));
    res
}

/// chunk 1, a 400 ms silence, chunk 2, done — timed so buffering shows.
fn sse_body() -> Body {
    Body::from_stream(futures::stream::unfold(0u8, |step| async move {
        let (chunk, next, delay): (Bytes, u8, Duration) = match step {
            0 => (Bytes::from_static(b"data: {\"chunk\":1}\n\n"), 1, Duration::from_millis(30)),
            1 => (Bytes::from_static(b"data: {\"chunk\":2}\n\n"), 2, Duration::from_millis(400)),
            2 => (Bytes::from_static(b"data: [DONE]\n\n"), 3, Duration::ZERO),
            _ => return None,
        };
        tokio::time::sleep(delay).await;
        Some((Ok::<_, Infallible>(chunk), next))
    }))
}

fn spawn_gateway(gateway: SocketAddr, mock: SocketAddr, config: &Path, with_provider: bool) -> Gateway {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magpie-gateway"));
    cmd.arg("--listen").arg(gateway.to_string());
    cmd.arg("--config").arg(config);
    cmd.arg("--no-window");
    if with_provider {
        cmd.arg("--upstream").arg(format!("http://{mock}"));
    }
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("gateway binary starts");
    for _ in 0..150 {
        if TcpStream::connect(gateway).is_ok() {
            return Gateway(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!("gateway never became ready at {gateway}");
}

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

fn write_report(checks: &[Check]) {
    let passed = checks.iter().filter(|c| c.ok).count();
    let report = serde_json::json!({
        "suite": "magpie-gateway pass-through E2E",
        "checks": checks
            .iter()
            .map(|c| serde_json::json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
            .collect::<Vec<_>>(),
        "passed": passed,
        "failed": checks.len() - passed,
        "all_passed": passed == checks.len(),
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("e2e-report.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
}
