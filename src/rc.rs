//! Binary arithmetic coder (carry-less range coder, 12-bit probabilities).
//!
//! The encoder and decoder must be driven by exactly the same probability
//! sequence; the model is responsible for that symmetry.

/// Encoder over an in-memory output buffer.
pub struct Encoder {
    x1: u32,
    x2: u32,
    pub out: Vec<u8>,
}

impl Encoder {
    pub fn new(capacity: usize) -> Self {
        Encoder {
            x1: 0,
            x2: 0xffff_ffff,
            out: Vec::with_capacity(capacity),
        }
    }

    /// Encode `bit` given `p` = P(bit == 1) scaled to 12 bits (1..4094).
    #[inline]
    pub fn encode(&mut self, bit: u32, p: u32) {
        debug_assert!(p < 4096);
        let range = self.x2 - self.x1;
        let xmid = self.x1 + (range >> 12) * p + (((range & 0xfff) * p) >> 12);
        debug_assert!(xmid >= self.x1 && xmid < self.x2);
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        // Emit bytes whose value is already settled.
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }

    /// Flush the remaining state; must be called exactly once.
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.out.push((self.x1 >> 24) as u8);
            self.x1 <<= 8;
        }
        self.out
    }
}

/// Decoder over an in-memory input buffer.
pub struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    inp: &'a [u8],
    pos: usize,
    overrun: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(inp: &'a [u8]) -> Self {
        let mut d = Decoder {
            x1: 0,
            x2: 0xffff_ffff,
            x: 0,
            inp,
            pos: 0,
            overrun: 0,
        };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next_byte() as u32;
        }
        d
    }

    /// How many bytes have been read past the end of the input. A well-formed
    /// archive never overruns; anything else is truncated or forged, and the
    /// caller should stop rather than decode an endless stream of zeros.
    pub fn overrun(&self) -> usize {
        self.overrun
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        match self.inp.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                b
            }
            None => {
                self.pos += 1;
                self.overrun += 1;
                0
            }
        }
    }

    /// Decode one bit given `p` = P(bit == 1) scaled to 12 bits.
    #[inline]
    pub fn decode(&mut self, p: u32) -> u32 {
        debug_assert!(p < 4096);
        let range = self.x2 - self.x1;
        let xmid = self.x1 + (range >> 12) * p + (((range & 0xfff) * p) >> 12);
        let bit = if self.x <= xmid { 1 } else { 0 };
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next_byte() as u32;
        }
        bit
    }
}
