//! `recording.mp4`: an `AVAssetWriter` that takes `SCStream`'s sample buffers. AVFoundation raises
//! Objective-C exceptions for misuse, which Rust can't catch, so settings are checked with
//! `canApplyOutputSettings:` first and nothing is called out of order (`Recorder` in the parent
//! holds the writer behind one lock).

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::time::Duration;

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSString, NSURL};
use screencapturekit::cm::CMSampleTimingInfo;
use screencapturekit::prelude::{CMSampleBuffer, CMTime};

/// How often a fragment reaches the disk, which is what's left of the movie if the app dies.
const FRAGMENT_SECONDS: f64 = 1.0;
/// A sync frame this many frames apart at most, so finding a keyframe decodes 2 s of video at
/// worst. In frames rather than seconds: a still screen sends almost none, and a timed interval
/// would turn each of those into a full picture.
const MAX_FRAMES_BETWEEN_SYNC: i64 = 120;
/// What `SCStream` delivers at most.
pub(super) const FRAME_RATE: u32 = 60;

/// `AVAssetWriterStatus`.
const STATUS_WRITING: isize = 1;
const STATUS_FAILED: isize = 3;

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVFileTypeMPEG4: &'static NSString;
    static AVMediaTypeVideo: &'static NSString;
    static AVVideoCodecKey: &'static NSString;
    static AVVideoCodecTypeH264: &'static NSString;
    static AVVideoWidthKey: &'static NSString;
    static AVVideoHeightKey: &'static NSString;
    static AVVideoCompressionPropertiesKey: &'static NSString;
    static AVVideoAverageBitRateKey: &'static NSString;
    static AVVideoExpectedSourceFrameRateKey: &'static NSString;
    static AVVideoMaxKeyFrameIntervalKey: &'static NSString;
    static AVVideoAllowFrameReorderingKey: &'static NSString;
    static AVVideoProfileLevelKey: &'static NSString;
    static AVVideoProfileLevelH264HighAutoLevel: &'static NSString;
    static AVVideoColorPropertiesKey: &'static NSString;
    static AVVideoColorPrimariesKey: &'static NSString;
    static AVVideoColorPrimaries_ITU_R_709_2: &'static NSString;
    static AVVideoTransferFunctionKey: &'static NSString;
    static AVVideoTransferFunction_ITU_R_709_2: &'static NSString;
    static AVVideoYCbCrMatrixKey: &'static NSString;
    static AVVideoYCbCrMatrix_ITU_R_709_2: &'static NSString;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMClockGetHostTimeClock() -> *mut c_void;
    fn CMClockGetTime(clock: *mut c_void) -> CMTime;
    fn CMSampleBufferCreateCopyWithNewTiming(
        allocator: *const c_void,
        original: *mut c_void,
        timing_count: isize,
        timing: *const CMSampleTimingInfo,
        copy: *mut *mut c_void,
    ) -> i32;
}

/// `struct opaqueCMSampleBuffer`, so `appendSampleBuffer:` passes objc2's encoding check.
#[repr(C)]
struct OpaqueSampleBuffer {
    _private: [u8; 0],
}

unsafe impl RefEncode for OpaqueSampleBuffer {
    const ENCODING_REF: Encoding =
        Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

/// A `CMTime` as an Objective-C argument.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct Time(CMTime);

unsafe impl Encode for Time {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[
            Encoding::LongLong,
            Encoding::Int,
            Encoding::UInt,
            Encoding::LongLong,
        ],
    );
}

/// Now on the clock `SCStream` stamps its samples with.
pub(super) fn host_now() -> CMTime {
    unsafe { CMClockGetTime(CMClockGetHostTimeClock()) }
}

pub(super) fn nanos(time: CMTime) -> i64 {
    if time.timescale == 0 {
        return 0;
    }
    (i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale)) as i64
}

pub(super) fn from_nanos(nanos: i64) -> CMTime {
    CMTime::new(nanos, 1_000_000_000)
}

/// The same pixels, shown at `pts` for a frame. The duration matters when this ends the movie: a
/// writer otherwise takes the last sample to run as long as the gap before it.
pub(super) fn retimed(sample: &CMSampleBuffer, pts: CMTime) -> Option<CMSampleBuffer> {
    let timing = CMSampleTimingInfo {
        duration: CMTime::new(1, FRAME_RATE as i32),
        presentation_time_stamp: pts,
        decode_time_stamp: CMTime::INVALID,
    };
    let mut copy = ptr::null_mut();
    let status = unsafe {
        CMSampleBufferCreateCopyWithNewTiming(
            ptr::null(),
            sample.as_ptr(),
            1,
            &raw const timing,
            &raw mut copy,
        )
    };
    if status != 0 {
        return None;
    }
    unsafe { CMSampleBuffer::from_raw(copy) }
}

/// Average bits per second. Sharp text wants about 12 Mbps at 2560x1600 (measured on a hard cut
/// between pages of code: 34 dB luma at 4 Mbps, 37.9 at 8, 39.1 at 12, 39.7 at 16), and a movie
/// only spends this when the screen is moving. Smaller frames have less to spare per pixel.
pub(super) fn bit_rate(width: u32, height: u32) -> i64 {
    (i64::from(width) * i64::from(height) * 3).clamp(4_000_000, 14_000_000)
}

fn output_settings(width: u32, height: u32) -> Retained<NSDictionary<NSString, AnyObject>> {
    let (bits, rate, gap, reorder) = (
        NSNumber::new_i64(bit_rate(width, height)),
        NSNumber::new_i64(i64::from(FRAME_RATE)),
        NSNumber::new_i64(MAX_FRAMES_BETWEEN_SYNC),
        NSNumber::new_bool(false),
    );
    let (wide, high) = (
        NSNumber::new_i64(i64::from(width)),
        NSNumber::new_i64(i64::from(height)),
    );
    unsafe {
        let compression: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[
                AVVideoAverageBitRateKey,
                AVVideoExpectedSourceFrameRateKey,
                AVVideoMaxKeyFrameIntervalKey,
                AVVideoAllowFrameReorderingKey,
                AVVideoProfileLevelKey,
            ],
            &[
                bits.as_ref(),
                rate.as_ref(),
                gap.as_ref(),
                reorder.as_ref(),
                AVVideoProfileLevelH264HighAutoLevel.as_ref(),
            ],
        );
        // Tagged so a decoder turns the frames back into what the screen showed: the encoder
        // converts from the frames' own sRGB tag to match.
        let color: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[
                AVVideoColorPrimariesKey,
                AVVideoTransferFunctionKey,
                AVVideoYCbCrMatrixKey,
            ],
            &[
                AVVideoColorPrimaries_ITU_R_709_2.as_ref(),
                AVVideoTransferFunction_ITU_R_709_2.as_ref(),
                AVVideoYCbCrMatrix_ITU_R_709_2.as_ref(),
            ],
        );
        NSDictionary::from_slices(
            &[
                AVVideoCodecKey,
                AVVideoWidthKey,
                AVVideoHeightKey,
                AVVideoCompressionPropertiesKey,
                AVVideoColorPropertiesKey,
            ],
            &[
                AVVideoCodecTypeH264.as_ref(),
                wide.as_ref(),
                high.as_ref(),
                compression.as_ref(),
                color.as_ref(),
            ],
        )
    }
}

fn described(error: Option<Retained<NSError>>) -> String {
    error.map_or_else(
        || "no error given".to_string(),
        |e| e.localizedDescription().to_string(),
    )
}

/// An open `AVAssetWriter` and its one video input. Calls are made from one thread at a time.
pub(super) struct Movie {
    writer: Retained<AnyObject>,
    input: Retained<AnyObject>,
    session_started: bool,
}

// Both objects may move between threads; `Recorder` keeps their use serial.
unsafe impl Send for Movie {}

impl Movie {
    /// Replaces whatever is at `path` and starts writing. Fragments reach the disk as they fill,
    /// so a movie cut off by a crash still plays up to its last fragment; finishing leaves an
    /// ordinary MP4.
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self, String> {
        autoreleasepool(|_| unsafe {
            let _ = std::fs::remove_file(path);
            let url = NSURL::from_file_path(path).ok_or("the movie's path isn't a file URL")?;
            let writer_class = AnyClass::get(c"AVAssetWriter").ok_or("no AVAssetWriter")?;
            let input_class =
                AnyClass::get(c"AVAssetWriterInput").ok_or("no AVAssetWriterInput")?;
            let writer: Result<Retained<AnyObject>, Retained<NSError>> = msg_send![
                writer_class,
                assetWriterWithURL: &*url,
                fileType: AVFileTypeMPEG4,
                error: _
            ];
            let writer = writer.map_err(|error| error.localizedDescription().to_string())?;
            let settings = output_settings(width, height);
            let usable: bool = msg_send![
                &*writer,
                canApplyOutputSettings: &*settings,
                forMediaType: AVMediaTypeVideo
            ];
            if !usable {
                return Err(format!("H.264 at {width}x{height} isn't available"));
            }
            let input: Option<Retained<AnyObject>> = msg_send![
                input_class,
                assetWriterInputWithMediaType: AVMediaTypeVideo,
                outputSettings: &*settings
            ];
            let input = input.ok_or("no writer input")?;
            let () = msg_send![&*input, setExpectsMediaDataInRealTime: true];
            let addable: bool = msg_send![&*writer, canAddInput: &*input];
            if !addable {
                return Err("the writer won't take the input".to_string());
            }
            let () = msg_send![&*writer, addInput: &*input];
            let every = Time(CMTime::new((FRAGMENT_SECONDS * 600.0) as i64, 600));
            let () = msg_send![&*writer, setMovieFragmentInterval: every];
            let started: bool = msg_send![&*writer, startWriting];
            if !started {
                let error: Option<Retained<NSError>> = msg_send![&*writer, error];
                return Err(described(error));
            }
            Ok(Self {
                writer,
                input,
                session_started: false,
            })
        })
    }

    fn status(&self) -> isize {
        unsafe { msg_send![&*self.writer, status] }
    }

    /// Why the writer gave up, once it has.
    pub fn failure(&self) -> Option<String> {
        (self.status() == STATUS_FAILED).then(|| {
            let error: Option<Retained<NSError>> = unsafe { msg_send![&*self.writer, error] };
            described(error)
        })
    }

    /// Starts the movie's clock at `at`, the first frame's time.
    pub fn start_session(&mut self, at: CMTime) {
        if self.status() == STATUS_WRITING {
            let () = unsafe { msg_send![&*self.writer, startSessionAtSourceTime: Time(at)] };
            self.session_started = true;
        }
    }

    pub fn ready(&self) -> bool {
        self.session_started
            && self.status() == STATUS_WRITING
            && unsafe { msg_send![&*self.input, isReadyForMoreMediaData] }
    }

    /// Call only when `ready`.
    pub fn append(&self, sample: &CMSampleBuffer) -> bool {
        let sample: NonNull<OpaqueSampleBuffer> = match NonNull::new(sample.as_ptr().cast()) {
            Some(sample) => sample,
            None => return false,
        };
        unsafe { msg_send![&*self.input, appendSampleBuffer: sample.as_ptr()] }
    }

    /// Ends the movie at `end` and waits for the file to close.
    pub fn finish(&self, end: CMTime, timeout: Duration) -> Result<(), String> {
        autoreleasepool(|_| {
            if let Some(failure) = self.failure() {
                return Err(failure);
            }
            if !self.session_started || self.status() != STATUS_WRITING {
                return Err("nothing was written".to_string());
            }
            let () = unsafe { msg_send![&*self.input, markAsFinished] };
            let () = unsafe { msg_send![&*self.writer, endSessionAtSourceTime: Time(end)] };
            let (done, finished) = mpsc::channel();
            let block = block2::RcBlock::new(move || {
                let _ = done.send(());
            });
            let () =
                unsafe { msg_send![&*self.writer, finishWritingWithCompletionHandler: &*block] };
            finished
                .recv_timeout(timeout)
                .map_err(|_| format!("the writer didn't finish within {timeout:?}"))?;
            self.failure().map_or(Ok(()), Err)
        })
    }

    /// Abandons the file. The caller removes it.
    pub fn cancel(&self) {
        if self.status() == STATUS_WRITING {
            let () = unsafe { msg_send![&*self.writer, cancelWriting] };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_rate_scales_with_the_frame_within_limits() {
        assert_eq!(bit_rate(2560, 1600), 12_288_000);
        assert_eq!(bit_rate(640, 400), 4_000_000);
        assert_eq!(bit_rate(3840, 2400), 14_000_000);
    }

    #[test]
    fn times_convert_through_any_timescale() {
        assert_eq!(nanos(CMTime::new(3, 2)), 1_500_000_000);
        assert_eq!(nanos(from_nanos(123_456_789)), 123_456_789);
        assert_eq!(nanos(CMTime::new(5, 0)), 0);
    }
}
