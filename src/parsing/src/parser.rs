use pest::iterators::Pair;
use std::collections::HashMap;
use std::{fmt, fs};

use crate::decl::{DataType, RelDecl};
use crate::diagnostic::{Diagnostic, Location, Result};
use crate::embedded::EmbeddedRust;
use crate::rule::FLRule;
use crate::{FlowLogParser, Parser, Rule, Val};

pub trait Lexeme {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self;
}

/// A parsed, validated program.
///
/// `parse` is the one entry point that reads text: it extracts the embedded
/// Rust, parses the Datalog, and validates the result, so a `Program` in hand
/// is one the engine evaluates as written. Every relation the program
/// mentions has column types after validation, declared or inferred.
#[derive(Debug, Clone)]
pub struct Program {
    edbs: Vec<RelDecl>,
    idbs: Vec<RelDecl>,
    rules: Vec<FLRule>,
    embedded_rust: Option<EmbeddedRust>,
    types: HashMap<String, Vec<DataType>>,
    name: String,
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let edbs = self
            .edbs
            .iter()
            .map(|rel_decl| format!(".decl {rel_decl}"))
            .collect::<Vec<String>>()
            .join("\n");

        let idbs = self
            .idbs
            .iter()
            .map(|rel_decl| format!(".decl {rel_decl}"))
            .collect::<Vec<String>>()
            .join("\n");

        let rules = self
            .rules
            .iter()
            .map(|rule| rule.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        if let Some(embedded) = &self.embedded_rust {
            for block in embedded.blocks() {
                writeln!(f, ".code rust")?;
                write!(f, "{}", block.source())?;
                if !block.source().ends_with('\n') {
                    writeln!(f)?;
                }
                writeln!(f, ".endcode")?;
            }
        }
        write!(f, ".in \n{}\n.printsize \n{}\n.rule \n{}", edbs, idbs, rules)
    }
}

impl Program {
    /// A program assembled from parts, validated like a parsed one.
    pub fn new(edbs: Vec<RelDecl>, idbs: Vec<RelDecl>, rules: Vec<FLRule>) -> Self {
        let mut program = Self {
            edbs,
            idbs,
            rules,
            embedded_rust: None,
            types: HashMap::new(),
            name: "(constructed)".to_string(),
        };
        if let Ok(types) = crate::validate::validate_program(&program) {
            program.types = types;
        }
        program
    }

    pub fn edbs(&self) -> &Vec<RelDecl> {
        &self.edbs
    }

    pub fn idbs(&self) -> &Vec<RelDecl> {
        &self.idbs
    }

    pub fn rules(&self) -> &Vec<FLRule> {
        &self.rules
    }

    pub fn embedded_rust(&self) -> Option<&EmbeddedRust> {
        self.embedded_rust.as_ref()
    }

    /// The name the program was parsed under: a path, or a label.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The column types of a relation the program mentions, declared or
    /// inferred by validation; `None` for a name the program never uses.
    pub fn column_types(&self, relation: &str) -> Option<&[DataType]> {
        self.types.get(relation).map(Vec::as_slice)
    }

    /// The column types of every relation the program mentions.
    pub fn relation_types(&self) -> &HashMap<String, Vec<DataType>> {
        &self.types
    }

    /// Whether any column of any relation is a symbol.
    pub fn uses_symbols(&self) -> bool {
        self.types
            .values()
            .any(|columns| columns.iter().any(DataType::is_symbol))
    }

    /// Replace every text constant of every rule by the symbol `intern`
    /// assigns it, so the planner and the operators see one cell per literal.
    pub fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        for rule in &mut self.rules {
            rule.lower_symbols(intern)?;
        }
        Ok(())
    }

    /// Read and parse a program file.
    pub fn read(path: &str) -> Result<Self> {
        let source = fs::read_to_string(path).map_err(|error| {
            Diagnostic::parse(format!("can't read program from \"{path}\": {error}"))
        })?;
        Self::parse(&source, path)
    }

    /// Parse and validate a program text. `name` is the source name reported
    /// in locations.
    pub fn parse(source: &str, name: &str) -> Result<Self> {
        let (datalog, embedded_rust) = EmbeddedRust::extract(source, name).map_err(|error| {
            // A parse diagnostic of the block extraction names the program.
            let mut error = error;
            if let Some(location) = error.location.as_mut() {
                if location.source.is_empty() {
                    location.source = name.to_string();
                }
            }
            error
        })?;

        let parsed_rule = FlowLogParser::parse(Rule::main_grammar, &datalog)
            .map_err(|error| {
                let (line, column) = match error.line_col {
                    pest::error::LineColLocation::Pos((line, column)) => (line, column),
                    pest::error::LineColLocation::Span((line, column), _) => (line, column),
                };
                Diagnostic::parse(format!("can't parse program from \"{name}\": {}", error.variant.message()))
                    .with_location(Location::new(name, line, column))
                    .with_detail(error.to_string())
            })?
            .next()
            .expect("the main grammar produces one program");
        let mut program = Self::from_parsed_rule(parsed_rule);
        program.embedded_rust = embedded_rust;
        program.name = name.to_string();
        // A program that parses can still be one the evaluator has no plan for.
        // Refusing it here keeps every entry point - and every stage after this
        // one - from having to re-derive that.
        program.types = crate::validate::validate_program(&program).map_err(|error| {
            let mut error = error;
            if let Some(location) = error.location.as_mut() {
                if location.source.is_empty() {
                    location.source = name.to_string();
                }
            }
            error
        })?;
        Ok(program)
    }

    /// `read`, refusing by panic. For callers that have no error channel.
    pub fn from_str(path: &str) -> Self {
        Self::read(path).unwrap_or_else(|error| panic!("{error}"))
    }

    /// `parse`, refusing by panic. For callers that have no error channel.
    pub fn from_source(unparsed_str: &str, source_name: &str) -> Self {
        Self::parse(unparsed_str, source_name).unwrap_or_else(|error| panic!("{error}"))
    }
}

impl Lexeme for Program {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let mut inner_rules = parsed_rule.into_inner();
        let mut edbs: Vec<RelDecl> = Vec::new();
        let mut idbs: Vec<RelDecl> = Vec::new();
        let mut rules: Vec<FLRule> = Vec::new();

        fn parse_rel_decls(vec: &mut Vec<RelDecl>, rule: Pair<Rule>) {
            let mut rel_decls = rule.into_inner();
            while let Some(rel_decl) = rel_decls.next() {
                vec.push(RelDecl::from_parsed_rule(rel_decl));
            }
        }

        fn parse_rules(vec: &mut Vec<FLRule>, rule: Pair<Rule>) {
            let mut rules_iterator = rule.into_inner();
            while let Some(rule) = rules_iterator.next() {
                vec.push(FLRule::from_parsed_rule(rule));
            }
        }

        while let Some(inner_rule) = inner_rules.next() {
            match inner_rule.as_rule() {
                Rule::edb_decl => parse_rel_decls(&mut edbs, inner_rule),
                Rule::idb_decl => parse_rel_decls(&mut idbs, inner_rule),
                Rule::rule_decl => parse_rules(&mut rules, inner_rule),
                _ => {}
            }
        }

        Self {
            edbs,
            idbs,
            rules,
            embedded_rust: None,
            types: HashMap::new(),
            name: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Program;
    use crate::decl::DataType;
    use crate::diagnostic::DiagnosticKind;
    use crate::rule::{CallPredicate, Predicate};

    #[test]
    fn parses_call_bindings_and_filters_from_a_complete_program() {
        let source = r#".code rust
pub fn normalize(x: i64) -> i64 { x.saturating_abs() }
pub fn keep(x: i64) -> bool { x < 256 }
.endcode
.in
.decl Input(x: number)
.printsize
.decl Result(x: number, y: number)
.rule
Result(X, Y) :- Input(X), Y = @call(normalize, X), @call(keep, Y).
"#;

        let program = Program::from_source(source, "inline-test.dl");
        assert_eq!(program.embedded_rust().unwrap().functions().len(), 2);
        let rule = &program.rules()[0];
        assert_eq!(rule.rhs().len(), 3);
        assert_eq!(rule.line(), 10);
        match &rule.rhs()[1] {
            Predicate::CallPredicate(CallPredicate::Bind { output, call }) => {
                assert_eq!(output, "Y");
                assert_eq!(call.function(), "normalize");
                assert_eq!(call.arguments().len(), 1);
            }
            predicate => panic!("expected a call binding, got {predicate:?}"),
        }
        match &rule.rhs()[2] {
            Predicate::CallPredicate(CallPredicate::Filter(call)) => {
                assert_eq!(call.function(), "keep");
            }
            predicate => panic!("expected a call filter, got {predicate:?}"),
        }
    }

    #[test]
    fn a_parse_error_is_a_located_diagnostic() {
        let error = Program::parse(".in\n.decl A(x: number)\n.rule\nB(x) :- A(x\n", "p.dl")
            .unwrap_err();
        assert_eq!(error.kind, DiagnosticKind::Parse);
        assert!(error.message.starts_with("can't parse program from \"p.dl\""));
        assert_eq!(error.location.as_ref().unwrap().source, "p.dl");
        assert_eq!(error.location.as_ref().unwrap().line, 4);
    }

    #[test]
    fn types_are_declared_or_inferred() {
        let program = Program::parse(
            ".in\n.decl A(x: number, s: symbol)\n.printsize\n.decl R(s: symbol, x: number)\n.rule\nH(s, x) :- A(x, s).\nR(s, x) :- H(s, x).\n",
            "p.dl",
        )
        .unwrap();
        assert_eq!(program.column_types("A"), Some(&[DataType::Integer, DataType::Symbol][..]));
        assert_eq!(program.column_types("H"), Some(&[DataType::Symbol, DataType::Integer][..]));
        assert!(program.uses_symbols());
    }

    #[test]
    fn true_is_dropped_and_false_never_fires() {
        let program = Program::parse(
            ".in\n.decl A(x: number)\n.printsize\n.decl R(x: number)\n.rule\nR(x) :- A(x), True.\nR(x) :- A(x), False.\n",
            "p.dl",
        )
        .unwrap();
        assert_eq!(program.rules()[0].rhs().len(), 1);
        assert_eq!(program.rules()[1].rhs().len(), 2);
    }
}
