use clap::Parser as ClapParser;

use flowlog::arg::Args;
use flowlog::daemon::serve;
use flowlog::engine::{Engine, ProgramSource};
use flowlog::files;
use parsing::diagnostic::{Diagnostic, DiagnosticKind, Result};
use parsing::parser::Program;
use std::path::Path;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    /* initialize tracing subscriber for logging */
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    /* CL args parsing */
    let args = Args::parse();
    debugging::debugger::display_info("Arguments", false, format!("{:#?}", args));

    if let Err(diagnostic) = run(&args) {
        report(&diagnostic);
        std::process::exit(1);
    }
    info!("success query");
}

fn run(args: &Args) -> Result<()> {
    let engine = Engine::new(args.engine_config()?);

    if let Some(socket) = args.daemon_socket().map(ToOwned::to_owned) {
        return serve(engine, args.service_defaults(), socket).map_err(|error| {
            Diagnostic::internal(format!("the service failed: {error}"))
        });
    }

    if args.check_only() {
        let path = args
            .program()
            .ok_or_else(|| Diagnostic::validation("a program is required: --program <FILE>"))?;
        let source = std::fs::read_to_string(path).map_err(|error| {
            Diagnostic::parse(format!("can't read program from \"{path}\": {error}"))
        })?;
        let report = engine.check(&source, path)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| Diagnostic::internal(format!("cannot render the report: {error}")))?
        );
        return Ok(());
    }

    let request = args.request()?;
    let program = match &request.program {
        ProgramSource::Text { name, source } => Program::parse(source, name)?,
        ProgramSource::Parsed(program) => program.clone(),
    };
    let delimiter = engine.config().delimiter;
    let result = engine.evaluate(request)?;

    if let Some(csvs) = args.csvs() {
        let directory = Path::new(csvs);
        let states = result
            .outputs
            .iter()
            .map(|(name, state)| (name.clone(), std::sync::Arc::clone(state)))
            .collect();
        files::write_outputs(&program, &states, directory, delimiter, engine.symbols())?;
        files::write_stats(directory, &result.stats)?;
        if args.explain_all() {
            files::write_witnesses(directory, &result.witnesses)?;
        }
    } else {
        for (name, state) in result.declared() {
            info!("Size of [{}]: {}", name, state.rows.len());
        }
    }
    Ok(())
}

/// Print a diagnostic the way a reader and a program can both take it: the
/// rendering on stderr, and with `FLOWLOG_DIAGNOSTIC_JSON=1` the JSON object
/// on its own last line.
fn report(diagnostic: &Diagnostic) {
    eprintln!("{diagnostic}");
    if diagnostic.kind == DiagnosticKind::Function {
        // The form a panic hook prints, for readers that grep for it.
        if let (Some(location), Some(function)) = (&diagnostic.location, &diagnostic.function) {
            eprintln!(
                "thread 'flowlog' panicked at {}:{}:{}:\n{}",
                location.source,
                location.line,
                location.column,
                diagnostic.detail.as_deref().unwrap_or(&diagnostic.message)
            );
            let _ = function;
        }
    }
    if std::env::var_os("FLOWLOG_DIAGNOSTIC_JSON").is_some_and(|value| !value.is_empty() && value != "0") {
        if let Ok(json) = serde_json::to_string(diagnostic) {
            eprintln!("{json}");
        }
    }
}
