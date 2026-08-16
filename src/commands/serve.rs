//! `catprinterd serve`: wire config → printer → engine → IPP service → HTTP, plus the DNS-SD
//! and CUPS-uuid side tasks, and graceful shutdown on SIGTERM/SIGINT.

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::config::{OnOff, ServeArgs};
use crate::engine::{Engine, EngineConfig};
use crate::http::AppState;
use crate::ipp::{IppService, PrinterConfig};
use crate::printer::fake::FakePrinter;
use crate::printer::Printer;
use crate::render::RenderOptions;

/// Namespace for our v5 printer-uuid.
const UUID_NS: uuid::Uuid = uuid::Uuid::from_bytes([
    0x3a, 0x6f, 0x1c, 0x2e, 0x9b, 0x40, 0x4d, 0x0e, 0xa1, 0x77, 0xca, 0x7b, 0x11, 0x2c, 0x5e, 0x9d,
]);

pub fn machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "catprinter".into())
}

pub fn default_uuid(port: u16) -> String {
    let u = uuid::Uuid::new_v5(&UUID_NS, format!("{}:{port}", machine_id()).as_bytes());
    format!("urn:uuid:{u}")
}

fn normalize_uuid(s: &str) -> String {
    let t = s.trim();
    if t.starts_with("urn:uuid:") {
        t.to_string()
    } else {
        format!("urn:uuid:{t}")
    }
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let shutdown = CancellationToken::new();

    // ---- printer
    let printer = match &args.fake_printer {
        Some(dir) => {
            tracing::warn!("FAKE PRINTER: jobs are written to {}", dir.display());
            Printer::Fake(
                FakePrinter::new(dir)
                    .with_context(|| format!("fake printer dir {}", dir.display()))?,
            )
        }
        None => Printer::Ble(crate::ble::BlePrinter {
            device_hint: args.ble.device.clone(),
            adapter: args.ble.adapter.clone(),
            forced_family: args.ble.model.family(),
            slow: args.ble.slow,
            pacing_ms: args.ble.pacing_ms,
        }),
    };
    let is_fake = printer.is_fake();

    // ---- engine
    let render = RenderOptions {
        max_lines_total: args.max_lines,
        max_lines_per_request: args.max_lines_per_request.min(65535).max(90),
        ..RenderOptions::default()
    };
    let cfg = EngineConfig {
        printer_wait: Duration::from_secs(args.printer_wait),
        queue_max: args.queue_max.max(1),
        max_document_bytes: args.max_document_mb.max(1) * 1024 * 1024,
        max_copies: args.max_copies.max(1),
        render,
        limits: crate::raster::Limits::default(),
        shutdown_grace: Duration::from_secs(15),
    };
    let (engine, worker) = Engine::start(cfg, printer, shutdown.clone());

    // ---- identity
    let uuid = args
        .uuid
        .as_deref()
        .map(normalize_uuid)
        .unwrap_or_else(|| default_uuid(args.port));
    let uuid = Arc::new(RwLock::new(uuid));
    let model_label = "MXW01".to_string();
    let pcfg = PrinterConfig {
        name: args.printer_name.clone(),
        info: format!("Cat Printer{}", if is_fake { " (fake)" } else { "" }),
        location: args.location.clone(),
        make_model: format!("Cat Printer {model_label}"),
        model_label: model_label.clone(),
        host: args.bind.to_string(),
        port: args.port,
        uuid: uuid.clone(),
        resolutions: if args.resolutions.is_empty() {
            vec![203]
        } else {
            args.resolutions.clone()
        },
        max_document_kb: (args.max_document_mb.max(1) * 1024) as i32,
        max_copies: args.max_copies.max(1) as i32,
        started: SystemTime::now(),
    };
    let ipp = Arc::new(IppService::new(engine.clone(), pcfg.clone()));

    // ---- bind first (fail fast if the port is taken)
    let addr = std::net::SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(
        "🐈 catprinterd {} — IPP printer at {}",
        crate::VERSION,
        pcfg.printer_uri()
    );

    let dnssd_status = Arc::new(RwLock::new(
        serde_json::json!({"enabled": args.dnssd == OnOff::On, "registered": false}),
    ));
    let extra_health = Arc::new(RwLock::new(serde_json::json!({})));

    // ---- side tasks
    let (uuid_tx, uuid_rx) = tokio::sync::watch::channel(uuid.read().unwrap().clone());
    if args.uuid.is_none() {
        let adopt = crate::cupsq::adopt_uuid_task(
            args.queue.clone(),
            uuid.clone(),
            uuid_tx,
            shutdown.clone(),
        );
        tokio::spawn(adopt);
    }
    if args.dnssd == OnOff::On {
        let cfg = crate::dnssd::DnssdConfig {
            name: args.dnssd_name.clone(),
            port: args.port,
            model_label: model_label.clone(),
            location: args.location.clone(),
            uuid: uuid_rx,
        };
        tokio::spawn(crate::dnssd::run(
            cfg,
            dnssd_status.clone(),
            shutdown.clone(),
        ));
    }
    if !is_fake {
        tokio::spawn(crate::ble::adapter_probe_task(
            args.ble.adapter.clone(),
            extra_health.clone(),
            shutdown.clone(),
        ));
    }
    // periodic housekeeping (sticky-error TTL, stale Create-Job)
    {
        let engine = engine.clone();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(15));
            loop {
                tokio::select! {
                    _ = iv.tick() => engine.store().tick(),
                    _ = sd.cancelled() => break,
                }
            }
        });
    }

    let state = Arc::new(AppState {
        ipp: ipp.clone(),
        engine: engine.clone(),
        max_body: args.max_document_mb.max(1) * 1024 * 1024 + 65536,
        dnssd: dnssd_status,
        extra_health,
    });

    // ---- signals
    {
        let sd = shutdown.clone();
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => tracing::info!("SIGTERM: shutting down"),
                _ = int.recv() => tracing::info!("SIGINT: shutting down"),
            }
            engine.begin_shutdown();
            sd.cancel();
        });
    }

    crate::http::run(listener, state, shutdown.clone()).await?;
    // Give the worker time to finish/cancel the current job cleanly.
    match tokio::time::timeout(Duration::from_secs(30), worker).await {
        Ok(_) => tracing::info!("worker stopped"),
        Err(_) => tracing::warn!("worker did not stop in time"),
    }
    Ok(())
}
