//! Time evaluations without formatting or writing result rows.
//! Usage: runtime_bench PROGRAM FACTS WORKERS whole|cold|warm REPETITIONS

use flowlog::engine::{
    Engine, EngineConfig, EvaluationOptions, EvaluationRequest, Inputs, ProgramSource, Schedule,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        args.len(),
        6,
        "PROGRAM FACTS WORKERS whole|cold|warm REPETITIONS"
    );
    let source = std::fs::read_to_string(&args[1]).expect("read program");
    let workers: usize = args[3].parse().expect("worker count");
    let mode = args[4].as_str();
    assert!(matches!(mode, "whole" | "cold" | "warm"));
    let repetitions: usize = args[5].parse().expect("repetition count");
    let config = EngineConfig {
        workers,
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(config.clone());
    let mut expected = None;
    for iteration in 0..repetitions + usize::from(mode == "warm") {
        if mode != "warm" {
            engine = Engine::new(config.clone());
        }
        let request = EvaluationRequest {
            program: ProgramSource::Text {
                name: args[1].clone(),
                source: source.clone(),
            },
            inputs: Inputs::Directory(PathBuf::from(&args[2])),
            options: EvaluationOptions {
                cache: Some(mode != "whole"),
                schedule: Some(if mode == "whole" {
                    Schedule::WholeProgram
                } else {
                    Schedule::PerStratum
                }),
                ..EvaluationOptions::default()
            },
        };
        let started = Instant::now();
        let result = engine.evaluate(request).expect("evaluate benchmark");
        let wall_micros = started.elapsed().as_micros();
        let outputs: BTreeMap<_, _> = result
            .outputs
            .iter()
            .map(|(name, state)| {
                (
                    name.clone(),
                    json!({"rows": state.rows.len(), "digest": state.digest}),
                )
            })
            .collect();
        if let Some(expected) = &expected {
            assert_eq!(expected, &outputs, "outputs changed between repetitions");
        } else {
            expected = Some(outputs.clone());
        }
        if mode == "warm" && iteration == 0 {
            continue;
        }
        println!(
            "{}",
            json!({
                "workers": workers, "mode": mode, "iteration": iteration,
                "wall_micros": wall_micros, "stats": result.stats, "outputs": outputs,
            })
        );
    }
}
