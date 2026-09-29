//! Token usage: every proxied answer's vendor usage report is picked out of
//! the stream, kept on disk and aggregated for the Usage tab — whose two
//! numbers are the tokens a window took and the cache hit rate, computed the
//! way pi does it: cache read over the whole prompt, across the calls whose
//! vendor reports caching at all.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// usage lives in the first and last events of every known wire format, so a
/// bounded head+tail copy of each answer is all the parsing ever needs
const KEEP: usize = 512 * 1024;
/// history on disk: about a year, and never more records than a busy year
const KEEP_DAYS: u64 = 366;
const KEEP_RECORDS: usize = 100_000;

/// one proxied call and the tokens its answer reported; `input` never
/// includes the cached tokens, the way Anthropic reports it natively
#[derive(Serialize, Deserialize)]
pub struct Record {
    pub time: u64,
    #[serde(default)]
    pub model: String,
    pub status: u16,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub reasoning: u64,
}

pub struct Store {
    records: Mutex<Vec<Record>>,
    path: PathBuf,
}

impl Store {
    pub fn load(path: PathBuf) -> Store {
        let records = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Store {
            records: Mutex::new(records),
            path,
        }
    }

    /// every call lands on disk at once: a quit is a hard exit, so anything
    /// still only in memory would be lost with it
    pub fn push(&self, record: Record) {
        self.records.lock().unwrap().push(record);
        self.save();
    }

    /// the log out of line and renamed, so a crash mid-write can't truncate it
    fn save(&self) {
        let mut records = self.records.lock().unwrap();
        let cutoff = now_ms().saturating_sub(KEEP_DAYS * 86_400_000);
        records.retain(|r| r.time >= cutoff);
        if records.len() > KEEP_RECORDS {
            let excess = records.len() - KEEP_RECORDS;
            records.drain(..excess);
        }
        let body = serde_json::to_string(&*records).unwrap_or_default();
        drop(records);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// The Usage tab's answer: totals over the window, the per-bucket series
    /// and the per-model share, plus the hit rate. Only calls whose vendor
    /// reported caching take part in the rate — for a provider that never
    /// reports it, a zero means nothing.
    pub fn summary(&self, period: &str, path: &str) -> Value {
        let records = self.records.lock().unwrap();
        let now = jiff::Zoned::now();
        let (start, bucket, days): (u64, &str, i64) = match period {
            "today" => (day_start(&now, 0), "hour", 0),
            "7d" => (day_start(&now, 6), "day", 6),
            "30d" => (day_start(&now, 29), "day", 29),
            _ => (0, "month", 0),
        };

        // the window's buckets, oldest first; each call lands in one by its
        // local time
        let mut labels: Vec<String> = Vec::new();
        match bucket {
            "hour" => labels.extend((0..24).map(|h| format!("{h:02}"))),
            "day" => labels.extend((0..=days).rev().filter_map(|i| {
                now.date()
                    .checked_add(jiff::Span::new().days(-i))
                    .ok()
                    .map(|d| d.to_string())
            })),
            _ => {
                if let Some(first) = records.iter().map(|r| r.time).min()
                    && let Some(oldest) = zoned(first)
                {
                    let (y0, m0) = (
                        i64::from(oldest.date().year()),
                        i64::from(oldest.date().month()),
                    );
                    let (y1, m1) = (i64::from(now.date().year()), i64::from(now.date().month()));
                    let months = (y1 - y0) * 12 + (m1 - m0);
                    labels.extend((0..=months).filter_map(|k| {
                        oldest
                            .date()
                            .checked_add(jiff::Span::new().months(k))
                            .ok()
                            .map(|d| format!("{}-{:02}", d.year(), d.month()))
                    }));
                }
            }
        }
        let mut series: Vec<(u64, u64, u64)> = vec![(0, 0, 0); labels.len()];
        let index: HashMap<&str, usize> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| (label.as_str(), i))
            .collect();

        let (mut calls, mut errors) = (0u64, 0u64);
        let (mut input, mut output) = (0u64, 0u64);
        let (mut cache_read, mut cache_write, mut reasoning) = (0u64, 0u64, 0u64);
        let (mut hit_read, mut hit_prompt) = (0u64, 0u64);
        let mut models: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
        for r in records.iter().filter(|r| r.time >= start) {
            calls += 1;
            errors += u64::from(r.status >= 400);
            input += r.input;
            output += r.output;
            cache_read += r.cache_read;
            cache_write += r.cache_write;
            reasoning += r.reasoning;
            if r.cache_read + r.cache_write > 0 {
                hit_read += r.cache_read;
                hit_prompt += r.input + r.cache_read + r.cache_write;
            }
            if !r.model.is_empty() {
                let m = models.entry(r.model.clone()).or_default();
                *m = (
                    m.0 + 1,
                    m.1 + u64::from(r.status >= 400),
                    m.2 + r.input,
                    m.3 + r.output,
                    m.4 + r.cache_read,
                );
            }
            if let Some(slot) = index.get(label(r.time, bucket).as_str()) {
                let b = &mut series[*slot];
                *b = (b.0 + 1, b.1 + r.input, b.2 + r.output);
            }
        }
        let hit_rate =
            (hit_prompt > 0).then(|| (1000.0 * hit_read as f64 / hit_prompt as f64).round() / 10.0);

        let mut rows: Vec<Value> = models
            .into_iter()
            .map(|(name, (n, errs, i, o, cr))| {
                json!({ "name": name, "icon": "generic", "calls": n, "errors": errs, "input": i, "output": o, "cache_read": cr })
            })
            .collect();
        rows.sort_by(|a, b| {
            let t = |v: &Value| {
                v["input"].as_u64().unwrap_or(0)
                    + v["output"].as_u64().unwrap_or(0)
                    + v["cache_read"].as_u64().unwrap_or(0)
            };
            t(b).cmp(&t(a))
        });

        json!({
            "calls": calls,
            "input": input,
            "output": output,
            "cache_read": cache_read,
            "cache_write": cache_write,
            "reasoning": reasoning,
            "errors": errors,
            "cost": 0.0,
            "unpriced": 0,
            "hit_rate": hit_rate,
            "series": series.iter().zip(labels).map(|((n, i, o), label)| json!({ "label": label, "calls": n, "input": i, "output": o })).collect::<Vec<_>>(),
            "bucket": bucket,
            "agents": [],
            "models": rows,
            "path": path,
        })
    }
}

/// local midnight `days_ago` days before `now`, as unix milliseconds
fn day_start(now: &jiff::Zoned, days_ago: i64) -> u64 {
    now.checked_sub(jiff::Span::new().days(days_ago))
        .ok()
        .and_then(|z| z.start_of_day().ok())
        .map(|z| z.timestamp().as_millisecond() as u64)
        .unwrap_or(0)
}

fn zoned(ms: u64) -> Option<jiff::Zoned> {
    jiff::Timestamp::from_millisecond(ms as i64)
        .ok()
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()))
}

/// the bucket label a call's local time lands under
fn label(ms: u64, bucket: &str) -> String {
    match zoned(ms) {
        Some(z) => match bucket {
            "hour" => format!("{:02}", z.hour()),
            "day" => z.date().to_string(),
            _ => format!("{}-{:02}", z.date().year(), z.date().month()),
        },
        None => String::new(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// the bytes of a streamed answer: head and tail kept, the middle dropped when
/// the body outgrows the budget
#[derive(Default)]
pub struct Tee {
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl Tee {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.head.len() < KEEP {
            let take = (KEEP - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
            self.push_tail(&bytes[take..]);
        } else {
            self.push_tail(bytes);
        }
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > KEEP {
            self.tail.drain(..self.tail.len() - KEEP);
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut all = Vec::with_capacity(self.head.len() + self.tail.len());
        all.extend_from_slice(&self.head);
        all.extend_from_slice(&self.tail);
        all
    }
}

/// the tokens one usage object reports, `None` where the vendor said nothing;
/// each field keeps its own maximum while merging, which is what the final
/// cumulative counts are on every wire format that repeats them
#[derive(Default)]
struct Tokens {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
}

fn normalize(v: &Value) -> Tokens {
    let as_num = |v: &Value| v.as_u64();
    let pick = |keys: &[&str]| keys.iter().find_map(|k| v.get(*k).and_then(as_num));
    let nested = |outer: &str, key: &str| v.get(outer).and_then(|o| o.get(key)).and_then(as_num);

    let prompt = pick(&["input_tokens", "prompt_tokens", "promptTokenCount"]);
    let mut t = Tokens {
        output: pick(&["output_tokens", "completion_tokens", "candidatesTokenCount"]),
        cache_write: pick(&["cache_creation_input_tokens"]),
        reasoning: nested("completion_tokens_details", "reasoning_tokens")
            .or_else(|| nested("output_tokens_details", "reasoning_tokens"))
            .or_else(|| pick(&["thoughtsTokenCount"])),
        ..Tokens::default()
    };
    // Anthropic's input_tokens already excludes the cached (and cache-written)
    // tokens; every other format reports the cache read as a subset of the
    // prompt, so it is subtracted
    let anthropic = pick(&["cache_read_input_tokens"]);
    let read = anthropic
        .or_else(|| nested("prompt_tokens_details", "cached_tokens"))
        .or_else(|| nested("input_tokens_details", "cached_tokens"))
        .or_else(|| pick(&["cachedContentTokenCount", "prompt_cache_hit_tokens"]));
    t.cache_read = read;
    t.input = match (prompt, read) {
        (Some(p), Some(r)) if anthropic.is_none() => Some(p.saturating_sub(r)),
        (p, _) => p,
    };
    t
}

fn merge(acc: &mut Tokens, t: Tokens) {
    fn over(acc: &mut Option<u64>, v: Option<u64>) {
        if v.is_some_and(|v| acc.is_none_or(|a| v > a)) {
            *acc = v;
        }
    }
    over(&mut acc.input, t.input);
    over(&mut acc.output, t.output);
    over(&mut acc.cache_read, t.cache_read);
    over(&mut acc.cache_write, t.cache_write);
    over(&mut acc.reasoning, t.reasoning);
}

/// every usage object a JSON value carries, at any of the shapes the popular
/// wire formats use: a chat answer's `usage`, Anthropic's `message.usage`, the
/// Responses API's `response.usage`, Gemini's `usageMetadata`
fn harvest(v: &Value, out: &mut Vec<Tokens>) {
    for holder in [Some(v), v.get("message"), v.get("response")]
        .into_iter()
        .flatten()
    {
        if let Some(u) = holder.get("usage").filter(|u| u.is_object()) {
            out.push(normalize(u));
        }
    }
    if let Some(u) = v.get("usageMetadata").filter(|u| u.is_object()) {
        out.push(normalize(u));
    }
}

/// the usage a whole answer reports: read as one JSON document (a plain
/// answer) and line by line (an SSE stream), then merged
fn extract(bytes: &[u8]) -> Tokens {
    let mut maps: Vec<Tokens> = Vec::new();
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        harvest(&v, &mut maps);
    }
    for line in bytes.split(|&b| b == b'\n') {
        let line = line.strip_prefix(b"data:").unwrap_or(line);
        let line = line.strip_prefix(b" ").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"[DONE]") {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            harvest(&v, &mut maps);
        }
    }
    let mut acc = Tokens::default();
    for t in maps {
        merge(&mut acc, t);
    }
    acc
}

/// parse the kept answer bytes and log the call with the tokens it reported
pub fn record(app: &crate::App, model: &str, status: u16, tee: &Mutex<Tee>) {
    let tokens = extract(&tee.lock().unwrap().bytes());
    app.usage.push(Record {
        time: now_ms(),
        model: model.to_owned(),
        status,
        input: tokens.input.unwrap_or(0),
        output: tokens.output.unwrap_or(0),
        cache_read: tokens.cache_read.unwrap_or(0),
        cache_write: tokens.cache_write.unwrap_or(0),
        reasoning: tokens.reasoning.unwrap_or(0),
    });
}
