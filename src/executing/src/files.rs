//! Relations as files: the command line's way in and out.
//!
//! An input file holds one row per line, cells separated by the delimiter;
//! a number cell is its decimal text and a symbol cell is its text. A written
//! relation is a valid input file, so one run's output can be another run's
//! input. The engine's own interface takes rows in memory; this module is an
//! adapter over it, and nothing in the engine depends on it.

use crate::cache::{CacheRunStats, RelationState};
use crate::rows::Rows;
use crate::explain::Witness;
use crate::symbols::SymbolTable;
use parsing::decl::DataType;
use parsing::diagnostic::{Diagnostic, Result};
use parsing::parser::Program;
use parsing::Val;
use reading::reader::{read_relation_file_partition_into, InputRow};
use reading::row::Row;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// Every declared input relation, read from its file under `directory`.
pub fn read_facts_directory(
    program: &Program,
    directory: &Path,
    delimiter: u8,
    symbols: &SymbolTable,
) -> Result<HashMap<String, Arc<RelationState>>> {
    read_facts_directory_with_workers(program, directory, delimiter, symbols, 1)
}

/// Read each relation's byte ranges concurrently, with at most `workers`
/// readers. The calling thread reads one range; the others join before the
/// relation is published. Symbol ids are content-derived across all readers.
pub fn read_facts_directory_with_workers(
    program: &Program,
    directory: &Path,
    delimiter: u8,
    symbols: &SymbolTable,
    workers: usize,
) -> Result<HashMap<String, Arc<RelationState>>> {
    let workers = workers.max(1);
    let mut states = HashMap::new();
    for declaration in program.edbs() {
        let path = match declaration.path() {
            Some(path) => directory.join(path),
            None => directory.join(format!("{}.facts", declaration.name())),
        };
        let types = declaration.column_types();
        let path = path.to_string_lossy();
        macro_rules! packed {
            ($arity:literal) => {
                flatten_rows(read_sorted_file::<Row<$arity>>(
                    declaration.name(), &types, &path, delimiter, symbols, workers,
                )?)
            };
        }
        let rows = match types.len() {
            0 => packed!(0), 1 => packed!(1), 2 => packed!(2),
            3 => packed!(3), 4 => packed!(4), 5 => packed!(5),
            6 => packed!(6), 7 => packed!(7), 8 => packed!(8),
            arity => Rows::from_vecs(arity, &read_sorted_file::<Vec<Val>>(
                declaration.name(), &types, &path, delimiter, symbols, workers,
            )?),
        };
        states.insert(
            declaration.name().to_string(),
            Arc::new(RelationState::from_sorted(declaration.name(), declaration.arity(), rows)),
        );
    }
    Ok(states)
}

fn read_sorted_file<R: InputRow + Ord + Send>(
    name: &str,
    types: &[DataType],
    path: &str,
    delimiter: u8,
    symbols: &SymbolTable,
    workers: usize,
) -> Result<Vec<R>> {
    let read = |index| -> Result<Vec<R>> {
        let mut rows = read_relation_file_partition_into::<R>(
            name, types, path, delimiter, index, workers, &mut |text| symbols.intern(text),
        )?;
        rows.sort_unstable();
        rows.dedup();
        Ok(rows)
    };
    if workers == 1 { return read(0); }
    let partitions = std::thread::scope(|scope| {
        let readers: Vec<_> = (1..workers).map(|index| {
            let read = &read;
            scope.spawn(move || read(index))
        }).collect();
        let mut results = vec![read(0)];
        // Join every reader even when an earlier range was invalid.
        for reader in readers {
            results.push(reader.join().unwrap_or_else(|_| Err(Diagnostic::internal(
                format!("input reader panicked for relation {name}")
            ))));
        }
        results.into_iter().collect::<Result<Vec<_>>>()
    })?;
    Ok(crate::merge::sorted(partitions, &R::cmp, &|_, _| true, None))
}

/// Sorted, distinct fixed-width rows laid out flat, in the same order.
fn flatten_rows<const N: usize>(rows: Vec<Row<N>>) -> Rows {
    let mut cells = Vec::with_capacity(rows.len() * N);
    for row in &rows {
        cells.extend_from_slice(row.as_slice());
    }
    Rows::from_cells(N, cells, rows.len())
}

/// One cell as it is written.
pub fn render_cell(cell: Val, column: DataType, symbols: &SymbolTable) -> String {
    match column {
        DataType::Integer => cell.to_string(),
        DataType::Symbol => symbols.resolve(cell).unwrap_or_else(|| cell.to_string()),
    }
}

/// Write every declared output relation that has a state to
/// `<directory>/csvs/<Relation>.csv`, and the sizes to `size.txt`.
pub fn write_outputs(
    program: &Program,
    states: &HashMap<String, Arc<RelationState>>,
    directory: &Path,
    delimiter: u8,
    symbols: &SymbolTable,
) -> Result<()> {
    let csvs = directory.join("csvs");
    fs::create_dir_all(&csvs).map_err(|error| {
        Diagnostic::input(format!("cannot create output directory {}: {error}", csvs.display()))
    })?;
    let sizes_path = csvs.join("size.txt");
    let mut sizes = BufWriter::new(File::create(&sizes_path).map_err(|error| {
        Diagnostic::input(format!("cannot create {}: {error}", sizes_path.display()))
    })?);
    let relations: Vec<(&str, &RelationState, Vec<DataType>)> = program
        .idbs()
        .iter()
        .filter_map(|declaration| {
            let state = states.get(declaration.name())?;
            let types = program
                .column_types(declaration.name())
                .map(<[DataType]>::to_vec)
                .unwrap_or_else(|| declaration.column_types());
            Some((declaration.name(), state.as_ref(), types))
        })
        .collect();

    // Every relation is its own file: write them on a few threads, the
    // largest first, each onto the thread with the fewest rows so far.
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .clamp(1, 8)
        .min(relations.len().max(1));
    let mut order: Vec<usize> = (0..relations.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(relations[index].1.rows.len()));
    let mut shares = vec![(0usize, Vec::new()); threads];
    for index in order {
        let share = shares.iter_mut().min_by_key(|share| share.0).expect("one thread at least");
        share.0 += relations[index].1.rows.len() + 1;
        share.1.push(index);
    }
    let written: Vec<Result<()>> = std::thread::scope(|scope| {
        let handles: Vec<_> = shares
            .iter()
            .map(|(_, indices)| {
                let relations = &relations;
                let csvs = &csvs;
                scope.spawn(move || -> Result<()> {
                    for &index in indices {
                        let (name, state, types) = &relations[index];
                        write_relation(&csvs.join(format!("{name}.csv")), state, types, delimiter, symbols)?;
                    }
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    Err(Diagnostic::internal("an output writer thread panicked"))
                })
            })
            .collect()
    });
    written.into_iter().collect::<Result<Vec<()>>>()?;

    for (name, state, _) in &relations {
        if !state.rows.is_empty() {
            writeln!(sizes, "{}: ((), (), {})", name, state.rows.len()).map_err(
                |error| Diagnostic::input(format!("cannot write {}: {error}", sizes_path.display())),
            )?;
        }
        info!("Size of [{}]: {}", name, state.rows.len());
    }
    sizes.flush().map_err(|error| {
        Diagnostic::input(format!("cannot write {}: {error}", sizes_path.display()))
    })?;
    Ok(())
}

/// One relation's rows as a file: one line per row, cells joined by the
/// delimiter, each line assembled whole before it is written.
fn write_relation(
    path: &Path,
    state: &RelationState,
    types: &[DataType],
    delimiter: u8,
    symbols: &SymbolTable,
) -> Result<()> {
    let write_error = |error: std::io::Error| {
        Diagnostic::input(format!("cannot write {}: {error}", path.display()))
    };
    let mut out = BufWriter::with_capacity(
        1 << 20,
        File::create(path).map_err(|error| {
            Diagnostic::input(format!("cannot create {}: {error}", path.display()))
        })?,
    );
    let mut line = Vec::with_capacity(256);
    let mut digits = itoa::Buffer::new();
    for row in state.rows.iter() {
        line.clear();
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                line.push(delimiter);
            }
            match types.get(column).copied().unwrap_or(DataType::Integer) {
                DataType::Integer => line.extend_from_slice(digits.format(*cell).as_bytes()),
                DataType::Symbol => line.extend_from_slice(
                    render_cell(*cell, DataType::Symbol, symbols).as_bytes(),
                ),
            }
        }
        line.push(b'\n');
        out.write_all(&line).map_err(write_error)?;
    }
    out.flush().map_err(write_error)
}

/// The run's counters, as JSON beside the outputs.
pub fn write_stats(directory: &Path, stats: &CacheRunStats) -> Result<()> {
    let csvs = directory.join("csvs");
    fs::create_dir_all(&csvs).map_err(|error| {
        Diagnostic::input(format!("cannot create output directory {}: {error}", csvs.display()))
    })?;
    let path = csvs.join("cache-stats.json");
    let bytes = serde_json::to_vec(stats)
        .map_err(|error| Diagnostic::internal(format!("cannot serialize the run's counters: {error}")))?;
    fs::write(&path, bytes)
        .map_err(|error| Diagnostic::input(format!("cannot write {}: {error}", path.display())))
}

/// The witnesses, one JSON object per line, beside the outputs.
pub fn write_witnesses(directory: &Path, witnesses: &[Witness]) -> Result<()> {
    let csvs = directory.join("csvs");
    fs::create_dir_all(&csvs).map_err(|error| {
        Diagnostic::input(format!("cannot create output directory {}: {error}", csvs.display()))
    })?;
    let path = csvs.join("explain.jsonl");
    let mut out = BufWriter::new(File::create(&path).map_err(|error| {
        Diagnostic::input(format!("cannot create {}: {error}", path.display()))
    })?);
    for witness in witnesses {
        let line = serde_json::to_string(witness)
            .map_err(|error| Diagnostic::internal(format!("cannot serialize a witness: {error}")))?;
        writeln!(out, "{line}")
            .map_err(|error| Diagnostic::input(format!("cannot write {}: {error}", path.display())))?;
    }
    out.flush()
        .map_err(|error| Diagnostic::input(format!("cannot write {}: {error}", path.display())))
}
