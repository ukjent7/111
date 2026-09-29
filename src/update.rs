//! Self-update against GitHub releases: whether a newer magpie is out, a
//! download into its own file, and a swap of the running binary that
//! restarts into the new one. The release ships the bare binary per
//! platform, so no unpacking is ever needed.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};

use crate::App;

/// where releases are read from; overridable, so a fork can self-host
const DEFAULT_RELEASES: &str = "https://api.github.com/repos/ukjent7/111/releases/latest";

/// the release artifact this build was published as
fn artifact() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("magpie-gateway-windows-x64"),
        ("macos", "aarch64") => Some("magpie-gateway-macos-arm64"),
        ("macos", "x86_64") => Some("magpie-gateway-macos-x64"),
        ("linux", "x86_64") => Some("magpie-gateway-linux-x64"),
        _ => None,
    }
}

fn releases_url() -> String {
    std::env::var("MAGPIE_UPDATE_URL").unwrap_or_else(|_| DEFAULT_RELEASES.to_owned())
}

#[derive(Clone, Default)]
pub struct Snapshot {
    /// idle | checking | latest | available | downloading | ready | error
    pub state: &'static str,
    pub latest: String,
    pub page: String,
    pub asset: String,
    pub done: u64,
    pub total: u64,
    pub error: String,
}

#[derive(Default)]
pub struct Update {
    inner: Mutex<Snapshot>,
}

impl Update {
    fn set(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.inner.lock().unwrap());
    }

    fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().clone()
    }
}

fn status_json(s: &Snapshot) -> Value {
    json!({
        "current": env!("CARGO_PKG_VERSION"),
        "state": s.state,
        "latest": s.latest,
        "url": s.page,
        "done": s.done,
        "total": s.total,
        "error": s.error,
    })
}

/// is `latest` a newer version than `current`? Numbers compare, everything
/// else is ties, so a suffix like `-beta` never looks like an upgrade.
fn newer(latest: &str, current: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (nums(latest), nums(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// ask the releases API what the newest magpie is
async fn ask(client: &reqwest::Client, url: &str) -> Result<(String, String, String), String> {
    let res = client
        .get(url)
        .header(header::USER_AGENT, "magpie-gateway")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("the release page answered {}", res.status()));
    }
    let text = res.text().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or("the release names no version")?
        .trim_start_matches('v')
        .to_owned();
    let page = v["html_url"].as_str().unwrap_or_default().to_owned();
    let asset = artifact()
        .and_then(|a| {
            v["assets"].as_array()?.iter().find_map(|x| {
                (x["name"].as_str() == Some(a))
                    .then(|| x["browser_download_url"].as_str()?.to_owned())
            })
        })
        .unwrap_or_default();
    Ok((tag, page, asset))
}

/// the check, shared by the button, the install and the startup probe
async fn check(app: &App) -> Snapshot {
    let busy = matches!(
        app.update.snapshot().state,
        "checking" | "downloading" | "ready"
    );
    if busy {
        return app.update.snapshot();
    }
    app.update.set(|s| {
        s.state = "checking";
        s.error.clear();
    });
    let res = ask(&app.client(), &releases_url()).await;
    app.update.set(|s| match res {
        Ok((latest, page, asset)) => {
            s.latest = latest;
            s.page = page;
            s.asset = asset;
            s.state = if newer(&s.latest, env!("CARGO_PKG_VERSION")) {
                "available"
            } else {
                "latest"
            };
        }
        Err(e) => {
            s.state = "error";
            s.error = e;
        }
    });
    app.update.snapshot()
}

/// at startup, so the Update pill knows before anyone asks
pub async fn background_check(app: &Arc<App>) {
    check(app).await;
}

pub async fn status(State(app): State<Arc<App>>) -> Response {
    Json(status_json(&app.update.snapshot())).into_response()
}

pub async fn check_handler(State(app): State<Arc<App>>) -> Response {
    Json(status_json(&check(&app).await)).into_response()
}

/// With a newer release in hand this downloads it (streamed, so the page can
/// show progress); with one already downloaded it swaps the binary and
/// restarts into it — an answer means it didn't.
pub async fn install(State(app): State<Arc<App>>) -> Response {
    let mut s = app.update.snapshot();
    if s.state == "ready" {
        return match swap_and_restart(&app) {
            Ok(()) => unreachable!("a successful swap never returns"),
            Err(e) => {
                app.update.set(|s| {
                    s.error = e.clone();
                });
                s.error = e;
                Json(status_json(&s)).into_response()
            }
        };
    }
    if s.state != "downloading" {
        s = check(&app).await;
        if s.state == "available" {
            start_download(&app, &s.asset);
            s = app.update.snapshot();
        }
    }
    Json(status_json(&s)).into_response()
}

fn start_download(app: &Arc<App>, asset: &str) {
    if asset.is_empty() {
        app.update.set(|s| {
            s.state = "error";
            s.error = "the release has no binary for this platform".to_owned();
        });
        return;
    }
    let app = app.clone();
    let asset = asset.to_owned();
    tokio::spawn(async move {
        let fail = |app: &App, e: String| {
            app.update.set(|s| {
                s.state = "error";
                s.error = e.clone();
            });
        };
        let res = match app.client().get(&asset).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => return fail(&app, format!("the download answered {}", r.status())),
            Err(e) => return fail(&app, e.to_string()),
        };
        let total = res.content_length().unwrap_or(0);
        app.update.set(|s| {
            s.state = "downloading";
            s.total = total;
            s.done = 0;
            s.error.clear();
        });
        let mut file = match std::fs::File::create(&app.update_path) {
            Ok(f) => f,
            Err(e) => return fail(&app, e.to_string()),
        };
        use std::io::Write;
        let mut stream = res.bytes_stream();
        let mut done = 0u64;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if let Err(e) = file.write_all(&bytes) {
                        return fail(&app, e.to_string());
                    }
                    done += bytes.len() as u64;
                    app.update.set(|s| s.done = done);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&app.update_path);
                    return fail(&app, e.to_string());
                }
            }
        }
        app.update.set(|s| {
            if done > 0 {
                s.state = "ready";
                s.total = total.max(done);
            } else {
                s.state = "error";
                s.error = "the download came back empty".to_owned();
            }
        });
    });
}

/// the running binary out of the way, the new one in its place, the process
/// replaced — the args carry over, so the gateway comes back where it was
fn swap_and_restart(app: &App) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if !app.update_path.exists() {
        return Err("the downloaded binary is gone".to_owned());
    }
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    // a running exe can still be renamed, on Windows as anywhere else
    std::fs::rename(&exe, &old).map_err(|e| format!("the running binary can't be moved: {e}"))?;
    // a rename across volumes won't go; a copy does
    if std::fs::rename(&app.update_path, &exe).is_err()
        && std::fs::copy(&app.update_path, &exe).is_err()
    {
        let _ = std::fs::rename(&old, &exe);
        return Err("the new binary can't be moved into place".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(&exe)
        .args(args)
        .spawn()
        .map_err(|e| format!("the new magpie wouldn't start: {e}"))?;
    tracing::info!("restarting into magpie {}", app.update.snapshot().latest);
    std::process::exit(0);
}
