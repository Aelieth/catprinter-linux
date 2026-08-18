//! `catprinterd doctor` / `doctor --json`.

use crate::adopt;
use crate::config::DoctorArgs;

pub async fn run(args: DoctorArgs) -> i32 {
    let dir = adopt::resolve_store_dir(args.state_dir.as_deref());
    let report =
        crate::doctor::collect(args.port, &args.queue, args.adapter.as_deref(), &dir).await;
    if args.json {
        println!("{}", report.to_json());
    } else {
        report.print_prose();
    }
    0
}
