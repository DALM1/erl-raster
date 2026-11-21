//! Platform-independent rasterisation for ERL Raster.
//!
//! This crate holds two things that are deliberately kept together: the primitives every backend
//! draws with, and the deterministic reference path those backends are measured against.
//!
//! The reference is integer-only from end to end. That is not a stylistic choice: a floating-point
//! kernel can differ between an x86 build machine, an Apple silicon phone and an ARM Android device,
//! which would make a conformance test either impossible to write or permanently red. Integers give
//! bit-for-bit equality across all three, so a backend that diverges is a backend that is wrong, not
//! a backend that is "slightly different".
//!
//! No `unsafe` lives here. The vectorised kernels that need it live in `erl-render-backends`, behind
//! the architecture check that makes them safe to call.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod error;
pub mod fixed;
pub mod geom;
pub mod glyph;
pub mod image;
pub mod op;
pub mod reference;
pub mod system;
pub mod workload;

pub use error::{RasterError, Result};
pub use geom::{Point, Rect, Size};
pub use glyph::{GlyphMask, GlyphRun, GlyphSpan};
pub use image::{Color, Surface, CHANNELS};
pub use op::{Batch, BlurOp, Op, OpKind, ResampleFilter, ResampleOp, Work};
pub use system::{SystemState, ThermalState};
pub use workload::{published_workloads, Workload};
