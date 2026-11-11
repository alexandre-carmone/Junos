//! The "None" algorithm: the move is the measured offset, unchanged. What
//! PHD2 builds for `GUIDE_ALGORITHM_NONE` and `GUIDE_ALGORITHM_IDENTITY`.
//! No parameters, no min-move, no history.
//!
//! Ported from PHD2 src/guide_algorithm_identity.h, src/guide_algorithm_identity.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::{AlgoCtx, AlgoKind, GuideAlgorithm};

/// `GuideAlgorithmIdentity`: `result(x) = x`.
#[derive(Debug, Clone, Default)]
pub struct Identity;

impl Identity {
    pub fn new() -> Self {
        Identity
    }
}

impl GuideAlgorithm for Identity {
    fn kind(&self) -> AlgoKind {
        AlgoKind::Identity
    }

    fn reset(&mut self) {}

    fn result(&mut self, input: f64, _ctx: &AlgoCtx) -> f64 {
        input
    }

    /// A lone newline (`guide_algorithm_identity.h:66`): the guide log's
    /// "X guide algorithm = None, " line ends there, and the Y line starts
    /// on its own.
    fn settings_summary(&self) -> String {
        "\n".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_the_input_through() {
        let mut a = Identity::new();
        let ctx = AlgoCtx::default();
        for x in [0.0, 0.01, -0.05, 1.5, -12.25] {
            assert_eq!(a.result(x, &ctx), x);
        }
        assert_eq!(a.kind(), AlgoKind::Identity);
        assert_eq!(a.class_name(), "Identity");
        assert!(a.params().is_empty());
        assert_eq!(a.settings_summary(), "\n");
    }
}
