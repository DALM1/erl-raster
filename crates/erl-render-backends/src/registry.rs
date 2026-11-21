//! Which pipelines this build can offer.
//!
//! The registry is where the compatibility ecueil lands. There is no discovery, no probing of a
//! graphics API, no attempt to find a device behind the platform's back: a pipeline appears here
//! because something explicitly linked it, and the CPU reference appears unconditionally because the
//! library must never be able to make an application worse than it was without it.

use crate::contract::Backend;
use crate::cpu::CpuBackend;
use crate::gpu::GpuBackend;

/// Every pipeline available on this build and this machine.
///
/// In practice this is the scalar reference plus the vectorised one. A platform adapter links a GPU
/// bridge and pushes the resulting [`GpuBackend`] onto the vector, which is why this returns a
/// collection whose order is the order the report will list it in.
#[must_use]
pub fn default_registry() -> Vec<Box<dyn Backend>> {
    let mut backends: Vec<Box<dyn Backend>> = Vec::with_capacity(3);
    backends.push(Box::new(CpuBackend::scalar()));
    if let Some(vectorised) = CpuBackend::neon() {
        backends.push(Box::new(vectorised));
    }
    backends
}

/// The registry with a linked GPU bridge added, in the position the report should print it.
#[must_use]
pub fn registry_with_gpu(bridge: Box<dyn crate::gpu::GpuSurfaceBridge>) -> Vec<Box<dyn Backend>> {
    let mut backends = default_registry();
    backends.push(Box::new(GpuBackend::linked(bridge)));
    backends
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::BackendId;

    #[test]
    fn the_fallback_is_always_first() {
        let backends = default_registry();
        assert!(!backends.is_empty());
        assert_eq!(backends[0].id(), BackendId::CpuScalar);
        assert!(backends[0].capabilities().operations.count() == 3);
    }

    #[test]
    fn the_registry_never_offers_the_same_pipeline_twice() {
        let backends = default_registry();
        for (index, backend) in backends.iter().enumerate() {
            for other in &backends[index + 1..] {
                assert_ne!(backend.id(), other.id());
            }
        }
    }
}
