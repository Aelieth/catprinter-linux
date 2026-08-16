//! Bluetooth transport over BlueZ's D-Bus API (zbus).
//!
//! Per print job: find the printer (cache-first, then LE scan), connect, detect the model, pack the
//! strip for that model, drive the family sequence for each copy, then always disconnect.

pub mod bluez;
pub mod discovery;
pub mod seqpacket;
pub mod session;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use zbus::Connection;

use crate::ble::discovery::{Candidate, DeviceHint};
use crate::ble::session::Session;
use crate::config::NotifyMode;
use crate::models::Family;
use crate::printer::{Condition, Phase, PreparedJob, PrintError, PrintReport, Progress};
use crate::render;

/// Real printer: discovery → connect → detect model → pack → drive → disconnect, per job.
pub struct BlePrinter {
    pub device_hint: Option<String>,
    pub adapter: Option<String>,
    pub forced_family: Option<Family>,
    pub slow: bool,
    pub pacing_ms: u64,
}

impl BlePrinter {
    fn hint(&self) -> Option<DeviceHint> {
        self.device_hint.as_deref().map(DeviceHint::parse)
    }

    /// Find and connect to the printer. Returns the session and whether the target was cached-connected.
    async fn open(
        &self,
        conn: &Connection,
        notify_mode: NotifyMode,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Session, PrintError> {
        let hint = self.hint();
        let objs = bluez::managed_objects(conn).await?;
        let adapter = discovery::choose_adapter(&objs, self.adapter.as_deref())?;

        progress(Progress {
            phase: Phase::Searching,
            percent: 0,
            message: "Looking for the cat printer".into(),
        });

        // Cache-first: is it already known/connected?
        let cached = discovery::candidates_from(&objs, &adapter.path, hint.as_ref());
        if let Some(best) = discovery::pick(&cached) {
            let was_connected = best.connected;
            tracing::info!(
                "found {} in the Bluetooth cache ({})",
                best.label(),
                if was_connected { "connected" } else { "known" }
            );
            progress(Progress {
                phase: Phase::Connecting,
                percent: 0,
                message: "Connecting".into(),
            });
            match Session::connect(
                conn,
                best,
                was_connected,
                self.forced_family,
                notify_mode,
                self.pacing_ms,
                self.slow,
            )
            .await
            {
                Ok(s) => return Ok(s),
                Err(e) if was_connected => return Err(e),
                Err(e) => tracing::info!("cached printer did not answer ({e}); scanning"),
            }
        }

        // Scan.
        progress(Progress {
            phase: Phase::Searching,
            percent: 0,
            message: "Scanning for the cat printer".into(),
        });
        let target: Candidate = discovery::scan(
            conn,
            &adapter,
            hint.as_ref(),
            discovery::default_scan_timeout(),
        )
        .await?;
        progress(Progress {
            phase: Phase::Connecting,
            percent: 0,
            message: "Connecting".into(),
        });
        let result = Session::connect(
            conn,
            &target,
            false,
            self.forced_family,
            notify_mode,
            self.pacing_ms,
            self.slow,
        )
        .await;
        discovery::stop_scan(conn, &adapter.path).await;
        result
    }

    pub async fn print(
        &mut self,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        let started = Instant::now();
        let conn = bluez::system_bus().await?;
        let session = self.open(&conn, NotifyMode::Props, progress).await?;

        let result = self.drive(&session, job, cancel, progress).await;
        // Always disconnect, even on error/cancel.
        session.close().await;
        let (confirmed, battery, model, family, mtu, lines, segments) = result?;
        Ok(PrintReport {
            model,
            family: Some(family),
            lines,
            segments,
            copies: job.copies.max(1),
            complete_confirmed: confirmed,
            mtu: Some(mtu),
            battery,
            elapsed: started.elapsed(),
        })
    }

    #[allow(clippy::type_complexity)]
    async fn drive(
        &self,
        session: &Session,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(bool, Option<u8>, String, Family, u16, u32, u32), PrintError> {
        progress(Progress {
            phase: Phase::Preparing,
            percent: 0,
            message: "Preparing image".into(),
        });
        let mode = render::mode_for(job.opts.tone, session.caps.grayscale_4bpp);
        let packed = render::pack(&job.strip, &job.opts, mode, session.caps.width_px)?;
        let intensity = job.opts.intensity();
        let copies = job.copies.max(1);
        let mut confirmed = true;
        let mut battery = None;
        for c in 0..copies {
            if cancel.is_cancelled() {
                return Err(PrintError::Cancelled);
            }
            if copies > 1 {
                progress(Progress {
                    phase: Phase::Printing,
                    percent: (c * 100 / copies) as u8,
                    message: format!("Copy {}/{}", c + 1, copies),
                });
            }
            let (ok, batt) = match session.family {
                Family::Mxw01 => {
                    crate::models::mxw01::print(session, &packed, intensity, cancel, progress)
                        .await?
                }
                Family::Classic => {
                    crate::models::classic::print(session, &packed, intensity, cancel, progress)
                        .await?
                }
            };
            confirmed &= ok;
            battery = batt.or(battery);
        }
        Ok((
            confirmed,
            battery,
            session.model_label.clone(),
            session.family,
            session.mtu(),
            packed.lines,
            packed.segments.len() as u32,
        ))
    }

    pub async fn status(&mut self, _cancel: &CancellationToken) -> Result<Condition, PrintError> {
        let conn = bluez::system_bus().await?;
        let mut noop = |_: Progress| {};
        let session = self.open(&conn, NotifyMode::Props, &mut noop).await?;
        let result = match session.family {
            Family::Mxw01 => crate::models::mxw01::status(&session).await,
            Family::Classic => crate::models::classic::status(&session).await,
        };
        session.close().await;
        result
    }

    pub async fn identify(&mut self, _cancel: &CancellationToken) -> Result<(), PrintError> {
        let conn = bluez::system_bus().await?;
        let mut noop = |_: Progress| {};
        let session = self.open(&conn, NotifyMode::Props, &mut noop).await?;
        let result = match session.family {
            Family::Mxw01 => crate::models::mxw01::identify(&session).await,
            Family::Classic => crate::models::classic::identify(&session).await,
        };
        session.close().await;
        result
    }
}

/// Periodically report adapter state into /health (bluetoothd reachable, adapter, powered).
pub async fn adapter_probe_task(
    adapter: Option<String>,
    extra: Arc<RwLock<serde_json::Value>>,
    shutdown: CancellationToken,
) {
    let mut iv = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = iv.tick() => {},
            _ = shutdown.cancelled() => break,
        }
        let bt = probe(adapter.as_deref()).await;
        if let Ok(mut v) = extra.write() {
            if let serde_json::Value::Object(map) = &mut *v {
                map.insert("bluetooth".into(), bt);
            } else {
                *v = serde_json::json!({ "bluetooth": bt });
            }
        }
    }
}

async fn probe(want: Option<&str>) -> serde_json::Value {
    let conn = match tokio::time::timeout(Duration::from_secs(2), Connection::system()).await {
        Ok(Ok(c)) => c,
        _ => return serde_json::json!({ "bluetoothd": false }),
    };
    let objs = match bluez::managed_objects(&conn).await {
        Ok(o) => o,
        Err(_) => {
            return serde_json::json!({ "bluetoothd": true, "adapter": serde_json::Value::Null })
        }
    };
    match discovery::choose_adapter(&objs, want) {
        Ok(a) => {
            serde_json::json!({ "bluetoothd": true, "adapter": a.path, "powered": a.powered, "address": a.address, "name": a.name })
        }
        Err(PrintError::AdapterOff) => {
            let any = discovery::adapters_from(&objs).into_iter().next();
            serde_json::json!({ "bluetoothd": true, "adapter": any.as_ref().map(|a| a.path.clone()), "powered": false })
        }
        Err(_) => serde_json::json!({ "bluetoothd": true, "adapter": serde_json::Value::Null }),
    }
}
