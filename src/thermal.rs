//! Turning raw detector counts into temperatures.
//!
//! FLIR cameras store a per-pixel radiance count, not a temperature. Recovering
//! °C means undoing the atmospheric path, the optional IR window and the
//! reflected background, then inverting the sensor's Planck curve. The model
//! below is the one FLIR's own tools and `ExifTool`'s `-Celsius` recipes use.
//!
//! Because the counts are 16-bit, the whole mapping fits in a 65 536-entry
//! table. Building one costs 65 536 evaluations, which is already twelve times
//! cheaper than evaluating the formula per pixel on a 1024×768 frame — and the
//! table can then be reused for every frame that shares the same parameters.

/// Planck curve coefficients from the camera's calibration.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanckConstants {
    /// `R1` scaling term.
    pub r1: f32,
    /// `B` exponent term, close to the Planck constant expression.
    pub b: f32,
    /// `F` shape term, typically 1.
    pub f: f32,
    /// `O` offset applied to raw counts.
    pub o: f32,
    /// `R2` scaling term.
    pub r2: f32,
}

impl PlanckConstants {
    /// Raw counts a black body at `celsius` would produce.
    #[inline]
    pub fn radiance_at(&self, celsius: f32) -> f32 {
        let kelvin = celsius + crate::metadata::KELVIN_OFFSET;
        self.r1 / (self.r2 * ((self.b / kelvin).exp() - self.f)) - self.o
    }

    /// Inverse of [`radiance_at`](Self::radiance_at).
    ///
    /// Returns `NaN` when `raw` falls outside the curve's domain.
    #[inline]
    pub fn celsius_at(&self, raw: f32) -> f32 {
        let ratio = self.r1 / (self.r2 * (raw + self.o)) + self.f;
        if ratio <= 0.0 {
            return f32::NAN;
        }
        self.b / ratio.ln() - crate::metadata::KELVIN_OFFSET
    }
}

/// Coefficients of the atmospheric transmission model.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AtmosphericTransmission {
    /// Attenuation coefficient of the first atmospheric component.
    pub alpha1: f32,
    /// Attenuation coefficient of the second atmospheric component.
    pub alpha2: f32,
    /// Water-vapour coefficient of the first component.
    pub beta1: f32,
    /// Water-vapour coefficient of the second component.
    pub beta2: f32,
    /// Mixing ratio between the two components.
    pub x: f32,
}

/// Everything needed to convert raw counts into temperatures.
///
/// The values are read from the frame's camera-info record, but every one of
/// them can be overridden — re-measuring a recording with a different
/// emissivity or object distance is just a matter of changing a field and
/// rebuilding the [`TemperatureTable`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RadiometricParameters {
    /// Emissivity of the observed surface, 0..=1.
    pub emissivity: f32,
    /// Distance to the object in metres.
    pub object_distance: f32,
    /// Apparent temperature of the reflected background, in °C.
    pub reflected_apparent_temperature: f32,
    /// Air temperature along the path, in °C.
    pub atmospheric_temperature: f32,
    /// Temperature of the external IR window, in °C.
    pub ir_window_temperature: f32,
    /// Transmission of the external IR window, 1 when none is fitted.
    pub ir_window_transmission: f32,
    /// Relative humidity along the path, as a percentage.
    pub relative_humidity: f32,
    /// Sensor calibration curve.
    pub planck: PlanckConstants,
    /// Atmospheric transmission coefficients.
    pub atmospheric: AtmosphericTransmission,
}

impl RadiometricParameters {
    /// Precomputes the parts of the model that do not depend on the pixel.
    fn constants(&self) -> ConversionConstants {
        let emissivity = self.emissivity;
        let window_transmission = self.ir_window_transmission;
        let window_emissivity = 1.0 - window_transmission;
        // The reflection off the window itself is assumed to be negligible,
        // matching FLIR's own reference implementation.
        let window_reflection = 0.0;

        // Partial pressure of water vapour along the path.
        let air = self.atmospheric_temperature;
        let h2o = (self.relative_humidity / 100.0)
            * (1.5587 + 0.06939 * air - 0.000_278_16 * air * air
                + 0.000_000_684_55 * air * air * air)
                .exp();

        // The window is assumed to sit halfway between camera and object, so
        // both path segments see the same transmission.
        let half_path = (self.object_distance / 2.0).max(0.0).sqrt();
        let tau = self.atmospheric.x
            * (-half_path * (self.atmospheric.alpha1 + self.atmospheric.beta1 * h2o.sqrt())).exp()
            + (1.0 - self.atmospheric.x)
                * (-half_path * (self.atmospheric.alpha2 + self.atmospheric.beta2 * h2o.sqrt()))
                    .exp();
        let (tau1, tau2) = (tau, tau);

        let planck = self.planck;
        let raw_reflected = planck.radiance_at(self.reflected_apparent_temperature);
        let raw_atmosphere = planck.radiance_at(self.atmospheric_temperature);
        let raw_window = planck.radiance_at(self.ir_window_temperature);

        let attenuation = (1.0 - emissivity) / emissivity * raw_reflected
            + (1.0 - tau1) / emissivity / tau1 * raw_atmosphere
            + window_emissivity / emissivity / tau1 / window_transmission * raw_window
            + window_reflection / emissivity / tau1 / window_transmission * raw_reflected
            + (1.0 - tau2) / emissivity / tau1 / window_transmission / tau2 * raw_atmosphere;

        ConversionConstants {
            scale: 1.0 / emissivity / tau1 / window_transmission / tau2,
            attenuation,
            planck,
        }
    }

    /// Converts a single raw count to °C.
    ///
    /// Returns `NaN` for counts outside the calibration curve's domain.
    pub fn raw_to_celsius(&self, raw: u16) -> f32 {
        self.constants().celsius(raw)
    }

    /// Builds the full 16-bit lookup table for these parameters.
    pub fn temperature_table(&self) -> TemperatureTable {
        let constants = self.constants();
        let mut celsius = vec![0.0f32; TemperatureTable::LEN].into_boxed_slice();
        for (raw, slot) in celsius.iter_mut().enumerate() {
            *slot = constants.celsius(raw as u16);
        }
        TemperatureTable {
            parameters: *self,
            celsius,
        }
    }
}

/// Scene-independent terms of the radiometric model.
struct ConversionConstants {
    scale: f32,
    attenuation: f32,
    planck: PlanckConstants,
}

impl ConversionConstants {
    #[inline]
    fn celsius(&self, raw: u16) -> f32 {
        let object_radiance = f32::from(raw) * self.scale - self.attenuation;
        self.planck.celsius_at(object_radiance)
    }
}

/// A precomputed raw-count to °C mapping.
///
/// Reuse one of these across frames whenever the radiometric parameters are
/// unchanged, which for a single recording is essentially always.
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureTable {
    parameters: RadiometricParameters,
    celsius: Box<[f32]>,
}

impl TemperatureTable {
    /// Number of entries, one per possible raw count.
    pub const LEN: usize = u16::MAX as usize + 1;

    /// Builds a table for the given parameters.
    pub fn new(parameters: &RadiometricParameters) -> Self {
        parameters.temperature_table()
    }

    /// The parameters this table was built from.
    pub fn parameters(&self) -> &RadiometricParameters {
        &self.parameters
    }

    /// Looks up one raw count.
    #[inline]
    pub fn celsius(&self, raw: u16) -> f32 {
        self.celsius[usize::from(raw)]
    }

    /// Looks up one raw count and returns kelvin.
    #[inline]
    pub fn kelvin(&self, raw: u16) -> f32 {
        self.celsius(raw) + crate::metadata::KELVIN_OFFSET
    }

    /// Converts a whole buffer of raw counts into `out`.
    pub fn convert_into(&self, raw: &[u16], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(raw.len());
        out.extend(raw.iter().map(|&value| self.celsius(value)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calibration of the FLIR T1020 the test fixtures came from.
    fn parameters() -> RadiometricParameters {
        RadiometricParameters {
            emissivity: 0.76,
            object_distance: 50.0,
            reflected_apparent_temperature: 31.0,
            atmospheric_temperature: 36.0,
            ir_window_temperature: 31.0,
            ir_window_transmission: 1.0,
            relative_humidity: 25.0,
            planck: PlanckConstants {
                r1: 11895.471,
                b: 1328.9,
                f: 1.0,
                o: -3869.0,
                r2: 0.013_583_817,
            },
            atmospheric: AtmosphericTransmission {
                alpha1: 0.006569,
                alpha2: 0.012620,
                beta1: -0.002276,
                beta2: -0.006670,
                x: 1.9,
            },
        }
    }

    #[test]
    fn planck_curve_round_trips() {
        let planck = parameters().planck;
        for celsius in [-20.0f32, 0.0, 20.0, 37.0, 100.0] {
            let raw = planck.radiance_at(celsius);
            let back = planck.celsius_at(raw);
            assert!(
                (back - celsius).abs() < 0.01,
                "{celsius} -> {raw} -> {back}"
            );
        }
    }

    #[test]
    fn table_matches_direct_evaluation() {
        let params = parameters();
        let table = params.temperature_table();
        for raw in [0u16, 1000, 11773, 13200, 15048, 40000, u16::MAX] {
            let direct = params.raw_to_celsius(raw);
            let looked_up = table.celsius(raw);
            assert!(
                (direct - looked_up).abs() < 1e-4 || (direct.is_nan() && looked_up.is_nan()),
                "raw {raw}: direct {direct} vs table {looked_up}"
            );
        }
    }

    #[test]
    fn temperature_increases_with_raw_counts() {
        let table = parameters().temperature_table();
        let mut previous = f32::NEG_INFINITY;
        for raw in (12_000u16..16_000).step_by(37) {
            let celsius = table.celsius(raw);
            assert!(celsius > previous, "not monotonic at raw {raw}");
            previous = celsius;
        }
    }

    #[test]
    fn unit_emissivity_and_transmission_is_the_bare_planck_curve() {
        // With nothing to correct for, the model must collapse to the sensor
        // calibration curve.
        let mut params = parameters();
        params.emissivity = 1.0;
        params.ir_window_transmission = 1.0;
        params.atmospheric.x = 1.0;
        params.atmospheric.alpha1 = 0.0;
        params.atmospheric.beta1 = 0.0;

        for raw in [12_000u16, 13_200, 15_000] {
            let modelled = params.raw_to_celsius(raw);
            let bare = params.planck.celsius_at(f32::from(raw));
            assert!(
                (modelled - bare).abs() < 0.01,
                "raw {raw}: {modelled} vs {bare}"
            );
        }
    }

    #[test]
    fn emissivity_correction_follows_the_reflected_background() {
        // Lowering emissivity attributes more of the measured signal to the
        // reflected background, so the derived object temperature moves away
        // from that background. Which direction that is depends on whether the
        // background is hotter or colder than the object.
        let raw = 15_000u16;

        let mut warm_background = parameters();
        warm_background.reflected_apparent_temperature = 80.0;
        let high_emissivity = warm_background.raw_to_celsius(raw);
        warm_background.emissivity = 0.5;
        let low_emissivity = warm_background.raw_to_celsius(raw);
        assert!(
            low_emissivity < high_emissivity,
            "a hot background should pull the estimate down: {low_emissivity} vs {high_emissivity}"
        );

        let mut cold_background = parameters();
        cold_background.reflected_apparent_temperature = -40.0;
        let high_emissivity = cold_background.raw_to_celsius(raw);
        cold_background.emissivity = 0.5;
        let low_emissivity = cold_background.raw_to_celsius(raw);
        assert!(
            low_emissivity > high_emissivity,
            "a cold background should push the estimate up: {low_emissivity} vs {high_emissivity}"
        );
    }
}
