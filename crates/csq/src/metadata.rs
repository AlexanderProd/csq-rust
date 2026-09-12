//! Metadata records carried alongside each thermal frame.
//!
//! Field offsets were derived from real recordings and cross-checked against
//! `ExifTool`'s FLIR module, so the values here match what `exiftool` reports
//! for the same file.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::fff::cstr;
use crate::thermal::{AtmosphericTransmission, PlanckConstants, RadiometricParameters};

/// Kelvin value of 0 °C.
pub(crate) const KELVIN_OFFSET: f32 = 273.15;

#[inline]
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[inline]
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline]
fn i32_at(b: &[u8], o: usize) -> i32 {
    u32_at(b, o) as i32
}

#[inline]
fn f32_at(b: &[u8], o: usize) -> f32 {
    f32::from_bits(u32_at(b, o))
}

#[inline]
fn f64_at(b: &[u8], o: usize) -> f64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[o..o + 8]);
    f64::from_le_bytes(bytes)
}

/// Wall-clock time a frame was captured.
///
/// FLIR stores the instant in UTC together with the camera's offset from UTC,
/// so both the absolute instant and the local reading are recoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Timestamp {
    /// Seconds since the Unix epoch, UTC.
    pub unix_seconds: i64,
    /// Sub-second part in milliseconds.
    pub milliseconds: u16,
    /// Camera's offset from UTC in minutes; `+120` means UTC+02:00.
    pub utc_offset_minutes: i16,
}

impl Timestamp {
    fn parse(bytes: &[u8], offset: usize) -> Option<Self> {
        let seconds = u32_at(bytes, offset);
        if seconds == 0 {
            return None;
        }
        // The file stores the offset with the opposite sign of the usual
        // convention: a camera set to UTC+02:00 records -120.
        let stored_offset = u16_at(bytes, offset + 8) as i16;
        Some(Self {
            unix_seconds: i64::from(seconds),
            milliseconds: (u32_at(bytes, offset + 4) & 0xffff) as u16,
            utc_offset_minutes: -stored_offset,
        })
    }

    /// The current instant, with the given offset from UTC in minutes.
    ///
    /// The offset only affects how the time reads back locally; the instant
    /// itself is stored in UTC either way. Pass `0` when the recording has no
    /// meaningful local time zone.
    pub fn now(utc_offset_minutes: i16) -> Self {
        Self::from_system_time(SystemTime::now(), utc_offset_minutes)
    }

    /// Converts a [`SystemTime`], with the given offset from UTC in minutes.
    pub fn from_system_time(time: SystemTime, utc_offset_minutes: i16) -> Self {
        let (seconds, milliseconds) = match time.duration_since(UNIX_EPOCH) {
            Ok(since) => (since.as_secs() as i64, since.subsec_millis() as u16),
            Err(before) => {
                let before = before.duration();
                // Milliseconds count forward from the second below, so a time
                // before the epoch rounds down rather than towards zero.
                let seconds = -(before.as_secs() as i64);
                match before.subsec_millis() {
                    0 => (seconds, 0),
                    millis => (seconds - 1, (1000 - millis) as u16),
                }
            }
        };
        Self {
            unix_seconds: seconds,
            milliseconds,
            utc_offset_minutes,
        }
    }

    /// The instant as a [`SystemTime`].
    pub fn as_system_time(&self) -> SystemTime {
        let base = if self.unix_seconds >= 0 {
            UNIX_EPOCH + Duration::from_secs(self.unix_seconds as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs(self.unix_seconds.unsigned_abs())
        };
        base + Duration::from_millis(u64::from(self.milliseconds))
    }

    /// Formats the local reading as RFC 3339, for example
    /// `2022-10-16T14:26:19.276+02:00`.
    pub fn to_rfc3339(&self) -> String {
        let local = self.unix_seconds + i64::from(self.utc_offset_minutes) * 60;
        let (year, month, day, hour, minute, second) = civil_from_unix(local);
        let sign = if self.utc_offset_minutes < 0 {
            '-'
        } else {
            '+'
        };
        let offset = self.utc_offset_minutes.unsigned_abs();
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}{sign}{:02}:{:02}",
            self.milliseconds,
            offset / 60,
            offset % 60
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Converts Unix seconds into a civil date, using Howard Hinnant's
/// `civil_from_days` algorithm.
fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (
        y,
        m as u32,
        d as u32,
        (secs_of_day / 3600) as u32,
        (secs_of_day % 3600 / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}

/// Identification of the camera, lens and filter that produced a frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CameraIdentity {
    /// Model name, for example `FLIR T1020`.
    pub model: String,
    /// Manufacturer part number.
    pub part_number: String,
    /// Camera serial number.
    pub serial_number: String,
    /// Firmware version.
    pub software: String,
    /// Lens model name.
    pub lens_model: String,
    /// Lens part number.
    pub lens_part_number: String,
    /// Lens serial number.
    pub lens_serial_number: String,
    /// Filter model name, empty when no filter is fitted.
    pub filter_model: String,
    /// Filter part number.
    pub filter_part_number: String,
    /// Filter serial number.
    pub filter_serial_number: String,
}

/// The temperature span the camera was configured for, in °C.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TemperatureRange {
    /// Lower end of the calibrated measurement range.
    pub min: f32,
    /// Upper end of the calibrated measurement range.
    pub max: f32,
    /// Lower clipping limit.
    pub min_clip: f32,
    /// Upper clipping limit.
    pub max_clip: f32,
    /// Lower warning threshold.
    pub min_warn: f32,
    /// Upper warning threshold.
    pub max_warn: f32,
    /// Value below which the detector saturates.
    pub min_saturated: f32,
    /// Value above which the detector saturates.
    pub max_saturated: f32,
}

/// The raw-count fields a camera records alongside a frame.
///
/// None of these are statistics of the image, which is worth knowing before
/// reaching for them. `min` and `max` are the counts the calibration
/// saturates at — the Planck curve evaluated at
/// [`TemperatureRange::min_saturated`] and [`TemperatureRange::max_saturated`],
/// so they are constant across a recording. `median` and `range` are the level
/// and span the camera had picked for its own display.
///
/// For the counts a particular frame actually holds, look at
/// [`Frame::raw`](crate::Frame::raw).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RawValueRange {
    /// Count at which the calibration saturates at the cold end.
    pub min: u16,
    /// Count at which the calibration saturates at the hot end.
    pub max: u16,
    /// Display level: the count the palette is centred on.
    pub median: u16,
    /// Display span: how many counts the palette covers.
    pub range: u16,
}

/// A position fix recorded with the frame.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GpsInfo {
    /// Latitude in signed decimal degrees, north positive.
    pub latitude: f64,
    /// Longitude in signed decimal degrees, east positive.
    pub longitude: f64,
    /// Altitude in metres.
    pub altitude: f32,
    /// Dilution of precision, if reported.
    pub dop: Option<f32>,
    /// Direction the camera was pointing, in degrees.
    pub image_direction: Option<f32>,
    /// `M` for magnetic north, `T` for true north.
    pub image_direction_ref: String,
    /// Geodetic datum, normally `WGS84`.
    pub map_datum: String,
}

impl GpsInfo {
    /// Parses a GPS record body, returning `None` when the fix is not valid.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 0x60 || u32_at(bytes, 0) == 0 {
            return None;
        }

        let latitude = f64_at(bytes, 0x10);
        let longitude = f64_at(bytes, 0x18);
        if !latitude.is_finite() || !longitude.is_finite() {
            return None;
        }

        let sign = |reference: &str, positive: &str| -> f64 {
            if reference.eq_ignore_ascii_case(positive) || reference.is_empty() {
                1.0
            } else {
                -1.0
            }
        };
        let lat_ref = cstr(&bytes[0x08..0x0a]);
        let lon_ref = cstr(&bytes[0x0a..0x0c]);

        let finite = |value: f32| value.is_finite().then_some(value);

        Some(Self {
            latitude: latitude.abs() * sign(&lat_ref, "N"),
            longitude: longitude.abs() * sign(&lon_ref, "E"),
            altitude: f32_at(bytes, 0x20),
            dop: finite(f32_at(bytes, 0x40)).filter(|v| *v > 0.0),
            image_direction: finite(f32_at(bytes, 0x54)).filter(|v| *v >= 0.0),
            image_direction_ref: cstr(&bytes[0x48..0x4a]),
            map_datum: cstr(&bytes[0x58..bytes.len().min(0x68)]),
        })
    }
}

/// A colour in the YCbCr space FLIR stores palettes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct YCbCr {
    /// Luma.
    pub y: u8,
    /// Blue-difference chroma.
    pub cb: u8,
    /// Red-difference chroma.
    pub cr: u8,
}

impl YCbCr {
    /// Converts to 8-bit RGB using the BT.601 studio-swing matrix, which is
    /// what FLIR's palettes assume.
    pub fn to_rgb(self) -> [u8; 3] {
        let y = (f32::from(self.y) - 16.0) * 1.164_383_5;
        let cb = f32::from(self.cb) - 128.0;
        let cr = f32::from(self.cr) - 128.0;

        let clamp = |v: f32| v.clamp(0.0, 255.0) as u8;
        [
            clamp(y + 1.596_027 * cr),
            clamp(y - 0.391_762_5 * cb - 0.812_967_6 * cr),
            clamp(y + 2.017_232_1 * cb),
        ]
    }
}

/// The display palette the camera had selected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Palette {
    /// Palette name as shown on the camera, for example `Iron` or `Gray`.
    pub name: String,
    /// Path of the palette file inside the camera.
    pub file_name: String,
    /// Colour ramp, from coldest to hottest.
    pub colors: Vec<YCbCr>,
    /// Colour used for pixels above the span.
    pub above_color: YCbCr,
    /// Colour used for pixels below the span.
    pub below_color: YCbCr,
    /// Colour used for detector overflow.
    pub overflow_color: YCbCr,
    /// Colour used for detector underflow.
    pub underflow_color: YCbCr,
    /// First isotherm colour.
    pub isotherm1_color: YCbCr,
    /// Second isotherm colour.
    pub isotherm2_color: YCbCr,
    /// How the camera maps temperatures onto the ramp.
    pub method: u8,
    /// How far the ramp is stretched over the scene.
    pub stretch: u8,
}

impl Palette {
    /// Parses a palette record body.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        const COLORS_OFFSET: usize = 0x70;

        if bytes.len() < COLORS_OFFSET {
            return None;
        }
        let count = u32_at(bytes, 0) as usize;
        let needed = COLORS_OFFSET.checked_add(count.checked_mul(3)?)?;
        if count == 0 || bytes.len() < needed {
            return None;
        }

        let color = |offset: usize| YCbCr {
            y: bytes[offset],
            cb: bytes[offset + 1],
            cr: bytes[offset + 2],
        };

        Some(Self {
            name: cstr(&bytes[0x50..0x70]),
            file_name: cstr(&bytes[0x30..0x50]),
            colors: bytes[COLORS_OFFSET..needed]
                .chunks_exact(3)
                .map(|c| YCbCr {
                    y: c[0],
                    cb: c[1],
                    cr: c[2],
                })
                .collect(),
            above_color: color(0x06),
            below_color: color(0x09),
            overflow_color: color(0x0c),
            underflow_color: color(0x0f),
            isotherm1_color: color(0x12),
            isotherm2_color: color(0x15),
            method: bytes[0x1a],
            stretch: bytes[0x1b],
        })
    }
}

/// Everything the camera recorded alongside one thermal frame.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FrameMetadata {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// Calibration and scene parameters needed to turn counts into
    /// temperatures.
    pub radiometric: RadiometricParameters,
    /// Camera, lens and filter identification.
    pub camera: CameraIdentity,
    /// Calibrated temperature span, in °C.
    pub temperature_range: TemperatureRange,
    /// Span of raw counts in this frame.
    pub raw_value_range: RawValueRange,
    /// Horizontal field of view in degrees.
    pub field_of_view: f32,
    /// Focus distance in metres.
    pub focus_distance: f32,
    /// Focus motor step count.
    pub focus_step_count: u16,
    /// Recording frame rate in frames per second, if the camera stored one.
    pub frame_rate: Option<f32>,
    /// Capture time.
    pub timestamp: Option<Timestamp>,
    /// Position fix, when the camera had one.
    pub gps: Option<GpsInfo>,
    /// Display palette the camera had selected.
    pub palette: Option<Palette>,
}

impl FrameMetadata {
    /// Metadata for a recording that is about to be written.
    ///
    /// Everything a [`CsqWriter`](crate::write::CsqWriter) cannot infer starts
    /// out empty, so a caller fills in what they know and leaves the rest:
    ///
    /// ```
    /// # let radiometric = csq::RadiometricParameters {
    /// #     emissivity: 0.95, object_distance: 2.0,
    /// #     reflected_apparent_temperature: 20.0, atmospheric_temperature: 20.0,
    /// #     ir_window_temperature: 20.0, ir_window_transmission: 1.0,
    /// #     relative_humidity: 50.0,
    /// #     planck: csq::PlanckConstants { r1: 17096.453, b: 1428.0, f: 1.0, o: -342.0, r2: 0.046642166 },
    /// #     atmospheric: csq::AtmosphericTransmission {
    /// #         alpha1: 0.006569, alpha2: 0.012620, beta1: -0.002276, beta2: -0.006670, x: 1.9,
    /// #     },
    /// # };
    /// let mut metadata = csq::FrameMetadata::new(640, 480, radiometric);
    /// metadata.camera.model = "Bench rig".into();
    /// metadata.frame_rate = Some(60.0);
    /// ```
    ///
    /// The temperature range is derived from the calibration curve: it is the
    /// span those Planck constants can express, which is the widest claim the
    /// parameters support.
    pub fn new(width: usize, height: usize, radiometric: RadiometricParameters) -> Self {
        let (min, max) = radiometric.resolvable_span();

        Self {
            width,
            height,
            radiometric,
            camera: CameraIdentity::default(),
            temperature_range: TemperatureRange {
                min,
                max,
                min_clip: min,
                max_clip: max,
                min_warn: min,
                max_warn: max,
                min_saturated: min,
                max_saturated: max,
            },
            raw_value_range: RawValueRange::default(),
            field_of_view: 0.0,
            focus_distance: 0.0,
            focus_step_count: 0,
            frame_rate: None,
            timestamp: None,
            gps: None,
            palette: None,
        }
    }

    /// Parses the camera-info record body.
    ///
    /// `width` and `height` come from the raw-data record, which is the
    /// authoritative source for the image geometry.
    pub(crate) fn parse_camera_info(bytes: &[u8], width: usize, height: usize) -> Result<Self> {
        // Every field this crate reads lives below 0x468.
        if bytes.len() < 0x468 {
            return Err(Error::Truncated {
                what: "camera info record",
                offset: 0,
            });
        }

        let celsius = |offset: usize| f32_at(bytes, offset) - KELVIN_OFFSET;

        // Humidity is stored as a fraction on most cameras but as a percentage
        // on some; ExifTool applies the same heuristic.
        let humidity = f32_at(bytes, 0x3c);
        let relative_humidity = if humidity <= 1.0 {
            humidity * 100.0
        } else {
            humidity
        };

        let radiometric = RadiometricParameters {
            emissivity: f32_at(bytes, 0x20),
            object_distance: f32_at(bytes, 0x24),
            reflected_apparent_temperature: celsius(0x28),
            atmospheric_temperature: celsius(0x2c),
            ir_window_temperature: celsius(0x30),
            ir_window_transmission: f32_at(bytes, 0x34),
            relative_humidity,
            planck: PlanckConstants {
                r1: f32_at(bytes, 0x58),
                b: f32_at(bytes, 0x5c),
                f: f32_at(bytes, 0x60),
                r2: f32_at(bytes, 0x30c),
                o: i32_at(bytes, 0x308) as f32,
            },
            atmospheric: AtmosphericTransmission {
                alpha1: f32_at(bytes, 0x70),
                alpha2: f32_at(bytes, 0x74),
                beta1: f32_at(bytes, 0x78),
                beta2: f32_at(bytes, 0x7c),
                x: f32_at(bytes, 0x80),
            },
        };

        let frame_rate = match u16_at(bytes, 0x464) {
            0 => None,
            rate => Some(f32::from(rate)),
        };

        Ok(Self {
            width,
            height,
            radiometric,
            camera: CameraIdentity {
                model: cstr(&bytes[0xd4..0xf4]),
                part_number: cstr(&bytes[0xf4..0x104]),
                serial_number: cstr(&bytes[0x104..0x114]),
                software: cstr(&bytes[0x114..0x124]),
                lens_model: cstr(&bytes[0x170..0x190]),
                lens_part_number: cstr(&bytes[0x190..0x1a0]),
                lens_serial_number: cstr(&bytes[0x1a0..0x1b0]),
                filter_model: cstr(&bytes[0x1ec..0x1fc]),
                filter_part_number: cstr(&bytes[0x1fc..0x21c]),
                filter_serial_number: cstr(&bytes[0x21c..0x23c]),
            },
            temperature_range: TemperatureRange {
                max: celsius(0x90),
                min: celsius(0x94),
                max_clip: celsius(0x98),
                min_clip: celsius(0x9c),
                max_warn: celsius(0xa0),
                min_warn: celsius(0xa4),
                max_saturated: celsius(0xa8),
                min_saturated: celsius(0xac),
            },
            raw_value_range: RawValueRange {
                min: u16_at(bytes, 0x310),
                max: u16_at(bytes, 0x312),
                median: u16_at(bytes, 0x338),
                range: u16_at(bytes, 0x33c),
            },
            field_of_view: f32_at(bytes, 0x1b4),
            focus_distance: f32_at(bytes, 0x45c),
            focus_step_count: u16_at(bytes, 0x390),
            frame_rate,
            timestamp: Timestamp::parse(bytes, 0x384),
            gps: None,
            palette: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_in_local_time() {
        // 2022-10-16 12:26:19.276 UTC, camera set to UTC+02:00.
        let ts = Timestamp {
            unix_seconds: 1_665_923_179,
            milliseconds: 276,
            utc_offset_minutes: 120,
        };
        assert_eq!(ts.to_rfc3339(), "2022-10-16T14:26:19.276+02:00");
    }

    #[test]
    fn handles_negative_utc_offsets() {
        let ts = Timestamp {
            unix_seconds: 0,
            milliseconds: 0,
            utc_offset_minutes: -330,
        };
        assert_eq!(ts.to_rfc3339(), "1969-12-31T18:30:00.000-05:30");
    }

    #[test]
    fn converts_palette_entries_to_rgb() {
        // The grayscale palette runs from studio black to studio white.
        assert_eq!(
            YCbCr {
                y: 16,
                cb: 128,
                cr: 128
            }
            .to_rgb(),
            [0, 0, 0]
        );
        assert_eq!(
            YCbCr {
                y: 235,
                cb: 128,
                cr: 128
            }
            .to_rgb(),
            [255, 255, 255]
        );
    }
}
