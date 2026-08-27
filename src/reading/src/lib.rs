pub mod arrangements;
pub mod config;
pub mod inspect;
pub mod reader;
pub mod rel;
pub mod row;
pub mod session;
pub mod semiring;

// export configuration constants for backwards compatibility
pub use config::{FALLBACK_ARITY, KV_MAX, PROD_MAX, ROW_MAX};

// export semiring types and functions for convenience
pub use semiring::{Semiring, semiring_one, SEMIRING_TYPE, Min};

// the engine-wide scalar value domain, defined once in `parsing`
pub use parsing::Val;

// feature propagation through dependency chain && mutually exclusive feature configuration
// workspace
//     ↓ --features isize-type
// executing crate
//     ↓ enables isize-type = ["reading/isize-type", "macros/isize-type"]
// macros crate
//     ↓ enables isize-type = ["reading/isize-type"]
// reading crate
//     ↓ compiles with isize type

pub type Time = ();

/// The iteration counter of a recursive stratum's inner timestamp.
///
/// Known limit: 65,535 iterations. A fixed point that needs more overflows the
/// counter rather than reporting anything, so a program whose recursion depth
/// can exceed that - long chains under a rule that advances one step per round
/// - is outside what this width supports. Widening it costs memory and
/// comparison work in every timestamp of every recursive dataflow.
pub type Iter = u16;