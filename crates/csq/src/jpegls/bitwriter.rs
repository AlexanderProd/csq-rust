//! Bit writer for JPEG-LS entropy-coded segments.
//!
//! This is the counterpart to [`BitReader`](super::bitreader::BitReader). It
//! writes the most significant bit first. After writing `0xFF`, it forces the
//! next bit to zero so the encoded data cannot be mistaken for a JPEG marker
//! (ITU-T T.87 § A.1). As a result, the next byte contains seven data bits
//! instead of eight.
//!
//! Bits are collected in a 64-bit cache before being copied to the output.

/// Accumulates bits and flushes whole bytes into an output buffer.
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Pending bits, stored from the most significant end.
    cache: u64,
    /// Number of pending bits in `cache`.
    valid: u32,
    /// Whether the next output byte must begin with a stuffed zero bit.
    ff_written: bool,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            out: Vec::new(),
            cache: 0,
            valid: 0,
            ff_written: false,
        }
    }

    /// Removes all output and pending bits while reusing the allocated buffer.
    pub(crate) fn reset(&mut self) {
        self.out.clear();
        self.cache = 0;
        self.valid = 0;
        self.ff_written = false;
    }

    /// Returns all bytes written so far.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.out
    }

    /// Reserves space for at least `additional` more output bytes.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.out.reserve(additional);
    }

    /// Writes the lowest `count` bits of `value`, most significant bit first.
    #[inline]
    pub(crate) fn push_bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 32);
        if count == 0 {
            return;
        }
        // Leave enough room for the largest supported write of 32 bits.
        if self.valid + count > 64 {
            self.drain();
        }
        let value = u64::from(value) & ((1u64 << count) - 1);
        self.cache |= value << (64 - self.valid - count);
        self.valid += count;
        if self.valid >= 32 {
            self.drain();
        }
    }

    /// Writes complete bytes without bit stuffing.
    ///
    /// Use this for JPEG marker segments, which are outside the entropy-coded
    /// data. The writer must already be at a byte boundary.
    pub(crate) fn push_raw(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.valid, 0, "markers must start on a byte boundary");
        self.out.extend_from_slice(bytes);
        self.ff_written = false;
    }

    /// Writes a unary code: `zeros` zero bits followed by one bit.
    #[inline]
    pub(crate) fn push_unary(&mut self, zeros: u32) {
        let mut left = zeros;
        while left >= 32 {
            self.push_bits(0, 32);
            left -= 32;
        }
        // Writing 1 across `left + 1` bits produces the remaining zeros and
        // the final one.
        self.push_bits(1, left + 1);
    }

    /// Returns the number of data bits that fit in the next output byte.
    ///
    /// After `0xFF`, one bit is reserved for stuffing, leaving seven.
    #[inline]
    fn bits_per_byte(&self) -> u32 {
        if self.ff_written {
            7
        } else {
            8
        }
    }

    /// Writes enough cached data to leave room for another 32-bit value.
    ///
    /// It writes four bytes at once when none requires stuffing. Otherwise it
    /// switches to the byte-by-byte path.
    fn drain(&mut self) {
        if !self.ff_written && self.valid >= 32 {
            let word = (self.cache >> 32) as u32;
            if !has_ff_byte(word) {
                self.out.extend_from_slice(&word.to_be_bytes());
                self.cache <<= 32;
                self.valid -= 32;
                return;
            }
        }
        self.drain_bytes();
    }

    /// Writes every complete byte currently in the cache, adding stuffed zero
    /// bits where required.
    fn drain_bytes(&mut self) {
        while self.valid >= self.bits_per_byte() {
            let byte = if self.ff_written {
                // The stuffed top bit is zero, so this cannot look like a
                // marker.
                let byte = (self.cache >> 57) as u8;
                self.cache <<= 7;
                self.valid -= 7;
                byte
            } else {
                let byte = (self.cache >> 56) as u8;
                self.cache <<= 8;
                self.valid -= 8;
                byte
            };
            self.ff_written = byte == 0xff;
            self.out.push(byte);
        }
    }

    /// Pads the final byte with zeros and writes all remaining data.
    ///
    /// The decoder stops after the final sample, so it does not read the
    /// padding.
    pub(crate) fn finish(&mut self) {
        self.drain_bytes();
        if self.valid > 0 {
            self.push_bits(0, self.bits_per_byte() - self.valid);
            self.drain_bytes();
        }
        debug_assert_eq!(self.valid, 0);
        self.cache = 0;

        // The stuffing rule also applies when 0xFF is the final data byte.
        // Add its required zero byte before the following marker.
        if self.ff_written {
            self.out.push(0);
            self.ff_written = false;
        }
    }
}

/// Returns whether any of the four bytes in `word` equals `0xFF`.
///
/// This complements the word and then uses the standard zero-byte test.
#[inline]
fn has_ff_byte(word: u32) -> bool {
    let inverted = !word;
    inverted.wrapping_sub(0x0101_0101) & !inverted & 0x8080_8080 != 0
}

#[cfg(test)]
mod tests {
    use super::{has_ff_byte, BitWriter};
    use crate::jpegls::bitreader::BitReader;

    fn written(f: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
        let mut writer = BitWriter::new();
        f(&mut writer);
        writer.finish();
        writer.as_slice().to_vec()
    }

    #[test]
    fn writes_bits_msb_first() {
        let bytes = written(|w| {
            w.push_bits(1, 1);
            w.push_bits(0b011, 3);
            w.push_bits(0b0010, 4);
        });
        assert_eq!(bytes, vec![0b1011_0010]);
    }

    #[test]
    fn stuffs_a_zero_bit_after_ff() {
        // After 0xFF, the next byte starts with a stuffed zero.
        let bytes = written(|w| {
            w.push_bits(0xff, 8);
            w.push_bits(0b111_1111, 7);
        });
        assert_eq!(bytes, vec![0xff, 0b0111_1111]);
    }

    #[test]
    fn never_leaves_a_trailing_ff() {
        let bytes = written(|w| w.push_bits(0xff, 8));
        assert_eq!(
            bytes,
            vec![0xff, 0x00],
            "a scan ending in 0xff would swallow the marker that follows it"
        );
    }

    #[test]
    fn finds_an_ff_byte_in_any_position() {
        assert!(!has_ff_byte(0));
        assert!(!has_ff_byte(0xfefe_fefe));
        assert!(!has_ff_byte(0x7f7f_7f7f));
        for shift in [0, 8, 16, 24] {
            assert!(has_ff_byte(0xff << shift), "0xff at bit {shift}");
            assert!(has_ff_byte(0x0102_0304 | 0xff << shift));
        }
        assert!(has_ff_byte(0xffff_ffff));
    }

    #[test]
    fn whole_words_and_stuffed_bytes_agree() {
        // Exercise both output paths with 0xFF at different byte positions.
        let values: Vec<(u32, u32)> = (0..400u32)
            .map(|i| match i % 5 {
                0 => (0xff, 8),
                1 => (i.wrapping_mul(0x9e37_79b9), 13),
                2 => (0x7fff, 15),
                3 => (i, 3 + i % 29),
                _ => (0xffff_ffff, 1 + i % 32),
            })
            .collect();

        let bytes = written(|w| {
            for &(value, count) in &values {
                w.push_bits(value, count);
            }
        });

        let mut reader = BitReader::new(&bytes);
        for &(value, count) in &values {
            let mask = if count == 32 {
                u32::MAX
            } else {
                (1 << count) - 1
            };
            assert_eq!(
                reader.read_bits(count),
                value & mask,
                "{count} bits of {value:#x}"
            );
        }
        assert!(!reader.used_padding());
        // A stuffed byte always has its top bit cleared.
        for pair in bytes.windows(2) {
            assert!(pair[0] != 0xff || pair[1] & 0x80 == 0, "{pair:02x?}");
        }
    }

    #[test]
    fn raw_bytes_are_never_stuffed() {
        let mut writer = BitWriter::new();
        writer.push_raw(&[0xff, 0xd8]);
        writer.push_bits(0b1010, 4);
        writer.finish();
        writer.push_raw(&[0xff, 0xd9]);
        assert_eq!(writer.as_slice(), &[0xff, 0xd8, 0b1010_0000, 0xff, 0xd9]);
    }

    #[test]
    fn round_trips_through_the_bit_reader() {
        // Include enough 0xFF bytes to test stuffing in writer and reader.
        let values: Vec<(u32, u32)> = (0..64)
            .map(|i: u32| (0xffff_ffff >> (i % 32), 32 - (i % 32)))
            .filter(|(_, count)| *count > 0)
            .collect();

        let bytes = written(|w| {
            for &(value, count) in &values {
                w.push_bits(value, count);
            }
        });

        let mut reader = BitReader::new(&bytes);
        for &(value, count) in &values {
            assert_eq!(reader.read_bits(count), value, "{count} bits of {value:#x}");
        }
        assert!(!reader.used_padding());
    }

    #[test]
    fn unary_codes_round_trip() {
        let lengths = [0u32, 1, 7, 8, 31, 32, 33, 70];
        let bytes = written(|w| {
            for &zeros in &lengths {
                w.push_unary(zeros);
            }
        });

        let mut reader = BitReader::new(&bytes);
        for &zeros in &lengths {
            assert_eq!(reader.read_unary(), zeros);
        }
    }
}
