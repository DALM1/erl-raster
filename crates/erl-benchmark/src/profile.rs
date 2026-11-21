//! The device profile: what was measured, on what, under which conditions.
//!
//! A profile is the only thing that survives a session. The benchmark measures once and the profile
//! carries the result across restarts, so its shape has to answer two questions without ambiguity:
//! which pipeline was fastest, and under which conditions was that true.
//!
//! The second question is the reason the thermal state is part of the identity rather than a field
//! next to it. A profile recorded while a device was throttling describes a device that no longer
//! exists, and looking it up on a cool device would be worse than having no profile at all.

use std::fmt::Write as _;
use std::path::Path;

use erl_raster_core::op::{OpKind, OpKindSet};
use erl_raster_core::system::SystemState;
use erl_render_backends::{BackendId, SwitchCostModel};

use crate::harness::{Trial, TrialStatus};
use crate::regime::Regime;

/// The format version. Written into every file, checked on every read.
pub const PROFILE_FORMAT_VERSION: u32 = 1;

/// What was measured for one pipeline on one workload in one regime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileEntry {
    pub backend: BackendId,
    pub workload: String,
    /// Which kinds of operation the workload contained, so a decision about one kind can be taken from
    /// a measurement of another.
    pub kinds: OpKindSet,
    pub regime: Regime,
    pub median_ns: u64,
    pub p95_ns: u64,
    pub pixels: u64,
    pub transfer_bytes: u64,
}

impl ProfileEntry {
    /// Nanoseconds per pixel of work, which is what makes two workloads comparable at all.
    #[must_use]
    pub fn nanoseconds_per_pixel(&self) -> Option<f64> {
        if self.pixels == 0 {
            return None;
        }
        Some(self.median_ns as f64 / self.pixels as f64)
    }
}

/// Everything known about this device, in this condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceProfile {
    pub state: SystemState,
    pub entries: Vec<ProfileEntry>,
    pub switch_costs: SwitchCostModel,
}

impl DeviceProfile {
    /// Builds a profile from a completed run.
    #[must_use]
    pub fn build(state: SystemState, trials: &[Trial], workloads: &[erl_raster_core::workload::Workload], switch_costs: SwitchCostModel) -> Self {
        let mut entries = Vec::new();

        for trial in trials {
            let TrialStatus::Measured { stats, transferred_bytes, .. } = &trial.status else {
                // A pipeline that did not run has no entry. Recording a zero would make it look like
                // the fastest possible pipeline, and the dispatcher would pick it.
                continue;
            };

            let Some(workload) = workloads.iter().find(|item| item.name == trial.workload) else {
                continue;
            };
            let work = workload.batch.work();

            entries.push(ProfileEntry {
                backend: trial.backend,
                workload: trial.workload.to_string(),
                kinds: workload.batch.kinds(),
                regime: trial.regime,
                median_ns: stats.median_ns,
                p95_ns: stats.p95_ns,
                pixels: work.pixels(),
                transfer_bytes: *transferred_bytes,
            });
        }

        Self {
            state,
            entries,
            switch_costs,
        }
    }

    /// The entry for one pipeline, one workload and one regime.
    #[must_use]
    pub fn entry(&self, backend: BackendId, workload: &str, regime: Regime) -> Option<&ProfileEntry> {
        self.entries
            .iter()
            .find(|entry| entry.backend == backend && entry.workload == workload && entry.regime == regime)
    }

    /// The pipelines that produced at least one usable measurement.
    #[must_use]
    pub fn measured_backends(&self) -> Vec<BackendId> {
        let mut backends: Vec<BackendId> = self.entries.iter().map(|entry| entry.backend).collect();
        backends.sort_unstable();
        backends.dedup();
        backends
    }

    /// The fastest pipeline for a kind, judged on the workloads devoted to that kind alone.
    ///
    /// Single-kind workloads are the only fair basis. Comparing a glyph run against a blur to decide
    /// which pipeline is better at glyphs would be comparing the shapes, not the pipelines.
    #[must_use]
    pub fn fastest_for_kind(&self, kind: OpKind, regime: Regime) -> Option<BackendId> {
        let mut best: Option<(BackendId, u64)> = None;
        for entry in &self.entries {
            if entry.regime != regime || entry.kinds.count() != 1 || !entry.kinds.contains(kind) {
                continue;
            }
            match best {
                Some((_, cost)) if cost <= entry.median_ns => {}
                _ => best = Some((entry.backend, entry.median_ns)),
            }
        }
        best.map(|(backend, _)| backend)
    }

    /// Every pipeline's measured cost for a kind, cheapest first.
    #[must_use]
    pub fn ranking_for_kind(&self, kind: OpKind, regime: Regime) -> Vec<(BackendId, u64)> {
        let mut ranking: Vec<(BackendId, u64)> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.regime == regime && entry.kinds.count() == 1 && entry.kinds.contains(kind)
            })
            .map(|entry| (entry.backend, entry.median_ns))
            .collect();
        ranking.sort_by_key(|(backend, cost)| (*cost, *backend));
        ranking
    }

    /// Serialises the profile to the line-based format.
    ///
    /// A hand-written format rather than a serialisation crate: the MVP has no third-party
    /// dependency, and this file is small enough that a reader can check it by eye, which matters for
    /// something whose whole purpose is to be trusted.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "erl-raster-profile {PROFILE_FORMAT_VERSION}");
        let _ = writeln!(out, "device {}", self.state.device);
        let _ = writeln!(out, "platform {}", self.state.platform);
        let _ = writeln!(out, "thermal {}", self.state.thermal.as_str());

        for entry in &self.entries {
            let _ = writeln!(
                out,
                "entry {} {} {} {} {} {} {} {}",
                entry.backend,
                entry.workload,
                entry.kinds.bits(),
                entry.regime,
                entry.median_ns,
                entry.p95_ns,
                entry.pixels,
                entry.transfer_bytes
            );
        }

        for (from, to, cost) in self.switch_costs.entries() {
            let _ = writeln!(out, "switch {from} {to} {cost}");
        }

        out
    }

    /// Reads a profile back, refusing anything it does not fully understand.
    ///
    /// Strict on purpose. A half-parsed profile would silently produce a dispatcher that believes it
    /// has measurements it does not, and the failure would appear as a wrong pipeline rather than as a
    /// parse error.
    pub fn from_text(text: &str) -> Result<Self, ProfileError> {
        let mut lines = text.lines().filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        });

        let header = lines.next().ok_or(ProfileError::Empty)?;
        let mut header_parts = header.split_whitespace();
        if header_parts.next() != Some("erl-raster-profile") {
            return Err(ProfileError::NotAProfile);
        }
        let version: u32 = header_parts
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or(ProfileError::NotAProfile)?;
        if version != PROFILE_FORMAT_VERSION {
            return Err(ProfileError::UnsupportedVersion(version));
        }

        let mut device = None;
        let mut platform = None;
        let mut thermal = None;
        let mut entries = Vec::new();
        let mut switch_costs = SwitchCostModel::new();

        for (index, line) in lines.enumerate() {
            let mut parts = line.split_whitespace();
            let Some(tag) = parts.next() else {
                continue;
            };
            match tag {
                "device" => device = Some(parts.collect::<Vec<_>>().join(" ")),
                "platform" => platform = Some(parts.collect::<Vec<_>>().join(" ")),
                "thermal" => {
                    thermal = Some(erl_raster_core::system::ThermalState::from_str(
                        parts.next().unwrap_or("unknown"),
                    ))
                }
                "entry" => {
                    let backend = parts
                        .next()
                        .and_then(BackendId::from_str)
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    let workload = parts.next().ok_or(ProfileError::MalformedEntry(index))?.to_string();
                    let kinds = parts
                        .next()
                        .and_then(|value| value.parse::<u8>().ok())
                        .map(OpKindSet::from_bits)
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    let regime = parts
                        .next()
                        .and_then(Regime::from_str)
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    let number = |value: Option<&str>| -> Result<u64, ProfileError> {
                        value
                            .and_then(|value| value.parse::<u64>().ok())
                            .ok_or(ProfileError::MalformedEntry(index))
                    };
                    let median_ns = number(parts.next())?;
                    let p95_ns = number(parts.next())?;
                    let pixels = number(parts.next())?;
                    let transfer_bytes = number(parts.next())?;

                    entries.push(ProfileEntry {
                        backend,
                        workload,
                        kinds,
                        regime,
                        median_ns,
                        p95_ns,
                        pixels,
                        transfer_bytes,
                    });
                }
                "switch" => {
                    let from = parts
                        .next()
                        .and_then(BackendId::from_str)
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    let to = parts
                        .next()
                        .and_then(BackendId::from_str)
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    let cost = parts
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or(ProfileError::MalformedEntry(index))?;
                    switch_costs.record(from, to, cost);
                }
                _ => return Err(ProfileError::UnknownSection(tag.to_string())),
            }
        }

        Ok(Self {
            state: SystemState::new(
                device.ok_or(ProfileError::MissingField("device"))?,
                platform.ok_or(ProfileError::MissingField("platform"))?,
                thermal.ok_or(ProfileError::MissingField("thermal"))?,
            ),
            entries,
            switch_costs,
        })
    }

    /// Writes the profile to a file.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_text())
    }

    /// Reads a profile from a file, or `None` when there is nothing usable there.
    ///
    /// A corrupt profile is treated as an absent one. The alternative, refusing to start, would let a
    /// damaged cache file take an application down.
    #[must_use]
    pub fn load(path: impl AsRef<Path>) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::from_text(&text).ok()
    }
}

/// Why a profile could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileError {
    Empty,
    NotAProfile,
    UnsupportedVersion(u32),
    MissingField(&'static str),
    MalformedEntry(usize),
    UnknownSection(String),
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("the profile was empty"),
            Self::NotAProfile => f.write_str("the file is not an ERL Raster profile"),
            Self::UnsupportedVersion(version) => {
                write!(f, "profile format version {version} is not supported")
            }
            Self::MissingField(field) => write!(f, "the profile has no {field} line"),
            Self::MalformedEntry(line) => write!(f, "entry on line {line} is malformed"),
            Self::UnknownSection(tag) => write!(f, "unknown section {tag}"),
        }
    }
}

impl std::error::Error for ProfileError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Trial, TrialStatus};
    use crate::stats::SampleStats;
    use erl_raster_core::system::ThermalState;
    use erl_raster_core::workload::published_workloads;

    fn sample_profile() -> DeviceProfile {
        let workloads = published_workloads().expect("builds");
        let trials = vec![Trial {
            backend: BackendId::CpuScalar,
            workload: "glyphs.paragraph",
            regime: Regime::Hot,
            status: TrialStatus::Measured {
                stats: SampleStats {
                    count: 30,
                    min_ns: 900,
                    median_ns: 1_000,
                    p95_ns: 1_400,
                    max_ns: 2_000,
                    mean_ns: 1_050,
                },
                transferred_bytes: 0,
                synchronised: false,
            },
        }];
        DeviceProfile::build(
            SystemState::new("SM-G980F", "Android 13", ThermalState::Nominal),
            &trials,
            &workloads,
            SwitchCostModel::new(),
        )
    }

    #[test]
    fn a_profile_survives_a_round_trip() {
        let profile = sample_profile();
        let text = profile.to_text();
        let parsed = DeviceProfile::from_text(&text).expect("parses");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn a_text_that_is_not_a_profile_is_refused() {
        assert_eq!(
            DeviceProfile::from_text("hello world"),
            Err(ProfileError::NotAProfile)
        );
        assert_eq!(
            DeviceProfile::from_text(""),
            Err(ProfileError::Empty)
        );
    }

    #[test]
    fn a_future_version_is_refused_rather_than_half_read() {
        let text = "erl-raster-profile 99\ndevice X\nplatform Y\nthermal nominal\n";
        assert_eq!(
            DeviceProfile::from_text(text),
            Err(ProfileError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn the_fastest_pipeline_for_a_kind_uses_single_kind_workloads_only() {
        let profile = sample_profile();
        assert_eq!(
            profile.fastest_for_kind(OpKind::FillGlyphs, Regime::Hot),
            Some(BackendId::CpuScalar)
        );
        // No blur workload was measured, so there is no answer, and no answer is the right answer.
        assert_eq!(profile.fastest_for_kind(OpKind::Blur, Regime::Hot), None);
    }

    #[test]
    fn an_unmeasured_pipeline_has_no_entry_at_all() {
        let profile = sample_profile();
        assert_eq!(profile.measured_backends(), vec![BackendId::CpuScalar]);
        assert!(profile.entry(BackendId::GpuMetal, "glyphs.paragraph", Regime::Hot).is_none());
    }
}
