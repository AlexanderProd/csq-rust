//! The JPEG-LS decoding procedure of ITU-T T.87, Annex A.
//!
//! The coding model itself — contexts, Golomb parameters, prediction and
//! reconstruction — lives in [`super::coding`], shared with the encoder.

use super::bitreader::BitReader;
use super::coding::{
    marker, predict, unmap_error, CodingParameters, Context, RunContext, Traits, CONTEXT_COUNT,
    DEFAULT_RESET, J,
};
use super::JpegLsError;

/// Properties of a decoded JPEG-LS image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegLsInfo {
    /// Image width in samples.
    pub width: usize,
    /// Image height in samples.
    pub height: usize,
    /// Sample precision in bits (2..=16).
    pub bits_per_sample: u32,
    /// Largest representable sample value.
    pub max_value: u16,
    /// Near-lossless error bound; `0` means the stream is lossless.
    pub near: u16,
    /// How many samples were decoded from real entropy-coded data.
    ///
    /// Less than [`sample_count`](Self::sample_count) when the stream ended
    /// early; the samples past that point are meaningless.
    pub decoded_samples: usize,
}

impl JpegLsInfo {
    /// Total number of samples in the image.
    pub fn sample_count(&self) -> usize {
        self.width * self.height
    }

    /// Whether the entropy-coded data ran out before the image was complete.
    pub fn is_truncated(&self) -> bool {
        self.decoded_samples < self.sample_count()
    }
}

/// A reusable JPEG-LS decoder.
///
/// Holds the line buffers and context statistics so that decoding a sequence of
/// same-sized frames does not allocate after the first frame.
pub struct JpegLsDecoder {
    allow_truncated: bool,
    contexts: Vec<Context>,
    run_contexts: [RunContext; 2],
    /// Previous reconstructed line, offset by one so index `x` maps to `x + 1`
    /// and slot `0` holds the `Rc` neighbour of the first column.
    prev_line: Vec<u16>,
    /// Current reconstructed line, same layout as `prev_line`.
    curr_line: Vec<u16>,
    run_index: usize,
    /// Set when the coded data stopped making sense, either because it ran out
    /// or because it decoded something impossible.
    data_lost: bool,
}

impl Default for JpegLsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegLsDecoder {
    /// Whether to return a partially decoded image instead of failing when the
    /// entropy-coded data ends early.
    ///
    /// Off by default: a truncated frame decodes into plausible-looking but
    /// meaningless pixels, which is the wrong default for measurement data.
    /// Turn it on to salvage what a damaged recording still holds, and check
    /// [`JpegLsInfo::is_truncated`] on the result.
    pub fn set_allow_truncated(&mut self, allow: bool) {
        self.allow_truncated = allow;
    }

    /// Creates a decoder with empty scratch buffers.
    pub fn new() -> Self {
        Self {
            allow_truncated: false,
            contexts: Vec::new(),
            run_contexts: [RunContext::new(0, 0), RunContext::new(0, 1)],
            prev_line: Vec::new(),
            curr_line: Vec::new(),
            run_index: 0,
            data_lost: false,
        }
    }

    /// Decodes `data` into `out`, replacing its previous contents.
    ///
    /// `out` keeps its allocation across calls, so reusing the same buffer for
    /// every frame of a CSQ file avoids per-frame allocation entirely.
    pub fn decode_into(
        &mut self,
        data: &[u8],
        out: &mut Vec<u16>,
    ) -> Result<JpegLsInfo, JpegLsError> {
        let header = parse_headers(data)?;
        let traits = Traits::new(header.params, header.near);

        let width = header.width;
        let height = header.height;
        let sample_count = width
            .checked_mul(height)
            .ok_or(JpegLsError::InvalidHeader("image dimensions overflow"))?;

        out.clear();
        out.resize(sample_count, 0);

        self.reset_state(&traits, width);

        let mut reader = BitReader::new(&data[header.scan_start..]);
        let mut decoded_samples = sample_count;

        for row in 0..height {
            std::mem::swap(&mut self.prev_line, &mut self.curr_line);

            // Ra of the first column is the sample directly above it, and Rd of
            // the last column repeats Rb (T.87 § A.2).
            self.curr_line[0] = self.prev_line[1];
            let stride = width + 2;
            self.prev_line[stride - 1] = self.prev_line[stride - 2];

            self.decode_line(&mut reader, &traits, width)?;

            let start = row * width;
            out[start..start + width].copy_from_slice(&self.curr_line[1..=width]);

            // The reader only reports padding once the decoder has actually
            // consumed it, so this fires on the first row that is not backed by
            // real data rather than merely near the end of the input.
            if (reader.used_padding() || self.data_lost) && decoded_samples == sample_count {
                decoded_samples = start;
                if !self.allow_truncated {
                    return Err(JpegLsError::UnexpectedEndOfScan {
                        decoded: decoded_samples,
                        expected: sample_count,
                    });
                }
            }
        }

        Ok(JpegLsInfo {
            width,
            height,
            bits_per_sample: header.bits_per_sample,
            max_value: traits.max_value as u16,
            near: header.near as u16,
            decoded_samples,
        })
    }

    fn reset_state(&mut self, traits: &Traits, width: usize) {
        let a_init = traits.initial_a();

        self.contexts.clear();
        self.contexts.resize(CONTEXT_COUNT, Context::new(a_init));
        self.run_contexts = [RunContext::new(a_init, 0), RunContext::new(a_init, 1)];
        self.run_index = 0;
        self.data_lost = false;

        let stride = width + 2;
        self.prev_line.clear();
        self.prev_line.resize(stride, 0);
        self.curr_line.clear();
        self.curr_line.resize(stride, 0);
    }

    fn decode_line(
        &mut self,
        reader: &mut BitReader<'_>,
        traits: &Traits,
        width: usize,
    ) -> Result<(), JpegLsError> {
        let mut x = 0usize;

        while x < width {
            // Buffer slot `x + 1` holds column `x`, so `curr_line[x]` is Ra.
            let ra = i32::from(self.curr_line[x]);
            let rb = i32::from(self.prev_line[x + 1]);
            let rc = i32::from(self.prev_line[x]);
            let rd = i32::from(self.prev_line[x + 2]);

            let q1 = traits.quantize(rd - rb);
            let q2 = traits.quantize(rb - rc);
            let q3 = traits.quantize(rc - ra);

            if q1 == 0 && q2 == 0 && q3 == 0 {
                x = self.decode_run(reader, traits, width, x, ra)?;
            } else {
                let sample = self.decode_regular(reader, traits, q1, q2, q3, ra, rb, rc);
                self.curr_line[x + 1] = sample as u16;
                x += 1;
            }
        }

        Ok(())
    }

    /// Regular-mode sample decoding (T.87 § A.4 to A.6).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn decode_regular(
        &mut self,
        reader: &mut BitReader<'_>,
        traits: &Traits,
        q1: i32,
        q2: i32,
        q3: i32,
        ra: i32,
        rb: i32,
        rc: i32,
    ) -> i32 {
        let qs = 81 * q1 + 9 * q2 + q3;
        let sign = if qs < 0 { -1 } else { 1 };
        let context = &mut self.contexts[qs.unsigned_abs() as usize];

        let px = traits.correct_prediction(predict(ra, rb, rc) + sign * context.c);

        let k = context.golomb_k();
        let mapped = decode_value(reader, k, traits.limit, traits.qbpp);
        let mut err = unmap_error(mapped);
        if k == 0 {
            err ^= context.error_correction(traits.near);
        }
        context.update(err, traits.near, traits.reset);

        traits.reconstruct(px, err * sign)
    }

    /// Run mode: a run of samples equal to `Ra`, optionally followed by a run
    /// interruption sample (T.87 § A.7).
    fn decode_run(
        &mut self,
        reader: &mut BitReader<'_>,
        traits: &Traits,
        width: usize,
        x: usize,
        ra: i32,
    ) -> Result<usize, JpegLsError> {
        let remaining = width - x;
        let mut count = 0usize;

        while reader.read_bit() == 1 {
            let block = 1usize << J[self.run_index];
            let taken = block.min(remaining - count);
            count += taken;
            if taken == block && self.run_index < 31 {
                self.run_index += 1;
            }
            if count == remaining {
                break;
            }
        }

        if count != remaining {
            let bits = J[self.run_index];
            if bits > 0 {
                count += reader.read_bits(bits) as usize;
            }
            if count > remaining {
                // A run longer than the line is impossible in a well-formed
                // stream, so the coded data is no longer usable from here.
                // Clamp and let the caller decide whether to keep going.
                count = remaining;
                self.data_lost = true;
            }
        }

        let run_value = ra as u16;
        for slot in &mut self.curr_line[x + 1..x + 1 + count] {
            *slot = run_value;
        }
        let mut x = x + count;

        if x < width {
            let rb = i32::from(self.prev_line[x + 1]);
            let sample = if (ra - rb).abs() <= traits.near {
                let err = self.decode_run_interruption_error(reader, traits, 1);
                traits.reconstruct(ra, err)
            } else {
                let err = self.decode_run_interruption_error(reader, traits, 0);
                let sign = if rb - ra >= 0 { 1 } else { -1 };
                traits.reconstruct(rb, err * sign)
            };
            self.curr_line[x + 1] = sample as u16;
            x += 1;

            if self.run_index > 0 {
                self.run_index -= 1;
            }
        }

        Ok(x)
    }

    #[inline]
    fn decode_run_interruption_error(
        &mut self,
        reader: &mut BitReader<'_>,
        traits: &Traits,
        ri_type: usize,
    ) -> i32 {
        let limit = traits.limit - J[self.run_index] as i32 - 1;
        let context = &mut self.run_contexts[ri_type];
        let k = context.golomb_k();
        let mapped = decode_value(reader, k, limit, traits.qbpp);
        let err = context.compute_errval(mapped + ri_type as i32, k);
        context.update(err, mapped, traits.reset);
        err
    }
}

/// Limited-length Golomb decoding (T.87 § A.5.3).
#[inline]
fn decode_value(reader: &mut BitReader<'_>, k: u32, limit: i32, qbpp: u32) -> i32 {
    let high_bits = reader.read_unary() as i32;
    if high_bits >= limit - qbpp as i32 - 1 {
        return reader.read_bits(qbpp) as i32 + 1;
    }
    if k == 0 {
        return high_bits;
    }
    (high_bits << k) + reader.read_bits(k) as i32
}

#[derive(Debug)]
struct Header {
    width: usize,
    height: usize,
    bits_per_sample: u32,
    near: i32,
    params: CodingParameters,
    scan_start: usize,
}

/// Parses the marker segments up to and including SOS.
fn parse_headers(data: &[u8]) -> Result<Header, JpegLsError> {
    if data.len() < 2 || data[0] != 0xff || data[1] != marker::SOI {
        return Err(JpegLsError::NotJpegLs);
    }

    let mut pos = 2usize;
    let mut frame: Option<(usize, usize, u32)> = None;
    let mut preset: Option<CodingParameters> = None;

    loop {
        // Markers may be preceded by fill bytes.
        while pos < data.len() && data[pos] == 0xff && data.get(pos + 1) == Some(&0xff) {
            pos += 1;
        }
        if pos + 1 >= data.len() {
            return Err(JpegLsError::Truncated);
        }
        if data[pos] != 0xff {
            return Err(JpegLsError::InvalidHeader("expected a marker"));
        }
        let code = data[pos + 1];
        pos += 2;

        match code {
            marker::SOI => return Err(JpegLsError::InvalidHeader("duplicate SOI")),
            marker::EOI => return Err(JpegLsError::MissingFrameHeader),
            0xd0..=0xd7 | 0x01 => continue, // standalone markers carry no payload
            _ => {}
        }

        let segment = read_segment(data, pos)?;
        let body = segment.body;
        pos = segment.next;

        match code {
            marker::SOF55 => {
                if body.len() < 6 {
                    return Err(JpegLsError::Truncated);
                }
                let precision = u32::from(body[0]);
                let height = usize::from(u16::from_be_bytes([body[1], body[2]]));
                let width = usize::from(u16::from_be_bytes([body[3], body[4]]));
                let components = usize::from(body[5]);

                if !(2..=16).contains(&precision) {
                    return Err(JpegLsError::InvalidHeader("sample precision"));
                }
                if components != 1 {
                    return Err(JpegLsError::Unsupported("multi-component images"));
                }
                if width == 0 {
                    return Err(JpegLsError::InvalidHeader("zero image width"));
                }
                // A height of zero is legal if a DNL segment supplies it later,
                // which CSQ never does.
                if height == 0 {
                    return Err(JpegLsError::Unsupported("height defined by DNL"));
                }
                frame = Some((width, height, precision));
            }
            marker::LSE => {
                if body.is_empty() {
                    return Err(JpegLsError::Truncated);
                }
                match body[0] {
                    // Preset coding parameters.
                    1 => {
                        if body.len() < 11 {
                            return Err(JpegLsError::Truncated);
                        }
                        let read = |i: usize| i32::from(u16::from_be_bytes([body[i], body[i + 1]]));
                        preset = Some(CodingParameters {
                            max_value: read(1),
                            t1: read(3),
                            t2: read(5),
                            t3: read(7),
                            reset: read(9),
                        });
                    }
                    2 | 3 => return Err(JpegLsError::Unsupported("mapping tables")),
                    4 => return Err(JpegLsError::Unsupported("oversize image dimensions")),
                    _ => return Err(JpegLsError::InvalidHeader("LSE id")),
                }
            }
            marker::DNL => return Err(JpegLsError::Unsupported("height defined by DNL")),
            marker::SOS => {
                let (width, height, precision) = frame.ok_or(JpegLsError::MissingFrameHeader)?;
                if body.len() < 4 {
                    return Err(JpegLsError::Truncated);
                }
                let component_count = usize::from(body[0]);
                if component_count != 1 {
                    return Err(JpegLsError::Unsupported("multi-component scans"));
                }
                // body: Ns, (Cs, Tm) per component, NEAR, ILV, Ah/Al
                if body.len() < 1 + 2 * component_count + 3 {
                    return Err(JpegLsError::Truncated);
                }
                let mapping_table = body[2];
                let near = i32::from(body[1 + 2 * component_count]);
                let interleave = body[2 + 2 * component_count];

                if mapping_table != 0 {
                    return Err(JpegLsError::Unsupported("mapping tables"));
                }
                if interleave != 0 {
                    return Err(JpegLsError::Unsupported("interleaved scans"));
                }

                let mut params = preset
                    .unwrap_or_else(|| CodingParameters::defaults((1i32 << precision) - 1, near));
                if params.max_value == 0 {
                    params.max_value = (1i32 << precision) - 1;
                }
                if params.t1 == 0 || params.t2 == 0 || params.t3 == 0 {
                    let defaults = CodingParameters::defaults(params.max_value, near);
                    if params.t1 == 0 {
                        params.t1 = defaults.t1;
                    }
                    if params.t2 == 0 {
                        params.t2 = defaults.t2;
                    }
                    if params.t3 == 0 {
                        params.t3 = defaults.t3;
                    }
                }
                if params.reset == 0 {
                    params.reset = DEFAULT_RESET;
                }
                if near < 0 || near > params.max_value {
                    return Err(JpegLsError::InvalidHeader("NEAR out of range"));
                }

                return Ok(Header {
                    width,
                    height,
                    bits_per_sample: precision,
                    near,
                    params,
                    scan_start: pos,
                });
            }
            _ => {} // APPn, COM and friends are skipped
        }
    }
}

struct Segment<'a> {
    body: &'a [u8],
    next: usize,
}

/// Reads a marker segment's length-prefixed payload starting at `pos`.
fn read_segment(data: &[u8], pos: usize) -> Result<Segment<'_>, JpegLsError> {
    if pos + 2 > data.len() {
        return Err(JpegLsError::Truncated);
    }
    let length = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
    if length < 2 || pos + length > data.len() {
        return Err(JpegLsError::Truncated);
    }
    Ok(Segment {
        body: &data[pos + 2..pos + length],
        next: pos + length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_jpegls_input() {
        assert_eq!(parse_headers(&[]).unwrap_err(), JpegLsError::NotJpegLs);
        assert_eq!(
            parse_headers(&[0x89, 0x50]).unwrap_err(),
            JpegLsError::NotJpegLs
        );
    }
}
