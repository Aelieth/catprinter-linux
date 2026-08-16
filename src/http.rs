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
use crate::ipp::codec::{parse_contained, peek_header, Resp, Status};
use crate::ipp::{IppService, OP_CLOSE_JOB, OP_SEND_DOCUMENT};

/// Shared state for request handlers.
pub struct AppState {
    pub ipp: Arc<IppService>,
    pub engine: Engine,
    pub max_body: usize,
    /// DNS-SD registration status text for /health.
    pub dnssd: Arc<RwLock<serde_json::Value>>,
    /// Extra JSON merged into /health (adapter state etc.).
    pub extra_health: Arc<RwLock<serde_json::Value>>,
    /// A request body that sends nothing for this long is dropped (408).
    pub body_idle: Duration,
    /// Hard cap on the total time one request body may take.
    pub body_total: Duration,
    /// Poked (via `/health?refresh`) so the uuid adopter re-checks the CUPS queue immediately —
    /// ensure-queue calls it right after creating the queue so the DNS-SD UUID converges in
    /// seconds instead of on the adopter's slow backoff.
    pub adopt_now: Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn new(ipp: Arc<IppService>, engine: Engine, max_body: usize) -> Self {
        AppState {
            ipp,
            engine,
            max_body,
            dnssd: Arc::new(RwLock::new(serde_json::json!({}))),
            extra_health: Arc::new(RwLock::new(serde_json::json!({}))),
            body_idle: Duration::from_secs(120),
            body_total: Duration::from_secs(30 * 60),
            adopt_now: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// Only the first bytes of a streaming body are worth parsing to learn the job id.
const PEEK_LIMIT: usize = 64 * 1024;
/// How often a still-streaming Send-Document refreshes the job's activity stamp.
const TOUCH_EVERY: Duration = Duration::from_secs(5);

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
    // Stream frames up to the cap. The CUPS backend streams a Send-Document while the filters
    // still render (minutes for a big PDF): the timer is per-frame idleness, not total time,
    // and the job's activity stamp is refreshed so the stale-Create-Job sweeper leaves it alone.
    let mut buf: Vec<u8> = Vec::with_capacity(declared.unwrap_or(64 * 1024).min(4 * 1024 * 1024));
    let started = std::time::Instant::now();
    let mut touched: Option<u32> = None;
    let mut peek_failed = false;
    let mut last_touch = started;
    loop {
        let frame = match tokio::time::timeout(st.body_idle, body.frame()).await {
            Err(_) => {
                return respond(
                    StatusCode::REQUEST_TIMEOUT,
                    "text/plain",
                    "request body timeout\n",
                )
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                tracing::debug!("body read: {e}");
                return respond(StatusCode::BAD_REQUEST, "text/plain", "bad request body\n");
            }
            Ok(Some(Ok(f))) => f,
        };
        if started.elapsed() > st.body_total {
            return respond(
                StatusCode::REQUEST_TIMEOUT,
                "text/plain",
                "request body took too long\n",
            );
        }
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > max {
                buf.extend_from_slice(&data[..(max - buf.len()).min(data.len())]);
                return oversize(Some(&buf));
            }
            buf.extend_from_slice(&data);
        }
        match touched {
            None if !peek_failed && buf.len() <= PEEK_LIMIT => {
                if let Some(id) = peek_job_id(&buf) {
                    st.engine.touch(id);
                    touched = Some(id);
                    last_touch = std::time::Instant::now();
                }
            }
            None => peek_failed = true,
            Some(id) => {
                if last_touch.elapsed() >= TOUCH_EVERY {
                    st.engine.touch(id);
                    last_touch = std::time::Instant::now();
                }
            }
        }
    }
    let out = st.ipp.handle(Bytes::from(buf));
    let status = StatusCode::from_u16(out.http_status).unwrap_or(StatusCode::OK);
    respond(status, "application/ipp", out.body)
}

/// If the bytes so far already hold a complete attribute section of a Send-Document / Close-Job,
/// return its job-id (the operation attributes come first, so this succeeds on the first frame
/// or two of a multi-megabyte document; a partial attribute section simply parses as an error).
fn peek_job_id(buf: &[u8]) -> Option<u32> {
    let req = parse_contained(Bytes::copy_from_slice(buf)).ok()?;
    if req.op != OP_SEND_DOCUMENT && req.op != OP_CLOSE_JOB {
        return None;
    }
    if let Some(i) = req.get_int(
        Some(crate::ipp::codec::Group::OperationAttributes),
        "job-id",
    ) {
        return u32::try_from(i).ok();
    }
    let uri = req.get_str(
        Some(crate::ipp::codec::Group::OperationAttributes),
        "job-uri",
    )?;
    uri.rsplit('/').next()?.parse().ok()
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
        st.adopt_now.notify_one();
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
        lm = esc(h.last_model.as_deref().unwrap_or("—")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};
    use crate::ipp::codec::{parse, v_charset, v_lang, v_mime, v_uri, Group};
    use crate::ipp::{PrinterConfig, OP_CREATE_JOB, OP_SEND_DOCUMENT};
    use crate::printer::fake::FakePrinter;
    use crate::printer::Printer;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn ipp_msg(
        op: u16,
        id: i32,
        op_attrs: &[(&str, ipp::value::IppValue)],
        payload: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(0x0200u16.to_be_bytes());
        b.extend(op.to_be_bytes());
        b.extend(id.to_be_bytes());
        b.push(0x01);
        for (n, v) in op_attrs {
            let a = ipp::attribute::IppAttribute::new(
                ipp::value::IppName::new_truncated(*n),
                v.clone(),
            );
            b.extend(a.to_bytes());
        }
        b.push(0x03);
        b.extend(payload);
        b
    }

    fn base_attrs() -> Vec<(&'static str, ipp::value::IppValue)> {
        vec![
            ("attributes-charset", v_charset("utf-8")),
            ("attributes-natural-language", v_lang("en")),
            ("printer-uri", v_uri("ipp://127.0.0.1/ipp/print")),
        ]
    }

    struct Server {
        addr: std::net::SocketAddr,
        shutdown: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
        engine: Engine,
        _dir: tempfile::TempDir,
    }

    async fn server_with(mut app: impl FnMut(&mut AppState)) -> Server {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state"), "ok").unwrap();
        let printer = Printer::Fake(FakePrinter::new(dir.path()).unwrap());
        let shutdown = CancellationToken::new();
        let cfg = EngineConfig {
            stale_document: Duration::from_millis(200),
            printer_wait: Duration::from_secs(2),
            ..EngineConfig::default()
        };
        let (engine, _worker) = Engine::start(cfg, printer, shutdown.clone());
        let pcfg = PrinterConfig {
            name: "CatPrinter".into(),
            info: "Cat Printer".into(),
            location: "here".into(),
            make_model: "Cat Printer MXW01".into(),
            model_label: "MXW01".into(),
            host: "127.0.0.1".into(),
            port: 0,
            uuid: Arc::new(RwLock::new("urn:uuid:0".into())),
            resolutions: vec![203],
            max_document_kb: 65536,
            max_copies: 10,
            started: std::time::SystemTime::now(),
        };
        let ipp = Arc::new(IppService::new(engine.clone(), pcfg));
        let mut state = AppState::new(ipp, engine.clone(), 2 * 1024 * 1024);
        app(&mut state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sd = shutdown.clone();
        let handle = tokio::spawn(async move {
            let _ = run(listener, Arc::new(state), sd).await;
        });
        // give the accept loop a moment
        tokio::time::sleep(Duration::from_millis(50)).await;
        Server {
            addr,
            shutdown,
            handle,
            engine,
            _dir: dir,
        }
    }

    async fn server() -> Server {
        server_with(|_| {}).await
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.shutdown.cancel();
            self.handle.abort();
        }
    }

    /// Send a full HTTP request, return (status_line, headers, body).
    async fn http_post(addr: std::net::SocketAddr, body: &[u8]) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /ipp/print HTTP/1.1\r\nHost: x\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        read_http(s).await
    }

    async fn read_http(mut s: TcpStream) -> (u16, Vec<u8>) {
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..sep]);
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        // de-chunk if needed
        let body = &raw[sep + 4..];
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_vec()
        };
        (status, body)
    }

    fn dechunk(mut b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
            let n = usize::from_str_radix(std::str::from_utf8(&b[..nl]).unwrap().trim(), 16)
                .unwrap_or(0);
            b = &b[nl + 2..];
            if n == 0 {
                break;
            }
            out.extend_from_slice(&b[..n]);
            b = &b[n + 2..];
        }
        out
    }

    fn ipp_status(body: &[u8]) -> u16 {
        u16::from_be_bytes([body[2], body[3]])
    }

    fn raster() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tiny-roll48.pwg"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn get_health_root_icons_strings_and_404() {
        let srv = server().await;
        for (path, want) in [
            ("/health", 200),
            ("/", 200),
            ("/strings/en.strings", 200),
            ("/icons/48.png", 200),
            ("/nope", 404),
        ] {
            let mut s = TcpStream::connect(srv.addr).await.unwrap();
            let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            s.write_all(req.as_bytes()).await.unwrap();
            let (status, _body) = read_http(s).await;
            assert_eq!(status, want, "GET {path}");
        }
    }

    #[tokio::test]
    async fn content_length_over_cap_gets_too_long_and_connection_close() {
        let srv = server_with(|st| st.max_body = 4096).await;
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        let body_len = 100_000usize;
        let head = format!(
            "POST /ipp/print HTTP/1.1\r\nHost: x\r\nContent-Type: application/ipp\r\nContent-Length: {body_len}\r\n\r\n"
        );
        s.write_all(head.as_bytes()).await.unwrap();
        // a valid-looking header so the daemon can echo the request id
        let msg = ipp_msg(OP_CREATE_JOB, 7, &base_attrs(), b"");
        s.write_all(&msg).await.unwrap();
        let (status, body) = read_http(s).await;
        assert_eq!(status, 200);
        assert_eq!(ipp_status(&body), 0x0409); // request-value-too-long
    }

    #[tokio::test]
    async fn body_idle_timeout_returns_408() {
        let srv = server_with(|st| st.body_idle = Duration::from_millis(300)).await;
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        // promise a big body, then stall
        let head = "POST /ipp/print HTTP/1.1\r\nHost: x\r\nContent-Type: application/ipp\r\nContent-Length: 100000\r\n\r\n";
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(&ipp_msg(OP_CREATE_JOB, 8, &base_attrs(), b""))
            .await
            .unwrap();
        // do not send the rest
        let (status, _body) = read_http(s).await;
        assert_eq!(status, 408);
    }

    #[tokio::test]
    async fn streaming_send_document_slower_than_stale_window_prints() {
        let srv = server().await;
        // Create-Job
        let (st, body) = http_post(
            srv.addr,
            &ipp_msg(
                OP_CREATE_JOB,
                1,
                &[
                    ("attributes-charset", v_charset("utf-8")),
                    ("attributes-natural-language", v_lang("en")),
                    ("printer-uri", v_uri("ipp://127.0.0.1/ipp/print")),
                    ("document-format", v_mime("image/pwg-raster")),
                ],
                b"",
            ),
        )
        .await;
        assert_eq!(st, 200);
        let back = parse(Bytes::from(body)).unwrap();
        let id = back.get_int(Some(Group::JobAttributes), "job-id").unwrap();
        // Send-Document, dribbled slower than stale_document (200 ms) so the touch path is exercised
        let doc = raster();
        let msg = ipp_msg(
            OP_SEND_DOCUMENT,
            2,
            &[
                ("attributes-charset", v_charset("utf-8")),
                ("attributes-natural-language", v_lang("en")),
                ("printer-uri", v_uri("ipp://127.0.0.1/ipp/print")),
                ("job-id", ipp::value::IppValue::Integer(id)),
                ("last-document", ipp::value::IppValue::Boolean(true)),
            ],
            &doc,
        );
        let total = msg.len();
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        let head = format!(
            "POST /ipp/print HTTP/1.1\r\nHost: x\r\nContent-Type: application/ipp\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(head.as_bytes()).await.unwrap();
        // header first so the job-id peek fires, then the document in slow chunks
        let split = msg.len().min(64);
        s.write_all(&msg[..split]).await.unwrap();
        let mut off = split;
        while off < total {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let end = (off + 4096).min(total);
            s.write_all(&msg[off..end]).await.unwrap();
            off = end;
        }
        let (status, resp) = read_http(s).await;
        assert_eq!(status, 200);
        assert_eq!(ipp_status(&resp), 0x0000);
        // job should not have been aborted as stale; it prints
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let state = srv.engine.store().jobs.get(&(id as u32)).map(|j| j.state);
            if state == Some(crate::engine::JobState::Completed) {
                return;
            }
            assert_ne!(
                state,
                Some(crate::engine::JobState::Aborted),
                "stale-aborted"
            );
        }
        panic!("streamed job never completed");
    }
}
