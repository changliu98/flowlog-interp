use std::fmt;
use std::collections::HashSet;
use crate::diagnostic::Result;
use crate::rule::Const;
use crate::{parser::Lexeme, Rule, Val};
use crate::arithmetic::Arithmetic;
use pest::iterators::Pair;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ComparisonOperator {
    Equals,
    NotEquals,
    GreaterThan,
    GreaterEqualThan,
    LessThan,
    LessEqualThan,
}

impl ComparisonOperator {
    pub fn is_equals(&self) -> bool {
        match self {
            Self::Equals => true,
            _ => false,
        }
    }

    /// Whether the operator orders its operands, which only numbers support.
    pub fn is_ordered(&self) -> bool {
        !matches!(self, Self::Equals | Self::NotEquals)
    }
}

impl fmt::Display for ComparisonOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            &ComparisonOperator::Equals => {
                write!(f, "==")
            }
            &ComparisonOperator::NotEquals => {
                write!(f, "≠")
            }
            &ComparisonOperator::GreaterThan => {
                write!(f, ">")
            }
            &ComparisonOperator::GreaterEqualThan => {
                write!(f, "≥")
            }
            &ComparisonOperator::LessThan => {
                write!(f, "<")
            }
            &ComparisonOperator::LessEqualThan => {
                write!(f, "≤")
            }
        }
    }
}

impl Lexeme for ComparisonOperator {
    fn from_parsed_rule(compare_operator_rule: Pair<Rule>) -> Self {
        let operator = compare_operator_rule.into_inner().next().unwrap();
        match operator.as_rule() {
            Rule::equals => ComparisonOperator::Equals,
            Rule::not_equals => ComparisonOperator::NotEquals,
            Rule::greater_than => ComparisonOperator::GreaterThan,
            Rule::greater_equal_than => ComparisonOperator::GreaterEqualThan,
            Rule::less_than => ComparisonOperator::LessThan,
            Rule::less_equal_than => ComparisonOperator::LessEqualThan,
            _ => unreachable!(),
        }
    }
}


#[derive(Debug, Clone)]
pub struct ComparisonExpr {
    left: Arithmetic,
    operator: ComparisonOperator,
    right: Arithmetic,
}

impl ComparisonExpr {
    pub fn new(left: Arithmetic, operator: ComparisonOperator, right: Arithmetic) -> Self {
        Self { left, operator, right }
    }

    /// A comparison that never holds: what `False` in a body means.
    pub fn never() -> Self {
        Self::new(
            Arithmetic::constant(Const::Integer(0)),
            ComparisonOperator::Equals,
            Arithmetic::constant(Const::Integer(1)),
        )
    }

    pub fn left(&self) -> &Arithmetic {
        &self.left
    }

    pub fn operator(&self) -> &ComparisonOperator {
        &self.operator
    }

    pub fn right(&self) -> &Arithmetic {
        &self.right
    }

    pub fn vars_set(&self) -> HashSet<&String> {
        self.left.vars_set().union(&self.right.vars_set()).cloned().collect()
    }

    pub fn left_vars(&self) -> Vec<&String> {
        self.left.vars()
    }

    pub fn right_vars(&self) -> Vec<&String> {
        self.right.vars()
    }

    pub fn lower_symbols(&mut self, intern: &mut dyn FnMut(&str) -> Result<Val>) -> Result<()> {
        self.left.lower_symbols(intern)?;
        self.right.lower_symbols(intern)
    }
}

impl fmt::Display for ComparisonExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} {} {}]", self.left, self.operator, self.right)
    }
}

impl Lexeme for ComparisonExpr {
    fn from_parsed_rule(parsed_rule: Pair<Rule>) -> Self {
        let mut inner_rule = parsed_rule.into_inner();
        let left = Arithmetic::from_parsed_rule(inner_rule.next().unwrap());
        let operator = ComparisonOperator::from_parsed_rule(inner_rule.next().unwrap());
        let right = Arithmetic::from_parsed_rule(inner_rule.next().unwrap());

        ComparisonExpr { left, operator, right }
    }
}
