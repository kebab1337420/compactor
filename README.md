# Compactor

A lossless, context-mixing compressor written in Rust with no external
dependencies. It trades speed for compression ratio: on both text and binary
data it compresses smaller than gzip, bzip2 and LZMA, at roughly 0.4 MiB/s.

```
cargo build --release
```

## Usage

```
compactor c <input> [output]     compress   (alias: compress)
compactor d <input> [output]     decompress (alias: x, decompress)
compactor t <input>              round-trip test, verifies the CRC
compactor bench <input>          timing report
compactor serve                  drag-and-drop web interface
compactor video <input> [output] lossy video re-encode through ffmpeg
compactor convert --to EXT <input> [output]   format conversion through ffmpeg
compactor dl <url> [dir]         download a link through yt-dlp or curl
```

Options:

| Option | Meaning |
| --- | --- |
| `-l N` | Level 0..9, default 6. Higher = more model memory, slightly better ratio. |
| `-f` | Overwrite the output file if it exists. |
| `-q` | Suppress the progress report. |
| `--port N` | Port for `serve`, default 8787. |
| `--host ADDR` | Address for `serve`, default 127.0.0.1. |
| `--max-size N` | Largest upload `serve` accepts, in MiB, default 512. |
| `-c` | For `dl`: compress the downloaded file and keep only the `.cpt`. |

Options for `video` only:

| Option | Meaning |
| --- | --- |
| `--width N` | Output width in pixels; the height follows the aspect ratio. |
| `--height N` | Output height in pixels; the width follows the aspect ratio. Giving both stretches the picture. |
| `--fps F` | Output frame rate, 1..240. Default: keep the source rate. |
| `--crf N` | Constant quality, lower is better. 0..51, or 0..63 for vp9 and av1. Default 28. |
| `--codec NAME` | `h264` (default), `h265`, `vp9` or `av1`. |
| `--preset NAME` | x264/x265 speed preset, `ultrafast`..`veryslow`, default `medium`. |
| `--no-audio` | Drop the audio track. |

Options for `convert` only:

| Option | Meaning |
| --- | --- |
| `--to EXT` | Target format. Also read from the output file name when one is given. |
| `--quality N` | 0..100, higher is better and bigger, default 75. Mapped to whatever scale the target encoder uses. |

`convert` also accepts `--width`, `--height`, `--fps` and `--no-audio`, with the
same meaning as above.

With no output path, `c` appends `.cpt`, `d` strips it, `video` writes
`<name>-reencode.<ext>`, and `convert` writes `<name>.<target ext>`.

## Video re-encoding

This is the one lossy part of the tool and it shares no code with the codec:
`compactor video` builds an ffmpeg command line and runs it. It exists because
re-encoding is the only thing that actually shrinks a video — a lossless pass
over an MP4 gains close to nothing.

```
compactor video --height 720 --fps 30 clip.mp4
compactor video --width 640 --crf 34 --codec h265 --no-audio clip.mov small.mp4
```

`ffmpeg` and `ffprobe` must be on PATH; nothing is bundled. When ffmpeg is
missing the command fails with a plain message and the web interface hides the
video panel. Every value is clamped before it reaches ffmpeg (16..7680 pixels,
1..240 fps, CRF to the encoder's maximum, preset against a fixed list) and all
arguments are passed as separate argv entries, so no user string is ever
interpreted by a shell. The server writes its temporary files under the system
temp directory with names built from the process and job ids, never from the
uploaded file name. A failed run leaves nothing behind: the partial file ffmpeg
had already opened is removed before the error is reported.

## Format conversion

`compactor convert` is the same idea one step wider: ffmpeg again, but the
target format decides the whole command line rather than just the video codec.

```
compactor convert --to webp --width 640 --fps 12 clip.mp4
compactor convert --to mp3 --quality 90 talk.mkv
compactor convert clip.mp4 poster.jpg
```

Sixteen targets, in four groups:

| Group | Formats | What comes out |
| --- | --- | --- |
| Video | `mp4`, `mkv`, `mov` (H.264), `webm` (VP9) | picture and sound |
| Animation | `gif`, `webp` | every frame, no sound |
| Image | `png`, `jpg`, `bmp`, `tiff` | the first frame only |
| Audio | `mp3`, `m4a`, `opus`, `ogg`, `flac`, `wav` | the audio track only |

`--quality` is a single 0..100 scale translated per encoder: a CRF for the video
codecs, `-qscale:v` for JPEG (where the scale runs backwards), `-q:v` for WebP,
a bitrate for the lossy audio encoders. It changes nothing for PNG, FLAC and
WAV, which are lossless. GIF goes through `palettegen`/`paletteuse` rather than
the default fixed palette, which is the whole difference between a usable GIF
and a muddy one.

Converting to the extension the file already has is allowed; the web interface
then proposes `<name>-converti.<ext>` so the download never lands on top of the
original. As with `video`, a failed run removes the partial file before
reporting the error, and formats whose encoder is missing from the local ffmpeg
build fail with ffmpeg's own message.

`ffprobe` is only used to learn the duration, which drives the percentage. When
it is missing the job still runs and reports the footage encoded so far without
a percentage.

Refusing to overwrite the input is not a plain path comparison: `clip.mp4` and
`./clip.mp4` are recognised as the same file, case-insensitively on Windows,
because ffmpeg reading and writing one file at once destroys it.

## Downloading

```
compactor dl https://example.com/dump.sql            # into the current directory
compactor dl -c -l 8 https://example.com/dump.sql .  # and compress what came back
```

Nothing here speaks HTTP itself. The transfer is handed to `yt-dlp` when it is
on PATH and to `curl` otherwise, both invoked with separate argv entries so no
part of the URL is ever seen by a shell. Only `http://` and `https://` are
accepted: every other scheme is a way to read something local that the caller
did not mean to expose, and a URL starting with `-` would be read as an option
by both tools.

The downloader picks the file name — from the URL, from `Content-Disposition`,
or from the media title — so it is given a scratch directory of its own and the
result is moved out once the name is known. Progress is the number of bytes on
disk, sampled four times a second, because neither tool reports a total this
side of the transfer.

`yt-dlp` is preferred when both are installed: it covers plain file links
through its generic extractor *and* the media sites `curl` cannot see, so
nothing is lost by picking it first.

YouTube needs one more thing: a session. An anonymous request is answered
either with "only images are available" or with a 403 a few megabytes into the
transfer — the media URL is handed out and then refused. So when the downloader
is `yt-dlp`, the first browser profile found on the machine (Firefox, then
Chrome, Edge, Brave, Vivaldi, Chromium) is passed as `--cookies-from-browser`
and yt-dlp reads its cookies. Nothing leaves the machine that was not already
going to the site being downloaded, and `--no-cookies` on the CLI or the switch
in the download tab turns it off — at the cost of YouTube refusing the download.
A JavaScript runtime (`node`, `deno` or `bun`) is passed the same way when one
is installed: YouTube's player challenge needs it.

A video comes back as MP4 whenever the site offers one, because that is the
container the browser preview, ffmpeg and whatever the file ends up on all read
without argument. Sites that only serve VP9/Opus — YouTube above 1080p, mostly —
hand out video and audio as two separate WebM streams, so joining them into an
MP4, and remuxing a finished WebM into one, is ffmpeg's job; without ffmpeg the
format chain falls back to the best single file the site has. Plain file links
are unaffected: the generic extractor has exactly one format, and it is usually
not a video at all.

## Web interface

```
compactor serve            # then open http://127.0.0.1:8787
```

The page has two tabs. **Compresseur** is the drag-and-drop interface described
below; **Téléchargement** takes a link, fetches it server-side and hands the
file back to the browser, optionally running the codec on it first.

Drop files on the page, paste them from the clipboard with Ctrl+V, or click to
browse. Images and videos get a thumbnail, every file gets a live progress bar,
and the result is downloaded from the server when the job finishes. Dropping a
`.cpt` archive decompresses it instead — the operation selector defaults to
`Auto`, which looks at the magic bytes.

The interface is deliberately blunt about one thing: JPEG, PNG, WebP, MP4 and
friends already contain an entropy coder, so a further lossless pass gains
almost nothing while costing minutes at 0.4 MiB/s. The page shows a warning
badge on those types. The real gains are on text, source code, logs, uncompressed
images (BMP, TIFF, PPM), WAV audio, databases and executables.

The page also has a video panel — resolution, frame rate, quality, codec and
audio — shown when the operation selector is set to "Ré-encoder la vidéo" and
hidden entirely when the server reports that ffmpeg is absent. Video jobs report
their progress in seconds of footage rather than bytes, which is why the status
JSON carries a `unit` field.

Next to it is a conversion panel — target format, quality, size, frame rate,
audio — under "Convertir le format". Its format list is not hard-coded in the
page: `/api/config` returns the table from `src/convert.rs`, grouped by kind.

The download tab posts to `/api/download?url=…`, which returns a job id like any
other: the link travels in the query string and the request body stays empty, so
a link to a several-gigabyte file costs one short request. `then=compress` chains
the codec onto the downloaded bytes, at the level set by the slider in the
compressor tab, and only the `.cpt` comes back — the file never makes the round
trip through the browser twice. A downloaded file larger than `--max-size` is
rejected once it lands, since the result is served out of memory. The tab stays
visible when neither downloader is installed but says so instead of failing on
the first click.

The server is a local tool. It binds 127.0.0.1, caps the request body, buffers
at most 16 KiB of headers and runs at most two jobs at a time, but it has no
authentication of any kind: binding it to another address with `--host` exposes
compression jobs to anyone who can reach the port.

## Benchmarks

Compressed size in bytes, smaller is better. Measured on this machine with a
single thread.

Python 3.12 standard library sources, concatenated, 3,000,000 bytes:

| Tool | Size | Ratio | bpc |
| --- | ---: | ---: | ---: |
| gzip -9 | 710,021 | 23.67% | 1.8934 |
| bzip2 -9 | 592,080 | 19.74% | 1.5789 |
| lzma -9e | 566,804 | 18.89% | 1.5115 |
| **compactor -l 6** | **476,652** | **15.89%** | **1.2711** |
| compactor -l 9 | 476,055 | 15.87% | 1.2695 |

`shell32.dll`, 9,083,632 bytes (machine code and resources, not text):

| Tool | Size | Ratio |
| --- | ---: | ---: |
| gzip -9 | 4,198,888 | 46.22% |
| bzip2 -9 | 3,872,167 | 42.63% |
| lzma -9e | 3,149,420 | 34.67% |
| **compactor -l 6** | **3,019,821** | **33.24%** |

Throughput is about 0.44 MiB/s on text and 0.36 MiB/s on binaries, and
decompression costs the same as compression — the decoder runs the identical
model. This is the usual context-mixing trade: roughly 100x slower than LZMA
for roughly 4-16% smaller output.

## How it works

Data is coded one bit at a time by a binary arithmetic coder driven by a
predicted probability. Everything interesting is in the predictor.

**Context models.** Each model hashes some context and looks up an adaptive
counter that estimates P(next bit = 1):

- order 0: the partial byte being coded
- order 1: the previous byte (direct-indexed, no hashing)
- orders 2, 3, 4, 6, 8: hashes of the byte history
- a word model: hash of the current alphanumeric run, which captures
  identifiers and natural-language words regardless of what precedes them
- a sparse model: bytes at distance 1 and 3, which catches fixed-stride
  record structures
- a match model: a hash of the last 6 bytes finds the most recent occurrence
  of the current context; if the match holds, the byte that followed it last
  time is a strong prediction, and its confidence is itself learned per
  (match length, bit position, predicted bit)

**Counters.** Each counter packs into one `u32` as `p:16 | count:10 | tag:6`.
The count drives the adaptation rate through a reciprocal table, so a fresh
counter moves fast and a well-established one moves slowly. The 6-bit tag
detects hash collisions: when the tag of a slot does not match, the slot is
reset instead of being polluted by an unrelated context.

**Mixing.** Model outputs are converted to the logistic domain (`stretch`) and
combined by a two-layer gated linear network trained by online gradient
descent on coding loss. Layer 1 has three mixers, each selecting its weight
set from a different gate: the partial byte combined with the match state, the
previous byte, and the match length combined with the bit position. Layer 2
combines those three outputs, and adapts much more slowly than layer 1 — this
mattered a great deal, and mixing the layers at the same rate cost about 1.5
percentage points of ratio.

**SSE.** Two adaptive probability map stages refine the mixed probability,
one keyed on the partial byte and one on a hash of the partial byte and recent
history. Each is blended 3:1 with its input.

## Format

```
offset  size  field
0       4     magic "CPT6"
4       1     version (1)
5       1     method: 0 = stored, 1 = context model
6       1     level used
7       8     original size, little endian
15      4     CRC-32 of the original data
19      ...   payload
```

If the modelled payload is not smaller than the input, the input is stored
verbatim, so a file never grows by more than the 19-byte header.

Every header field is treated as attacker-chosen, because an archive is
untrusted input:

- the magic, version and method must match, and an unknown method is an error
- the level must be 0..9, otherwise the model would be built from a nonsense size
- the declared original size must fit in `usize` and must be plausible for the
  payload length: probabilities clamp to 4094/4096, so a coded bit costs at
  least about 0.0007 bits and one payload byte can expand to at most ~1418
  output bytes. A header claiming 2^64-1 bytes is refused instead of being
  handed to `Vec::with_capacity`.
- the arithmetic decoder counts how many bytes it reads past the end of the
  payload, and decoding stops as soon as that happens, so a truncated archive
  fails rather than decoding an endless stream of zeros
- the CRC-32 is verified against the decoded output

Corruption is therefore reported rather than returned as data. The one
exception is the last few payload bytes, which are the coder's flush tail and
are partly redundant, so a flip there may go unnoticed by the coder itself —
the CRC still catches it if the output changed.

Both directions work on whole files in memory: peak usage is roughly the input
plus the output plus the model. There is no streaming mode, so files of several
gigabytes are out of scope.

## Levels

The level sets the hash table size: `2^(16+level)` entries per hashed model,
capped at `2^24`, and the match model's table is capped at `2^22`.

| Level | Model memory | Ratio on the text corpus |
| ---: | ---: | ---: |
| 0 | 6.5 MiB | 16.81% |
| 3 | 20.5 MiB | 16.01% |
| 6 | 132.5 MiB | 15.89% |
| 9 | 468.5 MiB | 15.87% |

The same memory is needed to decompress, and the level is recorded in the
header, so a file compressed at level 9 cannot be decompressed in less memory
than it took to make.

## Layout

| File | Contents |
| --- | --- |
| `src/lib.rs` | container format, public API, tests |
| `src/main.rs` | CLI |
| `src/server.rs` | HTTP server and job registry for `serve` |
| `src/video.rs` | ffmpeg wrapper for `video` (lossy, optional) |
| `src/download.rs` | yt-dlp / curl wrapper for `dl` and the download tab |
| `src/convert.rs` | ffmpeg wrapper for `convert`: the format table and its encoder settings |
| `src/ui.html` | the interface, embedded in the binary |
| `src/model.rs` | predictor: context models, counters, mixer, SSE |
| `src/rc.rs` | binary arithmetic coder |
| `src/crc32.rs` | integrity check |

`cargo test --release` runs 16 tests: round trips on empty input, a single
byte, repetitive text, incompressible data, structured binary data and every
level; both corruption cases; the hostile-header cases listed under Format; and
a fuzz test that throws 300 random archive-shaped blobs at the decoder and
requires an `Err` rather than a panic.
