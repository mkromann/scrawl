//! Stands in for `apple-metal` 0.10, whose Swift bridge needs newer SDKs than macOS 13's.
//! `screencapturekit` only reaches it for `MetalDevice::as_apple_metal`, which Scrawl never calls,
//! so this is that one function's types and nothing else. See `vendor/README.md`.

use core::ffi::c_void;
use core::ops::Deref;

/// Apple's `id<MTLDevice>`.
pub struct MetalDevice {
    _ptr: *mut c_void,
}

impl MetalDevice {
    /// Wraps a device without taking a reference to it.
    ///
    /// # Safety
    ///
    /// `ptr` must stay a valid `id<MTLDevice>` for as long as the result lives.
    #[must_use]
    pub unsafe fn from_raw_borrowed(ptr: *mut c_void) -> ManuallyDropDevice {
        ManuallyDropDevice {
            inner: Self { _ptr: ptr },
        }
    }
}

/// Borrowed [`MetalDevice`] that does not release on drop.
pub struct ManuallyDropDevice {
    inner: MetalDevice,
}

impl Deref for ManuallyDropDevice {
    type Target = MetalDevice;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
