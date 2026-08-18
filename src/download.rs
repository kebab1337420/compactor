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
    cookies: bool,
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
                // A 403 in the middle of a long transfer is routine on
                // YouTube: the media URL expires and yt-dlp has to ask for a
                // fresh one. Without these it gives up on the first refusal.
                "--retries",
                "10",
                "--fragment-retries",
                "10",
                "--extractor-retries",
                "3",
            ]);
            if let Some(rt) = js_runtime() {
                cmd.args(["--js-runtimes", rt]);
            }
            if cookies {
                if let Some(browser) = browser_profile() {
                    cmd.args(["--cookies-from-browser", browser]);
                }
            }
            cmd.args(mp4_args(crate::video::available()));
            cmd.args(["--", url]);
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

/// The first browser with a profile on this machine, for
/// `--cookies-from-browser`.
///
/// YouTube now answers an unauthenticated download with either "only images
/// are available" or a 403 a few megabytes into the transfer — the video URL
/// is handed out and then refused. A logged-in session from the user's own
/// browser is what makes the request look like a person; it is also the one
/// remedy that does not involve a second daemon.
///
/// Only the profile directory is looked for here, and yt-dlp is what actually
/// reads the cookie database. The caller can turn the whole thing off.
pub fn browser_profile() -> Option<&'static str> {
    for (browser, paths) in profile_paths() {
        if paths.iter().any(|p| !p.is_empty() && Path::new(p).exists()) {
            return Some(browser);
        }
    }
    None
}

/// Where each browser keeps the profile holding its cookie database. Firefox
/// comes first: it is the one yt-dlp can read while the browser is running,
/// since Chromium locks its database and encrypts the values.
fn profile_paths() -> Vec<(&'static str, Vec<String>)> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let join = |base: &str, rest: &str| -> String {
        if base.is_empty() {
            String::new()
        } else {
            Path::new(base).join(rest).to_string_lossy().into_owned()
        }
    };
    if cfg!(windows) {
        vec![
            ("firefox", vec![join(&appdata, "Mozilla/Firefox/Profiles")]),
            ("chrome", vec![join(&local, "Google/Chrome/User Data")]),
            ("edge", vec![join(&local, "Microsoft/Edge/User Data")]),
            ("brave", vec![join(&local, "BraveSoftware/Brave-Browser/User Data")]),
            ("vivaldi", vec![join(&local, "Vivaldi/User Data")]),
            ("chromium", vec![join(&local, "Chromium/User Data")]),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            (
                "firefox",
                vec![join(&home, "Library/Application Support/Firefox/Profiles")],
            ),
            (
                "chrome",
                vec![join(&home, "Library/Application Support/Google/Chrome")],
            ),
            (
                "brave",
                vec![join(
                    &home,
                    "Library/Application Support/BraveSoftware/Brave-Browser",
                )],
            ),
            (
                "edge",
                vec![join(&home, "Library/Application Support/Microsoft Edge")],
            ),
        ]
    } else {
        vec![
            (
                "firefox",
                vec![
                    join(&home, ".mozilla/firefox"),
                    join(&home, "snap/firefox/common/.mozilla/firefox"),
                ],
            ),
            ("chrome", vec![join(&home, ".config/google-chrome")]),
            ("chromium", vec![join(&home, ".config/chromium")]),
            (
                "brave",
                vec![join(&home, ".config/BraveSoftware/Brave-Browser")],
            ),
            ("vivaldi", vec![join(&home, ".config/vivaldi")]),
        ]
    }
}

/// A JavaScript runtime for yt-dlp to solve YouTube's player challenge with.
/// Without one it falls back to a client whose media URLs are refused partway
/// through the transfer — the download starts, runs for a while and dies on a
/// 403 — so this is the difference between working and not on every YouTube
/// link.
///
/// Only `deno` is enabled by default, hence the explicit flag; `node` is the
/// one people actually have installed. The option itself is recent, so older
/// yt-dlp builds are left alone rather than handed an argument they will
/// reject.
fn js_runtime() -> Option<&'static str> {
    if !supports_js_runtimes() {
        return None;
    }
    ["deno", "node", "bun"].into_iter().find(|rt| probe(rt))
}

fn supports_js_runtimes() -> bool {
    Command::new("yt-dlp")
        .arg("--help")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("--js-runtimes"))
        .unwrap_or(false)
}

/// Format selection for yt-dlp: a video comes back as MP4 whenever the site
/// offers one, because that is the container everything else here — the
/// browser preview, ffmpeg, the phone the file ends up on — reads without
/// argument.
///
/// Sites that only serve VP9/Opus (YouTube above 1080p, mostly) hand out video
/// and audio as separate WebM streams, and gluing them into an MP4 is ffmpeg's
/// job. Without ffmpeg those selectors would fail outright, so the chain falls
/// back to whatever single file the site has. The last `/b` in both chains is
/// what makes a plain file link still work: the generic extractor has one
/// format and it is not a video at all.
fn mp4_args(ffmpeg: bool) -> &'static [&'static str] {
    if ffmpeg {
        &[
            "-f",
            "bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b",
            // Only applies when two streams had to be joined.
            "--merge-output-format",
            "mp4",
            // And this one rewrites a finished WebM/MKV video into MP4 without
            // re-encoding it. Non-video downloads are left alone.
            "--remux-video",
            "mp4",
        ]
    } else {
        &["-f", "b[ext=mp4]/b"]
    }
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
