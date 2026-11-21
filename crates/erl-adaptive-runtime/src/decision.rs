//! What the runtime decided, and why.
//!
//! A dispatcher that changes the pipeline silently is a dispatcher nobody can debug. Every decision
//! carries the reason it was taken, including the decisions not to move, and the four numbers that
//! carried it: what the incumbent was predicted to cost, what the challenger was predicted to cost,
//! what the move was measured to cost, and how many consecutive wins the challenger had accumulated.
//! These are the fields the report prints, and they are also what makes the policy testable without a
//! phone.

use erl_render_backends::BackendId;

/// The pipeline chosen for one batch, with the reason it was chosen.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The pipeline the batch should run on.
    pub backend: BackendId,
    pub reason: DecisionReason,
    /// What the chosen pipeline is predicted to cost this batch, when anything could predict it.
    pub predicted_ns: Option<u64>,
}

impl Decision {
    /// Whether this decision moved the pipeline.
    #[must_use]
    pub const fn is_switch(&self) -> bool {
        matches!(self.reason, DecisionReason::Switched { .. })
    }
}

/// Why the runtime kept or changed the pipeline.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecisionReason {
    /// The incumbent was already the best candidate.
    IncumbentStable,
    /// The challenger was ahead, but not often enough yet.
    ChallengerNotProven { wins: u32, needed: u32 },
    /// The challenger was ahead, but not by enough to pay the move back and clear the margin.
    GainBelowSwitchCost { gain_ns: u64, cost_ns: u64 },
    /// The move has never been measured on this device, and an unmeasured move is not a free one.
    SwitchCostUnknown { from: BackendId, to: BackendId },
    /// The frame already spent its change.
    SwitchBudgetSpent,
    /// The host declared the frame critical.
    CriticalWindow,
    /// The incumbent cannot run this batch at all.
    ///
    /// Separate from [`DecisionReason::Switched`] because it is not an optimisation. No gain was
    /// weighed, no boot was paid back, and the move is not subject to the one-move-per-frame budget:
    /// without it the batch could not run.
    IncumbentCannotServe { from: BackendId, to: BackendId },
    /// The profile describes a device in a different condition than the one running now.
    StaleProfile,
    /// The challenger proved itself and took the incumbent's place.
    Switched {
        from: BackendId,
        gain_ns: u64,
        cost_ns: u64,
    },
    /// The incumbent stopped matching the profile it was chosen from.
    RegressionDemoted {
        demoted: BackendId,
        predicted_ns: u64,
        observed_ns: u64,
    },
    /// The incumbent failed in a way that means it cannot run here, and the fallback took over.
    UnavailableFallback { missing: BackendId },
}

impl core::fmt::Display for DecisionReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IncumbentStable => f.write_str("the incumbent was already the best candidate"),
            Self::ChallengerNotProven { wins, needed } => {
                write!(f, "the challenger had won {wins} of the {needed} frames it needs")
            }
            Self::GainBelowSwitchCost { gain_ns, cost_ns } => write!(
                f,
                "the challenger was {gain_ns} ns ahead, which does not pay a {cost_ns} ns move"
            ),
            Self::SwitchCostUnknown { from, to } => {
                write!(f, "the move from {from} to {to} has never been measured")
            }
            Self::SwitchBudgetSpent => f.write_str("this frame already changed pipeline"),
            Self::CriticalWindow => f.write_str("the host declared the frame critical"),
            Self::IncumbentCannotServe { from, to } => {
                write!(f, "{from} cannot run this batch, so {to} took it")
            }
            Self::StaleProfile => f.write_str("the profile was taken in a different thermal state"),
            Self::Switched {
                from,
                gain_ns,
                cost_ns,
            } => write!(
                f,
                "{from} gave way after being {gain_ns} ns behind, paying a {cost_ns} ns move"
            ),
            Self::RegressionDemoted {
                demoted,
                predicted_ns,
                observed_ns,
            } => write!(
                f,
                "{demoted} was predicted at {predicted_ns} ns and ran at {observed_ns} ns"
            ),
            Self::UnavailableFallback { missing } => {
                write!(f, "{missing} failed, so the fallback ran")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_completed_move_counts_as_a_switch() {
        let stable = Decision {
            backend: BackendId::CpuScalar,
            reason: DecisionReason::IncumbentStable,
            predicted_ns: Some(10),
        };
        let moved = Decision {
            backend: BackendId::CpuNeon,
            reason: DecisionReason::Switched {
                from: BackendId::CpuScalar,
                gain_ns: 900,
                cost_ns: 100,
            },
            predicted_ns: Some(10),
        };
        assert!(!stable.is_switch());
        assert!(moved.is_switch());
    }

    #[test]
    fn every_reason_says_something() {
        for reason in [
            DecisionReason::IncumbentStable,
            DecisionReason::ChallengerNotProven { wins: 1, needed: 3 },
            DecisionReason::GainBelowSwitchCost {
                gain_ns: 100,
                cost_ns: 200,
            },
            DecisionReason::SwitchCostUnknown {
                from: BackendId::CpuScalar,
                to: BackendId::GpuMetal,
            },
            DecisionReason::SwitchBudgetSpent,
            DecisionReason::CriticalWindow,
            DecisionReason::IncumbentCannotServe {
                from: BackendId::CpuNeon,
                to: BackendId::GpuVulkan,
            },
            DecisionReason::StaleProfile,
            DecisionReason::Switched {
                from: BackendId::CpuScalar,
                gain_ns: 100,
                cost_ns: 20,
            },
            DecisionReason::RegressionDemoted {
                demoted: BackendId::GpuVulkan,
                predicted_ns: 100,
                observed_ns: 900,
            },
            DecisionReason::UnavailableFallback {
                missing: BackendId::GpuMetal,
            },
        ] {
            assert!(!reason.to_string().is_empty(), "{reason:?}");
        }
    }
}
