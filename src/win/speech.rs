//! On-device transcription on Windows and macOS, with measured word timings: Parakeet through
//! `transcribe.cpp`, the engine Hex uses. It loads a model Hex has already installed
//! (`%APPDATA%\hex\models`, else the older `%APPDATA%\voice-control\models`; on a Mac the same
//! folders under `~/Library/Application Support`), else Scrawl's own copy of Hex's Parakeet Unified
//! English (`%LOCALAPPDATA%\Scrawl\models`, `~/Library/Application Support/scrawl/models`), which
//! `install` downloads once in the background and checks against its SHA-256. Like the Hex client,
//! it transcribes each speech island on its own; unlike it, every word keeps the time the model
//! heard it (`estimated: false`), so spoken cues ("look here") can cut keyframes.
//!
//! Windows runs it in process. A Mac runs it in a child, `scrawl parakeet FILE`
//! (`helper::transcribe`), as it does `scrawl-helper`: a crash can't reach the app, and the
//! model's memory goes when the child exits.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use transcribe_cpp::{Model, RunOptions, TimestampKind};

use crate::helper::SpeechModelLine;
use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

const KHZ: u64 = 16;
/// Silence kept around each clip, so first and last words aren't clipped.
const CLIP_PAD_MS: u64 = 200;
/// A clip's first word is taken to start with the clip's sound when the model puts it this close.
const FIRST_WORD_SLACK_MS: u64 = 500;

/// The model Scrawl downloads when Hex hasn't installed one: the same file, revision and
/// checksum as Hex's `PARAKEET_UNIFIED_ENGLISH_ARTIFACT`.
pub struct Artifact {
    pub filename: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

pub const PARAKEET: Artifact = Artifact {
    filename: "parakeet-unified-en-0.6b-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/parakeet-unified-en-0.6b-gguf/resolve/7e948f21b7bdbac698d3318db9d350f1096f3b6c/parakeet-unified-en-0.6b-Q8_0.gguf",
    bytes: 731_357_568,
    sha256: "4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795",
};

/// Where Scrawl keeps the model it downloaded (`SCRAWL_MODELS_DIR` for tests).
pub fn own_models_dir() -> PathBuf {
    std::env::var_os("SCRAWL_MODELS_DIR")
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(default_models_dir)
}

#[cfg(target_os = "windows")]
fn default_models_dir() -> PathBuf {
    crate::win::local_dir().join("models")
}

#[cfg(not(target_os = "windows"))]
fn default_models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Application Support/scrawl/models")
}

/// What runs `scrawl parakeet`: this program. A test binary sits in `target/*/deps`, under `scrawl`.
#[cfg(target_os = "macos")]
pub fn child_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Could not locate scrawl")?;
    Ok(if cfg!(test) {
        exe.with_file_name("../scrawl")
    } else {
        exe
    })
}

/// Whether Parakeet transcribes this `appleLocale`: Automatic (`None`) and any English. The model
/// knows no other language, so those stay with Apple's recognizer through `scrawl-helper`, which
/// needs the Speech Recognition permission and Dictation.
#[cfg(target_os = "macos")]
pub fn understands(locale: Option<&str>) -> bool {
    locale.is_none_or(|id| {
        id.split(['-', '_'])
            .next()
            .is_some_and(|language| language.eq_ignore_ascii_case("en"))
    })
}

fn receipt_path(model: &Path) -> PathBuf {
    model.with_file_name(format!(
        "{}.verified.json",
        model.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// `.gguf` files in `dir` that have a `.verified.json` receipt beside them.
fn verified_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "gguf") && receipt_path(path).is_file()
        })
        .collect()
}

/// Hex's installed models (verified downloads), Hex's chosen one first, then Parakeet ones;
/// then the one Scrawl downloaded.
pub fn installed_models() -> Vec<PathBuf> {
    let mut models = hex_models();
    models.extend(verified_in(&own_models_dir()));
    models
}

/// Each of Hex's folders in `hex::support_dirs` order (`hex`, then `voice-control`), each ranked
/// by that folder's own `settings.json`.
fn hex_models() -> Vec<PathBuf> {
    crate::hex::support_dirs()
        .iter()
        .flat_map(|dir| models_in(dir))
        .collect()
}

fn models_in(support: &Path) -> Vec<PathBuf> {
    let chosen = std::fs::read(support.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|settings| {
            settings["transcription"]["model"]
                .as_str()
                .map(str::to_owned)
        })
        .map(|id| id.replace('_', "-"));
    let mut models = verified_in(&support.join("models"));
    let rank = |path: &PathBuf| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let picked = chosen.as_deref().is_some_and(|id| name.starts_with(id));
        (!picked, !name.starts_with("parakeet"), name.into_owned())
    };
    models.sort_by_key(rank);
    models
}

/// How late the model reports a word, which its times are moved back by. Parakeet Unified English
/// is built for streaming and says a word 4 encoder frames (80 ms each) after it began: measured
/// against the waveform, a median of 320 ms over 43 separated words and 360 ms over 116 sentence
/// starts. Spoken cues only look 300 ms before a word, so they'd miss what they point at. Other
/// models are unmeasured.
fn word_lag_ms(model: &str) -> u64 {
    if model.starts_with("parakeet-unified") {
        320
    } else {
        0
    }
}

/// After a pause the model is later still, about 600 ms however much silence it's given, and a
/// pause is where "look" and "now" tend to be said. The clip's sound says when that word began.
fn start_at_the_sound(words: &mut [WordTiming], clip_start: u64) {
    if let Some(first) = words.first_mut()
        && first.t0.saturating_sub(clip_start) <= FIRST_WORD_SLACK_MS
    {
        first.t0 = first.t0.min(clip_start);
    }
}

/// The model's name for `TranscriptData.model`: its file name without the extension.
fn model_name(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The background download hasn't finished (or failed): the bundle is transcribed again once it
/// has. Asks this process's `speech_model` worker, so a Mac calls it before starting the child.
pub fn no_model() -> TranscriptData {
    let status = crate::speech_model::current();
    let why = match status.state {
        crate::speech_model::State::Failed => "failed",
        _ => "downloading",
    };
    TranscriptData {
        source: "offline".into(),
        missing: Some(crate::speech_model::missing(
            why,
            "en",
            status.error.as_deref(),
        )),
        ..Default::default()
    }
}

pub fn transcribe(audio: &Path) -> Result<TranscriptData> {
    let Some(path) = installed_models().into_iter().next() else {
        return Ok(no_model());
    };
    let wav =
        std::fs::read(audio).with_context(|| format!("Could not read {}", audio.display()))?;
    let samples = crate::audio::decode_wav_16k(&wav)?;
    let duration_ms = samples.len() as u64 / KHZ;
    let mut clips = crate::hex::plan_clips(&crate::audio::speech_islands(&samples), duration_ms);
    if clips.is_empty() && duration_ms >= 480 {
        clips.push((0, duration_ms));
    }
    let name = model_name(&path);
    let lag_ms = word_lag_ms(&name);
    let mut transcript = TranscriptData {
        source: format!("parakeet:{name}"),
        model: Some(name),
        estimated: false,
        ..Default::default()
    };
    if clips.is_empty() {
        return Ok(transcript);
    }

    let started = Instant::now();
    init_backends()?;
    let model = Model::load(&path).with_context(|| format!("Could not load {}", path.display()))?;
    let load_ms = started.elapsed().as_millis();
    let mut session = model.session()?;
    let clip_count = clips.len();
    let options = RunOptions {
        timestamps: TimestampKind::Word,
        language: Some("en".into()),
        ..Default::default()
    };
    let mut texts = Vec::new();
    for (start, end) in clips {
        let from_ms = start.saturating_sub(CLIP_PAD_MS);
        let from = (from_ms * KHZ) as usize;
        let to = (((end + CLIP_PAD_MS) * KHZ) as usize).min(samples.len());
        if from >= to {
            continue;
        }
        let result = session.run(&samples[from..to], &options)?;
        let text = result.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let at = |ms: i64| from_ms + (ms.max(0) as u64).saturating_sub(lag_ms);
        let mut words: Vec<WordTiming> = result
            .words
            .iter()
            .filter(|word| !word.text.trim().is_empty())
            .map(|word| WordTiming {
                t0: at(word.t0_ms),
                t1: at(word.t1_ms.max(word.t0_ms)),
                text: word.text.trim().to_owned(),
            })
            .collect();
        start_at_the_sound(&mut words, start);
        if words.is_empty() {
            // No word times from this model: spread the text over the clip, and say so.
            let (segments, words) = crate::hex::spread_words(&text, start, end);
            transcript.estimated = true;
            transcript.segments.extend(segments);
            transcript.words.extend(words);
        } else {
            let mut segments: Vec<TranscriptSegment> = result
                .segments
                .iter()
                .filter(|s| !s.text.trim().is_empty())
                .map(|s| TranscriptSegment {
                    t0: at(s.t0_ms),
                    t1: at(s.t1_ms.max(s.t0_ms)),
                    text: s.text.trim().to_owned(),
                })
                .collect();
            if let Some(first) = segments.first_mut() {
                first.t0 = first.t0.min(words[0].t0);
            }
            transcript.segments.extend(if segments.is_empty() {
                vec![TranscriptSegment {
                    t0: words[0].t0,
                    t1: words[words.len() - 1].t1,
                    text: text.clone(),
                }]
            } else {
                segments
            });
            transcript.words.extend(words);
        }
        texts.push(text);
    }
    transcript.text = texts.join(" ");
    crate::capture::scrawl_log(format!(
        "Parakeet: {duration_ms} ms of audio, {clip_count} clip(s), model {load_ms} ms, {} ms in all",
        started.elapsed().as_millis()
    ));
    Ok(transcript)
}

/// Runs the model once on two seconds of tone (`scrawl parakeet --warm`). The first run under an
/// app's bundle id has Metal compile ggml's shaders, about 20 s on an M1, and only then keeps them
/// on disk, so Scrawl does it in the background rather than leaving it to the first capture. After
/// that it costs what a short clip does, and leaves the model in the page cache.
#[cfg(target_os = "macos")]
pub fn warm() -> Result<()> {
    let Some(path) = installed_models().into_iter().next() else {
        return Ok(());
    };
    init_backends()?;
    let model = Model::load(&path).with_context(|| format!("Could not load {}", path.display()))?;
    let pcm: Vec<f32> = (0..2 * 1_000 * KHZ)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.1)
        .collect();
    model.session()?.run(&pcm, &RunOptions::default())?;
    Ok(())
}

/// Loads ggml's CPU backend DLL for this CPU (beside `transcribe.dll`), once per process: the
/// registry it fills isn't safe to change while another thread loads a model.
fn init_backends() -> Result<()> {
    static INIT: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        transcribe_cpp::disable_logging();
        transcribe_cpp::init_backends_default().map_err(|error| format!("{error:#}"))
    })
    .clone()
    .map_err(|error| anyhow::anyhow!("transcribe.cpp backends: {error}"))
}

/// Makes sure a model is on disk: `installed` when Hex's or Scrawl's own is there, else downloads
/// `PARAKEET` into `own_models_dir()`, reporting progress, and keeps it only if its SHA-256
/// matches. The same lines as `scrawl-helper speech-model --install`. A download stops as soon as
/// `stop` says a newer check superseded it, and the next try resumes it.
pub fn install(stop: &dyn Fn() -> bool, line: impl FnMut(SpeechModelLine)) -> Result<()> {
    let installed = !installed_models().is_empty();
    install_into(&PARAKEET, &own_models_dir(), installed, stop, line)
}

fn install_into(
    artifact: &Artifact,
    dir: &Path,
    installed: bool,
    stop: &dyn Fn() -> bool,
    mut line: impl FnMut(SpeechModelLine),
) -> Result<()> {
    let report = |status: &str, progress: Option<f64>, error: Option<String>| SpeechModelLine {
        status: status.into(),
        engine: Some("Parakeet".into()),
        locale: Some("en".into()),
        locales: None,
        progress,
        error,
    };
    if !installed {
        let mut progress = |done: f64| line(report("downloading", Some(done), None));
        if let Err(error) = download(artifact, dir, stop, &mut progress) {
            line(report("failed", None, Some(format!("{error:#}"))));
            return Ok(());
        }
    }
    line(report("installed", Some(1.0), None));
    Ok(())
}

/// Downloads `artifact` into `dir` (resuming a `.partial` file), checks it, and writes the
/// `.verified.json` receipt that `installed_models` looks for. A file that fails the check is
/// deleted, so the next try starts over.
pub fn download(
    artifact: &Artifact,
    dir: &Path,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(f64),
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    let model = dir.join(artifact.filename);
    if !model.is_file() {
        let partial = dir.join(format!("{}.partial", artifact.filename));
        if std::fs::metadata(&partial).map_or(0, |m| m.len()) < artifact.bytes {
            fetch(artifact.url, &partial, artifact.bytes, stop, progress)?;
        }
        std::fs::rename(&partial, &model)?;
    }
    if let Err(error) = verify(&model, artifact) {
        let _ = std::fs::remove_file(&model);
        return Err(error);
    }
    let receipt = serde_json::json!({
        "sha256": artifact.sha256,
        "bytes": artifact.bytes,
        "url": artifact.url,
    });
    std::fs::write(receipt_path(&model), receipt.to_string())?;
    Ok(model)
}

fn verify(path: &Path, artifact: &Artifact) -> Result<()> {
    use sha2::{Digest, Sha256};
    let len = std::fs::metadata(path)?.len();
    if len != artifact.bytes {
        bail!(
            "{} is {len} bytes, expected {}",
            artifact.filename,
            artifact.bytes
        );
    }
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hasher)?;
    let sha256 = format!("{:x}", hasher.finalize());
    if sha256 != artifact.sha256 {
        bail!("{} failed its checksum ({sha256})", artifact.filename);
    }
    Ok(())
}

/// Windows' own `curl.exe` (in System32 since Windows 10 1803), without a console window.
#[cfg(target_os = "windows")]
fn curl() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let curl = std::env::var_os("SystemRoot")
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join(r"System32\curl.exe");
    let mut command = std::process::Command::new(curl);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// The Mac's own curl, whatever is first on PATH.
#[cfg(not(target_os = "windows"))]
fn curl() -> std::process::Command {
    std::process::Command::new("/usr/bin/curl")
}

fn fetch(
    url: &str,
    partial: &Path,
    total: u64,
    stop: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(f64),
) -> Result<()> {
    use std::io::Read;
    let mut child = curl()
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--continue-at",
            "-",
            "--output",
        ])
        .arg(partial)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Could not run curl")?;
    loop {
        if stop() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the download was stopped");
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                progress(1.0);
                return Ok(());
            }
            let mut error = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut error);
            }
            bail!("the download failed ({status}): {}", error.trim());
        }
        let have = std::fs::metadata(partial).map_or(0, |m| m.len());
        progress((have as f64 / total as f64).min(0.99));
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A five-byte artifact whose checksum is `sha256("hello")`.
    const HELLO: Artifact = Artifact {
        filename: "fixture.gguf",
        url: "http://127.0.0.1:9/never-fetched",
        bytes: 5,
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrawl-speech-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_finished_download_that_matches_gets_a_receipt_and_counts_as_installed() {
        let dir = temp_dir("good");
        std::fs::write(dir.join("fixture.gguf.partial"), b"hello").unwrap();
        let model = download(&HELLO, &dir, &|| false, &mut |_| {}).unwrap();
        assert_eq!(model, dir.join("fixture.gguf"));
        assert!(receipt_path(&model).is_file());
        assert_eq!(verified_in(&dir), vec![model]);
    }

    #[test]
    fn a_download_that_fails_its_checksum_is_deleted_and_not_installed() {
        let dir = temp_dir("bad");
        std::fs::write(dir.join("fixture.gguf.partial"), b"hellp").unwrap();
        let error = download(&HELLO, &dir, &|| false, &mut |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("checksum"));
        assert!(!dir.join("fixture.gguf").exists());
        assert!(verified_in(&dir).is_empty());
    }

    /// Serves `body` from 127.0.0.1, honouring curl's `Range` when it resumes. The first answer can
    /// stop after `first_stops_at` bytes and hang, as a connection does when it dies.
    #[cfg(target_os = "macos")]
    fn serve(body: &'static [u8], first_stops_at: Option<usize>) -> &'static str {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/fixture.gguf", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut stops_at = first_stops_at;
            for mut stream in listener.incoming().flatten() {
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).to_lowercase();
                let from: usize = request
                    .split("range: bytes=")
                    .nth(1)
                    .and_then(|rest| rest.split('-').next()?.parse().ok())
                    .unwrap_or(0);
                let head = if from > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\n",
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    "HTTP/1.1 200 OK\r\n".to_owned()
                };
                let length = body.len() - from;
                let _ = write!(stream, "{head}Content-Length: {length}\r\n\r\n");
                let _ = stream.write_all(&body[from..stops_at.unwrap_or(body.len())]);
                let _ = stream.flush();
                if stops_at.take().is_some() {
                    let _ = stream.read(&mut byte);
                }
            }
        });
        Box::leak(url.into_boxed_str())
    }

    #[test]
    fn only_the_model_measured_late_is_moved() {
        assert_eq!(word_lag_ms("parakeet-unified-en-0.6b-Q8_0"), 320);
        assert_eq!(word_lag_ms("parakeet-tdt-0.6b-v3-Q8_0"), 0);
        assert_eq!(word_lag_ms("whisper-large-v3"), 0);
    }

    #[test]
    fn a_clips_first_word_starts_with_its_sound_unless_it_came_much_later() {
        let word = |t0| WordTiming {
            t0,
            t1: t0 + 80,
            text: "look".into(),
        };
        let mut words = [word(8_680), word(8_760)];
        start_at_the_sound(&mut words, 8_400);
        assert_eq!((words[0].t0, words[0].t1), (8_400, 8_760));
        assert_eq!(words[1].t0, 8_760);

        // Noise or a breath opened the clip, and the word really is later.
        let mut after_noise = [word(9_000)];
        start_at_the_sound(&mut after_noise, 8_400);
        assert_eq!(after_noise[0].t0, 9_000);

        start_at_the_sound(&mut [], 8_400);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_fresh_install_downloads_the_model_and_says_how_its_going() {
        let dir = temp_dir("fresh");
        let artifact = Artifact {
            url: serve(b"hello", None),
            ..HELLO
        };
        let mut lines = Vec::new();
        install_into(&artifact, &dir, false, &|| false, |line| lines.push(line)).unwrap();
        let (last, earlier) = lines.split_last().unwrap();
        assert_eq!(last.status, "installed");
        assert_eq!(last.progress, Some(1.0));
        assert_eq!(last.engine.as_deref(), Some("Parakeet"));
        assert_eq!(last.locale.as_deref(), Some("en"));
        assert!(!earlier.is_empty() && earlier.iter().all(|l| l.status == "downloading"));
        assert_eq!(verified_in(&dir), vec![dir.join("fixture.gguf")]);
        assert_eq!(std::fs::read(dir.join("fixture.gguf")).unwrap(), b"hello");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_download_that_cannot_start_reports_failed_and_leaves_no_model() {
        let dir = temp_dir("unreachable");
        let mut lines = Vec::new();
        install_into(&HELLO, &dir, false, &|| false, |line| lines.push(line)).unwrap();
        let last = lines.last().unwrap();
        assert_eq!(last.status, "failed");
        assert!(last.error.as_deref().unwrap().contains("download failed"));
        assert!(!dir.join("fixture.gguf").exists());
        assert!(verified_in(&dir).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_stopped_download_is_killed_and_the_next_try_resumes_it() {
        use sha2::{Digest, Sha256};
        let dir = temp_dir("resume");
        // Bigger than curl's write buffer, which is all that reaches the disk before it's killed.
        let body: &'static [u8] = Vec::leak(vec![7; 4 << 20]);
        let artifact = Artifact {
            url: serve(body, Some(2 << 20)),
            bytes: body.len() as u64,
            sha256: String::leak(format!("{:x}", Sha256::digest(body))),
            ..HELLO
        };
        let partial = dir.join("fixture.gguf.partial");
        let started = Instant::now();
        let stop = || std::fs::metadata(&partial).map_or(0, |m| m.len()) > 0;
        let error = download(&artifact, &dir, &stop, &mut |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("stopped"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let kept = std::fs::metadata(&partial).unwrap().len();
        assert!(kept > 0 && kept < artifact.bytes, "{kept}");
        assert!(verified_in(&dir).is_empty());

        let model = download(&artifact, &dir, &|| false, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(model).unwrap(), body);
        assert!(!partial.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_model_that_is_there_is_never_fetched() {
        let dir = temp_dir("present");
        let mut lines = Vec::new();
        install_into(&HELLO, &dir, true, &|| false, |line| lines.push(line)).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].status, "installed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn automatic_and_english_are_parakeets_and_nothing_else_is() {
        for id in [
            None,
            Some("en"),
            Some("en-GB"),
            Some("en_US"),
            Some("EN-au"),
        ] {
            assert!(understands(id), "{id:?}");
        }
        for id in ["da", "da-DK", "de-DE", "zh-TW", "eng", "ena-XX"] {
            assert!(!understands(Some(id)), "{id}");
        }
    }

    /// Downloads the real model (731 MB), loads it and runs it on two seconds of sound. CI's
    /// release job runs it, then again under Intel SDE as a CPU without AVX-512 (#58), with
    /// `SCRAWL_TEST_MODEL_DIR` keeping the download between the two.
    #[test]
    #[ignore]
    fn downloads_and_runs_parakeet() {
        let kept = std::env::var_os("SCRAWL_TEST_MODEL_DIR").map(PathBuf::from);
        let dir = kept.clone().unwrap_or_else(|| temp_dir("parakeet"));
        let fresh = !dir.join(PARAKEET.filename).is_file();
        let mut last = 0.0;
        let model = download(&PARAKEET, &dir, &|| false, &mut |done| last = done).unwrap();
        if fresh {
            assert_eq!(last, 1.0);
        }
        #[cfg(target_arch = "x86_64")]
        if std::env::var_os("SCRAWL_TEST_NO_AVX512").is_some() {
            // Under SDE: proves the emulated CPU is the one without AVX-512.
            assert!(!std::arch::is_x86_feature_detected!("avx512f"));
        }
        init_backends().unwrap();
        for device in transcribe_cpp::devices() {
            eprintln!("device: {} ({})", device.name, device.description);
        }
        let model = Model::load(&model).unwrap();
        let pcm: Vec<f32> = (0..32_000)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.1)
            .collect();
        model
            .session()
            .unwrap()
            .run(&pcm, &RunOptions::default())
            .unwrap();
        if kept.is_none() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
