//! `catprinterd status`: connect, report model / battery / paper. Exit 0 ok, 2 not ready, 3 not found.

use tokio_util::sync::CancellationToken;

use crate::ble::BlePrinter;
use crate::config::BleArgs;
use crate::printer::PrintError;

pub async fn run(args: BleArgs) -> i32 {
    let mut printer = BlePrinter {
        device_hint: args.device.clone(),
        adapter: args.adapter.clone(),
        forced_family: args.model.family(),
        slow: args.slow,
        pacing_ms: args.pacing_ms,
    };
    let cancel = CancellationToken::new();
    match printer.status(&cancel).await {
        Ok(cond) => {
            println!("state    : {}", cond.state);
            if let Some(b) = cond.battery {
                println!("battery  : {b}%");
            }
            if let Some(t) = cond.temperature {
                println!("temp     : {t}");
            }
            println!("message  : {}", cond.message);
            if cond.ok {
                0
            } else {
                2
            }
        }
        Err(PrintError::NotFound) => {
            eprintln!("Cat printer not found — turn it on and keep it near the computer.");
            3
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}
