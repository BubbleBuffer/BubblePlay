//! One lock per D3D11 device for every call that reaches that device's
//! immediate context, its DXGI duplication, or an NVENC session opened on it.
//!
//! Desktop Duplication and NVENC both drive the same device. Their calls may
//! not overlap: NVIDIA's threading guidance warns that cross-thread DXGI
//! acquisition and NVENC work on the application's device can be undefined.
//! The 2026-10-09 hang dumps showed a DXGI acquisition waiting while NVENC was
//! inside a driver call (a reconfigure in the first dump, a resource
//! registration in the second), so the serialization is not limited to one
//! NVENC entry point. Capture and encoder code therefore take the gate of
//! their device around each native call group.
//!
//! Rules for callers:
//! - Hold the gate only around native calls. Never hold it while dropping a
//!   frame lease, locking a backend, joining a worker, or entering a helper
//!   that takes the gate itself.
//! - Public entry points take the gate; private helpers assume it is held.
//!   Re-entry on the same thread panics instead of deadlocking.
//! - Every device that is shared through this gate has multithread protection
//!   enabled, so the immediate context is safe for the threads that use it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use anyhow::{ensure, Context, Result};
use windows::core::{IUnknown, Interface};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread};

/// Serializes native GPU calls made on one D3D11 device.
pub struct GpuGate {
    lock: Mutex<()>,
    /// A typed device reference keeps the canonical `IUnknown` identity alive
    /// while the gate exists, so its address cannot be reused for another
    /// device under the same registry key. `None` for test-only keys.
    _device: Option<ID3D11Device>,
}

thread_local! {
    static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Proof that the current thread holds a device gate.
pub struct GpuGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    key: usize,
}

impl GpuGate {
    fn new(device: Option<ID3D11Device>) -> Self {
        Self {
            lock: Mutex::new(()),
            _device: device,
        }
    }

    /// Blocks until this device's native calls are free. Panics on same-thread
    /// re-entry, which would otherwise deadlock forever.
    pub fn lock(&self) -> GpuGuard<'_> {
        let key = self as *const Self as usize;
        assert!(
            !HELD.with(|held| held.borrow().contains(&key)),
            "GPU gate re-entered on the same thread"
        );
        let guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        HELD.with(|held| held.borrow_mut().push(key));
        GpuGuard { _guard: guard, key }
    }
}

impl Drop for GpuGuard<'_> {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(position) = held.iter().rposition(|key| *key == self.key) {
                held.remove(position);
            }
        });
    }
}

fn registry() -> &'static Mutex<HashMap<usize, Weak<GpuGate>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, Weak<GpuGate>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gate_for(key: usize, device: Option<ID3D11Device>) -> Arc<GpuGate> {
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(gate) = registry.get(&key).and_then(Weak::upgrade) {
        // Same canonical device: the gate already holds the identity.
        return gate;
    }
    registry.retain(|_, gate| gate.strong_count() > 0);
    let gate = Arc::new(GpuGate::new(device));
    registry.insert(key, Arc::downgrade(&gate));
    gate
}

/// Returns the gate for a raw key, creating it when no live holder has one.
/// Test-only keys carry no device identity; production uses [`for_device`].
pub fn for_device_key(key: usize) -> Arc<GpuGate> {
    gate_for(key, None)
}

/// Gate for a device. The key is the device's canonical `IUnknown` pointer,
/// so every interface pointer to one device maps to the same lock. The gate
/// retains that identity, so the key cannot be reused while the gate lives.
pub fn for_device(device: &ID3D11Device) -> Arc<GpuGate> {
    let identity: IUnknown = device
        .cast()
        .expect("QueryInterface(IUnknown) on a live D3D11 device");
    gate_for(identity.as_raw() as usize, Some(device.clone()))
}

/// Enables multithread protection on a device's immediate context, under the
/// device's gate. Idempotent: an already shared device reads back protected.
///
/// `ID3D11Multithread` shares its IID with `ID3D10Multithread`, so the query
/// reaches the same interface. `SetMultithreadProtected` returns the previous
/// state, not success, so the result is read back instead. Callers must not
/// hold the device gate when they call this.
pub fn protect_device(device: &ID3D11Device) -> Result<()> {
    let gate = for_device(device);
    let _gpu = gate.lock();
    let context = unsafe { device.GetImmediateContext() }
        .context("GetImmediateContext for multithread protection")?;
    let multithread: ID3D11Multithread = context
        .cast()
        .context("query ID3D11Multithread from immediate context")?;
    // The return value is the previous state; verification below is the check.
    let _previous = unsafe { multithread.SetMultithreadProtected(true) };
    ensure!(
        unsafe { multithread.GetMultithreadProtected() }.as_bool(),
        "D3D11 multithread protection did not enable"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    /// Tracks how many simulated device calls are inside their native section.
    #[derive(Default)]
    struct Overlap {
        inside: AtomicUsize,
        max_inside: AtomicUsize,
    }

    impl Overlap {
        fn enter(&self) {
            let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inside.fetch_max(now, Ordering::SeqCst);
        }
        fn leave(&self) {
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn same_device_resolves_to_one_gate_and_distinct_devices_do_not() {
        let a = for_device_key(0x1000);
        let b = for_device_key(0x1000);
        let other = for_device_key(0x2000);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &other));
    }

    /// Once the last holder drops, the registry's Weak can never resurrect that
    /// allocation. A later lookup for the same key builds a new, working gate.
    #[test]
    fn expired_gate_is_not_resurrected_and_the_key_gets_a_fresh_working_gate() {
        let first = for_device_key(0x3000);
        let weak = Arc::downgrade(&first);
        drop(first);
        assert!(weak.upgrade().is_none());
        let second = for_device_key(0x3000);
        assert_eq!(Arc::strong_count(&second), 1);
        assert!(weak.upgrade().is_none(), "old allocation must stay expired");
        drop(second.lock());
    }

    fn create_warp_device() -> Result<ID3D11Device> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        };
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .context("create WARP device")?;
        device.context("WARP device missing")
    }

    /// Different interface pointers to one device must resolve to one gate,
    /// including an `IUnknown` round trip, so capture and encoder never split.
    #[test]
    fn every_interface_pointer_to_one_device_resolves_to_the_same_gate() -> Result<()> {
        use windows::core::IUnknown;
        let device = create_warp_device()?;
        let gate = for_device(&device);
        let clone = device.clone();
        let unknown: IUnknown = device.cast()?;
        let round_trip: ID3D11Device = unknown.cast()?;
        assert!(Arc::ptr_eq(&gate, &for_device(&clone)));
        assert!(Arc::ptr_eq(&gate, &for_device(&round_trip)));
        Ok(())
    }

    #[test]
    fn protect_device_enables_multithread_protection_on_a_real_device() -> Result<()> {
        let device = create_warp_device()?;
        protect_device(&device)?;
        // Idempotent: a second enable still reads back as protected.
        protect_device(&device)?;
        Ok(())
    }

    /// Capture (DXGI acquire) and encoder (NVENC) threads both use one gate.
    /// Their native sections must never overlap.
    #[test]
    fn capture_and_encoder_device_calls_never_overlap_on_a_shared_gate() {
        let gate = for_device_key(0x4000);
        let overlap = Overlap::default();
        let rounds = 200;
        thread::scope(|scope| {
            for role in ["capture", "encoder"] {
                let (gate, overlap) = (gate.clone(), &overlap);
                scope.spawn(move || {
                    for _ in 0..rounds {
                        let _guard = gate.lock();
                        overlap.enter();
                        if role == "encoder" {
                            thread::sleep(Duration::from_micros(50));
                        } else {
                            thread::yield_now();
                        }
                        overlap.leave();
                    }
                });
            }
        });
        assert_eq!(overlap.max_inside.load(Ordering::SeqCst), 1);
    }

    /// Control for the test above: without the gate, a barrier forces both
    /// sections to be inside at once, so the measurement can detect overlap.
    #[test]
    fn ungated_device_calls_are_detected_as_overlapping() {
        let overlap = Overlap::default();
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    overlap.enter();
                    barrier.wait();
                    overlap.leave();
                });
            }
        });
        assert_eq!(overlap.max_inside.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[should_panic(expected = "GPU gate re-entered on the same thread")]
    fn nested_acquisition_on_one_thread_panics_instead_of_deadlocking() {
        let gate = for_device_key(0x5000);
        let _outer = gate.lock();
        let _inner = gate.lock();
    }

    #[test]
    fn released_gate_is_reacquired_on_the_same_thread() {
        let gate = for_device_key(0x6000);
        drop(gate.lock());
        let _again = gate.lock();
    }
}
