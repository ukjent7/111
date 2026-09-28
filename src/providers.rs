//! The Providers tab's backend: presets to add from, the CRUD the editor
//! speaks, model-list fetches and endpoint tests — and the decision of where
//! a passing-through request goes, per protocol, among the configured
//! providers.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::{persist, App, ConfigState};

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
            *app.catalog.lock().unwrap() = v;
            Json(json!({ "agents": [], "profiles": [], "settings": {} })).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("models.dev didn't answer: {e}")),
    }
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

pub fn payload(app: &App, cfg: &ConfigState) -> Value {
    let catalog = app.catalog.lock().unwrap();
    let providers: Vec<Value> = cfg.providers.iter().map(|p| enrich(p, &catalog)).collect();
    let models: usize = providers
        .iter()
        .map(|p| p["models"].as_array().map(|m| m.iter().filter(|m| m["on"] == json!(true)).count()).unwrap_or(0))
        .sum();
    let calls: Vec<Value> = app.gateway.calls.lock().unwrap().iter().rev().cloned().collect();
    json!({
        "providers": providers,
        "presets": presets(&catalog),
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
fn enrich(p: &Value, catalog: &Value) -> Value {
    let mut out = p.clone();
    let key = p["key"].as_str().unwrap_or("");
    out["key"] = json!({ "set": !key.is_empty(), "masked": mask(key) });
    let host = ["chat", "responses", "anthropic"]
        .iter()
        .find_map(|f| p[f].as_str())
        .unwrap_or("");
    out["host"] = json!(host_of(host));
    let local = matches!(out["host"].as_str().unwrap_or(""), "127.0.0.1" | "localhost" | "[::1]" | "0.0.0.0");
    out["ready"] = json!(key.is_empty() && local);
    out["agents"] = json!([]);
    out["models"] = normalize_models(&p["models"]);
    let names = p["catalog"]
        .as_str()
        .and_then(|id| catalog.get(id))
        .and_then(|c| c.get("models"))
        .and_then(Value::as_object);
    if let (Some(names), Some(models)) = (names, out["models"].as_array_mut()) {
        for m in models {
            if let Some(id) = m["id"].as_str() {
                if m["name"].as_str().is_none() {
                    if let Some(name) = names.get(id).and_then(|e| e["name"].as_str()) {
                        m["name"] = json!(name);
                    }
                }
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
        n => format!("{}…{}", chars[..4].iter().collect::<String>(), chars[n - 4..].iter().collect::<String>()),
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

/// models.dev ids whose logos this app ships; the rest show the generic mark.
fn icon_for(id: &str) -> Option<&'static str> {
    Some(match id {
        "openai" => "openai",
        "anthropic" => "anthropic",
        "deepseek" => "deepseek-color",
        "moonshotai" | "moonshotai-cn" | "kimi-code-plan-cn" | "kimi-code-plan-global" => "moonshot",
        "zhipuai" | "zhipuai-coding-plan" => "zhipu-color",
        "zai" | "zai-coding-plan" => "zai",
        "siliconflow" | "siliconflow-cn" => "siliconcloud-color",
        "openrouter" => "openrouter",
        "302ai" => "ai302-color",
        "aihubmix" => "aihubmix-color",
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => "minimax-color",
        "stepfun" | "stepfun-ai" | "stepfun-step-plan" | "stepfun-ai-step-plan" => "stepfun-color",
        "groq" => "groq",
        "mistral" => "mistral-color",
        "xai" => "xai",
        "google" => "gemini-color",
        "fireworks-ai" => "fireworks-color",
        "ollama" | "ollama-cloud" => "ollama",
        "lmstudio" => "lmstudio",
        "github-copilot" => "githubcopilot",
        "alibaba" | "alibaba-cn" | "alibaba-coding-plan" | "alibaba-coding-plan-cn" | "alibaba-token-plan" | "alibaba-token-plan-cn" => "qwen-color",
        "cloudflare-workers-ai" => "cloudflare-color",
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
    let mut out = json!({
        "id": id,
        "name": p["name"].as_str().unwrap_or(id),
        "kind": "vendor",
        "chat": url,
        "catalog": id,
    });
    if let Some(icon) = icon_for(id) {
        out["icon"] = json!(icon);
    }
    Some(out)
}

// ---------- the editor's endpoints ----------

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

pub async fn save(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str().map(str::to_owned).filter(|s| !s.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "the provider needs an id");
    };
    let mut cfg = app.config.lock().await;
    // an edit arrives with `from` (its id before a rename); a partial save
    // only touches the fields it carries, so a models toggle keeps the key
    let pos = cfg
        .providers
        .iter()
        .position(|p| p["id"].as_str() == Some(body["from"].as_str().unwrap_or(&id)));
    let mut rec = pos.and_then(|i| cfg.providers.get(i).cloned()).unwrap_or_else(|| json!({}));
    rec["id"] = json!(id);
    if let Some(v) = body["name"].as_str() {
        rec["name"] = json!(v);
    }
    if rec["name"].as_str().unwrap_or("").is_empty() {
        rec["name"] = rec["id"].clone();
    }
    for f in ["preset", "api", "chat", "responses", "anthropic", "catalog", "icon", "balanceURL", "balancePath", "modelsURL"] {
        if let Some(v) = body[f].as_str() {
            rec[f] = json!(v);
        }
    }
    if rec["icon"].as_str().unwrap_or("").is_empty() {
        rec["icon"] = json!("generic");
    }
    if let Some(v) = body["key"].as_str() {
        rec["key"] = json!(v);
    }
    if let Some(v) = body["headers"].as_object() {
        rec["headers"] = json!(v);
    }
    if let Some(v) = body["unlisted"].as_bool() {
        rec["unlisted"] = json!(v);
    }
    if let Some(v) = body["fallback"].as_array() {
        rec["fallback"] = json!(v);
    }
    if let Some(v) = body["contexts"].as_object() {
        rec["contexts"] = json!(v);
    }
    let chosen: Vec<String> = body["models"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if body["models"].is_array() {
        rec["models"] = merge_models(rec["models"].as_array(), &chosen);
    }
    match pos {
        Some(i) => cfg.providers[i] = rec,
        None => cfg.providers.push(rec),
    }
    if let Err(e) = persist(&app.config_path, &cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save the config: {e}"));
    }
    Json(payload(&app, &cfg)).into_response()
}

pub async fn delete(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(id) = body["id"].as_str() else {
        return err(StatusCode::BAD_REQUEST, "which provider?");
    };
    let mut cfg = app.config.lock().await;
    cfg.providers.retain(|p| p["id"].as_str() != Some(id));
    if let Err(e) = persist(&app.config_path, &cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save the config: {e}"));
    }
    Json(payload(&app, &cfg)).into_response()
}

pub async fn reveal_key(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let cfg = app.config.lock().await;
    match cfg.providers.iter().find(|p| p["id"].as_str() == Some(body["id"].as_str().unwrap_or(""))) {
        Some(p) => Json(json!({ "key": p["key"].as_str().unwrap_or("") })).into_response(),
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
        let Some(p) = cfg.providers.iter().find(|p| p["id"].as_str() == Some(&id)) else {
            return err(StatusCode::NOT_FOUND, "no such provider");
        };
        let base = ["modelsURL", "chat", "responses"]
            .iter()
            .find_map(|f| p[f].as_str())
            .filter(|s| !s.is_empty())
            .map(|s| format!("{}/models", s.trim_end_matches('/')))
            .or_else(|| {
                p["anthropic"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{}/v1/models", s.trim_end_matches('/')))
            })
            .unwrap_or_default();
        (base, p["key"].as_str().unwrap_or("").to_owned(), p["anthropic"].as_str().is_some_and(|s| !s.is_empty()))
    };
    if base.is_empty() {
        return err(StatusCode::BAD_REQUEST, "this provider has no URL to list models from");
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
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("the vendor didn't answer: {e}")),
    };
    if !res.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, &format!("the vendor answered {}", res.status()));
    }
    let text = res.text().await.unwrap_or_default();
    let list: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_GATEWAY, "the vendor's model list is not JSON"),
    };
    let ids: Vec<String> = list
        .get("data")
        .or_else(|| list.get("models"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "the vendor listed no models");
    }
    {
        let mut cfg = app.config.lock().await;
        if let Some(p) = cfg.providers.iter_mut().find(|p| p["id"].as_str() == Some(&id)) {
            p["models"] = merge_models(p["models"].as_array(), &ids);
        }
        if let Err(e) = persist(&app.config_path, &cfg) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save the config: {e}"));
        }
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
        cfg.providers.iter().find(|p| p["id"].as_str() == Some(&id)).cloned()
    };
    let Some(p) = provider else {
        return err(StatusCode::NOT_FOUND, "no such provider");
    };
    let per_model: Vec<String> = body["test"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let results: Vec<Value> = if per_model.is_empty() {
        let fallback = p["models"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["on"] == json!(true)))
            .and_then(|m| m["id"].as_str())
            .unwrap_or("test")
            .to_owned();
        let mut out = Vec::new();
        for (name, field, proto) in
            [("chat", "chat", Proto::Chat), ("responses", "responses", Proto::Responses), ("anthropic", "anthropic", Proto::Anthropic)]
        {
            match p[field].as_str().filter(|s| !s.is_empty()) {
                None => out.push(json!({ "protocol": name, "ok": false, "error": "no URL for this API" })),
                Some(base) => {
                    let mut r = tiny_request(&app.client, proto, base, p["key"].as_str().unwrap_or(""), &fallback).await;
                    r["protocol"] = json!(name);
                    out.push(r);
                }
            }
        }
        out
    } else {
        let proto = api_proto(&p);
        let Some(base) = url_for(&p, proto).filter(|s| !s.is_empty()) else {
            return err(StatusCode::BAD_REQUEST, "this provider has no URL for its own API");
        };
        let key = p["key"].as_str().unwrap_or("");
        let mut out = Vec::new();
        for model in &per_model {
            out.push(tiny_request(&app.client, proto, base, key, model).await);
        }
        out
    };
    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

#[derive(Clone, Copy)]
enum Proto {
    Chat,
    Responses,
    Anthropic,
}

fn api_proto(p: &Value) -> Proto {
    match p["api"].as_str() {
        Some("responses") => Proto::Responses,
        Some("anthropic") => Proto::Anthropic,
        _ => {
            if p["anthropic"].as_str().is_some_and(|s| !s.is_empty()) && p["chat"].as_str().is_none_or(|s| s.is_empty()) {
                Proto::Anthropic
            } else {
                Proto::Chat
            }
        }
    }
}

fn url_for(p: &Value, proto: Proto) -> Option<&str> {
    let field = match proto {
        Proto::Chat => "chat",
        Proto::Responses => "responses",
        Proto::Anthropic => "anthropic",
    };
    p[field].as_str().filter(|s| !s.is_empty())
}

async fn tiny_request(client: &reqwest::Client, proto: Proto, base: &str, key: &str, model: &str) -> Value {
    let t0 = Instant::now();
    let url = match proto {
        Proto::Anthropic => format!("{}/v1/messages", base.trim_end_matches('/')),
        Proto::Chat => format!("{}/chat/completions", base.trim_end_matches('/')),
        Proto::Responses => format!("{}/responses", base.trim_end_matches('/')),
    };
    let body = match proto {
        Proto::Anthropic | Proto::Chat => {
            json!({ "model": model, "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }] }).to_string()
        }
        Proto::Responses => json!({ "model": model, "input": "hi", "max_output_tokens": 1 }).to_string(),
    };
    let mut req = client.post(&url).header(header::CONTENT_TYPE, "application/json").body(body);
    if !key.is_empty() {
        req = match proto {
            Proto::Anthropic => req.header("x-api-key", key).header("anthropic-version", "2023-06-01"),
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

/// Models may be stored as bare id strings (a hand-edited config); make them
/// objects without touching their on flags.
fn normalize_models(models: &Value) -> Value {
    match models.as_array() {
        Some(arr) => Value::Array(
            arr.iter()
                .map(|m| {
                    if let Some(id) = m.as_str() {
                        json!({ "id": id, "on": true })
                    } else {
                        m.clone()
                    }
                })
                .collect(),
        ),
        None => json!([]),
    }
}

/// All picked models on, kept ones off, new ones appended; display names
/// already given survive a refresh.
fn merge_models(existing: Option<&Vec<Value>>, on: &[String]) -> Value {
    let mut out: Vec<Value> = existing.cloned().unwrap_or_default();
    for m in out.iter_mut() {
        m["on"] = json!(false);
    }
    for id in on {
        match out.iter_mut().find(|m| m["id"].as_str() == Some(id.as_str())) {
            Some(m) => m["on"] = json!(true),
            None => out.push(json!({ "id": id, "on": true })),
        }
    }
    json!(out)
}

// ---------- where a pass-through request goes ----------

pub struct Target {
    pub url: String,
    pub key: String,
    pub extra: Value,
    pub anthropic: bool,
}

/// The request path decides the protocol; the first provider that serves it
/// wins. OpenAI-family base URLs carry their own `/v1`, so the prefix comes
/// off; Anthropic's is the root, so the path stays.
pub fn route_for(cfg: &ConfigState, path_and_query: &str) -> Option<Target> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let (field, sub) = if path.starts_with("/v1/messages") || path.starts_with("/v1/complete") {
        ("anthropic", path.to_owned())
    } else if path.contains("/responses") {
        ("responses", path.strip_prefix("/v1").unwrap_or(path).to_owned())
    } else {
        ("chat", path.strip_prefix("/v1").unwrap_or(path).to_owned())
    };
    let sub = if sub.is_empty() { "/".to_owned() } else { sub };
    for p in &cfg.providers {
        let Some(base) = p[field].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let mut url = format!("{}{}", base.trim_end_matches('/'), sub);
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        return Some(Target {
            url,
            key: p["key"].as_str().unwrap_or("").to_owned(),
            extra: p["headers"].clone(),
            anthropic: field == "anthropic",
        });
    }
    None
}
