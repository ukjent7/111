//! The agents' own sessions, read from their session files on disk — what
//! each cost and the command that picks it up again, as a segment of the
//! Usage page. Only reading happens here; the agents' files are never
//! touched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{App, usage};

/// the files are read again after this long; the page asks every few seconds
const TTL: Duration = Duration::from_secs(4);
/// the list is the latest few, not every session of the year
const SHOW: usize = 200;
/// a pause between messages shorter than this is work, not idle
const ACTIVE_GAP: u64 = 5 * 60;

pub struct Session {
    pub agent: &'static str,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub path: String,
    pub start: u64,
    pub last: u64,
    pub active: u64,
    /// model → [input, output, cache_read, cache_write]
    pub tokens: BTreeMap<String, [u64; 4]>,
}

fn agent_name(agent: &str) -> &'static str {
    match agent {
        "codex" => "Codex",
        _ => "Claude Code",
    }
}

fn agent_icon(agent: &str) -> &'static str {
    match agent {
        "codex" => "codex-color",
        _ => "claudecode-color",
    }
}

fn resume_command(agent: &str, id: &str) -> String {
    match agent {
        "codex" => format!("codex resume {id}"),
        _ => format!("claude --resume {id}"),
    }
}

/// an RFC 3339 stamp as unix milliseconds
fn ms(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_millisecond() as u64)
        .unwrap_or(0)
}

/// a message's text, whether the content is one string or blocks
fn first_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    v.as_array()
        .into_iter()
        .flatten()
        .find_map(|b| {
            (b["type"].as_str() == Some("text"))
                .then(|| b["text"].as_str().map(str::to_owned))
                .flatten()
        })
        .unwrap_or_default()
}

/// a session's spans: when it began and was last seen, and the time at work —
/// the pauses between messages, each under the gap, summed
fn finalize(mut s: Session, mut times: Vec<u64>) -> Option<Session> {
    if times.is_empty() {
        return None;
    }
    times.sort_unstable();
    s.start = times[0];
    s.last = times[times.len() - 1];
    s.active = times
        .windows(2)
        .filter(|w| w[1] - w[0] <= ACTIVE_GAP * 1000)
        .map(|w| w[1] - w[0])
        .sum::<u64>()
        / 1000;
    if s.title.is_empty() {
        s.title = "(no prompt)".to_owned();
    } else {
        s.title = s.title.chars().take(120).collect();
    }
    Some(s)
}

/// Claude Code: ~/.claude/projects/<project>/<session>.jsonl, one JSON
/// object per line
fn claude_session(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut s = Session {
        agent: "claude",
        id: path.file_stem()?.to_string_lossy().into_owned(),
        title: String::new(),
        cwd: String::new(),
        path: path.to_string_lossy().into_owned(),
        start: 0,
        last: 0,
        active: 0,
        tokens: BTreeMap::new(),
    };
    let mut times = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let t = ms(&v["timestamp"]);
        if t > 0 {
            times.push(t);
        }
        if s.cwd.is_empty() {
            s.cwd = v["cwd"].as_str().unwrap_or_default().to_owned();
        }
        match v["type"].as_str() {
            Some("summary") if s.title.is_empty() => {
                s.title = v["summary"].as_str().unwrap_or_default().to_owned();
            }
            Some("user") if s.title.is_empty() && !v["isMeta"].as_bool().unwrap_or(false) => {
                s.title = first_text(&v["message"]["content"]);
            }
            Some("assistant") => {
                let model = v["message"]["model"].as_str().unwrap_or_default();
                if model.is_empty() {
                    continue;
                }
                let u = &v["message"]["usage"];
                let e = s.tokens.entry(model.to_owned()).or_default();
                e[0] += u["input_tokens"].as_u64().unwrap_or(0);
                e[1] += u["output_tokens"].as_u64().unwrap_or(0);
                e[2] += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                e[3] += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            _ => {}
        }
    }
    finalize(s, times)
}

/// Codex: ~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl; its token counts
/// are cumulative, so each entry is read as the step from the one before
fn codex_session(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut s = Session {
        agent: "codex",
        id: String::new(),
        title: String::new(),
        cwd: String::new(),
        path: path.to_string_lossy().into_owned(),
        start: 0,
        last: 0,
        active: 0,
        tokens: BTreeMap::new(),
    };
    let mut times = Vec::new();
    let mut model = String::new();
    let mut seen: [u64; 3] = [0; 3];
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let t = ms(&v["timestamp"]);
        if t > 0 {
            times.push(t);
        }
        let p = &v["payload"];
        match v["type"].as_str() {
            Some("session_meta") => {
                s.id = p["id"].as_str().unwrap_or_default().to_owned();
                s.cwd = p["cwd"].as_str().unwrap_or_default().to_owned();
            }
            Some("turn_context") => {
                model = p["model"].as_str().unwrap_or_default().to_owned();
            }
            Some("event_msg") => match p["type"].as_str() {
                Some("user_message") if s.title.is_empty() => {
                    s.title = p["message"].as_str().unwrap_or_default().to_owned();
                }
                Some("token_count") => {
                    let u = &p["info"]["total_token_usage"];
                    let now = [
                        u["input_tokens"].as_u64().unwrap_or(0),
                        u["output_tokens"].as_u64().unwrap_or(0),
                        u["cached_input_tokens"].as_u64().unwrap_or(0),
                    ];
                    if !model.is_empty() && now.iter().any(|n| *n > 0) {
                        let e = s.tokens.entry(model.clone()).or_default();
                        e[0] += now[0].saturating_sub(seen[0]);
                        e[1] += now[1].saturating_sub(seen[1]);
                        e[2] += now[2].saturating_sub(seen[2]);
                    }
                    seen = now;
                }
                _ => {}
            },
            _ => {}
        }
    }
    if s.id.is_empty() {
        s.id = path.file_stem()?.to_string_lossy().into_owned();
    }
    finalize(s, times)
}

/// *.jsonl under `dir`, a few levels down
fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                walk(&p, depth - 1, out);
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

fn scan() -> Vec<Session> {
    let mut out = Vec::new();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .ok();
    if let Some(home) = home {
        let mut files = Vec::new();
        walk(&home.join(".claude/projects"), 3, &mut files);
        for f in files {
            if let Some(s) = claude_session(&f) {
                out.push(s);
            }
        }
        let mut files = Vec::new();
        walk(&home.join(".codex/sessions"), 4, &mut files);
        for f in files {
            if let Some(s) = codex_session(&f) {
                out.push(s);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last));
    out.truncate(SHOW);
    out
}

/// the scan, cached for a moment: two endpoints read it, and the page polls
fn read(app: &App) -> Arc<Vec<Session>> {
    let mut cache = app.sessions.lock().unwrap();
    if let Some((at, s)) = cache.as_ref()
        && at.elapsed() < TTL
    {
        return s.clone();
    }
    let s = Arc::new(scan());
    *cache = Some((Instant::now(), s.clone()));
    s
}

/// one session as the list row reads it, priced against the catalog
fn row(s: &Session, catalog: &Value) -> Value {
    let mut models = Vec::new();
    let (mut input, mut output, mut cache_read, mut cache_write) = (0u64, 0u64, 0u64, 0u64);
    let mut cost = 0.0f64;
    let mut unpriced = 0usize;
    for (model, t) in &s.tokens {
        let price = usage::price_of(catalog, model);
        let c = price
            .as_ref()
            .map_or(0.0, |(p, _)| p.at(t[0], t[1], t[2], t[3]));
        models.push(json!({
            "model": model,
            "input": t[0],
            "output": t[1],
            "cache_read": t[2],
            "cache_write": t[3],
            "cost": c,
            "priced": price.is_some(),
        }));
        input += t[0];
        output += t[1];
        cache_read += t[2];
        cache_write += t[3];
        cost += c;
        if price.is_none() {
            unpriced += 1;
        }
    }
    json!({
        "agent": s.agent,
        "name": agent_name(s.agent),
        "icon": agent_icon(s.agent),
        "id": s.id,
        "title": s.title,
        "cwd": s.cwd,
        "path": s.path,
        "start": s.start,
        "last": s.last,
        "active": s.active,
        "input": input,
        "output": output,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "cost": cost,
        "unpriced": unpriced,
        "models": models,
        "resume": resume_command(s.agent, &s.id),
    })
}

/// the local "YYYY-MM-DD" a timestamp falls on
fn day_of(ms: u64) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date().to_string())
        .unwrap_or_default()
}

fn day_ms(day: &str, end: bool) -> u64 {
    day.parse::<jiff::civil::Date>().ok().map(|d| {
        let z = d
            .to_zoned(jiff::tz::TimeZone::system())
            .unwrap_or_else(|_| jiff::Zoned::now());
        if end {
            z + jiff::Span::new().days(1)
        } else {
            z
        }
        .timestamp()
        .as_millisecond() as u64
    })
    .unwrap_or(0)
}

pub async fn list(State(app): State<Arc<App>>) -> Response {
    let sessions = read(&app);
    let catalog = app.catalog.lock().unwrap();
    let rows: Vec<Value> = sessions.iter().map(|s| row(s, &catalog)).collect();
    let dirs: Vec<String> = rows
        .iter()
        .filter_map(|r| r["cwd"].as_str())
        .filter(|c| !c.is_empty())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Json(json!({
        "sessions": rows,
        // the terminal button exists wherever a resume command does
        "terminal": "1",
        "dirs": dirs,
    }))
    .into_response()
}

/// the range's totals, by day: what was used on each, by which agent, model
/// and folder, and the active time
pub async fn stats(
    State(app): State<Arc<App>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let days: i64 = params.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let sessions = read(&app);
    let catalog = app.catalog.lock().unwrap();

    let today = jiff::Zoned::now().date().to_string();
    let earliest = sessions.iter().map(|s| day_of(s.start)).min().unwrap_or_else(|| today.clone());
    let first = if days > 0 {
        jiff::Zoned::now()
            .checked_sub(jiff::Span::new().days(days - 1))
            .map(|z| z.date().to_string())
            .unwrap_or_else(|_| earliest.clone())
    } else {
        earliest.clone()
    };
    let from = day_ms(&first, false);
    if from == 0 {
        return Json(json!({ "from": "", "to": "", "days": [], "agents": {} })).into_response();
    }

    // (date, agent, model, cwd) → [in, out, cr, cw, cost] and (date, agent,
    // cwd) → seconds
    let mut usage_by_day: BTreeMap<(String, String, String, String), [f64; 5]> = BTreeMap::new();
    let mut active_by_day: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for s in sessions.iter().filter(|s| s.last >= from) {
        let date = day_of(s.start);
        for (model, t) in &s.tokens {
            let price = usage::price_of(&catalog, model);
            let e = usage_by_day
                .entry((date.clone(), s.agent.to_owned(), model.clone(), s.cwd.clone()))
                .or_default();
            e[0] += t[0] as f64;
            e[1] += t[1] as f64;
            e[2] += t[2] as f64;
            e[3] += t[3] as f64;
            e[4] += price.map_or(0.0, |(p, _)| p.at(t[0], t[1], t[2], t[3]));
        }
        *active_by_day
            .entry((date, s.agent.to_owned(), s.cwd.clone()))
            .or_default() += s.active;
    }

    let to = today;
    let mut out = Vec::new();
    let mut cursor = first.clone();
    while cursor <= to {
        let date = cursor.clone();
        let usage: Vec<Value> = usage_by_day
            .iter()
            .filter(|((d, ..), _)| *d == date)
            .map(|((_, agent, model, cwd), e)| {
                json!({
                    "agent": agent,
                    "model": model,
                    "cwd": cwd,
                    "input": e[0] as u64,
                    "output": e[1] as u64,
                    "cache_read": e[2] as u64,
                    "cache_write": e[3] as u64,
                    "cost": e[4],
                    "priced": e[4] > 0.0,
                })
            })
            .collect();
        let active: Vec<Value> = active_by_day
            .iter()
            .filter(|((d, ..), _)| *d == date)
            .map(|((_, agent, cwd), secs)| {
                json!({ "agent": agent, "cwd": cwd, "seconds": secs })
            })
            .collect();
        out.push(json!({ "date": date, "usage": usage, "active": active }));
        let next = day_ms(&cursor, true);
        if next == 0 {
            break;
        }
        cursor = jiff::Timestamp::from_millisecond(next as i64)
            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date().to_string())
            .unwrap_or(to.clone());
    }

    let mut agents = serde_json::Map::new();
    for a in sessions.iter().map(|s| s.agent).collect::<BTreeSet<_>>() {
        agents.insert(a.to_owned(), json!(agent_name(a)));
    }
    Json(json!({
        "from": first,
        "to": to,
        "days": out,
        "agents": agents,
    }))
    .into_response()
}

/// a session again, in the terminal of this machine
pub async fn terminal(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let agent = body["agent"].as_str().unwrap_or_default().to_owned();
    let id = body["id"].as_str().unwrap_or_default().to_owned();
    let sessions = read(&app);
    let Some(s) = sessions
        .iter()
        .find(|s| s.agent == agent && s.id == id)
        .map(|s| (s.cwd.clone(), resume_command(s.agent, &s.id)))
    else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let (cwd, resume) = s;
    let ok = match std::env::consts::OS {
        "windows" => {
            let mut cmd = std::process::Command::new("cmd");
            cmd.arg("/C")
                .raw_arg(format!("start \"magpie\" /D \"{cwd}\" cmd /K {resume}"));
            cmd.spawn().is_ok()
        }
        "macos" => {
            let script =
                format!("tell application \"Terminal\" to do script \"cd \\\"{cwd}\\\" && {resume}\"");
            let mut cmd = std::process::Command::new("osascript");
            cmd.arg("-e").arg(script);
            cmd.spawn().is_ok()
        }
        _ => {
            let shell = format!("cd \"{cwd}\" && {resume}; exec bash");
            let mut opened = false;
            for term in ["gnome-terminal", "konsole", "x-terminal-emulator", "xterm"] {
                let mut cmd = std::process::Command::new(term);
                cmd.arg("-e").arg("bash").arg("-c").arg(&shell);
                if cmd.spawn().is_ok() {
                    opened = true;
                    break;
                }
            }
            opened
        }
    };
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "no terminal found").into_response()
    }
}
