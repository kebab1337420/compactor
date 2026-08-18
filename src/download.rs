//! Fetching a file from a URL, delegated to `yt-dlp` or `curl`.
//!
//! The crate has no dependencies, and speaking HTTPS by hand is not a
//! reasonable thing to write here, so the download is handed to whichever of
//! the two tools is on PATH. `yt-dlp` is preferred when both are present: it
//! handles plain file links through its generic extractor *and* the media
//! sites `curl` cannot see, so nothing is lost by picking it first.
//!
//! Arguments are passed as separate argv entries and the URL is validated
//! before it is used, so no user-supplied string is ever interpreted by a
//! shell nor mistaken for an option.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How often the output directory is measured while the child runs. The
/// downloaders write straight to disk, so the size on disk *is* the progress.
const POLL: Duration = Duration::from_millis(250);
/// Lines of the child's stderr kept for the error message.
const KEEP_LINES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
pub enum Tool {
    YtDlp,
    Curl,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Curl => "curl",
        }
    }
}

/// The downloader to use, or `None` when neither tool is callable. Probed once
/// at startup, like ffmpeg.
pub fn available() -> Option<Tool> {
    [Tool::YtDlp, Tool::Curl].into_iter().find(|t| probe(t.name()))
}

fn probe(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Accept only what we are willing to hand to a downloader: an absolute http
/// or https URL, no whitespace and no control characters. A string starting
/// with `-` would otherwise be read as an option by both tools, and every
/// other scheme (`file:`, `ftp:`, ...) is a way to read something local that
/// the caller never meant to expose.
pub fn check_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("l'adresse doit commencer par http:// ou https://".into());
    }
    if url.len() > 4096 {
        return Err("adresse trop longue".into());
    }
    if url.chars().any(|c| c.is_whitespace() || (c as u32) < 0x20) {
        return Err("adresse invalide : espaces ou caractères de contrôle".into());
    }
    Ok(())
}

/// Download `url` into `dir`, which must exist and should be empty: the file
/// that appears there is the result, and the name comes from the server or the
/// media site rather than from anything we compose.
///
/// `progress` is called with the number of bytes on disk so far, every
/// [`POLL`]. The total is not known in advance, so the caller shows an
/// indeterminate progress bar.
pub fn fetch(
    tool: Tool,
    url: &str,
    dir: &Path,
    mut progress: impl FnMut(usize),
) -> Result<PathBuf, String> {
    check_url(url)?;

    let mut cmd = Command::new(tool.name());
    match tool {
        Tool::YtDlp => {
            cmd.args([
                "--no-playlist",
                // Progress on its own lines instead of carriage returns, and
                // no `.part` file left behind if the transfer dies.
                "--newline",
                "--no-part",
                "--restrict-filenames",
                "-o",
                // Long titles are truncated: some filesystems still cap a
                // component at 255 bytes.
                "%(title).150s.%(ext)s",
                "--",
                url,
            ]);
        }
        Tool::Curl => {
            cmd.args([
                "-L",
                "--fail",
                "-sS",
                // Take the name from the URL, and from Content-Disposition
                // when the server sends one.
                "--remote-name",
                "--remote-header-name",
                "--",
                url,
            ]);
        }
    }
    let mut child = cmd
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", tool.name()))?;

    // stderr is drained on its own thread: a full pipe would otherwise block
    // the child while we sit in the polling loop below.
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(err) = child.stderr.take() {
        let log = Arc::clone(&log);
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut l = log.lock().unwrap();
                l.push(line);
                if l.len() > KEEP_LINES {
                    l.remove(0);
                }
            }
        });
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                progress(dir_size(dir));
                thread::sleep(POLL);
            }
            Err(e) => return Err(format!("{} failed: {e}", tool.name())),
        }
    };

    let found = newest_file(dir);
    if !status.success() {
        let tail = log.lock().unwrap().join("; ");
        let tail = if tail.is_empty() {
            format!("code {}", status.code().unwrap_or(-1))
        } else {
            tail
        };
        // curl only learns it has no name to write to after the request, and
        // says so on stderr; retrying with a fixed name is friendlier than
        // making the user rewrite the URL.
        if tool == Tool::Curl && found.is_none() && tail.contains("no length") {
            return fetch_curl_fallback(url, dir, progress);
        }
        return Err(format!("{} : {tail}", tool.name()));
    }
    progress(dir_size(dir));
    found.ok_or_else(|| format!("{} n'a produit aucun fichier", tool.name()))
}

/// `curl -O` with a URL that ends in `/`: nothing to name the file after, so
/// it gets a neutral one.
fn fetch_curl_fallback(
    url: &str,
    dir: &Path,
    mut progress: impl FnMut(usize),
) -> Result<PathBuf, String> {
    let out = dir.join("telechargement.bin");
    let mut child = Command::new("curl")
        .args(["-L", "--fail", "-sS", "-o"])
        .arg(&out)
        .args(["--", url])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                progress(dir_size(dir));
                thread::sleep(POLL);
            }
            Err(e) => return Err(format!("curl failed: {e}")),
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(format!("curl : code {}", status.code().unwrap_or(-1)));
    }
    progress(dir_size(dir));
    Ok(out)
}

/// Bytes sitting in `dir`, one level deep. Cheap enough to run four times a
/// second on a directory holding one file.
fn dir_size(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len() as usize)
        .sum()
}

/// The file the downloader produced. yt-dlp can leave a thumbnail or a
/// subtitle track next to the media, so the biggest file wins.
fn newest_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.metadata().map(|m| m.is_file()).unwrap_or(false))
        .max_by_key(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .map(|e| e.path())
}
