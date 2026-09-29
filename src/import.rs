//! Providers set up in other apps, for the Import dialog to bring over.
//! magpie reads those apps' settings and changes nothing in them: each
//! source's providers are offered with their key masked, and only the ones
//! the user ticks are added here.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{App, providers};

/// the sources magpie knows to look for; one that isn't on this computer
/// shows as such, so which ones can be imported stays plain
fn config_path(id: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .ok()?;
    Some(match id {
        "cc-switch" => home.join(".cc-switch").join("config.json"),
        _ => return None,
    })
}

fn sources_meta() -> Vec<(&'static str, &'static str)> {
    [("cc-switch", "CC Switch")].to_vec()
}

/// the first string under a key of this name, at any depth
fn find_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v {
        Value::Object(o) => o
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| o.values().find_map(|v| find_str(v, key))),
        Value::Array(a) => a.iter().find_map(|v| find_str(v, key)),
        _ => None,
    }
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
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

fn slug(s: &str) -> String {
    let out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "provider".to_owned()
    } else {
        trimmed
    }
}

/// the provider entries a config carries, whatever its exact shape: a
/// `providers` object keyed by id, or a list — the file may wrap its apps'
/// sections (`{"claude": …}`) one level down, so those are looked in too
fn entries(config: &Value) -> Vec<(String, &Value)> {
    fn scan<'a>(v: &'a Value, out: &mut Vec<(String, &'a Value)>) {
        let Some(p) = v.get("providers") else {
            return;
        };
        if let Some(map) = p.as_object() {
            for (k, e) in map {
                if e.is_object() {
                    out.push((k.clone(), e));
                }
            }
        } else if let Some(list) = p.as_array() {
            for (i, e) in list.iter().enumerate() {
                if e.is_object() {
                    out.push((i.to_string(), e));
                }
            }
        }
    }
    let mut out = Vec::new();
    scan(config, &mut out);
    if out.is_empty()
        && let Some(o) = config.as_object()
    {
        for v in o.values() {
            scan(v, &mut out);
        }
    }
    out
}

/// what an entry says, read off the keys the known apps use — at any depth,
/// so a nesting the file grows later still lands
fn extract(e: &Value, r: &str) -> (String, String, String, String) {
    let name = find_str(e, "name").unwrap_or(r).trim().to_owned();
    let anthropic = find_str(e, "ANTHROPIC_BASE_URL")
        .unwrap_or_default()
        .to_owned();
    let chat = find_str(e, "OPENAI_BASE_URL")
        .or_else(|| find_str(e, "baseUrl"))
        .or_else(|| find_str(e, "base_url"))
        .unwrap_or_default()
        .to_owned();
    let key = [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "apiKey",
        "api_key",
    ]
    .iter()
    .find_map(|k| find_str(e, k))
    .unwrap_or_default()
    .to_owned();
    (name, anthropic, chat, key)
}

/// one source as the dialog reads it: its path, whether it is here, and the
/// providers in it that can come over
pub async fn sources(State(app): State<Arc<App>>) -> Response {
    let cfg = app.config.lock().await;
    let out: Vec<Value> = sources_meta()
        .into_iter()
        .map(|(id, name)| {
            let path = config_path(id).unwrap_or_default();
            let (found, error, items) = match std::fs::read_to_string(&path)
                .ok()
                .map(|t| serde_json::from_str::<Value>(&t))
            {
                Some(Ok(v)) => (true, String::new(), items(&v, &cfg.providers)),
                Some(Err(e)) => (true, e.to_string(), Vec::new()),
                None => (false, String::new(), Vec::new()),
            };
            json!({
                "id": id,
                "name": name,
                "path": path.to_string_lossy(),
                "found": found,
                "error": error,
                "items": items,
            })
        })
        .collect();
    drop(cfg);
    Json(out).into_response()
}

/// the providers a source's config carries, with their key masked and their
/// standing against what magpie has already
fn items(config: &Value, existing: &[providers::Provider]) -> Vec<Value> {
    entries(config)
        .into_iter()
        .filter_map(|(r, e)| {
            let (name, anthropic, chat, key) = extract(e, &r);
            if anthropic.is_empty() && chat.is_empty() && key.is_empty() {
                return None;
            }
            let host = host_of(if anthropic.is_empty() {
                &chat
            } else {
                &anthropic
            });
            let same = existing.iter().find(|p| {
                !host.is_empty()
                    && host_of(p.base_url()) == host
                    && !p.key.is_empty()
                    && p.key == key
            });
            let taken = existing
                .iter()
                .find(|p| p.name == name || p.id == slug(&name));
            let mut item = json!({
                "ref": r,
                "provider": {
                    "name": name,
                    "key": mask(&key),
                    "anthropic": anthropic,
                    "chat": chat,
                    "models": [],
                },
            });
            if let Some(p) = same {
                item["status"] = json!("same");
                item["existing"] = json!(p.name);
            } else if let Some(p) = taken {
                item["status"] = json!("taken");
                item["existing"] = json!(p.name);
            } else if existing
                .iter()
                .any(|p| !host.is_empty() && host_of(p.base_url()) == host)
            {
                item["keyOf"] = json!(host);
            }
            Some(item)
        })
        .collect()
}

/// the ticks, made real: every picked provider lands in the config
pub async fn run(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let Some(picks) = body["picks"].as_array() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nothing picked" })),
        )
            .into_response();
    };
    let mut cfg = app.config.lock().await;
    let mut added = Vec::new();
    for pick in picks {
        let (Some(source), Some(r)) = (
            pick["source"].as_str(),
            pick["ref"].as_str().map(str::to_owned),
        ) else {
            continue;
        };
        let Some(path) = config_path(source) else {
            continue;
        };
        let Ok(Ok(config)) = std::fs::read_to_string(&path).map(|t| serde_json::from_str(&t))
        else {
            continue;
        };
        // the entry itself, not the dialog's masked copy: the key is real here
        let Some((_, e)) = entries(&config).into_iter().find(|(k, _)| *k == r) else {
            continue;
        };
        let (name, anthropic, chat, key) = extract(e, &r);
        if name.is_empty() {
            continue;
        }
        // an id of its own, numbered when the name is taken
        let base = slug(&name);
        let mut id = base.clone();
        let mut n = 1;
        while cfg.providers.iter().any(|p| p.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        added.push(name.clone());
        cfg.providers.push(providers::Provider {
            id,
            name,
            key,
            chat,
            anthropic,
            ..Default::default()
        });
    }
    if crate::persist(&app.config_path, &cfg).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "could not save the config" })),
        )
            .into_response();
    }
    Json(json!({
        "state": providers::payload(&app, &cfg),
        "added": added,
    }))
    .into_response()
}
