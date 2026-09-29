//! The Providers tab's backend: presets to add from, the CRUD the editor
//! speaks, model-list fetches and endpoint tests — and the decision of where
//! a passing-through request goes, per protocol, among the configured
//! providers.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{App, ConfigState, Logo, persist, shell_state};

// ---------- the payload the UI renders ----------

pub async fn list(State(app): State<Arc<App>>) -> Response {
    let cfg = app.config.lock().await;
    Json(payload(&app, &cfg)).into_response()
}

/// The header's refresh button: ask models.dev again, so its vendor list and
/// model names come back current.
pub async fn sync(State(app): State<Arc<App>>) -> Response {
    match fetch_catalog(&app.client()).await {
        Ok(v) => {
            store_catalog(&app, v);
            shell_state()
        }
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            &format!("models.dev didn't answer: {e}"),
        ),
    }
}

/// A fetched catalog replaces the one in memory and the disk cache together;
/// a failed cache write only costs the next launch a cold fetch.
pub fn store_catalog(app: &App, v: Value) {
    if let Some(dir) = app.catalog_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_vec(&v) {
        let _ = std::fs::write(&app.catalog_path, body);
    }
    *app.catalog.lock().unwrap() = v;
}

pub async fn fetch_catalog(client: &reqwest::Client) -> anyhow::Result<Value> {
    let text = client
        .get("https://models.dev/api.json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&text)?)
}

/// A vendor's logo, resolved via a multi-tier cache:
/// 1. In-memory `app.logos` cache.
/// 2. Embedded assets in binary (`UI` static dir) with vendor aliases.
/// 3. Persistent disk cache in `app.logos_dir`.
/// 4. Remote fetch from models.dev with a short timeout, saved to disk on success.
///
/// All successful responses include aggressive Cache-Control headers for the webview.
pub async fn icon(
    State(app): State<Arc<App>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Some(hit) = app.logos.lock().unwrap().get(&id).cloned() {
        return respond_logo(hit);
    }

    if let Some((bytes, mime)) = find_embedded_logo(&id) {
        app.logos
            .lock()
            .unwrap()
            .insert(id, Some((bytes.clone(), mime.clone())));
        return respond_logo(Some((bytes, mime)));
    }

    if let Some((bytes, mime)) = find_disk_logo(&app.logos_dir, &id) {
        app.logos
            .lock()
            .unwrap()
            .insert(id, Some((bytes.clone(), mime.clone())));
        return respond_logo(Some((bytes, mime)));
    }

    let fetched = fetch_and_persist_logo(&app.client(), &app.logos_dir, &id).await;
    app.logos.lock().unwrap().insert(id, fetched.clone());
    respond_logo(fetched)
}

fn respond_logo(logo: Logo) -> Response {
    match logo {
        Some((bytes, mime)) => (
            [
                (header::CONTENT_TYPE, mime),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=2592000, immutable".to_string(),
                ),
            ],
            bytes,
        )
            .into_response(),
        None => (
            [(header::CACHE_CONTROL, "public, max-age=300".to_string())],
            StatusCode::NOT_FOUND,
        )
            .into_response(),
    }
}

fn find_embedded_logo(id: &str) -> Option<(Vec<u8>, String)> {
    let mut names = vec![
        format!("icons/{id}-color.svg"),
        format!("icons/{id}.svg"),
        format!("icons/{id}.png"),
    ];
    match id {
        "google" => {
            names.push("icons/gemini-color.svg".into());
            names.push("icons/googlecloud-color.svg".into());
        }
        "gemini" => names.push("icons/gemini-color.svg".into()),
        "claude" => {
            names.push("icons/claude-color.svg".into());
            names.push("icons/anthropic.svg".into());
        }
        "siliconflow" => names.push("icons/siliconcloud-color.svg".into()),
        "zhipuai" | "glm" => names.push("icons/zhipu-color.svg".into()),
        "github" | "copilot" => names.push("icons/githubcopilot.svg".into()),
        "togetherai" => names.push("icons/together-color.svg".into()),
        "mistralai" => names.push("icons/mistral-color.svg".into()),
        "kimi" => names.push("icons/kimi.svg".into()),
        "dashscope" | "tongyi" => names.push("icons/qwen-color.svg".into()),
        "tencent" | "hunyuan" => names.push("icons/tencentcloud-color.svg".into()),
        "fireworksai" => names.push("icons/fireworks-color.svg".into()),
        _ => {}
    }
    for name in names {
        if let Some(file) = crate::UI.get_file(&name) {
            let mime = if name.ends_with(".png") {
                "image/png"
            } else {
                "image/svg+xml"
            };
            return Some((file.contents().to_vec(), mime.to_owned()));
        }
    }
    None
}

fn find_disk_logo(logos_dir: &Path, id: &str) -> Option<(Vec<u8>, String)> {
    for (ext, mime) in LOGO_EXTS {
        if let Ok(bytes) = std::fs::read(logos_dir.join(format!("{id}.{ext}"))) {
            return Some((bytes, mime.to_owned()));
        }
    }
    None
}

/// the file extensions a logo may live under, with their content types
const LOGO_EXTS: [(&str, &str); 7] = [
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("ico", "image/x-icon"),
];

/// the extension of an image the gateway fetched or was given, read off its
/// first bytes — a picture's content type is not to be trusted
fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [0xFF, 0xD8, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', ..] if bytes.len() > 12 && &bytes[8..12] == b"WEBP" => {
            Some("webp")
        }
        [0x00, 0x00, 0x01, 0x00, ..] => Some("ico"),
        _ if bytes.starts_with(b"<svg")
            || bytes.starts_with(b"<?xml")
            || bytes.starts_with(b"\xEF\xBB\xBF<") =>
        {
            Some("svg")
        }
        _ => None,
    }
}

/// an id for a picture of the user's own: a time, so uploads never collide
fn icon_id() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "c{:x}{n:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    )
}

fn store_logo(logos_dir: &Path, id: &str, ext: &str, bytes: &[u8]) {
    let _ = std::fs::create_dir_all(logos_dir);
    let _ = std::fs::write(logos_dir.join(format!("{id}.{ext}")), bytes);
}

/// A picture of the user's own, sent as base64 in JSON (the app's web view
/// drops a File sent as the body).
pub async fn upload_icon(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(data) = body["data"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "no picture given");
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return err(StatusCode::BAD_REQUEST, "that is not base64");
    };
    let Some(ext) = image_ext(&bytes) else {
        return err(
            StatusCode::BAD_REQUEST,
            "that is not a picture magpie can show",
        );
    };
    let id = icon_id();
    store_logo(&app.logos_dir, &id, ext, &bytes);
    Json(json!({ "icon": format!("file:{id}") })).into_response()
}

/// The icon of the site a base URL is on, for a custom provider.
pub async fn favicon(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(url) = body["url"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "no URL given");
    };
    let full = if url.contains("://") {
        url.to_owned()
    } else {
        format!("https://{url}")
    };
    let Some(host) = reqwest::Url::parse(&full)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    else {
        return err(StatusCode::BAD_REQUEST, "that is not a URL");
    };
    // Google's favicon service has almost every site; the site's own
    // /favicon.ico is there for when it has none — asked right under the
    // base URL the user typed, port and scheme included
    let urls = [
        format!("https://www.google.com/s2/favicons?domain={host}&sz=64"),
        format!("{}/favicon.ico", full.trim_end_matches('/')),
    ];
    let client = app.client();
    for url in urls {
        if let Ok(res) = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            && res.status().is_success()
            && let Ok(bytes) = res.bytes().await
            && !bytes.is_empty()
            && let Some(ext) = image_ext(&bytes)
        {
            let id = icon_id();
            store_logo(&app.logos_dir, &id, ext, &bytes);
            return Json(json!({ "icon": format!("file:{id}") })).into_response();
        }
    }
    err(StatusCode::BAD_GATEWAY, "no icon found for that site")
}

async fn fetch_and_persist_logo(client: &reqwest::Client, logos_dir: &Path, id: &str) -> Logo {
    for (ext, mime) in [("svg", "image/svg+xml"), ("png", "image/png")] {
        let url = format!("https://models.dev/logos/{id}.{ext}");
        let req = client.get(&url).timeout(Duration::from_secs(4));
        if let Ok(res) = req.send().await
            && res.status().is_success()
            && let Ok(bytes) = res.bytes().await
        {
            let data = bytes.to_vec();
            let _ = std::fs::create_dir_all(logos_dir);
            let disk_file = logos_dir.join(format!("{id}.{ext}"));
            let _ = std::fs::write(disk_file, &data);
            return Some((data, mime.to_owned()));
        }
    }
    None
}

pub fn payload(app: &App, cfg: &ConfigState) -> Value {
    let catalog = app.catalog.lock().unwrap();
    let providers: Vec<Value> = cfg.providers.iter().map(|p| enrich(p, &catalog)).collect();
    let models: usize = cfg
        .providers
        .iter()
        .map(|p| {
            if p.models.is_empty() {
                let cat_key = if !p.catalog.is_empty() {
                    &p.catalog
                } else {
                    &p.id
                };
                catalog
                    .get(cat_key.as_str())
                    .and_then(|c| c.get("models"))
                    .and_then(Value::as_object)
                    .map(|m| m.len())
                    .unwrap_or(0)
            } else {
                p.models.iter().filter(|m| m.on).count()
            }
        })
        .sum();
    let calls: Vec<Value> = app
        .gateway
        .calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .cloned()
        .collect();
    json!({
        "providers": providers,
        "presets": presets(&catalog),
        // the catalog is fetched in the background; while it is still on its
        // way the presets are not "there are no vendors" but "not loaded yet"
        "catalogReady": !catalog.is_null(),
        "excluded": [],
        "gateway": {
            "running": true,
            "mine": true,
            "url": app.gateway.url,
            "models": models,
            "groups": [],
            "calls": calls,
        },
    })
}

/// A stored provider, plus the derived bits the list row reads: display
/// names, context windows and reasoning levels come from the models.dev
/// catalog when it knows the model.
fn enrich(p: &Provider, catalog: &Value) -> Value {
    let mut out = serde_json::to_value(p).unwrap_or_default();
    let host = host_of(&p.chat);
    let local = matches!(
        host.as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "0.0.0.0"
    );
    out["key"] = json!({ "set": !p.key.is_empty(), "masked": mask(&p.key) });
    out["host"] = json!(host);
    out["ready"] = json!(p.key.is_empty() && local);
    out["agents"] = json!([]);
    let cat_key = if !p.catalog.is_empty() {
        &p.catalog
    } else {
        &p.id
    };
    let names = catalog
        .get(cat_key.as_str())
        .and_then(|c| c.get("models"))
        .and_then(Value::as_object);
    // a provider with no stored list shows the catalog's, on by default
    if out["models"].as_array().is_none_or(|m| m.is_empty())
        && let Some(cat_models) = names
    {
        let seeded: Vec<Value> = cat_models
            .iter()
            .map(|(id, m)| {
                json!({
                    "id": id,
                    "on": true,
                    "name": m.get("name").and_then(Value::as_str).unwrap_or(id),
                })
            })
            .collect();
        out["models"] = json!(seeded);
    }
    if let (Some(names), Some(models)) = (names, out["models"].as_array_mut()) {
        for m in models {
            let Some(id) = m["id"].as_str() else { continue };
            let Some(cat) = names.get(id) else { continue };
            // the vendor's own name is the default; a stored name overrides it
            if let Some(name) = cat["name"].as_str() {
                m["default"] = json!(name);
                if m["name"].as_str().is_none() {
                    m["name"] = json!(name);
                }
            }
            if let Some(limit) = cat.get("limit") {
                m["context"] = limit.get("context").cloned().unwrap_or(Value::Null);
                m["max"] = limit.get("output").cloned().unwrap_or(Value::Null);
            }
            let levels: Vec<&str> = cat
                .get("reasoning_options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|o| o.get("type").and_then(Value::as_str) == Some("effort"))
                .flat_map(|o| {
                    o.get("values")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                })
                .collect();
            if !levels.is_empty() {
                m["efforts"] = json!(levels);
            }
        }
    }
    out
}

/// A model's catalog entry, by id, with the vendor that has it.
pub fn catalog_model<'a>(catalog: &'a Value, id: &str) -> Option<(&'a Value, &'a str)> {
    catalog
        .as_object()?
        .iter()
        .find_map(|(vendor, v)| Some((v.get("models")?.get(id)?, vendor.as_str())))
}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    match chars.len() {
        0 => String::new(),
        n @ 1..=8 => "•".repeat(n),
        n => format!(
            "{}…{}",
            chars[..4].iter().collect::<String>(),
            chars[n - 4..].iter().collect::<String>()
        ),
    }
}

fn host_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(port)) => format!("{h}:{port}"),
            (Some(h), None) => h.to_owned(),
            (None, _) => String::new(),
        },
        Err(_) => String::new(),
    }
}

/// The add sheet's tiles: every vendor models.dev knows, each with the
/// endpoint models.dev publishes (a handful of big ones don't carry one —
/// those come from the fallback list). One entry needs no more than a key.
fn presets(catalog: &Value) -> Value {
    let mut out: Vec<Value> = catalog
        .as_object()
        .map(|o| o.values().filter_map(preset_from).collect())
        .unwrap_or_default();
    out.sort_by_key(|p| p["name"].as_str().unwrap_or("").to_lowercase());
    json!(out)
}

/// Endpoints models.dev leaves out, for the vendors everyone has heard of.
fn fallback_url(id: &str) -> Option<&'static str> {
    Some(match id {
        "openai" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "google" => "https://generativelanguage.googleapis.com/v1beta/openai",
        "groq" => "https://api.groq.com/openai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "xai" => "https://api.x.ai/v1",
        "togetherai" => "https://api.together.xyz/v1",
        "aihubmix" => "https://aihubmix.com/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        "perplexity" => "https://api.perplexity.ai",
        "cohere" => "https://api.cohere.ai/compatibility/v1",
        _ => return None,
    })
}

fn preset_from(p: &Value) -> Option<Value> {
    let id = p["id"].as_str()?;
    let url = p["api"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| fallback_url(id).map(str::to_owned))
        .filter(|u| u.starts_with("http") && !u.contains("${"))?;
    let host = reqwest::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    Some(json!({
        "id": id,
        "name": p["name"].as_str().unwrap_or(id),
        "kind": if matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "0.0.0.0") {
            "local"
        } else {
            "vendor"
        },
        "chat": url,
        "catalog": id,
        // the UI renders "file:" icons from /api/icons/<id>, where the
        // vendor's models.dev logo is served; a 404 falls back to generic
        "icon": format!("file:{id}"),
    }))
}

// ---------- the config's shape ----------

/// One row in the Providers tab, as stored in config.json. The fields the
/// gateway reads are typed; everything else the UI attaches (presets,
/// balance URLs, fallbacks, contexts, whatever it grows next) rides along in
/// `extra` and survives a save untouched.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat: String,
    /// when the vendor's own model list was last fetched, for the editor's
    /// "vendor list" hint; the models.dev list stands in until then
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub catalog: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(
        rename = "modelsURL",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub models_url: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unlisted: bool,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub headers: Map<String, Value>,
    /// always serialized, even empty: the list row reads it unguarded
    #[serde(default, deserialize_with = "de_models")]
    pub models: Vec<Model>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A model as the UI sees it: an id, its on flag, and the display name a
/// models.dev refresh may have given it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Models may be stored as bare id strings (a hand-edited config).
fn de_models<'de, D>(d: D) -> Result<Vec<Model>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<Value> = Deserialize::deserialize(d)?;
    raw.into_iter()
        .map(|v| match v {
            Value::String(id) => Ok(Model {
                id,
                on: true,
                name: None,
                extra: Map::new(),
            }),
            other => serde_json::from_value(other).map_err(serde::de::Error::custom),
        })
        .collect()
}

/// Everything after a `/v1` of its own: the version sits in both the
/// client's path and the base URL, so it is counted once — and the slash of
/// the prefix stays, so the rest still starts with one.
fn after_v1(path: &str) -> &str {
    path.strip_prefix("/v1").unwrap_or(path)
}

// ---------- the editor's endpoints ----------

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// The config-writing endpoints' shared failure path: persist breaking is a
/// 500 with the reason; `None` leaves the answering to the caller.
fn persisted(app: &App, cfg: &ConfigState) -> Option<Response> {
    let e = persist(&app.config_path, cfg).err()?;
    Some(err(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("could not save the config: {e}"),
    ))
}

/// The editor's save. It arrives with `from` when renaming; a partial save
/// only touches the fields it carries, so a models toggle keeps the key.
#[derive(Deserialize)]
struct SaveRequest {
    id: String,
    from: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    catalog: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(rename = "modelsURL", default)]
    models_url: Option<String>,
    #[serde(default)]
    unlisted: Option<bool>,
    #[serde(default)]
    headers: Option<Map<String, Value>>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

pub async fn save(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let req = match serde_json::from_value::<SaveRequest>(body) {
        Ok(req) if !req.id.is_empty() => req,
        _ => return err(StatusCode::BAD_REQUEST, "the provider needs an id"),
    };
    let mut cfg = app.config.lock().await;
    let pos = cfg
        .providers
        .iter()
        .position(|p| p.id == req.from.clone().unwrap_or_else(|| req.id.clone()));
    let mut rec = pos
        .and_then(|i| cfg.providers.get(i).cloned())
        .unwrap_or_default();
    rec.id = req.id.clone();
    if let Some(v) = req.name {
        rec.name = v;
    }
    if rec.name.is_empty() {
        rec.name = rec.id.clone();
    }
    if let Some(v) = req.key {
        rec.key = v;
    }
    if let Some(v) = req.chat {
        rec.chat = v;
    }
    if let Some(v) = req.catalog {
        rec.catalog = v;
    }
    if let Some(v) = req.icon {
        rec.icon = v;
    }
    if rec.icon.is_empty() {
        rec.icon = "generic".to_owned();
    }
    if let Some(v) = req.models_url {
        rec.models_url = v;
    }
    if let Some(v) = req.unlisted {
        rec.unlisted = v;
    }
    if let Some(v) = req.headers {
        rec.headers = v;
    }
    // presets, balance URLs, fallbacks, contexts — anything else the UI
    // carries lands in `extra`
    rec.extra.extend(req.rest);
    // per-protocol URLs of older builds are not read any more
    for k in ["api", "responses", "anthropic", "gemini"] {
        rec.extra.shift_remove(k);
    }
    // a preset's URLs come from the catalog, not the form: a preset asks
    // only for a key
    let preset_id = rec
        .extra
        .get("preset")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if !preset_id.is_empty() && rec.chat.is_empty() {
        let url = {
            let cat = app.catalog.lock().unwrap();
            cat.get(&preset_id)
                .and_then(|e| e["api"].as_str())
                .map(str::to_owned)
                .filter(|u| u.starts_with("http") && !u.contains("${"))
                .or_else(|| fallback_url(&preset_id).map(str::to_owned))
        };
        if let Some(u) = url.as_deref().map(|u| u.trim_end_matches('/')) {
            rec.chat = u.to_owned();
        }
    }
    let had_models = req.models.as_ref().is_some_and(|m| !m.is_empty());
    if let Some(on) = req.models {
        rec.models = merge_models(&rec.models, &on);
    }
    if rec.models.is_empty() {
        let catalog_key = if !rec.catalog.is_empty() {
            &rec.catalog
        } else if let Some(preset) = rec.extra.get("preset").and_then(Value::as_str) {
            preset
        } else {
            &rec.id
        };
        let cat = app.catalog.lock().unwrap();
        if let Some(cat_models) = cat
            .get(catalog_key)
            .and_then(|c| c.get("models"))
            .and_then(Value::as_object)
        {
            let ids: Vec<String> = cat_models.keys().cloned().collect();
            rec.models = merge_models(&rec.models, &ids);
        }
    }
    match pos {
        Some(i) => cfg.providers[i] = rec.clone(),
        None => cfg.providers.push(rec.clone()),
    }
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    if !had_models {
        let id_for_fetch = req.id.clone();
        let app_for_fetch = app.clone();
        tokio::spawn(async move {
            let _ = fetch_models_for_provider(&app_for_fetch, &id_for_fetch).await;
        });
    }
    Json(payload(&app, &cfg)).into_response()
}

pub async fn delete(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str().filter(|s| !s.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let mut cfg = app.config.lock().await;
    cfg.providers.retain(|p| p.id != id);
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    Json(payload(&app, &cfg)).into_response()
}

pub async fn reveal_key(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let cfg = app.config.lock().await;
    match cfg
        .providers
        .iter()
        .find(|p| p.id == body["id"].as_str().unwrap_or(""))
    {
        Some(p) => Json(json!({ "key": p.key })).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such provider"),
    }
}

pub async fn fetch_models_for_provider(app: &Arc<App>, id: &str) -> anyhow::Result<usize> {
    let (base, key) = {
        let cfg = app.config.lock().await;
        let p = cfg
            .providers
            .iter()
            .find(|p| p.id == id)
            .context("no such provider")?;
        let base = [p.models_url.as_str(), p.chat.as_str()]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(|s| format!("{}/models", s.trim_end_matches('/')))
            .unwrap_or_default();
        (base, p.key.clone())
    };
    if base.is_empty() {
        anyhow::bail!("this provider has no URL to list models from");
    }
    let mut req = app.client().get(&base).timeout(Duration::from_secs(5));
    if !key.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        anyhow::bail!("the vendor answered {}", res.status());
    }
    let text = res.text().await.unwrap_or_default();
    let list: Value = serde_json::from_str(&text).context("the vendor's model list is not JSON")?;
    let ids: Vec<String> = list
        .get("data")
        .or_else(|| list.get("models"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m["id"]
                        .as_str()
                        .or_else(|| m["name"].as_str())
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        anyhow::bail!("the vendor listed no models");
    }
    let count = ids.len();
    let mut cfg = app.config.lock().await;
    if let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) {
        p.models = merge_models(&p.models, &ids);
        p.fetched = Some(stamp());
    }
    let _ = persist(&app.config_path, &cfg);
    Ok(count)
}

/// a short local "when", for the editor's fetched-list hint
fn stamp() -> String {
    jiff::Zoned::now().strftime("%Y-%m-%d %H:%M").to_string()
}

/// A model's display name, saved apart from the editor's Save: agents see it
/// at once, and it changes nothing on the wire.
pub async fn name(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let Some(model) = body["model"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which model?");
    };
    let display = body["modelName"].as_str().unwrap_or("").trim().to_owned();
    let mut cfg = app.config.lock().await;
    let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) else {
        return err(StatusCode::NOT_FOUND, "no such provider");
    };
    if p.models.iter().all(|m| m.id != model) {
        p.models.push(Model {
            id: model.to_owned(),
            on: true,
            name: None,
            extra: Map::new(),
        });
    }
    let m = p.models.iter_mut().find(|m| m.id == model).unwrap();
    m.name = (!display.is_empty()).then_some(display);
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    Json(payload(&app, &cfg)).into_response()
}

/// The reasoning levels agents are offered for a model; none stored means
/// every level it has.
pub async fn efforts(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let Some(model) = body["model"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which model?");
    };
    let kept: Vec<String> = body["efforts"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut cfg = app.config.lock().await;
    let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) else {
        return err(StatusCode::NOT_FOUND, "no such provider");
    };
    let Some(m) = p.models.iter_mut().find(|m| m.id == model) else {
        return err(StatusCode::NOT_FOUND, "no such model");
    };
    if kept.is_empty() {
        m.extra.remove("kept");
    } else {
        m.extra.insert("kept".to_owned(), json!(kept));
    }
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    Json(payload(&app, &cfg)).into_response()
}

/// Drop the list fetched from the vendor: the models.dev one stands in until
/// the next Refresh asks.
pub async fn unfetch(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let mut cfg = app.config.lock().await;
    let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) else {
        return err(StatusCode::NOT_FOUND, "no such provider");
    };
    p.fetched = None;
    p.models.clear();
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    Json(payload(&app, &cfg)).into_response()
}

/// The vendor's own model list, stored as the provider's models.
pub async fn fetch_models(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str().map(str::to_owned) else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    match fetch_models_for_provider(&app, &id).await {
        Ok(count) => (StatusCode::OK, Json(json!({ "count": count }))).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// Tiny requests: one per API the provider speaks, or one per model when
/// `test` carries model ids. A vendor that answers can still have a model
/// that doesn't.
pub async fn test(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str().map(str::to_owned) else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let provider = {
        let cfg = app.config.lock().await;
        cfg.providers.iter().find(|p| p.id == id).cloned()
    };
    let Some(p) = provider else {
        return err(StatusCode::NOT_FOUND, "no such provider");
    };
    let per_model: Vec<String> = body["test"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let models: Vec<String> = if per_model.is_empty() {
        vec![p
            .models
            .iter()
            .find(|m| m.on)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "test".to_owned())]
    } else {
        per_model
    };
    if p.chat.is_empty() {
        return err(StatusCode::BAD_REQUEST, "this provider has no URL");
    }
    let mut results = Vec::new();
    for model in &models {
        results.push(tiny_request(&app.client(), &p.chat, &p.key, model).await);
    }
    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

async fn tiny_request(client: &reqwest::Client, base: &str, key: &str, model: &str) -> Value {
    let t0 = Instant::now();
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let body =
        json!({ "model": model, "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }] })
            .to_string();
    let mut req = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    if !key.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    match req.send().await {
        Ok(res) => {
            let status = res.status();
            let mut out = json!({ "ok": status.is_success(), "ms": t0.elapsed().as_millis() as u64, "status": status.as_u16(), "model": model });
            if !status.is_success() {
                let why = res.text().await.unwrap_or_default();
                out["error"] = json!(why.trim().chars().take(120).collect::<String>());
            }
            out
        }
        Err(e) => json!({ "ok": false, "error": e.to_string(), "model": model }),
    }
}

/// All picked models on, kept ones off, new ones appended; display names
/// already given survive a refresh.
fn merge_models(existing: &[Model], on: &[String]) -> Vec<Model> {
    let mut out = existing.to_vec();
    for m in &mut out {
        m.on = false;
    }
    for id in on {
        match out.iter_mut().find(|m| &m.id == id) {
            Some(m) => m.on = true,
            None => out.push(Model {
                id: id.clone(),
                on: true,
                name: None,
                extra: Map::new(),
            }),
        }
    }
    out
}

// ---------- where a pass-through request goes ----------

pub struct Target {
    pub url: String,
    pub key: String,
    pub headers: Map<String, Value>,
}

/// The hinted provider (by id or name, case-insensitive) takes the request,
/// else the first one with a chat URL. The path rides as-is, minus a single
/// leading `/v1` (the version sits in the base URL too).
pub fn route_for(
    cfg: &ConfigState,
    provider_hint: Option<&str>,
    path_and_query: &str,
) -> Option<Target> {
    let to_target = |p: &Provider| -> Option<Target> {
        let base = p.chat.trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        Some(Target {
            url: format!("{base}{}", after_v1(path_and_query)),
            key: p.key.clone(),
            headers: p.headers.clone(),
        })
    };
    provider_hint
        .and_then(|hint| {
            cfg.providers
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(hint) || p.name.eq_ignore_ascii_case(hint))
        })
        .or_else(|| cfg.providers.first())
        .and_then(to_target)
}
