//! Optional video re-encoding, delegated to ffmpeg.
//!
//! This is *lossy* and has nothing to do with the context-mixing codec: it
//! throws pixels and frames away on purpose. It exists because that is the only
//! thing that actually shrinks a video — a lossless pass over an already
//! encoded MP4 gains close to nothing.
//!
//! ffmpeg is looked up on PATH and never bundled; when it is missing the
//! feature is simply reported as unavailable. Arguments are passed as separate
//! argv entries, so no user-supplied string is ever interpreted by a shell.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

#[derive(Clone, Copy, PartialEq)]
pub enum Codec {
    H264,
    H265,
    Vp9,
    Av1,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Codec> {
        match s {
            "h264" | "x264" | "avc" => Some(Codec::H264),
            "h265" | "x265" | "hevc" => Some(Codec::H265),
            "vp9" => Some(Codec::Vp9),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }

    fn encoder(self) -> &'static str {
        match self {
            Codec::H264 => "libx264",
            Codec::H265 => "libx265",
            Codec::Vp9 => "libvpx-vp9",
            Codec::Av1 => "libsvtav1",
        }
    }

    /// Container extension that every build of ffmpeg will accept for this
    /// encoder.
    pub fn ext(self) -> &'static str {
        match self {
            Codec::Vp9 => "webm",
            _ => "mp4",
        }
    }

    /// Highest CRF value the encoder accepts. libsvtav1 and libvpx-vp9 go to
    /// 63, x264/x265 to 51.
    fn max_crf(self) -> u8 {
        match self {
            Codec::Vp9 | Codec::Av1 => 63,
            _ => 51,
        }
    }
}

pub struct Settings {
    /// Target width in pixels; `None` derives it from the height.
    pub width: Option<u32>,
    /// Target height in pixels; `None` derives it from the width.
    pub height: Option<u32>,
    /// Target frame rate; `None` keeps the source rate.
    pub fps: Option<f32>,
    /// Constant-quality value. Lower is better quality and a bigger file.
    pub crf: u8,
    /// x264/x265 speed preset, ignored by the other encoders.
    pub preset: String,
    pub codec: Codec,
    /// Keep the audio track (re-encoded to AAC/Opus) or drop it.
    pub audio: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: None,
            height: None,
            fps: None,
            crf: 28,
            preset: "medium".to_string(),
            codec: Codec::H264,
            audio: true,
        }
    }
}

/// How many stderr lines are kept while ffmpeg runs. Only the last one is ever
/// reported, the others are context when it is uninformative.
const ERR_TAIL: usize = 8;

const PRESETS: [&str; 9] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

impl Settings {
    /// Clamp everything to a range ffmpeg accepts. Values arrive from a web
    /// form, so nothing here may be trusted.
    pub fn sanitise(&mut self) {
        // 16..7680 covers thumbnails up to 8K; odd sizes are handled by the
        // `-2` in the scale filter, which rounds to an even number.
        self.width = self.width.map(|w| w.clamp(16, 7680));
        self.height = self.height.map(|h| h.clamp(16, 7680));
        self.fps = self
            .fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| f.clamp(1.0, 240.0));
        self.crf = self.crf.min(self.codec.max_crf());
        if !PRESETS.contains(&self.preset.as_str()) {
            self.preset = "medium".to_string();
        }
    }

    fn filter(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(f) = self.fps {
            parts.push(format!("fps={f}"));
        }
        // `-2` means "derive from the other side, rounded to an even number",
        // which every codec here requires. `force_original_aspect_ratio` is not
        // used: giving both sides is an explicit request to stretch.
        match (self.width, self.height) {
            (Some(w), Some(h)) => parts.push(format!("scale={w}:{h}")),
            (Some(w), None) => parts.push(format!("scale={w}:-2")),
            (None, Some(h)) => parts.push(format!("scale=-2:{h}")),
            (None, None) => {}
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }
}

/// Whether ffmpeg is callable. Cheap enough to run per request.
pub fn available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Duration of `path` in seconds, via ffprobe. `None` when ffprobe is absent or
/// the file has no readable duration; progress then stays indeterminate.
pub fn duration_secs(path: &Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Re-encode `input` into `output`. `progress` is called with the number of
/// seconds of video written so far.
pub fn transcode<F: FnMut(f64)>(
    input: &Path,
    output: &Path,
    set: &Settings,
    progress: F,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    cmd.arg("-i").arg(input);
    if let Some(f) = set.filter() {
        cmd.arg("-vf").arg(f);
    }
    cmd.arg("-c:v").arg(set.codec.encoder());
    cmd.arg("-crf").arg(set.crf.to_string());
    if matches!(set.codec, Codec::H264 | Codec::H265) {
        cmd.arg("-preset").arg(&set.preset);
        // Chroma subsampling and a moov atom at the front, so the result plays
        // in a browser and starts before it is fully downloaded.
        cmd.args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"]);
    }
    if set.codec == Codec::Vp9 {
        // Without an explicit bitrate of 0 libvpx treats -crf as a ceiling on a
        // bitrate-targeted encode instead of constant quality.
        cmd.args(["-b:v", "0"]);
    }
    if set.audio {
        match set.codec {
            Codec::Vp9 => cmd.args(["-c:a", "libopus", "-b:a", "128k"]),
            _ => cmd.args(["-c:a", "aac", "-b:a", "128k"]),
        };
    } else {
        cmd.arg("-an");
    }
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.arg(output);

    run(cmd, progress)
}

/// Spawn `cmd` and follow its `-progress` output, calling `progress` with the
/// number of seconds written so far. Shared by every ffmpeg-driven command.
pub fn run<F: FnMut(f64)>(mut cmd: Command, mut progress: F) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;

    // stderr has to be drained by its own thread. Reading it only after stdout
    // reaches EOF would deadlock: a talkative ffmpeg fills the stderr pipe,
    // blocks on the write, stops emitting progress on stdout, and both
    // processes wait for each other forever.
    let errs = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut tail: Vec<String> = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                // Only the end of the log is ever shown, so keeping more would
                // just let a broken file grow this buffer without bound.
                if tail.len() == ERR_TAIL {
                    tail.remove(0);
                }
                tail.push(line);
            }
            tail
        })
    });

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = v.trim().parse::<i64>() {
                    if us >= 0 {
                        progress(us as f64 / 1_000_000.0);
                    }
                }
            }
        }
    }

    let status = child.wait().map_err(|e| format!("ffmpeg failed: {e}"))?;
    let tail = errs.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        // ffmpeg's last line is the useful one; the rest is usually noise.
        let msg = tail.last().map(|s| s.trim()).unwrap_or("no output");
        return Err(format!("ffmpeg: {msg}"));
    }
    Ok(())
}
