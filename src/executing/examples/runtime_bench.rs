//! Time evaluations without formatting or writing result rows.
//! Usage: runtime_bench PROGRAM FACTS WORKERS REPETITIONS
//!
//! Each repetition is a fresh engine. Every relation's rows are digested
//! here (SHA-256 of arity, row count, then every cell as eight little-endian
//! bytes, row after row), outside the timed evaluation.

use flowlog::engine::{Engine, EngineConfig, EvaluationOptions, EvaluationRequest, Inputs, ProgramSource};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 5, "PROGRAM FACTS WORKERS REPETITIONS");
    let source = std::fs::read_to_string(&args[1]).expect("read program");
    let workers: usize = args[3].parse().expect("worker count");
    let repetitions: usize = args[4].parse().expect("repetition count");
    let config = EngineConfig {
        workers,
        ..EngineConfig::default()
    };
    let mut expected = None;
    for iteration in 0..repetitions {
        let engine = Engine::new(config.clone());
        let request = EvaluationRequest {
            program: ProgramSource::Text {
                name: args[1].clone(),
                source: source.clone(),
            },
            inputs: Inputs::Directory(PathBuf::from(&args[2])),
            options: EvaluationOptions::default(),
        };
        let started = Instant::now();
        let result = engine.evaluate(request).expect("evaluate benchmark");
        let wall_micros = started.elapsed().as_micros();
        let outputs: BTreeMap<_, _> = result
            .outputs
            .iter()
            .map(|(name, state)| {
                let mut digest = Sha256::new();
                digest.update((state.rows.arity() as u64).to_le_bytes());
                digest.update((state.rows.len() as u64).to_le_bytes());
                for cell in state.rows.cells() {
                    digest.update(cell.to_le_bytes());
                }
                let digest: [u8; 32] = digest.finalize().into();
                (name.clone(), json!({"rows": state.rows.len(), "digest": digest}))
            })
            .collect();
        if let Some(expected) = &expected {
            assert_eq!(expected, &outputs, "outputs changed between repetitions");
        } else {
            expected = Some(outputs.clone());
        }
        println!(
            "{}",
            json!({
                "workers": workers, "iteration": iteration,
                "wall_micros": wall_micros, "stats": result.stats, "outputs": outputs,
            })
        );
    }
}
