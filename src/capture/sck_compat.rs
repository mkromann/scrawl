//! ScreenCaptureKit's newer API that Scrawl calls: `SCScreenshotManager` (macOS 14) and
//! `SCRecordingOutput` (macOS 15). `screencapturekit` leaves both out when built for macOS 13
//! (`Cargo.toml`), so the same names live here, on what macOS 13 has, and the callers read as they
//! would against the crate:
//!
//! - `SCRecordingOutput` is an `SCStream` output that feeds an `AVAssetWriter` (`movie`).
//! - `SCScreenshotManager` takes the first frame of a short-lived stream.
//! - `CandidateStream` replaces the sampler's repeated screenshots: a stream that size takes
//!   hundreds of milliseconds to start, so a second, small one stays up and its newest frame is
//!   read when a candidate is due.
//!
//! All frames are BGRA in sRGB, which keeps a movie's colors and a candidate's the same whatever
//! profile the display uses. (macOS 13's default for a stream is 420v in the display's own space.)

mod movie;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use screencapturekit::cm::SCFrameStatus;
use screencapturekit::dispatch_queue::{DispatchQoS, DispatchQueue};
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::colors::color_space;

use super::scrawl_log;
use crate::model::RgbaFrame;
use crate::recover::RECORDING;
use movie::{FRAME_RATE, Movie, from_nanos, host_now, nanos, retimed};

/// How long a stream may take to show its first frame. Starting is the slow part; the first frame
/// doesn't wait for the screen to change.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// `SCStream`'s stop and `SCShareableContent` answer in milliseconds, or, when `replayd` has gone
/// away, only at the crate's 30 s limit. Nothing is worth waiting that long for.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const CONTENT_TIMEOUT: Duration = Duration::from_millis(2_500);
/// Closing a movie rewrites its index, which takes tens of milliseconds even for a long one.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a still screen's frame goes into the movie again. Each one flushes the fragment
/// before it, so what's on disk stays current without the screen moving (`src/recover.rs`), and
/// costs a few bytes.
const REPEAT_EVERY: Duration = Duration::from_secs(2);
/// Sample times closer than this could round to the same tick of the movie's 1/600 s clock.
const MIN_FRAME_GAP_NS: i64 = 2_000_000;
const FRAME_NS: i64 = 1_000_000_000 / FRAME_RATE as i64;
/// The candidate stream's rate: a frame is at most this old when it's read.
const CANDIDATE_RATE: u32 = 30;
/// How long an urgent candidate waits to see a sample from after the request. Two of the stream's
/// frames: past that the screen hasn't changed and the frame in hand is current.
const FRESH_WAIT: Duration = Duration::from_millis(2_000 / CANDIDATE_RATE as u64);

/// What a call into this module fails with.
#[derive(Debug)]
pub struct CaptureFailed(String);

impl fmt::Display for CaptureFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CaptureFailed {}

impl From<SCError> for CaptureFailed {
    fn from(error: SCError) -> Self {
        Self(format!("{error:?}"))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_complete(sample: &CMSampleBuffer) -> bool {
    sample.frame_status() == Some(SCFrameStatus::Complete)
}

/// The stream settings every stream here shares. `scalesToFit` is off by default on macOS 13,
/// where a window or `sourceRect` larger than the output would come out cropped.
fn stream_config(config: &SCStreamConfiguration) -> SCStreamConfiguration {
    let mut config = config
        .clone()
        .with_pixel_format(PixelFormat::BGRA)
        .with_scales_to_fit(true);
    if let Err(error) = config.set_color_space_name(color_space::SRGB) {
        scrawl_log(format!("Stream color space: {error:?}"));
    }
    config
}

/// `SCShareableContent::get()`, tried a second time rather than waited out if `replayd` doesn't
/// answer, as the sampler can't be held up for the crate's 30 s (the first try runs out on its own
/// thread).
pub fn shareable_content() -> Result<SCShareableContent, SCError> {
    for _ in 0..2 {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(SCShareableContent::get());
        });
        match receiver.recv_timeout(CONTENT_TIMEOUT) {
            Ok(content) => return content,
            Err(_) => scrawl_log(format!(
                "SCShareableContent: no answer in {CONTENT_TIMEOUT:?}"
            )),
        }
    }
    Err(SCError::NoShareableContent(
        "ScreenCaptureKit isn't answering".to_string(),
    ))
}

/// Stops `stream`, or gives up on it after `STOP_TIMEOUT`: a stop that hangs mustn't hold the movie
/// or the app up. Late samples are refused downstream.
pub fn stop_stream(stream: &SCStream) {
    let stream = stream.clone();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(stream.stop_capture());
    });
    match receiver.recv_timeout(STOP_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => scrawl_log(format!("Stopping a stream: {error:?}")),
        Err(_) => scrawl_log(format!("A stream didn't stop within {STOP_TIMEOUT:?}")),
    }
}

/// A BGRA sample as RGBA bytes, row by row (a row can be longer than its pixels).
fn sample_rgba(sample: &CMSampleBuffer) -> Option<(u32, u32, Vec<u8>)> {
    let buffer = sample.pixel_buffer()?;
    let (width, height, stride) = (buffer.width(), buffer.height(), buffer.bytes_per_row());
    let guard = buffer.lock_read_only().ok()?;
    let bytes = unsafe { guard.as_slice() }?;
    Some((
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        bgra_to_rgba(bytes, width, height, stride)?,
    ))
}

fn bgra_to_rgba(bytes: &[u8], width: usize, height: usize, stride: usize) -> Option<Vec<u8>> {
    if width == 0
        || height == 0
        || stride < width * 4
        || bytes.len() < stride * (height - 1) + width * 4
    {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in bytes.chunks(stride).take(height) {
        for [blue, green, red, alpha] in row.get(..width * 4)?.as_chunks().0 {
            rgba.extend_from_slice(&[*red, *green, *blue, *alpha]);
        }
    }
    Some(rgba)
}

#[derive(Default)]
struct Slot {
    frame: Option<CMSampleBuffer>,
    /// The time of the newest sample, including the idle ones that say nothing changed.
    seen_ns: i64,
    /// Frames from before this may show something the filter now leaves out.
    filtered_ns: i64,
}

/// The newest frame of a stream.
#[derive(Default)]
struct Latest {
    slot: Mutex<Slot>,
    arrived: Condvar,
}

impl Latest {
    fn take(&self, sample: CMSampleBuffer) {
        let idle = sample.frame_status() == Some(SCFrameStatus::Idle);
        if !idle && !is_complete(&sample) {
            return;
        }
        let mut slot = lock(&self.slot);
        slot.seen_ns = slot.seen_ns.max(nanos(sample.presentation_timestamp()));
        if !idle {
            slot.frame = Some(sample);
        }
        drop(slot);
        self.arrived.notify_all();
    }

    fn wait_until(&self, timeout: Duration, done: impl Fn(&Slot) -> bool) -> MutexGuard<'_, Slot> {
        let deadline = Instant::now() + timeout;
        let mut slot = lock(&self.slot);
        while !done(&slot) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            slot = self
                .arrived
                .wait_timeout(slot, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        slot
    }
}

/// A running stream that keeps its newest frame for whoever asks.
pub struct CandidateStream {
    stream: SCStream,
    latest: Arc<Latest>,
}

impl CandidateStream {
    /// Starts sending frames at `rate` per second; returns once the first one is in, or, unless
    /// `need_first`, once `FIRST_FRAME_TIMEOUT` has passed without one.
    fn with_rate(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
        rate: u32,
        need_first: bool,
    ) -> Result<Self, CaptureFailed> {
        let latest = Arc::new(Latest::default());
        let config = stream_config(config).with_fps(rate).with_queue_depth(3);
        let mut stream = SCStream::new(filter, &config)?;
        let sink = Arc::clone(&latest);
        let queue = DispatchQueue::new("scrawl.candidates", DispatchQoS::UserInitiated);
        stream.add_output_handler_with_queue(
            move |sample: CMSampleBuffer, kind| {
                if matches!(kind, SCStreamOutputType::Screen) {
                    sink.take(sample);
                }
            },
            SCStreamOutputType::Screen,
            Some(&queue),
        )?;
        stream.start_capture()?;
        let candidates = Self { stream, latest };
        let first = candidates
            .latest
            .wait_until(FIRST_FRAME_TIMEOUT, |slot| slot.frame.is_some())
            .frame
            .is_some();
        if first {
            Ok(candidates)
        } else if !need_first {
            // A covered or minimized window sends nothing yet; its frames come once it shows.
            scrawl_log("Candidate stream: no first frame yet; keeping it running");
            Ok(candidates)
        } else {
            candidates.stop();
            Err(CaptureFailed("no frame from ScreenCaptureKit".to_string()))
        }
    }

    pub fn start(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
    ) -> Result<Self, CaptureFailed> {
        Self::with_rate(filter, config, CANDIDATE_RATE, false)
    }

    /// Hides or shows windows from now on. The next frame read comes from after the change.
    pub fn update_content_filter(&self, filter: &SCContentFilter) -> Result<(), CaptureFailed> {
        self.stream.update_content_filter(filter)?;
        lock(&self.latest.slot).filtered_ns = nanos(host_now());
        Ok(())
    }

    /// The screen now, stamped `t_ms`. `SCStream` sends frames only when something changed, so the
    /// newest one is the screen unless a change is on its way, and an urgent frame (a click, a
    /// stroke) waits for that: it's the picture of that moment that matters. So does the first
    /// frame after a filter change.
    pub fn frame(&self, t_ms: u64, urgent: bool) -> Option<RgbaFrame> {
        let asked_ns = nanos(host_now());
        let wanted_ns = lock(&self.latest.slot).filtered_ns;
        let wanted_ns = if urgent { asked_ns } else { wanted_ns };
        let slot = self
            .latest
            .wait_until(FRESH_WAIT, |slot| slot.seen_ns >= wanted_ns);
        let (width, height, rgba) = sample_rgba(slot.frame.as_ref()?)?;
        Some(RgbaFrame::new(t_ms, width, height, rgba))
    }

    pub fn stop(self) {
        stop_stream(&self.stream);
    }
}

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

    pub fn save(&self, path: &str, _format: ImageFormat) -> Result<(), CaptureFailed> {
        let failed = |error: &dyn fmt::Display| CaptureFailed(format!("Saving {path}: {error}"));
        let file = std::fs::File::create(path).map_err(|e| failed(&e))?;
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            self.width as u32,
            self.height as u32,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| failed(&e))?;
        writer.write_image_data(&self.rgba).map_err(|e| failed(&e))
    }
}

pub struct SCScreenshotManager;

impl SCScreenshotManager {
    /// The first complete frame of a stream of its own.
    pub fn capture_image(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
    ) -> Result<Screenshot, CaptureFailed> {
        let stream = CandidateStream::with_rate(filter, config, FRAME_RATE, true)?;
        let frame = stream.frame(0, false);
        stream.stop();
        let frame = frame.ok_or_else(|| CaptureFailed("the frame had no pixels".to_string()))?;
        Ok(Screenshot {
            width: frame.width as usize,
            height: frame.height as usize,
            rgba: frame.rgba,
        })
    }
}

pub enum SCRecordingOutputCodec {
    H264,
}

pub enum SCRecordingOutputFileType {
    MP4,
}

pub struct SCRecordingOutputConfiguration {
    path: PathBuf,
    size: (u32, u32),
    clock: Option<(Instant, Arc<AtomicU64>)>,
}

impl SCRecordingOutputConfiguration {
    pub fn new() -> Result<Self, CaptureFailed> {
        Ok(Self {
            path: PathBuf::new(),
            size: (0, 0),
            clock: None,
        })
    }

    pub fn with_output_url(mut self, path: &Path) -> Result<Self, CaptureFailed> {
        self.path = path.to_path_buf();
        Ok(self)
    }

    /// H.264 is all this writes.
    pub fn with_video_codec(self, _codec: SCRecordingOutputCodec) -> Self {
        self
    }

    /// MP4 is all this writes.
    pub fn with_output_file_type(self, _file_type: SCRecordingOutputFileType) -> Self {
        self
    }

    /// The frames' size, which the real `SCRecordingOutput` gets from its stream.
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.size = (width, height);
        self
    }

    /// Stores into `offset_ms` when the movie starts on the clock that began at `started_at`.
    pub fn with_clock(mut self, started_at: Instant, offset_ms: Arc<AtomicU64>) -> Self {
        self.clock = Some((started_at, offset_ms));
        self
    }
}

struct Recording {
    movie: Movie,
    /// The newest frame, to repeat while the screen is still and to hold to the end.
    held: Option<CMSampleBuffer>,
    /// `held` didn't get into the movie: the encoder was busy.
    behind: bool,
    first_ns: Option<i64>,
    last_ns: i64,
    last_written: Instant,
    /// Frames from here on aren't part of the movie.
    end_ns: Option<i64>,
    frames: u64,
    repeats: u64,
    dropped: u64,
    closed: bool,
}

impl Recording {
    fn write(&mut self, sample: &CMSampleBuffer, pts_ns: i64) -> bool {
        if self.movie.ready() && self.movie.append(sample) {
            self.last_ns = pts_ns;
            self.last_written = Instant::now();
            self.behind = false;
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// The held frame again at `pts_ns`, if the movie has room for it there.
    fn repeat_at(&mut self, pts_ns: i64) -> bool {
        let Some(copy) = self
            .held
            .as_ref()
            .filter(|_| pts_ns >= self.last_ns + MIN_FRAME_GAP_NS)
            .and_then(|held| retimed(held, from_nanos(pts_ns)))
        else {
            return false;
        };
        let written = self.write(&copy, pts_ns);
        self.repeats += u64::from(written);
        written
    }
}

/// The movie being written and when it started, shared with the stream's output queue.
struct Recorder {
    path: PathBuf,
    started_at: Instant,
    offset_ms: Arc<AtomicU64>,
    recording: Mutex<Recording>,
}

impl Recorder {
    fn take(&self, sample: CMSampleBuffer) {
        if is_complete(&sample) {
            self.add_frame(sample);
        }
    }

    fn add_frame(&self, sample: CMSampleBuffer) {
        let pts = sample.presentation_timestamp();
        let pts_ns = nanos(pts);
        let mut recording = lock(&self.recording);
        if recording.closed || recording.end_ns.is_some_and(|end| pts_ns >= end) {
            return;
        }
        if recording.first_ns.is_none() {
            recording.movie.start_session(pts);
            recording.first_ns = Some(pts_ns);
            recording.last_ns = pts_ns - MIN_FRAME_GAP_NS;
            // Sample times are on the host clock, so this is when the frame was on screen on the
            // capture's, not a guess around `start_capture`.
            let age_ns = nanos(host_now()).saturating_sub(pts_ns);
            let elapsed_ns = self.started_at.elapsed().as_nanos() as i64;
            let offset_ms = (elapsed_ns - age_ns).max(0) / 1_000_000;
            self.offset_ms.store(offset_ms as u64, Ordering::Release);
            crate::ui::start_metric("first_frame_ms", self.started_at);
        }
        // A repeat can have taken this frame's place in line.
        let earliest_ns = recording.last_ns + MIN_FRAME_GAP_NS;
        let written = if pts_ns < earliest_ns {
            retimed(&sample, from_nanos(earliest_ns))
                .is_some_and(|late| recording.write(&late, earliest_ns))
        } else {
            recording.write(&sample, pts_ns)
        };
        recording.frames += u64::from(written);
        // Even a dropped frame is the newest picture of the screen.
        recording.held = Some(sample);
        recording.behind = !written;
    }

    /// Also catches up on a dropped frame: the screen may stay on it.
    fn repeat_if_still(&self) {
        let mut recording = lock(&self.recording);
        let due = recording.behind || recording.last_written.elapsed() >= REPEAT_EVERY;
        if due && !recording.closed && recording.movie.ready() {
            recording.last_written = Instant::now();
            recording.repeat_at(nanos(host_now()));
        }
    }

    fn stop_taking_frames(&self) {
        let mut recording = lock(&self.recording);
        if recording.end_ns.is_none() {
            recording.end_ns = Some(nanos(host_now()));
        }
    }

    /// Closes the movie. `SCStream` sends nothing while the screen is still and a writer ends a
    /// movie at its last sample, whatever `endSession` says, so the last frame goes in again just
    /// before the end. Without that a still screen's movie comes out short and keyframes near its
    /// end have nothing to decode.
    fn finish(&self) -> bool {
        self.stop_taking_frames();
        let mut recording = lock(&self.recording);
        if recording.closed {
            return false;
        }
        recording.closed = true;
        let started = Instant::now();
        let end_ns = recording.end_ns.unwrap_or_else(|| nanos(host_now()));
        let finished = if recording.first_ns.is_some() {
            let deadline = Instant::now() + Duration::from_millis(200);
            while !recording.movie.ready() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if recording.last_ns + 2 * FRAME_NS <= end_ns {
                recording.repeat_at(end_ns - FRAME_NS);
            }
            recording.movie.finish(from_nanos(end_ns), FINISH_TIMEOUT)
        } else {
            Err("no frames arrived".to_string())
        };
        recording.held = None;
        scrawl_log(format!(
            "Recording: {} frames, {} repeats, {} dropped, closed in {} ms",
            recording.frames,
            recording.repeats,
            recording.dropped,
            started.elapsed().as_millis()
        ));
        match finished {
            Ok(()) => true,
            Err(error) => {
                scrawl_log(format!("Recording failed: {error}"));
                recording.movie.cancel();
                let _ = std::fs::remove_file(&self.path);
                let _ = std::fs::remove_file(self.path.with_file_name(RECORDING));
                false
            }
        }
    }
}

/// Writes `recording.mp4` from the stream it's added to (`SCStreamRecording`).
pub struct SCRecordingOutput {
    recorder: Arc<Recorder>,
}

impl SCRecordingOutput {
    /// Opens the movie. Starting `AVAssetWriter` here rather than at the first frame keeps its
    /// cost (AVFoundation loading, the encoder) off the stream's queue.
    pub fn new(config: &SCRecordingOutputConfiguration) -> Option<Self> {
        let (width, height) = config.size;
        let (started_at, offset_ms) = config.clock.clone()?;
        let movie = Movie::create(&config.path, width, height)
            .map_err(|error| scrawl_log(format!("Recording could not start: {error}")))
            .ok()?;
        let _ = std::fs::write(
            config.path.with_file_name(RECORDING),
            std::process::id().to_string(),
        );
        Some(Self {
            recorder: Arc::new(Recorder {
                path: config.path.clone(),
                started_at,
                offset_ms,
                recording: Mutex::new(Recording {
                    movie,
                    held: None,
                    behind: false,
                    first_ns: None,
                    last_ns: 0,
                    last_written: Instant::now(),
                    end_ns: None,
                    frames: 0,
                    repeats: 0,
                    dropped: 0,
                    closed: false,
                }),
            }),
        })
    }

    /// `config` as a stream feeding this output needs it. Six frames: the encoder and `held` keep
    /// some, and a stream with none free drops frames.
    pub fn stream_config(config: &SCStreamConfiguration) -> SCStreamConfiguration {
        stream_config(config)
            .with_fps(FRAME_RATE)
            .with_queue_depth(6)
    }

    /// Call often while recording: keeps the file current on a still screen.
    pub fn tick(&self) {
        self.recorder.repeat_if_still();
    }

    /// The movie ends now: nothing the stream sends from here on goes in.
    pub fn stop_taking_frames(&self) {
        self.recorder.stop_taking_frames();
    }

    /// Finishes the movie, once its stream has stopped. Returns whether there's a movie: after a
    /// failure, or if no frame ever arrived, the file is gone. (The `RECORDING` marker stays until
    /// the bundle is built: until then there's no journal for `scrawl recover` to go on.)
    pub fn finish(&self) -> bool {
        self.recorder.finish()
    }
}

impl Drop for SCRecordingOutput {
    /// A movie that never got to `finish` (its stream didn't start) leaves nothing behind.
    fn drop(&mut self) {
        let recording = lock(&self.recorder.recording);
        if !recording.closed {
            recording.movie.cancel();
            let _ = std::fs::remove_file(&self.recorder.path);
            let _ = std::fs::remove_file(self.recorder.path.with_file_name(RECORDING));
        }
    }
}

/// `SCStream`'s recording method, which the crate only has for macOS 15. Takes the stream `mut`,
/// as an output handler does.
pub trait SCStreamRecording {
    fn add_recording_output(&mut self, output: &SCRecordingOutput) -> Result<(), CaptureFailed>;
}

impl SCStreamRecording for SCStream {
    fn add_recording_output(&mut self, output: &SCRecordingOutput) -> Result<(), CaptureFailed> {
        let recorder = Arc::clone(&output.recorder);
        let queue = DispatchQueue::new("scrawl.recording", DispatchQoS::UserInteractive);
        self.add_output_handler_with_queue(
            move |sample: CMSampleBuffer, kind| {
                if matches!(kind, SCStreamOutputType::Screen) {
                    recorder.take(sample);
                }
            },
            SCStreamOutputType::Screen,
            Some(&queue),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureMode, ScreenCaptureSession, TargetInfo, capture_size};
    use crate::model::RgbaFrame;
    use screencapturekit::CVPixelBuffer;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    #[test]
    fn bgra_becomes_rgba_row_by_row_skipping_row_padding() {
        // Two rows of 2 pixels, in rows 12 bytes long.
        let bytes = [
            1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, //
            9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0,
        ];
        assert_eq!(
            bgra_to_rgba(&bytes, 2, 2, 12).unwrap(),
            [3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
    }

    #[test]
    fn the_last_row_needs_no_padding() {
        let bytes = [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8];
        assert_eq!(
            bgra_to_rgba(&bytes, 1, 2, 8).unwrap(),
            [3, 2, 1, 4, 7, 6, 5, 8]
        );
    }

    #[test]
    fn a_short_or_empty_buffer_is_no_frame() {
        assert!(bgra_to_rgba(&[1, 2, 3, 4], 1, 2, 4).is_none());
        assert!(bgra_to_rgba(&[], 0, 0, 0).is_none());
        assert!(bgra_to_rgba(&[0; 16], 2, 1, 4).is_none());
    }

    fn movie_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrawl-sck-{}", crate::make_bundle_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn output_at(path: &Path, offset_ms: &Arc<AtomicU64>) -> SCRecordingOutput {
        let config = SCRecordingOutputConfiguration::new()
            .unwrap()
            .with_output_url(path)
            .unwrap()
            .with_size(64, 64)
            .with_clock(Instant::now(), Arc::clone(offset_ms));
        SCRecordingOutput::new(&config).expect("the writer starts")
    }

    /// A gray frame, as `SCStream` would send it, `ms` after an arbitrary start.
    fn frame_at(ms: i64) -> CMSampleBuffer {
        let pixels = CVPixelBuffer::create(64, 64, u32::from_be_bytes(*b"BGRA")).unwrap();
        CMSampleBuffer::create_for_image_buffer(
            &pixels,
            from_nanos(1_000_000_000_000 + ms * 1_000_000),
            CMTime::INVALID,
        )
        .unwrap()
    }

    /// The length a player sees, from `mvhd`. The last one: closing a fragmented movie leaves the
    /// first fragment's header behind inside `mdat`, with that fragment's length.
    fn movie_ms(path: &Path) -> u64 {
        let bytes = std::fs::read(path).unwrap();
        let at = bytes.windows(4).rposition(|b| b == b"mvhd").unwrap();
        let word = |o: usize| u32::from_be_bytes(bytes[at + o..at + o + 4].try_into().unwrap());
        u64::from(word(20)) * 1_000 / u64::from(word(16))
    }

    #[test]
    fn a_still_screen_s_movie_runs_to_the_stop_time() {
        let path = movie_path("still.mp4");
        let offset = Arc::new(AtomicU64::new(0));
        let output = output_at(&path, &offset);
        let recorder = &output.recorder;
        recorder.add_frame(frame_at(0));
        recorder.add_frame(frame_at(17));
        // Nothing changes for 3 s; then the capture stops.
        lock(&recorder.recording).end_ns = Some(1_000_000_000_000 + 3_000_000_000);
        assert!(output.finish());
        let ms = movie_ms(&path);
        assert!((2_950..=3_050).contains(&ms), "{ms} ms");
    }

    #[test]
    fn repeats_on_a_still_screen_do_not_lengthen_the_movie() {
        let path = movie_path("repeats.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        let recorder = &output.recorder;
        recorder.add_frame(frame_at(0));
        // Only the repeats, so nothing shows how long a frame lasts but their gaps.
        for s in 1..=3 {
            assert!(lock(&recorder.recording).repeat_at(1_000_000_000_000 + s * 2_000_000_000));
        }
        lock(&recorder.recording).end_ns = Some(1_000_000_000_000 + 6_010_000_000);
        assert!(output.finish());
        let ms = movie_ms(&path);
        assert!((5_950..=6_050).contains(&ms), "{ms} ms");
    }

    #[test]
    fn frames_past_the_stop_time_stay_out() {
        let path = movie_path("late.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        let recorder = &output.recorder;
        recorder.add_frame(frame_at(0));
        lock(&recorder.recording).end_ns = Some(1_000_000_000_000 + 1_000_000_000);
        recorder.add_frame(frame_at(1_000));
        recorder.add_frame(frame_at(1_500));
        assert_eq!(lock(&recorder.recording).frames, 1);
        assert!(output.finish());
        let ms = movie_ms(&path);
        assert!((950..=1_050).contains(&ms), "{ms} ms");
    }

    #[test]
    fn a_frame_behind_a_repeat_goes_in_after_it() {
        let path = movie_path("repeat.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        let recorder = &output.recorder;
        recorder.add_frame(frame_at(0));
        let mut recording = lock(&recorder.recording);
        assert!(recording.repeat_at(1_000_000_000_000 + 500_000_000));
        drop(recording);
        // Composited before the repeat, delivered after it.
        recorder.add_frame(frame_at(400));
        recording = lock(&recorder.recording);
        assert_eq!((recording.frames, recording.repeats), (2, 1));
        assert_eq!(recording.movie.failure(), None);
        drop(recording);
        lock(&recorder.recording).end_ns = Some(1_000_000_000_000 + 2_000_000_000);
        assert!(output.finish());
    }

    #[test]
    fn a_dropped_frame_goes_in_at_the_next_tick() {
        let path = movie_path("behind.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        let recorder = &output.recorder;
        recorder.add_frame(frame_at(0));
        output.tick();
        assert_eq!(lock(&recorder.recording).repeats, 0, "not due yet");
        lock(&recorder.recording).behind = true;
        let deadline = Instant::now() + Duration::from_secs(1);
        while lock(&recorder.recording).repeats == 0 && Instant::now() < deadline {
            output.tick();
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(lock(&recorder.recording).repeats, 1);
        assert!(!lock(&recorder.recording).behind);
        assert!(output.finish());
    }

    #[test]
    fn a_finished_movie_keeps_its_marker_until_the_bundle_is_built() {
        let path = movie_path("kept.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        output.recorder.add_frame(frame_at(0));
        lock(&output.recorder.recording).end_ns = Some(1_000_000_000_000 + 100_000_000);
        assert!(output.finish());
        drop(output);
        assert!(path.exists() && path.with_file_name(RECORDING).exists());
    }

    #[test]
    fn a_movie_with_no_frames_leaves_no_file() {
        let path = movie_path("empty.mp4");
        let output = output_at(&path, &Arc::new(AtomicU64::new(0)));
        assert!(!output.finish());
        assert!(!path.exists() && !path.with_file_name(RECORDING).exists());
    }

    #[test]
    fn a_movie_that_never_finished_leaves_no_file() {
        let path = movie_path("dropped.mp4");
        drop(output_at(&path, &Arc::new(AtomicU64::new(0))));
        assert!(!path.exists() && !path.with_file_name(RECORDING).exists());
    }

    #[test]
    fn a_path_that_cannot_be_written_is_no_recording() {
        let config = SCRecordingOutputConfiguration::new()
            .unwrap()
            .with_output_url(Path::new("/nonexistent/scrawl/recording.mp4"))
            .unwrap()
            .with_size(64, 64)
            .with_clock(Instant::now(), Arc::default());
        assert!(SCRecordingOutput::new(&config).is_none());
    }

    #[test]
    fn the_offset_is_the_first_frame_s_time_on_the_capture_clock() {
        let path = movie_path("offset.mp4");
        let offset = Arc::new(AtomicU64::new(0));
        let started_at = Instant::now();
        let config = SCRecordingOutputConfiguration::new()
            .unwrap()
            .with_output_url(&path)
            .unwrap()
            .with_size(64, 64)
            .with_clock(started_at, Arc::clone(&offset));
        let output = SCRecordingOutput::new(&config).unwrap();
        // A frame that was on screen 40 ms ago, delivered some way into the capture.
        thread::sleep(Duration::from_millis(100));
        let pixels = CVPixelBuffer::create(64, 64, u32::from_be_bytes(*b"BGRA")).unwrap();
        let shown_ns = nanos(host_now()) - 40_000_000;
        let frame =
            CMSampleBuffer::create_for_image_buffer(&pixels, from_nanos(shown_ns), CMTime::INVALID)
                .unwrap();
        let expected = started_at.elapsed().as_millis() as u64 - 40;
        output.recorder.add_frame(frame);
        assert!(offset.load(Ordering::Acquire).abs_diff(expected) <= 3);
        assert!(output.finish());
    }

    // The rest record the real screen and show a window, so they only run when asked:
    //
    //   swiftc -O scripts/test-panel.swift -o ~/Library/Caches/scrawl-port/test-panel
    //   cargo test -j 4 sck_compat -- --ignored --test-threads=1 --nocapture

    struct Panel(Child);

    impl Panel {
        /// Shows `scripts/test-panel.swift`; returns it with its window's id once it's up.
        fn show(args: &[&str]) -> (Self, u32, BufReader<std::process::ChildStdout>) {
            let tool = dirs::home_dir()
                .unwrap()
                .join("Library/Caches/scrawl-port/test-panel");
            let mut child = Command::new(tool)
                .args(args)
                .stdout(Stdio::piped())
                .spawn()
                .expect("build scripts/test-panel.swift first");
            let mut out = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            out.read_line(&mut line).unwrap();
            let id = serde_json::from_str::<serde_json::Value>(&line).unwrap()["window"]
                .as_u64()
                .unwrap();
            thread::sleep(Duration::from_millis(150));
            (Self(child), id as u32, out)
        }
    }

    impl Drop for Panel {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn region(rect: [f64; 4]) -> TargetInfo {
        TargetInfo {
            mode: CaptureMode::Region,
            app: None,
            bundle_id: None,
            window_title: None,
            window_id: None,
            display_id: crate::capture::main_display_id(),
            frame: crate::screens::with_scale(rect, 2.0),
        }
    }

    fn rect_arg(rect: [f64; 4]) -> String {
        format!("{},{},{},{}", rect[0], rect[1], rect[2], rect[3])
    }

    fn pixel(frame: &RgbaFrame, x: f64, y: f64) -> [u8; 3] {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let at = ((y * h as f64) as usize).min(h - 1) * w + ((x * w as f64) as usize).min(w - 1);
        let p = &frame.rgba[at * 4..at * 4 + 3];
        [p[0], p[1], p[2]]
    }

    fn reddish(p: [u8; 3]) -> bool {
        p[0] > 180 && p[1] < 90 && p[2] < 90
    }

    fn bluish(p: [u8; 3]) -> bool {
        p[0] < 90 && p[1] < 90 && p[2] > 180
    }

    /// Where a row of `frame` turns from red to blue, as a share of its width.
    fn red_blue_edge(frame: &RgbaFrame) -> f64 {
        (0..1_000)
            .map(|i| f64::from(i) / 1_000.0)
            .find(|&x| bluish(pixel(frame, x, 0.5)))
            .unwrap_or(1.0)
    }

    /// `rect` recorded for a moment; movie and candidates both show the whole region, scaled to
    /// the size `sck_config` asked for rather than cropped to it.
    fn check_region(rect: [f64; 4]) {
        let (_panel, _, _out) = Panel::show(&["--rect", &rect_arg(rect), "--mode", "split"]);
        let target = region(rect);
        let path = movie_path("region.mp4");
        let session = ScreenCaptureSession::start(target.clone(), Some(path), Arc::default());
        thread::sleep(Duration::from_millis(1_500));
        let recording = session.stop();
        let movie = recording.video.expect("a movie");
        let decoded =
            crate::movie_frames_at(&movie, &[1_000], recording.video_offset_ms, 0).unwrap();
        let frame = &decoded[0];
        assert_eq!(
            (frame.width, frame.height),
            capture_size(&target, 1280.0),
            "movie size"
        );
        assert!(reddish(pixel(frame, 0.02, 0.5)) && reddish(pixel(frame, 0.45, 0.02)));
        assert!(bluish(pixel(frame, 0.98, 0.98)) && bluish(pixel(frame, 0.55, 0.5)));
        assert!((red_blue_edge(frame) - 0.5).abs() < 0.01, "movie's split");
        let candidate = recording.frames.last().unwrap();
        assert_eq!(
            (candidate.width, candidate.height),
            capture_size(&target, crate::capture::CANDIDATE_EDGE),
            "candidate size"
        );
        assert!(
            (red_blue_edge(candidate) - 0.5).abs() < 0.02,
            "candidate's split"
        );
    }

    #[test]
    #[ignore = "records the real screen"]
    fn a_large_region_is_scaled_not_cropped() {
        check_region([300.0, 200.0, 800.0, 500.0]);
    }

    #[test]
    #[ignore = "records the real screen"]
    fn a_small_region_records_at_native_size() {
        check_region([300.0, 200.0, 300.0, 200.0]);
    }

    #[test]
    #[ignore = "records the real screen"]
    fn with_no_movie_the_candidates_are_sharp_and_real() {
        let rect = [300.0, 200.0, 800.0, 500.0];
        let (_panel, _, _out) = Panel::show(&["--rect", &rect_arg(rect), "--mode", "split"]);
        let target = region(rect);
        let session = ScreenCaptureSession::start(target.clone(), None, Arc::default());
        thread::sleep(Duration::from_millis(1_000));
        let recording = session.stop();
        assert!(recording.video.is_none());
        let frame = recording.frames.last().unwrap();
        assert_eq!((frame.width, frame.height), capture_size(&target, 1280.0));
        assert!((red_blue_edge(frame) - 0.5).abs() < 0.01);
    }

    #[test]
    #[ignore = "records the real screen"]
    fn a_path_that_cannot_be_written_falls_back_to_sharp_candidates() {
        let rect = [300.0, 200.0, 800.0, 500.0];
        let (_panel, _, _out) = Panel::show(&["--rect", &rect_arg(rect), "--mode", "split"]);
        let target = region(rect);
        let path = PathBuf::from("/nonexistent/scrawl/recording.mp4");
        let session = ScreenCaptureSession::start(target.clone(), Some(path), Arc::default());
        thread::sleep(Duration::from_millis(1_000));
        let recording = session.stop();
        assert!(recording.video.is_none());
        assert!((red_blue_edge(recording.frames.last().unwrap()) - 0.5).abs() < 0.01);
    }

    /// A keyframe decoded from the movie has the candidate's colors, so nothing shifts between
    /// the frames of a bundle and the picture Scrawl chose them from.
    #[test]
    #[ignore = "records the real screen"]
    fn the_movie_keeps_the_candidates_colors() {
        let rect = [300.0, 200.0, 160.0, 160.0];
        let patches = [
            [128, 128, 128],
            [20, 20, 20],
            [235, 235, 235],
            [40, 44, 52],
            [200, 120, 60],
            [30, 160, 220],
            [90, 200, 90],
            [220, 60, 80],
        ];
        for patch in patches {
            let color = format!("{},{},{}", patch[0], patch[1], patch[2]);
            let (_panel, _, _out) = Panel::show(&[
                "--rect",
                &rect_arg(rect),
                "--mode",
                "solid",
                "--color",
                &color,
            ]);
            let path = movie_path("color.mp4");
            let session = ScreenCaptureSession::start(region(rect), Some(path), Arc::default());
            thread::sleep(Duration::from_millis(500));
            let recording = session.stop();
            let movie = recording.video.expect("a movie");
            let candidate = pixel(recording.frames.last().unwrap(), 0.5, 0.5);
            let frames = crate::movie_frames_at(&movie, &[300], recording.video_offset_ms, 0);
            let decoded = pixel(&frames.unwrap()[0], 0.5, 0.5);
            println!("{patch:?}: candidate {candidate:?}, movie {decoded:?}");
            let apart = (0..3).map(|i| candidate[i].abs_diff(decoded[i])).max();
            assert!(
                apart.unwrap() <= 2,
                "{patch:?}: {candidate:?} vs {decoded:?}"
            );
        }
    }

    /// The disk filling up is a failed movie, not a stuck capture: the file goes, the capture ends
    /// on time and the candidates remain.
    #[test]
    #[ignore = "records the real screen"]
    fn a_full_disk_ends_the_movie_and_keeps_the_candidates() {
        let dir = movie_path("disk").with_file_name("volume");
        let image = dir.with_extension("dmg");
        let hdiutil = |args: &[&std::ffi::OsStr]| {
            assert!(
                Command::new("hdiutil")
                    .args(args)
                    .stdout(Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        hdiutil(&[
            "create".as_ref(),
            "-size".as_ref(),
            "2m".as_ref(),
            "-fs".as_ref(),
            "HFS+".as_ref(),
            "-volname".as_ref(),
            "scrawl-full".as_ref(),
            image.as_os_str(),
        ]);
        hdiutil(&[
            "attach".as_ref(),
            "-nobrowse".as_ref(),
            "-mountpoint".as_ref(),
            dir.as_os_str(),
            image.as_os_str(),
        ]);
        let rect = [300.0, 200.0, 800.0, 500.0];
        let (_panel, _, _out) = Panel::show(&["--rect", &rect_arg(rect), "--mode", "noise"]);
        let path = dir.join("recording.mp4");
        let session = ScreenCaptureSession::start(region(rect), Some(path.clone()), Arc::default());
        thread::sleep(Duration::from_millis(4_000));
        let stopping = Instant::now();
        let recording = session.stop();
        let stop_ms = stopping.elapsed().as_millis();
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        println!("stopped in {stop_ms} ms; left on the volume: {files:?}");
        hdiutil(&["detach".as_ref(), dir.as_os_str()]);
        assert!(recording.video.is_none());
        assert!(!recording.frames.is_empty());
        assert!(stop_ms < 2_000, "{stop_ms} ms");
        assert!(!files.iter().any(|name| *name == "recording.mp4"));
    }

    /// A hidden window leaves both streams: the movie and the candidates.
    #[test]
    #[ignore = "records the real screen"]
    fn a_filter_change_reaches_the_movie_and_the_candidates() {
        let rect = [300.0, 200.0, 400.0, 300.0];
        let (_panel, window, _out) = Panel::show(&[
            "--rect",
            &rect_arg(rect),
            "--mode",
            "solid",
            "--color",
            "255,0,255",
        ]);
        let magenta = |p: [u8; 3]| p[0] > 180 && p[1] < 90 && p[2] > 180;
        let hidden = Arc::new(Mutex::new(Vec::new()));
        let path = movie_path("filter.mp4");
        let session = ScreenCaptureSession::start(region(rect), Some(path), Arc::clone(&hidden));
        thread::sleep(Duration::from_millis(1_500));
        let hidden_at = session.elapsed_ms();
        lock(&hidden).push(window);
        thread::sleep(Duration::from_millis(1_500));
        let recording = session.stop();
        let movie = recording.video.expect("a movie");
        let offset = recording.video_offset_ms;
        let at = |t: u64| {
            let frames = crate::movie_frames_at(&movie, &[t], offset, 0).unwrap();
            pixel(&frames[0], 0.5, 0.5)
        };
        assert!(magenta(at(hidden_at - 300)), "shown before");
        assert!(!magenta(at(hidden_at + 700)), "hidden in the movie");
        let before = recording.frames.iter().filter(|f| f.t_ms < hidden_at - 100);
        let after = recording.frames.iter().filter(|f| f.t_ms > hidden_at + 300);
        assert!(before.clone().count() > 0 && before.clone().all(|f| magenta(pixel(f, 0.5, 0.5))));
        assert!(after.clone().count() > 0);
        assert!(
            after.clone().all(|f| !magenta(pixel(f, 0.5, 0.5))),
            "hidden in the candidates"
        );
    }

    /// `CLOCK_UPTIME_RAW`, which is the host clock and the panel's.
    fn uptime_ns() -> u64 {
        nanos(host_now()) as u64
    }

    /// Flips a panel at known times around a recording. Prints the flips (on the capture clock),
    /// the offset and the movie, for finding each flip in the decoded video.
    #[test]
    #[ignore = "records the real screen"]
    fn the_offset_puts_the_screen_at_its_own_time() {
        let rect = [300.0, 200.0, 200.0, 200.0];
        let soon = uptime_ns() + 1_500_000_000;
        let flips: Vec<u64> = [0, 700, 1_450, 2_210, 3_033, 3_800]
            .iter()
            .map(|ms: &u64| soon + ms * 1_000_000)
            .collect();
        let list = flips
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let (_panel, _, _out) = Panel::show(&[
            "--rect",
            &rect_arg(rect),
            "--mode",
            "flip",
            "--flip-at",
            &list,
        ]);
        let t0 = uptime_ns();
        let path = movie_path("flips.mp4");
        let session = ScreenCaptureSession::start(region(rect), Some(path), Arc::default());
        thread::sleep(Duration::from_millis(6_500));
        let recording = session.stop();
        let movie = recording.video.expect("a movie");
        let on_capture_clock: Vec<f64> = flips
            .iter()
            .map(|at| (*at as f64 - t0 as f64) / 1_000_000.0)
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "flipsMs": on_capture_clock,
                "offsetMs": recording.video_offset_ms,
                "durationMs": recording.duration_ms,
                "movie": movie,
            })
        );
    }

    /// An urgent candidate taken right after the screen changed shows the change.
    #[test]
    #[ignore = "records the real screen"]
    fn an_urgent_frame_shows_the_screen_now() {
        let rect = [300.0, 200.0, 200.0, 200.0];
        for delay_ms in [20_u64, 40, 60, 100] {
            let soon = uptime_ns() + 1_200_000_000;
            let flips: Vec<u64> = (0..8).map(|i| soon + i * 400_000_000).collect();
            let list = flips
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let (_panel, _, _out) = Panel::show(&[
                "--rect",
                &rect_arg(rect),
                "--mode",
                "flip",
                "--flip-at",
                &list,
            ]);
            let session = ScreenCaptureSession::start(region(rect), None, Arc::default());
            let mut asked = Vec::new();
            for (index, at) in flips.iter().enumerate() {
                while uptime_ns() < at + delay_ms * 1_000_000 {
                    thread::sleep(Duration::from_millis(1));
                }
                asked.push((session.elapsed_ms(), index % 2 == 0));
                session.request_immediate_frame();
                thread::sleep(Duration::from_millis(100));
            }
            let recording = session.stop();
            let (mut right, mut lag_ms) = (0, 0);
            for (t_ms, white) in &asked {
                let frame = recording.frames.iter().find(|f| f.t_ms >= *t_ms).unwrap();
                lag_ms = lag_ms.max(frame.t_ms - t_ms);
                right += usize::from((pixel(frame, 0.5, 0.5)[0] > 128) == *white);
            }
            println!(
                "{delay_ms} ms after the change: {right} of {} current",
                asked.len()
            );
            assert!(lag_ms < 60, "served in {lag_ms} ms");
            if delay_ms >= 60 {
                assert_eq!(right, asked.len(), "{delay_ms} ms");
            }
        }
    }
}
