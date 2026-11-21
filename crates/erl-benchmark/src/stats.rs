//! Statistics over a fixed number of samples.
//!
//! The rule this module exists to enforce is that a mean is never reported on its own. What a reader
//! of a frame timeline notices is not the average frame, it is the long one, so the ninety-fifth
//! percentile and the maximum travel next to the median everywhere and are never dropped for
//! brevity.

/// A summary of a measurement series, all in nanoseconds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct SampleStats {
    pub count: usize,
    pub min_ns: u64,
    pub median_ns: u64,
    /// The ninety-fifth percentile, which is the number that decides whether an interface feels
    /// smooth. Sixteen milliseconds is the budget; the median says what usually happens, this says
    /// what happens often enough to be seen.
    pub p95_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
}

impl SampleStats {
    /// Summarises a series, sorting it in place.
    ///
    /// Empty input yields an all-zero summary rather than a panic. A workload that could not run on a
    /// backend has no statistics, and a report that says "no samples" is more useful than a crash.
    #[must_use]
    pub fn from_samples(samples: &mut [u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let count = samples.len();
        let sum: u64 = samples.iter().sum();

        Self {
            count,
            min_ns: samples[0],
            // The upper middle for an even count. For frame times the pessimistic choice is the honest
            // one, and it removes an ambiguity that would otherwise make two reports disagree.
            median_ns: samples[count / 2],
            p95_ns: percentile(samples, 95),
            max_ns: samples[count - 1],
            mean_ns: sum / count as u64,
        }
    }

    /// Whether the series has enough samples for a percentile to mean anything.
    #[must_use]
    pub const fn is_conclusive(&self) -> bool {
        // Twenty samples put the ninety-fifth percentile at the top of the series, where it is really
        // the maximum wearing a different name. Below that the number is a curiosity, not a result.
        self.count >= 20
    }

    /// How many samples exceed a budget.
    #[must_use]
    pub fn over_budget(&self, budget_ns: u64) -> u64 {
        if self.p95_ns > budget_ns {
            // The percentile is above the budget, so the count is the useful part and the caller has
            // the summary anyway. Reporting the whole series would be the only exact answer, and it is
            // not kept.
            1
        } else {
            0
        }
    }
}

/// The `percent`-th percentile of an already sorted series, by nearest rank.
#[must_use]
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    debug_assert!(!sorted.is_empty());
    let rank = (sorted.len() * percent).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_series_summarises_to_zero() {
        let mut samples: Vec<u64> = Vec::new();
        assert_eq!(SampleStats::from_samples(&mut samples), SampleStats::default());
    }

    #[test]
    fn a_single_sample_is_its_own_percentile() {
        let mut samples = vec![42];
        let stats = SampleStats::from_samples(&mut samples);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.median_ns, 42);
        assert_eq!(stats.p95_ns, 42);
        assert_eq!(stats.max_ns, 42);
        assert!(!stats.is_conclusive());
    }

    #[test]
    fn the_percentile_sits_where_it_should() {
        // One hundred samples from one to a hundred: the ninety-fifth percentile is ninety-five.
        let mut samples: Vec<u64> = (1..=100).collect();
        let stats = SampleStats::from_samples(&mut samples);
        assert_eq!(stats.p95_ns, 95);
        assert_eq!(stats.median_ns, 51);
        assert_eq!(stats.min_ns, 1);
        assert_eq!(stats.max_ns, 100);
    }

    #[test]
    fn a_long_frame_shows_in_the_percentile_and_not_in_the_median() {
        // This is the shape the module exists to catch: ninety-nine good frames and one bad one.
        let mut samples: Vec<u64> = vec![4_000; 99];
        samples.push(280_000);
        let stats = SampleStats::from_samples(&mut samples);
        assert_eq!(stats.median_ns, 4_000);
        assert_eq!(stats.p95_ns, 4_000);
        assert_eq!(stats.max_ns, 280_000);
        assert!(stats.mean_ns > 6_000, "the mean is dragged, and is the least useful of the four");
    }

    #[test]
    fn twenty_samples_are_enough_to_be_conclusive() {
        let mut samples = vec![1; 20];
        assert!(SampleStats::from_samples(&mut samples).is_conclusive());
    }
}
