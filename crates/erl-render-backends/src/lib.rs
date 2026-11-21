//! The implementations, behind one contract.
//!
//! The contract is deliberately built around a host-memory target rather than around a device texture.
//! A GPU backend therefore uploads the target, draws, and downloads the result, and pays for both
//! inside the region the harness measures. That is not a limitation being apologised for: it is the
//! only shape that makes the comparison honest. A GPU backend measured on a texture it already owns
//! would look faster than it is, and the transfer it hides is precisely the cost that decides most
//! real cases.
//!
//! Zero-copy is not excluded, it is deferred. The iOS `IOSurface` and Android `AHardwareBuffer`
//! routes belong to the "host view" integration level and arrive once the arbitrage itself is proven.

#![warn(missing_debug_implementations)]

pub mod contract;
pub mod cpu;
pub mod gpu;
pub mod neon;
pub mod registry;

// The first real GPU bridge, and the only one whose backend is compiled into this build. It is gated
// by platform because `wgpu`'s `metal` feature exists only under `target_vendor = "apple"`, and
// because `Instance::new` panics when the running platform has no compiled backend. The Android
// adapter will arrive as its own module with its own gate.
#[cfg(target_os = "macos")]
pub mod wgpu_bridge;

pub use contract::{
    Backend, BackendId, Capabilities, CostModel, DeviceClass, Executed, SwitchCostModel,
};
pub use cpu::CpuBackend;
pub use gpu::{GpuBackend, GpuDeviceInfo, GpuSurfaceBridge};
pub use registry::{default_registry, registry_with_gpu};
#[cfg(target_os = "macos")]
pub use wgpu_bridge::MetalBridge;
