//! The JPEG-LS encoding procedure of ITU-T T.87, Annex A.
//!
//! The encoder and decoder share the contexts, predictor, and reconstruction
//! logic in [`super::coding`]. Both update their state from reconstructed
//! samples, which keeps near-lossless encoding and decoding synchronized.

use super::bitwriter::BitWriter;
use super::coding::{
    map_error, marker, predict, CodingParameters, Context, GradientTable, RunContext, Traits,
    CONTEXT_COUNT, J,
};
use super::JpegLsError;

/// The sample precision used by FLIR and by this encoder.
const BITS_PER_SAMPLE: u32 = 16;

/// Largest sample value at [`BITS_PER_SAMPLE`].
const MAX_VALUE: i32 = 65535;

/// Dimensions and error tolerance for a JPEG-LS image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Image width in samples.
    pub width: usize,
    /// Image height in samples.
    pub height: usize,
    /// Near-lossless error bound: no decoded sample will differ from the
    /// original by more than this. `0` codes losslessly.
    pub near: u16,
}

impl EncodeOptions {
    /// Creates options for a lossless image of the given dimensions.
    pub fn lossless(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            near: 0,
        }
    }

    /// Near-lossless coding with an error bound of `near` counts.
    pub fn near_lossless(width: usize, height: usize, near: u16) -> Self {
        Self {
            width,
            height,
            near,
        }
    }

    fn validate(&self, samples: usize) -> Result<(), JpegLsError> {
        if self.width == 0 || self.height == 0 {
            return Err(JpegLsError::InvalidHeader("zero image dimension"));
        }
        if self.width > u16::MAX as usize || self.height > u16::MAX as usize {
            return Err(JpegLsError::Unsupported("oversize image dimensions"));
        }
        if i32::from(self.near) > MAX_VALUE {
            return Err(JpegLsError::InvalidHeader("NEAR out of range"));
        }
        if samples != self.width * self.height {
            return Err(JpegLsError::InvalidHeader("sample count"));
        }
        Ok(())
    }
}

/// Encodes 16-bit images as JPEG-LS streams.
///
/// The encoder reuses its line buffers, statistics, and output buffer between
/// calls. After the first frame, a stream of same-sized frames normally needs
/// no further allocations.
///
/// ```
/// use csq::jpegls::{EncodeOptions, JpegLsEncoder};
///
/// let image: Vec<u16> = (0..64 * 32).map(|i| (i % 800) as u16).collect();
/// let mut encoder = JpegLsEncoder::new();
/// let stream = encoder.encode(&image, EncodeOptions::lossless(64, 32))?;
///
/// let (info, decoded) = csq::jpegls::decode(stream)?;
/// assert_eq!(info.width, 64);
/// assert_eq!(decoded, image);
/// # Ok::<(), csq::jpegls::JpegLsError>(())
/// ```
pub struct JpegLsEncoder {
    contexts: Vec<Context>,
    run_contexts: [RunContext; 2],
    /// Previous reconstructed row, with one extra neighbour on each side.
    prev_line: Vec<u16>,
    /// Current reconstructed row, using the same layout as `prev_line`.
    curr_line: Vec<u16>,
    run_index: usize,
    writer: BitWriter,
    gradients: GradientTable,
}

impl Default for JpegLsEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegLsEncoder {
    /// Creates an encoder with empty scratch buffers.
    pub fn new() -> Self {
        Self {
            contexts: Vec::new(),
            run_contexts: [RunContext::new(0, 0), RunContext::new(0, 1)],
            prev_line: Vec::new(),
            curr_line: Vec::new(),
            run_index: 0,
            writer: BitWriter::new(),
            gradients: GradientTable::new(),
        }
    }

    /// Encodes a row-major image and returns its complete JPEG-LS stream.
    ///
    /// The returned slice is valid until the next call to `encode`. Copy it if
    /// it must be kept longer.
    pub fn encode(
        &mut self,
        samples: &[u16],
        options: EncodeOptions,
    ) -> Result<&[u8], JpegLsError> {
        options.validate(samples.len())?;

        let near = i32::from(options.near);
        let params = CodingParameters::defaults(MAX_VALUE, near);
        let traits = Traits::new(params, near);
        self.gradients.prepare(&traits);

        self.writer.reset();
        // One byte per sample is usually enough to avoid growing the output
        // buffer while encoding.
        self.writer.reserve(samples.len() + 64);
        write_headers(&mut self.writer, &options, &params);

        self.reset_state(&traits, options.width);
        for row in samples.chunks_exact(options.width) {
            std::mem::swap(&mut self.prev_line, &mut self.curr_line);

            // At the left edge, Ra is the sample above. At the right edge, Rd
            // repeats Rb (T.87 § A.2).
            self.curr_line[0] = self.prev_line[1];
            let stride = options.width + 2;
            self.prev_line[stride - 1] = self.prev_line[stride - 2];

            self.encode_line(&traits, row);
        }

        self.writer.finish();
        self.writer.push_raw(&[0xff, marker::EOI]);

        Ok(self.writer.as_slice())
    }

    fn reset_state(&mut self, traits: &Traits, width: usize) {
        let a_init = traits.initial_a();

        self.contexts.clear();
        self.contexts.resize(CONTEXT_COUNT, Context::new(a_init));
        self.run_contexts = [RunContext::new(a_init, 0), RunContext::new(a_init, 1)];
        self.run_index = 0;

        let stride = width + 2;
        self.prev_line.clear();
        self.prev_line.resize(stride, 0);
        self.curr_line.clear();
        self.curr_line.resize(stride, 0);
    }

    fn encode_line(&mut self, traits: &Traits, row: &[u16]) {
        let width = row.len();
        let mut x = 0usize;

        while x < width {
            // Column x is stored at x + 1, making curr_line[x] its left
            // neighbour Ra.
            let ra = i32::from(self.curr_line[x]);
            let rb = i32::from(self.prev_line[x + 1]);
            let rc = i32::from(self.prev_line[x]);
            let rd = i32::from(self.prev_line[x + 2]);

            let q1 = self.gradients.quantize(rd - rb);
            let q2 = self.gradients.quantize(rb - rc);
            let q3 = self.gradients.quantize(rc - ra);

            if q1 == 0 && q2 == 0 && q3 == 0 {
                x = self.encode_run(traits, row, x, ra);
            } else {
                let reconstructed =
                    self.encode_regular(traits, q1, q2, q3, ra, rb, rc, i32::from(row[x]));
                self.curr_line[x + 1] = reconstructed as u16;
                x += 1;
            }
        }
    }

    /// Encodes one non-run sample using regular mode (T.87 § A.4 to A.6).
    ///
    /// Returns the reconstructed value used to predict later samples.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn encode_regular(
        &mut self,
        traits: &Traits,
        q1: i32,
        q2: i32,
        q3: i32,
        ra: i32,
        rb: i32,
        rc: i32,
        sample: i32,
    ) -> i32 {
        let qs = 81 * q1 + 9 * q2 + q3;
        let sign = if qs < 0 { -1 } else { 1 };
        let context = &mut self.contexts[qs.unsigned_abs() as usize];

        let px = traits.correct_prediction(predict(ra, rb, rc) + sign * context.c);

        let k = context.golomb_k();
        let err = traits.modulo_range(traits.quantize_error(sign * (sample - px)));

        // The decoder applies the same XOR to recover the original error.
        let correction = if k == 0 {
            context.error_correction(traits.near)
        } else {
            0
        };
        encode_value(
            &mut self.writer,
            k,
            map_error(err ^ correction),
            traits.limit,
            traits.qbpp,
        );
        context.update(err, traits.near, traits.reset);

        // In lossless mode, reconstruction always equals the input sample.
        if traits.near == 0 {
            return sample;
        }
        traits.reconstruct(px, err * sign)
    }

    /// Encodes a sequence close to `Ra`, followed by an optional interruption
    /// sample (T.87 § A.7).
    fn encode_run(&mut self, traits: &Traits, row: &[u16], x: usize, ra: i32) -> usize {
        let width = row.len();

        let count = if traits.near == 0 {
            // In lossless mode, direct equality is sufficient and cheaper.
            let value = ra as u16;
            row[x..]
                .iter()
                .take_while(|&&sample| sample == value)
                .count()
        } else {
            row[x..]
                .iter()
                .take_while(|&&sample| (i32::from(sample) - ra).abs() <= traits.near)
                .count()
        };
        self.curr_line[x + 1..x + 1 + count].fill(ra as u16);

        // The decoder already knows the row width, so an end-of-row run needs
        // no terminator.
        let end_of_line = x + count == width;
        let mut remaining = count;
        while remaining >= (1usize << J[self.run_index]) {
            self.writer.push_bits(1, 1);
            remaining -= 1 << J[self.run_index];
            if self.run_index < 31 {
                self.run_index += 1;
            }
        }
        if end_of_line {
            if remaining > 0 {
                self.writer.push_bits(1, 1);
            }
        } else {
            self.writer.push_bits(0, 1);
            let bits = J[self.run_index];
            if bits > 0 {
                self.writer.push_bits(remaining as u32, bits);
            }
        }

        let mut x = x + count;
        if !end_of_line {
            let rb = i32::from(self.prev_line[x + 1]);
            let sample = i32::from(row[x]);

            let reconstructed = if (ra - rb).abs() <= traits.near {
                let err = traits.modulo_range(traits.quantize_error(sample - ra));
                self.encode_run_interruption(traits, err, 1);
                traits.reconstruct(ra, err)
            } else {
                let sign = if rb - ra >= 0 { 1 } else { -1 };
                let err = traits.modulo_range(traits.quantize_error(sign * (sample - rb)));
                self.encode_run_interruption(traits, err, 0);
                traits.reconstruct(rb, err * sign)
            };
            self.curr_line[x + 1] = reconstructed as u16;
            x += 1;

            if self.run_index > 0 {
                self.run_index -= 1;
            }
        }

        x
    }

    #[inline]
    fn encode_run_interruption(&mut self, traits: &Traits, err: i32, ri_type: usize) {
        let limit = traits.limit - J[self.run_index] as i32 - 1;
        let context = &mut self.run_contexts[ri_type];
        let k = context.golomb_k();

        // Store the sign as the parity of the mapped magnitude
        // (T.87 § A.7.2.2).
        let mapped = 2 * err.abs() - ri_type as i32 - context.error_map(err, k);
        debug_assert!(
            mapped >= 0,
            "run interruption samples always differ by more than NEAR"
        );

        encode_value(&mut self.writer, k, mapped, limit, traits.qbpp);
        context.update(err, mapped, traits.reset);
    }
}

/// Writes a non-negative value using JPEG-LS's length-limited Golomb code
/// (T.87 § A.5.3).
#[inline]
fn encode_value(writer: &mut BitWriter, k: u32, mapped: i32, limit: i32, qbpp: u32) {
    let high_bits = mapped >> k;
    let escape_at = limit - qbpp as i32 - 1;

    if high_bits < escape_at {
        writer.push_unary(high_bits as u32);
        if k > 0 {
            writer.push_bits(mapped as u32 & ((1u32 << k) - 1), k);
        }
    } else {
        // Use the fixed-width escape form when the unary prefix would be too
        // long.
        writer.push_unary(escape_at as u32);
        writer.push_bits((mapped - 1) as u32 & ((1u32 << qbpp) - 1), qbpp);
    }
}

/// Writes the JPEG-LS start marker, frame header, coding parameters, and scan
/// header.
///
/// The LSE segment contains the standard defaults and is technically optional.
/// It is included for compatibility with software that expects FLIR-style
/// output.
fn write_headers(writer: &mut BitWriter, options: &EncodeOptions, params: &CodingParameters) {
    let be16 = |value: i32| (value as u16).to_be_bytes();
    let [h1, h0] = be16(options.height as i32);
    let [w1, w0] = be16(options.width as i32);
    let [m1, m0] = be16(params.max_value);
    let [t1a, t1b] = be16(params.t1);
    let [t2a, t2b] = be16(params.t2);
    let [t3a, t3b] = be16(params.t3);
    let [r1, r0] = be16(params.reset);

    writer.push_raw(&[
        0xff,
        marker::SOI,
        // SOF55: precision, height, width, one component with no subsampling.
        0xff,
        marker::SOF55,
        0,
        11,
        BITS_PER_SAMPLE as u8,
        h1,
        h0,
        w1,
        w0,
        1,
        1,
        0x11,
        0,
        // LSE: preset coding parameters.
        0xff,
        marker::LSE,
        0,
        13,
        1,
        m1,
        m0,
        t1a,
        t1b,
        t2a,
        t2b,
        t3a,
        t3b,
        r1,
        r0,
        // SOS: one component, NEAR, no interleaving.
        0xff,
        marker::SOS,
        0,
        8,
        1,
        1,
        0,
        options.near as u8,
        0,
        0,
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpegls::decode;

    fn round_trip(image: &[u16], options: EncodeOptions) -> Vec<u16> {
        let mut encoder = JpegLsEncoder::new();
        let stream = encoder.encode(image, options).expect("encoding failed");
        let (info, decoded) = decode(stream).expect("decoding failed");
        assert_eq!((info.width, info.height), (options.width, options.height));
        assert_eq!(info.near, options.near);
        assert!(!info.is_truncated());
        decoded
    }

    /// Creates deterministic noise that is difficult to compress.
    fn noise(width: usize, height: usize) -> Vec<u16> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..width * height)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u16
            })
            .collect()
    }

    /// Creates a smooth image with local detail, similar to thermal data.
    fn scene(width: usize, height: usize) -> Vec<u16> {
        (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let base = 9000 + (x * 3 + y * 7) as u16;
                    if (x / 8 + y / 8) % 5 == 0 {
                        base + 400
                    } else {
                        base
                    }
                })
            })
            .collect()
    }

    #[test]
    fn lossless_round_trip_is_exact() {
        for (width, height) in [(1, 1), (1, 17), (17, 1), (64, 48), (65, 33)] {
            let image = scene(width, height);
            assert_eq!(
                round_trip(&image, EncodeOptions::lossless(width, height)),
                image,
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn lossless_round_trip_survives_noise() {
        let image = noise(97, 43);
        assert_eq!(
            round_trip(&image, EncodeOptions::lossless(97, 43)),
            image,
            "entropy coding must be exact even when it cannot compress"
        );
    }

    #[test]
    fn flat_images_use_run_mode_and_compress_hard() {
        let image = vec![12_345u16; 256 * 256];
        let mut encoder = JpegLsEncoder::new();
        let stream = encoder
            .encode(&image, EncodeOptions::lossless(256, 256))
            .unwrap();
        assert!(
            stream.len() < 512,
            "a constant image should collapse to almost nothing, got {} bytes",
            stream.len()
        );

        let (_, decoded) = decode(stream).unwrap();
        assert_eq!(decoded, image);
    }

    #[test]
    fn near_lossless_stays_within_its_bound() {
        for near in [1u16, 4, 5, 16] {
            let image = scene(80, 60);
            let decoded = round_trip(&image, EncodeOptions::near_lossless(80, 60, near));
            for (i, (&original, &back)) in image.iter().zip(&decoded).enumerate() {
                let error = i32::from(original) - i32::from(back);
                assert!(
                    error.abs() <= i32::from(near),
                    "near {near}: sample {i} moved by {error}"
                );
            }
        }
    }

    #[test]
    fn near_lossless_beats_lossless_on_smooth_data() {
        let image = scene(128, 128);
        let mut encoder = JpegLsEncoder::new();
        let lossless = encoder
            .encode(&image, EncodeOptions::lossless(128, 128))
            .unwrap()
            .len();
        let near = encoder
            .encode(&image, EncodeOptions::near_lossless(128, 128, 8))
            .unwrap()
            .len();
        assert!(near < lossless, "{near} bytes vs {lossless} lossless");
    }

    #[test]
    fn extreme_sample_values_round_trip() {
        // Exercise prediction clamping at both ends of the sample range.
        let image = vec![0u16, 65535, 0, 65535, 32768, 1, 65534, 0];
        assert_eq!(round_trip(&image, EncodeOptions::lossless(4, 2)), image);
    }

    #[test]
    fn the_encoder_can_be_reused_without_state_leaking() {
        let mut encoder = JpegLsEncoder::new();
        let first = scene(40, 30);
        let expected = encoder
            .encode(&first, EncodeOptions::lossless(40, 30))
            .unwrap()
            .to_vec();

        let _ = encoder.encode(&noise(11, 13), EncodeOptions::lossless(11, 13));
        let again = encoder
            .encode(&first, EncodeOptions::lossless(40, 30))
            .unwrap();

        assert_eq!(again, expected, "a reused encoder must be deterministic");
    }

    #[test]
    fn rejects_impossible_geometry() {
        let mut encoder = JpegLsEncoder::new();
        assert!(encoder.encode(&[], EncodeOptions::lossless(0, 4)).is_err());
        assert!(encoder
            .encode(&[1, 2, 3], EncodeOptions::lossless(2, 2))
            .is_err());
    }

    #[test]
    fn writes_the_stream_flir_writes() {
        // Compare the header with a real T1020 frame.
        let mut encoder = JpegLsEncoder::new();
        let stream = encoder
            .encode(
                &vec![0u16; 1024 * 768],
                EncodeOptions::near_lossless(1024, 768, 5),
            )
            .unwrap();
        let expected = [
            0xff, 0xd8, // SOI
            0xff, 0xf7, 0x00, 0x0b, 0x10, 0x03, 0x00, 0x04, 0x00, 0x01, 0x01, 0x11,
            0x00, // SOF55
            0xff, 0xf8, 0x00, 0x0d, 0x01, 0xff, 0xff, 0x00, 0x21, 0x00, 0x5c, 0x01, 0x37, 0x00,
            0x40, // LSE
            0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x05, 0x00, 0x00, // SOS
        ];
        assert_eq!(&stream[..expected.len()], &expected);
        assert_eq!(&stream[stream.len() - 2..], &[0xff, 0xd9]);
    }
}
