//! HTTP/1.1 front end (hyper 1): IPP over POST, `/health`, a status page, printer icons and the
//! strings file for printer-strings-uri. Body size is capped; oversize IPP requests get an IPP
//! `client-error-request-value-too-long` (which makes the CUPS backend cancel the job cleanly).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::engine::Engine;
use crate::ipp::codec::{peek_header, Resp, Status};
use crate::ipp::IppService;

/// Shared state for request handlers.
pub struct AppState {
    pub ipp: Arc<IppService>,
    pub engine: Engine,
    pub max_body: usize,
    /// DNS-SD registration status text for /health.
    pub dnssd: Arc<RwLock<serde_json::Value>>,
    /// Extra JSON merged into /health (adapter state etc.).
    pub extra_health: Arc<RwLock<serde_json::Value>>,
}

const BODY_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn run(
    listener: TcpListener,
    state: Arc<AppState>,
    shutdown: CancellationToken,
) -> Result<()> {
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let io = TokioIo::new(stream);
                let st = state.clone();
                let svc = service_fn(move |req| {
                    let st = st.clone();
                    async move { handle(req, st).await }
                });
                let conn = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(30))
                    .keep_alive(true)
                    .max_buf_size(1024 * 1024)
                    .serve_connection(io, svc);
                let fut = graceful.watch(conn);
                tokio::spawn(async move {
                    if let Err(e) = fut.await {
                        tracing::debug!(%peer, "connection ended: {e}");
                    }
                });
            }
            _ = shutdown.cancelled() => break,
        }
    }
    tracing::info!("http: draining connections");
    tokio::select! {
        _ = graceful.shutdown() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => tracing::warn!("http: drain timed out"),
    }
    Ok(())
}

type Out = Response<Full<Bytes>>;

fn respond(status: StatusCode, ctype: &str, body: impl Into<Bytes>) -> Out {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::SERVER, format!("catprinterd/{}", crate::VERSION))
        .body(Full::new(body.into()))
        .unwrap()
}

async fn handle(
    req: Request<Incoming>,
    st: Arc<AppState>,
) -> Result<Out, std::convert::Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let out = match (method, path.as_str()) {
        (Method::POST, _) => ipp_post(req, &st).await,
        (Method::GET, "/health") | (Method::HEAD, "/health") => health(&st, &query),
        (Method::GET, "/") | (Method::HEAD, "/") => status_page(&st),
        (Method::GET, "/strings/en.strings") => respond(
            StatusCode::OK,
            "text/strings; charset=utf-8",
            crate::ipp::media::strings_en(),
        ),
        (Method::GET, p) if p.starts_with("/icons/") => icon(p),
        (Method::GET, p) if p.starts_with("/jobs/") && p.ends_with("/preview.png") => {
            preview(&st, p)
        }
        (Method::OPTIONS, _) => respond(StatusCode::NO_CONTENT, "text/plain", ""),
        _ => respond(StatusCode::NOT_FOUND, "text/plain", "not found\n"),
    };
    Ok(out)
}

async fn ipp_post(req: Request<Incoming>, st: &AppState) -> Out {
    let max = st.max_body;
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    let mut body = req.into_body();
    // Too big by declaration: answer without reading the document. Peek the header if possible.
    if declared.is_some_and(|l| l > max) {
        let head = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .ok()
            .and_then(|f| f?.ok())
            .and_then(|f| f.into_data().ok());
        return oversize(head.as_deref());
    }
    // Stream frames up to the cap.
    let mut buf: Vec<u8> = Vec::with_capacity(declared.unwrap_or(64 * 1024).min(max));
    let collect = async {
        while let Some(frame) = body.frame().await {
            let frame = match frame {
                Ok(f) => f,
                Err(e) => return Err(format!("body read: {e}")),
            };
            if let Ok(data) = frame.into_data() {
                if buf.len() + data.len() > max {
                    buf.extend_from_slice(&data[..(max - buf.len()).min(data.len())]);
                    return Ok(false);
                }
                buf.extend_from_slice(&data);
            }
        }
        Ok(true)
    };
    match tokio::time::timeout(BODY_TIMEOUT, collect).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return oversize(Some(&buf)),
        Ok(Err(e)) => {
            tracing::debug!("{e}");
            return respond(StatusCode::BAD_REQUEST, "text/plain", "bad request body\n");
        }
        Err(_) => {
            return respond(
                StatusCode::REQUEST_TIMEOUT,
                "text/plain",
                "request body timeout\n",
            )
        }
    }
    let out = st.ipp.handle(Bytes::from(buf));
    let status = StatusCode::from_u16(out.http_status).unwrap_or(StatusCode::OK);
    respond(status, "application/ipp", out.body)
}

fn oversize(head: Option<&[u8]>) -> Out {
    let (ver, _op, id) = head.and_then(peek_header).unwrap_or((0x0101, 0, 1));
    let ver = if matches!(ver >> 8, 1 | 2) {
        ver
    } else {
        0x0101
    };
    let r = Resp::new(ver, Status::ClientErrorRequestValueTooLong, id)
        .status_message("document too large for this printer");
    let mut out = respond(StatusCode::OK, "application/ipp", r.into_bytes());
    out.headers_mut()
        .insert(header::CONNECTION, "close".parse().unwrap());
    out
}

fn health(st: &AppState, query: &str) -> Out {
    if query.contains("refresh") {
        // hint for the uuid adopter (cheap; it polls anyway)
        st.engine.store().tick();
    }
    let mut v = serde_json::to_value(st.engine.store().health()).unwrap_or_default();
    if let serde_json::Value::Object(ref mut m) = v {
        m.insert(
            "dnssd".into(),
            st.dnssd
                .read()
                .map(|d| d.clone())
                .unwrap_or(serde_json::Value::Null),
        );
        m.insert(
            "printer_uri".into(),
            serde_json::Value::String(st.ipp.cfg.printer_uri()),
        );
        m.insert(
            "printer_uuid".into(),
            serde_json::Value::String(st.ipp.cfg.uuid()),
        );
        if let Ok(extra) = st.extra_health.read() {
            if let serde_json::Value::Object(em) = &*extra {
                for (k, val) in em {
                    m.insert(k.clone(), val.clone());
                }
            }
        }
    }
    let mut body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into());
    body.push('\n');
    respond(StatusCode::OK, "application/json", body)
}

fn status_page(st: &AppState) -> Out {
    let h = st.engine.store().health();
    let jobs: Vec<String> = {
        let s = st.engine.store();
        s.jobs
            .values()
            .rev()
            .take(20)
            .map(|j| {
                format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    j.id,
                    esc(&j.opts.job_name),
                    esc(&j.opts.user),
                    crate::engine::state_name(j.state),
                    esc(&j.message)
                )
            })
            .collect()
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>Cat Printer</title>\
<style>body{{font:15px system-ui;margin:2em;max-width:60em}}table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.3em .6em;text-align:left}}.s{{font-size:1.4em}}</style>\
<h1>🐈 Cat Printer <small>catprinterd {v}</small></h1>\
<p class=s>State: <b>{state}</b> {reasons}<br>{msg}</p>\
<p>Queue depth {qd} · jobs so far {jt} · last model {lm} · battery {bat}</p>\
<p>IPP: <code>{uri}</code> · <a href=/health>health JSON</a></p>\
<table><tr><th>#</th><th>Job</th><th>User</th><th>State</th><th>Message</th></tr>{rows}</table>",
        v = crate::VERSION,
        state = h.printer_state,
        reasons = if h.reasons.is_empty() { String::new() } else { format!("({})", h.reasons.join(", ")) },
        msg = esc(&h.message),
        qd = h.queue_depth,
        jt = h.jobs_total,
        lm = h.last_model.as_deref().unwrap_or("—"),
        bat = h.battery.map(|b| format!("{b}%")).unwrap_or_else(|| "—".into()),
        uri = st.ipp.cfg.printer_uri(),
        rows = jobs.join("")
    );
    respond(StatusCode::OK, "text/html; charset=utf-8", html)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn preview(st: &AppState, path: &str) -> Out {
    let id: Option<u32> = path
        .trim_start_matches("/jobs/")
        .trim_end_matches("/preview.png")
        .parse()
        .ok();
    let png = id
        .and_then(|id| {
            st.engine
                .store()
                .jobs
                .get(&id)
                .and_then(|j| j.preview.clone())
        })
        .and_then(|p| crate::render::preview_png_bytes(&p).ok());
    match png {
        Some(bytes) => respond(StatusCode::OK, "image/png", bytes),
        None => respond(StatusCode::NOT_FOUND, "text/plain", "no preview\n"),
    }
}

fn icon(path: &str) -> Out {
    let size: u32 = match path {
        "/icons/48.png" => 48,
        "/icons/128.png" => 128,
        "/icons/512.png" => 512,
        _ => return respond(StatusCode::NOT_FOUND, "text/plain", "not found\n"),
    };
    match icon_png(size) {
        Ok(b) => respond(StatusCode::OK, "image/png", b),
        Err(e) => respond(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            format!("{e}\n"),
        ),
    }
}

/// A simple cat-face icon drawn procedurally (no binary assets to ship).
pub fn icon_png(size: u32) -> Result<Vec<u8>> {
    use image::{Rgba, RgbaImage};
    let s = size as f32;
    let mut img = RgbaImage::from_pixel(size, size, Rgba([255, 255, 255, 0]));
    let inside = |x: f32, y: f32| -> Option<Rgba<u8>> {
        // rounded white tile
        let r = s * 0.18;
        let d = |cx: f32, cy: f32| ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        let in_tile = (x > r && x < s - r)
            || (y > r && y < s - r)
            || d(r, r) < r
            || d(s - r, r) < r
            || d(r, s - r) < r
            || d(s - r, s - r) < r;
        if !in_tile {
            return None;
        }
        let mut col = Rgba([250, 244, 232, 255]);
        // head
        let (hx, hy, hr) = (s * 0.5, s * 0.58, s * 0.30);
        let head = d(hx, hy) < hr;
        // ears: triangles
        let ear = |ex: f32, dir: f32| {
            let bx = ex;
            let by = hy - hr * 0.55;
            let tipx = ex + dir * hr * 0.35;
            let tipy = hy - hr * 1.25;
            let basex2 = ex + dir * hr * 0.75;
            // point-in-triangle
            let (ax, ay, bx2, by2, cx, cy) = (bx, by, tipx, tipy, basex2, by + hr * 0.25);
            let sign =
                |px: f32, py: f32, qx: f32, qy: f32| (x - qx) * (py - qy) - (px - qx) * (y - qy);
            let d1 = sign(ax, ay, bx2, by2);
            let d2 = sign(bx2, by2, cx, cy);
            let d3 = sign(cx, cy, ax, ay);
            let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
            let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
            !(has_neg && has_pos)
        };
        if head || ear(hx - hr * 0.55, -1.0) || ear(hx + hr * 0.55, 1.0) {
            col = Rgba([40, 40, 44, 255]);
        }
        // eyes
        let er = hr * 0.13;
        if d(hx - hr * 0.38, hy - hr * 0.1) < er || d(hx + hr * 0.38, hy - hr * 0.1) < er {
            col = Rgba([250, 244, 232, 255]);
        }
        // nose
        if d(hx, hy + hr * 0.25) < er * 0.7 {
            col = Rgba([232, 120, 130, 255]);
        }
        Some(col)
    };
    for y in 0..size {
        for x in 0..size {
            if let Some(c) = inside(x as f32 + 0.5, y as f32 + 0.5) {
                img.put_pixel(x, y, c);
            }
        }
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}
