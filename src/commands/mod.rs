//! Subcommand entry points. Each returns a process exit code.

use std::path::Path;

use crate::config::{BleArgs, CheckArgs, EnsureQueueArgs, PrintArgs, ServeArgs};

pub mod check;
pub mod print;
pub mod serve;
pub mod status;

pub async fn serve(args: ServeArgs) -> i32 {
    match serve::run(args).await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    }
}

pub async fn check(args: CheckArgs) -> i32 {
    check::run(args).await
}

pub async fn status(args: BleArgs) -> i32 {
    status::run(args).await
}

pub async fn print(args: PrintArgs) -> i32 {
    print::run(args).await
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

pub mod ensure_queue;

pub async fn ensure_queue(args: EnsureQueueArgs) -> i32 {
    ensure_queue::ensure_queue(args).await
}
