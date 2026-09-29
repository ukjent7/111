//! The Providers tab's backend: presets to add from, the CRUD the editor
//! speaks, model-list fetches and endpoint tests — and the decision of where
//! a passing-through request goes, per protocol, among the configured
//! providers.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
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
    match fetch_catalog(&app.client).await {
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

    let fetched = fetch_and_persist_logo(&app.client, &app.logos_dir, &id).await;
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
    let svg_path = logos_dir.join(format!("{id}.svg"));
    if let Ok(bytes) = std::fs::read(&svg_path) {
        return Some((bytes, "image/svg+xml".to_owned()));
    }
    let png_path = logos_dir.join(format!("{id}.png"));
    if let Ok(bytes) = std::fs::read(&png_path) {
        return Some((bytes, "image/png".to_owned()));
    }
    None
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
        .map(|p| p.models.iter().filter(|m| m.on).count())
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
/// names come from the models.dev catalog when it knows the model.
fn enrich(p: &Provider, catalog: &Value) -> Value {
    let mut out = serde_json::to_value(p).unwrap_or_default();
    let host = host_of(p.base_url());
    let local = matches!(
        host.as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "0.0.0.0"
    );
    out["key"] = json!({ "set": !p.key.is_empty(), "masked": mask(&p.key) });
    out["host"] = json!(host);
    out["ready"] = json!(p.key.is_empty() && local);
    out["agents"] = json!([]);
    let names = catalog
        .get(p.catalog.as_str())
        .and_then(|c| c.get("models"))
        .and_then(Value::as_object);
    if let (Some(names), Some(models)) = (names, out["models"].as_array_mut()) {
        for m in models {
            if let Some(id) = m["id"].as_str()
                && m["name"].as_str().is_none()
                && let Some(name) = names.get(id).and_then(|e| e["name"].as_str())
            {
                m["name"] = json!(name);
            }
        }
    }
    out
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
        "anthropic" => "https://api.anthropic.com",
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
    Some(json!({
        "id": id,
        "name": p["name"].as_str().unwrap_or(id),
        "kind": "vendor",
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
    pub api: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub responses: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anthropic: String,
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

/// The protocol a request or a provider speaks.
#[derive(Clone, Copy, PartialEq)]
enum Api {
    Chat,
    Responses,
    Anthropic,
}

impl Provider {
    fn url(&self, api: Api) -> &str {
        match api {
            Api::Chat => &self.chat,
            Api::Responses => &self.responses,
            Api::Anthropic => &self.anthropic,
        }
    }

    /// The URL an API is actually reachable on.
    fn url_set(&self, api: Api) -> Option<&str> {
        Some(self.url(api)).filter(|s| !s.is_empty())
    }

    /// The first URL the provider declares, in chat → responses → anthropic
    /// order; the host shown in the list row is picked from it.
    fn base_url(&self) -> &str {
        [Api::Chat, Api::Responses, Api::Anthropic]
            .into_iter()
            .find_map(|api| self.url_set(api))
            .unwrap_or("")
    }
}

fn api_proto(p: &Provider) -> Api {
    match p.api.as_str() {
        "responses" => Api::Responses,
        "anthropic" => Api::Anthropic,
        _ if !p.anthropic.is_empty() && p.chat.is_empty() => Api::Anthropic,
        _ => Api::Chat,
    }
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
    api: Option<String>,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    responses: Option<String>,
    #[serde(default)]
    anthropic: Option<String>,
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
    if let Some(v) = req.api {
        rec.api = v;
    }
    if let Some(v) = req.chat {
        rec.chat = v;
    }
    if let Some(v) = req.responses {
        rec.responses = v;
    }
    if let Some(v) = req.anthropic {
        rec.anthropic = v;
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
    if let Some(on) = req.models {
        rec.models = merge_models(&rec.models, &on);
    }
    match pos {
        Some(i) => cfg.providers[i] = rec,
        None => cfg.providers.push(rec),
    }
    if let Some(res) = persisted(&app, &cfg) {
        return res;
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

/// The vendor's own model list, stored as the provider's models.
pub async fn fetch_models(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str().map(str::to_owned) else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let (base, key, anthropic) = {
        let cfg = app.config.lock().await;
        let Some(p) = cfg.providers.iter().find(|p| p.id == id) else {
            return err(StatusCode::NOT_FOUND, "no such provider");
        };
        let base = [p.models_url.as_str(), p.chat.as_str(), p.responses.as_str()]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(|s| format!("{}/models", s.trim_end_matches('/')))
            .or_else(|| {
                (!p.anthropic.is_empty())
                    .then(|| format!("{}/v1/models", p.anthropic.trim_end_matches('/')))
            })
            .unwrap_or_default();
        (base, p.key.clone(), !p.anthropic.is_empty())
    };
    if base.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "this provider has no URL to list models from",
        );
    }
    let mut req = app.client.get(&base);
    if anthropic {
        if !key.is_empty() {
            req = req.header("x-api-key", &key);
        }
        req = req.header("anthropic-version", "2023-06-01");
    } else if !key.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let res = match req.send().await {
        Ok(res) => res,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                &format!("the vendor didn't answer: {e}"),
            );
        }
    };
    if !res.status().is_success() {
        return err(
            StatusCode::BAD_GATEWAY,
            &format!("the vendor answered {}", res.status()),
        );
    }
    let text = res.text().await.unwrap_or_default();
    let list: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "the vendor's model list is not JSON",
            );
        }
    };
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
        return err(StatusCode::BAD_GATEWAY, "the vendor listed no models");
    }
    let mut cfg = app.config.lock().await;
    if let Some(p) = cfg.providers.iter_mut().find(|p| p.id == id) {
        p.models = merge_models(&p.models, &ids);
    }
    if let Some(res) = persisted(&app, &cfg) {
        return res;
    }
    (StatusCode::OK, Json(json!({ "count": ids.len() }))).into_response()
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
    let results: Vec<Value> = if per_model.is_empty() {
        let fallback = p
            .models
            .iter()
            .find(|m| m.on)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "test".to_owned());
        let mut out = Vec::new();
        for (name, api) in [
            ("chat", Api::Chat),
            ("responses", Api::Responses),
            ("anthropic", Api::Anthropic),
        ] {
            match p.url_set(api) {
                None => out
                    .push(json!({ "protocol": name, "ok": false, "error": "no URL for this API" })),
                Some(base) => {
                    let mut r = tiny_request(&app.client, api, base, &p.key, &fallback).await;
                    r["protocol"] = json!(name);
                    out.push(r);
                }
            }
        }
        out
    } else {
        let proto = api_proto(&p);
        let Some(base) = p.url_set(proto) else {
            return err(
                StatusCode::BAD_REQUEST,
                "this provider has no URL for its own API",
            );
        };
        let mut out = Vec::new();
        for model in &per_model {
            out.push(tiny_request(&app.client, proto, base, &p.key, model).await);
        }
        out
    };
    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

async fn tiny_request(
    client: &reqwest::Client,
    proto: Api,
    base: &str,
    key: &str,
    model: &str,
) -> Value {
    let t0 = Instant::now();
    let url = match proto {
        Api::Anthropic => format!("{}/v1/messages", base.trim_end_matches('/')),
        Api::Chat => format!("{}/chat/completions", base.trim_end_matches('/')),
        Api::Responses => format!("{}/responses", base.trim_end_matches('/')),
    };
    let body = match proto {
        Api::Anthropic | Api::Chat => {
            json!({ "model": model, "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }] }).to_string()
        }
        Api::Responses => json!({ "model": model, "input": "hi", "max_output_tokens": 1 }).to_string(),
    };
    let mut req = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    if !key.is_empty() {
        req = match proto {
            Api::Anthropic => req
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            _ => req.header(header::AUTHORIZATION, format!("Bearer {key}")),
        };
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
    pub anthropic: bool,
}

/// The request path decides the protocol. If a `provider_hint` is provided,
/// the matching provider (by id or name, case-insensitive) is preferred.
/// Otherwise, the first provider that serves the protocol wins.
pub fn route_for_with_provider(
    cfg: &ConfigState,
    provider_hint: Option<&str>,
    path_and_query: &str,
) -> Option<Target> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let (api, sub) = if path.starts_with("/v1/messages") || path.starts_with("/v1/complete") {
        (Api::Anthropic, path.to_owned())
    } else if path.contains("/responses") {
        (
            Api::Responses,
            path.strip_prefix("/v1").unwrap_or(path).to_owned(),
        )
    } else {
        (
            Api::Chat,
            path.strip_prefix("/v1").unwrap_or(path).to_owned(),
        )
    };
    let sub = if sub.is_empty() { "/".to_owned() } else { sub };

    let to_target = |p: &Provider, allow_fallback: bool| -> Option<Target> {
        let base = if allow_fallback {
            p.url_set(api).unwrap_or_else(|| p.base_url())
        } else {
            p.url(api)
        }
        .trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        let mut url = format!("{base}{sub}");
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        Some(Target {
            url,
            key: p.key.clone(),
            headers: p.headers.clone(),
            anthropic: api == Api::Anthropic,
        })
    };

    if let Some(hint) = provider_hint {
        if let Some(p) = cfg
            .providers
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(hint) || p.name.eq_ignore_ascii_case(hint))
        {
            if let Some(target) = to_target(p, true) {
                return Some(target);
            }
        }
    }

    cfg.providers.iter().find_map(|p| to_target(p, false))
}

pub fn route_for(cfg: &ConfigState, path_and_query: &str) -> Option<Target> {
    route_for_with_provider(cfg, None, path_and_query)
}
