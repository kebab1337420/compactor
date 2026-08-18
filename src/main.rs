//! Command line front end for the Compactor codec.

mod convert;
mod download;
mod server;
mod video;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use compactor::{
    compress, decompress, is_archive, model_memory, DEFAULT_LEVEL, HEADER_LEN, MAX_LEVEL,
};

const USAGE: &str = "\
compactor — advanced lossless compressor (context mixing + arithmetic coding)

USAGE:
    compactor c [-l 0..9] <input> [output]   compress   (default output: <input>.cpt)
    compactor d <input> [output]             decompress (default output: <input> without .cpt)
    compactor t [-l 0..9] <input>            round-trip self-test, no files written
    compactor bench [-l 0..9] <input>        compress, verify and report size and speed
    compactor serve [--port N]               drag-and-drop web interface on localhost
    compactor video [video options] <in> [out]  re-encode a video (lossy, needs ffmpeg)
    compactor convert --to EXT <in> [out]    convert between formats (needs ffmpeg)
    compactor dl <url> [dir]                 download a link (needs yt-dlp or curl)

OPTIONS:
    -l N          compression level 0..9 (default 6). Higher = more memory, better ratio.
    -f            overwrite the output file if it exists.
    -q            quiet: no statistics on stderr.
    --port N      port for `serve` (default 8787).
    --host ADDR   address for `serve` (default 127.0.0.1; anything else exposes
                  the interface to your network).
    --max-size N  largest upload `serve` accepts, in MiB (default 512).

VIDEO OPTIONS (for `video`; lossy re-encode through ffmpeg, which must be on PATH):
    --width N     output width in pixels; the height follows the aspect ratio.
    --height N    output height in pixels; the width follows the aspect ratio.
                  Giving both stretches the picture.
    --fps F       output frame rate (1..240). Default: keep the source rate.
    --crf N       constant quality, lower is better (0..51, or 0..63 for vp9/av1).
                  Default 28.
    --codec NAME  h264 (default), h265, vp9 or av1.
    --preset NAME x264/x265 speed preset, ultrafast..veryslow (default medium).
    --no-audio    drop the audio track.

DOWNLOAD OPTIONS (for `dl`; goes through yt-dlp when present, else curl):
    -c            compress the downloaded file to <name>.cpt and delete the
                  original.
    -l N          level for that compression.

CONVERT OPTIONS (for `convert`; goes through ffmpeg too):
    --to EXT      target format. Also read from the output file name when given.
    --quality N   0..100, higher is better and bigger (default 75). Mapped to
                  whatever scale the target encoder uses.
    --width, --height, --fps, --no-audio behave as above.
";

struct Opts {
    level: u8,
    force: bool,
    quiet: bool,
    port: u16,
    host: String,
    max_size: usize,
    /// `dl -c`: compress what was downloaded.
    compress: bool,
    to: Option<String>,
    quality: u8,
    video: video::Settings,
    files: Vec<String>,
}

fn parse_level(s: &str) -> Result<u8, String> {
    let v: u8 = s.parse().map_err(|_| "level must be 0..9".to_string())?;
    if v > MAX_LEVEL {
        return Err("level must be 0..9".into());
    }
    Ok(v)
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        level: DEFAULT_LEVEL,
        force: false,
        quiet: false,
        port: 8787,
        host: "127.0.0.1".to_string(),
        max_size: 512,
        compress: false,
        to: None,
        quality: 75,
        video: video::Settings::default(),
        files: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-l" | "--level" => {
                i += 1;
                o.level = parse_level(args.get(i).ok_or("-l needs a value")?)?;
            }
            s if s.starts_with("-l") && s.len() > 2 => o.level = parse_level(&s[2..])?,
            "--port" => {
                i += 1;
                o.port = args
                    .get(i)
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|_| "port must be 1..65535".to_string())?;
            }
            "--host" => {
                i += 1;
                o.host = args.get(i).ok_or("--host needs a value")?.clone();
            }
            "--max-size" => {
                i += 1;
                o.max_size = args
                    .get(i)
                    .ok_or("--max-size needs a value")?
                    .parse()
                    .map_err(|_| "--max-size must be a whole number of MiB".to_string())?;
            }
            "--width" => {
                i += 1;
                o.video.width = Some(
                    args.get(i)
                        .ok_or("--width needs a value")?
                        .parse()
                        .map_err(|_| "--width must be a whole number of pixels".to_string())?,
                );
            }
            "--height" => {
                i += 1;
                o.video.height = Some(
                    args.get(i)
                        .ok_or("--height needs a value")?
                        .parse()
                        .map_err(|_| "--height must be a whole number of pixels".to_string())?,
                );
            }
            "--fps" => {
                i += 1;
                o.video.fps = Some(
                    args.get(i)
                        .ok_or("--fps needs a value")?
                        .parse()
                        .map_err(|_| "--fps must be a number".to_string())?,
                );
            }
            "--crf" => {
                i += 1;
                o.video.crf = args
                    .get(i)
                    .ok_or("--crf needs a value")?
                    .parse()
                    .map_err(|_| "--crf must be 0..63".to_string())?;
            }
            "--codec" => {
                i += 1;
                let name = args.get(i).ok_or("--codec needs a value")?;
                o.video.codec = video::Codec::parse(&name.to_lowercase())
                    .ok_or_else(|| format!("unknown codec '{name}' (h264, h265, vp9, av1)"))?;
            }
            "--preset" => {
                i += 1;
                o.video.preset = args.get(i).ok_or("--preset needs a value")?.clone();
            }
            "--to" | "--format" => {
                i += 1;
                o.to = Some(args.get(i).ok_or("--to needs a value")?.clone());
            }
            "--quality" => {
                i += 1;
                let v: u32 = args
                    .get(i)
                    .ok_or("--quality needs a value")?
                    .parse()
                    .map_err(|_| "--quality must be 0..100".to_string())?;
                if v > 100 {
                    return Err("--quality must be 0..100".into());
                }
                o.quality = v as u8;
            }
            "--no-audio" => o.video.audio = false,
            "-c" | "--compress" => o.compress = true,
            "-f" | "--force" => o.force = true,
            "-q" | "--quiet" => o.quiet = true,
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            s => o.files.push(s.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

fn human(n: usize) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

/// Whether two paths name the same file. A plain `==` misses `a.mp4` against
/// `./a.mp4`, and ffmpeg reading and writing one file at once destroys it.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let real = |p: &Path| -> Option<PathBuf> {
        // The output usually does not exist yet, so only its directory can be
        // resolved; the file name is compared as written.
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
        // Both sides go through `canonicalize`, including the implicit current
        // directory: on Windows it prefixes the path, so comparing a canonical
        // path against a raw `current_dir` would never match.
        let dir = match dir {
            Some(d) => d.canonicalize().ok()?,
            None => std::env::current_dir().ok()?.canonicalize().ok()?,
        };
        Some(dir.join(p.file_name()?))
    };
    match (real(a), real(b)) {
        // Windows file names are case-insensitive, so `SRC.mp4` and `src.mp4`
        // are the same file and must both be rejected.
        (Some(x), Some(y)) if cfg!(windows) => x
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&y.as_os_str().to_string_lossy()),
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn check_output(path: &Path, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists (use -f to overwrite)",
            path.display()
        ));
    }
    Ok(())
}

/// `original` and `coded` are the uncompressed and compressed sizes whichever
/// direction we ran in, so the ratio always reads the same way.
fn report(label: &str, original: usize, coded: usize, secs: f64, quiet: bool) {
    if quiet {
        return;
    }
    let (ratio, bpc) = if original > 0 {
        (
            coded as f64 / original as f64 * 100.0,
            coded as f64 * 8.0 / original as f64,
        )
    } else {
        (0.0, 0.0)
    };
    let speed = if secs > 0.0 {
        original as f64 / secs / (1024.0 * 1024.0)
    } else {
        f64::INFINITY
    };
    eprintln!(
        "{label}: {} original, {} coded ({ratio:.2}%, {bpc:.4} bpc) in {secs:.2}s ({speed:.2} MiB/s)",
        human(original),
        human(coded)
    );
}

fn expect_files(opts: &Opts, max: usize) -> Result<(), String> {
    if opts.files.len() > max {
        return Err(format!(
            "unexpected extra argument '{}'",
            opts.files[max]
        ));
    }
    Ok(())
}

/// Lossy re-encode. Separate from every other command: nothing here goes
/// through the Compactor codec, ffmpeg does all of the work.
fn run_video(opts: &Opts, input: &str) -> Result<(), String> {
    if !video::available() {
        return Err("ffmpeg was not found on PATH; the `video` command needs it".into());
    }
    let mut set = video::Settings {
        width: opts.video.width,
        height: opts.video.height,
        fps: opts.video.fps,
        crf: opts.video.crf,
        preset: opts.video.preset.clone(),
        codec: opts.video.codec,
        audio: opts.video.audio,
    };
    set.sanitise();

    let in_path = PathBuf::from(input);
    let out_path: PathBuf = match opts.files.get(1) {
        Some(p) => PathBuf::from(p),
        None => {
            let stem = input.rsplit_once('.').map(|(s, _)| s).unwrap_or(input);
            PathBuf::from(format!("{stem}-reencode.{}", set.codec.ext()))
        }
    };
    if same_file(&in_path, &out_path) {
        return Err("input and output are the same file".into());
    }
    check_output(&out_path, opts.force)?;

    let before = fs::metadata(&in_path)
        .map_err(|e| format!("cannot read {input}: {e}"))?
        .len() as usize;
    let total = video::duration_secs(&in_path);
    let t = Instant::now();
    let mut last = -1.0f64;
    let res = video::transcode(&in_path, &out_path, &set, |secs| {
        if opts.quiet || secs - last < 0.5 {
            return;
        }
        last = secs;
        match total {
            Some(d) if d > 0.0 => {
                eprint!("\rencoding: {:.0}% ({secs:.0}s / {d:.0}s)", secs / d * 100.0)
            }
            _ => eprint!("\rencoding: {secs:.0}s"),
        }
        let _ = std::io::stderr().flush();
    });
    if let Err(e) = res {
        // ffmpeg creates the output before it fails, so a broken half-file
        // would otherwise be left exactly where a good one belongs.
        let _ = fs::remove_file(&out_path);
        return Err(e);
    }
    let secs = t.elapsed().as_secs_f64();
    let after = fs::metadata(&out_path).map(|m| m.len() as usize).unwrap_or(0);
    if !opts.quiet {
        // Not `report`: bits per character means nothing for a lossy re-encode.
        let ratio = if before > 0 {
            after as f64 / before as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "\r\x1b[Kvideo: {} in, {} out ({ratio:.1}%) in {secs:.1}s",
            human(before),
            human(after)
        );
    }
    println!("{}", out_path.display());
    Ok(())
}

/// Format conversion. Like `video`, ffmpeg does all of the work and nothing
/// here touches the Compactor codec.
fn run_convert(opts: &Opts, input: &str) -> Result<(), String> {
    if !video::available() {
        return Err("ffmpeg was not found on PATH; the `convert` command needs it".into());
    }
    // The target comes from --to, or from the extension of an explicit output
    // name: `convert clip.mp4 clip.webp` says the same thing twice otherwise.
    let named = opts.to.clone().or_else(|| {
        opts.files
            .get(1)
            .and_then(|p| p.rsplit_once('.').map(|(_, e)| e.to_string()))
    });
    let named = named.ok_or_else(|| {
        format!(
            "missing target format: give --to EXT (one of {})",
            convert::names()
        )
    })?;
    let format = convert::format(&named)
        .ok_or_else(|| format!("unknown format '{named}' (one of {})", convert::names()))?;

    let mut o = convert::Options::new(format);
    o.width = opts.video.width;
    o.height = opts.video.height;
    o.fps = opts.video.fps;
    o.quality = opts.quality;
    o.audio = opts.video.audio;
    o.sanitise();

    let in_path = PathBuf::from(input);
    let out_path: PathBuf = match opts.files.get(1) {
        Some(p) => PathBuf::from(p),
        None => {
            let stem = input.rsplit_once('.').map(|(s, _)| s).unwrap_or(input);
            PathBuf::from(format!("{stem}.{}", format.ext))
        }
    };
    if same_file(&in_path, &out_path) {
        return Err("input and output are the same file".into());
    }
    check_output(&out_path, opts.force)?;

    let before = fs::metadata(&in_path)
        .map_err(|e| format!("cannot read {input}: {e}"))?
        .len() as usize;
    let total = video::duration_secs(&in_path);
    let t = Instant::now();
    let mut last = -1.0f64;
    let res = convert::convert(&in_path, &out_path, &o, |secs| {
        if opts.quiet || secs - last < 0.5 {
            return;
        }
        last = secs;
        match total {
            Some(d) if d > 0.0 => eprint!(
                "\rconverting: {:.0}% ({secs:.0}s / {d:.0}s)",
                secs / d * 100.0
            ),
            _ => eprint!("\rconverting: {secs:.0}s"),
        }
        let _ = std::io::stderr().flush();
    });
    if let Err(e) = res {
        // ffmpeg creates the output before it fails, so a broken half-file
        // would otherwise be left exactly where a good one belongs.
        let _ = fs::remove_file(&out_path);
        return Err(e);
    }
    let secs = t.elapsed().as_secs_f64();
    let after = fs::metadata(&out_path).map(|m| m.len() as usize).unwrap_or(0);
    if !opts.quiet {
        let ratio = if before > 0 {
            after as f64 / before as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "\r\x1b[Kconvert: {} in, {} out ({ratio:.1}%) in {secs:.1}s",
            human(before),
            human(after)
        );
    }
    println!("{}", out_path.display());
    Ok(())
}

/// `compactor dl <url> [dir]`: fetch a link into `dir` (the current directory
/// by default), optionally compressing what comes back.
fn run_download(opts: &Opts, url: &str) -> Result<(), String> {
    let tool = download::available()
        .ok_or("neither yt-dlp nor curl was found on PATH; the `dl` command needs one of them")?;
    download::check_url(url)?;
    let dir = PathBuf::from(opts.files.get(1).map(String::as_str).unwrap_or("."));
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // The downloader names the file itself, so it gets a scratch directory of
    // its own and the result is moved next door once the name is known.
    let scratch = dir.join(format!(".compactor-dl-{}", std::process::id()));
    fs::create_dir_all(&scratch).map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;

    let t = Instant::now();
    let mut last = 0usize;
    let fetched = download::fetch(tool, url, &scratch, |bytes| {
        if opts.quiet || bytes == last {
            return;
        }
        last = bytes;
        eprint!("\rdownloading: {}", human(bytes));
        let _ = std::io::stderr().flush();
    });
    let fetched = match fetched {
        Ok(p) => p,
        Err(e) => {
            let _ = fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };

    let name = fetched
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "telechargement".to_string());
    let out_path = dir.join(&name);
    let finish = |p: &PathBuf| -> Result<(), String> {
        check_output(p, opts.force)?;
        // A rename across the same filesystem is instant; the copy is the
        // fallback for the cases where it is not.
        if fs::rename(&fetched, p).is_err() {
            fs::copy(&fetched, p).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        }
        Ok(())
    };
    let res = finish(&out_path);
    let downloaded = fs::metadata(&fetched)
        .or_else(|_| fs::metadata(&out_path))
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    if let Err(e) = res {
        let _ = fs::remove_dir_all(&scratch);
        return Err(e);
    }
    let _ = fs::remove_dir_all(&scratch);
    let secs = t.elapsed().as_secs_f64();
    if !opts.quiet {
        eprintln!("\r\x1b[Kdownload: {} in {secs:.1}s", human(downloaded));
    }

    if !opts.compress {
        println!("{}", out_path.display());
        return Ok(());
    }

    let data = fs::read(&out_path).map_err(|e| format!("cannot read {}: {e}", out_path.display()))?;
    let cpt = PathBuf::from(format!("{}.cpt", out_path.display()));
    check_output(&cpt, opts.force)?;
    let t = Instant::now();
    let blob = compress(&data, opts.level);
    let secs = t.elapsed().as_secs_f64();
    fs::write(&cpt, &blob).map_err(|e| format!("cannot write {}: {e}", cpt.display()))?;
    // The download was only a means to get the bytes: keeping both files is
    // never what `-c` was asked for.
    let _ = fs::remove_file(&out_path);
    report("compress", data.len(), blob.len(), secs, opts.quiet);
    println!("{}", cpt.display());
    Ok(())
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        print!("{USAGE}");
        return Ok(());
    }
    let cmd = argv[0].clone();
    if cmd == "-h" || cmd == "--help" || cmd == "help" {
        print!("{USAGE}");
        return Ok(());
    }
    let opts = parse_args(&argv[1..])?;

    if cmd == "serve" {
        expect_files(&opts, 0)?;
        return server::serve(&opts.host, opts.port, opts.level, opts.max_size);
    }

    let input = opts
        .files
        .first()
        .ok_or_else(|| "missing input file".to_string())?;

    if cmd == "video" {
        expect_files(&opts, 2)?;
        return run_video(&opts, input);
    }

    if cmd == "convert" {
        expect_files(&opts, 2)?;
        return run_convert(&opts, input);
    }

    if cmd == "dl" || cmd == "download" {
        expect_files(&opts, 2)?;
        return run_download(&opts, input);
    }

    let data = fs::read(input).map_err(|e| format!("cannot read {input}: {e}"))?;

    match cmd.as_str() {
        "c" | "compress" => {
            expect_files(&opts, 2)?;
            let out_path: PathBuf = match opts.files.get(1) {
                Some(p) => PathBuf::from(p),
                None => PathBuf::from(format!("{input}.cpt")),
            };
            check_output(&out_path, opts.force)?;
            let t = Instant::now();
            let blob = compress(&data, opts.level);
            let secs = t.elapsed().as_secs_f64();
            check_output(&out_path, opts.force)?;
            fs::write(&out_path, &blob)
                .map_err(|e| format!("cannot write {}: {e}", out_path.display()))?;
            report("compress", data.len(), blob.len(), secs, opts.quiet);
            if !opts.quiet {
                eprintln!(
                    "model memory: {} (level {})",
                    human(model_memory(opts.level)),
                    opts.level
                );
            }
        }
        "d" | "x" | "decompress" => {
            expect_files(&opts, 2)?;
            if !is_archive(&data) {
                return Err(format!("{input} is not a Compactor archive"));
            }
            let out_path: PathBuf = match opts.files.get(1) {
                Some(p) => PathBuf::from(p),
                None => {
                    let s = input.strip_suffix(".cpt").ok_or_else(|| {
                        "cannot infer output name (input does not end in .cpt); give it explicitly"
                            .to_string()
                    })?;
                    PathBuf::from(s)
                }
            };
            check_output(&out_path, opts.force)?;
            let t = Instant::now();
            let out = decompress(&data)?;
            let secs = t.elapsed().as_secs_f64();
            check_output(&out_path, opts.force)?;
            fs::write(&out_path, &out)
                .map_err(|e| format!("cannot write {}: {e}", out_path.display()))?;
            report("decompress", out.len(), data.len(), secs, opts.quiet);
        }
        "t" | "test" => {
            expect_files(&opts, 1)?;
            let t = Instant::now();
            let blob = compress(&data, opts.level);
            let ct = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let back = decompress(&blob)?;
            let dt = t.elapsed().as_secs_f64();
            if back != data {
                return Err("ROUND-TRIP FAILED: output differs from input".into());
            }
            report("compress", data.len(), blob.len(), ct, opts.quiet);
            report("decompress", data.len(), blob.len(), dt, opts.quiet);
            println!("round-trip OK ({} bytes, crc verified)", data.len());
        }
        "bench" => {
            expect_files(&opts, 1)?;
            let t = Instant::now();
            let blob = compress(&data, opts.level);
            let ct = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let back = decompress(&blob)?;
            let dt = t.elapsed().as_secs_f64();
            if back != data {
                return Err("ROUND-TRIP FAILED".into());
            }
            let n = data.len().max(1) as f64;
            println!("input           : {} bytes", data.len());
            println!("stored          : {} bytes", data.len() + HEADER_LEN);
            println!(
                "compactor -l{}   : {} bytes  ({:.2}%, {:.4} bpc)",
                opts.level,
                blob.len(),
                blob.len() as f64 / n * 100.0,
                blob.len() as f64 * 8.0 / n
            );
            println!(
                "compress        : {ct:.2}s ({:.2} MiB/s)",
                n / ct.max(1e-9) / (1024.0 * 1024.0)
            );
            println!(
                "decompress      : {dt:.2}s ({:.2} MiB/s)",
                n / dt.max(1e-9) / (1024.0 * 1024.0)
            );
            println!("model memory    : {}", human(model_memory(opts.level)));
        }
        other => return Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        let _ = writeln!(std::io::stderr(), "error: {e}");
        std::process::exit(1);
    }
}
