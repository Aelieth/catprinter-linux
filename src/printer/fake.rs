//! `--fake-printer DIR`: a printer that "prints" to PNG files, for hardware-free end-to-end tests
//! through real CUPS. Behaviour is scripted by `DIR/state`:
//!   ok (default) | off | no-paper | overheated | low-battery | slow | flaky:N (fail the next N prints)
//! Every print writes `DIR/job-<id>-<slug>.png` (what the head would burn, top-first), `DIR/last.png`,
//! and `DIR/job-<id>.json` (id, name, model, mode, lines, copies, preset, tone, layout).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::models::Family;
use crate::printer::{Condition, Phase, PreparedJob, PrintError, PrintReport, Progress};
use crate::protocol::PrintMode;
use crate::raster::GrayPage;
use crate::render;

pub struct FakePrinter {
    pub dir: PathBuf,
    /// Pretend to be this family (default MXW01: 384 px, 4bpp).
    pub family: Family,
    identify_count: u32,
}

impl FakePrinter {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(FakePrinter {
            dir,
            family: Family::Mxw01,
            identify_count: 0,
        })
    }

    fn state(&self) -> String {
        std::fs::read_to_string(self.dir.join("state"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "ok".into())
    }

    fn set_state(&self, s: &str) {
        let _ = std::fs::write(self.dir.join("state"), s);
    }

    /// Consume one "flaky" credit if present; returns Some(err) if this attempt must fail.
    fn scripted_failure(&self) -> Option<PrintError> {
        let st = self.state();
        match st.as_str() {
            "off" => Some(PrintError::NotFound),
            "no-paper" => Some(PrintError::Condition(cond(
                false,
                "no-paper",
                "The cat printer is out of paper.",
            ))),
            "overheated" => Some(PrintError::Condition(cond(
                false,
                "overheated",
                "The cat printer is too hot. Give it a minute.",
            ))),
            "low-battery" => Some(PrintError::Condition(cond(
                false,
                "low-battery",
                "The cat printer battery is low. Charge it.",
            ))),
            "adapter-off" => Some(PrintError::AdapterOff),
            // test hook: a panic inside the print path must not kill the worker
            "panic" => panic!("fake printer: scripted panic"),
            s if s.starts_with("flaky:") => {
                let n: u32 = s[6..].trim().parse().unwrap_or(0);
                if n == 0 {
                    self.set_state("ok");
                    None
                } else {
                    self.set_state(&format!("flaky:{}", n - 1));
                    Some(PrintError::ConnectFailed {
                        attempts: 3,
                        last: "le-connection-abort-by-local".into(),
                        hint: String::new(),
                    })
                }
            }
            _ => None,
        }
    }

    pub async fn print(
        &mut self,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        let r = self.print_inner(job, cancel, progress).await;
        if matches!(r, Err(PrintError::Cancelled)) {
            // Marker for tests: the driver observed the token and stopped by itself (as opposed
            // to its future being dropped from the outside).
            let _ = std::fs::write(self.dir.join(format!("job-{}-cancelled.txt", job.id)), "");
        }
        r
    }

    async fn print_inner(
        &mut self,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        let started = Instant::now();
        progress(Progress {
            phase: Phase::Searching,
            percent: 0,
            message: "Looking for the cat printer (fake)".into(),
        });
        sleep_cancellable(Duration::from_millis(50), cancel).await?;
        if let Some(e) = self.scripted_failure() {
            return Err(e);
        }
        let caps = match self.family {
            Family::Mxw01 => crate::models::lookup("MXW01").unwrap().caps,
            Family::Classic => crate::models::lookup("GB01").unwrap().caps,
        };
        let mode = render::mode_for(job.opts.tone, caps.grayscale_4bpp);
        progress(Progress {
            phase: Phase::Preparing,
            percent: 5,
            message: "Preparing image (fake)".into(),
        });
        let packed = render::pack(&job.strip, &job.opts, mode, caps.width_px)?;

        let slow = self.state() == "slow";
        let per_copy = if slow {
            Duration::from_secs(15)
        } else {
            Duration::from_millis((200 + packed.lines as u64 * 1000 / 1500).min(3000))
        };
        let copies = job.copies.max(1);
        for c in 0..copies {
            let steps = 5u32;
            for s in 0..steps {
                let pct = ((c * steps + s) * 90 / (copies * steps)) as u8 + 5;
                progress(Progress {
                    phase: Phase::Printing,
                    percent: pct,
                    message: format!("Printing (fake) copy {}/{}", c + 1, copies),
                });
                sleep_cancellable(per_copy / steps, cancel).await?;
            }
        }
        let slug = slugify(&job.name);
        let png = self.dir.join(format!("job-{}-{}.png", job.id, slug));
        write_png(&png, &packed.preview)?;
        let _ = std::fs::copy(&png, self.dir.join("last.png"));
        let meta = serde_json::json!({
            "id": job.id,
            "name": job.name,
            "model": match self.family { Family::Mxw01 => "MXW01 (fake)", Family::Classic => "GB01 (fake)" },
            "mode": match packed.mode { PrintMode::Mono => "1bpp", PrintMode::Gray4 => "4bpp" },
            "width": packed.width,
            "lines": packed.lines,
            "segments": packed.segments.len(),
            "copies": copies,
            "preset": job.opts.preset.name(),
            "tone": job.opts.tone.name(),
            "layout": format!("{:?}", job.strip.layout),
            "pages": job.strip.pages,
            "png": png.file_name().map(|s| s.to_string_lossy().to_string()),
        });
        std::fs::write(
            self.dir.join(format!("job-{}.json", job.id)),
            serde_json::to_vec_pretty(&meta).unwrap(),
        )?;
        progress(Progress {
            phase: Phase::Finishing,
            percent: 100,
            message: "Printed (fake)".into(),
        });
        Ok(PrintReport {
            model: "MXW01 (fake)".into(),
            family: Some(self.family),
            lines: packed.lines,
            segments: packed.segments.len() as u32,
            copies,
            complete_confirmed: true,
            mtu: Some(247),
            battery: Some(88),
            elapsed: started.elapsed(),
        })
    }

    pub async fn status(&mut self, cancel: &CancellationToken) -> Result<Condition, PrintError> {
        sleep_cancellable(Duration::from_millis(20), cancel).await?;
        match self.scripted_failure() {
            None => Ok(cond(true, "", "Printer ready (standby, battery 88%)")),
            Some(PrintError::Condition(c)) => Ok(c),
            Some(e) => Err(e),
        }
    }

    pub async fn identify(&mut self, cancel: &CancellationToken) -> Result<(), PrintError> {
        sleep_cancellable(Duration::from_millis(20), cancel).await?;
        if let Some(e) = self.scripted_failure() {
            return Err(e);
        }
        self.identify_count += 1;
        std::fs::write(
            self.dir
                .join(format!("identify-{}.txt", self.identify_count)),
            "flash\n",
        )?;
        Ok(())
    }
}

fn cond(ok: bool, error: &str, message: &str) -> Condition {
    Condition {
        ok,
        state: if ok { "standby".into() } else { "error".into() },
        battery: Some(88),
        temperature: Some(30),
        error: if error.is_empty() {
            None
        } else {
            Some(error.into())
        },
        message: message.into(),
    }
}

async fn sleep_cancellable(d: Duration, cancel: &CancellationToken) -> Result<(), PrintError> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = cancel.cancelled() => Err(PrintError::Cancelled),
    }
}

fn slugify(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    let s: String = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        "job".into()
    } else {
        s.chars().take(40).collect()
    }
}

/// Write an 8-bit gray PNG.
pub fn write_png(path: &Path, page: &GrayPage) -> Result<(), PrintError> {
    let img = image::GrayImage::from_raw(page.width, page.height, page.data.clone())
        .ok_or_else(|| PrintError::Bus("preview buffer size mismatch".into()))?;
    img.save_with_format(path, image::ImageFormat::Png)
        .map_err(|e| PrintError::Bus(format!("write png: {e}")))?;
    Ok(())
}
