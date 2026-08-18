//! Context-mixing model.
//!
//! One bit at a time, several independent context models each output a
//! probability. The probabilities are converted to the logistic domain
//! (`stretch`), combined by a gated linear mixer whose weights are trained
//! online, then refined by two adaptive probability maps (SSE stages).
//!
//! Models used:
//!   - order 0 (partial byte only)
//!   - orders 1, 2, 3, 4, 6 over the byte history
//!   - a word model (hash of the current alphanumeric run)
//!   - a match model predicting the continuation of the longest recent match

// ---------------------------------------------------------------------------
// Logistic helpers
// ---------------------------------------------------------------------------

const SQUASH_T: [i32; 33] = [
    1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348,
    3607, 3785, 3901, 3975, 4024, 4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094,
];

/// Inverse of `stretch`: maps a logistic value in [-2047, 2047] to a 12-bit
/// probability.
#[inline]
pub fn squash(d: i32) -> i32 {
    let d = d.clamp(-2047, 2047);
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (SQUASH_T[i] * (128 - w) + SQUASH_T[i + 1] * w + 64) >> 7
}

fn stretch_table() -> &'static [i16; 4096] {
    use std::sync::OnceLock;
    static T: OnceLock<[i16; 4096]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i16; 4096];
        let mut pi = 0usize;
        // squash is monotonic, so invert it by sweeping the logistic domain.
        for x in -2047..=2047i32 {
            let v = squash(x) as usize;
            if v >= pi {
                t[pi..=v].fill(x as i16);
            }
            pi = v + 1;
        }
        t[pi..].fill(2047);
        t
    })
}

/// Maps a 12-bit probability to the logistic domain.
#[inline]
pub fn stretch(p: i32) -> i32 {
    stretch_table()[p.clamp(0, 4095) as usize] as i32
}

// ---------------------------------------------------------------------------
// Adaptive bit counters
// ---------------------------------------------------------------------------

// A counter packs a 16-bit probability, a 10-bit observation count and a
// 6-bit tag: `p:16 | count:10 | tag:6`. The count drives the adaptation rate,
// fast while the context is fresh and slow (hence stable) once it has seen
// enough bits. The tag is a fingerprint of the context that owns the slot; on
// a hashed table it lets an unrelated context that collided be detected and
// the slot recycled, instead of the two contexts poisoning each other.
const COUNT_LIMIT: u32 = 255;
const TAG_MASK: u32 = 0x3f;
const CTR_INIT: u32 = 0x8000_0000;

fn recip_table() -> &'static [u32; 1026] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 1026]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 1026];
        for (n, e) in t.iter_mut().enumerate() {
            *e = 65536 / (n as u32 + 2);
        }
        t
    })
}

#[inline]
fn ctr_p12(v: u32) -> i32 {
    (v >> 20) as i32
}

/// Read a slot from a hashed table, recycling it if it belongs to another
/// context.
#[inline]
fn ctr_get_tagged(v: &mut u32, tag: u32) -> i32 {
    if (*v & TAG_MASK) != tag {
        *v = CTR_INIT | tag;
    }
    (*v >> 20) as i32
}

#[inline]
fn ctr_update(v: &mut u32, bit: u32, limit: u32) {
    let p = (*v >> 16) as i32;
    let n = (*v >> 6) & 0x3ff;
    let target = if bit != 0 { 65535 } else { 0 };
    let rate = recip_table()[n as usize] as i64;
    let np = p + (((target - p) as i64 * rate) >> 16) as i32;
    let nn = if n < limit { n + 1 } else { n };
    *v = ((np.clamp(0, 65535) as u32) << 16) | (nn << 6) | (*v & TAG_MASK);
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

/// Gated linear mixer in the logistic domain. One weight vector per selection
/// context; weights are 16.16 fixed point and trained by online gradient
/// descent on coding loss.
struct Mixer {
    n: usize,
    w: Vec<i32>,
    base: usize,
    pr: i32,
    lr: i32,
}

/// Learning rates, expressed as the right shift applied to `input * error`.
/// Larger means slower. Tuned on a mixed text/binary corpus.
const MIX_LR: i32 = 9;
const MIX2_LR: i32 = 13;

/// Weight magnitude ceiling, far above anything training reaches in practice.
const W_LIMIT: i32 = 1 << 24;

impl Mixer {
    fn new(n: usize, contexts: usize) -> Self {
        Self::with_lr(n, contexts, MIX_LR)
    }

    fn with_lr(n: usize, contexts: usize, lr: i32) -> Self {
        Mixer {
            n,
            // Start at 1/n each so the initial mix is a plain average.
            w: vec![65536 / n as i32; n * contexts],
            base: 0,
            pr: 2048,
            lr,
        }
    }

    #[inline]
    fn mix(&mut self, inputs: &[i32], cx: usize) -> i32 {
        debug_assert_eq!(inputs.len(), self.n);
        debug_assert!((cx + 1) * self.n <= self.w.len(), "mixer context {cx} out of range");
        self.base = cx * self.n;
        let w = &self.w[self.base..self.base + self.n];
        let mut dot: i64 = 0;
        for i in 0..self.n {
            dot += inputs[i] as i64 * w[i] as i64;
        }
        self.pr = squash((dot >> 16) as i32);
        self.pr
    }

    #[inline]
    fn update(&mut self, inputs: &[i32], bit: u32) {
        let err = ((bit as i32) << 12) - self.pr;
        let w = &mut self.w[self.base..self.base + self.n];
        for i in 0..self.n {
            // Gradient descent is self-correcting, so the clamp practically
            // never binds; it is here so that a pathological input cannot walk
            // a weight into i32 overflow.
            w[i] = (w[i] + ((inputs[i] * err) >> self.lr)).clamp(-W_LIMIT, W_LIMIT);
        }
    }
}

// ---------------------------------------------------------------------------
// Adaptive probability map (SSE)
// ---------------------------------------------------------------------------

/// Refines a probability given a small context, by interpolating in a learned
/// table over the logistic domain.
struct Apm {
    t: Vec<u16>,
    idx: usize,
    contexts: usize,
}

impl Apm {
    fn new(contexts: usize) -> Self {
        let mut t = vec![0u16; contexts * 33];
        for c in 0..contexts {
            for j in 0..33 {
                t[c * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16;
            }
        }
        Apm {
            t,
            idx: 0,
            contexts,
        }
    }

    #[inline]
    fn refine(&mut self, pr: i32, cx: usize) -> i32 {
        debug_assert!(cx < self.contexts, "APM context {cx} out of range");
        // `stretch` returns [-2047, 2047], so s is in [1, 4095] and s >> 7 is
        // in [0, 31]: the interpolation reads slots 0..=32 of the row and never
        // spills into the next one.
        let s = stretch(pr) + 2048;
        debug_assert!((1..=4095).contains(&s));
        let w = s & 127;
        let i = cx * 33 + (s >> 7) as usize;
        self.idx = i + (w >> 6) as usize;
        ((self.t[i] as i32 * (128 - w) + self.t[i + 1] as i32 * w) >> 11).clamp(0, 4095)
    }

    #[inline]
    fn update(&mut self, bit: u32, rate: u32) {
        let g = ((bit as i32) << 16) + ((bit as i32) << rate) - (bit as i32) - (bit as i32);
        let v = self.t[self.idx] as i32;
        // The fixed point of this recurrence sits just above u16::MAX, but the
        // increment reaches zero at 65535, so the cast never truncates. Changing
        // `rate` or the table init without rechecking that would break it.
        let nv = v + ((g - v) >> rate);
        debug_assert!((0..=65535).contains(&nv), "APM value {nv} out of u16 range");
        self.t[self.idx] = nv as u16;
    }
}

// ---------------------------------------------------------------------------
// Match model
// ---------------------------------------------------------------------------

const MATCH_MIN: usize = 6;
const MATCH_MAX_LEN: u32 = 65535;

/// Predicts the next bits by following the most recent occurrence of the
/// current order-`MATCH_MIN` context. This is what gives long-range
/// redundancy (repeated blocks, duplicated files) a cheap, near-free encoding.
struct MatchModel {
    ht: Vec<u32>,
    mask: u32,
    ptr: usize,
    len: u32,
    predicted: u8,
    sm: Vec<u32>,
    sm_idx: usize,
    active: bool,
    expected_bit: u32,
}

impl MatchModel {
    fn new(bits: u32) -> Self {
        MatchModel {
            ht: vec![0u32; 1 << bits],
            mask: (1 << bits) - 1,
            ptr: 0,
            len: 0,
            predicted: 0,
            sm: vec![CTR_INIT; 16 * 8 * 2],
            sm_idx: 0,
            active: false,
            expected_bit: 0,
        }
    }

    /// Called after `buf` has been extended with a freshly decoded byte.
    fn update_byte(&mut self, buf: &[u8], h: u32) {
        let pos = buf.len();
        if self.len > 0 {
            // Extend the current match if it still holds.
            if self.ptr < pos && buf[self.ptr] == buf[pos - 1] {
                self.ptr += 1;
                if self.len < MATCH_MAX_LEN {
                    self.len += 1;
                }
            } else {
                self.len = 0;
            }
        }
        if self.len == 0 && pos >= MATCH_MIN {
            let cand = self.ht[(h & self.mask) as usize] as usize;
            if cand > 0 && cand < pos {
                // Measure how far back the candidate agrees with us.
                let mut l = 0usize;
                while l < 32 && l < cand && buf[cand - 1 - l] == buf[pos - 1 - l] {
                    l += 1;
                }
                if l >= MATCH_MIN {
                    self.ptr = cand;
                    self.len = l as u32;
                }
            }
        }
        if pos >= MATCH_MIN {
            self.ht[(h & self.mask) as usize] = pos as u32;
        }
        self.predicted = if self.len > 0 && self.ptr < pos {
            buf[self.ptr]
        } else {
            0
        };
    }

    /// Logistic prediction for the current bit, or 0 when no match applies.
    #[inline]
    fn predict(&mut self, c0: u32, bitpos: u32) -> i32 {
        self.active = false;
        if self.len == 0 {
            return 0;
        }
        // The match only speaks for this bit if the bits of the byte decoded
        // so far still agree with the predicted byte.
        let pb = self.predicted as u32 | 256;
        if (pb >> (8 - bitpos)) != c0 {
            self.len = 0;
            return 0;
        }
        let expected = (pb >> (7 - bitpos)) & 1;
        let lb = (31 - (self.len | 1).leading_zeros()).min(7) * 2
            + if self.len >= 24 { 1 } else { 0 };
        let lb = (lb as usize).min(15);
        self.sm_idx = (lb * 8 + bitpos as usize) * 2 + expected as usize;
        self.active = true;
        self.expected_bit = expected;
        let p = ctr_p12(self.sm[self.sm_idx]);
        stretch(p)
    }

    #[inline]
    fn update_bit(&mut self, bit: u32) {
        if self.active {
            ctr_update(&mut self.sm[self.sm_idx], bit, COUNT_LIMIT);
        }
    }
}

// ---------------------------------------------------------------------------
// Predictor
// ---------------------------------------------------------------------------

// Hashed models: orders 2, 3, 4, 6, 8, the word model and a sparse model.
const HASHED: usize = 7;
const NINPUTS: usize = 1 + 1 + 1 + HASHED + 1; // bias + order0 + order1 + hashed + match
const NMIX: usize = 3; // first-layer mixers, each gated on a different context

#[inline]
fn hash(x: u64, salt: u64) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    h as u32
}

pub struct Predictor {
    // Byte history, shared by every model.
    buf: Vec<u8>,
    hist: u64,
    word: u32,
    c0: u32,   // partial byte with a leading 1 bit
    bitpos: u32,

    // Model tables.
    t0: Vec<u32>,          // order 0: indexed by c0
    t1: Vec<u32>,          // order 1: indexed by (c1, c0)
    th: Vec<Vec<u32>>,     // hashed higher orders
    hmask: u32,
    ctx: [u32; HASHED],    // per-byte hashes
    idx0: usize,
    idx1: usize,
    idxh: [usize; HASHED],

    tags: [u32; HASHED],
    inputs: Vec<i32>,
    l1_out: Vec<i32>,
    l1_cx: [usize; NMIX],
    l2_cx: usize,

    mm: MatchModel,
    mix1: Vec<Mixer>,
    mix2: Mixer,
    apm1: Apm,
    apm2: Apm,
    pr: i32,
}

impl Predictor {
    /// `level` in 0..=9 trades memory for compression ratio.
    pub fn new(level: u8) -> Self {
        let level = level.min(9) as u32;
        let hbits = (16 + level).min(24);
        let mbits = (16 + level).min(22);
        Predictor {
            buf: Vec::new(),
            hist: 0,
            word: 0,
            c0: 1,
            bitpos: 0,
            t0: vec![CTR_INIT; 256],
            t1: vec![CTR_INIT; 256 * 256],
            th: (0..HASHED).map(|_| vec![CTR_INIT; 1 << hbits]).collect(),
            hmask: (1u32 << hbits) - 1,
            ctx: [0; HASHED],
            idx0: 1,
            idx1: 1,
            idxh: [0; HASHED],
            tags: [0; HASHED],
            inputs: Vec::with_capacity(NINPUTS),
            l1_out: Vec::with_capacity(NMIX),
            l1_cx: [0; NMIX],
            l2_cx: 0,
            mm: MatchModel::new(mbits),
            mix1: vec![
                Mixer::new(NINPUTS, 3 * 256), // partial byte + match state (gate 0..2)
                Mixer::new(NINPUTS, 256),  // previous byte
                Mixer::new(NINPUTS, 256),  // match length + bit position
            ],
            mix2: Mixer::with_lr(NMIX, 256, MIX2_LR),
            apm1: Apm::new(256),
            apm2: Apm::new(65536),
            pr: 2048,
        }
    }

    /// Memory footprint of the model tables, in bytes.
    pub fn memory_usage(&self) -> usize {
        (self.t0.len() + self.t1.len() + self.th.iter().map(|t| t.len()).sum::<usize>()) * 4
            + self.mm.ht.len() * 4
            + self.mix1.iter().map(|m| m.w.len()).sum::<usize>() * 4
            + self.mix2.w.len() * 4
            + (self.apm1.t.len() + self.apm2.t.len()) * 2
    }

    /// Probability that the next bit is 1, as a 12-bit value in 1..=4094.
    ///
    /// This is not a pure query: it selects the table slots and mixer weight
    /// sets that [`Predictor::update`] will then train, and it drops a match
    /// that no longer agrees with the partial byte. Call it exactly once per
    /// bit, always followed by `update`. The encoder and the decoder must run
    /// the same sequence of calls or they will diverge.
    #[inline]
    pub fn p(&mut self) -> u32 {
        let c0 = self.c0;
        self.idx0 = c0 as usize;
        let c1 = (self.hist & 0xff) as u32;
        self.idx1 = ((c1 << 8) | (c0 & 0xff)) as usize;
        // c0 is spread by a multiplicative constant so that adjacent partial
        // bytes land far apart in the table.
        let spread = c0.wrapping_mul(0x9E37_79B1);
        for i in 0..HASHED {
            let h = self.ctx[i] ^ spread;
            self.idxh[i] = (h & self.hmask) as usize;
            // Tag from bits the index does not use, so it is independent of it.
            self.tags[i] = (h >> 26) | 1;
        }

        self.inputs.clear();
        self.inputs.push(256); // bias
        self.inputs.push(stretch(ctr_p12(self.t0[self.idx0])));
        self.inputs.push(stretch(ctr_p12(self.t1[self.idx1])));
        for i in 0..HASHED {
            let tag = self.tags[i];
            let p = ctr_get_tagged(&mut self.th[i][self.idxh[i]], tag);
            self.inputs.push(stretch(p));
        }
        let ms = self.mm.predict(c0, self.bitpos);
        self.inputs.push(ms);

        // Gate the mixers on facts that change which models deserve trust:
        // the partial byte, whether a match is speaking, the previous byte,
        // and how long the current match is.
        let gate = if self.mm.active {
            if self.mm.expected_bit != 0 { 1 } else { 2 }
        } else {
            0
        };
        // lenb is in 1..=31 and bitpos in 0..=7, so the third gate stays inside
        // its 256 weight sets.
        let lenb = (32 - (self.mm.len | 1).leading_zeros()).min(31) as usize;
        self.l1_cx = [
            c0 as usize + gate * 256,
            c1 as usize,
            lenb * 8 + self.bitpos as usize,
        ];
        self.l2_cx = c0 as usize;

        self.l1_out.clear();
        for i in 0..NMIX {
            let p = self.mix1[i].mix(&self.inputs, self.l1_cx[i]);
            self.l1_out.push(stretch(p));
        }
        let mut pr = self.mix2.mix(&self.l1_out, self.l2_cx);
        pr = (self.apm1.refine(pr, (c0 & 0xff) as usize) * 3 + pr) >> 2;
        let cx2 = (hash(
            (c0 as u64) | ((self.hist & 0xffff) << 9),
            0xA5,
        ) & 0xffff) as usize;
        pr = (self.apm2.refine(pr, cx2) * 3 + pr) >> 2;
        self.pr = pr.clamp(1, 4094);
        self.pr as u32
    }

    /// Feed back the bit that was actually coded.
    #[inline]
    pub fn update(&mut self, bit: u32) {
        self.apm1.update(bit, 7);
        self.apm2.update(bit, 7);
        self.mix2.update(&self.l1_out, bit);
        for i in 0..NMIX {
            self.mix1[i].update(&self.inputs, bit);
        }
        ctr_update(&mut self.t0[self.idx0], bit, 255);
        ctr_update(&mut self.t1[self.idx1], bit, COUNT_LIMIT);
        for i in 0..HASHED {
            ctr_update(&mut self.th[i][self.idxh[i]], bit, COUNT_LIMIT);
        }
        self.mm.update_bit(bit);

        self.c0 = (self.c0 << 1) | bit;
        self.bitpos += 1;
        if self.bitpos == 8 {
            let byte = (self.c0 & 0xff) as u8;
            self.c0 = 1;
            self.bitpos = 0;
            self.push_byte(byte);
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.buf.push(byte);
        self.hist = (self.hist << 8) | byte as u64;
        self.word = if byte.is_ascii_alphanumeric() {
            (self.word.wrapping_add((byte | 32) as u32)).wrapping_mul(0x3F5B_ADFF)
        } else {
            0
        };
        self.ctx[0] = hash(self.hist & 0xffff, 2);
        self.ctx[1] = hash(self.hist & 0x00ff_ffff, 3);
        self.ctx[2] = hash(self.hist & 0xffff_ffff, 4);
        self.ctx[3] = hash(self.hist & 0x0000_ffff_ffff_ffff, 6);
        self.ctx[4] = hash(self.hist, 8);
        self.ctx[5] = hash(self.word as u64, 7);
        // Sparse context: bytes at distance 1 and 3, skipping distance 2.
        // Picks up fixed-width records and interleaved fields.
        self.ctx[6] = hash((self.hist & 0xff) | ((self.hist >> 16) & 0xff00), 13);
        let mh = hash(self.hist & 0x0000_ffff_ffff_ffff, 11);
        self.mm.update_byte(&self.buf, mh);
    }
}
