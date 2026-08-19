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
///
/// With yt-dlp the download is attempted more than once, with different
/// settings. YouTube hands out adaptive stream URLs and then refuses them with
/// a 403 partway through unless the request carries a signed-in session, and
/// there is no way to know in advance which variant this machine can actually
/// finish — so the attempts run best quality first and stop at the first one
/// that completes. Every attempt starts from an empty directory, so the
/// remains of a transfer that died at 8% are never mistaken for the result.
pub fn fetch(
    tool: Tool,
    url: &str,
    dir: &Path,
    cookies: bool,
    mut progress: impl FnMut(usize),
) -> Result<PathBuf, String> {
    check_url(url)?;

    let plans = match tool {
        Tool::YtDlp => ytdlp_plans(cookies),
        Tool::Curl => vec![Plan::Curl],
    };

    let mut last = String::new();
    for (i, plan) in plans.iter().enumerate() {
        if i > 0 {
            clear_dir(dir);
        }
        let attempt = match plan {
            Plan::Curl => run_curl(url, dir, &mut progress),
            plan => run_ytdlp(plan, url, dir, &mut progress),
        };
        match attempt {
            Ok(file) => return ensure_mp4(file, dir),
            Err(e) => {
                // yt-dlp saying this means it recognises neither the site nor
                // the file: a plain link to something it has no extractor for.
                // Retrying it with other player settings cannot help, and curl
                // fetches exactly that kind of URL.
                let plain_link = e.contains("Unsupported URL");
                last = e;
                if plain_link {
                    if probe("curl") {
                        clear_dir(dir);
                        return run_curl(url, dir, &mut progress).and_then(|f| ensure_mp4(f, dir));
                    }
                    break;
                }
            }
        }
    }
    clear_dir(dir);
    Err(last)
}

/// One way of asking a downloader for the file.
enum Plan {
    /// yt-dlp with the browser's cookies: the request then looks like a
    /// signed-in person, which is what YouTube wants before it will serve the
    /// adaptive MP4 streams all the way to the end.
    YtDlpCookies(&'static str),
    /// yt-dlp as itself. Enough for every site that does not fight back, and
    /// for YouTube whenever the anonymous player still works.
    YtDlpPlain,
    /// yt-dlp restricted to the mobile-web player, whose only format is the
    /// old single-file progressive MP4 — 360p, and the whole point: that URL
    /// does not expire mid-transfer. Low quality, but it is what stands
    /// between a 403 and nothing at all.
    YtDlpProgressive,
    Curl,
}

/// The yt-dlp attempts to make, best quality first. The cookie attempt is only
/// listed when the user allowed it *and* a browser profile exists to read.
fn ytdlp_plans(cookies: bool) -> Vec<Plan> {
    let mut plans = Vec::new();
    if cookies {
        if let Some(browser) = browser_profile() {
            plans.push(Plan::YtDlpCookies(browser));
        }
    }
    plans.push(Plan::YtDlpPlain);
    plans.push(Plan::YtDlpProgressive);
    plans
}

fn run_ytdlp(
    plan: &Plan,
    url: &str,
    dir: &Path,
    progress: &mut impl FnMut(usize),
) -> Result<PathBuf, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "--no-playlist",
        // Progress on its own lines instead of carriage returns, and no
        // `.part` file left behind if the transfer dies.
        "--newline",
        "--no-part",
        "--restrict-filenames",
        "-o",
        // Long titles are truncated: some filesystems still cap a component at
        // 255 bytes.
        "%(title).150s.%(ext)s",
        // A 403 in the middle of a long transfer is routine on YouTube: the
        // media URL expires and yt-dlp has to ask for a fresh one. Without
        // these it gives up on the first refusal.
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
    match plan {
        Plan::YtDlpCookies(browser) => {
            cmd.args(["--cookies-from-browser", browser]);
            cmd.args(mp4_args(crate::video::available()));
        }
        Plan::YtDlpProgressive => {
            // `player_client=mweb` is the one YouTube client still offering a
            // plain progressive file. The format chain has to be loose here,
            // because that client offers exactly one format and a strict
            // selector would simply not match it.
            cmd.args(["--extractor-args", "youtube:player_client=mweb"]);
            cmd.args(["-f", "b[ext=mp4]/b"]);
        }
        _ => {
            cmd.args(mp4_args(crate::video::available()));
        }
    }
    cmd.args(["--", url]);
    run_child(cmd, "yt-dlp", dir, progress)
}

fn run_curl(url: &str, dir: &Path, progress: &mut impl FnMut(usize)) -> Result<PathBuf, String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-L",
        "--fail",
        "-sS",
        // Take the name from the URL, and from Content-Disposition when the
        // server sends one.
        "--remote-name",
        "--remote-header-name",
        "--",
        url,
    ]);
    match run_child(cmd, "curl", dir, progress) {
        Ok(file) => Ok(file),
        // curl only learns it has no name to write to after the request, and
        // says so on stderr; retrying with a fixed name is friendlier than
        // making the user rewrite the URL.
        Err(e) if e.contains("no length") && newest_file(dir).is_none() => {
            fetch_curl_fallback(url, dir, progress)
        }
        Err(e) => Err(e),
    }
}

/// Spawn a downloader in `dir`, poll the directory while it runs, and return
/// the file it left behind.
fn run_child(
    mut cmd: Command,
    name: &str,
    dir: &Path,
    progress: &mut impl FnMut(usize),
) -> Result<PathBuf, String> {
    let mut child = cmd
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {name}: {e}"))?;

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
            Err(e) => return Err(format!("{name} failed: {e}")),
        }
    };

    if !status.success() {
        let tail = log.lock().unwrap().join("; ");
        let tail = if tail.is_empty() {
            format!("code {}", status.code().unwrap_or(-1))
        } else {
            tail
        };
        return Err(format!("{name} : {tail}"));
    }
    progress(dir_size(dir));
    newest_file(dir).ok_or_else(|| format!("{name} n'a produit aucun fichier"))
}

/// Containers that hold video and that ffmpeg can repackage into MP4 without
/// touching a single frame. Anything else — an audio track, an archive, a PDF
/// pulled in by the generic extractor — is returned exactly as it came.
const REMUXABLE: [&str; 6] = ["webm", "mkv", "mov", "avi", "flv", "ts"];

/// Last word on the container. yt-dlp is already asked for MP4 and told to
/// remux, but a site that only serves WebM, a yt-dlp too old for
/// `--remux-video`, or a fallback attempt that had to take whatever was on
/// offer all end up here with something else. Repackaging is a stream copy, so
/// it costs seconds and loses nothing; if it fails, the original file is kept
/// rather than the download thrown away.
fn ensure_mp4(file: PathBuf, dir: &Path) -> Result<PathBuf, String> {
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !REMUXABLE.contains(&ext.as_str()) || !crate::video::available() {
        return Ok(file);
    }
    let stem = file.file_stem().unwrap_or_default().to_owned();
    let out = dir.join(&stem).with_extension("mp4");
    if out == file || out.exists() {
        return Ok(file);
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    cmd.arg("-i").arg(&file);
    // Every stream is copied as it is, minus the subtitles: MP4 cannot carry
    // the WebVTT tracks WebM arrives with, and their presence alone fails the
    // whole remux.
    cmd.args(["-map", "0", "-c", "copy", "-sn", "-movflags", "+faststart"]);
    cmd.arg(&out);
    match crate::video::run(cmd, |_| {}) {
        Ok(()) => {
            let _ = std::fs::remove_file(&file);
            Ok(out)
        }
        Err(_) => {
            let _ = std::fs::remove_file(&out);
            Ok(file)
        }
    }
}

/// Empty the scratch directory between two attempts, so the leftovers of a
/// transfer that died halfway cannot be picked up as the result of the next.
fn clear_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
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

/// Whether this yt-dlp knows `--js-runtimes`. Cached: the answer costs a
/// process launch, and `fetch` now asks once per attempt.
fn supports_js_runtimes() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        Command::new("yt-dlp")
            .arg("--help")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("--js-runtimes"))
            .unwrap_or(false)
    })
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
    progress: &mut impl FnMut(usize),
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
