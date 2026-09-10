//! Rust source embedded in a program: the functions a program defines for its
//! own rules to call.
//!
//! A program may hold any number of `.code rust` ... `.endcode` sections. Each
//! is one *block*: a compilation unit with its own `use`s, private helpers,
//! types and constants, whose public top-level functions are the ones `@call`
//! can name. Blocks are independent of one another - a function cannot call a
//! function of another block - so a block is also the unit the engine
//! compiles and caches, and an edit to one block leaves every other block's
//! artifact where it was.
//!
//! The call interface is typed by the program's own types: a parameter is
//! `i64` for a `number` or `Symbol` for a `symbol`; a result is `i64`, `bool`
//! or `Symbol`. `Symbol` is a type the engine's generated prelude provides to
//! every block; see the executing crate's native call module for what it can
//! do.

use std::collections::HashSet;
use std::fmt;

use syn::{FnArg, Item, ReturnType, Type, Visibility};

use crate::decl::DataType;
use crate::diagnostic::{Diagnostic, Location, Result};

/// The results an embedded function may hand back to a rule.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum RustReturnType {
    I64,
    Bool,
    Symbol,
}

impl RustReturnType {
    /// The column type a bound result has. A `bool` is used as a filter and
    /// never bound, so it has no column type of its own.
    pub fn value_type(&self) -> Option<DataType> {
        match self {
            Self::I64 => Some(DataType::Integer),
            Self::Symbol => Some(DataType::Symbol),
            Self::Bool => None,
        }
    }
}

impl fmt::Display for RustReturnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I64 => write!(f, "i64"),
            Self::Bool => write!(f, "bool"),
            Self::Symbol => write!(f, "Symbol"),
        }
    }
}

/// One public top-level function exported by a block.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RustFunction {
    name: String,
    parameters: Vec<DataType>,
    return_type: RustReturnType,
    block: usize,
}

impl RustFunction {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arity(&self) -> usize {
        self.parameters.len()
    }

    pub fn parameters(&self) -> &[DataType] {
        &self.parameters
    }

    pub fn return_type(&self) -> RustReturnType {
        self.return_type
    }

    /// The index of the block that defines the function.
    pub fn block(&self) -> usize {
        self.block
    }

    /// The signature as a reader would write it.
    pub fn signature(&self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| match parameter {
                DataType::Integer => "i64",
                DataType::Symbol => "Symbol",
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("fn {}({parameters}) -> {}", self.name, self.return_type)
    }
}

/// One `.code rust` section.
#[derive(Debug, Clone)]
pub struct EmbeddedBlock {
    source: String,
    first_line: usize,
    functions: Vec<RustFunction>,
}

impl EmbeddedBlock {
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The program line on which the block's first source line sits, so a
    /// line of the block can be said as a line of the program.
    pub fn first_line(&self) -> usize {
        self.first_line
    }

    pub fn functions(&self) -> &[RustFunction] {
        &self.functions
    }
}

/// Every block of one program.
#[derive(Debug, Clone, Default)]
pub struct EmbeddedRust {
    blocks: Vec<EmbeddedBlock>,
}

impl EmbeddedRust {
    /// One block whose first line is line 1: the shape a caller that holds
    /// the Rust text alone builds.
    pub fn parse(source: String) -> Result<Self> {
        Self::from_blocks(vec![(source, 1)])
    }

    /// Blocks with the program lines they start on.
    pub fn from_blocks(sources: Vec<(String, usize)>) -> Result<Self> {
        let mut blocks = Vec::with_capacity(sources.len());
        let mut names: HashSet<String> = HashSet::new();
        for (index, (source, first_line)) in sources.into_iter().enumerate() {
            let functions = parse_block(&source, index, first_line)?;
            for function in &functions {
                if !names.insert(function.name.clone()) {
                    return Err(Diagnostic::validation(format!(
                        "embedded Rust exports {:?} twice; every public function needs a name \
                         of its own",
                        function.name
                    ))
                    .with_function(function.name.clone()));
                }
            }
            blocks.push(EmbeddedBlock {
                source,
                first_line,
                functions,
            });
        }
        Ok(Self { blocks })
    }

    pub fn blocks(&self) -> &[EmbeddedBlock] {
        &self.blocks
    }

    pub fn block(&self, index: usize) -> &EmbeddedBlock {
        &self.blocks[index]
    }

    /// Every exported function of every block.
    pub fn functions(&self) -> Vec<&RustFunction> {
        self.blocks
            .iter()
            .flat_map(|block| block.functions.iter())
            .collect()
    }

    pub fn function(&self, name: &str) -> Option<&RustFunction> {
        self.blocks
            .iter()
            .flat_map(|block| block.functions.iter())
            .find(|function| function.name == name)
    }

    /// The whole embedded source: every block's text, in program order.
    pub fn source(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.source.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The indices of the blocks defining the named functions, each once.
    pub fn blocks_defining<'a, I>(&self, names: I) -> Vec<usize>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut indices = Vec::new();
        for name in names {
            if let Some(function) = self.function(name) {
                if !indices.contains(&function.block) {
                    indices.push(function.block);
                }
            }
        }
        indices.sort_unstable();
        indices
    }

    /// Remove every `.code rust` section before the Datalog grammar is parsed.
    /// Newlines are retained so later parser errors keep their original
    /// source line numbers.
    pub fn extract(program: &str, source_name: &str) -> Result<(String, Option<Self>)> {
        let mut datalog = String::with_capacity(program.len());
        let mut blocks: Vec<(String, usize)> = Vec::new();
        let mut current: Option<(String, usize)> = None;

        for (line_index, line) in program.split_inclusive('\n').enumerate() {
            let line_number = line_index + 1;
            let without_newline = line.strip_suffix('\n').unwrap_or(line);
            let logical = without_newline
                .strip_suffix('\r')
                .unwrap_or(without_newline);
            let trimmed = logical.trim();

            if let Some((source, _)) = current.as_mut() {
                if trimmed == ".endcode" {
                    let (source, first_line) = current.take().expect("open block");
                    blocks.push((source, first_line));
                } else {
                    source.push_str(line);
                }
                retain_newline(&mut datalog, line);
                continue;
            }

            if trimmed == ".code rust" {
                current = Some((String::new(), line_number + 1));
                retain_newline(&mut datalog, line);
                continue;
            }
            if trimmed.starts_with(".code ") {
                return Err(Diagnostic::parse(format!(
                    "unsupported embedded language in {trimmed:?}; expected .code rust"
                ))
                .with_location(Location::new(source_name, line_number, 0)));
            }
            if trimmed == ".endcode" {
                return Err(Diagnostic::parse(".endcode has no matching .code rust")
                    .with_location(Location::new(source_name, line_number, 0)));
            }
            datalog.push_str(line);
        }

        if let Some((_, first_line)) = current {
            return Err(Diagnostic::parse(".code rust section is missing .endcode")
                .with_location(Location::new(source_name, first_line - 1, 0)));
        }

        let embedded = if blocks.is_empty() {
            None
        } else {
            Some(Self::from_blocks(blocks)?)
        };
        Ok((datalog, embedded))
    }
}

fn parse_block(source: &str, block: usize, first_line: usize) -> Result<Vec<RustFunction>> {
    let file = syn::parse_file(source).map_err(|error| {
        let (line, column) = {
            let start = error.span().start();
            (start.line, start.column + 1)
        };
        Diagnostic::function(format!("invalid embedded Rust: {error}"))
            .with_location(Location::new("", first_line + line.saturating_sub(1), column))
    })?;
    let mut functions = Vec::new();

    for item in file.items {
        let Item::Fn(item_fn) = item else {
            continue;
        };
        if !matches!(item_fn.vis, Visibility::Public(_)) {
            continue;
        }

        let signature = &item_fn.sig;
        let name = signature.ident.to_string();
        let refuse = |message: String| {
            Err(Diagnostic::validation(message).with_function(name.clone()))
        };
        if name.starts_with("r#") {
            return refuse(format!(
                "embedded Rust export {name:?} uses a raw identifier, which @call cannot name"
            ));
        }
        if !is_flowlog_identifier(&name) {
            return refuse(format!(
                "embedded Rust export {name:?} is not an ASCII FlowLog identifier and cannot \
                 be named by @call"
            ));
        }
        if signature.asyncness.is_some()
            || signature.unsafety.is_some()
            || signature.abi.is_some()
            || signature.variadic.is_some()
            || !signature.generics.params.is_empty()
            || signature.generics.where_clause.is_some()
        {
            return refuse(format!(
                "embedded Rust export {name:?} must be a safe, synchronous, non-generic Rust \
                 function"
            ));
        }

        let mut parameters = Vec::new();
        for argument in &signature.inputs {
            match argument {
                FnArg::Typed(argument) if is_plain_type(&argument.ty, "i64") => {
                    parameters.push(DataType::Integer);
                }
                FnArg::Typed(argument) if is_plain_type(&argument.ty, "Symbol") => {
                    parameters.push(DataType::Symbol);
                }
                FnArg::Typed(argument) => {
                    return refuse(format!(
                        "embedded Rust export {name:?} has parameter type {}; a parameter is \
                         i64 for a number or Symbol for a symbol",
                        display_type(&argument.ty)
                    ));
                }
                FnArg::Receiver(_) => {
                    return refuse(format!(
                        "embedded Rust export {name:?} must be a free function"
                    ));
                }
            }
        }

        let return_type = match &signature.output {
            ReturnType::Type(_, output) if is_plain_type(output, "i64") => RustReturnType::I64,
            ReturnType::Type(_, output) if is_plain_type(output, "bool") => RustReturnType::Bool,
            ReturnType::Type(_, output) if is_plain_type(output, "Symbol") => {
                RustReturnType::Symbol
            }
            ReturnType::Type(_, output) => {
                return refuse(format!(
                    "embedded Rust export {name:?} returns {}; a result is i64, bool or Symbol",
                    display_type(output)
                ));
            }
            ReturnType::Default => {
                return refuse(format!(
                    "embedded Rust export {name:?} must return i64, bool or Symbol"
                ));
            }
        };

        functions.push(RustFunction {
            name,
            parameters,
            return_type,
            block,
        });
    }

    Ok(functions)
}

fn retain_newline(output: &mut String, line: &str) {
    if line.ends_with('\n') {
        output.push('\n');
    }
}

fn is_plain_type(value: &Type, expected: &str) -> bool {
    let Type::Path(path) = value else {
        return false;
    };
    path.qself.is_none()
        && path.path.leading_colon.is_none()
        && path.path.segments.len() == 1
        && path.path.segments[0].ident == expected
        && matches!(path.path.segments[0].arguments, syn::PathArguments::None)
}

fn display_type(value: &Type) -> String {
    match value {
        Type::Path(path) => path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "a non-path type".to_string(),
    }
}

fn is_flowlog_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let first = match bytes.next() {
        Some(b'_') => bytes.next(),
        other => other,
    };
    matches!(first, Some(byte) if byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::{EmbeddedRust, RustReturnType};
    use crate::decl::DataType;

    #[test]
    fn extracts_and_describes_public_rust_functions() {
        let program = r#".code rust
use std::cmp::min;
pub fn clamp(x: i64, limit: i64) -> i64 { min(x, limit) }
pub fn positive(x: i64) -> bool { x > 0 }
fn helper(x: i64) -> i64 { x + 1 }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Output(x: number)
.rule
Output(x) :- Input(x).
"#;

        let (datalog, embedded) = EmbeddedRust::extract(program, "test.dl").unwrap();
        assert!(!datalog.contains("pub fn"));
        let embedded = embedded.unwrap();
        assert_eq!(embedded.functions().len(), 2);
        assert_eq!(embedded.function("clamp").unwrap().arity(), 2);
        assert_eq!(
            embedded.function("positive").unwrap().return_type(),
            RustReturnType::Bool
        );
        assert!(embedded.function("helper").is_none());
        assert_eq!(embedded.blocks()[0].first_line(), 2);
    }

    #[test]
    fn several_blocks_are_independent_units_with_one_namespace() {
        let program = ".code rust\npub fn a(x: i64) -> i64 { x }\n.endcode\n.code rust\npub fn b(s: Symbol) -> Symbol { s }\n.endcode\n.in\n.decl I(x: number)\n.printsize\n.decl O(x: number)\n.rule\nO(x) :- I(x).\n";
        let (_, embedded) = EmbeddedRust::extract(program, "test.dl").unwrap();
        let embedded = embedded.unwrap();
        assert_eq!(embedded.blocks().len(), 2);
        assert_eq!(embedded.function("a").unwrap().block(), 0);
        assert_eq!(embedded.function("b").unwrap().block(), 1);
        assert_eq!(embedded.function("b").unwrap().parameters(), &[DataType::Symbol]);
        assert_eq!(embedded.blocks_defining(["b", "a", "b"]), vec![0, 1]);

        let duplicate = ".code rust\npub fn a(x: i64) -> i64 { x }\n.endcode\n.code rust\npub fn a(x: i64) -> i64 { x }\n.endcode\n";
        let error = EmbeddedRust::extract(duplicate, "test.dl").unwrap_err();
        assert!(error.message.contains("twice"), "{error}");
    }

    #[test]
    fn rejects_an_export_outside_the_call_interface() {
        let error =
            EmbeddedRust::parse("pub fn bad(x: i32) -> i64 { x as i64 }".to_string()).unwrap_err();
        assert!(error.message.contains("a parameter is i64 for a number or Symbol for a symbol"));
    }
}
