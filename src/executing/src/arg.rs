use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// path of the Datalog program
    #[arg(short, long)]
    program: String,

    /// direct path of the EDBs .facts
    #[arg(short, long)]
    facts: String,

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

    /// timely arguments
    /// -w, --workers: number of per-process worker threads.
    #[arg(short, long, default_value_t = 1)]
    workers: usize,

    /// optimization Level
    /// 0: as is, 1: sip, 2: planning, 3: sip + planning
    #[arg(short = 'O', value_parser = clap::value_parser!(u8).range(0..=3))]
    opt_level: Option<u8>,

    /// directory for content-addressed compiled `.code rust` modules
    #[arg(long)]
    call_cache: Option<PathBuf>,

    /// run as a resident cache daemon on this Unix-domain socket
    #[arg(long)]
    daemon_socket: Option<PathBuf>,

    /// maximum memory retained for materialized relation states
    #[arg(long, default_value_t = 4096)]
    cache_max_mib: usize,

    /// directory of a content-addressed store of relation states, shared by
    /// every process that points at it; enables cached evaluation without a daemon
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// approximate disk budget under --cache-dir, enforced by shared incremental cleanup
    #[arg(long, default_value_t = 32768)]
    cache_disk_max_mib: usize,
}

impl Args {
    pub fn program(&self) -> &String {
        &self.program
    }

    pub fn program_name(&self) -> String {
        std::path::Path::new(&self.program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown_program".into())
    }

    pub fn fact_name(&self) -> String {
        std::path::Path::new(&self.facts)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown_fact")
            .to_string()
    }

    pub fn facts(&self) -> String {
        (&self.facts).to_owned()
    }

    pub fn csvs(&self) -> Option<String> {
        self.csvs.clone()
    }

    pub fn delimiter(&self) -> &String {
        &self.delimiter
    }

    pub fn fat_mode(&self) -> bool {
        self.fat_mode
    }

    pub fn no_sharing(&self) -> bool {
        self.no_sharing
    }

    pub fn timely_args(&self) -> Vec<String> {
        vec![
            String::from("-w"),
            String::from(format!("{}", &self.workers)),
        ]
    }

    pub fn opt_level(&self) -> Option<u8> {
        self.opt_level
    }

    pub fn call_cache(&self) -> Option<&Path> {
        self.call_cache.as_deref()
    }

    pub fn daemon_socket(&self) -> Option<&Path> {
        self.daemon_socket.as_deref()
    }

    pub fn cache_max_bytes(&self) -> usize {
        self.cache_max_mib.saturating_mul(1024 * 1024)
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub fn cache_disk_max_bytes(&self) -> u64 {
        (self.cache_disk_max_mib as u64).saturating_mul(1024 * 1024)
    }

    /// These arguments about another program, fact directory or output
    /// directory; each `None` keeps the value this process was started with.
    pub fn with_paths(
        &self,
        program: Option<String>,
        facts: Option<String>,
        csvs: Option<String>,
    ) -> Args {
        let mut args = self.clone();
        if let Some(program) = program {
            args.program = program;
        }
        if let Some(facts) = facts {
            args.facts = facts;
        }
        if let Some(csvs) = csvs {
            args.csvs = Some(csvs);
        }
        args
    }
}
