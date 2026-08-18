//! `catprinterd doctor` / `doctor --json`.

use crate::config::DoctorArgs;

pub async fn run(args: DoctorArgs) -> i32 {
    let report = crate::doctor::collect(
        args.port,
        &args.queue,
        args.adapter.as_deref(),
        args.state_dir.as_deref(),
    )
    .await;
    if args.json {
        println!("{}", report.to_json());
    } else {
        report.print_prose();
    }
    0
}
