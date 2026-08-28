use std::fmt;
use std::sync::Arc;
use catalog::compare::ComparisonExprPos;
use parsing::rule::Const;
use catalog::atoms::{AtomArgumentSignature, AtomSignature};
use crate::collections::{Collection, CollectionSignature};
use crate::calls::CallProjection;
// use crate::compare::ComparisonExprArgument;
use crate::flow::TransformationFlow;

/*
    Collection { Atom { "tc" }, ((y), x) }, Collection { Atom { "arc" }, ((y), z, w) }
    Collection { Atom { "tc" }, ((z), x, w) }

    TransformationFlow::Jn    { 
                                    key:   TransformationArgument::Jn((1, 1, 0)), 
                                    value: TransformationArgument::Jn((0, 1, 0)), TransformationArgument::Jn((1, 1, 2)) 
                                }
*/
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum Transformation {
    /// Pure embedded Rust calls and the final head projection for one rule.
    CallRowToRow {
        input: Arc<Collection>,
        output: Arc<Collection>,
        projection: CallProjection,
    },

    /* direct truncate or filtering, e.g. tc(x, y) :- arc(x, y, _) */
    RowToRow {
        input: Arc<Collection>,
        output: Arc<Collection>,
        flow: TransformationFlow,
        is_no_op: bool,
    },

    /* re-arrange to join */
    RowToK {
        input: Arc<Collection>,
        output: Arc<Collection>,
        flow: TransformationFlow,
        is_no_op: bool,
    },

    RowToKv {
        input: Arc<Collection>,
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    /* filtering intemediates, e.g. tc(x, w) :- arc(x, y), arc(y, z), arc(z, w), x < z */
    KvToKv {
        input: Arc<Collection>,
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    KvToK {
        input: Arc<Collection>,
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    /* An empty output key means the output is a row. A rule head asks for one,
       but so do the operators that read rows rather than arrangements - both
       sides of a `Cartesian` and the input of a `CallRowToRow` - so a
       row-shaped output is not by itself the last transformation of a rule. */
    JnKK {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    JnKKv {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    JnKvK {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    JnKvKv {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    Cartesian {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    /* antijoin */
    NjKvK {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    },

    NjKK {
        input: (Arc<Collection>, Arc<Collection>),
        output: Arc<Collection>,
        flow: TransformationFlow,
    }
}


impl Transformation {
    pub fn unary(&self) -> &Arc<Collection> {
        match self {
            Self::CallRowToRow { input, .. } => input,
            Self::RowToRow { input, .. } => input,
            Self::RowToKv { input, .. } => input,
            Self::RowToK { input, .. } => input,

            _ => panic!("Transformation::single_input: not implemented"),
        }
    }

    pub fn is_unary(&self) -> bool {
        match self {
            Self::CallRowToRow { .. } => true,
            Self::RowToRow { .. } => true,
            Self::RowToKv { .. } => true,
            Self::RowToK { .. } => true,
            _ => false,
        }
    }

    pub fn binary(&self) -> &(Arc<Collection>, Arc<Collection>) {
        match self {
            Self::JnKK { input, .. } => input,
            Self::JnKKv { input, .. } => input,
            Self::JnKvK { input, .. } => input,
            Self::JnKvKv { input, .. } => input,
            Self::Cartesian { input, .. } => input,
            Self::NjKvK { input, .. } => input,
            Self::NjKK { input, .. } => input,
            _ => panic!("Transformation::binary: not implemented"),
        }
    }

    pub fn output(&self) -> &Arc<Collection> {
        match self {
            Self::CallRowToRow { output, .. } => output,
            Self::RowToRow { output, .. } => output,
            Self::RowToKv { output, .. } => output,
            Self::RowToK { output, .. } => output,

            Self::KvToKv { output, .. } => output,
            Self::KvToK { output, .. } => output,

            Self::JnKK { output, .. } => output,
            Self::JnKKv { output, .. } => output,
            Self::JnKvK { output, .. } => output,
            Self::JnKvKv { output, .. } => output,

            Self::Cartesian { output, .. } => output,

            Self::NjKvK { output, .. } => output,
            Self::NjKK { output, .. } => output,
        }
    }

    pub fn flow(&self) -> &TransformationFlow {
        match self {
            Self::CallRowToRow { .. } => {
                panic!("Transformation::flow() called on an embedded call projection")
            }
            Self::RowToRow { flow, .. } => flow,
            Self::RowToKv { flow, .. } => flow,
            Self::RowToK { flow, .. } => flow,

            Self::KvToKv { flow, .. } => flow,
            Self::KvToK { flow, .. } => flow,

            Self::JnKK { flow, .. } => flow,
            Self::JnKKv { flow, .. } => flow,
            Self::JnKvK { flow, .. } => flow,
            Self::JnKvKv { flow, .. } => flow,
            Self::Cartesian { flow, .. } => flow,
            Self::NjKvK { flow, .. } => flow,
            Self::NjKK { flow, .. } => flow,
        }
    }

    pub fn kv_to_kv(input: Arc<Collection>, 
                    output_key_signatures: &Vec<AtomArgumentSignature>, 
                    output_value_signatures: &Vec<AtomArgumentSignature>,
                    const_eq_constraints: &Vec<(AtomArgumentSignature, Const)>, 
                    var_eq_constraints: &Vec<(AtomArgumentSignature, AtomArgumentSignature)>,
                    compare_exprs: &Vec<ComparisonExprPos>
                ) -> Self {
        let (input_key_signatures, input_value_signatures) = input.kv_argument_signatures();
        let flow = 
            TransformationFlow::kv_to_kv(
                input_key_signatures, 
                input_value_signatures, 
                output_key_signatures, 
                output_value_signatures, 
                const_eq_constraints, 
                var_eq_constraints, 
                compare_exprs
            );
        
        let is_row_in = input_key_signatures.is_empty();
        let is_row_out = output_key_signatures.is_empty();
        let is_key_only_out = output_value_signatures.is_empty();

        // input signatures is identical to output signatures and there are no filters
        let is_no_op = is_row_in && 
            (is_row_out || is_key_only_out) &&
            const_eq_constraints.is_empty() && var_eq_constraints.is_empty() && compare_exprs.is_empty() &&
            input_key_signatures
                .iter()
                .chain(input_value_signatures.iter())
                .eq(
                    output_key_signatures
                        .iter()
                        .chain(output_value_signatures.iter())
                );

        let input_signature_name = input.signature().name();
        let output_name = match (is_row_out, is_key_only_out) {
            (true, false) => format!("Row({}){}", input_signature_name, flow),
            (false, true) => format!("K({}){}", input_signature_name, flow),
            (false, false) => format!("Kv({}){}", input_signature_name, flow),
            // Retaining no column is a signature, not a missing one. It is what
            // an atom used purely as an existential guard asks for - every
            // position a wildcard, or every variable dead downstream, as in
            // `out(A) :- s(A), t(_, _).` - and it means the atom contributes
            // whether it has a row and nothing else. The projection is the
            // relation's 0-column image: empty when the relation is empty, one
            // row otherwise.
            (true, true) => format!("Exists({}){}", input_signature_name, flow),
        };
           
        let output = 
            Arc::new(
                Collection::new(
                    CollectionSignature::UnaryTransformationOutput { name: output_name },
                    output_key_signatures,
                    output_value_signatures
                )
            );  

        match (is_row_in, is_row_out, is_key_only_out) {
            (true, true, _) => Self::RowToRow { input, output, flow, is_no_op },
            (true, false, true) => Self::RowToK { input, output, flow, is_no_op },
            (true, false, false) => Self::RowToKv { input, output, flow },
            (false, false, false) => Self::KvToKv { input, output, flow },
            (false, false, true) => Self::KvToK { input, output, flow },
            _ => panic!("Transformation::kv_to_kv: unexpected kv to row transformation"),
        }
    }

    pub fn call_projection(input: Arc<Collection>, projection: CallProjection) -> Self {
        assert!(
            input.key_argument_signatures().is_empty(),
            "embedded calls must run over a row collection"
        );
        assert!(
            !projection.head().is_empty(),
            "embedded call projection must emit a non-empty head"
        );

        let output_value_signatures = (0..projection.head().len())
            .map(|argument_id| {
                AtomArgumentSignature::new(AtomSignature::new(true, usize::MAX), argument_id)
            })
            .collect::<Vec<_>>();
        let output = Arc::new(Collection::new(
            CollectionSignature::UnaryTransformationOutput {
                name: format!("Call({}){}", input.signature().name(), projection),
            },
            &vec![],
            &output_value_signatures,
        ));

        Self::CallRowToRow {
            input,
            output,
            projection,
        }
    }

    pub fn join(input: (Arc<Collection>, Arc<Collection>),
                output_key_signatures: &Vec<AtomArgumentSignature>,
                output_value_signatures: &Vec<AtomArgumentSignature>,
                compare_exprs: &Vec<ComparisonExprPos>
            ) -> Self {
        let (left_key_signatures, left_value_signatures) = input.0.kv_argument_signatures();
        let (right_key_signatures, right_value_signatures) = input.1.kv_argument_signatures();

        let flow = 
            TransformationFlow::join_to_kv(
                left_key_signatures, 
                left_value_signatures, 
                right_key_signatures, 
                right_value_signatures, 
                output_key_signatures, 
                output_value_signatures,
                compare_exprs
            );
        
        let is_key_only_left_input = left_value_signatures.is_empty();
        let is_key_only_right_input = right_value_signatures.is_empty();
        let is_cartesian = left_key_signatures.is_empty();
        let left_signature_name = input.0.signature().name();
        let right_signature_name = input.1.signature().name();

        let output =
            Arc::new(
                Collection::new(
                    CollectionSignature::JnOutput {
                        name: match (is_cartesian, is_key_only_left_input, is_key_only_right_input) {
                            (true, _, _) => format!("Cartesian({}, {}){}", left_signature_name, right_signature_name, flow),
                            (_, true, true) => format!("JnKK({}, {}){}", left_signature_name, right_signature_name, flow),
                            (_, false, true) => format!("JnKvK({}, {}){}", left_signature_name, right_signature_name, flow),
                            (_, false, false) => format!("JnKvKv({}, {}){}", left_signature_name, right_signature_name, flow),
                            (_, true, false) => format!("JnKKv({}, {}){}", left_signature_name, right_signature_name, flow),
                        }
                    },
                    output_key_signatures,
                    output_value_signatures
                )
            );
        
        if is_cartesian {
            Self::Cartesian { input, output, flow }
        } else {
            match (is_key_only_left_input, is_key_only_right_input) {
                (true, true) => Self::JnKK { input, output, flow },
                (false, true) => Self::JnKvK { input, output, flow },
                (false, false) => Self::JnKvKv { input, output, flow },
                (true, false) => Self::JnKKv { input, output, flow },
            }
        }
    }

    pub fn antijoin(input: (Arc<Collection>, Arc<Collection>),
                    output_key_signatures: &Vec<AtomArgumentSignature>, 
                    output_value_signatures: &Vec<AtomArgumentSignature>
            ) -> Self {
        let (left_key_signatures, left_value_signatures) = input.0.kv_argument_signatures();
        let (right_key_signatures, right_value_signatures) = input.1.kv_argument_signatures();

        assert!(right_value_signatures.is_empty(), "Transformation::antijoin right_value_signatures must be empty");
        
        let flow = 
            TransformationFlow::join_to_kv(
                left_key_signatures, 
                left_value_signatures, 
                right_key_signatures, 
                right_value_signatures, 
                output_key_signatures, 
                output_value_signatures,
                &vec![] // there shouldn't be any compare_exprs for antijoins
            );
        
        let is_key_only_left_input = left_value_signatures.is_empty();
        let output = 
            Arc::new(
                Collection::new(
                    CollectionSignature::NegJnOutput { 
                        name: if is_key_only_left_input { 
                                format!("NjKK({}, {}){}", input.0.signature().name(), input.1.signature().name(), flow) 
                              } else { 
                                format!("NjKvK({}, {}){}", input.0.signature().name(), input.1.signature().name(), flow) 
                              },
                    },
                    output_key_signatures,
                    output_value_signatures
                )
            );
        
        match is_key_only_left_input {
            true => Self::NjKK { input, output, flow },
            false => Self::NjKvK { input, output, flow },
        }
    }
}


impl fmt::Display for Transformation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallRowToRow {
                output,
                projection,
                ..
            } => write!(f, "@ {} {}", projection, output.pprint()),
            Self::RowToRow { output, is_no_op, .. } => {
                write!(
                    f,
                    "{} {}",
                    if *is_no_op { "∅" } else { "→" }, output.pprint(),
                )
            }
            Self::RowToK { output, is_no_op, .. } => {
                write!(
                    f,
                    "{} {}",
                    if *is_no_op { "∅" } else { "⟶" }, output.pprint(),
                )
            }
            Self::RowToKv { output, .. } => {
                write!(
                    f,
                    "⟶ {}",
                    output.pprint(),
                )
            }
            Self::KvToKv { output, .. } | Self::KvToK { output, .. } => {
                write!(
                    f,
                    "⟶ {}",
                    output.pprint(),
                )
            }
            Self::JnKK { output, .. } | Self::JnKKv { output, .. } | Self::JnKvK { output, .. } | Self::JnKvKv { output, .. } => {
                write!(
                    f,
                    "⋈ {}", // bowtie for join
                    output.pprint(),
                )
            }
            Self::Cartesian { output, .. } => {
                write!(
                    f,
                    "⨯ {}", 
                    output.pprint(),
                )
            }
            Self::NjKvK { output, .. } | Self::NjKK { output, .. } => {
                write!(
                    f,
                    "¬ {}", // negation symbol for antijoin
                    output.pprint(),
                )
            }
            // _ => panic!("Transformation::fmt: not implemented"),
        }
    }
}
