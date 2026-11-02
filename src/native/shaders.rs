//! The Metal shaders as source, for a build without the `metal` tool (`scrawl_runtime_shaders`,
//! see `build.rs`): Metal compiles them here instead.
//!
//! Metal keeps what it compiled on disk, keyed by the source, so only the first compile of a
//! shader is slow. On an M1 that's 60–130 ms (laser) and 40–100 ms (pill), then 0.1–2.5 ms. The
//! overlay compiles on the main thread at every recording start, and starting must stay instant
//! (AGENTS.md), so `warm_up` pays for any missing entry at launch, on its own thread. GPUI's
//! shaders aren't here: they only compile with a GPUI window, which ⌥⇧S doesn't open.

use std::thread;

use metal::{CompileOptions, DeviceRef, Library};

pub const LASER: &str = include_str!("../laser_overlay.metal");
pub const PILL: &str = include_str!("pill/pill.metal");

pub fn library(device: &DeviceRef, source: &str) -> Result<Library, String> {
    device.new_library_with_source(source, &CompileOptions::new())
}

pub fn warm_up() {
    thread::spawn(|| {
        let Some(device) = metal::Device::system_default() else {
            return;
        };
        for source in [LASER, PILL] {
            if let Err(error) = library(&device, source) {
                crate::capture::scrawl_log(format!("Shader compile failed: {error}"));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the metallib's target check covers elsewhere: without one, it's this macOS's Metal
    /// that has to accept the source, and an error here would leave the overlay shut.
    #[test]
    fn both_shaders_compile_from_source() {
        let device = metal::Device::system_default().expect("a Metal device");
        for source in [LASER, PILL] {
            library(&device, source).expect("the shader compiles");
        }
    }
}
