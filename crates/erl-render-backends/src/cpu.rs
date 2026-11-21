//! The CPU backend, in both of its flavours.
//!
//! One type serves both, distinguished by a flag, and the reason is not laziness. The scalar backend
//! *is* the reference: it calls the same functions the conformance test compares against. Keeping a
//! separate "reference backend" would mean three implementations to keep in step instead of two, and
//! the third would be the one nobody exercised.
//!
//! The vectorised flavour replaces exactly one thing, the glyph compositing row. That is where the
//! measurements put the cost: a screen of text is a large number of small masks, the blending loop is
//! four multiply-accumulates per pixel over an almost entirely transparent plane, and it is the only
//! kernel in the published load with a tight, branch-free, data-parallel body. Resampling is a gather
//! and gains little from wide registers; the box blur is a running sum whose normalisation is an
//! integer division that has no lane-parallel form on this architecture. Both therefore stay on the
//! reference path, and [`Capabilities`] says so rather than pretending otherwise.

use erl_raster_core::geom::Size;
use erl_raster_core::glyph::GlyphRun;
use erl_raster_core::image::{Surface, MAX_DIMENSION};
use erl_raster_core::op::{Batch, Op, OpKindSet, Work, MAX_BLUR_RADIUS};
use erl_raster_core::{reference, RasterError, Result};

use crate::contract::{Backend, BackendId, Capabilities, CostModel, DeviceClass, Executed};
use crate::neon;

/// The CPU pipeline, scalar or vectorised.
#[derive(Debug)]
pub struct CpuBackend {
    capabilities: Capabilities,
    cost_model: CostModel,
    vectorised: bool,
}

impl CpuBackend {
    /// The scalar reference, and the fallback the dispatcher is never allowed to drop.
    #[must_use]
    pub fn scalar() -> Self {
        Self {
            capabilities: Capabilities {
                id: BackendId::CpuScalar,
                device_class: DeviceClass::Cpu,
                device_name: "scalar reference".to_string(),
                operations: OpKindSet::ALL,
                bit_exact: true,
                // Executing a batch leaves nothing to re-bind, so two consecutive batches cost exactly
                // what their two bodies cost. This is the property that makes CPU the safe fallback.
                reentrant_costly: false,
                max_surface: Size::new(MAX_DIMENSION, MAX_DIMENSION),
                notes: "portable, always available, defines the expected output",
            },
            // A scalar core blends roughly one pixel per nanosecond on a phone, before the branch that
            // skips transparent coverage. The model is a starting point only; the harness replaces it
            // with what it measures.
            cost_model: CostModel {
                fixed_ns: 0,
                per_operation_ns: 40,
                per_pixel_ns_thousandths: 1_200,
                per_byte_ns_thousandths: 20,
            },
            vectorised: false,
        }
    }

    /// The vectorised reference, when the architecture has one.
    ///
    /// Returns `None` on anything but AArch64 rather than silently handing back the scalar backend
    /// under a NEON name. A backend that misreports which pipeline it is would poison the profile it
    /// writes, and the profile is what every later session dispatches on.
    #[must_use]
    pub fn neon() -> Option<Self> {
        if !neon::is_available() {
            return None;
        }
        Some(Self {
            capabilities: Capabilities {
                id: BackendId::CpuNeon,
                device_class: DeviceClass::Cpu,
                device_name: neon::report_name().to_string(),
                operations: OpKindSet::ALL,
                bit_exact: true,
                reentrant_costly: false,
                max_surface: Size::new(MAX_DIMENSION, MAX_DIMENSION),
                notes: "vectorised compositing row, scalar elsewhere",
            },
            cost_model: CostModel {
                fixed_ns: 0,
                per_operation_ns: 40,
                // The vector row removes most of the per-pixel cost of a glyph run. The model cannot
                // express "conditional on operation kind", so it errs on the fast side and the measured
                // replacement corrects it.
                per_pixel_ns_thousandths: 700,
                per_byte_ns_thousandths: 20,
            },
            vectorised: true,
        })
    }

    /// The best CPU pipeline this machine can offer.
    #[must_use]
    pub fn best_available() -> Self {
        Self::neon().unwrap_or_else(Self::scalar)
    }

    #[must_use]
    pub const fn is_vectorised(&self) -> bool {
        self.vectorised
    }

    /// Draws a glyph run with whichever row blender this flavour owns.
    fn draw_glyphs(&self, target: &mut Surface, run: &GlyphRun) -> Result<u64> {
        if self.vectorised {
            reference::fill_glyphs_with(target, run, neon::blend_span_row)
        } else {
            reference::fill_glyphs(target, run)
        }
    }
}

impl Backend for CpuBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn cost_model(&self) -> CostModel {
        self.cost_model
    }

    fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed> {
        if batch.target() != target.size() {
            return Err(RasterError::TargetMismatch {
                batch: (batch.target().width, batch.target().height),
                target: (target.width(), target.height()),
            });
        }

        let mut work = Work::default();
        for op in batch.ops() {
            work += match op {
                Op::FillGlyphs(run) => Work {
                    operations: 1,
                    glyph_pixels: self.draw_glyphs(target, run)?,
                    ..Work::default()
                },
                Op::Resample(resample) => Work {
                    operations: 1,
                    resample_pixels: reference::resample(target, resample)?,
                    source_bytes: resample.source_bytes(),
                    ..Work::default()
                },
                Op::Blur(blur) => {
                    let area = reference::blur(target, blur)?;
                    Work {
                        operations: 1,
                        // Reported in the same units the dispatcher's estimate uses, so a measured
                        // cost and a predicted cost can be compared without a conversion that could
                        // be forgotten in one place and applied in another.
                        blur_pixels: area
                            * u64::from(blur.passes)
                            * u64::from(blur.taps())
                            * 2,
                        ..Work::default()
                    }
                }
            };
        }

        Ok(Executed {
            work,
            // A CPU backend reads its sources straight out of the target's address space. The only
            // bytes it moves are the ones it reads, and the target is already where the compositor
            // wants it.
            transferred_bytes: batch.retained_bytes(),
            synchronised: false,
        })
    }
}

/// The tap ceiling the reference blur relies on, re-exported so the documentation and the dispatcher
/// agree on one number.
pub const SUPPORTED_BLUR_RADIUS: u32 = MAX_BLUR_RADIUS;

#[cfg(test)]
mod tests {
    use super::*;
    use erl_raster_core::workload::published_workloads;

    #[test]
    fn the_vector_flavour_exists_where_it_is_claimed_to() {
        if neon::is_available() {
            assert!(CpuBackend::neon().is_some());
        } else {
            assert!(CpuBackend::neon().is_none());
        }
    }

    #[test]
    fn both_flavours_produce_the_same_bytes() {
        // The claim the whole design rests on, checked over the published load rather than over a
        // hand-picked case. If this ever fails, the vectorised backend has to be withdrawn from the
        // candidate set, not tolerated.
        let Some(mut vectorised) = CpuBackend::neon() else {
            return;
        };
        let mut scalar = CpuBackend::scalar();

        for workload in published_workloads().expect("builds") {
            let mut reference_target = Surface::try_new(workload.batch.target()).expect("fits");
            let mut vector_target = Surface::try_new(workload.batch.target()).expect("fits");

            scalar
                .execute(&workload.batch, &mut reference_target)
                .expect("runs");
            vectorised
                .execute(&workload.batch, &mut vector_target)
                .expect("runs");

            assert_eq!(
                reference_target.checksum(),
                vector_target.checksum(),
                "{} diverged",
                workload.name
            );
        }
    }

    #[test]
    fn both_flavours_report_the_same_work() {
        let Some(mut vectorised) = CpuBackend::neon() else {
            return;
        };
        let mut scalar = CpuBackend::scalar();
        let workloads = published_workloads().expect("builds");
        let batch = &workloads[1].batch;

        let mut left = Surface::try_new(batch.target()).expect("fits");
        let mut right = Surface::try_new(batch.target()).expect("fits");
        let scalar_work = scalar.execute(batch, &mut left).expect("runs").work;
        let vector_work = vectorised.execute(batch, &mut right).expect("runs").work;
        assert_eq!(scalar_work, vector_work);
    }

    #[test]
    fn a_batch_against_the_wrong_target_is_refused() {
        let mut backend = CpuBackend::scalar();
        let batch = Batch::new(Size::new(4, 4));
        let mut target = Surface::try_new(Size::new(8, 8)).expect("fits");
        assert!(matches!(
            backend.execute(&batch, &mut target),
            Err(RasterError::TargetMismatch { .. })
        ));
    }
}
