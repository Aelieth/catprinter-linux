//! MXW01 model driver: the A2/A1/A9/AE03/AD/AA print sequence over a connected `Session`.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ble::session::Session;
use crate::printer::{Condition, Phase, PrintError, Progress};
use crate::protocol::mxw01 as proto;
use crate::render::Packed;

/// Once image data has reached the printer, a link-shaped failure must not be blindly retried —
/// the part already written comes out of the head again on the next attempt. Map those errors to
/// the non-retryable `Interrupted`; before any data is written they stay as-is (retryable).
pub(crate) fn interrupted(e: PrintError, lines_sent: u32, any_data_sent: bool) -> PrintError {
    if !any_data_sent {
        return e;
    }
    match e {
        PrintError::LinkLost
        | PrintError::Bus(_)
        | PrintError::Timeout(_)
        | PrintError::NoAnswer(_) => PrintError::Interrupted { lines_sent },
        other => other,
    }
}

/// A9 line counts are u16 on the wire; a larger segment is a render-side bug, not a link problem.
fn seg_line_count(lines: u32) -> Result<u16, PrintError> {
    u16::try_from(lines).map_err(|_| {
        PrintError::Render(crate::render::RenderError::Internal(format!(
            "segment of {lines} lines exceeds the protocol limit of 65535"
        )))
    })
}

/// The A9 ack: accepted only when the first byte exists and is zero. An EMPTY ack is a reject
/// too (Python: `if not ack or ack[0] != 0`).
fn check_print_ack(ack: &[u8]) -> Result<(), PrintError> {
    match ack.first() {
        Some(0) => Ok(()),
        _ => Err(PrintError::Rejected(if ack.is_empty() {
            "empty ack".into()
        } else {
            ack.iter().map(|b| format!("{b:02x}")).collect()
        })),
    }
}

/// Poll A1 until the printer is back in Standby (≤ 5 s, every 250 ms). After an AA the head can
/// still be feeding; an A9 sent while it runs is rejected or mis-timed on some units. Gives up
/// silently after the deadline — the A9 ack still gates the actual print.
pub async fn wait_standby(session: &Session) -> Result<(), PrintError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = get_status(session).await?;
        if status.state == proto::State::Standby {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::debug!(
                "printer still {} 5 s after the last segment; proceeding",
                status.state.name()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

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
    // AE03 bytes that actually reached the printer, across all segments.
    let mut sent_bytes: usize = 0;
    for (i, seg) in packed.segments.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(PrintError::Cancelled);
        }
        // Between segments: let the head finish the previous strip before the next A9.
        if i > 0 {
            if let Err(e) = wait_standby(session).await {
                return Err(interrupted(e, done, sent_bytes > 0));
            }
        }
        let bytes = &packed.data[seg.bytes.clone()];
        // A9 print request; wait for the ack.
        let lines = seg_line_count(seg.lines)?;
        let ack = match session
            .request(
                &proto::cmd_print_request(lines, packed.mode),
                proto::Cmd::Print as u8,
                Duration::from_secs(proto::NOTIFICATION_TIMEOUT_S),
            )
            .await
        {
            Ok(a) => a,
            Err(e) => return Err(interrupted(e, done, sent_bytes > 0)),
        };
        check_print_ack(&ack)?;

        // Arm AA before flushing, then stream + flush.
        let mut aa_rx = session.subscribe();
        let seg_start = sent_bytes;
        if let Err(e) = session
            .write_bulk(bytes, row_bytes, &mut sent_bytes, cancel)
            .await
        {
            let lines_sent = done + ((sent_bytes - seg_start) / row_bytes) as u32;
            return Err(interrupted(e, lines_sent, sent_bytes > 0));
        }
        if let Err(e) = session.write_ctrl(&proto::cmd_flush()).await {
            return Err(interrupted(e, done + seg.lines, sent_bytes > 0));
        }

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
            Ok(false) => {
                return Err(interrupted(
                    PrintError::LinkLost,
                    done + seg.lines,
                    sent_bytes > 0,
                ))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_a9_ack_is_a_reject() {
        assert!(
            matches!(check_print_ack(&[]), Err(PrintError::Rejected(s)) if s == "empty ack"),
            "empty ack must reject (Python: `if not ack or ack[0] != 0`)"
        );
        assert!(matches!(
            check_print_ack(&[1]),
            Err(PrintError::Rejected(_))
        ));
        assert!(check_print_ack(&[0]).is_ok());
        assert!(check_print_ack(&[0, 9]).is_ok());
    }

    #[test]
    fn segment_line_count_is_u16_checked() {
        assert_eq!(seg_line_count(1).unwrap(), 1);
        assert_eq!(seg_line_count(65535).unwrap(), 65535);
        assert!(matches!(seg_line_count(65536), Err(PrintError::Render(_))));
        assert!(matches!(
            seg_line_count(u32::MAX),
            Err(PrintError::Render(_))
        ));
    }

    #[test]
    fn interrupted_only_after_data_was_sent() {
        // Before any data: errors stay as-is (retryable).
        assert!(matches!(
            interrupted(PrintError::LinkLost, 0, false),
            PrintError::LinkLost
        ));
        // After data: link-shaped errors become the non-retryable Interrupted{lines_sent}.
        let e = interrupted(PrintError::LinkLost, 42, true);
        assert!(matches!(e, PrintError::Interrupted { lines_sent: 42 }));
        assert!(!e.retryable());
        assert!(matches!(
            interrupted(PrintError::NoAnswer("x"), 7, true),
            PrintError::Interrupted { lines_sent: 7 }
        ));
        assert!(matches!(
            interrupted(PrintError::Timeout("x"), 7, true),
            PrintError::Interrupted { .. }
        ));
        assert!(matches!(
            interrupted(PrintError::Bus("x".into()), 7, true),
            PrintError::Interrupted { .. }
        ));
        // Cancel / reject / condition pass through untouched.
        assert!(matches!(
            interrupted(PrintError::Cancelled, 7, true),
            PrintError::Cancelled
        ));
        assert!(matches!(
            interrupted(PrintError::Rejected("01".into()), 7, true),
            PrintError::Rejected(_)
        ));
    }
}
