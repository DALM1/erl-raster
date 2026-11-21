//! The measurement engine, and the profile it leaves behind.
//!
//! Four pieces, each answering one of the ways a benchmark can mislead.
//!
//! [`stats`] refuses to report a mean on its own. [`regime`] separates the measurement of a cache from
//! the measurement of an operation. [`harness`] owns the clock, discards the warmup and interleaves
//! the pipelines. [`profile`] is the only thing that outlives the process, and it carries the thermal
//! state of the device it was taken on, because a profile taken while a phone was throttling describes
//! a phone that no longer exists.
//!
//! Nothing here decides anything. The engine measures and the runtime arbitrates, which is why this
//! crate has no dependency on `erl-adaptive-runtime` and never will.

#![warn(missing_debug_implementations)]

pub mod harness;
pub mod profile;
pub mod regime;
pub mod stats;

pub use harness::{
    describe_failure, measure_switch_costs, run_trials, HarnessConfig, SwitchMeasurement, Trial,
    TrialStatus,
};
pub use profile::{DeviceProfile, ProfileEntry, ProfileError, PROFILE_FORMAT_VERSION};
pub use regime::Regime;
pub use stats::SampleStats;
