//! MXW01 model driver: the A2/A1/A9/AE03/AD/AA print sequence over a connected `Session`.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ble::session::Session;
use crate::printer::{Condition, Phase, PrintError, Progress};
use crate::protocol::mxw01 as proto;
use crate::render::Packed;

/// Run one print (all copies handled by the caller). Returns whether the printer confirmed
/// completion (AA) and the battery percent seen in the status check.
pub async fn print(
    session: &Session,
    packed: &Packed,
    intensity: u8,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<(bool, Option<u8>), PrintError> {
    let row_bytes = proto::bytes_per_row(packed.mode);

    // A2 set intensity.
    session
        .write_ctrl(&proto::cmd_set_intensity(intensity))
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A1 status — abort if not ready.
    let status = get_status(session).await?;
    if !status.ok {
        return Err(PrintError::Condition(condition(&status)));
    }
    let battery = status.battery;

    let mut confirmed_all = true;
    let total: u32 = packed.segments.iter().map(|s| s.lines).sum::<u32>().max(1);
    let mut done: u32 = 0;
    for (i, seg) in packed.segments.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(PrintError::Cancelled);
        }
        let bytes = &packed.data[seg.bytes.clone()];
        // A9 print request; wait for the ack.
        let ack = session
            .request(
                &proto::cmd_print_request(seg.lines as u16, packed.mode),
                proto::Cmd::Print as u8,
                Duration::from_secs(proto::NOTIFICATION_TIMEOUT_S),
            )
            .await?;
        if ack.first().is_some_and(|b| *b != 0) {
            return Err(PrintError::Rejected(
                ack.iter().map(|b| format!("{b:02x}")).collect(),
            ));
        }

        // Arm AA before flushing, then stream + flush.
        let mut aa_rx = session.subscribe();
        session.write_bulk(bytes, row_bytes, cancel).await?;
        session.write_ctrl(&proto::cmd_flush()).await?;

        let timeout = proto::print_complete_timeout(seg.lines);
        let wait_aa = async {
            loop {
                match aa_rx.recv().await {
                    Ok(raw) => {
                        if let Some(f) = proto::parse_notification(&raw) {
                            if f.cmd == proto::Cmd::PrintComplete as u8 {
                                return true;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return false,
                }
            }
        };
        match tokio::time::timeout(timeout, wait_aa).await {
            Ok(true) => {}
            Ok(false) => return Err(PrintError::LinkLost),
            Err(_) => {
                tracing::warn!(
                    "no print-complete for segment {}/{}; it may still have printed",
                    i + 1,
                    packed.segments.len()
                );
                confirmed_all = false;
            }
        }
        done += seg.lines;
        let pct = (done * 95 / total).min(95) as u8;
        progress(Progress {
            phase: Phase::Printing,
            percent: pct,
            message: format!("Printing ({done}/{total} lines)"),
        });
    }
    progress(Progress {
        phase: Phase::Finishing,
        percent: 100,
        message: "Printed".into(),
    });
    Ok((confirmed_all, battery))
}

pub async fn get_status(session: &Session) -> Result<proto::PrinterStatus, PrintError> {
    let payload = session
        .request(
            &proto::cmd_get_status(),
            proto::Cmd::GetStatus as u8,
            Duration::from_secs(proto::NOTIFICATION_TIMEOUT_S),
        )
        .await?;
    Ok(proto::parse_status(&payload))
}

pub async fn status(session: &Session) -> Result<Condition, PrintError> {
    Ok(condition(&get_status(session).await?))
}

pub async fn identify(session: &Session) -> Result<(), PrintError> {
    session.write_ctrl(&proto::cmd_eject(40)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

fn condition(s: &proto::PrinterStatus) -> Condition {
    Condition {
        ok: s.ok,
        state: s.state.name(),
        battery: s.battery,
        temperature: s.temperature,
        error: s.error_key().map(String::from),
        message: s.kid_message(),
    }
}
