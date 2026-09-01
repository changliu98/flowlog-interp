use clap::{Parser, Subcommand};
use executing::daemon::{request, DaemonRequest};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Control a resident FlowLog cache daemon")]
struct Args {
    /// Unix-domain socket passed to `executing --daemon-socket`
    #[arg(short, long)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Re-read the program and evaluate cache misses
    Reload,
    /// Report the last run and resident cache size
    Stats,
    /// Ask the daemon to exit cleanly
    Shutdown,
}

fn main() {
    let args = Args::parse();
    let command = match args.command {
        Command::Reload => DaemonRequest::Reload,
        Command::Stats => DaemonRequest::Stats,
        Command::Shutdown => DaemonRequest::Shutdown,
    };
    let response = request(&args.socket, command)
        .unwrap_or_else(|error| panic!("daemon request failed: {error}"));
    print!("{response}");

    let ok = serde_json::from_str::<serde_json::Value>(&response)
        .ok()
        .and_then(|response| response.get("ok").and_then(|ok| ok.as_bool()))
        .unwrap_or(false);
    if !ok {
        std::process::exit(1);
    }
}
