use clap::{Parser, Subcommand};
use flowlog::daemon::{request, DaemonRequest, InputSpec, OptionsSpec, OutputSpec, ProgramSpec};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Control a FlowLog service")]
struct Args {
    /// Unix-domain socket passed to `executing --daemon-socket`
    #[arg(short, long)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Evaluate a program, reading along the cache (protocol version 1 form)
    Reload {
        /// program to evaluate (default: the service's)
        #[arg(long)]
        program: Option<String>,
        /// fact directory to read (default: the service's)
        #[arg(long)]
        facts: Option<String>,
        /// output directory to write (default: the service's)
        #[arg(long)]
        csvs: Option<String>,
    },
    /// Evaluate a program with the protocol version 2 form
    Evaluate {
        /// request id, for `cancel`
        #[arg(long)]
        id: Option<String>,
        /// program file
        #[arg(long)]
        program: String,
        /// fact directory
        #[arg(long)]
        facts: Option<String>,
        /// output directory
        #[arg(long)]
        csvs: Option<String>,
        /// answer with the output rows inline
        #[arg(long, default_value_t = false)]
        inline: bool,
        /// wall-clock budget in seconds
        #[arg(long)]
        budget_seconds: Option<f64>,
        /// explain every output row
        #[arg(long, default_value_t = false)]
        explain: bool,
    },
    /// Check a program without evaluating it
    Check {
        #[arg(long)]
        program: String,
    },
    /// Cancel an evaluation in flight
    Cancel {
        #[arg(long)]
        id: String,
    },
    /// Report the last run and the cache's occupancy
    Stats,
    /// Ask the service to exit cleanly
    Shutdown,
}

fn main() {
    let args = Args::parse();
    let command = match args.command {
        Command::Reload {
            program,
            facts,
            csvs,
        } => DaemonRequest::Reload {
            program,
            facts,
            csvs,
        },
        Command::Evaluate {
            id,
            program,
            facts,
            csvs,
            inline,
            budget_seconds,
            explain,
        } => DaemonRequest::Evaluate {
            id,
            program: ProgramSpec {
                path: Some(program),
                text: None,
                name: None,
            },
            inputs: InputSpec { facts, rows: None },
            output: OutputSpec { csvs, inline },
            options: OptionsSpec {
                budget_seconds,
                explain_all: explain,
                ..OptionsSpec::default()
            },
        },
        Command::Check { program } => DaemonRequest::Check {
            program: ProgramSpec {
                path: Some(program),
                text: None,
                name: None,
            },
        },
        Command::Cancel { id } => DaemonRequest::Cancel { id },
        Command::Stats => DaemonRequest::Stats,
        Command::Shutdown => DaemonRequest::Shutdown,
    };
    let response = request(&args.socket, command)
        .unwrap_or_else(|error| panic!("service request failed: {error}"));
    print!("{response}");

    let ok = serde_json::from_str::<serde_json::Value>(&response)
        .ok()
        .and_then(|response| response.get("ok").and_then(|ok| ok.as_bool()))
        .unwrap_or(false);
    if !ok {
        std::process::exit(1);
    }
}
