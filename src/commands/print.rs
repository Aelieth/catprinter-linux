//! `catprinterd print FILE`: render a PWG raster or image and print it straight over Bluetooth
//! (hardware bring-up), or `--preview-only` to write what would be printed to a PNG.

use tokio_util::sync::CancellationToken;

use crate::ble::BlePrinter;
use crate::config::PrintArgs;
use crate::printer::fake::write_png;
use crate::printer::{PreparedJob, Printer, Progress};
use crate::raster::{self, Limits};
use crate::render::{self, Dither, Layout, Preset, RenderOptions, Tone};

pub async fn run(args: PrintArgs) -> i32 {
    let (preset, tone) = match args.quality.trim().to_ascii_lowercase().as_str() {
        "draft" | "text" => (Preset::Text, Tone::BlackWhite),
        "high" | "picture" | "photo" => (Preset::Picture, Tone::Grayscale),
        _ => (Preset::Default, Tone::BlackWhite),
    };
    let tone = if args.bi_level {
        Tone::BlackWhite
    } else {
        tone
    };

    let bytes = match std::fs::read(&args.file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}: {e}", args.file.display());
            return 1;
        }
    };

    // Decode input → gray pages.
    let (pages, is_image) = if raster::is_raster(&bytes) {
        match raster::decode(&bytes, &Limits::default()) {
            Ok(p) => (p, false),
            Err(e) => {
                eprintln!("raster: {e}");
                return 1;
            }
        }
    } else if render::imagein::is_image(&bytes) {
        match render::imagein::load(&bytes) {
            Ok(p) => (vec![p], true),
            Err(e) => {
                eprintln!("image: {e}");
                return 1;
            }
        }
    } else {
        eprintln!("unsupported file (need PNG, JPEG, or PWG raster)");
        return 1;
    };

    let dither_override = args.dither.as_deref().and_then(Dither::parse);
    let opts = RenderOptions {
        preset,
        tone,
        layout: if args.sheet {
            Layout::Sheet
        } else if is_image {
            Layout::Tape
        } else {
            Layout::Auto
        },
        dither_override,
        intensity_override: args.intensity,
        rotate_180: !args.top_first,
        max_lines_total: args.max_lines,
        ..RenderOptions::default()
    };

    let strip = match render::prepare(pages, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // Preview only: pack for a 4bpp-capable head at 384 and write the preview PNG.
    if let Some(out) = &args.preview_only {
        let mode = render::mode_for(tone, true);
        match render::pack(&strip, &opts, mode, 384) {
            Ok(packed) => match write_png(out, &packed.preview) {
                Ok(()) => {
                    println!("wrote {} ({} lines)", out.display(), packed.lines);
                    return 0;
                }
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            },
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    }

    let job = PreparedJob {
        id: 0,
        name: args
            .file
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "print".into()),
        strip,
        opts,
        copies: args.copies.max(1),
    };
    let mut printer = Printer::Ble(BlePrinter {
        device_hint: args.ble.device.clone(),
        adapter: args.ble.adapter.clone(),
        forced_family: args.ble.model.family(),
        slow: args.ble.slow,
        pacing_ms: args.ble.pacing_ms,
    });
    let cancel = CancellationToken::new();
    // Cancel on Ctrl-C so the Drop guard disconnects.
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\ncancelling…");
            cancel.cancel();
        });
    }
    let mut last = 255u8;
    let mut progress = move |p: Progress| {
        if p.percent != last {
            eprintln!("  {:>3}%  {}", p.percent, p.message);
            last = p.percent;
        }
    };
    match printer.print(&job, &cancel, &mut progress).await {
        Ok(rep) => {
            eprintln!(
                "printed on {} ({} lines, MTU {}{})",
                rep.model,
                rep.lines,
                rep.mtu.unwrap_or(0),
                if rep.complete_confirmed {
                    ""
                } else {
                    ", unconfirmed"
                }
            );
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
