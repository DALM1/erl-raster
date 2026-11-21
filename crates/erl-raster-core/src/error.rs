//! The failure surface of the core.
//!
//! Every variant is something a caller can act on. Nothing here is a programming error being papered
//! over: an out-of-range surface, an operation a backend does not implement, a backend that is not
//! present on this device. Those are all decisions the dispatcher has to be able to take.

use core::fmt;

/// Result alias used throughout the workspace.
pub type Result<T> = core::result::Result<T, RasterError>;

/// Everything that can go wrong below the benchmark engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RasterError {
    /// A surface was requested outside the range the core is willing to allocate.
    SizeOutOfRange { width: u32, height: u32 },
    /// The allocator refused, or the requested surface would exceed the per-surface ceiling.
    AllocationFailed { bytes: u64 },
    /// A batch was submitted against a surface of a different size than it was built for.
    TargetMismatch {
        batch: (u32, u32),
        target: (u32, u32),
    },
    /// A glyph span refers to a mask the run does not hold.
    UnknownGlyphMask { span: usize, masks: usize },
    /// The backend exists but cannot serve this operation.
    UnsupportedOperation {
        backend: &'static str,
        operation: &'static str,
    },
    /// The backend is absent on this device, or was never initialised.
    BackendUnavailable {
        backend: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for RasterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOutOfRange { width, height } => {
                write!(f, "surface {width}x{height} is outside the supported range")
            }
            Self::AllocationFailed { bytes } => {
                write!(f, "could not allocate {bytes} bytes for a surface")
            }
            Self::TargetMismatch { batch, target } => write!(
                f,
                "batch was built for {}x{} but the target is {}x{}",
                batch.0, batch.1, target.0, target.1
            ),
            Self::UnknownGlyphMask { span, masks } => write!(
                f,
                "glyph span {span} refers to mask out of range (the run holds {masks})"
            ),
            Self::UnsupportedOperation { backend, operation } => {
                write!(f, "{backend} does not implement {operation}")
            }
            Self::BackendUnavailable { backend, reason } => {
                write!(f, "{backend} is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for RasterError {}
