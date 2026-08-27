use arrayvec::ArrayVec;
use parsing::Val;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::fmt;
use std::fmt::Debug;
use std::hash::Hash;

/* ------------------------------------------------------------------------------------ */
/* Array */
/* ------------------------------------------------------------------------------------ */

///
/// a trait to abstract ops over array implementations
pub trait Array: Debug + Send + Sync {
    /// insert a value
    fn push(&mut self, v: Val);
    /// return the number of columns
    fn arity(&self) -> usize;
    /// return the value of a column
    fn column(&self, id: usize) -> Val;
}

/// stack-allocated row for small arities using const generics
#[derive(Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row<const N: usize> {
    values: ArrayVec<Val, N>,
}

impl<const N: usize> Row<N> {
    pub fn new() -> Self {
        Self {
            values: ArrayVec::new(),
        }
    }

    // pub fn extend(&mut self, slice: &[Val]) {
    //     self.values.extend(slice.iter().cloned());
    // }
}

impl<const N: usize> Array for Row<N> {
    fn push(&mut self, v: Val) {
        self.values.push(v);
    }

    fn arity(&self) -> usize {
        self.values.len()
    }

    fn column(&self, id: usize) -> Val {
        unsafe { *self.values.get_unchecked(id) }
    }
}

impl<const N: usize> fmt::Display for Row<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// heap-allocated row for large arities using SmallVec as fallback
#[derive(Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatRow {
    values: SmallVec<[Val; crate::FALLBACK_ARITY]>,
}

impl FatRow {
    pub fn new() -> Self {
        Self {
            values: SmallVec::new(),
        }
    }
}

impl Array for FatRow {
    fn push(&mut self, v: Val) {
        self.values.push(v);
    }

    fn arity(&self) -> usize {
        self.values.len()
    }

    fn column(&self, id: usize) -> Val {
        unsafe { *self.values.get_unchecked(id) }
    }
}

impl fmt::Display for FatRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
