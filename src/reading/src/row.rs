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
    /// return the number of columns
    fn arity(&self) -> usize;
    /// return the value of a column
    fn column(&self, id: usize) -> Val;
}

/// A fixed-arity dataflow row: exactly N cells, without a runtime length field.
#[derive(Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row<const N: usize> {
    #[serde(with = "array_values")]
    values: [Val; N],
}

impl<const N: usize> Row<N> {
    pub fn builder() -> RowBuilder<N> {
        RowBuilder { values: ArrayVec::new() }
    }

    pub fn from_slice(values: &[Val]) -> Self {
        Self { values: values.try_into().expect("row width must match its arity") }
    }

    pub fn as_slice(&self) -> &[Val] { &self.values }
}

/// Construction keeps its cursor on the stack; completed rows store only cells.
pub struct RowBuilder<const N: usize> {
    values: ArrayVec<Val, N>,
}

impl<const N: usize> RowBuilder<N> {
    pub fn push(&mut self, value: Val) { self.values.push(value); }

    pub fn finish(self) -> Row<N> {
        Row { values: self.values.into_inner().expect("row width must match its arity") }
    }
}

impl<const N: usize> FromIterator<Val> for Row<N> {
    fn from_iter<I: IntoIterator<Item = Val>>(iter: I) -> Self {
        let values: ArrayVec<Val, N> = iter.into_iter().collect();
        RowBuilder { values }.finish()
    }
}

impl<const N: usize> Array for Row<N> {
    fn arity(&self) -> usize {
        N
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

    pub fn push(&mut self, value: Val) { self.values.push(value); }
}

impl Array for FatRow {
    fn arity(&self) -> usize {
        self.values.len()
    }

    fn column(&self, id: usize) -> Val {
        unsafe { *self.values.get_unchecked(id) }
    }
}

impl FromIterator<Val> for FatRow {
    fn from_iter<I: IntoIterator<Item = Val>>(iter: I) -> Self {
        Self { values: iter.into_iter().collect() }
    }
}

// Keep the existing serde sequence format while checking the exact width.
mod array_values {
    use super::Val;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(values: &[Val; N], serializer: S) -> Result<S::Ok, S::Error> {
        values.as_slice().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(deserializer: D) -> Result<[Val; N], D::Error> {
        let values = arrayvec::ArrayVec::<Val, N>::deserialize(deserializer)?;
        values.into_inner().map_err(|_| serde::de::Error::custom("row width must match its arity"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rows_are_dense_and_keep_their_serialized_cells() {
        assert_eq!(std::mem::size_of::<Row<0>>(), 0);
        assert_eq!(std::mem::size_of::<Row<2>>(), 2 * std::mem::size_of::<Val>());
        assert_eq!(std::mem::size_of::<Row<8>>(), 8 * std::mem::size_of::<Val>());
        let row = Row::<3>::from_slice(&[Val::MIN, 0, Val::MAX]);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json, serde_json::json!({"values": [Val::MIN, 0, Val::MAX]}));
        assert_eq!(serde_json::from_value::<Row<3>>(json).unwrap(), row);
        assert!(serde_json::from_str::<Row<3>>(r#"{"values":[1,2]}"#).is_err());
        assert_eq!(Row::<0>::builder().finish().arity(), 0);
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
