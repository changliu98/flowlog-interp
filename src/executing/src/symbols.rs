//! The engine's symbol domain: text values as cells.
//!
//! A `symbol` column holds text. Inside a row a symbol is a cell like any
//! other: a `Val` that identifies the text. The identity is content-derived -
//! `symbol_id` is the first eight bytes of the SHA-256 of the UTF-8 bytes,
//! read big-endian, with the sign bit cleared - so it is the same in every
//! process that ever interns the text, and a host embedding the engine can
//! compute an id without a round trip. The table exists to go the other
//! way, id to text, and to refuse the one thing content-derived ids can get
//! wrong: two texts under one id, which is reported at interning rather than
//! silently merged.
//!
//! The table only grows. A text's bytes never move once interned, which is
//! what lets an embedded function read a symbol through a raw pointer for as
//! long as the engine that owns the table is alive.

use parsing::diagnostic::{Diagnostic, Result};
use parsing::Val;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;

/// The content-derived identity of a text.
pub fn symbol_id(text: &str) -> Val {
    let digest = Sha256::digest(text.as_bytes());
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(prefix) & (u64::MAX >> 1)) as Val
}

#[derive(Debug, Default)]
struct Inner {
    texts: HashMap<Val, Box<str>>,
}

/// Every symbol this engine has seen, by id.
#[derive(Debug, Default)]
pub struct SymbolTable {
    inner: RwLock<Inner>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of `text`, remembering the text under it.
    pub fn intern(&self, text: &str) -> Result<Val> {
        let id = symbol_id(text);
        {
            let inner = self.inner.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(known) = inner.texts.get(&id) {
                return if &**known == text {
                    Ok(id)
                } else {
                    Err(collision(text, known, id))
                };
            }
        }
        let mut inner = self.inner.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        match inner.texts.get(&id) {
            Some(known) if &**known == text => Ok(id),
            Some(known) => Err(collision(text, known, id)),
            None => {
                inner.texts.insert(id, text.into());
                Ok(id)
            }
        }
    }

    /// The text under `id`, if this engine has seen it.
    pub fn resolve(&self, id: Val) -> Option<String> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .texts
            .get(&id)
            .map(|text| text.to_string())
    }

    /// The bytes of the text under `id`, as a pointer that stays valid for the
    /// table's lifetime: texts are boxed and never removed.
    pub fn resolve_raw(&self, id: Val) -> Option<(*const u8, usize)> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .texts
            .get(&id)
            .map(|text| (text.as_ptr(), text.len()))
    }

    pub fn contains(&self, id: Val) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .texts
            .contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .texts
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn collision(text: &str, known: &str, id: Val) -> Diagnostic {
    Diagnostic::internal(format!(
        "symbol id collision: {text:?} and {known:?} both hash to {id}; the engine cannot \
         tell the two texts apart in a row"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_content_derived_and_stable() {
        assert_eq!(symbol_id("main"), symbol_id("main"));
        assert_ne!(symbol_id("main"), symbol_id("Main"));
        assert!(symbol_id("anything") >= 0);
    }

    #[test]
    fn interning_round_trips() {
        let table = SymbolTable::new();
        let id = table.intern("hello").unwrap();
        assert_eq!(table.intern("hello").unwrap(), id);
        assert_eq!(table.resolve(id).as_deref(), Some("hello"));
        assert_eq!(table.resolve(id + 1), None);
        let (pointer, length) = table.resolve_raw(id).unwrap();
        let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
        assert_eq!(bytes, b"hello");
    }
}
