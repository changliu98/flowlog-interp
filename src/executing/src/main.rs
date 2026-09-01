use clap::Parser as ClapParser;

use debugging::debugger;
use executing::arg::Args;
use executing::runner::run_once;
use mimalloc::MiMalloc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    /* initialize tracing subscriber for logging */
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    /* CL args parsing */
    let args = Args::parse();

    debugger::display_info("Arguments", false, format!("{:#?}", args));

    run_once(args);

    info!("success query");
}

// ./target/debug/executing -p ./examples/programs/tc.dl -f ./examples/facts -c ./examples/csvs -v
