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

// The one real GPU bridge, and the only module in the workspace whose graphics API is compiled in. It
// is gated by platform because `wgpu`'s `metal` feature exists only under `target_vendor = "apple"`,
// because its `vulkan` backend exists only where a loader does, and because `Instance::new` panics when
// the running platform has no compiled backend. It is gated by feature as well, so a build that links
// only the vectorised CPU path keeps the graphics stack out of the artefact entirely. One module
// serves both platforms: nothing above the API choice differs, so the kernels, the seam and the
// measurement rule cannot drift apart between them. What does change lives in the bridge's own
// `platform` module, namely the API the instance opens and the identifier the device reports.
#[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
pub mod wgpu_bridge;

// The display path, as opposed to the measured one. It shares the instance the bridge opens but none
// of its kernels, because the two want opposite things from the same device: the bridge wants bytes it
// can compare, the presenter wants pixels it can show without ever reading them back. Gated exactly as
// the bridge is, for the same reason — it compiles a graphics API, and only the two platforms that
// document their window handles are served.
#[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
pub mod present;

pub use contract::{
    Backend, BackendId, Capabilities, CostModel, DeviceClass, Executed, SwitchCostModel,
};
pub use cpu::CpuBackend;
pub use gpu::{GpuBackend, GpuDeviceInfo, GpuSurfaceBridge};
#[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
pub use present::{PresentFilter, Presented, Presenter, WindowHandle};
pub use registry::{default_registry, registry_with_gpu};
#[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
pub use wgpu_bridge::WgpuBridge;
