//! Build-time configuration constants for FlowLog
//!
//! These constants control the compile-time code generation and runtime limits
//! for various operations in the FlowLog engine.

/// Maximum arity for key-value in code generation
pub const KV_MAX: usize = 4;

/// Maximum arity for row in code generation  
pub const ROW_MAX: usize = 7;

/// Maximum arity for product in code generation
pub const PROD_MAX: usize = 2;

/// Cells a fat row holds inline; a wider row keeps its cells on the heap.
/// Most rows of a fat stratum are narrow, and 4 rather than `ROW_MAX` ran the
/// rlcrafter workloads up to a fifth faster in up to a third less memory.
pub const FALLBACK_ARITY: usize = 4;

/// Configuration for compile-time code generation limits
pub struct CodegenLimits;

impl CodegenLimits {
    pub const KV_MAX: usize = KV_MAX;
    pub const ROW_MAX: usize = ROW_MAX;
    pub const PROD_MAX: usize = PROD_MAX;
    pub const FALLBACK_ARITY: usize = FALLBACK_ARITY;
}
