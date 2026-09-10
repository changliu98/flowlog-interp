//! The command line: one client of the engine.

use crate::accounting::Limits;
use crate::daemon::ServiceDefaults;
use crate::engine::{EngineConfig, EvaluationOptions, EvaluationRequest, Inputs, ProgramSource, Schedule};
use clap::Parser;
use parsing::diagnostic::{Diagnostic, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// path of the Datalog program
    #[arg(short, long)]
    program: Option<String>,

    /// direct path of the EDBs .facts
    #[arg(short, long)]
    facts: Option<String>,

    /// direct path of the IDBs .csv
    #[arg(short, long)]
    csvs: Option<String>,

    /// delimiter
    #[arg(short, long, default_value = ",")]
    delimiter: String,

    /// enable fat mode for larger arities (uses heap-allocated SmallVec)
    #[arg(long, default_value_t = false)]
    fat_mode: bool,

    /// disable common subexpression reuse to examine and compare the benefit of this reuse
    #[arg(long, default_value_t = false)]
    no_sharing: bool,

    /// number of worker threads per evaluation
    #[arg(short, long, default_value_t = 1)]
    workers: usize,

    /// optimization Level
    /// 0: as is, 1: sip, 2: planning, 3: sip + planning
    #[arg(short = 'O', value_parser = clap::value_parser!(u8).range(0..=3))]
    opt_level: Option<u8>,

    /// directory for content-addressed compiled `.code rust` blocks
    #[arg(long)]
    call_cache: Option<PathBuf>,

    /// run as a service on this Unix-domain socket
    #[arg(long)]
    daemon_socket: Option<PathBuf>,

    /// maximum memory retained by the state cache's memory tier
    #[arg(long, default_value_t = 4096)]
    cache_max_mib: usize,

    /// directory of the state cache's disk tier, shared by every process that
    /// points at it; without it a one-shot run keeps its states in memory only
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// approximate disk budget under --cache-dir, enforced by shared incremental cleanup
    #[arg(long, default_value_t = 32768)]
    cache_disk_max_mib: usize,

    /// evaluate stratum by stratum against the state cache even without a
    /// disk tier (the default is one dataflow for the whole program)
    #[arg(long, default_value_t = false)]
    cached: bool,

    /// only check the program: parse, validate, stratify and plan it
    #[arg(long, default_value_t = false)]
    check: bool,

    /// wall-clock budget of the evaluation, in seconds
    #[arg(long)]
    budget_seconds: Option<f64>,

    /// bytes the evaluation may hold at once, in MiB
    #[arg(long)]
    memory_limit_mib: Option<u64>,

    /// rows the evaluation may materialize at its unit boundaries
    #[arg(long)]
    tuple_limit: Option<u64>,

    /// evaluations the service runs at once (0 for no limit)
    #[arg(long, default_value_t = 0)]
    max_concurrent: usize,

    /// write a witness for every row of every output relation to explain.jsonl
    #[arg(long, default_value_t = false)]
    explain: bool,
}

impl Args {
    pub fn program(&self) -> Option<&str> {
        self.program.as_deref()
    }

    pub fn facts(&self) -> Option<&str> {
        self.facts.as_deref()
    }

    pub fn csvs(&self) -> Option<&str> {
        self.csvs.as_deref()
    }

    pub fn daemon_socket(&self) -> Option<&Path> {
        self.daemon_socket.as_deref()
    }

    pub fn check_only(&self) -> bool {
        self.check
    }

    pub fn explain_all(&self) -> bool {
        self.explain
    }

    pub fn delimiter_byte(&self) -> Result<u8> {
        let bytes = self.delimiter.as_bytes();
        if bytes.len() != 1 {
            return Err(Diagnostic::validation(format!(
                "the delimiter must be one byte, not {:?}",
                self.delimiter
            )));
        }
        Ok(bytes[0])
    }

    pub fn engine_config(&self) -> Result<EngineConfig> {
        Ok(EngineConfig {
            workers: self.workers,
            fat_mode: self.fat_mode,
            sharing: !self.no_sharing,
            opt_level: self.opt_level,
            cache_memory_bytes: self.cache_max_mib.saturating_mul(1024 * 1024),
            cache_dir: self.cache_dir.clone(),
            cache_disk_bytes: (self.cache_disk_max_mib as u64).saturating_mul(1024 * 1024),
            call_cache: self.call_cache.clone(),
            max_concurrent: self.max_concurrent,
            delimiter: self.delimiter_byte()?,
        })
    }

    pub fn service_defaults(&self) -> ServiceDefaults {
        ServiceDefaults {
            program: self.program.as_ref().map(PathBuf::from),
            facts: self.facts.as_ref().map(PathBuf::from),
            csvs: self.csvs.as_ref().map(PathBuf::from),
        }
    }

    /// Whether the command line asks to read along the state cache.
    pub fn cached(&self) -> bool {
        self.cached || self.cache_dir.is_some()
    }

    pub fn options(&self) -> EvaluationOptions {
        EvaluationOptions {
            limits: Limits {
                time: self.budget_seconds.map(Duration::from_secs_f64),
                cancel: None,
                memory_bytes: self.memory_limit_mib.map(|mib| mib.saturating_mul(1024 * 1024)),
                tuples: self.tuple_limit,
            },
            cache: Some(self.cached()),
            schedule: Some(if self.cached() {
                Schedule::PerStratum
            } else {
                Schedule::WholeProgram
            }),
            explain: Vec::new(),
            explain_all: self.explain,
        }
    }

    /// The evaluation this command line describes.
    pub fn request(&self) -> Result<EvaluationRequest> {
        let program = self.program.clone().ok_or_else(|| {
            Diagnostic::validation("a program is required: --program <FILE>")
        })?;
        let facts = self.facts.clone().ok_or_else(|| {
            Diagnostic::validation("a facts directory is required: --facts <DIR>")
        })?;
        let source = std::fs::read_to_string(&program).map_err(|error| {
            Diagnostic::parse(format!("can't read program from \"{program}\": {error}"))
        })?;
        Ok(EvaluationRequest {
            program: ProgramSource::Text {
                name: program,
                source,
            },
            inputs: Inputs::Directory(PathBuf::from(facts)),
            options: self.options(),
        })
    }
}
