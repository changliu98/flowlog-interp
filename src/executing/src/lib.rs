//! FlowLog: a Datalog engine over differential dataflow, as a library.
//!
//! `engine::Engine` is the whole interface: hold one, `check` and `evaluate`
//! programs through it. The command line (`executing`), the service
//! (`daemon`) and the C interface (`capi`) are clients of it and add nothing
//! it does not have.

pub mod accounting;
pub mod aggregation;
pub mod arg;
pub mod cache;
pub mod canonical;
pub mod capi;
pub mod collector;
pub mod compare;
pub mod daemon;
pub mod dataflow;
pub mod engine;
pub mod explain;
pub mod files;
pub mod jn;
pub mod map;
mod merge;
pub mod native_calls;
pub mod runner;
pub mod symbols;
pub mod transformer;
pub mod worker;

pub use accounting::{CancelToken, Limits};
pub use engine::{
    Engine, EngineConfig, EvaluationOptions, EvaluationRequest, EvaluationResult, Inputs,
    ProgramReport, ProgramSource, Schedule,
};
pub use explain::Witness;
pub use parsing::diagnostic::{Diagnostic, DiagnosticKind, Location};

pub type Time = ();
pub type Iter = u16;

/// The allocator every binary and library of the engine runs on: mimalloc,
/// with each thread's bytes attributed to the evaluation it serves.
#[global_allocator]
static GLOBAL: accounting::CountingAllocator<mimalloc::MiMalloc> =
    accounting::CountingAllocator(mimalloc::MiMalloc);
