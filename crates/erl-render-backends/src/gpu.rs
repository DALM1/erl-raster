//! The GPU backend, and the seam a platform plugs into.
//!
//! This module is the contract, not the implementation. Writing it first is not a formality: the shape
//! of this seam decides what the two platform adapters will have to provide, and getting it wrong
//! after three thousand lines of Metal exist is expensive.
//!
//! The decisive line is the target. A GPU backend receives a **host-memory** [`Surface`], uploads it,
//! draws, and downloads the result. Everything that costs is therefore inside the region the harness
//! measures, including the transfer that decides most real cases. The alternative, a backend that
//! renders into a texture it already owns, produces numbers that look spectacular and describe a
//! situation the caller is not in.
//!
//! Zero copy is the second phase, and the seam is already shaped for it: the bridge owns the device
//! objects, so a later bridge can be handed an `IOSurface` or an `AHardwareBuffer` instead of a host
//! buffer without the contract above it moving. What it will not do is change the measurement rule. A
//! zero-copy path is measured as a variant of the pipeline, not smuggled into the baseline.
//!
//! Until a bridge is linked, [`GpuBackend::absent`] exists so that a report can say *why* the GPU was
//! not a candidate instead of the device silently not appearing at all.

use erl_raster_core::geom::Size;
use erl_raster_core::image::Surface;
use erl_raster_core::op::{Batch, OpKindSet};
use erl_raster_core::{RasterError, Result};

use crate::contract::{Backend, BackendId, Capabilities, CostModel, DeviceClass, Executed};

/// What the platform reports about the device it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuDeviceInfo {
    /// Which backend identifier this device answers to.
    pub id: BackendId,
    /// The device name, as the platform reports it.
    pub name: String,
    pub max_surface: Size,
    /// Whether the device can run a compute kernel. Relevant later for blurs that are cheaper as a
    /// compute dispatch than as two render passes; irrelevant to the MVP, which has no compute path.
    pub supports_compute: bool,
    /// Whether the device reproduces the reference byte for byte.
    ///
    /// This is expected to be `false` in practice, and the library is built for that: a driver is free
    /// to round a blend differently, and the conformance test compares with a per-operation tolerance
    /// when this is false. What is *not* negotiable is the tolerance being written down before the
    /// backend is allowed to serve a frame.
    pub bit_exact: bool,
    /// Whether timestamp queries are available, which is what the embedded benchmark uses to separate
    /// the kernel's time from the round trip around it.
    pub supports_timestamps: bool,
}

/// The seam between the contract and a platform.
///
/// One implementation per platform, written in the platform's language and exposed to Rust through a
/// thin FFI layer. The trait is deliberately small: three calls, no lifetime tricks, nothing that
/// forces the adapter to hold a borrow across a frame. A larger seam would be a larger surface for the
/// two adapters to disagree on.
pub trait GpuSurfaceBridge: Send {
    /// The device this bridge speaks for.
    fn device(&self) -> &GpuDeviceInfo;

    /// Brings device buffers and pipelines into a state where the first timed execution is
    /// representative. Called before the warmup, never inside a measured region.
    fn prepare(&mut self, batch: &Batch) -> Result<()>;

    /// Uploads, draws and downloads.
    fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed>;

    /// Releases device objects.
    fn release(&mut self) {}
}

/// The GPU backend, linked or absent.
pub struct GpuBackend {
    bridge: Option<Box<dyn GpuSurfaceBridge>>,
    capabilities: Capabilities,
    cost_model: CostModel,
    /// Why it is absent, kept so the report can print a reason rather than a blank.
    absence: Option<&'static str>,
}

impl core::fmt::Debug for GpuBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpuBackend")
            // The bridge is a platform object with no textual form, and asking every adapter to
            // implement `Debug` to satisfy a log line would be the wrong trade. Whether one is linked
            // is the only thing a reader of a report needs.
            .field("linked", &self.bridge.is_some())
            .field("capabilities", &self.capabilities)
            .field("cost_model", &self.cost_model)
            .field("absence", &self.absence)
            .finish()
    }
}

impl GpuBackend {
    /// Wraps a linked bridge.
    #[must_use]
    pub fn linked(bridge: Box<dyn GpuSurfaceBridge>) -> Self {
        let device = bridge.device().clone();
        let capabilities = Capabilities {
            id: device.id,
            device_class: DeviceClass::Gpu,
            device_name: device.name.clone(),
            operations: OpKindSet::ALL,
            bit_exact: device.bit_exact,
            // Binding a pipeline, updating descriptors and waiting on the previous frame's fence is a
            // real cost, and it is paid again on every submission. This is the flag that tells the
            // dispatcher a GPU pipeline does not want to be switched into and out of every frame.
            reentrant_costly: true,
            max_surface: device.max_surface,
            notes: "host-memory target: upload and download are inside the measurement",
        };

        Self {
            bridge: Some(bridge),
            capabilities,
            cost_model: CostModel {
                // A GPU submission has a fixed cost the CPU does not have, and it is large enough to
                // lose a four-glyph run outright. The default is pessimistic on purpose: the harness
                // replaces it, and a first session that distrusts the GPU is a better first session
                // than one that jitters between pipelines.
                fixed_ns: 120_000,
                per_operation_ns: 400,
                per_pixel_ns_thousandths: 120,
                // Uploading and downloading the target is bandwidth-bound on a shared bus, and this is
                // the term that explains why a GPU loses small batches.
                per_byte_ns_thousandths: 90,
            },
            absence: None,
        }
    }

    /// A placeholder for a device that is not linked into this build.
    ///
    /// It serves nothing and refuses everything, with a reason attached. It exists so that a profile
    /// and a report name the missing pipeline instead of omitting it, which is the difference between
    /// a finding and a mystery.
    #[must_use]
    pub fn absent(id: BackendId, reason: &'static str) -> Self {
        Self {
            bridge: None,
            capabilities: Capabilities {
                id,
                device_class: DeviceClass::Gpu,
                device_name: format!("{} (not linked)", id.as_str()),
                operations: OpKindSet::EMPTY,
                bit_exact: false,
                reentrant_costly: true,
                max_surface: Size::ZERO,
                notes: reason,
            },
            cost_model: CostModel::default(),
            absence: Some(reason),
        }
    }

    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.bridge.is_some()
    }

    /// Why this backend is not usable, when it is not.
    #[must_use]
    pub fn absence_reason(&self) -> Option<&'static str> {
        self.absence
    }
}

impl Backend for GpuBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn cost_model(&self) -> CostModel {
        self.cost_model
    }

    fn prepare(&mut self, batch: &Batch) -> Result<()> {
        match self.bridge.as_mut() {
            Some(bridge) => bridge.prepare(batch),
            None => Err(RasterError::BackendUnavailable {
                backend: self.capabilities.id.as_str(),
                reason: self.absence.unwrap_or("no bridge linked"),
            }),
        }
    }

    fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed> {
        if batch.target() != target.size() {
            return Err(RasterError::TargetMismatch {
                batch: (batch.target().width, batch.target().height),
                target: (target.width(), target.height()),
            });
        }
        match self.bridge.as_mut() {
            Some(bridge) => bridge.execute(batch, target),
            None => Err(RasterError::BackendUnavailable {
                backend: self.capabilities.id.as_str(),
                reason: self.absence.unwrap_or("no bridge linked"),
            }),
        }
    }

    fn release(&mut self) {
        if let Some(bridge) = self.bridge.as_mut() {
            bridge.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erl_raster_core::geom::Rect;
    use erl_raster_core::op::{BlurOp, Op};

    #[test]
    fn an_absent_device_refuses_with_a_reason_rather_than_vanishing() {
        let mut backend = GpuBackend::absent(BackendId::GpuMetal, "no Metal bridge in this build");
        assert!(!backend.is_linked());
        assert_eq!(backend.absence_reason(), Some("no Metal bridge in this build"));

        let batch = Batch::new(Size::new(4, 4));
        let mut target = Surface::try_new(Size::new(4, 4)).expect("fits");
        match backend.execute(&batch, &mut target) {
            Err(RasterError::BackendUnavailable { backend, reason }) => {
                assert_eq!(backend, "gpu-metal");
                assert!(reason.contains("Metal"));
            }
            other => panic!("expected an unavailable error, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_device_claims_no_operations() {
        let backend = GpuBackend::absent(BackendId::GpuVulkan, "no Vulkan bridge in this build");
        assert!(backend.capabilities().operations.is_empty());

        // The batch has to carry an operation for the second half to mean anything. `serves` asks
        // whether every kind in the batch is covered, and over an empty batch that is vacuously true:
        // an assertion on `Batch::new` alone would pass for a device that claimed the whole world.
        let mut batch = Batch::new(Size::new(2, 2));
        batch.push(Op::Blur(BlurOp::new(Rect::from_size(Size::new(2, 2)), 1, 1)));
        assert!(!backend.capabilities().serves(&batch));
    }
}
