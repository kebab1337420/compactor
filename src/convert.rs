//! Format conversion, delegated to ffmpeg.
//!
//! Same footing as [`crate::video`]: this is *lossy* for every format that has
//! a quality knob, and it exists because a lossless pass cannot turn an MP4
//! into a WebP. The target format decides the whole command line — codec,
//! frame handling, whether audio survives — so everything a caller can choose
//! lives in [`Options`].
//!
//! Arguments are passed as separate argv entries, so no user-supplied string is
//! ever interpreted by a shell.

use std::path::Path;
use std::process::Command;

use crate::video;

/// What a target format produces. It decides whether the source's audio and
/// its extra frames are kept.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Moving picture, with audio when the container holds it.
    Video,
    /// Moving picture with no audio track at all (GIF, animated WebP).
    Animation,
    /// A single frame; only the first one is written.
    Image,
    /// Audio only; the video stream is dropped.
    Audio,
}

pub struct Format {
    /// File extension, and the name accepted on the command line and in the
    /// web form.
    pub ext: &'static str,
    pub kind: Kind,
    /// Shown in the interface.
    pub label: &'static str,
}

/// Everything that can be produced. Formats whose encoder is not compiled into
/// the local ffmpeg simply fail at run time with ffmpeg's own message, which is
/// clearer than anything guessed here.
pub const FORMATS: [Format; 16] = [
    Format { ext: "mp4", kind: Kind::Video, label: "MP4 (H.264)" },
    Format { ext: "webm", kind: Kind::Video, label: "WebM (VP9)" },
    Format { ext: "mkv", kind: Kind::Video, label: "MKV (H.264)" },
    Format { ext: "mov", kind: Kind::Video, label: "MOV (H.264)" },
    Format { ext: "gif", kind: Kind::Animation, label: "GIF animé" },
    Format { ext: "webp", kind: Kind::Animation, label: "WebP (animé si vidéo)" },
    Format { ext: "png", kind: Kind::Image, label: "PNG" },
    Format { ext: "jpg", kind: Kind::Image, label: "JPEG" },
    Format { ext: "bmp", kind: Kind::Image, label: "BMP" },
    Format { ext: "tiff", kind: Kind::Image, label: "TIFF" },
    Format { ext: "mp3", kind: Kind::Audio, label: "MP3" },
    Format { ext: "m4a", kind: Kind::Audio, label: "M4A (AAC)" },
    Format { ext: "opus", kind: Kind::Audio, label: "Opus" },
    Format { ext: "ogg", kind: Kind::Audio, label: "OGG (Vorbis)" },
    Format { ext: "flac", kind: Kind::Audio, label: "FLAC" },
    Format { ext: "wav", kind: Kind::Audio, label: "WAV" },
];

/// Look a format up by extension. Aliases the spellings people actually type.
pub fn format(name: &str) -> Option<&'static Format> {
    let name = match name.trim().trim_start_matches('.') {
        "jpeg" => "jpg",
        "tif" => "tiff",
        "matroska" => "mkv",
        "m4v" => "mp4",
        "oga" => "ogg",
        other => other,
    };
    FORMATS.iter().find(|f| f.ext.eq_ignore_ascii_case(name))
}

/// Comma-separated list of every target, for error messages and usage text.
pub fn names() -> String {
    FORMATS
        .iter()
        .map(|f| f.ext)
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct Options {
    pub format: &'static Format,
    /// Target width in pixels; `None` derives it from the height.
    pub width: Option<u32>,
    /// Target height in pixels; `None` derives it from the width.
    pub height: Option<u32>,
    /// Target frame rate; `None` keeps the source rate. Ignored for a still
    /// image and for audio.
    pub fps: Option<f32>,
    /// 0..100, higher is better quality and a bigger file. Every format maps it
    /// to whatever scale its encoder uses.
    pub quality: u8,
    /// Keep the audio track when the target can hold one.
    pub audio: bool,
}

impl Options {
    pub fn new(format: &'static Format) -> Options {
        Options {
            format,
            width: None,
            height: None,
            fps: None,
            quality: 75,
            audio: true,
        }
    }

    /// Clamp everything to a range ffmpeg accepts. Values arrive from a web
    /// form, so nothing here may be trusted.
    pub fn sanitise(&mut self) {
        self.width = self.width.map(|w| w.clamp(16, 7680));
        self.height = self.height.map(|h| h.clamp(16, 7680));
        self.fps = self
            .fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| f.clamp(1.0, 240.0));
        self.quality = self.quality.min(100);
    }

    /// Scaling and frame rate, as a filter chain. Shared by every kind, so the
    /// GIF palette pass can splice it in front of its own filters.
    fn filter(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(f) = self.fps.filter(|_| self.format.kind != Kind::Image) {
            parts.push(format!("fps={f}"));
        }
        // `-2` means "derive from the other side, rounded to an even number",
        // which the video codecs require; giving both sides is an explicit
        // request to stretch.
        match (self.width, self.height) {
            (Some(w), Some(h)) => parts.push(format!("scale={w}:{h}:flags=lanczos")),
            (Some(w), None) => parts.push(format!("scale={w}:-2:flags=lanczos")),
            (None, Some(h)) => parts.push(format!("scale=-2:{h}:flags=lanczos")),
            (None, None) => {}
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }

    /// JPEG and friends take `-qscale:v`, where 2 is best and 31 is worst —
    /// the opposite direction and a different range from ours.
    fn qscale(&self) -> u32 {
        let q = self.quality as f64 / 100.0;
        (31.0 - q * 29.0).round().clamp(2.0, 31.0) as u32
    }

    /// Constant-quality value for the video codecs, where 0 is best. x264 and
    /// x265 stop at 51, VP9 at 63; the low end is left alone because a CRF of 0
    /// on a re-encode is a waste of bytes.
    fn crf(&self, max: u32) -> u32 {
        let q = self.quality as f64 / 100.0;
        let hi = max as f64;
        ((1.0 - q) * hi).round().clamp(4.0, hi) as u32
    }

    /// Audio bitrate in kbit/s, for the encoders that want one.
    fn audio_kbps(&self) -> u32 {
        (64 + self.quality as u32 * 2).clamp(64, 320)
    }
}

/// Convert `input` into `output`, which must already carry the extension of
/// `opts.format`. `progress` is called with the number of seconds written so
/// far; for a still image it is never called at all.
pub fn convert<F: FnMut(f64)>(
    input: &Path,
    output: &Path,
    opts: &Options,
    progress: F,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    cmd.arg("-i").arg(input);

    let filter = opts.filter();
    match opts.format.kind {
        Kind::Video => {
            if let Some(f) = &filter {
                cmd.arg("-vf").arg(f);
            }
            match opts.format.ext {
                "webm" => {
                    // Without an explicit bitrate of 0 libvpx treats -crf as a
                    // ceiling on a bitrate-targeted encode, not constant
                    // quality.
                    cmd.args(["-c:v", "libvpx-vp9", "-b:v", "0"]);
                    cmd.arg("-crf").arg(opts.crf(63).to_string());
                }
                _ => {
                    cmd.args(["-c:v", "libx264", "-preset", "medium"]);
                    cmd.arg("-crf").arg(opts.crf(51).to_string());
                    // Chroma subsampling every player accepts, and a moov atom
                    // at the front so the result starts before it is fully
                    // downloaded.
                    cmd.args(["-pix_fmt", "yuv420p"]);
                    if opts.format.ext == "mp4" || opts.format.ext == "mov" {
                        cmd.args(["-movflags", "+faststart"]);
                    }
                }
            }
            if opts.audio {
                match opts.format.ext {
                    "webm" => cmd.args(["-c:a", "libopus", "-b:a", "128k"]),
                    _ => cmd.args(["-c:a", "aac", "-b:a", "128k"]),
                };
            } else {
                cmd.arg("-an");
            }
        }
        Kind::Animation => {
            // Neither container carries sound.
            cmd.arg("-an");
            if opts.format.ext == "gif" {
                // A GIF holds 256 colours, and ffmpeg's default palette is a
                // fixed one that turns gradients into mud. Generating the
                // palette from the actual frames costs one extra filter and is
                // the whole difference between a usable GIF and a cheap one.
                let pre = filter.map(|f| format!("{f},")).unwrap_or_default();
                cmd.arg("-filter_complex").arg(format!(
                    "{pre}split[a][b];[a]palettegen=stats_mode=diff[p];\
                     [b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle"
                ));
                cmd.args(["-loop", "0"]);
            } else {
                if let Some(f) = &filter {
                    cmd.arg("-vf").arg(f);
                }
                cmd.args(["-c:v", "libwebp", "-loop", "0", "-preset", "picture"]);
                cmd.arg("-q:v").arg(opts.quality.to_string());
            }
        }
        Kind::Image => {
            if let Some(f) = &filter {
                cmd.arg("-vf").arg(f);
            }
            // A video source would otherwise write one file per frame, and
            // ffmpeg refuses without a numbered pattern in the name.
            cmd.args(["-frames:v", "1", "-an"]);
            match opts.format.ext {
                "jpg" => {
                    cmd.arg("-qscale:v").arg(opts.qscale().to_string());
                }
                "png" => {
                    // PNG is lossless; quality only buys compression effort.
                    cmd.args(["-compression_level", "9"]);
                }
                _ => {}
            }
        }
        Kind::Audio => {
            cmd.arg("-vn");
            let kbps = format!("{}k", opts.audio_kbps());
            match opts.format.ext {
                "mp3" => {
                    cmd.args(["-c:a", "libmp3lame"]);
                    cmd.arg("-b:a").arg(&kbps);
                }
                "m4a" => {
                    cmd.args(["-c:a", "aac"]);
                    cmd.arg("-b:a").arg(&kbps);
                }
                "opus" => {
                    cmd.args(["-c:a", "libopus"]);
                    cmd.arg("-b:a").arg(&kbps);
                }
                "ogg" => {
                    cmd.args(["-c:a", "libvorbis"]);
                    cmd.arg("-b:a").arg(&kbps);
                }
                // FLAC and WAV are lossless: a bitrate would be ignored.
                "flac" => {
                    cmd.args(["-c:a", "flac"]);
                }
                _ => {
                    cmd.args(["-c:a", "pcm_s16le"]);
                }
            }
        }
    }

    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.arg(output);
    video::run(cmd, progress)
}
