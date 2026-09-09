//! Bit reader for JPEG-LS entropy-coded segments.
//!
//! JPEG-LS inserts a stuffing bit after every `0xFF` byte so that a marker can
//! never appear inside the entropy-coded data (ITU-T T.87 § A.1): the byte
//! following `0xFF` carries only seven payload bits, with its most significant
//! bit forced to zero. A `0xFF` followed by a byte with its high bit set is
//! therefore a real marker, and ends the scan.
//!
//! Past the end of the input the reader hands out zero bits so that a decoder
//! walking a truncated stream keeps making progress instead of faulting. Those
//! bits are not real data, so the reader tracks whether any of them have
//! actually been consumed — see [`BitReader::used_padding`]. The distinction
//! matters: the cache is filled ahead of the decoder, so reaching the last byte
//! of input is not the same as having run out of bits to decode with.

/// Reads bits MSB-first (most significant bit first) out of an entropy-coded segment.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Bits are consumed from the most significant end of `cache`.
    cache: u64,
    /// Number of valid bits currently held in `cache`, padding included.
    valid: u32,
    /// How many of those bits came from real input.
    real: u32,
    /// Whether the previously consumed byte was `0xFF`.
    prev_ff: bool,
    /// No more input bytes: the end was reached or a marker was found.
    input_done: bool,
    /// At least one padding bit has been handed to the decoder.
    used_padding: bool,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            cache: 0,
            valid: 0,
            real: 0,
            prev_ff: false,
            input_done: false,
            used_padding: false,
        }
    }

    /// True once the decoder has consumed bits that were not in the input.
    ///
    /// Everything decoded after that point is meaningless, so this is the
    /// signal that a stream was truncated.
    pub(crate) fn used_padding(&self) -> bool {
        self.used_padding
    }

    /// Refills `cache` to at least 57 valid bits.
    #[cold]
    fn fill(&mut self) {
        while self.valid <= 56 {
            if self.input_done || self.pos >= self.data.len() {
                self.input_done = true;
                self.valid += 8;
                continue;
            }
            let byte = self.data[self.pos];
            if self.prev_ff {
                if byte & 0x80 != 0 {
                    // A marker ends the entropy-coded segment.
                    self.input_done = true;
                    self.valid += 8;
                    continue;
                }
                self.cache |= u64::from(byte & 0x7f) << (64 - self.valid - 7);
                self.valid += 7;
                self.real += 7;
                self.prev_ff = false;
            } else {
                self.cache |= u64::from(byte) << (64 - self.valid - 8);
                self.valid += 8;
                self.real += 8;
                self.prev_ff = byte == 0xff;
            }
            self.pos += 1;
        }
    }

    #[inline]
    fn shift_out(&mut self, n: u32) {
        if n > self.real {
            self.used_padding = true;
        }
        self.real = self.real.saturating_sub(n);
        self.cache = if n >= 64 { 0 } else { self.cache << n };
        self.valid -= n;
    }

    #[inline]
    pub(crate) fn read_bit(&mut self) -> u32 {
        if self.valid == 0 {
            self.fill();
        }
        let bit = (self.cache >> 63) as u32;
        self.shift_out(1);
        bit
    }

    /// Reads `n` bits (`n <= 32`) as an unsigned integer.
    #[inline]
    pub(crate) fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        if self.valid < n {
            self.fill();
        }
        let value = (self.cache >> (64 - n)) as u32;
        self.shift_out(n);
        value
    }

    /// Counts zero bits up to and including the terminating one bit, returning
    /// the number of zeros.
    #[inline]
    pub(crate) fn read_unary(&mut self) -> u32 {
        let mut zeros = 0;
        loop {
            if self.valid == 0 {
                self.fill();
            }
            let run = self.cache.leading_zeros().min(self.valid);
            if run < self.valid {
                zeros += run;
                self.shift_out(run + 1);
                return zeros;
            }
            zeros += run;
            self.shift_out(run);
            if self.input_done && self.real == 0 {
                // Only padding is left, so the terminating one bit will never
                // arrive; give up rather than spin.
                return zeros;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    #[test]
    fn reads_bits_msb_first() {
        let data = [0b1011_0010u8, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bits(3), 0b011);
        assert_eq!(r.read_bits(4), 0b0010);
        assert_eq!(r.read_bits(2), 0b01);
        assert!(!r.used_padding());
    }

    #[test]
    fn skips_stuffing_bit_after_ff() {
        // 0xFF then a stuffed byte: only the low seven bits are payload.
        let data = [0xffu8, 0b0101_0101];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(8), 0xff);
        // The stuffed byte contributes 0b101_0101, MSB-first.
        assert_eq!(r.read_bits(7), 0b101_0101);
        assert!(!r.used_padding());
    }

    #[test]
    fn a_marker_ends_the_segment() {
        let data = [0b1010_1010u8, 0xff, 0xd9];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(8), 0b1010_1010);
        assert_eq!(r.read_bits(8), 0xff);
        // Everything from the marker on is padding.
        let _ = r.read_bit();
        assert!(r.used_padding());
    }

    #[test]
    fn reaching_the_last_byte_is_not_running_out_of_bits() {
        // The cache is filled far ahead of the decoder, so a reader that has
        // consumed every input byte may still hold plenty of real bits.
        let data = [0xabu8; 4];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(8), 0xab);
        assert!(!r.used_padding(), "24 real bits are still buffered");
        for _ in 0..3 {
            assert_eq!(r.read_bits(8), 0xab);
        }
        assert!(!r.used_padding(), "exactly the real bits were consumed");
        let _ = r.read_bit();
        assert!(r.used_padding(), "this bit came from padding");
    }

    #[test]
    fn unary_counts_leading_zeros() {
        // 0000_0101 -> five zeros then the terminating one bit.
        let data = [0b0000_0101u8, 0x00];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_unary(), 5);
        assert_eq!(r.read_bit(), 0);
        assert_eq!(r.read_bit(), 1);
    }

    #[test]
    fn unary_spanning_many_bytes_terminates() {
        let data = [0x00u8; 4];
        let mut r = BitReader::new(&data);
        // All-zero input has no terminating bit; the reader must give up.
        let n = r.read_unary();
        assert!(r.used_padding());
        assert!(n >= 32);
    }
}
