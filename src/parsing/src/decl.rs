/*
    DataType: number | symbol
    Attribute: <name>: <DataType>
    RelDecl: <name>(<Attribute>, <Attribute>, ...)
*/

use crate::parser::Lexeme;
use crate::Rule;
use pest::iterators::Pair;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The type of one column.
///
/// Every cell is a `Val` at run time. The type says how the cell is read: a
/// `number` is its own value, a `symbol` is the content-derived id of a text
/// (see the engine's symbol table). Arithmetic and ordered comparison are
/// defined over numbers; equality, joins and aggregation by `count` are
/// defined over both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    #[serde(rename = "number")]
    Integer,
    Symbol,
}

impl DataType {
    fn from_str(type_str: &str) -> Self {
        match type_str {
            "number" => Self::Integer,
            "string" | "symbol" => Self::Symbol,
            _ => unreachable!(),
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Self::Integer)
    }

    pub fn is_symbol(&self) -> bool {
        matches!(self, Self::Symbol)
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer => write!(f, "number"),
            Self::Symbol => write!(f, "symbol"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    name: String,
    data_type: DataType,
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.data_type)
    }
}

impl Attribute {
    fn from_str(name: &str, data_type: &str) -> Self {
        Self {
            name: name.to_string(),
            data_type: DataType::from_str(data_type),
        }
    }

    pub fn new(name: &str, data_type: DataType) -> Self {
        Self {
            name: name.to_string(),
            data_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }
}

#[derive(Debug, Clone)]
pub struct RelDecl {
    name: String,
    attributes: Vec<Attribute>,
    path: Option<String>,
    line: usize,
}

impl fmt::Display for RelDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.name,
            self.attributes
                .iter()
                .map(|attr| attr.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )?;
        if let Some(ref path) = self.path {
            write!(f, " read as {}", path)?;
        }
        Ok(())
    }
}

impl RelDecl {
    fn from_str(name: &str, attributes: Vec<Attribute>, path: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            attributes,
            path: path.map(|p| p.to_string()),
            line: 0,
        }
    }

    pub fn new(name: &str, attributes: Vec<Attribute>) -> Self {
        Self::from_str(name, attributes, None)
    }

    pub fn push_attr(&mut self, attr: Attribute) {
        self.attributes.push(attr);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attributes(&self) -> &Vec<Attribute> {
        &self.attributes
    }

    pub fn column_types(&self) -> Vec<DataType> {
        self.attributes
            .iter()
            .map(|attribute| *attribute.data_type())
            .collect()
    }

    pub fn arity(&self) -> usize {
        self.attributes.len()
    }

    pub fn path(&self) -> Option<String> {
        self.path.clone()
    }

    /// The source line of the declaration, 0 when it was not parsed from text.
    pub fn line(&self) -> usize {
        self.line
    }
}

impl Lexeme for RelDecl {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let line = parsed_rule.line_col().0;
        let mut parsed_rule = parsed_rule.into_inner();
        /* parsing the relation name */
        let name = parsed_rule.next().unwrap().as_str();

        let mut attributes = Vec::new();
        let mut path = None;
        for part in parsed_rule {
            match part.as_rule() {
                Rule::attributes_decl => {
                    attributes = part
                        .into_inner()
                        .map(|attr| {
                            let mut attr = attr.into_inner();
                            let name = attr.next().unwrap().as_str();
                            let data_type = attr.next().unwrap().as_str();
                            Attribute::from_str(name, data_type)
                        })
                        .collect();
                }
                Rule::in_decl | Rule::out_decl => {
                    path = Some(part.into_inner().next().unwrap().as_str().to_string());
                }
                _ => {}
            }
        }

        let mut declaration = Self::from_str(name, attributes, path.as_deref());
        declaration.line = line;
        declaration
    }
}
