//! Building the record bodies that go into a written frame.
//!
//! Every offset here is the one [`crate::metadata`] reads back, so the two
//! files are each other's mirror: a field that moves in one has to move in the
//! other or the round-trip tests stop passing.
//!
//! Real cameras also fill in a good deal this crate does not model — detector
//! telemetry, per-sensor temperature histories, factory calibration tables.
//! Those bytes are left zero. What is *not* left zero is the handful of fields
//! that hold the same value in every recording seen so far, whatever the camera
//! model: they read as structural rather than model-specific, and a viewer that
//! looks at them should find what it expects.

use crate::fff::RecordType;
use crate::metadata::{FrameMetadata, GpsInfo, Palette, RawValueRange, Timestamp, YCbCr};
use crate::thermal::RadiometricParameters;

/// Length of the geometry preamble that opens the camera-info and raw-data
/// records alike.
pub(crate) const IMAGE_HEADER_LEN: usize = 32;

/// Length of the camera-info record body. Every FLIR camera writes this size.
pub(crate) const CAMERA_INFO_LEN: usize = 2476;

/// Length of the GPS record body.
const GPS_INFO_LEN: usize = 184;

/// Offset of the colour ramp inside a palette record body.
const PALETTE_COLORS: usize = 0x70;

/// Record body layout version, as written by the cameras this was derived from.
pub(crate) struct RecordVersions;

impl RecordVersions {
    pub(crate) fn of(kind: RecordType) -> u32 {
        match kind {
            RecordType::CameraInfo => 116,
            RecordType::RawData => 100,
            RecordType::PaletteInfo => 105,
            RecordType::GpsInfo => 100,
            _ => 0,
        }
    }

    pub(crate) fn subtype_of(kind: RecordType) -> u16 {
        match kind {
            RecordType::CameraInfo => 0,
            RecordType::RawData => 4,
            _ => 1,
        }
    }
}

#[inline]
fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn put_f32(buf: &mut [u8], offset: usize, value: f32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn put_f64(buf: &mut [u8], offset: usize, value: f64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Writes a temperature into a field the format stores in kelvin.
#[inline]
fn put_kelvin(buf: &mut [u8], offset: usize, celsius: f32) {
    put_f32(buf, offset, celsius + crate::metadata::KELVIN_OFFSET);
}

/// Writes a NUL-padded string into a fixed-size field, truncating on a
/// character boundary if it does not fit.
fn put_str(buf: &mut [u8], offset: usize, capacity: usize, value: &str) {
    let field = &mut buf[offset..offset + capacity];
    field.fill(0);
    // One byte is reserved for the terminator, which is what a reader looks
    // for; the truncation point has to stay on a character boundary so the
    // field never holds half a code point.
    let mut end = value.len().min(capacity - 1);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    field[..end].copy_from_slice(&value.as_bytes()[..end]);
}

fn put_color(buf: &mut [u8], offset: usize, color: YCbCr) {
    buf[offset] = color.y;
    buf[offset + 1] = color.cb;
    buf[offset + 2] = color.cr;
}

/// Writes the geometry preamble that opens both image-bearing records.
///
/// `sequence` is the camera's running frame counter; recordings that were cut
/// out of a longer session start it wherever they were cut.
pub(crate) fn put_image_header(buf: &mut [u8], width: usize, height: usize, sequence: u32) {
    buf[..IMAGE_HEADER_LEN].fill(0);
    put_u16(buf, 0x00, 2);
    put_u16(buf, 0x02, width as u16);
    put_u16(buf, 0x04, height as u16);
    put_u16(buf, 0x0c, (width - 1) as u16);
    put_u16(buf, 0x10, (height - 1) as u16);
    put_u16(buf, 0x12, 16);
    // The counter is 16 bit in every recording seen, and wraps rather than
    // stopping the recording.
    put_u16(buf, 0x18, sequence as u16);
}

/// Builds the camera-info record body for a recording.
///
/// The result is a template: [`put_frame_fields`] patches the parts that change
/// from frame to frame, which is what keeps writing a 60 fps feed from
/// rebuilding two and a half kilobytes per frame.
pub(crate) fn camera_info(metadata: &FrameMetadata) -> Vec<u8> {
    let mut buf = vec![0u8; CAMERA_INFO_LEN];

    put_image_header(&mut buf, metadata.width, metadata.height, 0);
    put_radiometric(&mut buf, &metadata.radiometric);

    let range = &metadata.temperature_range;
    put_kelvin(&mut buf, 0x90, range.max);
    put_kelvin(&mut buf, 0x94, range.min);
    put_kelvin(&mut buf, 0x98, range.max_clip);
    put_kelvin(&mut buf, 0x9c, range.min_clip);
    put_kelvin(&mut buf, 0xa0, range.max_warn);
    put_kelvin(&mut buf, 0xa4, range.min_warn);
    put_kelvin(&mut buf, 0xa8, range.max_saturated);
    put_kelvin(&mut buf, 0xac, range.min_saturated);

    let camera = &metadata.camera;
    put_str(&mut buf, 0xd4, 0x20, &camera.model);
    put_str(&mut buf, 0xf4, 0x10, &camera.part_number);
    put_str(&mut buf, 0x104, 0x10, &camera.serial_number);
    put_str(&mut buf, 0x114, 0x10, &camera.software);
    put_str(&mut buf, 0x170, 0x20, &camera.lens_model);
    put_str(&mut buf, 0x190, 0x10, &camera.lens_part_number);
    put_str(&mut buf, 0x1a0, 0x10, &camera.lens_serial_number);
    put_str(&mut buf, 0x1ec, 0x10, &camera.filter_model);
    put_str(&mut buf, 0x1fc, 0x20, &camera.filter_part_number);
    put_str(&mut buf, 0x21c, 0x20, &camera.filter_serial_number);

    put_f32(&mut buf, 0x1b4, metadata.field_of_view);
    put_f32(&mut buf, 0x45c, metadata.focus_distance);
    put_u16(&mut buf, 0x390, metadata.focus_step_count);
    put_u16(
        &mut buf,
        0x464,
        metadata.frame_rate.unwrap_or(0.0).round().max(0.0) as u16,
    );

    // Fields that hold the same value in every recording examined, across four
    // camera generations. Their meaning is not documented anywhere this crate
    // could check, so they are reproduced rather than interpreted.
    for offset in [0x50, 0xb4, 0x16c, 0x340, 0x3c0, 0x44c, 0x458] {
        put_u32(&mut buf, offset, 1);
    }
    put_u16(&mut buf, 0x1b2, 1);
    put_f32(&mut buf, 0x40, 6.0);
    put_f32(&mut buf, 0x8c, 4.0);
    put_f32(&mut buf, 0xb0, 2.0);

    buf
}

/// Writes the scene and calibration parameters into a camera-info body.
pub(crate) fn put_radiometric(buf: &mut [u8], parameters: &RadiometricParameters) {
    put_f32(buf, 0x20, parameters.emissivity);
    put_f32(buf, 0x24, parameters.object_distance);
    put_kelvin(buf, 0x28, parameters.reflected_apparent_temperature);
    put_kelvin(buf, 0x2c, parameters.atmospheric_temperature);
    put_kelvin(buf, 0x30, parameters.ir_window_temperature);
    put_f32(buf, 0x34, parameters.ir_window_transmission);
    // Humidity goes out as a fraction, which is the form a reader that has to
    // guess between the two conventions resolves correctly at any value.
    put_f32(buf, 0x3c, parameters.relative_humidity / 100.0);

    let planck = &parameters.planck;
    put_f32(buf, 0x58, planck.r1);
    put_f32(buf, 0x5c, planck.b);
    put_f32(buf, 0x60, planck.f);
    put_i32(buf, 0x308, planck.o as i32);
    put_f32(buf, 0x30c, planck.r2);
    // Cameras write the offset and second scaling term twice; a viewer reading
    // either copy has to find the same calibration.
    put_i32(buf, 0x450, planck.o as i32);
    put_f32(buf, 0x454, planck.r2);

    let atmospheric = &parameters.atmospheric;
    put_f32(buf, 0x70, atmospheric.alpha1);
    put_f32(buf, 0x74, atmospheric.alpha2);
    put_f32(buf, 0x78, atmospheric.beta1);
    put_f32(buf, 0x7c, atmospheric.beta2);
    put_f32(buf, 0x80, atmospheric.x);
}

/// Patches the camera-info fields that differ from frame to frame.
pub(crate) fn put_frame_fields(
    buf: &mut [u8],
    timestamp: Option<Timestamp>,
    raw_values: &RawValueRange,
) {
    match timestamp {
        Some(timestamp) => {
            put_u32(buf, 0x384, timestamp.unix_seconds.max(0) as u32);
            put_u32(buf, 0x388, u32::from(timestamp.milliseconds));
            // The file stores the offset with the opposite sign of the usual
            // convention: a camera set to UTC+02:00 records -120.
            put_u16(buf, 0x38c, (-timestamp.utc_offset_minutes) as u16);
        }
        None => {
            put_u32(buf, 0x384, 0);
            put_u32(buf, 0x388, 0);
            put_u16(buf, 0x38c, 0);
        }
    }

    put_u16(buf, 0x310, raw_values.min);
    put_u16(buf, 0x312, raw_values.max);
    put_u16(buf, 0x338, raw_values.median);
    put_u16(buf, 0x33c, raw_values.range);
}

/// Builds a palette record body.
pub(crate) fn palette(palette: &Palette) -> Vec<u8> {
    let colors = palette.colors.len();
    let mut buf = vec![0u8; PALETTE_COLORS + colors * 3];

    put_u32(&mut buf, 0x00, colors as u32);
    put_color(&mut buf, 0x06, palette.above_color);
    put_color(&mut buf, 0x09, palette.below_color);
    put_color(&mut buf, 0x0c, palette.overflow_color);
    put_color(&mut buf, 0x0f, palette.underflow_color);
    put_color(&mut buf, 0x12, palette.isotherm1_color);
    put_color(&mut buf, 0x15, palette.isotherm2_color);
    buf[0x1a] = palette.method;
    buf[0x1b] = palette.stretch;
    // The same value in every recording examined; see the note at the top.
    put_u32(&mut buf, 0x1c, 0x35);
    put_str(&mut buf, 0x30, 0x20, &palette.file_name);
    put_str(&mut buf, 0x50, 0x20, &palette.name);

    for (slot, color) in buf[PALETTE_COLORS..]
        .chunks_exact_mut(3)
        .zip(&palette.colors)
    {
        slot[0] = color.y;
        slot[1] = color.cb;
        slot[2] = color.cr;
    }

    buf
}

/// Builds a GPS record body.
pub(crate) fn gps(gps: &GpsInfo) -> Vec<u8> {
    let mut buf = vec![0u8; GPS_INFO_LEN];

    // A zero here marks the fix invalid, which is how a reader tells a frame
    // that had no position from one that had.
    put_u32(&mut buf, 0x00, 1);
    put_str(
        &mut buf,
        0x08,
        2,
        if gps.latitude < 0.0 { "S" } else { "N" },
    );
    put_str(
        &mut buf,
        0x0a,
        2,
        if gps.longitude < 0.0 { "W" } else { "E" },
    );
    put_f64(&mut buf, 0x10, gps.latitude.abs());
    put_f64(&mut buf, 0x18, gps.longitude.abs());
    put_f32(&mut buf, 0x20, gps.altitude);
    put_f32(&mut buf, 0x40, gps.dop.unwrap_or(-1.0));
    put_str(&mut buf, 0x48, 2, &gps.image_direction_ref);
    put_f32(&mut buf, 0x54, gps.image_direction.unwrap_or(-1.0));
    put_str(&mut buf, 0x58, 0x10, &gps.map_datum);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::CameraIdentity;

    #[test]
    fn strings_are_nul_terminated_and_truncated_on_a_boundary() {
        let mut buf = [0xffu8; 8];
        put_str(&mut buf, 0, 8, "ok");
        assert_eq!(&buf, b"ok\0\0\0\0\0\0");

        // A field always keeps room for the terminator.
        put_str(&mut buf, 0, 8, "0123456789");
        assert_eq!(&buf, b"0123456\0");

        // Truncating must not split a multi-byte character.
        let mut buf = [0xffu8; 4];
        put_str(&mut buf, 0, 4, "aa€");
        assert_eq!(&buf, b"aa\0\0", "the euro sign does not fit in one byte");
    }

    #[test]
    fn gps_hemispheres_follow_the_sign() {
        let mut fix = GpsInfo {
            latitude: -33.9,
            longitude: 151.2,
            altitude: 5.0,
            dop: None,
            image_direction: None,
            image_direction_ref: "T".into(),
            map_datum: "WGS84".into(),
        };
        let body = gps(&fix);
        let parsed = GpsInfo::parse(&body).expect("a written fix must parse back");
        assert!((parsed.latitude - fix.latitude).abs() < 1e-9);
        assert!((parsed.longitude - fix.longitude).abs() < 1e-9);

        fix.longitude = -151.2;
        let parsed = GpsInfo::parse(&gps(&fix)).unwrap();
        assert!((parsed.longitude - fix.longitude).abs() < 1e-9);
    }

    #[test]
    fn camera_info_round_trips_through_the_parser() {
        let mut metadata = FrameMetadata::new(64, 48, crate::test_support::parameters());
        metadata.camera = CameraIdentity {
            model: "FLIR T1020".into(),
            serial_number: "72502433".into(),
            software: "38.0.0".into(),
            ..Default::default()
        };
        metadata.frame_rate = Some(30.0);
        metadata.field_of_view = 44.6;
        metadata.focus_distance = 4.7;

        let mut body = camera_info(&metadata);
        let timestamp = Timestamp {
            unix_seconds: 1_665_923_179,
            milliseconds: 276,
            utc_offset_minutes: 120,
        };
        let raw_values = RawValueRange {
            min: 5589,
            max: 53132,
            median: 13200,
            range: 1384,
        };
        put_frame_fields(&mut body, Some(timestamp), &raw_values);

        let parsed = FrameMetadata::parse_camera_info(&body, 64, 48).unwrap();
        assert_eq!(parsed.camera, metadata.camera);
        assert_eq!(parsed.radiometric, metadata.radiometric);
        assert_eq!(parsed.temperature_range, metadata.temperature_range);
        assert_eq!(parsed.frame_rate, Some(30.0));
        assert_eq!(parsed.timestamp, Some(timestamp));
        assert_eq!(parsed.raw_value_range, raw_values);
    }

    #[test]
    fn a_frame_without_a_timestamp_reads_back_as_having_none() {
        let metadata = FrameMetadata::new(4, 4, crate::test_support::parameters());
        let mut body = camera_info(&metadata);
        put_frame_fields(&mut body, None, &RawValueRange::default());

        let parsed = FrameMetadata::parse_camera_info(&body, 4, 4).unwrap();
        assert_eq!(parsed.timestamp, None);
    }

    #[test]
    fn humidity_survives_the_readers_percent_or_fraction_guess() {
        // A reader cannot tell 0.5 % from 50 % by looking at the field, so it
        // guesses; whatever is written has to come back through that guess.
        for humidity in [0.0f32, 1.0, 25.0, 50.0, 99.0, 100.0] {
            let mut parameters = crate::test_support::parameters();
            parameters.relative_humidity = humidity;
            let metadata = FrameMetadata::new(4, 4, parameters);

            let parsed = FrameMetadata::parse_camera_info(&camera_info(&metadata), 4, 4).unwrap();
            assert!(
                (parsed.radiometric.relative_humidity - humidity).abs() < 0.01,
                "{humidity} % came back as {}",
                parsed.radiometric.relative_humidity
            );
        }
    }

    #[test]
    fn palettes_round_trip() {
        let original = Palette {
            name: "Iron".into(),
            file_name: "/FLIR/usr/etc/iron.pal".into(),
            colors: (0..224)
                .map(|i| YCbCr {
                    y: i as u8,
                    cb: 128,
                    cr: (255 - i) as u8,
                })
                .collect(),
            above_color: YCbCr {
                y: 145,
                cb: 128,
                cr: 128,
            },
            below_color: YCbCr {
                y: 100,
                cb: 128,
                cr: 128,
            },
            overflow_color: YCbCr {
                y: 67,
                cb: 216,
                cr: 98,
            },
            underflow_color: YCbCr {
                y: 41,
                cb: 110,
                cr: 240,
            },
            isotherm1_color: YCbCr {
                y: 100,
                cb: 32,
                cr: 32,
            },
            isotherm2_color: YCbCr {
                y: 100,
                cb: 216,
                cr: 98,
            },
            method: 0,
            stretch: 2,
        };

        let parsed = Palette::parse(&palette(&original)).expect("a written palette must parse");
        assert_eq!(parsed, original);
    }
}
