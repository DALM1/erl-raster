//! What the device is doing, as far as an application can tell.
//!
//! Both target platforms expose this through platform-specific adapters, so the shape lives here and
//! the reading lives there. The dispatcher keys its decisions on it because a measurement taken on a
//! hot device is not comparable with one taken on a cold device, and the project already learned that
//! the hard way: every reading taken above nominal had to be thrown away rather than interpreted.

use core::fmt;

/// The thermal pressure the platform reports.
///
/// The variant order matters and is used for ordering comparisons: a profile recorded at `Fair` is a
/// lower bound for a device now at `Nominal`, and none of them substitute for one another.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ThermalState {
    /// No throttling. The only state in which an absolute measurement is valid.
    #[default]
    Nominal,
    /// Slightly elevated. Measurements drift, and are kept only to detect further drift.
    Fair,
    /// The platform is actively reducing clocks. Absolute numbers are meaningless.
    Serious,
    /// Emergency. Anything measured here describes a device that is about to shut down.
    Critical,
    /// The platform did not answer, which is not the same as nominal.
    Unknown,
}

impl ThermalState {
    /// Whether a measurement taken in this state may be compared with another.
    ///
    /// Only nominal qualifies. Everything else is recorded and used for relative drift, never as a
    /// basis for choosing a pipeline on absolute grounds.
    #[must_use]
    pub const fn permits_absolute_measurement(self) -> bool {
        matches!(self, Self::Nominal)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Fair => "fair",
            Self::Serious => "serious",
            Self::Critical => "critical",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn from_str(value: &str) -> Self {
        match value.as_bytes() {
            b"nominal" => Self::Nominal,
            b"fair" => Self::Fair,
            b"serious" => Self::Serious,
            b"critical" => Self::Critical,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for ThermalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A snapshot of the conditions a profile belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SystemState {
    /// Platform model, for instance `SM-G980F`.
    pub device: String,
    /// Platform version, for instance `Android 13`.
    pub platform: String,
    /// What the thermal interface last reported.
    pub thermal: ThermalState,
    /// Bytes the process could still allocate without being killed, when the platform says.
    pub available_memory_bytes: Option<u64>,
    /// Depth of the platform's own compositor queue, when it can be observed.
    pub compositor_queue_depth: Option<u32>,
}

impl SystemState {
    /// Builds a state with the two identifying fields filled in and everything else unknown.
    #[must_use]
    pub fn new(device: impl Into<String>, platform: impl Into<String>, thermal: ThermalState) -> Self {
        Self {
            device: device.into(),
            platform: platform.into(),
            thermal,
            available_memory_bytes: None,
            compositor_queue_depth: None,
        }
    }

    /// A key under which a profile is stored: the same hardware in the same thermal state.
    ///
    /// Memory and queue depth are deliberately excluded. They change within a session, and a profile
    /// indexed by them would never be found again.
    #[must_use]
    pub fn profile_key(&self) -> String {
        format!("{}|{}|{}", self.device, self.platform, self.thermal.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_nominal_permits_an_absolute_measurement() {
        assert!(ThermalState::Nominal.permits_absolute_measurement());
        for state in [
            ThermalState::Fair,
            ThermalState::Serious,
            ThermalState::Critical,
            ThermalState::Unknown,
        ] {
            assert!(!state.permits_absolute_measurement(), "{state}");
        }
    }

    #[test]
    fn ordering_puts_nominal_below_every_state_that_throttles() {
        assert!(ThermalState::Nominal < ThermalState::Fair);
        assert!(ThermalState::Fair < ThermalState::Serious);
        assert!(ThermalState::Serious < ThermalState::Critical);
    }

    #[test]
    fn a_profile_key_ignores_what_changes_within_a_session() {
        let mut state = SystemState::new("SM-G980F", "Android 13", ThermalState::Nominal);
        let key = state.profile_key();
        state.available_memory_bytes = Some(1024);
        state.compositor_queue_depth = Some(3);
        assert_eq!(state.profile_key(), key);
    }

    #[test]
    fn thermal_states_round_trip_through_their_names() {
        for state in [
            ThermalState::Nominal,
            ThermalState::Fair,
            ThermalState::Serious,
            ThermalState::Critical,
            ThermalState::Unknown,
        ] {
            assert_eq!(ThermalState::from_str(state.as_str()), state);
        }
    }
}
