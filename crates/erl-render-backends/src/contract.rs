//! What a backend must answer, and what it must pay.
//!
//! Two things are separated here that are usually conflated. The `Capabilities` describe what a
//! backend *is*: which operations it serves, whether it reproduces the reference bit for bit, how big
//! a surface it will accept. The `CostModel` describes what it *costs*, in three parts that the
//! dispatcher needs independently, because they scale with different things. A model with one number
//! would make the dispatcher unable to tell a small batch on a fast pipeline from a large batch on a
//! slow one, which is the entire decision.

use erl_raster_core::geom::Size;
use erl_raster_core::op::{Batch, OpKindSet, Work};
use erl_raster_core::{Result, Surface};

/// Which implementation is answering.
///
/// The list is closed rather than open, because the dispatcher's stored profiles are keyed on these
/// and a backend that could appear from outside would invalidate a persisted measurement. Adding a
/// variant is a deliberate act that invalidates profiles, and the type system enforces that it be
/// treated as one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackendId {
    /// The scalar reference kernels. Never removed from the candidate set.
    CpuScalar,
    /// The same kernels with the compositing inner loop vectorised.
    CpuNeon,
    /// Metal, through the platform bridge.
    GpuMetal,
    /// Vulkan, through the platform bridge.
    GpuVulkan,
}

impl BackendId {
    /// Every identifier, in the order a report should list them. CPU first, because the fallback is
    /// the one thing that is always there.
    pub const ALL: [Self; 4] = [Self::CpuScalar, Self::CpuNeon, Self::GpuMetal, Self::GpuVulkan];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuScalar => "cpu-scalar",
            Self::CpuNeon => "cpu-neon",
            Self::GpuMetal => "gpu-metal",
            Self::GpuVulkan => "gpu-vulkan",
        }
    }

    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"cpu-scalar" => Some(Self::CpuScalar),
            b"cpu-neon" => Some(Self::CpuNeon),
            b"gpu-metal" => Some(Self::GpuMetal),
            b"gpu-vulkan" => Some(Self::GpuVulkan),
            _ => None,
        }
    }

    #[must_use]
    pub const fn device_class(self) -> DeviceClass {
        match self {
            Self::CpuScalar | Self::CpuNeon => DeviceClass::Cpu,
            Self::GpuMetal | Self::GpuVulkan => DeviceClass::Gpu,
        }
    }

    /// Whether this backend is the permanently available fallback.
    #[must_use]
    pub const fn is_fallback(self) -> bool {
        matches!(self, Self::CpuScalar)
    }
}

impl core::fmt::Display for BackendId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a backend runs, which is not the same as what it is good at.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    Cpu,
    Gpu,
}

impl DeviceClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }
}

/// What a backend is and is not capable of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub id: BackendId,
    pub device_class: DeviceClass,
    /// A human-readable name for the report, for instance the Metal device name.
    pub device_name: String,
    /// The operations this backend serves.
    pub operations: OpKindSet,
    /// Whether the backend reproduces the reference byte for byte.
    ///
    /// False does not mean "wrong". It means the conformance test compares it against the reference
    /// with an explicit per-operation tolerance instead of demanding equality, and that the tolerance
    /// is part of what has to be justified before the backend is allowed to serve a frame.
    pub bit_exact: bool,
    /// Whether executing an operation forces the pipeline to be re-bound, i.e. whether two consecutive
    /// batches on this backend cost more than one long one.
    pub reentrant_costly: bool,
    /// The largest surface this backend will accept.
    pub max_surface: Size,
    /// Free-form note shown in the report.
    pub notes: &'static str,
}

impl Capabilities {
    /// Whether this backend can serve every kind in a batch.
    ///
    /// A backend that claims only part of the batch is not a candidate at all. Splitting a batch across
    /// two pipelines to use each where it shines is the optimisation the measurement should earn, not
    /// one to be assumed: it costs a switch, a second target-sized buffer and a composition step, and
    /// the MVP does not do it.
    #[must_use]
    pub fn serves(&self, batch: &Batch) -> bool {
        batch
            .kinds()
            .iter()
            .all(|kind| self.operations.contains(kind))
    }
}

/// A four-part model of what a backend costs.
///
/// The four parts are the ones the second ecueil forces apart: a fixed cost paid whatever the size, a
/// cost per operation, a cost proportional to the pixels touched, and a cost proportional to the bytes
/// moved. They are integers in nanoseconds, with the two proportional terms stored scaled by a
/// thousand, because the interesting values are fractions of a nanosecond per pixel and a model in
/// whole nanoseconds per pixel could not express a fast pipeline at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct CostModel {
    /// Paid once per executed batch: binding, staging, fencing.
    pub fixed_ns: u64,
    /// Paid per operation in the batch.
    pub per_operation_ns: u64,
    /// Paid per pixel of work, in thousandths of a nanosecond.
    pub per_pixel_ns_thousandths: u64,
    /// Paid per byte transferred, in thousandths of a nanosecond.
    pub per_byte_ns_thousandths: u64,
}

impl CostModel {
    /// The cost this model predicts for a batch.
    #[must_use]
    pub fn predict(&self, work: Work, transfer_bytes: u64) -> u64 {
        self.fixed_ns
            + u64::from(work.operations) * self.per_operation_ns
            + work.pixels() * self.per_pixel_ns_thousandths / 1000
            + transfer_bytes * self.per_byte_ns_thousandths / 1000
    }

    /// The cost of a batch that is not known in advance, derived from what it asks for.
    #[must_use]
    pub fn predict_batch(&self, batch: &Batch) -> u64 {
        self.predict(batch.work(), batch.transfer_bytes())
    }

    /// Rebuilds a model from a measured duration and the work it covered.
    ///
    /// The split between fixed and proportional is not recoverable from one measurement, so the fixed
    /// term is assumed zero here and the proportional terms are filled from the average. This is what
    /// the harness uses when it has measured a pipeline but has not yet run the dedicated fixed-cost
    /// probe; the probe refines it.
    #[must_use]
    pub fn from_measurement(work: Work, transfer_bytes: u64, measured_ns: u64) -> Self {
        let pixels = work.pixels();
        if pixels == 0 && transfer_bytes == 0 {
            return Self {
                fixed_ns: measured_ns,
                ..Self::default()
            };
        }
        let per_pixel = if pixels == 0 {
            0
        } else {
            measured_ns.saturating_mul(1000) / pixels
        };
        let per_byte = if transfer_bytes == 0 {
            0
        } else {
            measured_ns.saturating_mul(1000) / transfer_bytes
        };
        Self {
            fixed_ns: 0,
            per_operation_ns: 0,
            per_pixel_ns_thousandths: per_pixel,
            per_byte_ns_thousandths: per_byte,
        }
    }
}

/// What a backend actually spent running a batch.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Executed {
    pub work: Work,
    /// Bytes the backend moved. For a CPU backend this is the sources it read; for a GPU backend it
    /// includes the target uploaded and downloaded, which is the number that explains why a GPU can
    /// lose a small batch.
    pub transferred_bytes: u64,
    /// Whether the backend had to block before returning. A backend that reports true has a cost the
    /// caller inherits on the frame it runs.
    pub synchronised: bool,
}

/// The cost of moving from one pipeline to another, as measured rather than assumed.
///
/// Stored per ordered pair, because the cost is not symmetric: going from a GPU that still has work in
/// flight to a CPU that has to wait for a fence is not the same as the reverse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwitchCostModel {
    entries: Vec<(BackendId, BackendId, u64)>,
}

impl SwitchCostModel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, from: BackendId, to: BackendId, nanoseconds: u64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(left, right, _)| *left == from && *right == to)
        {
            entry.2 = nanoseconds;
            return;
        }
        self.entries.push((from, to, nanoseconds));
    }

    /// The measured cost of going from `from` to `to`, or `None` when it has never been measured.
    ///
    /// `None` is deliberately not turned into a default. A dispatcher that assumed a switch was free
    /// because nobody had measured it would switch constantly on the first session of a new device,
    /// which is exactly the failure the hysteresis exists to prevent.
    #[must_use]
    pub fn between(&self, from: BackendId, to: BackendId) -> Option<u64> {
        self.entries
            .iter()
            .find(|(left, right, _)| *left == from && *right == to)
            .map(|(_, _, cost)| *cost)
    }

    #[must_use]
    pub fn entries(&self) -> &[(BackendId, BackendId, u64)] {
        &self.entries
    }

    /// The worst measured switch cost leaving a backend, used as a conservative estimate when the
    /// exact ordered pair has not been measured.
    #[must_use]
    pub fn leaving(&self, from: BackendId) -> Option<u64> {
        self.entries
            .iter()
            .filter(|(left, _, _)| *left == from)
            .map(|(_, _, cost)| *cost)
            .max()
    }
}

/// Everything a pipeline has to answer.
pub trait Backend: Send {
    fn capabilities(&self) -> &Capabilities;

    #[must_use]
    fn id(&self) -> BackendId {
        self.capabilities().id
    }

    /// The backend's own estimate, used before any measurement exists.
    fn cost_model(&self) -> CostModel;

    /// Brings the pipeline into a state where the first timed execution is representative.
    ///
    /// Whatever a backend normally does once and pays for once belongs here: allocating device buffers,
    /// compiling shaders, warming a scratch region. Leaving it in the first timed iteration is the
    /// single easiest way to make a benchmark measure the wrong thing, and it is why the harness calls
    /// this before its warmup rather than trusting the warmup to absorb it.
    fn prepare(&mut self, _batch: &Batch) -> Result<()> {
        Ok(())
    }

    /// Runs a whole batch against a host-memory target.
    fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed>;

    /// Releases whatever the pipeline is holding. Called when the runtime drops a backend.
    fn release(&mut self) {}
}
