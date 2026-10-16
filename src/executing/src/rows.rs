//! The rows of one relation, stored flat.
//!
//! Every row of a relation has the relation's arity, so the rows are kept
//! row-major in one buffer instead of one allocation per row: row `i` is
//! `cells[i * arity .. (i + 1) * arity]`. Reading a state back from the disk
//! tier is then one copy, digesting it one pass over contiguous bytes, and
//! the memory a state holds is its cells. A relation of arity 0 has no cells
//! and at most one row, the empty one, so the number of rows is kept apart
//! from the cells.

use parsing::Val;
use std::fmt;
use std::ops::Index;

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Rows {
    arity: usize,
    count: usize,
    cells: Vec<Val>,
}

impl Rows {
    /// No rows of `arity` cells.
    pub fn new(arity: usize) -> Self {
        Self { arity, count: 0, cells: Vec::new() }
    }

    /// No rows yet, with room for `rows` of them.
    pub fn with_capacity(arity: usize, rows: usize) -> Self {
        Self { arity, count: 0, cells: Vec::with_capacity(arity * rows) }
    }

    /// Rows already laid out row-major. For arity 0, `rows` says whether
    /// the empty row is present.
    pub fn from_cells(arity: usize, cells: Vec<Val>, rows: usize) -> Self {
        assert_eq!(cells.len(), arity * rows, "cells must fill {rows} rows of arity {arity}");
        Self { arity, count: if arity == 0 { rows.min(1) } else { rows }, cells }
    }

    /// Rows given one vector each, in the order given.
    pub fn from_vecs(arity: usize, rows: &[Vec<Val>]) -> Self {
        let mut flat = Self::with_capacity(arity, rows.len());
        for row in rows {
            flat.push(row);
        }
        flat
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Every cell, row after row.
    pub fn cells(&self) -> &[Val] {
        &self.cells
    }

    /// Bytes the cells occupy.
    pub fn heap_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Val>()
    }

    pub fn get(&self, index: usize) -> Option<&[Val]> {
        (index < self.count).then(|| &self.cells[index * self.arity..(index + 1) * self.arity])
    }

    /// Append one row of exactly `arity` cells. A second empty row of an
    /// arity-0 relation is the first one again.
    pub fn push(&mut self, row: &[Val]) {
        assert_eq!(row.len(), self.arity, "a row of {} cells in a relation of arity {}", row.len(), self.arity);
        if self.arity == 0 {
            self.count = 1;
            return;
        }
        self.cells.extend_from_slice(row);
        self.count += 1;
    }

    pub fn iter(&self) -> RowsIter<'_> {
        RowsIter { rows: self, front: 0, back: self.count }
    }

    /// The rows `range.start .. range.end`, as an iterator.
    pub fn range(&self, start: usize, end: usize) -> RowsIter<'_> {
        let end = end.min(self.count);
        RowsIter { rows: self, front: start.min(end), back: end }
    }

    /// The position of `row` among sorted rows, as `slice::binary_search`.
    pub fn binary_search(&self, row: &[Val]) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let middle = low + (high - low) / 2;
            match self[middle].cmp(row) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    }

    /// Whether the rows are strictly increasing: sorted and distinct.
    pub fn is_sorted_set(&self) -> bool {
        (1..self.count).all(|index| self[index - 1] < self[index])
    }

    /// Sort the rows and drop repeats.
    pub fn sort_dedup(&mut self) {
        if self.arity == 0 || self.count < 2 || self.is_sorted_set() {
            return;
        }
        let arity = self.arity;
        let mut order: Vec<usize> = (0..self.count).collect();
        order.sort_unstable_by(|&a, &b| self[a].cmp(&self[b]));
        order.dedup_by(|&mut b, &mut a| self[a] == self[b]);
        let mut cells = Vec::with_capacity(order.len() * arity);
        for index in &order {
            cells.extend_from_slice(&self[*index]);
        }
        self.count = order.len();
        self.cells = cells;
    }

    /// One vector per row, for callers that want to own them.
    pub fn to_vecs(&self) -> Vec<Vec<Val>> {
        self.iter().map(<[Val]>::to_vec).collect()
    }
}

impl Index<usize> for Rows {
    type Output = [Val];

    fn index(&self, index: usize) -> &[Val] {
        assert!(index < self.count, "row {index} of {}", self.count);
        &self.cells[index * self.arity..(index + 1) * self.arity]
    }
}

impl fmt::Debug for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl PartialEq<Vec<Vec<Val>>> for Rows {
    fn eq(&self, other: &Vec<Vec<Val>>) -> bool {
        self.count == other.len() && self.iter().zip(other).all(|(mine, theirs)| mine == theirs.as_slice())
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a [Val];
    type IntoIter = RowsIter<'a>;

    fn into_iter(self) -> RowsIter<'a> {
        self.iter()
    }
}

#[derive(Clone)]
pub struct RowsIter<'a> {
    rows: &'a Rows,
    front: usize,
    back: usize,
}

impl<'a> Iterator for RowsIter<'a> {
    type Item = &'a [Val];

    fn next(&mut self) -> Option<&'a [Val]> {
        (self.front < self.back).then(|| {
            self.front += 1;
            let arity = self.rows.arity;
            &self.rows.cells[(self.front - 1) * arity..self.front * arity]
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.back - self.front;
        (left, Some(left))
    }
}

impl DoubleEndedIterator for RowsIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        (self.front < self.back).then(|| {
            self.back -= 1;
            let arity = self.rows.arity;
            &self.rows.cells[self.back * arity..(self.back + 1) * arity]
        })
    }
}

impl ExactSizeIterator for RowsIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_index_iterate_and_search_like_vectors() {
        let rows = Rows::from_vecs(2, &[vec![1, 2], vec![3, 4], vec![5, 6]]);
        assert_eq!(rows.len(), 3);
        assert_eq!(&rows[1], &[3, 4]);
        assert_eq!(rows.iter().rev().next(), Some(&[5, 6][..]));
        assert_eq!(rows.binary_search(&[3, 4]), Ok(1));
        assert_eq!(rows.binary_search(&[4, 0]), Err(2));
        assert_eq!(rows, vec![vec![1, 2], vec![3, 4], vec![5, 6]]);
        assert_eq!(rows.range(1, 9).count(), 2);
    }

    #[test]
    fn sorting_drops_repeats_and_arity_zero_holds_one_row() {
        let mut rows = Rows::from_vecs(1, &[vec![3], vec![1], vec![3], vec![2]]);
        rows.sort_dedup();
        assert_eq!(rows, vec![vec![1], vec![2], vec![3]]);
        let mut empty = Rows::new(0);
        empty.push(&[]);
        empty.push(&[]);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty.iter().count(), 1);
        assert_eq!(Rows::from_cells(0, Vec::new(), 5).len(), 1);
    }
}
