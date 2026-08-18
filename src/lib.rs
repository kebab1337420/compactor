//! Compactor — advanced lossless compressor built on context mixing.
//!
//! The container format is described in `README.md`. Everything in this crate
//! is deterministic integer arithmetic, so an archive produced on one machine
//! decodes bit-for-bit identically on any other.

mod crc32;
pub mod model;
mod rc;

use crc32::crc32;
use model::Predictor;
use rc::{Decoder, Encoder};

pub const MAGIC: &[u8; 4] = b"CPT6";
pub const VERSION: u8 = 1;
pub const METHOD_STORED: u8 = 0;
pub const METHOD_CM: u8 = 1;
pub const HEADER_LEN: usize = 19;
pub const DEFAULT_LEVEL: u8 = 6;
pub const MAX_LEVEL: u8 = 9;

/// Upper bound on how many output bytes a single payload byte can legitimately
/// stand for. Probabilities are clamped to 4094/4096, so a coded bit costs at
/// least -log2(4094/4096) ~ 0.0007 bits, i.e. one payload byte can expand to
/// about 1418 output bytes. The bound below leaves margin for that while still
/// rejecting a forged header that asks for an unbounded decode.
const MAX_EXPANSION: usize = 2048;

/// Cap on the output buffer reserved up front. Beyond this the vector simply
/// grows as bytes are produced, so a hostile size field cannot turn into a
/// single huge allocation.
const MAX_PREALLOC: usize = 1 << 22;

/// Compress `data` into a self-describing container.
pub fn compress(data: &[u8], level: u8) -> Vec<u8> {
    compress_with_progress(data, level, |_| {})
}

/// Same as [`compress`], calling `progress` with the number of input bytes
/// consumed so far, about a hundred times over the whole input.
pub fn compress_with_progress<F: FnMut(usize)>(
    data: &[u8],
    level: u8,
    mut progress: F,
) -> Vec<u8> {
    let level = level.min(MAX_LEVEL);
    let mut p = Predictor::new(level);
    let mut enc = Encoder::new(data.len() / 3 + 64);
    let step = (data.len() / 100).max(1 << 16);
    let mut next = step;
    for (i, &byte) in data.iter().enumerate() {
        let b = byte as u32;
        for shift in (0..8).rev() {
            let bit = (b >> shift) & 1;
            let pr = p.p();
            enc.encode(bit, pr);
            p.update(bit);
        }
        if i + 1 >= next {
            progress(i + 1);
            next += step;
        }
    }
    let payload = enc.finish();
    progress(data.len());

    // Incompressible input must never grow by more than the header.
    let (method, body) = if payload.len() < data.len() {
        (METHOD_CM, payload)
    } else {
        (METHOD_STORED, data.to_vec())
    };

    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(method);
    out.push(level);
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Decompress a container produced by [`compress`], verifying its checksum.
pub fn decompress(blob: &[u8]) -> Result<Vec<u8>, String> {
    decompress_with_progress(blob, |_, _| {})
}

/// Same as [`decompress`], calling `progress(done, total)` as output bytes are
/// produced.
///
/// Every header field is treated as hostile: an archive is 19 bytes of
/// attacker-chosen data followed by a payload, and none of it may be trusted
/// to size an allocation or bound a loop on its own.
pub fn decompress_with_progress<F: FnMut(usize, usize)>(
    blob: &[u8],
    mut progress: F,
) -> Result<Vec<u8>, String> {
    if blob.len() < HEADER_LEN || &blob[0..4] != MAGIC {
        return Err("not a Compactor archive (bad magic)".into());
    }
    let version = blob[4];
    if version != VERSION {
        return Err(format!("unsupported archive version {version}"));
    }
    let method = blob[5];
    let level = blob[6];
    if level > MAX_LEVEL {
        return Err(format!("invalid level {level} in header"));
    }
    let raw_len = u64::from_le_bytes(blob[7..15].try_into().unwrap());
    let orig_len = usize::try_from(raw_len)
        .map_err(|_| "original size does not fit in memory on this platform".to_string())?;
    let want_crc = u32::from_le_bytes(blob[15..19].try_into().unwrap());
    let body = &blob[HEADER_LEN..];

    let out = match method {
        METHOD_STORED => {
            if body.len() != orig_len {
                return Err("truncated stored archive".into());
            }
            progress(orig_len, orig_len);
            body.to_vec()
        }
        METHOD_CM => {
            if body.is_empty() && orig_len > 0 {
                return Err("truncated archive (no payload)".into());
            }
            let bound = body.len().saturating_add(8).saturating_mul(MAX_EXPANSION);
            if orig_len > bound {
                return Err(format!(
                    "implausible original size {orig_len} for a {} byte payload \
                     (corrupt or hostile archive)",
                    body.len()
                ));
            }
            let mut p = Predictor::new(level);
            let mut dec = Decoder::new(body);
            let mut out = Vec::with_capacity(orig_len.min(MAX_PREALLOC));
            let step = (orig_len / 100).max(1 << 16);
            let mut next = step;
            for i in 0..orig_len {
                let mut byte = 0u32;
                for _ in 0..8 {
                    let pr = p.p();
                    let bit = dec.decode(pr);
                    p.update(bit);
                    byte = (byte << 1) | bit;
                }
                out.push(byte as u8);
                if i + 1 >= next {
                    // Reading past the payload means the archive was cut short
                    // or forged; without this the decoder would happily invent
                    // bytes from an endless run of zeros.
                    if dec.overrun() > 4 {
                        return Err("truncated archive (payload ended early)".into());
                    }
                    progress(i + 1, orig_len);
                    next += step;
                }
            }
            if dec.overrun() > 4 {
                return Err("truncated archive (payload ended early)".into());
            }
            progress(orig_len, orig_len);
            out
        }
        m => return Err(format!("unknown compression method {m}")),
    };

    let got_crc = crc32(&out);
    if got_crc != want_crc {
        return Err(format!(
            "checksum mismatch: expected {want_crc:08x}, got {got_crc:08x} (corrupt archive)"
        ));
    }
    Ok(out)
}

/// True if `blob` looks like a Compactor archive, for callers that want to pick
/// between compressing and decompressing on their own.
pub fn is_archive(blob: &[u8]) -> bool {
    blob.len() >= HEADER_LEN && &blob[0..4] == MAGIC
}

/// Model memory needed at `level`, in bytes. Both directions need the same.
pub fn model_memory(level: u8) -> usize {
    Predictor::new(level.min(MAX_LEVEL)).memory_usage()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8], level: u8) {
        let blob = compress(data, level);
        let back = decompress(&blob).expect("decompress failed");
        assert_eq!(back, data, "round-trip mismatch ({} bytes)", data.len());
    }

    fn header(method: u8, level: u8, len: u64, crc: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.push(VERSION);
        h.push(method);
        h.push(level);
        h.extend_from_slice(&len.to_le_bytes());
        h.extend_from_slice(&crc.to_le_bytes());
        h
    }

    #[test]
    fn empty_input() {
        roundtrip(b"", 3);
    }

    #[test]
    fn single_byte() {
        roundtrip(b"x", 3);
    }

    #[test]
    fn text_repeats() {
        let mut d = Vec::new();
        for i in 0..2000 {
            d.extend_from_slice(
                format!("line {i}: the quick brown fox jumps over it\n").as_bytes(),
            );
        }
        roundtrip(&d, 4);
        let blob = compress(&d, 4);
        assert!(
            blob.len() * 20 < d.len(),
            "expected strong compression on repetitive text, got {} from {}",
            blob.len(),
            d.len()
        );
    }

    #[test]
    fn incompressible_stays_small() {
        // Deterministic pseudo-random bytes: must not grow beyond the header.
        let mut s = 0x243F_6A88_85A3_08D3u64;
        let d: Vec<u8> = (0..50_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 33) as u8
            })
            .collect();
        roundtrip(&d, 4);
        assert!(compress(&d, 4).len() <= d.len() + HEADER_LEN);
    }

    #[test]
    fn all_levels() {
        let d: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
        for level in 0..=9u8 {
            roundtrip(&d, level);
        }
    }

    #[test]
    fn binary_structured() {
        let d: Vec<u8> = (0..40_000u32).flat_map(|i| i.to_le_bytes()).collect();
        roundtrip(&d, 5);
    }

    #[test]
    fn corruption_detected() {
        let d = b"the quick brown fox jumps over the lazy dog".repeat(50);
        let mut blob = compress(&d, 3);
        // Flip a bit in the middle of the payload. The last few bytes are the
        // coder's flush tail and are partly redundant, so a flip there is not
        // guaranteed to change the decoded output.
        let mid = HEADER_LEN + (blob.len() - HEADER_LEN) / 2;
        blob[mid] ^= 0xff;
        assert!(decompress(&blob).is_err(), "corruption went unnoticed");
    }

    #[test]
    fn rejects_foreign_data() {
        assert!(decompress(b"not an archive at all").is_err());
    }

    #[test]
    fn rejects_huge_declared_size() {
        // An archive claiming to hold 2^64-1 bytes must be refused outright,
        // not turned into an allocation or an endless decode. With no payload
        // the truncation check fires first, which is just as good; with a
        // payload it is the plausibility bound that has to catch it.
        let blob = header(METHOD_CM, 6, u64::MAX, 0);
        let e = decompress(&blob).unwrap_err();
        assert!(
            e.contains("implausible") || e.contains("does not fit") || e.contains("truncated"),
            "{e}"
        );

        let mut blob = header(METHOD_CM, 6, u64::MAX, 0);
        blob.extend_from_slice(&[0u8; 64]);
        let e = decompress(&blob).unwrap_err();
        assert!(e.contains("implausible") || e.contains("does not fit"), "{e}");

        let mut blob = header(METHOD_CM, 6, 1 << 40, 0);
        blob.extend_from_slice(&[0u8; 64]);
        assert!(decompress(&blob).is_err());
    }

    #[test]
    fn rejects_invalid_level() {
        let blob = header(METHOD_CM, 200, 0, 0);
        assert_eq!(
            decompress(&blob).unwrap_err(),
            "invalid level 200 in header"
        );
    }

    #[test]
    fn rejects_truncated_payload() {
        let d = b"the quick brown fox jumps over the lazy dog".repeat(200);
        let blob = compress(&d, 3);
        // Cut the payload in half but leave the declared size intact.
        let cut = HEADER_LEN + (blob.len() - HEADER_LEN) / 2;
        assert!(decompress(&blob[..cut]).is_err(), "truncation went unnoticed");
    }

    #[test]
    fn rejects_empty_payload() {
        let blob = header(METHOD_CM, 6, 1000, 0);
        assert!(decompress(&blob).is_err());
    }

    #[test]
    fn rejects_truncated_stored() {
        let mut blob = header(METHOD_STORED, 6, 100, 0);
        blob.extend_from_slice(b"short");
        assert_eq!(decompress(&blob).unwrap_err(), "truncated stored archive");
    }

    #[test]
    fn rejects_unknown_method() {
        let blob = header(9, 6, 0, 0);
        assert!(decompress(&blob).unwrap_err().contains("unknown compression method"));
    }

    #[test]
    fn header_fuzz_never_panics() {
        // Every archive-shaped blob must come back as Ok or Err, never a panic
        // and never an unbounded run.
        let mut s = 0x1234_5678_9abc_def0u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..300 {
            let mut blob = MAGIC.to_vec();
            blob.push(VERSION);
            blob.push((rnd() % 3) as u8);
            blob.push((rnd() % 12) as u8);
            blob.extend_from_slice(&(rnd() % 4096).to_le_bytes());
            blob.extend_from_slice(&(rnd() as u32).to_le_bytes());
            let n = (rnd() % 64) as usize;
            for _ in 0..n {
                blob.push(rnd() as u8);
            }
            let _ = decompress(&blob);
        }
    }

    #[test]
    fn progress_is_monotonic_and_complete() {
        let d: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
        let mut last = 0;
        let blob = compress_with_progress(&d, 2, |done| {
            assert!(done >= last);
            last = done;
        });
        assert_eq!(last, d.len());
        let mut last = 0;
        let back = decompress_with_progress(&blob, |done, total| {
            assert_eq!(total, d.len());
            assert!(done >= last);
            last = done;
        })
        .unwrap();
        assert_eq!(last, d.len());
        assert_eq!(back, d);
    }
}
