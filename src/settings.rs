//! Settings, usage and session endpoints for the shell UI.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{App, ConfigState, persist};

pub fn get_settings(app: &App, cfg: &ConfigState) -> Value {
    let saved = cfg.settings.as_object();
    let get_str = |key: &str, def: &str| -> String {
        saved
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or(def)
            .to_string()
    };
    let get_bool = |key: &str, def: bool| -> bool {
        saved
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_bool())
            .unwrap_or(def)
    };
    let get_u64 = |key: &str, def: u64| -> u64 {
        saved
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(def)
    };

    let dir = app
        .config_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    json!({
        "theme": get_str("theme", "system"),
        "lang": get_str("lang", "zh-CN"),
        "tray": get_str("tray", "panel"),
        "dock": get_bool("dock", false),
        "dockWindow": get_bool("dockWindow", false),
        "login": get_bool("login", false),
        "quotaLeft": get_bool("quotaLeft", false),
        "proxy": get_str("proxy", ""),
        "proxyNow": "",
        "proxySource": "none",
        "codexWarmup": get_str("codexWarmup", "off"),
        "codexWarmed": Value::Null,
        "claudeWarmup": get_str("claudeWarmup", "off"),
        "claudeWarmed": Value::Null,
        "redact": get_bool("redact", false),
        "redactPersonal": get_bool("redactPersonal", false),
        "redactWords": saved.and_then(|m| m.get("redactWords")).cloned().unwrap_or_else(|| json!([])),
        "noStats": get_bool("noStats", false),
        "lan": get_bool("lan", false),
        "lanURLs": [],
        "lanKey": "",
        "trayUsage": get_str("trayUsage", ""),
        "trayUsageEvery": get_u64("trayUsageEvery", 3),
        "version": env!("CARGO_PKG_VERSION"),
        "dir": dir,
        "gateway": app.gateway.url,
    })
}

pub async fn get(State(app): State<Arc<App>>) -> Response {
    let cfg = app.config.lock().await;
    Json(get_settings(&app, &cfg)).into_response()
}

pub async fn save(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let mut cfg = app.config.lock().await;
    if let Some(map) = body.as_object() {
        if !cfg.settings.is_object() {
            cfg.settings = Value::Object(serde_json::Map::new());
        }
        if let Some(cur) = cfg.settings.as_object_mut() {
            for (k, v) in map {
                cur.insert(k.clone(), v.clone());
            }
        }
        let _ = persist(&app.config_path, &cfg);
    }
    Json(get_settings(&app, &cfg)).into_response()
}

pub async fn set_quota_left(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let mut cfg = app.config.lock().await;
    let on = body.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
    if !cfg.settings.is_object() {
        cfg.settings = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = cfg.settings.as_object_mut() {
        map.insert("quotaLeft".to_string(), json!(on));
    }
    let _ = persist(&app.config_path, &cfg);
    Json(get_settings(&app, &cfg)).into_response()
}

pub async fn set_login(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let mut cfg = app.config.lock().await;
    let on = body.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
    if !cfg.settings.is_object() {
        cfg.settings = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = cfg.settings.as_object_mut() {
        map.insert("login".to_string(), json!(on));
    }
    let _ = persist(&app.config_path, &cfg);
    Json(get_settings(&app, &cfg)).into_response()
}

pub async fn set_lan(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let mut cfg = app.config.lock().await;
    let on = body.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
    if !cfg.settings.is_object() {
        cfg.settings = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = cfg.settings.as_object_mut() {
        map.insert("lan".to_string(), json!(on));
    }
    let _ = persist(&app.config_path, &cfg);
    Json(get_settings(&app, &cfg)).into_response()
}

pub async fn reveal(State(app): State<Arc<App>>) -> Response {
    if let Some(dir) = app.config_path.parent() {
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(dir).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn open(Json(body): Json<Value>) -> Response {
    if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
        let url = url.trim();
        if url.starts_with("http://") || url.starts_with("https://") {
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", url])
                .spawn();
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(url).spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn usage(State(app): State<Arc<App>>) -> Response {
    let path = app
        .config_path
        .parent()
        .map(|p| p.join("usage.json").to_string_lossy().into_owned())
        .unwrap_or_default();
    let requests = app.gateway.requests.load(Ordering::Relaxed);
    let errors = app.gateway.errors.load(Ordering::Relaxed);

    Json(json!({
        "calls": requests,
        "input": 0,
        "output": 0,
        "cache_read": 0,
        "cache_write": 0,
        "reasoning": 0,
        "errors": errors,
        "cost": 0.0,
        "unpriced": 0,
        "series": [],
        "bucket": "day",
        "agents": [],
        "models": [],
        "path": path,
    }))
    .into_response()
}

pub async fn usage_quotas() -> Response {
    Json(json!([])).into_response()
}

pub async fn sessions() -> Response {
    Json(json!({
        "sessions": [],
        "terminal": "",
        "dirs": [],
    }))
    .into_response()
}

pub async fn sessions_stats() -> Response {
    Json(json!({
        "from": "",
        "to": "",
        "days": [],
        "agents": [],
    }))
    .into_response()
}

pub async fn davsync() -> Response {
    Json(json!({ "on": false })).into_response()
}

pub async fn drift() -> Response {
    Json(json!({})).into_response()
}
