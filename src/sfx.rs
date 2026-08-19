//! The sound played when a file has been compressed or downloaded.
//!
//! The clip is compiled into the binary, so there is nothing to install next
//! to it and nothing to lose when the executable is moved. The web UI fetches
//! it from `/done.m4a` and lets the browser play it; the command line has no
//! audio output of its own, so it hands the file to `ffplay`, which ships with
//! ffmpeg and is therefore already there whenever the video features are.
//!
//! Playing is best-effort by design: a machine with no ffplay, no sound card
//! or no speakers finishes its job in silence rather than failing it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The clip itself, AAC in an MP4 container: what every browser plays and what
/// ffplay reads without a decoder to install.
pub const SOUND: &[u8] = include_bytes!("done.m4a");

/// MIME type for the `/done.m4a` route.
pub const MIME: &str = "audio/mp4";

/// Play the clip, or do nothing at all if that is not possible. Never blocks:
/// the child is left running and the caller returns while it is still playing,
/// which is why the clip outlives a command line that exits immediately.
pub fn play() {
    if std::env::var_os("COMPACTOR_NO_SOUND").is_some() {
        return;
    }
    let Some(path) = cached() else { return };
    let _ = Command::new("ffplay")
        .args(["-nodisp", "-autoexit", "-loglevel", "quiet"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The clip on disk, written to the temporary directory the first time it is
/// needed: ffplay wants a file, and rewriting 57 KiB on every job would be
/// wasteful. A file of the right size is trusted as-is.
fn cached() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("compactor-done.m4a");
    let fresh = std::fs::metadata(&path)
        .map(|m| m.len() as usize == SOUND.len())
        .unwrap_or(false);
    if !fresh {
        std::fs::write(&path, SOUND).ok()?;
    }
    Some(path)
}
