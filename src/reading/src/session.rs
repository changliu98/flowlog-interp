use paste::paste;
use timely::progress::Timestamp;
use differential_dataflow::input::InputSession;

use crate::Time;
use crate::{semiring_one, Semiring};
use crate::row::Row;
use crate::row::FatRow;
use crate::row::Array;
use parsing::Val;

/* ------------------------------------------------------------------------------------ */
/* session generics */
/* ------------------------------------------------------------------------------------ */
macro_rules! impl_input_sessions {
    ($($arity:literal),*) => {
        paste! {
            pub enum InputSessionGeneric<T: Timestamp + Clone> {
                $( [<InputSession $arity>](InputSession<T, Row<$arity>, Semiring>), )*
                // Fat session for large arities
                InputSessionFat(InputSession<T, FatRow, Semiring>, usize), // Store arity
            }

            impl InputSessionGeneric<Time> {
                pub fn arity(&self) -> usize {
                    match self {
                        $( InputSessionGeneric::[<InputSession $arity>](_) => $arity, )*
                        InputSessionGeneric::InputSessionFat(_, arity) => *arity,
                    }
                }

                pub fn close(self) {
                    match self {
                        $( InputSessionGeneric::[<InputSession $arity>](session) => session.close(), )*
                        InputSessionGeneric::InputSessionFat(session, _) => session.close(),
                    }
                }

                /// Insert one type-erased row into this typed input session.
                ///
                /// Resident cache entries use `Vec<Val>` because their arity is
                /// known only after parsing the edited program. The dataflow still
                /// receives the same fixed-size `Row<N>` representation used by
                /// file inputs.
                pub fn update_values(&mut self, values: &[Val]) {
                    assert_eq!(
                        values.len(),
                        self.arity(),
                        "cached row arity does not match its relation",
                    );

                    match self {
                        $(
                            InputSessionGeneric::[<InputSession $arity>](session) => {
                                let mut row = Row::<$arity>::new();
                                for &value in values {
                                    row.push(value);
                                }
                                session.update(row, semiring_one());
                            }
                        )*
                        InputSessionGeneric::InputSessionFat(session, _) => {
                            let mut row = FatRow::new();
                            for &value in values {
                                row.push(value);
                            }
                            session.update(row, semiring_one());
                        }
                    }
                }

                $(
                    pub fn [<listen_ $arity>](&mut self) -> &mut InputSession<Time, Row<$arity>, Semiring> {
                        match self {
                            InputSessionGeneric::[<InputSession $arity>](session) => session,
                            _ => panic!("panic access to listen of arity {}", $arity),
                        }
                    }
                )*

                pub fn listen_fat(&mut self) -> &mut InputSession<Time, FatRow, Semiring> {
                    match self {
                        InputSessionGeneric::InputSessionFat(session, _) => session,
                        _ => panic!("Cannot access fat session on fixed-arity session"),
                    }
                }
            }
        }
    };
}

impl_input_sessions!(1, 2, 3, 4, 5, 6, 7, 8);