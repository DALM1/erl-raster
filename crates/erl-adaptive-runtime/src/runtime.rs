//! The dispatcher: one owner of the pipelines, one decision per batch.
//!
//! The runtime is the only component that both knows the measurements and holds the pipelines, and
//! that is the whole point. A caller writes a batch, hands it over, and the runtime decides where it
//! runs. Nobody outside has to know that Metal was the better choice for a blur on this device at
//! nominal temperature, and nobody outside can get it wrong.
//!
//! Five rules hold the thing together, and each one closes a way the naive version fails.
//!
//! **The incumbent is never dropped for being slow; it is only ever displaced by something better.**
//! There is no "no pipeline" state. The CPU reference is always in the set, it can always run, and the
//! worst case of using the library is therefore that it runs at reference speed.
//!
//! **A challenger must win repeatedly, not once.** One frame where the incumbent stalled is a
//! scheduling artefact, not evidence, and a dispatcher that reacts to it thrashes.
//!
//! **A move has to pay for itself, in measured nanoseconds.** The cost of the move is read from the
//! profile, where the benchmark put it. An unmeasured move is not treated as free, it is refused: the
//! failure mode of assuming a switch is cheap is a first session that spends all its time switching.
//!
//! **At most one performance move per frame.** Below that, the fixed cost of the move is paid twice
//! inside the very frame the change was supposed to improve. A move made for correctness, because the
//! incumbent cannot run the batch at all, is not subject to this budget: that is not churn, that is the
//! batch being unable to run otherwise.
//!
//! **The profile is only used in the condition it was taken in.** If the thermal state no longer
//! matches the one the profile was recorded at, the incumbent is put back on the fallback and every
//! number in the profile is treated as describing another device. That is the honest reading: the
//! profile is a measurement of a device that was, and a throttling phone is not that device.

use std::time::Instant;

use erl_benchmark::{DeviceProfile, Regime};
use erl_raster_core::op::Batch;
use erl_raster_core::system::SystemState;
use erl_raster_core::{RasterError, Result, Surface};
use erl_render_backends::{Backend, BackendId, Executed};

use crate::decision::{Decision, DecisionReason};
use crate::policy::{DispatchPolicy, FrameContext};

/// Why a runtime could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    /// The set of pipelines handed over contains no permanent fallback.
    ///
    /// Refused rather than tolerated. A runtime without a fallback can reach a state where no pipeline
    /// is willing to run a batch, which is a worse outcome than any amount of lost performance.
    NoFallback,
}

impl core::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoFallback => f.write_str("the pipeline set has no permanent fallback"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// The pipelines, the profile, and the state of the arbitration between them.
pub struct AdaptiveRuntime {
    backends: Vec<Box<dyn Backend>>,
    profile: DeviceProfile,
    policy: DispatchPolicy,
    fallback: BackendId,
    /// The pipeline that currently owns the frames. Starts at the fallback, and only ever moves
    /// because something earned it.
    tenant: BackendId,
    /// The pipeline currently trying to earn the place, and how many consecutive batches it has won.
    challenger: Option<(BackendId, u32)>,
    /// Pipelines that failed in a way that means they cannot run on this device.
    unavailable: Vec<(BackendId, String)>,
    /// Pipelines already warmed up, so `prepare` is not repeated on every batch.
    prepared: Vec<BackendId>,
    context: FrameContext,
    /// Whether the profile describes the condition the device is in right now.
    trusted: bool,
    /// Performance moves already made in this frame. Never above one.
    moves_this_frame: u32,
    /// Consecutive batches that ran slower than their own measurement predicted.
    slow_streak: u32,
    /// A reason owed to the next decision, set by an event that happened outside a decision.
    next_reason: Option<DecisionReason>,
    last_decision: Option<Decision>,
}

impl AdaptiveRuntime {
    /// Takes ownership of the pipelines and of what was measured for them.
    pub fn new(
        backends: Vec<Box<dyn Backend>>,
        profile: DeviceProfile,
        policy: DispatchPolicy,
    ) -> core::result::Result<Self, RuntimeError> {
        let fallback = backends
            .iter()
            .map(|backend| backend.id())
            .find(|id| id.is_fallback())
            .ok_or(RuntimeError::NoFallback)?;

        Ok(Self {
            backends,
            profile,
            policy,
            fallback,
            tenant: fallback,
            challenger: None,
            unavailable: Vec::new(),
            prepared: Vec::new(),
            context: FrameContext::default(),
            trusted: false,
            moves_this_frame: 0,
            slow_streak: 0,
            next_reason: None,
            last_decision: None,
        })
    }

    /// Declares the start of a frame, and the conditions it runs in.
    ///
    /// This resets the change budget, which is why it must be called once per frame. It also re-reads
    /// the system state: a device that has started throttling or has been closed by the platform is
    /// running a different machine than the one the profile was taken on, and the arbitration has to
    /// notice before it decides anything rather than after.
    pub fn begin_frame(&mut self, state: &SystemState, context: FrameContext) {
        self.moves_this_frame = 0;
        self.context = context;

        let trusted = state.profile_key() == self.profile.state.profile_key();
        if !trusted && self.trusted && !self.tenant.is_fallback() {
            // The measurement stopped describing this device. Returning to the fallback is not a
            // performance decision, so nothing has to be proven first.
            self.tenant = self.fallback;
            self.challenger = None;
            self.slow_streak = 0;
        }
        self.trusted = trusted;
    }

    /// Runs a batch on whichever pipeline the arbitration says is best.
    pub fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed> {
        self.ensure_serviceable(batch)?;
        let decision = self.choose(batch);
        self.run(decision.backend, decision.predicted_ns, batch, target)
    }

    /// The pipeline that currently owns the frames.
    #[must_use]
    pub const fn tenant(&self) -> BackendId {
        self.tenant
    }

    /// The pipeline that is always available, whatever else happens.
    #[must_use]
    pub const fn fallback(&self) -> BackendId {
        self.fallback
    }

    /// The decision taken for the most recent batch.
    #[must_use]
    pub const fn last_decision(&self) -> Option<&Decision> {
        self.last_decision.as_ref()
    }

    /// The pipelines that turned out not to be runnable here, with what they said.
    #[must_use]
    pub fn unavailable(&self) -> &[(BackendId, String)] {
        &self.unavailable
    }

    /// Whether the profile still describes the condition the device is in.
    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// The best candidate for a batch, taking no decision and changing no state.
    #[must_use]
    pub fn preview(&self, batch: &Batch) -> Option<(BackendId, u64)> {
        self.best_candidate(batch)
    }

    /// Chooses the pipeline for one batch, and records why.
    fn choose(&mut self, batch: &Batch) -> Decision {
        let incumbent = self.tenant;
        let predicted = self.cost_of(incumbent, batch, self.context.regime);

        if let Some(reason) = self.next_reason.take() {
            return self.decide(Decision {
                backend: incumbent,
                reason,
                predicted_ns: predicted,
            });
        }

        if !self.serves(incumbent, batch) {
            // Not a performance question. The incumbent cannot be used for this batch at all, so the
            // budget that limits churn does not apply, and neither does the hysteresis.
            if let Some((candidate, cost)) = self.best_candidate(batch) {
                self.tenant = candidate;
                self.challenger = None;
                self.slow_streak = 0;
                return self.decide(Decision {
                    backend: candidate,
                    reason: DecisionReason::IncumbentCannotServe {
                        from: incumbent,
                        to: candidate,
                    },
                    predicted_ns: Some(cost),
                });
            }
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::IncumbentStable,
                predicted_ns: predicted,
            });
        }

        if self.context.critical {
            self.challenger = None;
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::CriticalWindow,
                predicted_ns: predicted,
            });
        }

        if !self.trusted {
            self.challenger = None;
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::StaleProfile,
                predicted_ns: predicted,
            });
        }

        if !self.policy.switching_enabled {
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::IncumbentStable,
                predicted_ns: predicted,
            });
        }

        let Some((candidate, candidate_cost)) = self.best_candidate(batch) else {
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::IncumbentStable,
                predicted_ns: predicted,
            });
        };

        if candidate == incumbent {
            self.challenger = None;
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::IncumbentStable,
                predicted_ns: predicted,
            });
        }

        let Some(incumbent_cost) = predicted else {
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::IncumbentStable,
                predicted_ns: None,
            });
        };

        let Some(switch_cost) = self.profile.switch_costs.between(incumbent, candidate) else {
            self.challenger = None;
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::SwitchCostUnknown {
                    from: incumbent,
                    to: candidate,
                },
                predicted_ns: Some(incumbent_cost),
            });
        };

        let gain = incumbent_cost.saturating_sub(candidate_cost);
        if !self
            .policy
            .gain_clears_the_bar(incumbent_cost, candidate_cost, switch_cost)
        {
            self.challenger = None;
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::GainBelowSwitchCost {
                    gain_ns: gain,
                    cost_ns: switch_cost,
                },
                predicted_ns: Some(incumbent_cost),
            });
        }

        let wins = match self.challenger {
            Some((current, count)) if current == candidate => count + 1,
            _ => 1,
        };
        if wins < self.policy.required_consecutive_wins {
            self.challenger = Some((candidate, wins));
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::ChallengerNotProven {
                    wins,
                    needed: self.policy.required_consecutive_wins,
                },
                predicted_ns: Some(incumbent_cost),
            });
        }

        if self.moves_this_frame >= 1 {
            self.challenger = Some((candidate, wins));
            return self.decide(Decision {
                backend: incumbent,
                reason: DecisionReason::SwitchBudgetSpent,
                predicted_ns: Some(incumbent_cost),
            });
        }

        self.tenant = candidate;
        self.challenger = None;
        self.moves_this_frame += 1;
        self.slow_streak = 0;
        self.decide(Decision {
            backend: candidate,
            reason: DecisionReason::Switched {
                from: incumbent,
                gain_ns: gain,
                cost_ns: switch_cost,
            },
            predicted_ns: Some(candidate_cost),
        })
    }

    /// Runs the batch on one pipeline, having warmed it up first.
    fn run(
        &mut self,
        id: BackendId,
        predicted_ns: Option<u64>,
        batch: &Batch,
        target: &mut Surface,
    ) -> Result<Executed> {
        let Some(index) = self.index_of(id) else {
            return self.retry_with_fallback(id, batch, target);
        };

        if !self.prepared.contains(&id) {
            if let Err(error) = self.backends[index].prepare(batch) {
                self.mark_unavailable(id, error.to_string());
                return self.retry_with_fallback(id, batch, target);
            }
            self.prepared.push(id);
        }

        let started = Instant::now();
        let outcome = self.backends[index].execute(batch, target);
        let elapsed = started.elapsed().as_nanos() as u64;

        match outcome {
            Ok(executed) => {
                self.observe(id, predicted_ns, elapsed);
                Ok(executed)
            }
            Err(RasterError::BackendUnavailable { reason, .. }) => {
                // A pipeline that says it is not there must not be asked again this session. This is
                // the case a device losing its GPU context belongs to, and re-submitting to it every
                // frame would turn one lost context into a permanently broken application.
                self.mark_unavailable(id, reason.to_string());
                self.retry_with_fallback(id, batch, target)
            }
            Err(error) => Err(error),
        }
    }

    /// Hands the batch to the fallback after the chosen pipeline turned out to be unusable.
    fn retry_with_fallback(
        &mut self,
        failed: BackendId,
        batch: &Batch,
        target: &mut Surface,
    ) -> Result<Executed> {
        if failed.is_fallback() {
            return Err(RasterError::BackendUnavailable {
                backend: self.fallback.as_str(),
                reason: "the reference pipeline is the last resort and it failed",
            });
        }

        self.tenant = self.fallback;
        self.challenger = None;
        self.slow_streak = 0;
        self.next_reason = Some(DecisionReason::UnavailableFallback { missing: failed });

        let decision = self.choose(batch);
        self.run(decision.backend, decision.predicted_ns, batch, target)
    }

    /// Compares what a pipeline just took with what it was predicted to take.
    ///
    /// The prediction comes from the profile, so this measures exactly one thing: whether the device
    /// still behaves the way it did when it was measured. A GPU that has been taken over by another
    /// application, or a CPU whose cores have been parked by the scheduler, shows up here as a
    /// pipeline that has stopped being the pipeline it was chosen as.
    fn observe(&mut self, id: BackendId, predicted_ns: Option<u64>, observed_ns: u64) {
        let Some(predicted) = predicted_ns else {
            self.slow_streak = 0;
            return;
        };

        if self.policy.is_regression(predicted, observed_ns) {
            self.slow_streak += 1;
        } else {
            self.slow_streak = 0;
            return;
        }

        if self.slow_streak >= self.policy.regression_window && !id.is_fallback() {
            self.slow_streak = 0;
            if self.tenant == id {
                self.tenant = self.fallback;
            }
            self.challenger = None;
            self.next_reason = Some(DecisionReason::RegressionDemoted {
                demoted: id,
                predicted_ns: predicted,
                observed_ns,
            });
        }
    }

    /// The cheapest pipeline that can run this batch, judged on the profile where it has an entry and
    /// on the pipeline's own model where it does not.
    fn best_candidate(&self, batch: &Batch) -> Option<(BackendId, u64)> {
        let mut best: Option<(BackendId, u64)> = None;
        for backend in &self.backends {
            let id = backend.id();
            if self.is_unavailable(id) || !backend.capabilities().serves(batch) {
                continue;
            }
            let Some(cost) = self.cost_of(id, batch, self.context.regime) else {
                continue;
            };
            match best {
                Some((_, best_cost)) if best_cost <= cost => {}
                _ => best = Some((id, cost)),
            }
        }
        best
    }

    /// What a pipeline is expected to cost for this batch.
    ///
    /// The profile wins when it has something to say, because it is a measurement of this device rather
    /// than a table written by hand. Its entries are per workload, so the number that travels is the
    /// cost per pixel, scaled to the batch at hand. The pipeline's own model is the fallback, and it is
    /// deliberately pessimistic on the GPU side: a model that has never seen this device should not be
    /// able to talk the dispatcher into a move on its own.
    fn cost_of(&self, id: BackendId, batch: &Batch, regime: Regime) -> Option<u64> {
        let backend = self.backend(id)?;
        if let Some(measured) = self.measured_cost(id, batch, regime) {
            return Some(measured);
        }
        Some(backend.cost_model().predict_batch(batch))
    }

    /// The measured cost of a comparable workload, scaled to this batch.
    fn measured_cost(&self, id: BackendId, batch: &Batch, regime: Regime) -> Option<u64> {
        let kinds = batch.kinds();
        let entry = self.profile.entries.iter().find(|entry| {
            entry.backend == id && entry.regime == regime && entry.kinds == kinds
        })?;
        let per_pixel = entry.nanoseconds_per_pixel()?;
        Some((per_pixel * batch.work().pixels() as f64).round() as u64)
    }

    /// Refuses a batch no pipeline in the set can run.
    ///
    /// Reachable only with a set that is missing a kind altogether, which is why it is checked here
    /// rather than assumed away: the set is built by a platform adapter, and an adapter that forgot to
    /// register a pipeline is a plausible mistake whose symptom should be a clear error.
    fn ensure_serviceable(&self, batch: &Batch) -> Result<()> {
        let unserved = batch.kinds().iter().find(|kind| {
            !self
                .backends
                .iter()
                .any(|backend| backend.capabilities().operations.contains(*kind))
        });
        match unserved {
            None => Ok(()),
            Some(kind) => Err(RasterError::UnsupportedOperation {
                backend: self.tenant.as_str(),
                operation: kind.as_str(),
            }),
        }
    }

    fn backend(&self, id: BackendId) -> Option<&dyn Backend> {
        self.backends
            .iter()
            .find(|backend| backend.id() == id)
            .map(|backend| &**backend)
    }

    fn index_of(&self, id: BackendId) -> Option<usize> {
        self.backends
            .iter()
            .position(|backend| backend.id() == id)
    }

    fn serves(&self, id: BackendId, batch: &Batch) -> bool {
        self.backend(id)
            .is_some_and(|backend| backend.capabilities().serves(batch))
    }

    fn is_unavailable(&self, id: BackendId) -> bool {
        self.unavailable.iter().any(|(backend, _)| *backend == id)
    }

    fn mark_unavailable(&mut self, id: BackendId, reason: String) {
        if !self.is_unavailable(id) {
            self.unavailable.push((id, reason));
        }
    }

    fn decide(&mut self, decision: Decision) -> Decision {
        self.last_decision = Some(decision);
        decision
    }
}

impl core::fmt::Debug for AdaptiveRuntime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AdaptiveRuntime")
            .field(
                "pipelines",
                &self.backends.iter().map(|backend| backend.id()).collect::<Vec<_>>(),
            )
            .field("tenant", &self.tenant)
            .field("fallback", &self.fallback)
            .field("trusted", &self.trusted)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Drop for AdaptiveRuntime {
    fn drop(&mut self) {
        for backend in &mut self.backends {
            backend.release();
        }
    }
}
