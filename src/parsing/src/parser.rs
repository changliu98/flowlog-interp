use pest::iterators::Pair;
use std::{fmt, fs};

use crate::decl::RelDecl; // crate :: the root of the module tree
use crate::embedded::EmbeddedRust;
use crate::rule::FLRule;
use crate::{FlowLogParser, Parser, Rule};

pub trait Lexeme {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self;
}

#[derive(Debug, Clone)]
pub struct Program {
    edbs: Vec<RelDecl>,
    idbs: Vec<RelDecl>,
    rules: Vec<FLRule>,
    embedded_rust: Option<EmbeddedRust>,
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let edbs = self
            .edbs
            .iter()
            .map(|rel_decl| rel_decl.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        let idbs = self
            .idbs
            .iter()
            .map(|rel_decl| rel_decl.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        let rules = self
            .rules
            .iter()
            .map(|rule| rule.to_string())
            .collect::<Vec<String>>()
            .join("\n");

        if let Some(embedded) = &self.embedded_rust {
            writeln!(f, ".code rust")?;
            write!(f, "{}", embedded.source())?;
            if !embedded.source().ends_with('\n') {
                writeln!(f)?;
            }
            writeln!(f, ".endcode")?;
        }
        write!(f, ".in \n{}\n.printsize \n{}\n.rule \n{}", edbs, idbs, rules)
    }
}

impl Program {
    pub fn new(edbs: Vec<RelDecl>, idbs: Vec<RelDecl>, rules: Vec<FLRule>) -> Self {
        Self {
            edbs,
            idbs,
            rules,
            embedded_rust: None,
        }
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

    pub fn from_str(path: &str) -> Self {
        let unparsed_str = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("can't read program from \"{}\"", path));

        Self::from_source(&unparsed_str, path)
    }

    pub fn from_source(unparsed_str: &str, source_name: &str) -> Self {
        let (datalog, embedded_rust) = EmbeddedRust::extract(unparsed_str)
            .unwrap_or_else(|error| panic!("can't parse program from \"{source_name}\":\n{error}"));

        let parsed_rule = FlowLogParser::parse(Rule::main_grammar, &datalog)
            .unwrap_or_else(|error| {
                panic!(
                    "can't parse program from \"{}\": \n{:?}",
                    source_name, error
                )
            })
            .next()
            .unwrap();
        let mut program = Self::from_parsed_rule(parsed_rule);
        program.embedded_rust = embedded_rust;
        program
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Program;
    use crate::rule::{CallPredicate, Predicate};

    #[test]
    fn parses_call_bindings_and_filters_from_a_complete_program() {
        let source = r#".code rust
pub fn normalize(x: i32) -> i32 { x.saturating_abs() }
pub fn keep(x: i32) -> bool { x < 256 }
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
}
