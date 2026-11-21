//! The two regimes.
//!
//! Measuring a pipeline on a buffer it has already touched measures the cache, and on a phone the cache
//! is large enough that the answer can be five times wrong. There is no single regime that describes
//! both a glyph redrawn every frame from the same atlas and an image decoded once and scaled down. So
//! both are measured, both are stored, and the dispatcher chooses between them from what it knows about
//! the operation, which the benchmark cannot know.

/// How the working set is prepared before each iteration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Regime {
    /// The target is reused and the sources are already resident.
    ///
    /// Describes an operation repeated on data that is already in flight: a text atlas being redrawn,
    /// a status bar being composited on top of the same background. This is the regime the interface
    /// spends most of its time in, and it is also the one that flatters a pipeline the most.
    Hot,
    /// The target is freshly allocated and the caches are flushed before the iteration.
    ///
    /// Describes an operation performed once on data that has just arrived: the first frame of a
    /// screen, a decoded image being resized, a blur applied to a newly captured view. The allocation
    /// is inside the measured region on purpose, because allocation is part of what the operation
    /// costs on the frame it first runs.
    Cold,
}

impl Regime {
    pub const ALL: [Self; 2] = [Self::Hot, Self::Cold];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Cold => "cold",
        }
    }

    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"hot" => Some(Self::Hot),
            b"cold" => Some(Self::Cold),
            _ => None,
        }
    }

    /// Whether the target is allocated inside the measured region.
    #[must_use]
    pub const fn allocates_per_iteration(self) -> bool {
        matches!(self, Self::Cold)
    }
}

impl core::fmt::Display for Regime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regimes_round_trip_through_their_names() {
        for regime in Regime::ALL {
            assert_eq!(Regime::from_str(regime.as_str()), Some(regime));
        }
        assert_eq!(Regime::from_str("tepid"), None);
    }

    #[test]
    fn only_the_cold_regime_allocates_inside_the_measurement() {
        assert!(!Regime::Hot.allocates_per_iteration());
        assert!(Regime::Cold.allocates_per_iteration());
    }
}
