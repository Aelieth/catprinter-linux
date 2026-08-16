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

/// Map a burn intensity (0..=255, MXW01-style) to a classic energy word (u16). Only used when
/// the user explicitly overrides the intensity; scale linearly so higher intensity → more energy.
pub fn energy_for(intensity: u8) -> u16 {
    // 0x5D (93) → ~0x5D00; 0xFF → 0xFF00-ish, clamped below 0xFFFF.
    ((intensity as u32 * 0xFF00 / 0xFF) as u16).max(0x2000)
}

/// The energy word actually sent: upstream's default 0xFFFF (`cmds_print_img(img, energy=0xffff)`,
/// cmds.py:188) unless `--intensity` was explicitly given, in which case it maps through
/// [`energy_for`].
pub fn energy(intensity_override: Option<u8>) -> u16 {
    intensity_override
        .map(energy_for)
        .unwrap_or(proto::DEFAULT_ENERGY)
}

/// Bytes per BLE write of the AE01 stream: `mtu - 3`, exactly like upstream
/// (`chunk_size = client.mtu_size - 3`, ble.py:100), floored at BLE 4.0's 20-byte minimum —
/// BlueZ often leaves the MTU property stuck at 23. `--slow` shrinks to exactly 20 bytes,
/// never below (per-byte writes would confuse the framing on some units).
pub fn chunk_size(mtu: u16, slow: bool) -> usize {
    if slow {
        20
    } else {
        (mtu as usize).saturating_sub(3).max(20)
    }
}

/// Delay after every chunk: at least upstream's `WAIT_AFTER_EACH_CHUNK_S` (20 ms, ble.py:28,112),
/// more when the user configured extra pacing.
pub fn chunk_pacing_ms(configured_ms: u64) -> u64 {
    configured_ms.max(proto::WAIT_AFTER_CHUNK_MS)
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
    intensity_override: Option<u8>,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<(bool, Option<u8>), PrintError> {
    if packed.mode != crate::protocol::PrintMode::Mono {
        return Err(PrintError::NotCatPrinter(
            "this printer only does black and white".into(),
        ));
    }
    let rows = rows_from_packed(packed);
    let stream = proto::print_stream(
        rows.iter().map(|r| r.as_slice()),
        energy(intensity_override),
        proto::DEFAULT_FEED_AFTER,
    );

    progress(Progress {
        phase: Phase::Printing,
        percent: 5,
        message: "Sending image".into(),
    });
    // Arm the ready-notification watcher BEFORE the first write (upstream arms it before the
    // stream too, ble.py:105) so a ping landing during the tail of the stream is not missed.
    let mut ready_rx = session.subscribe();

    // One plain GATT write of `mtu - 3` bytes per chunk with a pause after each — byte- and
    // timing-faithful to upstream ble.py:100,110-112. The stream is a command sequence, so
    // chunks fall on arbitrary byte boundaries (frames span writes; the printer reassembles).
    let chunk = chunk_size(session.mtu(), session.slow);
    let pace = Duration::from_millis(chunk_pacing_ms(session.pacing_ms));
    let total = stream.len().max(1);
    let mut sent = 0usize;
    for piece in stream.chunks(chunk) {
        if cancel.is_cancelled() {
            return Err(PrintError::Cancelled);
        }
        session.write_data(piece).await?;
        sent += piece.len();
        // Upstream sleeps after EVERY chunk, the last one included (ble.py:112).
        tokio::time::sleep(pace).await;
        let pct = (sent * 90 / total).min(90) as u8 + 5;
        progress(Progress {
            phase: Phase::Printing,
            percent: pct,
            message: "Printing".into(),
        });
    }

    let confirmed = match Session::wait_raw_on(
        &mut ready_rx,
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
    progress(Progress {
        phase: Phase::Finishing,
        percent: 100,
        message: "Printed".into(),
    });
    Ok((confirmed, None))
}

pub async fn status(session: &Session) -> Result<Condition, PrintError> {
    // Best-effort: the A3 reply layout is not reliably known across the classic models, so it is
    // NEVER turned into a not-ready verdict — a mis-parse must not stall a job with a phantom
    // "no paper". The reply (if any) is only logged for debugging.
    let mut rx = session.subscribe();
    let _ = session.write_ctrl(&proto::cmd_get_dev_state()).await;
    if let Ok(raw) = Session::wait_raw_on(&mut rx, Duration::from_millis(1500), |raw| {
        proto::parse_frame(raw).is_some_and(|(cmd, _)| cmd == proto::cmd::GET_DEV_STATE)
    })
    .await
    {
        tracing::debug!("classic A3 state reply: {raw:02x?}");
    }
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

    #[test]
    fn energy_defaults_to_upstream_ffff() {
        // Upstream: `def cmds_print_img(img, energy: int = 0xffff)` (cmds.py:188).
        assert_eq!(energy(None), 0xFFFF);
        assert_eq!(energy(None), proto::DEFAULT_ENERGY);
        // An explicit --intensity still maps through energy_for.
        assert_eq!(energy(Some(0x5D)), energy_for(0x5D));
        assert_eq!(energy(Some(0xFF)), 0xFF00);
    }

    #[test]
    fn chunk_plan_matches_upstream_mtu_rule() {
        // Upstream: `chunk_size = client.mtu_size - 3` (ble.py:100). Default BlueZ MTU 23 → 20.
        assert_eq!(chunk_size(23, false), 20);
        assert_eq!(chunk_size(512, false), 509);
        // A bogus / absent MTU property never chunks below BLE 4.0's 20-byte floor.
        assert_eq!(chunk_size(0, false), 20);
        assert_eq!(chunk_size(10, false), 20);
        // --slow shrinks to exactly 20 bytes, never per-byte writes.
        assert_eq!(chunk_size(512, true), 20);
        assert_eq!(chunk_size(23, true), 20);
    }

    #[test]
    fn chunk_pacing_never_faster_than_upstream() {
        // WAIT_AFTER_EACH_CHUNK_S = 0.02 (ble.py:28): the default 8 ms pacing is raised to 20 ms.
        assert_eq!(chunk_pacing_ms(8), 20);
        assert_eq!(chunk_pacing_ms(0), 20);
        assert_eq!(chunk_pacing_ms(20), 20);
        assert_eq!(chunk_pacing_ms(50), 50);
    }

    #[test]
    fn classic_pack_skips_the_90_row_minimum() {
        use crate::protocol::PrintMode;
        use crate::render::{pack, pack_padded, GrayStrip, Layout, RenderOptions};
        let strip = GrayStrip {
            width: 384,
            height: 10,
            data: vec![0u8; 384 * 10], // all black so every row carries ink
            pages: 1,
            layout: Layout::Tape,
        };
        let opts = RenderOptions::default();
        // Classic (pad_to_min = false): exactly the image rows, no blank padding before the feed.
        let p = pack_padded(&strip, &opts, PrintMode::Mono, 384, false).unwrap();
        assert_eq!(p.lines, 10);
        assert_eq!(p.data.len(), 10 * 48);
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.segments[0].lines, 10);
        assert_eq!(rows_from_packed(&p).len(), 10);
        // MXW01 (pack = pad_to_min true) still pads the same strip to the protocol minimum.
        let m = pack(&strip, &opts, PrintMode::Mono, 384).unwrap();
        assert_eq!(m.lines, 90);
        assert_eq!(m.data.len(), 90 * 48);
        assert_eq!(rows_from_packed(&m).len(), 90);
    }
}
