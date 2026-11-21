//! Dynamic dispatch: profile, live conditions and the batch at hand, turned into a pipeline.
//!
//! This crate is where the project stops measuring and starts deciding. It takes the pipelines, the
//! device profile the benchmark produced, and the campaign the host described, and it answers one
//! question per batch: which pipeline runs this. Everything it needs to answer that question was
//! either measured, declared, or refused as unknown.
//!
//! The shape of the answer is what makes the project usable. A caller hands over a batch and gets back
//! the bytes of its rendering, without ever naming a pipeline, and the better of the two is used on
//! each device without the application being written differently for either. That is the whole
//! proposition: write the rendering once, and let the arbitration pick.
//!
//! What the runtime will not do is pretend. It will not switch on a gain it cannot see, it will not
//! treat an unmeasured move as free, it will not use a profile taken on a phone that was hot, and it
//! will not change pipeline more than once per frame to chase a difference. Each of those refusals is
//! a place where the naive dispatcher is worse than no dispatcher at all.
//!
//! Nothing here contains rendering code. The pipelines arrive through
//! [`erl_render_backends::Backend`], and the platform adapters that link them stay on the far side of
//! that trait.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod decision;
pub mod policy;
pub mod runtime;

pub use decision::{Decision, DecisionReason};
pub use policy::{DispatchPolicy, FrameContext};
pub use runtime::{AdaptiveRuntime, RuntimeError};
