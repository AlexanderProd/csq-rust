//! The JPEG-LS coding model of ITU-T T.87, shared by the decoder and encoder.
//!
//! Everything here is symmetric: an encoder and a decoder walking the same
//! image must derive the same contexts, the same Golomb parameters and the
//! same reconstructed samples, or the stream falls apart at the first
//! disagreement. Keeping the model in one place is what makes that
//! structural rather than a matter of two implementations being kept in sync.
//!
//! Symbol names follow the standard so the code can be read side by side with
//! it: `Ra`/`Rb`/`Rc`/`Rd` are the causal neighbours, `Px` the prediction,
//! `A`/`B`/`C`/`N` the per-context statistics, and `J` the run-length order
//! table.

/// Marker codes used by JPEG-LS.
pub(super) mod marker {
    pub const SOI: u8 = 0xd8;
    pub const EOI: u8 = 0xd9;
    pub const SOS: u8 = 0xda;
    pub const SOF55: u8 = 0xf7;
    pub const LSE: u8 = 0xf8;
    pub const DNL: u8 = 0xdc;
}

/// Run-length order table (T.87 Table A.5).
pub(super) const J: [u32; 32] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13,
    14, 15,
];

/// Number of regular-mode contexts (T.87 § A.3.4).
pub(super) const CONTEXT_COUNT: usize = 365;

const MIN_C: i32 = -128;
const MAX_C: i32 = 127;

pub(super) const DEFAULT_RESET: i32 = 64;

/// Per-context statistics for regular mode (T.87 § A.3.4).
#[derive(Clone, Copy)]
pub(super) struct Context {
    a: i32,
    b: i32,
    pub(super) c: i32,
    n: i32,
}

impl Context {
    /// Creates a regular-mode context with the given initial error total.
    pub(super) fn new(a: i32) -> Self {
        Self {
            a,
            b: 0,
            c: 0,
            n: 1,
        }
    }

    /// Chooses the Golomb-Rice parameter `k` from this context's statistics
    /// (T.87 § A.5.1).
    #[inline]
    pub(super) fn golomb_k(&self) -> u32 {
        smallest_shift(self.n, self.a)
    }

    /// Returns the lossless-mode correction used before mapping an error
    /// (T.87 § A.5.2).
    ///
    /// The result is either `0` (leave the error unchanged) or `-1` (invert
    /// every bit). Applying the same XOR again restores the original error.
    #[inline]
    pub(super) fn error_correction(&self, near: i32) -> i32 {
        if near != 0 || 2 * self.b + self.n > 0 {
            0
        } else {
            -1
        }
    }

    /// Updates this context after coding one prediction error.
    ///
    /// This records the error's magnitude and bias, periodically scales the
    /// totals down, and adjusts the prediction correction `C`
    /// (T.87 § A.6.1 and A.6.2).
    #[inline]
    pub(super) fn update(&mut self, err: i32, near: i32, reset: i32) {
        self.a += err.abs();
        self.b += err * (2 * near + 1);

        if self.n == reset {
            self.a >>= 1;
            self.b >>= 1;
            self.n >>= 1;
        }
        self.n += 1;

        if self.b <= -self.n {
            self.b += self.n;
            if self.b <= -self.n {
                self.b = -self.n + 1;
            }
            if self.c > MIN_C {
                self.c -= 1;
            }
        } else if self.b > 0 {
            self.b -= self.n;
            if self.b > 0 {
                self.b = 0;
            }
            if self.c < MAX_C {
                self.c += 1;
            }
        }
    }
}

/// Finds the smallest Golomb parameter `k` for which `n * 2^k >= a`.
///
/// JPEG-LS limits `k` to 31. Comparing the bit lengths of `n` and `a` avoids
/// the loop shown in the standard; one final comparison corrects the result
/// when their leading bits alone are not enough.
#[inline]
fn smallest_shift(n: i32, a: i32) -> u32 {
    debug_assert!(n > 0);
    if n >= a {
        return 0;
    }
    // `a` is the larger, so it has no more leading zeros than `n`, and `n`
    // shifted by the difference is as long as `a` without overflowing.
    let k = n.leading_zeros() - a.leading_zeros();
    let k = if n << k >= a { k } else { k + 1 };
    k.min(31)
}

/// Statistics for the two run-interruption contexts (T.87 § A.7.2).
#[derive(Clone, Copy)]
pub(super) struct RunContext {
    a: i32,
    n: i32,
    nn: i32,
    /// 0 when `|Ra - Rb| > NEAR`, 1 otherwise.
    ri_type: i32,
}

impl RunContext {
    /// Creates one of the two run-interruption contexts.
    ///
    /// `ri_type` is `0` when the interruption sample differs from `Rb` by
    /// more than `NEAR`, and `1` otherwise.
    pub(super) fn new(a: i32, ri_type: i32) -> Self {
        Self {
            a,
            n: 1,
            nn: 0,
            ri_type,
        }
    }

    /// Chooses the Golomb-Rice parameter `k` for run-interruption coding.
    #[inline]
    pub(super) fn golomb_k(&self) -> u32 {
        let temp = self.a + (self.n >> 1) * self.ri_type;
        smallest_shift(self.n, temp)
    }

    /// Returns whether a parity bit of one represents a negative error.
    ///
    /// The meaning depends on `k` and on whether this context has seen mostly
    /// positive or negative errors.
    #[inline]
    fn negative_maps_to_one(&self, k: u32) -> bool {
        k != 0 || 2 * self.nn >= self.n
    }

    /// Maps the sign of `errval` to the parity flag written to the stream
    /// (T.87 § A.7.2.2).
    #[inline]
    pub(super) fn error_map(&self, errval: i32, k: u32) -> i32 {
        let negative_maps_to_one = self.negative_maps_to_one(k);
        let map = match errval {
            e if e < 0 => negative_maps_to_one,
            e if e > 0 => !negative_maps_to_one,
            _ => false,
        };
        i32::from(map)
    }

    /// Restores a signed run-interruption error from its mapped value
    /// (T.87 § A.7.2.2).
    ///
    /// `temp` is the transmitted value plus `RItype`. Its low bit carries the
    /// sign mapping, while the remaining bits carry the error magnitude.
    #[inline]
    pub(super) fn compute_errval(&self, temp: i32, k: u32) -> i32 {
        let map = temp & 1 != 0;
        let errval_abs = (temp + i32::from(map)) / 2;
        if self.negative_maps_to_one(k) == map {
            -errval_abs
        } else {
            errval_abs
        }
    }

    /// Updates this context after coding one run-interruption error.
    ///
    /// `mapped` is the non-negative error value written to or read from the
    /// Golomb coder.
    #[inline]
    pub(super) fn update(&mut self, errval: i32, mapped: i32, reset: i32) {
        if errval < 0 {
            self.nn += 1;
        }
        self.a += (mapped + 1 - self.ri_type) >> 1;
        if self.n == reset {
            self.a >>= 1;
            self.n >>= 1;
            self.nn >>= 1;
        }
        self.n += 1;
    }
}

/// Coding parameters, either taken from an LSE segment or derived from the
/// sample precision (T.87 § C.2.4.1.1).
#[derive(Debug, Clone, Copy)]
pub(super) struct CodingParameters {
    pub(super) max_value: i32,
    pub(super) t1: i32,
    pub(super) t2: i32,
    pub(super) t3: i32,
    pub(super) reset: i32,
}

impl CodingParameters {
    /// Derives the standard JPEG-LS thresholds and reset value for `MAXVAL`
    /// and `NEAR`.
    pub(super) fn defaults(max_value: i32, near: i32) -> Self {
        const BASIC_T1: i32 = 3;
        const BASIC_T2: i32 = 7;
        const BASIC_T3: i32 = 21;

        let (t1, t2, t3) = if max_value >= 128 {
            let factor = (max_value.min(4095) + 128) / 256;
            (
                factor * (BASIC_T1 - 2) + 2 + 3 * near,
                factor * (BASIC_T2 - 3) + 3 + 5 * near,
                factor * (BASIC_T3 - 4) + 4 + 7 * near,
            )
        } else {
            let factor = 256 / (max_value + 1);
            (
                (BASIC_T1 / factor).max(2) + 3 * near,
                (BASIC_T2 / factor).max(3) + 5 * near,
                (BASIC_T3 / factor).max(4) + 7 * near,
            )
        };

        // The thresholds must stay ordered and within range.
        let t1 = t1.clamp(near + 1, max_value);
        let t2 = t2.clamp(t1, max_value);
        let t3 = t3.clamp(t2, max_value);

        Self {
            max_value,
            t1,
            t2,
            t3,
            reset: DEFAULT_RESET,
        }
    }
}

/// Values derived from `MAXVAL` and `NEAR` that drive the Golomb coder
/// (T.87 § A.1).
#[derive(Clone, Copy)]
pub(super) struct Traits {
    pub(super) max_value: i32,
    pub(super) near: i32,
    pub(super) range: i32,
    pub(super) qbpp: u32,
    pub(super) limit: i32,
    pub(super) reset: i32,
    t1: i32,
    t2: i32,
    t3: i32,
}

/// Returns the number of bits needed to represent `n` distinct values.
///
/// In mathematical terms, this is the smallest `x` for which `2^x >= n`.
pub(super) fn ceil_log2(n: i32) -> u32 {
    let mut x = 0;
    while n > (1 << x) {
        x += 1;
    }
    x
}

impl Traits {
    /// Derives the values used while coding a scan from its parameters and
    /// allowed per-sample error.
    pub(super) fn new(params: CodingParameters, near: i32) -> Self {
        let max_value = params.max_value;
        let range = (max_value + 2 * near) / (2 * near + 1) + 1;
        let bpp = ceil_log2(max_value).max(2) as i32;
        Self {
            max_value,
            near,
            range,
            qbpp: ceil_log2(range),
            limit: 2 * (bpp + bpp.max(8)),
            reset: params.reset,
            t1: params.t1,
            t2: params.t2,
            t3: params.t3,
        }
    }

    /// Returns the starting accumulated error (`A`) for every context
    /// (T.87 § A.3.4).
    pub(super) fn initial_a(&self) -> i32 {
        ((self.range + 32) / 64).max(2)
    }

    /// Places a neighbouring-sample difference into one of nine gradient
    /// buckets, numbered `-4` through `4` (T.87 § A.3.3).
    #[inline]
    pub(super) fn quantize(&self, d: i32) -> i32 {
        if d <= -self.t3 {
            -4
        } else if d <= -self.t2 {
            -3
        } else if d <= -self.t1 {
            -2
        } else if d < -self.near {
            -1
        } else if d <= self.near {
            0
        } else if d < self.t1 {
            1
        } else if d < self.t2 {
            2
        } else if d < self.t3 {
            3
        } else {
            4
        }
    }

    /// Restricts a predicted sample to the valid range `0..=MAXVAL`
    /// (T.87 § A.4.2).
    #[inline]
    pub(super) fn correct_prediction(&self, px: i32) -> i32 {
        px.clamp(0, self.max_value)
    }

    /// Reduces a prediction error to the precision required by `NEAR`
    /// (T.87 § A.4.4).
    ///
    /// A no-op when the stream is lossless.
    #[inline]
    pub(super) fn quantize_error(&self, error: i32) -> i32 {
        if self.near == 0 {
            return error;
        }
        let divisor = 2 * self.near + 1;
        if error > 0 {
            (error + self.near) / divisor
        } else {
            -((self.near - error) / divisor)
        }
    }

    /// Wraps a prediction error into JPEG-LS's signed coding range
    /// (T.87 § A.4.5).
    #[inline]
    pub(super) fn modulo_range(&self, mut error: i32) -> i32 {
        if error < 0 {
            error += self.range;
        }
        if error >= (self.range + 1) / 2 {
            error -= self.range;
        }
        error
    }

    /// Rebuilds a sample from its prediction and quantised error, wrapping and
    /// clamping the result when necessary (T.87 § A.4.5 and A.6.3).
    #[inline]
    pub(super) fn reconstruct(&self, px: i32, err: i32) -> i32 {
        let mut value = px + err * (2 * self.near + 1);
        let modulo = self.range * (2 * self.near + 1);
        if value < -self.near {
            value += modulo;
        } else if value > self.max_value + self.near {
            value -= modulo;
        }
        self.correct_prediction(value)
    }
}

/// [`Traits::quantize`] as a table, for loops that quantise three gradients a
/// sample.
///
/// The standard's chain of comparisons is cheap when its branches are
/// predicted, but thermal images put gradients near a threshold often enough
/// that a Raspberry Pi 4 mispredicts two of them a pixel. Counting crossed
/// thresholds without branches is no general cure: the compiler vectorises
/// it into a longer chain of dependent operations than the one it replaces,
/// which Apple's cores wait on long enough to end up slower than with the
/// branches. A lookup has no data-dependent branch and a short chain, and
/// was the fastest of the three on both — almost a quarter off the encoder's
/// time on the Pi, a seventh on an M1.
///
/// Beyond the largest threshold the answer no longer changes, so the table
/// only covers the gradients up to it and clamps the rest — a few hundred
/// bytes that stay in the L1 cache.
pub(super) struct GradientTable {
    /// Quantised values for the gradients `-bound..=bound`.
    values: Vec<i8>,
    bound: i32,
    /// The parameters the table was built for.
    built_for: Option<(i32, i32, i32, i32)>,
}

impl GradientTable {
    /// Creates an empty table. Call [`Self::prepare`] before using it.
    pub(super) fn new() -> Self {
        Self {
            values: Vec::new(),
            bound: 0,
            built_for: None,
        }
    }

    /// Builds the lookup table for `traits`, or keeps the existing table when
    /// the relevant thresholds have not changed.
    pub(super) fn prepare(&mut self, traits: &Traits) {
        let key = (traits.near, traits.t1, traits.t2, traits.t3);
        if self.built_for == Some(key) {
            return;
        }
        // Past every threshold, and past NEAR, the comparisons all come out
        // the same way, whatever order the thresholds are in.
        self.bound = traits.t1.max(traits.t2).max(traits.t3).max(traits.near + 1);
        self.values.clear();
        self.values
            .extend((-self.bound..=self.bound).map(|d| traits.quantize(d) as i8));
        self.built_for = Some(key);
    }

    /// Quantises a gradient using the traits passed to the last
    /// [`Self::prepare`] call.
    #[inline]
    pub(super) fn quantize(&self, d: i32) -> i32 {
        i32::from(self.values[(d.clamp(-self.bound, self.bound) + self.bound) as usize])
    }
}

/// Predicts the next sample from its left (`Ra`), upper (`Rb`), and
/// upper-left (`Rc`) neighbours using JPEG-LS's median edge detector
/// (T.87 § A.4.2).
#[inline]
pub(super) fn predict(ra: i32, rb: i32, rc: i32) -> i32 {
    if rc >= ra.max(rb) {
        ra.min(rb)
    } else if rc <= ra.min(rb) {
        ra.max(rb)
    } else {
        ra + rb - rc
    }
}

/// Maps a signed prediction error to a non-negative integer for Golomb-Rice
/// coding (T.87 § A.5.2).
#[inline]
pub(super) fn map_error(error: i32) -> i32 {
    if error >= 0 {
        2 * error
    } else {
        -2 * error - 1
    }
}

/// Restores a signed prediction error produced by [`map_error`].
#[inline]
pub(super) fn unmap_error(mapped: i32) -> i32 {
    if mapped & 1 != 0 {
        -((mapped + 1) >> 1)
    } else {
        mapped >> 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_log2_matches_definition() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(255), 8);
        assert_eq!(ceil_log2(256), 8);
        assert_eq!(ceil_log2(257), 9);
        assert_eq!(ceil_log2(65535), 16);
        assert_eq!(ceil_log2(5959), 13);
    }

    #[test]
    fn error_mapping_roundtrips() {
        for err in -50..=50 {
            assert_eq!(unmap_error(map_error(err)), err, "failed for {err}");
        }
    }

    #[test]
    fn traits_match_flir_stream_parameters() {
        // The parameters FLIR's CSQ encoder emits: 16 bit, NEAR = 5.
        let params = CodingParameters {
            max_value: 65535,
            t1: 33,
            t2: 92,
            t3: 311,
            reset: 64,
        };
        let traits = Traits::new(params, 5);
        assert_eq!(traits.range, 5959);
        assert_eq!(traits.qbpp, 13);
        assert_eq!(traits.limit, 64);
    }

    #[test]
    fn flir_thresholds_are_the_defaults_for_their_near() {
        // FLIR writes an LSE segment rather than relying on the defaults, but
        // the values it writes are the ones T.87 would derive anyway.
        let defaults = CodingParameters::defaults(65535, 5);
        assert_eq!((defaults.t1, defaults.t2, defaults.t3), (33, 92, 311));
    }

    #[test]
    fn near_lossless_quantisation_stays_within_its_bound() {
        let traits = Traits::new(CodingParameters::defaults(65535, 5), 5);
        for error in -200..=200 {
            let quantized = traits.quantize_error(error);
            // Reconstructing from the quantised error must land within NEAR.
            let reconstructed = quantized * 11;
            assert!(
                (reconstructed - error).abs() <= 5,
                "error {error} quantised to {quantized} reconstructs {reconstructed}"
            );
        }
    }

    #[test]
    fn lossless_quantisation_is_the_identity() {
        let traits = Traits::new(CodingParameters::defaults(65535, 0), 0);
        for error in [-1000, -1, 0, 1, 1000] {
            assert_eq!(traits.quantize_error(error), error);
        }
    }

    #[test]
    fn the_gradient_table_matches_the_standard() {
        let unordered = CodingParameters {
            max_value: 65535,
            t1: 40,
            t2: 12,
            t3: 25,
            reset: 64,
        };
        let cases = [0, 1, 5, 16, 255]
            .map(|near| (CodingParameters::defaults(65535, near), near))
            .into_iter()
            .chain([(unordered, 0), (unordered, 60)]);

        let mut table = GradientTable::new();
        for (params, near) in cases {
            let traits = Traits::new(params, near);
            table.prepare(&traits);
            // Every difference two 16-bit samples can have.
            for d in -65535..=65535 {
                assert_eq!(
                    table.quantize(d),
                    traits.quantize(d),
                    "near {near}, thresholds {:?}, d {d}",
                    (params.t1, params.t2, params.t3)
                );
            }
        }
    }

    #[test]
    fn the_golomb_parameter_matches_the_standards_loop() {
        let reference = |n: i32, a: i32| {
            let mut k = 0;
            while k < 31 && (n << k) < a {
                k += 1;
            }
            k
        };
        for n in 1..=130 {
            for a in (0..5000).chain([65_535, 1 << 20, (1 << 30) - 1]) {
                assert_eq!(smallest_shift(n, a), reference(n, a), "n {n}, a {a}");
            }
        }
    }

    #[test]
    fn the_run_interruption_parity_flag_round_trips() {
        let traits = Traits::new(CodingParameters::defaults(65535, 0), 0);
        for ri_type in [0i32, 1] {
            let context = RunContext::new(traits.initial_a(), ri_type);
            for k in [0u32, 1, 5] {
                for errval in [-7i32, -1, 0, 1, 7] {
                    let map = context.error_map(errval, k);
                    let mapped = 2 * errval.abs() - ri_type - map;
                    assert_eq!(
                        context.compute_errval(mapped + ri_type, k),
                        errval,
                        "ri_type {ri_type}, k {k}, errval {errval}"
                    );
                }
            }
        }
    }
}
