//! E2E: the real gateway binary against a mock upstream.
//! Every check proves one facet of lossless pass-through; the run leaves a
//! machine-readable report at the manifest root for CI to upload.

use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use futures::StreamExt;

const SSE_EXPECTED: &[u8] = b"data: {\"chunk\":1}\n\ndata: {\"chunk\":2}\n\ndata: {\"id\":\"c\",\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":100,\"prompt_tokens_details\":{\"cached_tokens\":800},\"completion_tokens_details\":{\"reasoning_tokens\":60}}}\n\ndata: [DONE]\n\n";

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
    Check {
        name,
        ok,
        detail: result.unwrap_or_else(|e| e),
    }
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
        Err(_) => vec![check(
            "the whole suite finishes in 60s",
            Err("timed out".into()),
        )],
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
    checks.extend(usage_scenario(&client, &mock_addr.to_string()).await);
    checks.extend(host_scenario(gateway_addr));
    checks.extend(provider_scenario(&client, &gateway, &mock_addr.to_string(), &captured).await);
    checks.extend(config_scenario(&config, &mock_addr.to_string()));
    checks.extend(no_provider_scenario(&client).await);
    drop(guard); // keep the child alive until every check ran
    checks
}

/// A page that rebound a hostname to 127.0.0.1 would arrive with that
/// hostname in its Host header; the gateway refuses it before anything else,
/// key routes included.
fn host_scenario(gateway_addr: SocketAddr) -> Vec<Check> {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(gateway_addr).expect("connect for the host check");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .write_all(
            b"GET /api/provider/key HTTP/1.1\r\nHost: rebinding.example\r\nConnection: close\r\n\r\n",
        )
        .expect("send the rebound request");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap_or_default();
    let status = buf.lines().next().unwrap_or_default().to_owned();
    vec![check(
        "a rebound Host is refused before anything else",
        status
            .contains(" 403 ")
            .then(|| status.clone())
            .ok_or_else(|| format!("answered {status:?}")),
    )]
}

/// A provider given on the command line lands in the config file, so the next
/// launch can be a plain double-click — and the file holds keys, so on Unix
/// it is owner-only.
fn config_scenario(config: &Path, mock_addr: &str) -> Vec<Check> {
    let saved = std::fs::read(config)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| v["upstream"].as_str().map(str::to_owned));
    let expected = format!("http://{mock_addr}");
    #[cfg(unix)]
    let owner_only = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(config)
            .map(|m| m.permissions().mode() & 0o777)
            .is_ok_and(|mode| mode == 0o600)
    };
    #[cfg(not(unix))]
    let owner_only = true;
    vec![check(
        "the provider setting is remembered for the next launch",
        (saved.as_deref() == Some(expected.as_str()) && owner_only)
            .then(|| format!("config.json now names {saved:?}, owner-only: {owner_only}"))
            .ok_or_else(|| {
                format!(
                    "config.json has {saved:?}, expected {expected:?}, owner-only: {owner_only}"
                )
            }),
    )]
}

/// A provider added through the UI's editor: listed back with its models,
/// then actually used as the pass-through target — path mapped, key swapped
/// in, extra headers attached — and gone again after Remove.
async fn provider_scenario(
    client: &reqwest::Client,
    gateway: &str,
    mock_addr: &str,
    captured: &Shared,
) -> Vec<Check> {
    let post = |path: &str, body: String| {
        let client = client.clone();
        let url = format!("{gateway}{path}");
        async move {
            client
                .post(url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
        }
    };
    let get_json = |url: String| {
        let client = client.clone();
        async move {
            let body = client.get(url).send().await?.text().await?;
            Ok::<_, reqwest::Error>(
                serde_json::from_str::<serde_json::Value>(&body).expect("api answer is json"),
            )
        }
    };
    let save_body = serde_json::json!({
        "id": "e2e", "new": true, "name": "E2E Vendor", "api": "openai",
        "chat": format!("http://{mock_addr}/v1"), "key": "sk-test-1234",
        "models": ["alpha", "beta"], "headers": { "x-extra": "1" },
    });
    let saved = get_json_raw(
        post("/api/provider/save", save_body.to_string())
            .await
            .unwrap(),
    )
    .await;
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

    // Register a second provider e2e-b that also provides the same model "alpha"
    let save_b_body = serde_json::json!({
        "id": "e2e-b", "new": true, "name": "E2E Vendor B", "api": "openai",
        "chat": format!("http://{mock_addr}/b/v1"), "key": "sk-test-b",
        "models": ["alpha"], "headers": { "x-provider": "b" },
    });
    let _ = post("/api/provider/save", save_b_body.to_string())
        .await
        .unwrap();

    // 1. Explicit slash prefix in model name: "e2e-b/alpha"
    let prefix_body = r#"{"model":"e2e-b/alpha","messages":[{"role":"user","content":"hi"}]}"#;
    client
        .post(format!("{gateway}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(prefix_body)
        .send()
        .await
        .unwrap();
    let c_prefix = captured.lock().unwrap().clone();

    // 2. Explicit colon prefix in model name: "e2e-b:alpha"
    let colon_body = r#"{"model":"e2e-b:alpha","messages":[{"role":"user","content":"hi"}]}"#;
    client
        .post(format!("{gateway}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(colon_body)
        .send()
        .await
        .unwrap();
    let c_colon = captured.lock().unwrap().clone();

    // 3. Explicit path prefix: "/e2e-b/v1/chat/completions"
    let path_body = r#"{"model":"alpha","messages":[{"role":"user","content":"hi"}]}"#;
    client
        .post(format!("{gateway}/e2e-b/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(path_body)
        .send()
        .await
        .unwrap();
    let c_path = captured.lock().unwrap().clone();

    // Clean up e2e-b
    let _ = post(
        "/api/provider/delete",
        serde_json::json!({"id": "e2e-b"}).to_string(),
    )
    .await
    .unwrap();

    let fetched = get_json_raw(
        post(
            "/api/provider/models",
            serde_json::json!({"id": "e2e"}).to_string(),
        )
        .await
        .unwrap(),
    )
    .await;
    let after_fetch = get_json(format!("{gateway}/api/providers")).await.unwrap();
    let deleted = get_json_raw(
        post(
            "/api/provider/delete",
            serde_json::json!({"id": "e2e"}).to_string(),
        )
        .await
        .unwrap(),
    )
    .await;

    // vendor logos come from models.dev or embedded assets, served through /api/icons/<id>
    let presets = listed["presets"].as_array().cloned().unwrap_or_default();
    let logo = if presets.is_empty() {
        check(
            "preset tiles carry models.dev logos",
            Ok("no presets to check (catalog offline)".into()),
        )
    } else {
        let id = presets[0]["id"].as_str().unwrap_or("").to_string();
        let icon = presets[0]["icon"].as_str().unwrap_or("").to_string();
        let res = client
            .get(format!("{gateway}/api/icons/{id}"))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let mime = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let cc = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        check(
            "preset tiles carry models.dev logos with cache headers",
            (icon.starts_with("file:")
                && status.is_success()
                && mime.starts_with("image/")
                && cc.contains("max-age="))
            .then(|| format!("{id}: {status} {mime} cc={cc}"))
            .ok_or_else(|| format!("{id}: icon {icon:?}, {status} {mime}, cc {cc:?}")),
        )
    };

    let embedded_res = client
        .get(format!("{gateway}/api/icons/openai"))
        .send()
        .await
        .unwrap();
    let embedded_ok = embedded_res.status().is_success()
        && embedded_res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            == Some("image/svg+xml")
        && embedded_res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(|cc| cc.contains("max-age="))
            .unwrap_or(false);
    let embedded_logo = check(
        "embedded vendor logos resolve immediately from binary with cache headers",
        embedded_ok
            .then(|| "openai: 200 image/svg+xml cached".to_string())
            .ok_or_else(|| format!("openai failed: status={}", embedded_res.status())),
    );

    vec![
        check(
            "a provider added in the UI is listed back",
            (saved["providers"][0]["id"] == serde_json::json!("e2e")
                && saved["providers"][0]["models"].as_array().map(|m| m.len()) == Some(2)
                && saved["providers"][0]["key"]["set"] == serde_json::json!(true)
                && saved["gateway"]["models"] == serde_json::json!(2)
                && listed["presets"].is_array())
            .then(|| {
                format!(
                    "{} preset tiles, gateway serves {} models",
                    listed["presets"].as_array().map(|p| p.len()).unwrap_or(0),
                    saved["gateway"]["models"]
                )
            })
            .ok_or_else(|| format!("saved: {saved}, listed: {listed}")),
        ),
        check(
            "requests route through the provider, with its key",
            (c.path == "/v1/chat/completions"
                && c.headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some("Bearer sk-test-1234")
                && c.headers.get("x-extra").and_then(|v| v.to_str().ok()) == Some("1")
                && c.body == sent_body.as_bytes())
            .then(|| {
                format!(
                    "{} {} with the provider's key and headers",
                    c.method, c.path
                )
            })
            .ok_or_else(|| format!("captured {c:#?}")),
        ),
        check(
            "the vendor's model list lands in the provider",
            (fetched["count"] == serde_json::json!(3)
                && after_fetch["gateway"]["models"] == serde_json::json!(3))
            .then(|| "3 models fetched, all served".into())
            .ok_or_else(|| format!("fetched {fetched}, after {after_fetch}")),
        ),
        check(
            "a provider removed in the UI is gone",
            (deleted["providers"]
                .as_array()
                .map(|p| p.is_empty())
                .unwrap_or(false)
                && deleted["gateway"]["models"] == serde_json::json!(0))
            .then(|| "no providers left, no models served".into())
            .ok_or_else(|| format!("deleted: {deleted}")),
        ),
        check(
            "explicit slash prefix in model routes to provider and strips prefix",
            (c_prefix.path == "/b/v1/chat/completions"
                && c_prefix
                    .headers
                    .get("x-provider")
                    .and_then(|v| v.to_str().ok())
                    == Some("b")
                && serde_json::from_slice::<serde_json::Value>(&c_prefix.body)
                    .map(|v| v["model"] == "alpha")
                    .unwrap_or(false))
            .then(|| "e2e-b/alpha forwarded as alpha to e2e-b".into())
            .ok_or_else(|| format!("c_prefix: {c_prefix:#?}")),
        ),
        check(
            "explicit colon prefix in model routes to provider and strips prefix",
            (c_colon.path == "/b/v1/chat/completions"
                && c_colon
                    .headers
                    .get("x-provider")
                    .and_then(|v| v.to_str().ok())
                    == Some("b")
                && serde_json::from_slice::<serde_json::Value>(&c_colon.body)
                    .map(|v| v["model"] == "alpha")
                    .unwrap_or(false))
            .then(|| "e2e-b:alpha forwarded as alpha to e2e-b".into())
            .ok_or_else(|| format!("c_colon: {c_colon:#?}")),
        ),
        check(
            "explicit path prefix routes to provider",
            (c_path.path == "/b/v1/chat/completions"
                && c_path
                    .headers
                    .get("x-provider")
                    .and_then(|v| v.to_str().ok())
                    == Some("b")
                && serde_json::from_slice::<serde_json::Value>(&c_path.body)
                    .map(|v| v["model"] == "alpha")
                    .unwrap_or(false))
            .then(|| "/e2e-b/v1/chat/completions routed to e2e-b".into())
            .ok_or_else(|| format!("c_path: {c_path:#?}")),
        ),
        logo,
        embedded_logo,
    ]
}

/// api answers arrive as json bodies (or json errors the page can show)
async fn get_json_raw(res: reqwest::Response) -> serde_json::Value {
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    serde_json::from_str(&text)
        .unwrap_or_else(|_| serde_json::json!({ "error": text, "status": status.as_u16() }))
}

/// The Usage tab's numbers, from answers whose vendors reported usage in both
/// wire shapes — OpenAI's (the prompt includes the cached tokens) and
/// Anthropic's (input_tokens already excludes them): the tokens of the window
/// and the pi-style cache hit rate, with the log on disk as the artifact.
async fn usage_scenario(client: &reqwest::Client, mock_addr: &str) -> Vec<Check> {
    let addr = free_port();
    let config = std::env::temp_dir().join(format!("magpie-e2e-usage-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&config);
    let guard = spawn_gateway(addr, mock_addr.parse().unwrap(), &config, true);
    let gateway = format!("http://{addr}");

    // an OpenAI-style SSE answer: 1000 prompt with 800 cached → 200 in, 800
    // read; 100 out, 60 of them reasoning
    let sse = client
        .post(format!("{gateway}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    let _ = sse.bytes().await.unwrap();

    // an Anthropic-style answer: 500 in (cache excluded), 300 read, 200 written
    // (read to the end: the gateway counts the call when its stream settles)
    let _ = client
        .post(format!("{gateway}/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-x","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let u: serde_json::Value = client
        .get(format!("{gateway}/api/usage?period=today"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .parse()
        .unwrap();
    let stored = std::fs::read(config.with_file_name("usage.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    drop(guard);

    let hit = u["hit_rate"].as_f64();
    let models = u["models"].as_array().cloned().unwrap_or_default();
    vec![
        check(
            "the usage tab counts the window's tokens across both wire shapes",
            (u["calls"] == serde_json::json!(2)
                && u["input"] == serde_json::json!(700)
                && u["output"] == serde_json::json!(150)
                && u["cache_read"] == serde_json::json!(1100)
                && u["cache_write"] == serde_json::json!(200)
                && u["reasoning"] == serde_json::json!(60)
                && u["errors"] == serde_json::json!(0))
            .then(|| {
                format!(
                    "{} calls, {} in, {} out, cached {}/{}",
                    u["calls"], u["input"], u["output"], u["cache_read"], u["cache_write"]
                )
            })
            .ok_or_else(|| format!("usage: {u}")),
        ),
        check(
            "the cache hit rate is cache read over the whole reported prompt",
            (hit == Some(55.0))
                .then(|| format!("hit rate {hit:?}% (1100 of 2000 prompt tokens)"))
                .ok_or_else(|| format!("hit_rate was {hit:?} in {u}")),
        ),
        check(
            "the window's series and per-model rows come along",
            (u["bucket"] == serde_json::json!("hour")
                && u["series"].as_array().map(|s| s.len()) == Some(24)
                && models.len() == 2)
                .then(|| {
                    format!(
                        "bucket hour, {} hour buckets, models: {}",
                        u["series"].as_array().map(|s| s.len()).unwrap_or(0),
                        models
                            .iter()
                            .filter_map(|m| m["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .ok_or_else(|| format!("usage: {u}")),
        ),
        check(
            "the usage log lands next to the config for the next launch",
            stored
                .as_ref()
                .and_then(|s| s.as_array())
                .is_some_and(|a| a.len() == 2)
                .then(|| "usage.json holds both calls".to_owned())
                .ok_or_else(|| {
                    format!(
                        "usage.json missing or wrong at {}",
                        config.with_file_name("usage.json").display()
                    )
                }),
        ),
    ]
}

/// The double-click experience before any provider was ever set: the app
/// still runs and answers with a clear hint instead of refusing to start.
async fn no_provider_scenario(client: &reqwest::Client) -> Vec<Check> {
    let addr = free_port();
    let config = std::env::temp_dir().join(format!("magpie-e2e-empty-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&config);
    let guard = spawn_gateway(addr, "0.0.0.0:1".parse().unwrap(), &config, false);
    let res = client
        .get(format!("http://{addr}/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    drop(guard);

    vec![check(
        "with no provider configured, the app still runs and answers clearly",
        (status == StatusCode::BAD_GATEWAY && body.contains("provider"))
            .then(|| format!("{status}: {body}"))
            .ok_or_else(|| format!("{status}: {body}")),
    )]
}

/// An OpenAI-style POST whose answer is an SSE stream with a quiet gap in the
/// middle: proves bytes arrive live (not buffered) and arrive complete.
async fn sse_scenario(
    client: &reqwest::Client,
    gateway: &str,
    mock_addr: &str,
    captured: &Shared,
) -> Vec<Check> {
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
    let res = request
        .send()
        .await
        .expect("SSE request through the gateway");
    let res_headers = res.headers().clone();
    let mut stream = res.bytes_stream();
    let first = stream
        .next()
        .await
        .expect("at least one SSE chunk")
        .expect("first SSE chunk");
    let ttfb = t0.elapsed();
    let mut received = first.to_vec();
    while let Some(chunk) = stream.next().await {
        received.extend(chunk.expect("SSE chunk"));
    }
    let total = t0.elapsed();

    vec![
        check(
            "SSE body arrives byte for byte",
            (received == SSE_EXPECTED)
                .then(|| format!("{} bytes", received.len()))
                .ok_or_else(|| format!("expected {SSE_EXPECTED:?}, got {received:?}")),
        ),
        check(
            "SSE streams live instead of buffering",
            (total >= Duration::from_millis(350) && ttfb < Duration::from_millis(250))
                .then(|| format!("first byte after {ttfb:?}, done after {total:?}"))
                .ok_or_else(|| {
                    format!(
                        "first byte after {ttfb:?}, done after {total:?} — the gap was swallowed"
                    )
                }),
        ),
        check("response headers reach the client", {
            let got = res_headers;
            (got.get("x-request-id").and_then(|v| v.to_str().ok()) == Some("upstream-42")
                && got
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .starts_with("text/event-stream"))
            .then(|| "x-request-id and content-type survived".into())
            .ok_or_else(|| format!("got {:#?}", got))
        }),
        {
            let c = captured.lock().unwrap().clone();
            check(
                "the upstream saw the request intact",
                (c.method == "POST"
                    && c.path == "/v1/chat/completions"
                    && c.body == body.as_bytes()
                    && c.headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer magpie")
                    && c.headers
                        .get("anthropic-version")
                        .and_then(|v| v.to_str().ok())
                        == Some("2023-06-01")
                    && c.headers
                        .get("x-magpie-probe")
                        .and_then(|v| v.to_str().ok())
                        == Some("ping")
                    && !c.headers.contains_key("x-magpie-hop")
                    && c.headers.get(header::HOST).and_then(|v| v.to_str().ok())
                        == Some(mock_addr))
                .then(|| format!("{} {} ({} bytes of body)", c.method, c.path, c.body.len()))
                .ok_or_else(|| format!("captured {c:#?}")),
            )
        },
    ]
}

/// One mebibyte of noise tagged with `content-encoding: gzip` the gateway must
/// neither touch nor decode: round-trips byte for byte.
async fn binary_scenario(client: &reqwest::Client, gateway: &str) -> Vec<Check> {
    let payload: Vec<u8> = (0..(1024 * 1024))
        .map(|i| {
            (i as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_shr(33) as u8
        })
        .collect();
    let res = client
        .post(format!("{gateway}/echo"))
        .header("content-encoding", "gzip")
        .body(payload.clone())
        .send()
        .await
        .expect("binary request through the gateway");
    let content_encoding = res
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let got = res.bytes().await.expect("binary response body");

    vec![check(
        "a 1 MiB encoded body round-trips untouched",
        (got == payload && content_encoding.as_deref() == Some("gzip"))
            .then(|| format!("{} bytes back", got.len()))
            .ok_or_else(|| {
                format!(
                    "{} bytes back, content-encoding {content_encoding:?}",
                    got.len()
                )
            }),
    )]
}

/// A 429 with a query string: status codes, response headers and the query all
/// survive the hop.
async fn status_scenario(
    client: &reqwest::Client,
    gateway: &str,
    mock_addr: &str,
    captured: &Shared,
) -> Vec<Check> {
    let res = client
        .get(format!("{gateway}/status?code=429"))
        .send()
        .await
        .expect("status request through the gateway");
    let status = res.status();
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let c = captured.lock().unwrap().clone();

    vec![
        check(
            "status codes pass through",
            (status == StatusCode::TOO_MANY_REQUESTS)
                .then(|| format!("got {status}"))
                .ok_or_else(|| format!("got {status}")),
        ),
        check(
            "response headers pass through",
            (retry_after.as_deref() == Some("7"))
                .then(|| "retry-after survived".into())
                .ok_or_else(|| format!("retry-after was {retry_after:?}")),
        ),
        check(
            "the query string and method pass through",
            (c.method == "GET"
                && c.query == "code=429"
                && c.path == "/status"
                && c.headers.get(header::HOST).and_then(|v| v.to_str().ok()) == Some(mock_addr))
            .then(|| format!("{} {}?{}", c.method, c.path, c.query))
            .ok_or_else(|| format!("captured {c:#?}")),
        ),
    ]
}

/// The UI at the gateway root and the handful of /api calls the page needs to
/// boot — plus proof that unknown api calls never leak to the upstream.
async fn ui_scenario(client: &reqwest::Client, gateway: &str) -> Vec<Check> {
    let index = client.get(gateway).send().await.unwrap();
    let index_headers = index.headers().clone();
    let html = index.text().await.unwrap();
    let boot = client
        .get(format!("{gateway}/boot.js"))
        .send()
        .await
        .unwrap();
    let boot_headers = boot.headers().clone();
    let get_json = |url: String| {
        let client = client.clone();
        async move {
            let body = client.get(url).send().await?.text().await?;
            Ok::<_, reqwest::Error>(
                serde_json::from_str::<serde_json::Value>(&body).expect("api answer is json"),
            )
        }
    };
    let state = get_json(format!("{gateway}/api/state")).await.unwrap();
    let providers = get_json(format!("{gateway}/api/providers")).await.unwrap();
    let trace = get_json(format!("{gateway}/api/gateway/trace?after=0"))
        .await
        .unwrap();
    let no_api = client
        .get(format!("{gateway}/api/nope"))
        .send()
        .await
        .unwrap();
    let no_api_headers = no_api.headers().clone();

    vec![
        check(
            "the UI is served at the gateway root",
            (html.contains("<title>magpie</title>")
                && index_headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .starts_with("text/html"))
            .then(|| format!("index.html, {} bytes", html.len()))
            .ok_or_else(|| format!("content-type {index_headers:?}, {} bytes", html.len())),
        ),
        check(
            "the UI's assets are served",
            (boot.status().is_success()
                && boot_headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .contains("javascript"))
            .then(|| {
                format!(
                    "boot.js, {}",
                    boot_headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                )
            })
            .ok_or_else(|| format!("status {}, headers {boot_headers:?}", boot.status())),
        ),
        check(
            "the UI boots from the api",
            (state["agents"].is_array()
                && state["settings"].is_object()
                && providers["gateway"]["running"] == serde_json::json!(true)
                && providers["gateway"]["url"] == serde_json::json!(gateway)
                && providers["gateway"]["calls"].is_array())
            .then(|| "state and providers answer in the shapes the page reads".into())
            .ok_or_else(|| format!("state {state}, providers {providers}")),
        ),
        check("the gateway's pass-through calls show in the UI", {
            let calls = providers["gateway"]["calls"].as_array().unwrap();
            (calls.len() >= 3 && calls.iter().any(|c| c["status"] == serde_json::json!(429)))
                .then(|| format!("{} calls recorded", calls.len()))
                .ok_or_else(|| format!("calls: {calls:?}"))
        }),
        check(
            "the routing view hears the gateway",
            (trace["mine"] == serde_json::json!(true)
                && trace["routes"].is_array()
                && trace["totals"]["requests"].as_u64().unwrap_or(0) >= 3)
                .then(|| format!("totals: {}", trace["totals"]))
                .ok_or_else(|| format!("trace: {trace}")),
        ),
        check(
            "unknown api calls never reach the upstream",
            (no_api.status() == StatusCode::NOT_FOUND
                && !no_api_headers.contains_key("x-mock-upstream"))
            .then(|| "404 from magpie itself".into())
            .ok_or_else(|| {
                format!(
                    "status {}, mock header {:?}",
                    no_api.status(),
                    no_api_headers.get("x-mock-upstream")
                )
            }),
        ),
    ]
}

async fn spawn_mock(captured: Shared) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(mock).with_state(captured);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream serves")
    });
    addr
}

/// Records whatever the gateway sent, then answers from a handful of shapes.
async fn mock(State(captured): State<Shared>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX)
        .await
        .expect("mock reads request body");
    *captured.lock().unwrap() = Captured {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        body: bytes.to_vec(),
        headers: parts.headers.clone(),
    };

    // every mock answer is marked, so a check can prove a request never got here
    let mut res = match parts.uri.path() {
        p if p.ends_with("/chat/completions") => Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-request-id", "upstream-42")
            .body(sse_body())
            .unwrap(),
        "/v1/models" => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"data":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#))
            .unwrap(),
        // an Anthropic-style answer: its usage names the cache buckets itself,
        // and its input_tokens already excludes them
        "/v1/messages" => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":500,"output_tokens":50,"cache_creation_input_tokens":200,"cache_read_input_tokens":300}}"#,
            ))
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
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    };
    res.headers_mut()
        .insert("x-mock-upstream", HeaderValue::from_static("1"));
    res
}

/// chunk 1, a 400 ms silence, chunk 2, the usage report, done — timed so
/// buffering shows
fn sse_body() -> Body {
    Body::from_stream(futures::stream::unfold(0u8, |step| async move {
        let (chunk, next, delay): (Bytes, u8, Duration) = match step {
            0 => (
                Bytes::from_static(b"data: {\"chunk\":1}\n\n"),
                1,
                Duration::from_millis(30),
            ),
            1 => (
                Bytes::from_static(b"data: {\"chunk\":2}\n\n"),
                2,
                Duration::from_millis(400),
            ),
            2 => (
                Bytes::from_static(
                    b"data: {\"id\":\"c\",\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":100,\"prompt_tokens_details\":{\"cached_tokens\":800},\"completion_tokens_details\":{\"reasoning_tokens\":60}}}\n\n",
                ),
                3,
                Duration::ZERO,
            ),
            3 => (Bytes::from_static(b"data: [DONE]\n\n"), 4, Duration::ZERO),
            _ => return None,
        };
        tokio::time::sleep(delay).await;
        Some((Ok::<_, Infallible>(chunk), next))
    }))
}

fn spawn_gateway(
    gateway: SocketAddr,
    mock: SocketAddr,
    config: &Path,
    with_provider: bool,
) -> Gateway {
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
    let _ = child.wait();
    panic!("gateway never became ready at {gateway}");
}

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
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
