use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use reqwest::redirect::Policy;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let (mut listen, mut upstream) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().with_context(|| format!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => listen = Some(value),
            "--upstream" => upstream = Some(value),
            other => bail!("unknown argument {other:?}; usage: magpie-gateway --listen <addr> --upstream <url>"),
        }
    }
    let listen: SocketAddr = listen
        .or_else(|| std::env::var("MAGPIE_LISTEN").ok())
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
        .context("invalid --listen address")?;
    let upstream = upstream
        .or_else(|| std::env::var("MAGPIE_UPSTREAM").ok())
        .context("no upstream: pass --upstream <url> or set MAGPIE_UPSTREAM")?;
    reqwest::Url::parse(&upstream).context("invalid --upstream URL")?;

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let app = Router::new()
        .fallback(proxy)
        .layer(DefaultBodyLimit::disable())
        .with_state((client, upstream));

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("listening on http://{listen}, passing everything through to {upstream}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Whatever arrives goes out unchanged: same method, path, query, headers and
/// body bytes (streamed both ways, so SSE and big payloads never buffer).
async fn proxy(State((client, upstream)): State<(reqwest::Client, String)>, req: Request) -> Response {
    let started = Instant::now();
    let (mut parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".into());
    let method = parts.method.to_string();
    let has_body = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .is_some_and(|v| v != "0")
        || parts.headers.contains_key(header::TRANSFER_ENCODING);
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove(header::HOST);
    parts.headers.remove(header::CONTENT_LENGTH);

    let url = format!("{}{}", upstream.trim_end_matches('/'), path);
    let mut sent = client.request(parts.method, url).headers(parts.headers);
    if has_body {
        sent = sent.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }

    let upstream_res = match sent.send().await {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!(%method, %path, error = %err, "upstream request failed");
            return (StatusCode::BAD_GATEWAY, format!("magpie: {err}")).into_response();
        }
    };

    let status = upstream_res.status();
    let mut headers = upstream_res.headers().clone();
    strip_hop_by_hop(&mut headers);
    let body = Body::from_stream(upstream_res.bytes_stream());
    let mut res = Response::builder()
        .status(status)
        .body(body)
        .expect("status and stream body are always valid");
    *res.headers_mut() = headers;
    tracing::info!(%method, %path, status = status.as_u16(), ms = started.elapsed().as_millis() as u64, "passed through");
    res
}

/// Hop-by-hop headers describe one connection, never the message; anything the
/// `Connection` header lists belongs to them too (RFC 9110 §7.6.1).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').filter_map(|name| name.trim().parse().ok()).collect())
        .unwrap_or_default();
    for name in listed {
        headers.remove(&name);
    }
    for name in [
        header::CONNECTION,
        header::KEEP_ALIVE,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}
