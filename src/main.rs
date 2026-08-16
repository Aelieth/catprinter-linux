use clap::Parser;
use tracing_subscriber::EnvFilter;

use catprinterd::config::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&cli.log_level).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .without_time()
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async move {
        match cli.cmd.unwrap_or(Cmd::Serve(cli.serve)) {
            Cmd::Serve(a) => catprinterd::commands::serve(a).await,
            Cmd::Check(a) => catprinterd::commands::check(a).await,
            Cmd::Status(a) => catprinterd::commands::status(a).await,
            Cmd::Print(a) => catprinterd::commands::print(a).await,
            Cmd::Inspect { file } => catprinterd::commands::inspect(&file).await,
            Cmd::EnsureQueue(a) => catprinterd::commands::ensure_queue(a).await,
        }
    });
    std::process::exit(code);
}
