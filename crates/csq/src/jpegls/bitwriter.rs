//! Bit writer for JPEG-LS entropy-coded segments.
//!
//! The mirror image of [`BitReader`](super::bitreader::BitReader): bits go out
//! most significant first, and a stuffing zero bit is inserted after every
//! `0xFF` byte (ITU-T T.87 § A.1) so that no marker can appear inside the coded
//! data. Concretely, the byte following a `0xFF` carries only seven payload
//! bits with its top bit forced to zero.
//!
//! Bits accumulate in a 64-bit register and leave it a byte at a time, so the
//! common case — a handful of bits per sample — costs a shift and an or.

/// Accumulates bits and flushes whole bytes into an output buffer.
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Bits are added at the most significant free position of `cache`.
    cache: u64,
    /// Number of valid bits currently held in `cache`.
    valid: u32,
    /// Whether the last byte written out was `0xFF`, so the next one is stuffed.
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

    /// Clears the writer, keeping the output buffer's allocation.
    pub(crate) fn reset(&mut self) {
        self.out.clear();
        self.cache = 0;
        self.valid = 0;
        self.ff_written = false;
    }

    /// The bytes written so far.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.out
    }

    /// Reserves room for `additional` output bytes.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.out.reserve(additional);
    }

    /// Appends the low `count` bits of `value`, most significant first.
    #[inline]
    pub(crate) fn push_bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 32);
        if count == 0 {
            return;
        }
        // Draining below 32 valid bits leaves room for any single append.
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

    /// Appends whole bytes verbatim, bypassing the bit cache.
    ///
    /// Marker segments are not part of the entropy-coded data and so are not
    /// bit-stuffed. Each one also restarts the stuffing state, because a
    /// decoder resynchronises on it. Only valid on a byte boundary.
    pub(crate) fn push_raw(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.valid, 0, "markers must start on a byte boundary");
        self.out.extend_from_slice(bytes);
        self.ff_written = false;
    }

    /// Appends `zeros` zero bits followed by a terminating one bit.
    #[inline]
    pub(crate) fn push_unary(&mut self, zeros: u32) {
        let mut left = zeros;
        while left >= 32 {
            self.push_bits(0, 32);
            left -= 32;
        }
        // The value 1 written in `left + 1` bits is `left` zeros then a one.
        self.push_bits(1, left + 1);
    }

    /// How many cached bits the next output byte consumes.
    ///
    /// Seven after a `0xFF`, because that byte's top bit belongs to the
    /// stuffing rule rather than to the data.
    #[inline]
    fn bits_per_byte(&self) -> u32 {
        if self.ff_written {
            7
        } else {
            8
        }
    }

    /// Moves whole bytes out of the cache.
    fn drain(&mut self) {
        while self.valid >= self.bits_per_byte() {
            let byte = if self.ff_written {
                // The byte's top bit stays clear, so it can never be mistaken
                // for a marker.
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

    /// Pads to a byte boundary and flushes everything.
    ///
    /// The padding is zero bits, which a decoder never reads: it has already
    /// decoded its last sample by the time it reaches them.
    pub(crate) fn finish(&mut self) {
        self.drain();
        if self.valid > 0 {
            self.push_bits(0, self.bits_per_byte() - self.valid);
            self.drain();
        }
        debug_assert_eq!(self.valid, 0);
        self.cache = 0;

        // A trailing 0xFF would merge with the marker that follows the scan, so
        // give it the stuffed byte the format expects.
        if self.ff_written {
            self.out.push(0);
            self.ff_written = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BitWriter;
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
        // Eight ones make a 0xFF; the next seven bits must land in a byte whose
        // top bit is clear.
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
        // Enough 0xFF bytes to exercise stuffing in both directions.
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
