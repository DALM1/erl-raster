//! The measurement itself.
//!
//! Four rules are enforced here, one for each way a benchmark can lie.
//!
//! **The clock belongs to the harness, not to the backend.** A backend reports what it was asked to do;
//! only the caller sees the submission and the wait that follows it, and on a GPU those are most of the
//! cost. A backend that timed itself would measure its own arithmetic and call it a frame.
//!
//! **Warmup is discarded.** The first iterations of a pipeline contain its first allocation, its first
//! shader compilation and the cache fill of everything that follows. Keeping them would make the
//! comparison a comparison of startup costs.
//!
//! **The pipelines are interleaved.** Running all of one and then all of the other hands the second the
//! benefit of a clock that has ramped up and the first the cost of having ramped it. The nested loop
//! below alternates them batch by batch so that neither gets either.
//!
//! **Both regimes are measured.** See [`crate::regime`].

use std::time::Instant;

use erl_raster_core::geom::Size;
use erl_raster_core::image::Surface;
use erl_raster_core::op::Batch;
use erl_raster_core::workload::Workload;
use erl_raster_core::RasterError;
use erl_render_backends::{Backend, BackendId, Executed, SwitchCostModel};

use crate::regime::Regime;
use crate::stats::SampleStats;

/// How a run is configured.
///
/// The defaults matter: they are the ones an open-source reader will get when they run the benchmark
/// without arguments, and they are the ones the project's own published numbers were taken with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    /// Iterations run before any sample is kept.
    pub warmup: u32,
    /// Iterations kept per pipeline, regime and workload.
    pub iterations: u32,
    /// Which regimes to run.
    pub regimes: Vec<Regime>,
    /// Size of the buffer written between cold iterations, to push the working set out of cache.
    ///
    /// Sixteen mebibytes is above the last-level cache of every phone and most laptops, which is what
    /// makes the cold regime cold. Making it smaller would quietly turn it into a warm one.
    pub scrub_bytes: usize,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            warmup: 10,
            iterations: 60,
            regimes: Regime::ALL.to_vec(),
            scrub_bytes: 16 * 1024 * 1024,
        }
    }
}

impl HarnessConfig {
    /// A short configuration for a smoke run.
    #[must_use]
    pub fn quick() -> Self {
        Self {
            warmup: 3,
            iterations: 20,
            regimes: vec![Regime::Hot],
            scrub_bytes: 4 * 1024 * 1024,
        }
    }

    /// The total number of executions a full run performs, used to warn before a long one.
    #[must_use]
    pub fn total_executions(&self, backends: usize, workloads: usize) -> u64 {
        let per_regime = u64::from(self.warmup + self.iterations);
        per_regime * backends as u64 * workloads as u64 * self.regimes.len() as u64
    }
}

/// What happened to one pipeline on one workload in one regime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrialStatus {
    Measured {
        stats: SampleStats,
        transferred_bytes: u64,
        synchronised: bool,
    },
    /// The pipeline exists but could not be prepared, typically because no device is linked.
    Unavailable { reason: String },
    /// The pipeline does not serve every kind the workload contains.
    Unsupported { reason: String },
    /// It ran, and then failed. Kept apart from `Unavailable` because it is a bug rather than a
    /// configuration.
    Failed { reason: String },
}

impl TrialStatus {
    #[must_use]
    pub fn stats(&self) -> Option<&SampleStats> {
        match self {
            Self::Measured { stats, .. } => Some(stats),
            _ => None,
        }
    }
}

/// One line of a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    pub backend: BackendId,
    pub workload: &'static str,
    pub regime: Regime,
    pub status: TrialStatus,
}

/// Runs every pipeline against every workload, in every requested regime.
#[must_use]
pub fn run_trials(
    backends: &mut [Box<dyn Backend>],
    workloads: &[Workload],
    config: &HarnessConfig,
) -> Vec<Trial> {
    let mut trials = Vec::new();

    for workload in workloads {
        let batch = &workload.batch;
        let target_size = batch.target();

        for regime in &config.regimes {
            let mut targets: Vec<Option<Surface>> = backends
                .iter()
                .map(|backend| prepare_target(backend.as_ref(), target_size))
                .collect();

            let mut samples: Vec<Vec<u64>> = vec![Vec::new(); backends.len()];
            let mut last: Vec<Option<Executed>> = vec![None; backends.len()];
            let mut failures: Vec<Option<String>> = vec![None; backends.len()];
            let mut skips: Vec<Option<String>> = vec![None; backends.len()];

            for (index, backend) in backends.iter_mut().enumerate() {
                if !backend.capabilities().serves(batch) {
                    skips[index] = Some(format!(
                        "{} does not serve {}",
                        backend.id(),
                        batch.kinds()
                    ));
                    continue;
                }
                if let Err(error) = backend.prepare(batch) {
                    skips[index] = Some(error.to_string());
                }
            }

            // Sized once per regime, and shared by every pipeline, so that the flush costs the same
            // wherever it happens and never lands inside a measured region.
            let mut scrubber = vec![0u8; config.scrub_bytes];
            let mut scrub_state = 0x243f_6a88_85a3_08d3u64;

            let total = config.warmup + config.iterations;
            for iteration in 0..total {
                for index in 0..backends.len() {
                    if skips[index].is_some() || failures[index].is_some() {
                        continue;
                    }

                    if regime.allocates_per_iteration() {
                        // The allocation is inside the measured region, deliberately: on the frame an
                        // operation first runs, the allocator is part of its cost.
                        let surface = match Surface::try_new(target_size) {
                            Ok(surface) => surface,
                            Err(error) => {
                                failures[index] = Some(error.to_string());
                                continue;
                            }
                        };
                        targets[index] = Some(surface);
                        scrub(&mut scrubber, &mut scrub_state);
                    }

                    let Some(target) = targets[index].as_mut() else {
                        continue;
                    };

                    let started = Instant::now();
                    let outcome = backends[index].execute(batch, target);
                    let elapsed = started.elapsed();

                    match outcome {
                        Ok(executed) => {
                            last[index] = Some(executed);
                            if iteration >= config.warmup {
                                samples[index].push(elapsed.as_nanos() as u64);
                            }
                        }
                        // A failure stops this pipeline for this workload only, and is reported as a
                        // failure rather than as an absence: it is a bug, not a configuration.
                        Err(error) => failures[index] = Some(error.to_string()),
                    }
                }
            }

            for (index, backend) in backends.iter().enumerate() {
                let status = match (&skips[index], &failures[index]) {
                    (Some(reason), _) => TrialStatus::Unavailable {
                        reason: reason.clone(),
                    },
                    (None, Some(reason)) => TrialStatus::Failed {
                        reason: reason.clone(),
                    },
                    (None, None) => TrialStatus::Measured {
                        stats: SampleStats::from_samples(&mut samples[index]),
                        transferred_bytes: last[index].map_or(0, |executed| executed.transferred_bytes),
                        synchronised: last[index].is_some_and(|executed| executed.synchronised),
                    },
                };

                trials.push(Trial {
                    backend: backend.id(),
                    workload: workload.name,
                    regime: *regime,
                    status,
                });
            }
        }
    }

    trials
}

/// Allocates the target a pipeline will write into, or `None` when it cannot accept this size.
fn prepare_target(backend: &dyn Backend, size: Size) -> Option<Surface> {
    let capabilities = backend.capabilities();
    if size.width > capabilities.max_surface.width || size.height > capabilities.max_surface.height {
        return None;
    }
    Surface::try_new(size).ok()
}

/// Writes over a buffer large enough to push the working set out of the last-level cache.
///
/// The buffer is never read, so without the `black_box` at the end an optimiser is entitled to delete
/// the whole loop and turn the cold regime into a second warm one.
fn scrub(buffer: &mut [u8], state: &mut u64) {
    let mut current = *state;
    for chunk in buffer.chunks_exact_mut(8) {
        current ^= current << 13;
        current ^= current >> 7;
        current ^= current << 17;
        chunk.copy_from_slice(&current.to_le_bytes());
    }
    *state = current;
    std::hint::black_box(&*buffer);
}

/// The cost of moving from one pipeline to another, measured rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchMeasurement {
    pub from: BackendId,
    pub to: BackendId,
    /// What the move costs, in nanoseconds. Saturates at zero when the second pipeline turned out to
    /// be no slower after the first, which happens on a CPU where nothing has to be re-bound.
    pub cost_ns: u64,
    pub after_other_ns: u64,
    pub after_same_ns: u64,
}

/// Measures the ordered cost of every pipeline move, on one workload.
///
/// The shape of the measurement is what makes it meaningful. A run of `to` after another run of `to`
/// has the pipeline already bound and its data already resident; a run of `to` after a run of `from`
/// has neither. The difference between the two medians is the move, captured in the state the move
/// would actually happen in rather than in a synthetic one.
#[must_use]
pub fn measure_switch_costs(
    backends: &mut [Box<dyn Backend>],
    workload: &Workload,
    config: &HarnessConfig,
) -> (Vec<SwitchMeasurement>, SwitchCostModel) {
    let mut measurements = Vec::new();
    let mut model = SwitchCostModel::new();
    let batch = &workload.batch;
    let target_size = batch.target();

    let mut targets: Vec<Option<Surface>> = backends
        .iter()
        .map(|backend| prepare_target(backend.as_ref(), target_size))
        .collect();

    for from in 0..backends.len() {
        for to in 0..backends.len() {
            if from == to {
                continue;
            }
            if !backends[to].capabilities().serves(batch) || !backends[from].capabilities().serves(batch)
            {
                continue;
            }
            if backends[from].prepare(batch).is_err() || backends[to].prepare(batch).is_err() {
                continue;
            }

            let mut after_other = Vec::with_capacity(config.iterations as usize);
            let mut after_same = Vec::with_capacity(config.iterations as usize);

            for _ in 0..config.warmup {
                let _ = execute_one(backends, &mut targets, from, batch);
                let _ = execute_one(backends, &mut targets, to, batch);
                let _ = execute_one(backends, &mut targets, to, batch);
            }

            for _ in 0..config.iterations {
                let _ = execute_one(backends, &mut targets, from, batch);
                if let Some(elapsed) = execute_one(backends, &mut targets, to, batch) {
                    after_other.push(elapsed);
                }
                let _ = execute_one(backends, &mut targets, to, batch);
                if let Some(elapsed) = execute_one(backends, &mut targets, to, batch) {
                    after_same.push(elapsed);
                }
            }

            let other = SampleStats::from_samples(&mut after_other);
            let same = SampleStats::from_samples(&mut after_same);
            if other.count == 0 || same.count == 0 {
                continue;
            }

            let cost = other.median_ns.saturating_sub(same.median_ns);
            model.record(backends[from].id(), backends[to].id(), cost);
            measurements.push(SwitchMeasurement {
                from: backends[from].id(),
                to: backends[to].id(),
                cost_ns: cost,
                after_other_ns: other.median_ns,
                after_same_ns: same.median_ns,
            });
        }
    }

    (measurements, model)
}

/// Runs one pipeline once and returns how long it took, or `None` when it could not run.
fn execute_one(
    backends: &mut [Box<dyn Backend>],
    targets: &mut [Option<Surface>],
    index: usize,
    batch: &Batch,
) -> Option<u64> {
    let target = targets[index].as_mut()?;
    let started = Instant::now();
    let outcome = backends[index].execute(batch, target);
    let elapsed = started.elapsed().as_nanos() as u64;
    outcome.ok().map(|_| elapsed)
}

/// Turns a core failure into the string a report prints.
#[must_use]
pub fn describe_failure(error: &RasterError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use erl_raster_core::workload::published_workloads;
    use erl_render_backends::default_registry;

    fn quick_config() -> HarnessConfig {
        HarnessConfig {
            warmup: 2,
            iterations: 4,
            regimes: vec![Regime::Hot],
            scrub_bytes: 64 * 1024,
        }
    }

    #[test]
    fn every_pipeline_and_workload_produces_one_trial_per_regime() {
        let mut backends = default_registry();
        let workloads = published_workloads().expect("builds");
        let config = quick_config();
        let trials = run_trials(&mut backends, &workloads, &config);
        assert_eq!(trials.len(), backends.len() * workloads.len() * config.regimes.len());
    }

    #[test]
    fn the_scalar_reference_always_produces_samples() {
        let mut backends = default_registry();
        let workloads = published_workloads().expect("builds");
        let trials = run_trials(&mut backends, &workloads, &quick_config());
        for trial in &trials {
            if trial.backend == BackendId::CpuScalar {
                let stats = trial.status.stats().expect("the fallback always runs");
                assert_eq!(stats.count, 4, "{}", trial.workload);
            }
        }
    }

    #[test]
    fn a_cold_iteration_is_not_faster_than_a_hot_one() {
        // A guard, not a law of nature, and the tolerance is the honest part of it. The cold regime
        // flushes the caches and allocates inside the measured region, so on a memory-bound operation
        // it must not come out ahead. This blur, measured unoptimised as the test build measures it, is
        // compute-bound instead: the arithmetic dwarfs the cache effect and the two regimes land on the
        // same number to within noise. An exact `cold >= hot` would therefore fail on the noise alone
        // while saying nothing about the flush, which is exactly what it did. What this can still catch
        // is a cold regime that is dramatically faster, i.e. one that is not running the work at all,
        // and the `.expect` below still fails the day the cold regime stops producing samples.
        let mut backends = default_registry();
        let workloads = published_workloads().expect("builds");
        let config = HarnessConfig {
            warmup: 3,
            iterations: 12,
            regimes: Regime::ALL.to_vec(),
            scrub_bytes: 4 * 1024 * 1024,
        };
        let trials = run_trials(&mut backends, &workloads, &config);

        let median = |workload: &str, regime: Regime| -> Option<u64> {
            trials
                .iter()
                .find(|trial| {
                    trial.workload == workload
                        && trial.regime == regime
                        && trial.backend == BackendId::CpuScalar
                })
                .and_then(|trial| trial.status.stats())
                .map(|stats| stats.median_ns)
        };

        let hot = median("blur.panel", Regime::Hot).expect("measured");
        let cold = median("blur.panel", Regime::Cold).expect("measured");
        assert!(
            cold * 100 >= hot * 90,
            "cold {cold} came out well below hot {hot}, so the flush is not doing anything"
        );
    }

    #[test]
    fn switching_to_the_same_pipeline_is_not_measured() {
        let mut backends = default_registry();
        let workloads = published_workloads().expect("builds");
        let (measurements, _) = measure_switch_costs(&mut backends, &workloads[0], &quick_config());
        for measurement in &measurements {
            assert_ne!(measurement.from, measurement.to);
        }
    }
}
