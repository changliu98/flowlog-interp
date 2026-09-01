use differential_dataflow::difference::{IsZero, Monoid, Multiply, Semigroup};
use parsing::Val;
use serde::{Deserialize, Serialize};

// Re-export Present for convenience
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
use differential_dataflow::difference::Present;

// Conditional compilation for semiring type selection
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub type Semiring = Present;

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub type Semiring = isize;

// Helper function to create the appropriate semiring value
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub fn semiring_one() -> Semiring {
    Present {}
}

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub fn semiring_one() -> Semiring {
    1
}

/// Convert an engine difference into an integer weight for materializing a
/// relation outside the dataflow. `Present` can only report presence, whereas
/// the incremental build can also report retractions.
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub fn semiring_weight(_difference: &Semiring) -> isize {
    1
}

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub fn semiring_weight(difference: &Semiring) -> isize {
    *difference
}

// Compile-time check to ensure exactly one semiring feature is enabled
#[cfg(all(feature = "present-type", feature = "isize-type"))]
compile_error!("Cannot enable both present-type and isize-type features at once");

#[cfg(not(any(feature = "present-type", feature = "isize-type")))]
compile_error!("Must enable exactly one semiring feature: either present-type or isize-type");

// Debug: expose which semiring type is active
#[cfg(all(feature = "present-type", not(feature = "isize-type")))]
pub const SEMIRING_TYPE: &str = "Present";

#[cfg(all(feature = "isize-type", not(feature = "present-type")))]
pub const SEMIRING_TYPE: &str = "isize";

/// MIN Semiring
///
/// The carried value is a `Val`, the engine's own value domain, so that the
/// derived `Ord` and the `min` below are the order of the values being
/// aggregated. An unsigned carrier would need an order-preserving encoding of
/// the signed domain, and the plain cast that stood here instead reversed the
/// order of every negative value.
#[derive(Copy, Debug, Clone, Hash, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
pub struct Min {
    pub value: Val,
}

impl Min {
    /// Creates a new `Min` with a value.
    pub fn new(value: Val) -> Self {
        Min { value }
    }

    /// Creates a new `Min` representing infinity (`Val::MAX`).
    /// This serves as the additive identity in the MIN semiring:
    /// min(a, ∞) = a for any value a.
    ///
    /// The identity is also a legal value, which is harmless: `min` is
    /// idempotent, so a group whose true minimum is `Val::MAX` combines to
    /// `Val::MAX`, and the identity is never treated as an absence
    /// (`is_zero` is constantly false).
    pub fn infinity() -> Self {
        Min { value: Val::MAX }
    }

    /// Returns true if this Min represents infinity.
    pub fn is_infinity(&self) -> bool {
        self.value == Val::MAX
    }
}

impl IsZero for Min {
    fn is_zero(&self) -> bool {
        false // always return false
    }
}

impl Semigroup for Min {
    fn plus_equals(&mut self, rhs: &Self) {
        self.value = std::cmp::min(self.value, rhs.value);
    }
}

impl Monoid for Min {
    fn zero() -> Self {
        Min::infinity() // additive identity is infinity
    }
}

// For converting i64 differences to Min (preserves the Min value)
impl Multiply<i64> for Min {
    type Output = Min;

    fn multiply(self, _rhs: &i64) -> Self::Output {
        self
    }
}

// Convenience implementations for easier use
impl From<Val> for Min {
    fn from(value: Val) -> Self {
        Min::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_semigroup() {
        let mut a = Min::new(5);
        let b = Min::new(3);
        a.plus_equals(&b);
        assert_eq!(a.value, 3);

        let mut inf = Min::infinity();
        inf.plus_equals(&Min::new(42));
        assert_eq!(inf.value, 42);
    }

    #[test]
    fn min_identity_is_not_a_zero_diff() {
        let zero = Min::zero();
        assert!(!zero.is_zero());
        assert!(zero.is_infinity());
        assert_eq!(zero.value, Val::MAX);
    }

    #[test]
    fn min_orders_negative_values_below_positive_ones() {
        let mut accumulated = Min::new(3);
        accumulated.plus_equals(&Min::new(-4));
        assert_eq!(accumulated.value, -4);

        // The threshold operator of the specialised min path compares two
        // differences directly, so the derived order has to be the value order.
        assert!(Min::new(-4) < Min::new(3));
        assert!(Min::new(-4) < Min::infinity());
        assert!(Min::new(Val::MIN) < Min::new(0));
    }
}
