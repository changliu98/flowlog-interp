use std::collections::HashSet;
use std::fmt;

use syn::{FnArg, Item, ReturnType, Type, Visibility};

/// Scalar results that an embedded Rust function may expose to `@call`.
///
/// FlowLog's physical rows contain `i64` values, so an `i64` result can bind a
/// rule variable, while a `bool` result is used as a body filter.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum RustReturnType {
    I64,
    Bool,
}

impl fmt::Display for RustReturnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I64 => write!(f, "i64"),
            Self::Bool => write!(f, "bool"),
        }
    }
}

/// One public top-level function exported by a `.code rust` section.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RustFunction {
    name: String,
    arity: usize,
    return_type: RustReturnType,
}

impl RustFunction {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn return_type(&self) -> RustReturnType {
        self.return_type
    }
}

/// Rust source embedded directly in one FlowLog program.
///
/// Every public, top-level function is an `@call` export. Private functions,
/// imports, types, constants, and modules remain ordinary implementation
/// details in the same source block.
#[derive(Debug, Clone)]
pub struct EmbeddedRust {
    source: String,
    functions: Vec<RustFunction>,
}

impl EmbeddedRust {
    pub fn parse(source: String) -> Result<Self, String> {
        let file =
            syn::parse_file(&source).map_err(|error| format!("invalid embedded Rust: {error}"))?;
        let mut functions = Vec::new();
        let mut names = HashSet::new();

        for item in file.items {
            let Item::Fn(item_fn) = item else {
                continue;
            };
            if !matches!(item_fn.vis, Visibility::Public(_)) {
                continue;
            }

            let signature = &item_fn.sig;
            let name = signature.ident.to_string();
            if name.starts_with("r#") {
                return Err(format!(
                    "embedded Rust export {name:?} uses a raw identifier, which @call cannot name"
                ));
            }
            if !is_flowlog_identifier(&name) {
                return Err(format!(
                    "embedded Rust export {name:?} is not an ASCII FlowLog identifier and cannot be named by @call"
                ));
            }
            if !names.insert(name.clone()) {
                return Err(format!("duplicate embedded Rust export {name:?}"));
            }
            if signature.asyncness.is_some()
                || signature.unsafety.is_some()
                || signature.abi.is_some()
                || signature.variadic.is_some()
                || !signature.generics.params.is_empty()
                || signature.generics.where_clause.is_some()
            {
                return Err(format!(
                    "embedded Rust export {name:?} must be a safe, synchronous, non-generic Rust function"
                ));
            }

            for argument in &signature.inputs {
                match argument {
                    FnArg::Typed(argument) if is_plain_type(&argument.ty, "i64") => {}
                    FnArg::Typed(argument) => {
                        return Err(format!(
                            "embedded Rust export {name:?} has unsupported argument type {}; the @call ABI is i64, so only i64 inputs are supported",
                            display_type(&argument.ty)
                        ));
                    }
                    FnArg::Receiver(_) => {
                        return Err(format!(
                            "embedded Rust export {name:?} must be a free function"
                        ));
                    }
                }
            }

            let return_type = match &signature.output {
                ReturnType::Type(_, output) if is_plain_type(output, "i64") => RustReturnType::I64,
                ReturnType::Type(_, output) if is_plain_type(output, "bool") => {
                    RustReturnType::Bool
                }
                ReturnType::Type(_, output) => {
                    return Err(format!(
                        "embedded Rust export {name:?} returns {}; the @call ABI is i64, so only i64 and bool results are supported",
                        display_type(output)
                    ));
                }
                ReturnType::Default => {
                    return Err(format!(
                        "embedded Rust export {name:?} must return i64 or bool"
                    ));
                }
            };

            functions.push(RustFunction {
                name,
                arity: signature.inputs.len(),
                return_type,
            });
        }

        Ok(Self { source, functions })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn functions(&self) -> &[RustFunction] {
        &self.functions
    }

    pub fn function(&self, name: &str) -> Option<&RustFunction> {
        self.functions.iter().find(|function| function.name == name)
    }

    /// Remove one top-level `.code rust` section before the Datalog grammar is
    /// parsed. Newlines are retained so later parser errors keep their original
    /// source line numbers.
    pub fn extract(program: &str) -> Result<(String, Option<Self>), String> {
        let mut datalog = String::with_capacity(program.len());
        let mut rust = String::new();
        let mut in_rust = false;
        let mut code_start = 0usize;
        let mut saw_block = false;

        for (line_index, line) in program.split_inclusive('\n').enumerate() {
            let line_number = line_index + 1;
            let without_newline = line.strip_suffix('\n').unwrap_or(line);
            let logical = without_newline
                .strip_suffix('\r')
                .unwrap_or(without_newline);
            let trimmed = logical.trim();

            if in_rust {
                if trimmed == ".endcode" {
                    in_rust = false;
                    retain_newline(&mut datalog, line);
                } else {
                    rust.push_str(line);
                    retain_newline(&mut datalog, line);
                }
                continue;
            }

            if trimmed == ".code rust" {
                if saw_block {
                    return Err(format!(
                        "line {line_number}: only one .code rust section is allowed"
                    ));
                }
                saw_block = true;
                in_rust = true;
                code_start = line_number;
                retain_newline(&mut datalog, line);
                continue;
            }
            if trimmed.starts_with(".code ") {
                return Err(format!(
                    "line {line_number}: unsupported embedded language in {trimmed:?}; expected .code rust"
                ));
            }
            if trimmed == ".endcode" {
                return Err(format!(
                    "line {line_number}: .endcode has no matching .code rust"
                ));
            }
            datalog.push_str(line);
        }

        if in_rust {
            return Err(format!(
                "line {code_start}: .code rust section is missing .endcode"
            ));
        }

        let embedded = if saw_block {
            Some(Self::parse(rust)?)
        } else {
            None
        };
        Ok((datalog, embedded))
    }
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

        let (datalog, embedded) = EmbeddedRust::extract(program).unwrap();
        assert!(!datalog.contains("pub fn"));
        let embedded = embedded.unwrap();
        assert_eq!(embedded.functions().len(), 2);
        assert_eq!(embedded.function("clamp").unwrap().arity(), 2);
        assert_eq!(
            embedded.function("positive").unwrap().return_type(),
            RustReturnType::Bool
        );
        assert!(embedded.function("helper").is_none());
    }

    #[test]
    fn rejects_an_export_outside_the_physical_scalar_abi() {
        let error =
            EmbeddedRust::parse("pub fn bad(x: i32) -> i64 { x as i64 }".to_string()).unwrap_err();
        assert!(error.contains("the @call ABI is i64, so only i64 inputs are supported"));
    }
}
