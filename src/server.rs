//! Minimal HTTP server backing the drag-and-drop interface.
//!
//! Hand-rolled on top of `std::net` so the crate keeps zero dependencies. It
//! serves one page and a small job API: the browser uploads bytes, the server
//! runs the codec on a worker thread and the page polls for progress.
//!
//! This is a local tool, not a public service. It binds 127.0.0.1 by default,
//! caps request size, and limits how many jobs run at once, but it does no
//! authentication whatsoever — do not expose it to an untrusted network.

use std::collections::HashMap;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use compactor::{
    compress_with_progress, decompress_with_progress, is_archive, DEFAULT_LEVEL, MAX_LEVEL,
};

use crate::convert;
use crate::download;
use crate::video;

const PAGE: &str = include_str!("ui.html");

/// Longest request line + headers we will buffer before giving up.
const MAX_HEADER: usize = 16 * 1024;
/// Jobs kept in the registry; the oldest finished ones are evicted past this.
const MAX_JOBS: usize = 32;
/// Codec runs at a fraction of a MiB/s and is memory hungry, so a handful of
/// concurrent jobs is already more than a desktop wants.
const MAX_RUNNING: usize = 2;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Running,
    Done,
    Failed,
}

struct Job {
    state: State,
    /// Progress counter and its target. For the codec these are input bytes;
    /// for a video re-encode they are milliseconds of video, because ffmpeg
    /// reports its position in time and not in bytes.
    done: usize,
    total: usize,
    /// What `done` and `total` count, so the page can format them: "bytes" or
    /// "ms".
    unit: &'static str,
    in_size: usize,
    out_size: usize,
    secs: f64,
    op: &'static str,
    name: String,
    /// Extension of the produced file, for video jobs only.
    out_ext: String,
    error: String,
    result: Vec<u8>,
    created: u64,
}

struct Server {
    jobs: Mutex<HashMap<u64, Job>>,
    next_id: AtomicU64,
    running: Mutex<usize>,
    max_body: usize,
    default_level: u8,
    /// Whether ffmpeg answered at startup. Probed once: the page hides the
    /// re-encoding panel when it is missing.
    ffmpeg: bool,
    /// Downloader found at startup, if any. Probed once: the page hides the
    /// download tab when neither yt-dlp nor curl is on PATH.
    downloader: Option<download::Tool>,
}

pub fn serve(host: &str, port: u16, default_level: u8, max_size_mib: usize) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).map_err(|e| format!("cannot bind {addr}: {e}"))?;
    let ffmpeg = video::available();
    let downloader = download::available();
    let srv = Arc::new(Server {
        jobs: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        running: Mutex::new(0),
        max_body: max_size_mib.saturating_mul(1024 * 1024),
        default_level,
        ffmpeg,
        downloader,
    });

    println!("compactor serve: http://{addr}");
    println!("default level {default_level}, upload limit {max_size_mib} MiB");
    if ffmpeg {
        println!("ffmpeg found: video re-encoding (fps, resolution) available");
    } else {
        println!("ffmpeg not on PATH: video re-encoding disabled");
    }
    match downloader {
        Some(t) => println!("{} found: downloading from a link available", t.name()),
        None => println!("neither yt-dlp nor curl on PATH: downloading disabled"),
    }
    if host != "127.0.0.1" && host != "localhost" {
        println!("warning: bound to {host}; the interface has no authentication.");
    }
    println!("Ctrl-C to stop.");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let srv = Arc::clone(&srv);
                // Each connection gets its own thread; a panic inside one is
                // contained and only kills that request.
                thread::spawn(move || {
                    let _ = handle(&srv, s);
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- HTTP plumbing

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut BufReader<TcpStream>, max_body: usize) -> Result<Request, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Read byte by byte until the blank line; requests here are tiny and this
    // avoids over-reading into the body.
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("connection closed before headers".into()),
            Ok(_) => head.push(byte[0]),
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read failed: {e}")),
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > MAX_HEADER {
            return Err("request headers too large".into());
        }
    }

    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let start = lines.next().ok_or("empty request")?;
    let mut parts = start.split_whitespace();
    let method = parts.next().ok_or("malformed request line")?.to_string();
    let target = parts.next().ok_or("malformed request line")?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut len = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v
                    .trim()
                    .parse()
                    .map_err(|_| "malformed Content-Length".to_string())?;
            }
        }
    }
    if len > max_body {
        return Err(format!(
            "upload too large ({len} bytes, limit {max_body}); raise it with --max-size"
        ));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        stream
            .read_exact(&mut body)
            .map_err(|e| format!("incomplete body: {e}"))?;
    }
    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

fn send(stream: &mut TcpStream, status: &str, ctype: &str, extra: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn send_json(stream: &mut TcpStream, status: &str, json: &str) {
    send(stream, status, "application/json; charset=utf-8", "", json.as_bytes());
}

fn send_error(stream: &mut TcpStream, status: &str, msg: &str) {
    send_json(stream, status, &format!("{{\"error\":{}}}", json_string(msg)));
}

/// Quote a string as a JSON literal. Control characters are escaped so error
/// text coming from a file name can never break out of the document.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Percent-decode one query parameter value (`+` means space).
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| url_decode(v))
    })
}

// ---------------------------------------------------------------- routing

fn handle(srv: &Arc<Server>, stream: TcpStream) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut out = stream;
    let req = match read_request(&mut reader, srv.max_body) {
        Ok(r) => r,
        Err(e) => {
            send_error(&mut out, "400 Bad Request", &e);
            return Ok(());
        }
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => send(
            &mut out,
            "200 OK",
            "text/html; charset=utf-8",
            "Cache-Control: no-store\r\n",
            PAGE.as_bytes(),
        ),
        ("GET", "/api/config") => send_json(
            &mut out,
            "200 OK",
            &format!(
                "{{\"default_level\":{},\"max_level\":{},\"max_size\":{},\"ffmpeg\":{},\"downloader\":{},\"formats\":[{}]}}",
                srv.default_level,
                MAX_LEVEL,
                srv.max_body,
                srv.ffmpeg,
                match srv.downloader {
                    Some(t) => json_string(t.name()),
                    None => "null".to_string(),
                },
                convert::FORMATS
                    .iter()
                    .map(|f| format!(
                        "{{\"ext\":{},\"label\":{},\"kind\":{}}}",
                        json_string(f.ext),
                        json_string(f.label),
                        json_string(match f.kind {
                            convert::Kind::Video => "video",
                            convert::Kind::Animation => "animation",
                            convert::Kind::Image => "image",
                            convert::Kind::Audio => "audio",
                        })
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("POST", "/api/jobs") => post_job(srv, &req, &mut out),
        ("POST", "/api/download") => post_download(srv, &req, &mut out),
        ("GET", p) if p.starts_with("/api/jobs/") => get_job(srv, p, &mut out),
        _ => send_error(&mut out, "404 Not Found", "no such route"),
    }
    Ok(())
}

fn post_job(srv: &Arc<Server>, req: &Request, out: &mut TcpStream) {
    if req.body.is_empty() {
        send_error(out, "400 Bad Request", "empty upload");
        return;
    }
    let level = query_param(&req.query, "level")
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|l| *l <= MAX_LEVEL)
        .unwrap_or(DEFAULT_LEVEL);
    let name = query_param(&req.query, "name").unwrap_or_else(|| "fichier".to_string());
    let op = match query_param(&req.query, "op").as_deref() {
        Some("compress") => "compress",
        Some("decompress") => "decompress",
        Some("video") => "video",
        Some("convert") => "convert",
        // `auto` (and anything unspecified) decompresses our own archives and
        // compresses everything else.
        _ => {
            if is_archive(&req.body) {
                "decompress"
            } else {
                "compress"
            }
        }
    };
    if op == "decompress" && !is_archive(&req.body) {
        send_error(out, "400 Bad Request", "ce fichier n'est pas une archive Compactor");
        return;
    }

    let mut settings = None;
    let mut conversion = None;
    let mut out_ext = String::new();
    if op == "convert" {
        if !srv.ffmpeg {
            send_error(
                out,
                "503 Service Unavailable",
                "ffmpeg est introuvable dans le PATH : la conversion est indisponible",
            );
            return;
        }
        let target = query_param(&req.query, "to").unwrap_or_default();
        let fmt = match convert::format(&target) {
            Some(f) => f,
            None => {
                send_error(out, "400 Bad Request", "format de sortie inconnu");
                return;
            }
        };
        let o = convert_options(&req.query, fmt);
        out_ext = fmt.ext.to_string();
        conversion = Some(o);
    }
    if op == "video" {
        if !srv.ffmpeg {
            send_error(
                out,
                "503 Service Unavailable",
                "ffmpeg est introuvable dans le PATH : le ré-encodage vidéo est indisponible",
            );
            return;
        }
        let s = video_settings(&req.query);
        out_ext = s.codec.ext().to_string();
        settings = Some(s);
    }

    {
        let mut running = srv.running.lock().unwrap();
        if *running >= MAX_RUNNING {
            send_error(
                out,
                "503 Service Unavailable",
                "trop de compressions en cours, réessayez dans un instant",
            );
            return;
        }
        *running += 1;
    }

    let id = srv.next_id.fetch_add(1, Ordering::SeqCst);
    let total = req.body.len();
    {
        let mut jobs = srv.jobs.lock().unwrap();
        evict(&mut jobs);
        jobs.insert(
            id,
            Job {
                state: State::Running,
                done: 0,
                // A video job counts milliseconds and does not know the
                // duration until ffprobe has answered; 0 means indeterminate,
                // and the page shows the elapsed footage without a percentage.
                total: if op == "video" || op == "convert" { 0 } else { total },
                unit: if op == "video" || op == "convert" { "ms" } else { "bytes" },
                in_size: total,
                out_size: 0,
                secs: 0.0,
                op,
                name: name.clone(),
                out_ext,
                error: String::new(),
                result: Vec::new(),
                created: id,
            },
        );
    }

    let data = req.body.clone();
    let srv2 = Arc::clone(srv);
    thread::spawn(move || {
        let res = match (settings, conversion) {
            (Some(set), _) => run_video_job(&srv2, id, &name, &data, &set),
            (_, Some(o)) => run_convert_job(&srv2, id, &name, &data, &o),
            _ => run_job(&srv2, id, op, level, &data),
        };
        finish_job(&srv2, id, res);
        *srv2.running.lock().unwrap() -= 1;
    });

    send_json(out, "202 Accepted", &format!("{{\"id\":{id}}}"));
}

/// Drop finished jobs, oldest first, once the registry is full. Running jobs
/// are never evicted.
fn evict(jobs: &mut HashMap<u64, Job>) {
    while jobs.len() >= MAX_JOBS {
        let victim = jobs
            .iter()
            .filter(|(_, j)| j.state != State::Running)
            .min_by_key(|(_, j)| j.created)
            .map(|(k, _)| *k);
        match victim {
            Some(k) => {
                jobs.remove(&k);
            }
            None => break,
        }
    }
}

fn run_job(
    srv: &Arc<Server>,
    id: u64,
    op: &str,
    level: u8,
    data: &[u8],
) -> Result<(Vec<u8>, f64), String> {
    let t = Instant::now();
    let bump = |done: usize| {
        if let Ok(mut jobs) = srv.jobs.lock() {
            if let Some(j) = jobs.get_mut(&id) {
                j.done = done;
            }
        }
    };
    let out = if op == "compress" {
        compress_with_progress(data, level, bump)
    } else {
        decompress_with_progress(data, |done, total| {
            if let Ok(mut jobs) = srv.jobs.lock() {
                if let Some(j) = jobs.get_mut(&id) {
                    j.done = done;
                    j.total = total.max(1);
                }
            }
        })?
    };
    Ok((out, t.elapsed().as_secs_f64()))
}

/// Read the re-encoding parameters out of the query string. Everything is
/// optional and everything is clamped by `Settings::sanitise`, since these
/// values come straight from a form.
fn video_settings(query: &str) -> video::Settings {
    let num = |k: &str| query_param(query, k).and_then(|s| s.parse::<u32>().ok());
    let mut s = video::Settings {
        width: num("width").filter(|v| *v > 0),
        height: num("height").filter(|v| *v > 0),
        fps: query_param(query, "fps")
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|f| *f > 0.0),
        // Lowercased first: `Codec::parse` only knows lowercase names, and
        // silently falling back to H264 on `codec=VP9` would write the wrong
        // file with no warning.
        codec: query_param(query, "codec")
            .map(|s| s.to_lowercase())
            .as_deref()
            .and_then(video::Codec::parse)
            .unwrap_or(video::Codec::H264),
        audio: query_param(query, "audio").as_deref() != Some("0"),
        ..video::Settings::default()
    };
    if let Some(crf) = num("crf") {
        s.crf = crf.min(63) as u8;
    }
    if let Some(p) = query_param(query, "preset") {
        s.preset = p.to_lowercase();
    }
    s.sanitise();
    s
}

/// Re-encode through ffmpeg. The upload is already in memory but ffmpeg wants
/// files, so both ends go through the temp directory and are removed whatever
/// happens.
fn run_video_job(
    srv: &Arc<Server>,
    id: u64,
    name: &str,
    data: &[u8],
    set: &video::Settings,
) -> Result<(Vec<u8>, f64), String> {
    let t = Instant::now();
    let dir = std::env::temp_dir();
    // Names are built from the job id, never from the uploaded file name, so a
    // hostile name cannot escape the temp directory.
    let src_ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let pid = std::process::id();
    let input = dir.join(format!("compactor-{pid}-{id}-in.{src_ext}"));
    let output = dir.join(format!("compactor-{pid}-{id}-out.{}", set.codec.ext()));

    let run = || -> Result<Vec<u8>, String> {
        std::fs::write(&input, data).map_err(|e| format!("cannot write temp file: {e}"))?;
        if let Some(d) = video::duration_secs(&input) {
            if d > 0.0 {
                if let Ok(mut jobs) = srv.jobs.lock() {
                    if let Some(j) = jobs.get_mut(&id) {
                        j.total = (d * 1000.0) as usize;
                    }
                }
            }
        }
        video::transcode(&input, &output, set, |secs| {
            if let Ok(mut jobs) = srv.jobs.lock() {
                if let Some(j) = jobs.get_mut(&id) {
                    let ms = (secs * 1000.0) as usize;
                    // A zero total is the indeterminate case: nothing to clamp
                    // against.
                    j.done = if j.total > 0 { ms.min(j.total) } else { ms };
                }
            }
        })?;
        std::fs::read(&output).map_err(|e| format!("cannot read ffmpeg output: {e}"))
    };

    let res = run();
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    Ok((res?, t.elapsed().as_secs_f64()))
}

/// Conversion settings from the query string. Everything is optional and
/// everything is clamped: these values come straight from a web form.
fn convert_options(query: &str, fmt: &'static convert::Format) -> convert::Options {
    let num = |k: &str| query_param(query, k).and_then(|s| s.parse::<u32>().ok());
    let mut o = convert::Options::new(fmt);
    o.width = num("width").filter(|v| *v > 0);
    o.height = num("height").filter(|v| *v > 0);
    o.fps = query_param(query, "fps")
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|f| *f > 0.0);
    if let Some(q) = num("quality") {
        o.quality = q.min(100) as u8;
    }
    o.audio = query_param(query, "audio").as_deref() != Some("0");
    o.sanitise();
    o
}

/// Convert through ffmpeg. The upload is already in memory but ffmpeg wants
/// files, so both ends go through the temp directory and are removed whatever
/// happens.
fn run_convert_job(
    srv: &Arc<Server>,
    id: u64,
    name: &str,
    data: &[u8],
    opts: &convert::Options,
) -> Result<(Vec<u8>, f64), String> {
    let t = Instant::now();
    let dir = std::env::temp_dir();
    // Names are built from the job id, never from the uploaded file name, so a
    // hostile name cannot escape the temp directory. The source extension is
    // still worth keeping: some demuxers need it to recognise the container.
    let src_ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let pid = std::process::id();
    let input = dir.join(format!("compactor-{pid}-{id}-in.{src_ext}"));
    let output = dir.join(format!("compactor-{pid}-{id}-out.{}", opts.format.ext));

    let run = || -> Result<Vec<u8>, String> {
        std::fs::write(&input, data).map_err(|e| format!("cannot write temp file: {e}"))?;
        if let Some(d) = video::duration_secs(&input) {
            if d > 0.0 {
                if let Ok(mut jobs) = srv.jobs.lock() {
                    if let Some(j) = jobs.get_mut(&id) {
                        j.total = (d * 1000.0) as usize;
                    }
                }
            }
        }
        convert::convert(&input, &output, opts, |secs| {
            if let Ok(mut jobs) = srv.jobs.lock() {
                if let Some(j) = jobs.get_mut(&id) {
                    let ms = (secs * 1000.0) as usize;
                    // A zero total is the indeterminate case: nothing to clamp
                    // against.
                    j.done = if j.total > 0 { ms.min(j.total) } else { ms };
                }
            }
        })?;
        std::fs::read(&output).map_err(|e| format!("cannot read ffmpeg output: {e}"))
    };

    let res = run();
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    Ok((res?, t.elapsed().as_secs_f64()))
}

fn finish_job(srv: &Arc<Server>, id: u64, res: Result<(Vec<u8>, f64), String>) {
    let mut jobs = srv.jobs.lock().unwrap();
    let Some(j) = jobs.get_mut(&id) else { return };
    match res {
        Ok((out, secs)) => {
            j.done = j.total;
            j.out_size = out.len();
            j.secs = secs;
            j.result = out;
            j.state = State::Done;
        }
        Err(e) => {
            j.error = e;
            j.state = State::Failed;
        }
    }
}

fn get_job(srv: &Arc<Server>, path: &str, out: &mut TcpStream) {
    let rest = &path["/api/jobs/".len()..];
    let (id_str, want_result) = match rest.strip_suffix("/result") {
        Some(s) => (s, true),
        None => (rest, false),
    };
    let Ok(id) = id_str.parse::<u64>() else {
        send_error(out, "400 Bad Request", "identifiant de tâche invalide");
        return;
    };
    let jobs = srv.jobs.lock().unwrap();
    let Some(j) = jobs.get(&id) else {
        send_error(out, "404 Not Found", "tâche inconnue ou expirée");
        return;
    };

    if want_result {
        if j.state != State::Done {
            send_error(out, "409 Conflict", "tâche non terminée");
            return;
        }
        let fname = download_name(j);
        let extra = format!(
            "Content-Disposition: attachment; filename=\"{}\"\r\n",
            fname.replace(['"', '\\', '\r', '\n'], "_")
        );
        let body = j.result.clone();
        drop(jobs);
        send(out, "200 OK", "application/octet-stream", &extra, &body);
        return;
    }

    let state = match j.state {
        State::Running => "running",
        State::Done => "done",
        State::Failed => "failed",
    };
    let json = format!(
        "{{\"state\":{},\"op\":{},\"done\":{},\"total\":{},\"unit\":{},\"in_size\":{},\"out_size\":{},\"secs\":{:.3},\"name\":{},\"error\":{}}}",
        json_string(state),
        json_string(j.op),
        j.done,
        j.total,
        json_string(j.unit),
        j.in_size,
        j.out_size,
        j.secs,
        json_string(&j.name),
        json_string(&j.error)
    );
    drop(jobs);
    send_json(out, "200 OK", &json);
}

/// Start a download job. Nothing is uploaded here: the body is empty and the
/// URL travels in the query string, so a link to a several-gigabyte file costs
/// one short request.
fn post_download(srv: &Arc<Server>, req: &Request, out: &mut TcpStream) {
    let Some(tool) = srv.downloader else {
        send_error(
            out,
            "503 Service Unavailable",
            "aucun téléchargeur trouvé : installez yt-dlp ou curl et relancez le serveur",
        );
        return;
    };
    let Some(url) = query_param(&req.query, "url") else {
        send_error(out, "400 Bad Request", "paramètre url manquant");
        return;
    };
    if let Err(e) = download::check_url(&url) {
        send_error(out, "400 Bad Request", &e);
        return;
    }
    // `then=compress` chains the codec onto the downloaded bytes, so the file
    // never has to make the round trip through the browser twice.
    let compress_after = query_param(&req.query, "then").as_deref() == Some("compress");
    let level = query_param(&req.query, "level")
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|l| *l <= MAX_LEVEL)
        .unwrap_or(srv.default_level);

    {
        let mut running = srv.running.lock().unwrap();
        if *running >= MAX_RUNNING {
            send_error(
                out,
                "503 Service Unavailable",
                "trop de tâches en cours, réessayez dans un instant",
            );
            return;
        }
        *running += 1;
    }

    let op = if compress_after {
        "download-compress"
    } else {
        "download"
    };
    let id = srv.next_id.fetch_add(1, Ordering::SeqCst);
    {
        let mut jobs = srv.jobs.lock().unwrap();
        evict(&mut jobs);
        jobs.insert(
            id,
            Job {
                state: State::Running,
                done: 0,
                // The size is unknown until the transfer ends: 0 means
                // indeterminate and the page shows the bytes received without
                // a percentage.
                total: 0,
                unit: "bytes",
                in_size: 0,
                out_size: 0,
                secs: 0.0,
                op,
                // Replaced by the real file name as soon as it is known; until
                // then the card shows the link.
                name: url.clone(),
                out_ext: String::new(),
                error: String::new(),
                result: Vec::new(),
                created: id,
            },
        );
    }

    let srv2 = Arc::clone(srv);
    thread::spawn(move || {
        let res = run_download_job(&srv2, id, tool, &url, compress_after, level);
        finish_job(&srv2, id, res);
        *srv2.running.lock().unwrap() -= 1;
    });

    send_json(out, "202 Accepted", &format!("{{\"id\":{id}}}"));
}

/// Fetch the link into a temp directory of its own, then optionally compress
/// what came back. The directory is removed whatever happens.
fn run_download_job(
    srv: &Arc<Server>,
    id: u64,
    tool: download::Tool,
    url: &str,
    compress_after: bool,
    level: u8,
) -> Result<(Vec<u8>, f64), String> {
    let t = Instant::now();
    let pid = std::process::id();
    // A directory per job: the downloader picks the file name itself, so it
    // needs a place where whatever it writes cannot collide with another job.
    let dir = std::env::temp_dir().join(format!("compactor-{pid}-{id}-dl"));

    let run = || -> Result<Vec<u8>, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create temp dir: {e}"))?;
        let file = download::fetch(tool, url, &dir, |bytes| {
            if let Ok(mut jobs) = srv.jobs.lock() {
                if let Some(j) = jobs.get_mut(&id) {
                    j.done = bytes;
                }
            }
        })?;
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "telechargement".to_string());
        let data = std::fs::read(&file).map_err(|e| format!("cannot read download: {e}"))?;
        if data.is_empty() {
            return Err("le téléchargement est vide".into());
        }
        if data.len() > srv.max_body {
            return Err(format!(
                "fichier téléchargé plus gros que la limite du serveur ({} octets) ; relancez avec --max-size",
                srv.max_body
            ));
        }
        {
            let mut jobs = srv.jobs.lock().unwrap();
            if let Some(j) = jobs.get_mut(&id) {
                j.name = name;
                j.in_size = data.len();
                j.done = data.len();
                j.total = data.len();
            }
        }
        if !compress_after {
            return Ok(data);
        }
        // Second phase: the counters restart on the codec, which does know its
        // total, so the bar becomes a real percentage here.
        {
            let mut jobs = srv.jobs.lock().unwrap();
            if let Some(j) = jobs.get_mut(&id) {
                j.done = 0;
                j.total = data.len();
            }
        }
        Ok(compress_with_progress(&data, level, |done| {
            if let Ok(mut jobs) = srv.jobs.lock() {
                if let Some(j) = jobs.get_mut(&id) {
                    j.done = done;
                }
            }
        }))
    };

    let res = run();
    let _ = std::fs::remove_dir_all(&dir);
    Ok((res?, t.elapsed().as_secs_f64()))
}

/// Name proposed to the browser: `.cpt` appended when compressing, stripped
/// when decompressing, and a `-reencode` suffix on a re-encoded video so it
/// never lands on top of the original.
fn download_name(j: &Job) -> String {
    let base = j.name.rsplit(['/', '\\']).next().unwrap_or("fichier");
    let base = if base.is_empty() { "fichier" } else { base };
    match j.op {
        "compress" | "download-compress" => format!("{base}.cpt"),
        "download" => base.to_string(),
        "video" => {
            let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
            format!("{stem}-reencode.{}", j.out_ext)
        }
        "convert" => {
            let (stem, ext) = base.rsplit_once('.').unwrap_or((base, ""));
            // Converting to the extension the file already has would otherwise
            // propose the original name and quietly overwrite it.
            if ext.eq_ignore_ascii_case(&j.out_ext) {
                format!("{stem}-converti.{}", j.out_ext)
            } else {
                format!("{stem}.{}", j.out_ext)
            }
        }
        _ => base.strip_suffix(".cpt").unwrap_or(base).to_string(),
    }
}
