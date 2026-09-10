//! The one shape every refusal takes.
//!
//! A program can be refused by the grammar, by validation, by stratification,
//! by the planner, by an input file, by an embedded function, by a resource
//! ceiling, or by a defect in the engine itself. Whichever layer refuses,
//! the caller receives one `Diagnostic`: what kind of refusal it is, one
//! sentence saying what was refused, and where - the source location, the
//! rule text, the relation names and the function name the sentence is about,
//! whichever apply.
//!
//! The struct is data, not prose: it serializes to JSON for the service and
//! the C API, and `Display` renders it for the command line. A consumer that
//! wants to act on a refusal reads the fields; a consumer that wants to show
//! it prints the rendering. Neither has to parse the other.

use serde::{Deserialize, Serialize};
use std::fmt;

/// What refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// The text is not a program.
    Parse,
    /// The program is well-formed text that this engine does not evaluate as
    /// written: a type disagreement, an unbound variable, an unsupported
    /// shape.
    Validation,
    /// Negation through recursion: no stratum order evaluates the program.
    Stratification,
    /// A shape the planner has no plan for.
    Planning,
    /// An input file or row the engine cannot read.
    Input,
    /// An embedded function did not compile, or panicked while evaluating.
    Function,
    /// The program faulted on its data: a division by zero, a symbol no
    /// table knows.
    Evaluation,
    /// A time budget, a cancellation, or a memory or tuple ceiling.
    Resource,
    /// A defect in the engine: a panic in engine code, never a verdict on the
    /// program.
    Internal,
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parse => "parse",
            Self::Validation => "validation",
            Self::Stratification => "stratification",
            Self::Planning => "planning",
            Self::Input => "input",
            Self::Function => "function",
            Self::Evaluation => "evaluation",
            Self::Resource => "resource",
            Self::Internal => "internal",
        })
    }
}

/// A position in a source text. `line` and `column` are 1-based; a zero
/// column means "the line".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Location {
    pub source: String,
    pub line: usize,
    pub column: usize,
}

impl Location {
    pub fn new(source: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            source: source.into(),
            line,
            column,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.column > 0 {
            write!(f, "{}:{}:{}", self.source, self.line, self.column)
        } else {
            write!(f, "{}:{}", self.source, self.line)
        }
    }
}

/// One refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// One sentence, complete on its own.
    pub message: String,
    /// Where in the source, when the refusal is about a place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// The rule, as written, when the refusal is about one rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// The relations the refusal is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<String>,
    /// The embedded function the refusal is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Supplementary text: a compiler's output, a panic payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Diagnostic {
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            location: None,
            rule: None,
            relations: Vec::new(),
            function: None,
            detail: None,
        }
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Parse, message)
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Validation, message)
    }

    pub fn stratification(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Stratification, message)
    }

    pub fn planning(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Planning, message)
    }

    pub fn input(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Input, message)
    }

    pub fn function(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Function, message)
    }

    pub fn evaluation(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Evaluation, message)
    }

    pub fn resource(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Resource, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(DiagnosticKind::Internal, message)
    }

    pub fn with_location(mut self, location: Location) -> Self {
        self.location = Some(location);
        self
    }

    pub fn with_rule(mut self, rule: impl fmt::Display) -> Self {
        self.rule = Some(rule.to_string());
        self
    }

    pub fn with_relation(mut self, relation: impl Into<String>) -> Self {
        let relation = relation.into();
        if !self.relations.contains(&relation) {
            self.relations.push(relation);
        }
        self
    }

    pub fn with_relations<I, S>(mut self, relations: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for relation in relations {
            self = self.with_relation(relation);
        }
        self
    }

    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        self.function = Some(function.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = if detail.is_empty() { None } else { Some(detail) };
        self
    }

    /// The rendering the command line prints: the sentence, then the places
    /// it is about, then any supplementary text on the following lines.
    pub fn render(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} error: {}", self.kind, self.message)?;
        if let Some(function) = &self.function {
            write!(f, " [function {function}]")?;
        }
        if let Some(location) = &self.location {
            write!(f, " [at {location}]")?;
        }
        if let Some(rule) = &self.rule {
            write!(f, "\n  rule: {rule}")?;
        }
        if !self.relations.is_empty() {
            write!(f, "\n  relations: {}", self.relations.join(", "))?;
        }
        if let Some(detail) = &self.detail {
            write!(f, "\n{detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// The result type of every fallible engine entry point.
pub type Result<T> = std::result::Result<T, Diagnostic>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_renders_its_sentence_and_its_places() {
        let diagnostic = Diagnostic::validation("variable x is unbound")
            .with_location(Location::new("p.dl", 3, 7))
            .with_rule("R(x) :- S(y).")
            .with_relation("R")
            .with_relation("R");
        let text = diagnostic.to_string();
        assert!(text.starts_with("validation error: variable x is unbound"));
        assert!(text.contains("[at p.dl:3:7]"));
        assert!(text.contains("rule: R(x) :- S(y)."));
        assert_eq!(diagnostic.relations, vec!["R".to_string()]);
    }

    #[test]
    fn a_diagnostic_round_trips_through_json() {
        let diagnostic = Diagnostic::function("f panicked")
            .with_function("f")
            .with_detail("index out of bounds");
        let json = serde_json::to_string(&diagnostic).unwrap();
        assert!(json.contains("\"kind\":\"function\""));
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(back, diagnostic);
    }
}
