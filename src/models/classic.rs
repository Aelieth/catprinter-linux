//! Classic-family model driver (GB0x/GT01/MX0x/YT01/X5/X6). One byte stream to AE01, then wait for
//! the ready notification. There is no per-segment protocol, so all rows go out as one stream.
//!
//! Not hardware-verified by this project (only an MXW01 is on hand) — best-effort per the reference.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ble::session::Session;
use crate::printer::{Condition, Phase, PrintError, Progress};
use crate::protocol::classic as proto;
use crate::render::Packed;

/// Map a burn intensity (0..=255, MXW01-style) to a classic energy word (u16). The reference uses
/// 0xFFFF as a strong default; scale linearly so higher intensity → more energy, capping at 0xFFFF.
pub fn energy_for(intensity: u8) -> u16 {
    // 0x5D (93) → ~0x5D00; 0xFF → 0xFF00-ish, clamped below 0xFFFF.
    ((intensity as u32 * 0xFF00 / 0xFF) as u16).max(0x2000)
}

/// Unpack the packed 1bpp rows of a segment (48 bytes/row, LSB = leftmost) back into per-row bool
/// vectors for the RLE encoder.
fn rows_from_packed(packed: &Packed) -> Vec<Vec<bool>> {
    let rb = 48usize;
    let mut rows = Vec::new();
    for seg in &packed.segments {
        let bytes = &packed.data[seg.bytes.clone()];
        for row in bytes.chunks_exact(rb) {
            let mut bits = Vec::with_capacity(384);
            for &b in row {
                for bit in 0..8 {
                    bits.push((b >> bit) & 1 == 1);
                }
            }
            rows.push(bits);
        }
    }
    rows
}

pub async fn print(
    session: &Session,
    packed: &Packed,
    intensity: u8,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<(bool, Option<u8>), PrintError> {
    if packed.mode != crate::protocol::PrintMode::Mono {
        return Err(PrintError::NotCatPrinter(
            "this printer only does black and white".into(),
        ));
    }
    let rows = rows_from_packed(packed);
    let energy = energy_for(intensity);
    let stream = proto::print_stream(
        rows.iter().map(|r| r.as_slice()),
        energy,
        proto::DEFAULT_FEED_AFTER,
    );

    progress(Progress {
        phase: Phase::Printing,
        percent: 5,
        message: "Sending image".into(),
    });
    // Arm the ready-notification watcher before writing the stream.
    let mut rx = session.subscribe();

    // Classic printers stream to the control characteristic (AE01) — use write_bulk which chunks to MTU.
    // Chunk on arbitrary byte boundaries (the stream is a command sequence, not fixed rows), so pass row_bytes=1.
    let total = stream.len().max(1);
    let chunk = (session.mtu() as usize).saturating_sub(3).max(20);
    let mut sent = 0usize;
    for piece in stream.chunks(chunk) {
        if cancel.is_cancelled() {
            return Err(PrintError::Cancelled);
        }
        session.write_bulk(piece, 1, cancel).await?;
        sent += piece.len();
        let pct = (sent * 90 / total).min(90) as u8 + 5;
        progress(Progress {
            phase: Phase::Printing,
            percent: pct,
            message: "Printing".into(),
        });
    }

    let confirmed = match session
        .wait_raw(
            Duration::from_secs(proto::WAIT_FOR_READY_TIMEOUT_S),
            proto::is_ready_notification,
        )
        .await
    {
        Ok(_) => true,
        Err(_) => {
            tracing::warn!("no ready notification from classic printer; it may still have printed");
            false
        }
    };
    let _ = &mut rx;
    progress(Progress {
        phase: Phase::Finishing,
        percent: 100,
        message: "Printed".into(),
    });
    Ok((confirmed, None))
}

pub async fn status(session: &Session) -> Result<Condition, PrintError> {
    // Best-effort: ask for device state; we don't reliably know the reply layout across models.
    let _ = session.write_ctrl(&proto::cmd_get_dev_state()).await;
    Ok(Condition {
        ok: true,
        state: "ready".into(),
        battery: None,
        temperature: None,
        error: None,
        message: "Cat printer ready (classic family)".into(),
    })
}

pub async fn identify(session: &Session) -> Result<(), PrintError> {
    session.write_ctrl(&proto::cmd_feed_paper(40)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_scales_and_clamps() {
        assert!(energy_for(0x5D) > 0x5000 && energy_for(0x5D) < 0x6000);
        assert_eq!(energy_for(0xFF), 0xFF00);
        assert_eq!(energy_for(0), 0x2000);
    }
}
