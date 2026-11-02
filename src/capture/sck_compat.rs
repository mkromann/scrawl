//! ScreenCaptureKit's newer API that Scrawl calls: `SCScreenshotManager` (macOS 14) and
//! `SCRecordingOutput` (macOS 15). `screencapturekit` leaves both out when built for macOS 13
//! (`Cargo.toml`), so the same names live here and the callers read as they would against the
//! crate.
//!
//! Nothing is implemented yet: every call reports `Unsupported`, which the callers already take
//! as a failed capture.

use std::fmt;
use std::path::Path;

use screencapturekit::prelude::*;

/// What a call into this module fails with.
#[derive(Debug)]
pub struct Unsupported(&'static str);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} isn't available on this macOS yet", self.0)
    }
}

impl std::error::Error for Unsupported {}

pub enum ImageFormat {
    Png,
}

/// A captured screenshot, as `CGImageExt` shows a `CGImage` to callers.
pub struct Screenshot {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl Screenshot {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn rgba_data(&self) -> Result<Vec<u8>, Unsupported> {
        Ok(self.rgba.clone())
    }

    pub fn save(&self, _path: &str, _format: ImageFormat) -> Result<(), Unsupported> {
        Err(Unsupported("Saving a screenshot"))
    }
}

pub struct SCScreenshotManager;

impl SCScreenshotManager {
    pub fn capture_image(
        _filter: &SCContentFilter,
        _config: &SCStreamConfiguration,
    ) -> Result<Screenshot, Unsupported> {
        Err(Unsupported("SCScreenshotManager"))
    }
}

pub enum SCRecordingOutputCodec {
    H264,
}

pub enum SCRecordingOutputFileType {
    MP4,
}

pub struct SCRecordingOutputConfiguration;

impl SCRecordingOutputConfiguration {
    pub fn new() -> Result<Self, Unsupported> {
        Err(Unsupported("SCRecordingOutput"))
    }

    pub fn with_output_url(self, _path: &Path) -> Result<Self, Unsupported> {
        Ok(self)
    }

    pub fn with_video_codec(self, _codec: SCRecordingOutputCodec) -> Self {
        self
    }

    pub fn with_output_file_type(self, _file_type: SCRecordingOutputFileType) -> Self {
        self
    }
}

pub struct SCRecordingOutput;

impl SCRecordingOutput {
    pub fn new(_config: &SCRecordingOutputConfiguration) -> Option<Self> {
        None
    }
}

/// `SCStream`'s recording methods, which the crate only has for macOS 15.
pub trait SCStreamRecording {
    fn add_recording_output(&self, output: &SCRecordingOutput) -> Result<(), Unsupported>;
    fn remove_recording_output(&self, output: &SCRecordingOutput) -> Result<(), Unsupported>;
}

impl SCStreamRecording for SCStream {
    fn add_recording_output(&self, _output: &SCRecordingOutput) -> Result<(), Unsupported> {
        Err(Unsupported("SCRecordingOutput"))
    }

    fn remove_recording_output(&self, _output: &SCRecordingOutput) -> Result<(), Unsupported> {
        Err(Unsupported("SCRecordingOutput"))
    }
}
