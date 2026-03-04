//! Round budget conversions.
//!
//! The CodexPotter app-server protocol represents round counts as `u32`. The CLI parses `--rounds`
//! as a `NonZeroUsize` for ergonomic arithmetic, but we must reject values that cannot be
//! represented in the protocol instead of silently clamping them.

use std::num::NonZeroUsize;

/// Convert a non-zero round budget into the `u32` representation used by the app-server protocol.
///
/// Returns an error when the value cannot be represented as `u32` instead of silently clamping.
pub fn round_budget_to_u32(rounds: NonZeroUsize) -> anyhow::Result<u32> {
    let rounds_usize = rounds.get();
    u32::try_from(rounds_usize).map_err(|_| {
        let max_rounds = u32::MAX;
        anyhow::anyhow!("--rounds must be <= {max_rounds}, got {rounds_usize}")
    })
}

/// Convert a `usize` value into `u32`, producing diagnostics that mention `label`.
///
/// This is useful when internal counts are tracked as `usize` but must be sent over the wire as
/// `u32`.
pub fn usize_to_u32(value: usize, label: &'static str) -> anyhow::Result<u32> {
    u32::try_from(value).map_err(|_| {
        let max = u32::MAX;
        anyhow::anyhow!("{label} must be <= {max}, got {value}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_budget_to_u32_accepts_small_values() {
        assert_eq!(
            round_budget_to_u32(NonZeroUsize::new(1).expect("rounds")).expect("rounds u32"),
            1
        );
    }

    #[test]
    fn round_budget_to_u32_accepts_u32_max() {
        assert_eq!(
            round_budget_to_u32(NonZeroUsize::new(u32::MAX as usize).expect("rounds"))
                .expect("rounds u32"),
            u32::MAX
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn round_budget_to_u32_rejects_values_larger_than_u32_max() {
        let rounds = (u64::from(u32::MAX) + 1) as usize;
        let err = round_budget_to_u32(NonZeroUsize::new(rounds).expect("rounds")).unwrap_err();
        assert!(
            err.to_string().contains("4294967295"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn usize_to_u32_accepts_small_values() {
        assert_eq!(usize_to_u32(0, "value").expect("u32"), 0);
        assert_eq!(usize_to_u32(1, "value").expect("u32"), 1);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn usize_to_u32_rejects_values_larger_than_u32_max() {
        let value = (u64::from(u32::MAX) + 1) as usize;
        let err = usize_to_u32(value, "value").unwrap_err();
        assert!(
            err.to_string().contains("4294967295"),
            "unexpected error: {err:#}"
        );
    }
}
