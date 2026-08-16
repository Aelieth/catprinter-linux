//! Subcommand entry points. Each returns a process exit code.

use std::path::Path;

use crate::config::{CheckArgs, EnsureQueueArgs, PrintArgs, ServeArgs, BleArgs};

pub async fn serve(args: ServeArgs) -> i32 {
    match crate::http::serve(args).await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    }
}

pub async fn check(_args: CheckArgs) -> i32 {
    eprintln!("check: not implemented yet");
    2
}

pub async fn status(_args: BleArgs) -> i32 {
    eprintln!("status: not implemented yet");
    2
}

pub async fn print(_args: PrintArgs) -> i32 {
    eprintln!("print: not implemented yet");
    2
}

pub async fn inspect(file: &Path) -> i32 {
    match std::fs::read(file) {
        Ok(bytes) => match crate::raster::inspect(&bytes) {
            Ok(headers) => {
                for (i, h) in headers.iter().enumerate() {
                    println!(
                        "page {}: {}x{} px, {} bpp (color {} bit), colorspace {}, order {}, {}x{} dpi, copies {}, size {:?} pt, name {:?}, {:?}",
                        i + 1, h.width, h.height, h.bits_per_pixel, h.bits_per_color, h.color_space, h.color_order,
                        h.hw_resolution.0, h.hw_resolution.1, h.num_copies, h.page_size_pt, h.page_size_name, h.sync
                    );
                }
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        Err(e) => {
            eprintln!("{}: {e}", file.display());
            1
        }
    }
}

pub async fn ensure_queue(_args: EnsureQueueArgs) -> i32 {
    eprintln!("ensure-queue: not implemented yet");
    2
}
