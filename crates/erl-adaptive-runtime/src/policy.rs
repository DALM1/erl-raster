//! What the host declares, and what the library decides on its own.
//!
//! The split is deliberate and it is the answer to the fourth ecueil. The library cannot intercept
//! anything, so it must be told the two things only the caller knows: which regime the work belongs to,
//! and whether the frame is one during which nothing may change. Everything else, the arbitration
//! itself, belongs to the library and is not something an application should be asked to get right.

use erl_benchmark::Regime;

/// What only the host can know about the work that is about to run.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameContext {
    /// Whether the operation runs on data already in flight, or on data that has just arrived.
    ///
    /// This is the one thing the benchmark explicitly cannot deduce. It measured both regimes because
    /// it does not know which one a given frame will be in, and only the caller does.
    pub regime: Regime,
    /// Declared by the host while an operation must not change pipeline.
    ///
    /// An animation that is already committed to a compositor, a frame that shares a target with
    /// another pipeline, a capture in progress. While this is set the incumbent runs unchanged and the
    /// evidence for changing it stops accumulating, because `consecutive` is the whole point of it.
    pub critical: bool,
}

impl Default for FrameContext {
    fn default() -> Self {
        Self {
            regime: Regime::Hot,
            critical: false,
        }
    }
}

impl FrameContext {
    /// The ordinary case: work on resident data, free to change pipeline.
    #[must_use]
    pub const fn hot() -> Self {
        Self {
            regime: Regime::Hot,
            critical: false,
        }
    }

    /// The first frame of a screen, where the sources have just arrived.
    #[must_use]
    pub const fn cold() -> Self {
        Self {
            regime: Regime::Cold,
            critical: false,
        }
    }

    /// A frame during which nothing may move.
    #[must_use]
    pub const fn frozen(regime: Regime) -> Self {
        Self {
            regime,
            critical: true,
        }
    }
}

/// The numbers the hysteresis is made of.
///
/// Every one of them exists because of a specific way the naive version fails, and the defaults are
/// the ones the project measures with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DispatchPolicy {
    /// How far ahead of its own cost plus the move a challenger must be, in percent of the incumbent's
    /// cost.
    ///
    /// Without it, two pipelines that measure within a few percent of each other would trade places
    /// forever. The margin has to be wide enough to swallow the measurement noise of a phone, which is
    /// noticeably wider than the noise of a desktop.
    pub minimum_gain_percent: u32,
    /// How many consecutive evaluations a challenger must win before it takes the incumbent's place.
    ///
    /// One lucky frame is not evidence. Three is short enough to react within a handful of frames and
    /// long enough that a single scheduling hiccup cannot move the pipeline.
    pub required_consecutive_wins: u32,
    /// How far above its own measurement an execution must land to count as a regression, in percent.
    pub regression_percent: u32,
    /// How many consecutive regressions before the incumbent is distrusted.
    pub regression_window: u32,
    /// Whether the runtime may change pipeline at all.
    ///
    /// Set false to pin the incumbent. This is the A/B switch the project uses to measure what the
    /// library is worth: the same application, the same load, one run with arbitration and one without.
    pub switching_enabled: bool,
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        Self {
            minimum_gain_percent: 10,
            required_consecutive_wins: 3,
            regression_percent: 150,
            regression_window: 4,
            switching_enabled: true,
        }
    }
}

impl DispatchPolicy {
    /// Whether a challenger is far enough ahead to be worth considering.
    ///
    /// The inequality is a single one on purpose: the gain has to pay the move back and then clear the
    /// margin. Two separate thresholds would let a policy be written in which a move is both approved
    /// and not worth it, which is the kind of contradiction that survives review precisely because it
    /// is spread over two places.
    #[must_use]
    pub fn gain_clears_the_bar(&self, incumbent_ns: u64, challenger_ns: u64, switch_ns: u64) -> bool {
        let Some(gain) = incumbent_ns.checked_sub(challenger_ns) else {
            return false;
        };
        let margin = u128::from(incumbent_ns) * u128::from(self.minimum_gain_percent) / 100;
        let required = u128::from(switch_ns) + margin;
        u128::from(gain) >= required
    }

    /// Whether an observed duration counts as a regression against its own prediction.
    #[must_use]
    pub fn is_regression(&self, predicted_ns: u64, observed_ns: u64) -> bool {
        if predicted_ns == 0 {
            return false;
        }
        u128::from(observed_ns) * 100 > u128::from(predicted_ns) * u128::from(self.regression_percent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gain_that_only_pays_the_move_back_is_refused() {
        let policy = DispatchPolicy::default();
        // The incumbent costs a thousand, the move costs a thousand: the challenger has to be free
        // before the margin is even reached, and free is not something a pipeline is.
        assert!(!policy.gain_clears_the_bar(1_000, 0, 1_000));
        // Paying the move and clearing ten percent of a thousand is not enough either.
        assert!(!policy.gain_clears_the_bar(1_000, 900, 100));
        // A tenth of the incumbent plus the move is the bar.
        assert!(policy.gain_clears_the_bar(10_000, 8_000, 500));
    }

    #[test]
    fn a_challenger_that_is_not_better_is_never_approved() {
        let policy = DispatchPolicy::default();
        assert!(!policy.gain_clears_the_bar(1_000, 1_000, 0));
        assert!(!policy.gain_clears_the_bar(1_000, 2_000, 0));
    }

    #[test]
    fn a_regression_needs_half_again_the_prediction() {
        let policy = DispatchPolicy::default();
        assert!(!policy.is_regression(1_000, 1_499));
        assert!(policy.is_regression(1_000, 1_501));
        // A prediction of zero carries no information, so nothing can regress against it.
        assert!(!policy.is_regression(0, 10_000_000));
    }

    #[test]
    fn a_frozen_context_is_critical_by_definition() {
        assert!(FrameContext::frozen(Regime::Cold).critical);
        assert!(!FrameContext::hot().critical);
        assert_eq!(FrameContext::default(), FrameContext::hot());
    }
}
