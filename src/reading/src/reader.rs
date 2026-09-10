/* ------------------------------------------------------------------------------------ */
/* Reading relations into memory, and the dataflow's input and variable constructors     */
/* ------------------------------------------------------------------------------------ */

use std::fs::File;
use std::io::{BufRead, BufReader};

use differential_dataflow::input::Input;
use differential_dataflow::operators::iterate::SemigroupVariable;

use timely::dataflow::Scope;
use timely::order::Product;

use parsing::decl::DataType;
use parsing::diagnostic::{Diagnostic, Result};
use crate::row::Row;
use crate::row::FatRow;
use crate::rel::Rel;
use crate::session::InputSessionGeneric;
use crate::Time;
use crate::Iter;
use crate::Semiring;
use crate::Val;

/// The longest fragment of an input file quoted back in an error message.
const QUOTED_FRAGMENT_MAX: usize = 120;

/// Quotes a fragment of an input file for an error message.
///
/// Bounded, so that one pathological line cannot become the whole message, and
/// lossy, so that a fragment which is not valid UTF-8 can still be shown.
fn quoted(fragment: &[u8]) -> String {
    let head = &fragment[..fragment.len().min(QUOTED_FRAGMENT_MAX)];
    let text = String::from_utf8_lossy(head);
    if fragment.len() > QUOTED_FRAGMENT_MAX {
        format!("\"{text}\"... ({} bytes)", fragment.len())
    } else {
        format!("\"{text}\"")
    }
}

/// Reads one cell of an input file, or refuses the file.
///
/// A cell that is not a number used to discard the whole row and continue, so a
/// damaged, mis-delimited or out-of-range input file produced a smaller
/// relation instead of an error, and every query over it answered confidently
/// with less data than the file contained. There is no value in the domain that
/// means "unreadable", so the only honest outcomes are the number or a refusal.
pub fn parse_number_cell(rel_path: &str, line: &[u8], cell: &[u8]) -> Result<Val> {
    let text = std::str::from_utf8(cell).map_err(|error| {
        Diagnostic::input(format!(
            "can't read data from \"{rel_path}\": cell {} is not valid UTF-8 ({error}), \
             on line {}",
            quoted(cell),
            quoted(line),
        ))
    })?;
    text.parse::<Val>().map_err(|error| {
        Diagnostic::input(format!(
            "can't read data from \"{rel_path}\": cell {} is not a number ({error}), \
             on line {}; a value is a 64-bit signed integer",
            quoted(cell),
            quoted(line),
        ))
    })
}

/// Reads one symbol cell: the text between the delimiters, interned.
fn parse_symbol_cell(
    rel_path: &str,
    line: &[u8],
    cell: &[u8],
    intern: &mut dyn FnMut(&str) -> Result<Val>,
) -> Result<Val> {
    let text = std::str::from_utf8(cell).map_err(|error| {
        Diagnostic::input(format!(
            "can't read data from \"{rel_path}\": cell {} is not valid UTF-8 ({error}), \
             on line {}",
            quoted(cell),
            quoted(line),
        ))
    })?;
    intern(text)
}

/// Reads one relation file into rows: the same delimiter and cell domain as
/// every other reader of the engine, refusing the file on the first cell that
/// is not of its column's type and on the first line whose width is not the
/// relation's arity.
///
/// A file for an arity-0 relation holds one line per row and nothing on it:
/// an empty file is the empty relation, any line at all is its one row.
pub fn read_relation_file(
    rel_name: &str,
    column_types: &[DataType],
    rel_path: &str,
    delimiter: u8,
    intern: &mut dyn FnMut(&str) -> Result<Val>,
) -> Result<Vec<Vec<Val>>> {
    let arity = column_types.len();
    let file = File::open(rel_path).map_err(|error| {
        Diagnostic::input(format!(
            "can't read data from \"{rel_path}\" for relation {rel_name}: {error}"
        ))
        .with_relation(rel_name)
    })?;
    let mut reader = BufReader::new(file);
    let mut rows = Vec::new();
    let mut line = Vec::with_capacity(256);
    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).map_err(|error| {
            Diagnostic::input(format!("can't read data from \"{rel_path}\": {error}"))
                .with_relation(rel_name)
        })?;
        if bytes_read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if arity == 0 {
            rows.push(Vec::new());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let row = parse_row(rel_name, column_types, rel_path, &line, delimiter, intern)?;
        rows.push(row);
    }
    Ok(rows)
}

/// Parses one line of a relation file.
pub fn parse_row(
    rel_name: &str,
    column_types: &[DataType],
    rel_path: &str,
    line: &[u8],
    delimiter: u8,
    intern: &mut dyn FnMut(&str) -> Result<Val>,
) -> Result<Vec<Val>> {
    let arity = column_types.len();
    let mut row = Vec::with_capacity(arity);
    let mut values = 0usize;
    for cell in line.split(|&byte| byte == delimiter) {
        values += 1;
        if values <= arity {
            let value = match column_types[values - 1] {
                DataType::Integer => parse_number_cell(rel_path, line, cell)?,
                DataType::Symbol => parse_symbol_cell(rel_path, line, cell, intern)?,
            };
            row.push(value);
        }
    }
    if values != arity {
        return Err(Diagnostic::input(format!(
            "can't read data from \"{rel_path}\": expected {arity} values, got {values}, \
             on line {}",
            quoted(line),
        ))
        .with_relation(rel_name));
    }
    Ok(row)
}

/* ------------------------------------------------------------------------------------ */
/* construct session and table of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_construct_session_and_table {
    ($($n:expr),*) => {
        pub fn construct_session_and_table<G: Scope<Timestamp=Time>>(
            scope: &mut G,
            arity: usize,
            fat_mode: bool,
        ) -> (InputSessionGeneric<Time>, Rel<G>) {
            if !fat_mode {
                match arity {
                    $(
                        $n => {
                            let (session, input_rel) = scope.new_collection::<Row<$n>, Semiring>();
                            paste::paste! {
                                (
                                    InputSessionGeneric::[<InputSession $n>](session),
                                    Rel::[<Collection $n>](input_rel).dedup(),
                                )
                            }
                        }
                    )*
                    _ => unreachable!("construct_session_and_table: arity {} overflows", arity),
                }
            } else {
                let (session, input_rel) = scope.new_collection::<FatRow, Semiring>();
                (
                    InputSessionGeneric::InputSessionFat(session, arity),
                    Rel::CollectionFat(input_rel, arity).dedup()
                )
            }
        }
    };
}

// `construct_session_and_table(scope: &mut G, arity: usize, fat_mode) -> (InputSessionGeneric<Time>, Rel<G>)` for arities 0 to 8
generate_construct_session_and_table!(0, 1, 2, 3, 4, 5, 6, 7, 8);

/* ------------------------------------------------------------------------------------ */
/* construct semigroup variable of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_construct_var {
    ($($n:expr),*) => {
        pub fn construct_var<G: Scope<Timestamp=Product<Time, Iter>>>(
            scope: &mut G,
            arity: usize,
            fat_mode: bool,
        ) -> Rel<G> {
            if !fat_mode {
                match arity {
                    $(
                        $n => paste::paste! {
                            Rel::[<Variable $n>](SemigroupVariable::<_, Vec<(Row<$n>, Product<Time, Iter>, Semiring)>>::new(scope, Product::new(Default::default(), 1)))
                        },
                    )*
                    _ => unreachable!("arity {} should be handled by match arms if <= MAX_ROW_ARITY", arity),
                }
            } else {
                // fat mode
                Rel::VariableFat(
                    SemigroupVariable::<_, Vec<(FatRow, Product<Time, Iter>, Semiring)>>::new(scope, Product::new(Default::default(), 1)),
                    arity
                )
            }
        }
    };
}

// `construct_var(scope: &mut G, arity: usize, fat_mode) -> Rel<G>` for arities 0 to 8
generate_construct_var!(0, 1, 2, 3, 4, 5, 6, 7, 8);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn no_symbols(text: &str) -> Result<Val> {
        Err(Diagnostic::input(format!("unexpected symbol {text:?}")))
    }

    fn fixture(contents: &[u8]) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "flowlog-interp-reader-{}-{nonce}.csv",
            std::process::id(),
        ));
        std::fs::write(&path, contents).expect("write reader fixture");
        path
    }

    #[test]
    fn a_file_is_read_line_by_line_across_the_whole_value_domain() {
        let path = fixture(b"1,10\n\n222222222,20\r\n3,-9223372036854775808\n9223372036854775807,40");
        let rows = read_relation_file(
            "Edge",
            &[DataType::Integer, DataType::Integer],
            &path.to_string_lossy(),
            b',',
            &mut no_symbols,
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![1, 10],
                vec![222222222, 20],
                vec![3, Val::MIN],
                vec![Val::MAX, 40]
            ]
        );
        std::fs::remove_file(path).expect("remove reader fixture");
    }

    #[test]
    fn a_cell_that_is_not_a_number_refuses_the_file() {
        let error = parse_number_cell("/facts/Edge.facts", b"3,abc", b"abc").unwrap_err();
        assert!(error.message.contains("cell \"abc\" is not a number"), "{error}");
        let error = parse_number_cell(
            "/facts/Edge.facts",
            b"3,9223372036854775808",
            b"9223372036854775808",
        )
        .unwrap_err();
        assert!(error.message.contains("is not a number"), "{error}");
    }

    #[test]
    fn a_refusal_names_the_file_and_bounds_the_quoted_line() {
        let line = vec![b'7'; 4096];
        let error = parse_number_cell("/facts/Edge.facts", &line, b"x").unwrap_err();
        assert!(error.message.contains("/facts/Edge.facts"), "{error}");
        assert!(error.message.contains("cell \"x\""), "{error}");
        assert!(error.message.contains("(4096 bytes)"), "{error}");
        assert!(error.message.len() < 2 * QUOTED_FRAGMENT_MAX + 256);
    }

    #[test]
    fn a_line_wider_than_the_declared_arity_refuses_the_file() {
        let error = parse_row(
            "Edge",
            &[DataType::Integer, DataType::Integer],
            "/facts/Edge.facts",
            b"1,2,3",
            b',',
            &mut no_symbols,
        )
        .unwrap_err();
        assert!(error.message.contains("expected 2 values, got 3"), "{error}");
    }

    #[test]
    fn symbol_columns_are_interned_and_arity_zero_counts_lines() {
        let mut seen = Vec::new();
        let mut intern = |text: &str| -> Result<Val> {
            seen.push(text.to_string());
            Ok(seen.len() as Val)
        };
        let row = parse_row(
            "Named",
            &[DataType::Symbol, DataType::Integer],
            "Named.facts",
            b"main,4",
            b',',
            &mut intern,
        )
        .unwrap();
        assert_eq!(row, vec![1, 4]);
        assert_eq!(seen, vec!["main".to_string()]);

        let path = fixture(b"\n");
        let rows = read_relation_file("Any", &[], &path.to_string_lossy(), b',', &mut no_symbols)
            .unwrap();
        assert_eq!(rows, vec![Vec::<Val>::new()]);
        std::fs::remove_file(path).expect("remove reader fixture");
    }
}
