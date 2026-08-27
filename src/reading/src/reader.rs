/* ------------------------------------------------------------------------------------ */
/* I/O methods - Macro-based implementation for arities 1 through MAX_ARITY */
/* ------------------------------------------------------------------------------------ */

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use differential_dataflow::input::InputSession;
use differential_dataflow::input::Input;
use differential_dataflow::operators::iterate::SemigroupVariable;

use timely::dataflow::Scope;
use timely::order::Product;

use tracing::debug;

use parsing::decl::RelDecl;
use crate::row::Row;
use crate::row::FatRow;
use crate::row::Array;
use crate::rel::Rel;
use crate::session::InputSessionGeneric;
use crate::Time;
use crate::Iter;
use crate::Semiring;
use crate::semiring_one;


fn byte_range_reader(rel_path: &str, id: usize, peers: usize) -> (BufReader<File>, u64) {
    assert!(peers > 0, "byte_range_reader requires at least one worker");
    assert!(id < peers, "worker {id} is outside worker count {peers}");

    let mut file = File::open(rel_path)
        .unwrap_or_else(|error| panic!("can't read data from \"{rel_path}\": {error}"));
    let file_size = file
        .metadata()
        .unwrap_or_else(|error| panic!("can't stat data file \"{rel_path}\": {error}"))
        .len();
    let chunk = file_size / peers as u64;
    let start = chunk * id as u64;
    let end = if id + 1 == peers {
        file_size
    } else {
        chunk * (id + 1) as u64
    };

    if start >= end {
        file.seek(SeekFrom::End(0))
            .unwrap_or_else(|error| panic!("can't seek in data file \"{rel_path}\": {error}"));
        return (BufReader::new(file), 0);
    }

    if start == 0 {
        return (BufReader::new(file), end);
    }

    file.seek(SeekFrom::Start(start - 1))
        .unwrap_or_else(|error| panic!("can't seek in data file \"{rel_path}\": {error}"));
    let mut reader = BufReader::new(file);
    let mut preceding = [0u8; 1];
    reader
        .read_exact(&mut preceding)
        .unwrap_or_else(|error| panic!("can't align data file \"{rel_path}\": {error}"));

    if preceding[0] == b'\n' {
        return (reader, end - start);
    }

    let mut partial_line = Vec::new();
    let skipped = reader
        .read_until(b'\n', &mut partial_line)
        .unwrap_or_else(|error| panic!("can't align data file \"{rel_path}\": {error}"));
    (reader, (end - start).saturating_sub(skipped as u64))
}

fn for_each_line_in_range(
    rel_path: &str,
    id: usize,
    peers: usize,
    mut consume: impl FnMut(&[u8]),
) {
    let (mut reader, byte_budget) = byte_range_reader(rel_path, id, peers);
    let mut bytes_consumed = 0u64;
    let mut line = Vec::with_capacity(256);

    while bytes_consumed < byte_budget {
        line.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line)
            .unwrap_or_else(|error| panic!("can't read data from \"{rel_path}\": {error}"));
        if bytes_read == 0 {
            break;
        }
        bytes_consumed += bytes_read as u64;

        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if !line.is_empty() {
            consume(&line);
        }
    }
}


/* ------------------------------------------------------------------------------------ */
/* read row for thin relations */
/* ------------------------------------------------------------------------------------ */
macro_rules! generate_read_row_functions {
    ($($n:expr),*) => {
        $(
            paste::paste! {
                pub fn [<read_row_ $n>](
                    rel_decl: &RelDecl,
                    rel_path: &str,
                    delimiter: &u8,
                    session: &mut InputSession<Time, Row<$n>, Semiring>,
                    id: usize,
                    peers: usize
                ) {
                    let rel_arity = rel_decl.arity();

                    if id == 0 {
                        debug!("reading {} from {}", rel_decl, rel_path);
                    }

                    for_each_line_in_range(rel_path, id, peers, |line| {
                        let mut tuple = line.split(|&byte| byte == *delimiter);
                        let Some(first_value) = tuple
                            .next()
                            .and_then(|value| std::str::from_utf8(value).ok())
                            .and_then(|value| value.parse::<i32>().ok())
                        else {
                            return;
                        };

                        let mut row = Row::<$n>::new();
                        row.push(first_value);

                        for value in tuple {
                            let Some(parsed_value) = std::str::from_utf8(value)
                                .ok()
                                .and_then(|value| value.parse::<i32>().ok())
                            else {
                                return;
                            };
                            row.push(parsed_value);
                        }

                        if row.arity() != rel_arity {
                            panic!("expected {} values, got {}", rel_arity, row.arity());
                        }

                        session.update(row, semiring_one());
                    });
                }
            }
        )*
    };
}

// `read_row_i(rel_decl: &RelDecl, rel_path: &str, delimiter: &u8, session: &mut InputSession<Time, Row<i>, Semiring>, id: usize, peers: usize)` for i from 1 to 8
generate_read_row_functions!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10);




/* ------------------------------------------------------------------------------------ */
/* read row for fat relations */
/* ------------------------------------------------------------------------------------ */

pub fn read_row_fat(
    rel_decl: &RelDecl,
    rel_path: &str,
    delimiter: &u8,
    session: &mut InputSession<Time, FatRow, Semiring>,
    id: usize,
    peers: usize,
) {
    let rel_arity = rel_decl.arity();
    
    for_each_line_in_range(rel_path, id, peers, |line| {
        let mut tuple = line.split(|&byte| byte == *delimiter);
        let Some(first_value) = tuple
            .next()
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<i32>().ok())
        else {
            return;
        };

        let mut row = FatRow::new();
        row.push(first_value);

        for value in tuple {
            let Some(parsed_value) = std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
            else {
                return;
            };
            row.push(parsed_value);
        }

        if row.arity() != rel_arity {
            panic!("expected {} values, got {}", rel_arity, row.arity());
        }

        session.update(row, semiring_one());
    });
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

// `construct_session_and_table_i(scope: &mut G, arity: usize) -> (InputSessionGeneric<Time>, Rel<G>)` for i from 1 to 8
generate_construct_session_and_table!(1, 2, 3, 4, 5, 6, 7, 8);




/* ------------------------------------------------------------------------------------ */
/* read and insert row of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_read_row_generic {
    ($($n:expr),*) => {
        pub fn read_row_generic(
            rel_decl: &RelDecl,
            rel_path: &str,
            delimiter: &u8,
            session_generic: &mut InputSessionGeneric<Time>,
            id: usize,
            peers: usize,
            fat_mode: bool,
        ) {
            let arity = rel_decl.arity();
            if !fat_mode {
                match arity {
                    $(
                        $n => paste::paste! {
                            [<read_row_ $n>](rel_decl, rel_path, delimiter, &mut session_generic.[<listen_ $n>](), id, peers)
                        },
                    )*
                    _ => unreachable!("arity {} should be handled by match arms if <= MAX_ROW_ARITY", arity),
                }
            } else {
                // fat mode
                read_row_fat(rel_decl, rel_path, delimiter, &mut session_generic.listen_fat(), id, peers)
            }
        }
    };
}

// `read_row_generic(rel_decl: &RelDecl, rel_path: &str, delimiter: &u8, session_generic: &mut InputSessionGeneric<Time>, id: usize, peers: usize)` for i from 1 to 8
generate_read_row_generic!(1, 2, 3, 4, 5, 6, 7, 8);



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

// `construct_var_i(scope: &mut G, arity: usize) -> Rel<G>` for i from 1 to 8
generate_construct_var!(1, 2, 3, 4, 5, 6, 7, 8);

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn byte_ranges_cover_each_complete_line_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "flowlog-interp-reader-{}-{nonce}.csv",
            std::process::id(),
        ));
        std::fs::write(&path, b"1,10\n\n222222222,20\r\n3,30\n-4,40")
            .expect("write reader fixture");
        let path_string = path.to_string_lossy();
        let expected = vec![
            b"1,10".to_vec(),
            b"222222222,20".to_vec(),
            b"3,30".to_vec(),
            b"-4,40".to_vec(),
        ];

        for peers in [1, 2, 3, 64] {
            let mut actual = Vec::new();
            for id in 0..peers {
                for_each_line_in_range(&path_string, id, peers, |line| {
                    actual.push(line.to_vec());
                });
            }
            assert_eq!(actual, expected, "worker count {peers}");
        }

        std::fs::remove_file(path).expect("remove reader fixture");
    }
}
