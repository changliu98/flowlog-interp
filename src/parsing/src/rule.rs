use crate::compare::ComparisonExpr;
use crate::arithmetic::Arithmetic;
use crate::diagnostic::Result;
use crate::{head::Head, parser::Lexeme, Rule, Val};
use pest::iterators::Pair;
use std::fmt;
use tracing::error;

/*
    Atom: NAME(AtomArg, AtomArg, ...)
    AtomArg: Var(String) | Const(Const) | Placeholder
    Const: Integer(Val) | Text(String) | Symbol { text, cell }
*/

// atom_arg = var | const | placeholder
#[derive(Debug, Clone)]
pub enum AtomArg {
    Var(String),
    Const(Const),
    Placeholder,
}

impl AtomArg {
    pub fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }

    pub fn is_const(&self) -> bool {
        matches!(self, Self::Const(_))
    }

    pub fn is_placeholder(&self) -> bool {
        matches!(self, Self::Placeholder)
    }

    pub fn as_var(&self) -> &String {
        match self {
            Self::Var(var) => var,
            _ => panic!("expects var: {:?}", self),
        }
    }

    fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        if let Self::Const(constant) = self {
            constant.lower_symbols(intern)?;
        }
        Ok(())
    }
}

impl fmt::Display for AtomArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Var(var) => write!(f, "{}", var),
            Self::Const(constant) => write!(f, "{}", constant),
            Self::Placeholder => write!(f, "_"),
        }
    }
}

impl Lexeme for AtomArg {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        match parsed_rule.as_rule() {
            Rule::variable => Self::Var(parsed_rule.as_str().to_string()),
            Rule::constant => Self::Const(Const::from_parsed_rule(parsed_rule)),
            Rule::placeholder => Self::Placeholder,
            _ => unreachable!(),
        }
    }
}

/// A literal of the program.
///
/// `Text` is a string literal as parsed; before planning, the engine lowers
/// every text into a `Symbol`, which carries the text and the cell the symbol
/// table assigned it, so that the planner and the operators see one `Val`
/// while the canonical rendering of a rule keeps naming the text.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Const {
    Integer(Val),
    Text(String),
    Symbol { text: String, cell: Val },
}

impl Const {
    /// The cell this constant is in a row.
    pub fn integer(&self) -> Val {
        match self {
            Self::Integer(int) => *int,
            Self::Symbol { cell, .. } => *cell,
            Self::Text(text) => panic!(
                "text constant {text:?} reached the planner before being lowered to a symbol"
            ),
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text(_) | Self::Symbol { .. })
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) | Self::Symbol { text, .. } => Some(text),
            Self::Integer(_) => None,
        }
    }

    pub fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        if let Self::Text(text) = self {
            let cell = intern(text)?;
            *self = Self::Symbol {
                text: std::mem::take(text),
                cell,
            };
        }
        Ok(())
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(int) => write!(f, "{}", int),
            Self::Text(text) | Self::Symbol { text, .. } => write!(f, "\"{}\"", text),
        }
    }
}

/// Reads one integer literal of a program into the engine's value domain.
///
/// The grammar already guarantees the text is a signed run of digits, so the
/// only way this fails is a literal outside `Val`, which is reported rather
/// than wrapped.
fn parse_integer(text: &str) -> Val {
    text.parse::<Val>().unwrap_or_else(|error| {
        panic!(
            "integer constant {text} is outside this engine's value domain \
             ({} .. {}): {error}",
            Val::MIN,
            Val::MAX
        )
    })
}

impl Lexeme for Const {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let inner = parsed_rule.into_inner().next().unwrap();
        match inner.as_rule() {
            Rule::integer => Self::Integer(parse_integer(inner.as_str())),
            Rule::string => {
                let quoted = inner.as_str();
                let text = quoted
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .unwrap_or(quoted);
                Self::Text(text.to_string())
            }
            _ => { error!("constant parsing panic {:?}", inner); unreachable!() }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Atom {
    name: String,
    arguments: Vec<AtomArg>,
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.name,
            self.arguments
                .iter()
                .map(|arg| arg.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

impl Atom {
    pub fn from_str(name: &str, arguments: Vec<AtomArg>) -> Self {
        Self {
            name: name.to_string(),
            arguments,
        }
    }

    pub fn push_arg(&mut self, arg: AtomArg) {
        self.arguments.push(arg);
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Vec<AtomArg> {
        &self.arguments
    }

    pub fn arity(&self) -> usize {
        self.arguments.len()
    }

    /// The variables of the atom, in order, each once.
    pub fn variables(&self) -> Vec<&String> {
        let mut seen = Vec::new();
        for argument in &self.arguments {
            if let AtomArg::Var(variable) = argument {
                if !seen.contains(&variable) {
                    seen.push(variable);
                }
            }
        }
        seen
    }

    fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        for argument in &mut self.arguments {
            argument.lower_symbols(intern)?;
        }
        Ok(())
    }
}

impl Lexeme for Atom {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let mut inner_rules = parsed_rule.into_inner();
        let name = inner_rules.next().unwrap().as_str();

        let arguments = inner_rules
            .map(|arg| {
                let arg_inner = arg.into_inner().next().unwrap();
                AtomArg::from_parsed_rule(arg_inner)
            })
            .collect();

        Self::from_str(name, arguments)
    }
}

/// A pure function invocation implemented by one of the program's
/// `.code rust` sections.
#[derive(Debug, Clone)]
pub struct CallExpr {
    function: String,
    arguments: Vec<AtomArg>,
}

impl CallExpr {
    pub fn new(function: &str, arguments: Vec<AtomArg>) -> Self {
        Self {
            function: function.to_string(),
            arguments,
        }
    }

    pub fn function(&self) -> &str {
        &self.function
    }

    pub fn arguments(&self) -> &[AtomArg] {
        &self.arguments
    }

    fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        for argument in &mut self.arguments {
            argument.lower_symbols(intern)?;
        }
        Ok(())
    }
}

impl fmt::Display for CallExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "@call({}{})",
            self.function,
            if self.arguments.is_empty() {
                String::new()
            } else {
                format!(
                    ", {}",
                    self.arguments
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )
    }
}

impl Lexeme for CallExpr {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        debug_assert_eq!(parsed_rule.as_rule(), Rule::call_expr);
        let mut inner = parsed_rule.into_inner();
        let function = inner.next().unwrap().as_str().to_string();
        let arguments = inner
            .map(|argument| {
                let argument = argument.into_inner().next().unwrap();
                AtomArg::from_parsed_rule(argument)
            })
            .collect();
        Self {
            function,
            arguments,
        }
    }
}

/// A call either binds one result or acts as a boolean body filter.
#[derive(Debug, Clone)]
pub enum CallPredicate {
    Bind { output: String, call: CallExpr },
    Filter(CallExpr),
}

impl CallPredicate {
    pub fn call(&self) -> &CallExpr {
        match self {
            Self::Bind { call, .. } | Self::Filter(call) => call,
        }
    }

    pub fn output(&self) -> Option<&str> {
        match self {
            Self::Bind { output, .. } => Some(output),
            Self::Filter(_) => None,
        }
    }

    fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        match self {
            Self::Bind { call, .. } | Self::Filter(call) => call.lower_symbols(intern),
        }
    }
}

impl fmt::Display for CallPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind { output, call } => write!(f, "{output} = {call}"),
            Self::Filter(call) => write!(f, "{call}"),
        }
    }
}

impl Lexeme for CallPredicate {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        match parsed_rule.as_rule() {
            Rule::call_binding => {
                let mut inner = parsed_rule.into_inner();
                let output = inner.next().unwrap().as_str().to_string();
                let call = CallExpr::from_parsed_rule(inner.next().unwrap());
                Self::Bind { output, call }
            }
            Rule::call_expr => Self::Filter(CallExpr::from_parsed_rule(parsed_rule)),
            _ => unreachable!(),
        }
    }
}

/*
    FLRule: <Head> :- <Predicate>, <Predicate>, ...
    Predicate: <Atom> | !<Atom> | <Comparison> | <Call>
*/

#[derive(Debug, Clone)]
pub enum Predicate {
    AtomPredicate(Atom),
    NegatedAtomPredicate(Atom),
    ComparePredicate(ComparisonExpr),
    CallPredicate(CallPredicate),
}

impl Predicate {
    pub fn arguments(&self) -> Vec<&AtomArg> {
        match self {
            Self::AtomPredicate(atom) => atom.arguments().iter().collect(),
            Self::NegatedAtomPredicate(atom) => atom.arguments().iter().collect(),
            Self::ComparePredicate(_) => panic!("Predicate.arguments() on cmpr"),
            Self::CallPredicate(_) => panic!("Predicate.arguments() on call"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::AtomPredicate(atom) => atom.name(),
            Self::NegatedAtomPredicate(atom) => atom.name(),
            Self::ComparePredicate(_) => panic!("Predicate.name() on cmpr"),
            Self::CallPredicate(call) => call.call().function(),
        }
    }

    fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        match self {
            Self::AtomPredicate(atom) | Self::NegatedAtomPredicate(atom) => {
                atom.lower_symbols(intern)
            }
            Self::ComparePredicate(comparison) => comparison.lower_symbols(intern),
            Self::CallPredicate(call) => call.lower_symbols(intern),
        }
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtomPredicate(atom) => write!(f, "{}", atom),
            Self::NegatedAtomPredicate(atom) => write!(f, "!{}", atom),
            Self::ComparePredicate(expr) => write!(f, "{}", expr),
            Self::CallPredicate(call) => write!(f, "{}", call),
        }
    }
}

impl Lexeme for Predicate {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        match parsed_rule.as_rule() {
            Rule::atom => {
                let atom = Atom::from_parsed_rule(parsed_rule);
                Self::AtomPredicate(atom)
            }
            Rule::neg_atom => {
                // an extra layer of parsing for negation to the atom level (neg_atom >> { "!" ~ atom})
                let inner_rule = parsed_rule.into_inner().next().unwrap();
                let negated_atom = Atom::from_parsed_rule(inner_rule);
                Self::NegatedAtomPredicate(negated_atom)
            }
            Rule::compare_expr => {
                let compare_expr = ComparisonExpr::from_parsed_rule(parsed_rule);
                Self::ComparePredicate(compare_expr)
            }
            Rule::call_binding | Rule::call_expr => {
                Self::CallPredicate(CallPredicate::from_parsed_rule(parsed_rule))
            }
            // `False` in a body is a comparison that never holds; `True` is
            // dropped by the rule parser and never reaches here.
            Rule::BOOLEAN => Self::ComparePredicate(ComparisonExpr::never()),
            _ => unreachable!(),
        }
    }
}

/*
    FLRule: <Head> :- <Predicate>, <Predicate>, ...
*/
#[derive(Debug, Clone)]
pub struct FLRule {
    head: Head,
    rhs: Vec<Predicate>,
    is_planning: bool,
    is_sip: bool,
    line: usize,
    is_sideways: bool,
}

impl fmt::Display for FLRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} :- {}.",
            self.head,
            self.rhs
                .iter()
                .map(|pred| pred.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        )
    }
}

impl FLRule {
    pub fn new(head: Head, rhs: Vec<Predicate>, is_planning: bool, is_sip: bool) -> Self {
        Self { head, rhs, is_planning, is_sip, line: 0, is_sideways: false }
    }

    /// The same rule, remembering the source line it came from.
    pub fn at_line(mut self, line: usize) -> Self {
        self.line = line;
        self
    }

    /// The same rule, marked as one the sideways-passing rewrite produced:
    /// its head is a slice the rewrite reads back, not a relation of the
    /// program.
    pub fn sideways(mut self) -> Self {
        self.is_sideways = true;
        self
    }

    /// Whether the sideways-passing rewrite produced this rule.
    pub fn is_sideways(&self) -> bool {
        self.is_sideways
    }

    pub fn head(&self) -> &Head {
        &self.head
    }

    pub fn rhs(&self) -> &Vec<Predicate> {
        &self.rhs
    }

    pub fn is_planning(&self) -> bool {
        self.is_planning
    }

    pub fn is_sip(&self) -> bool {
        self.is_sip
    }

    /// The source line of the rule, 0 when it was not parsed from text.
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn get(&self, i: usize) -> &Predicate {
        &self.rhs[i]
    }

    pub fn positive_atoms(&self) -> impl Iterator<Item = &Atom> {
        self.rhs.iter().filter_map(|predicate| match predicate {
            Predicate::AtomPredicate(atom) => Some(atom),
            _ => None,
        })
    }

    pub fn negated_atoms(&self) -> impl Iterator<Item = &Atom> {
        self.rhs.iter().filter_map(|predicate| match predicate {
            Predicate::NegatedAtomPredicate(atom) => Some(atom),
            _ => None,
        })
    }

    pub fn comparisons(&self) -> impl Iterator<Item = &ComparisonExpr> {
        self.rhs.iter().filter_map(|predicate| match predicate {
            Predicate::ComparePredicate(comparison) => Some(comparison),
            _ => None,
        })
    }

    pub fn calls(&self) -> impl Iterator<Item = &CallPredicate> {
        self.rhs.iter().filter_map(|predicate| match predicate {
            Predicate::CallPredicate(call) => Some(call),
            _ => None,
        })
    }

    /// Replace every text constant by its symbol.
    pub fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        self.head.lower_symbols(intern)?;
        for predicate in &mut self.rhs {
            predicate.lower_symbols(intern)?;
        }
        Ok(())
    }
}

impl Lexeme for FLRule {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let line = parsed_rule.line_col().0;
        let mut inner_rules = parsed_rule.into_inner();

        /* parsing the head */
        let head = Head::from_parsed_rule(inner_rules.next().unwrap());
        /* parsing the rhs */
        let rhs = inner_rules
            .next()
            .unwrap()
            .into_inner()
            .filter_map(|pred| {
                let pred_inner = pred.into_inner().next().unwrap();
                // `True` contributes nothing to a body.
                if pred_inner.as_rule() == Rule::BOOLEAN && pred_inner.as_str() == "True" {
                    return None;
                }
                Some(Predicate::from_parsed_rule(pred_inner))
            })
            .collect();

        let rule = match inner_rules.next() {
            Some(next) => match next.as_str() {
                ".plan" => Self::new(head, rhs, true, false),
                ".sip" => Self::new(head, rhs, false, true),
                ".optimize" => Self::new(head, rhs, true, true),
                _ => unreachable!(),
            },
            None => Self::new(head, rhs, false, false),
        };
        rule.at_line(line)
    }
}

impl Arithmetic {
    /// Whether this expression mentions no variable.
    pub fn is_constant(&self) -> bool {
        self.vars().is_empty()
    }
}
