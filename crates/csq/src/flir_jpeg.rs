//! FLIR radiometric JPEGs.
//!
//! A still taken with a FLIR camera is an ordinary JPEG — the picture any
//! viewer shows — with a complete FFF frame tucked into its APP1 segments. A
//! JPEG segment holds at most 64 KiB, so a frame of any size is split across
//! several of them, each starting with an 8-byte header:
//!
//! ```text
//! 0x00  5   "FLIR\0"
//! 0x05  1   format version, always 1
//! 0x06  1   index of this part, from 0
//! 0x07  1   index of the last part
//! ```
//!
//! Putting the parts back together in order yields the same FFF frame a CSQ
//! recording would hold, so everything downstream — the record directory, the
//! calibration, the temperature conversion — is shared with the video formats.
//! [`CsqFile`](crate::CsqFile) does this transparently when it is handed a
//! JPEG.

use crate::error::{Error, Result};

/// The two bytes every JPEG starts with.
const SOI: [u8; 2] = [0xff, 0xd8];

/// Marker of an APP1 segment, which FLIR shares with Exif and XMP.
const APP1: u8 = 0xe1;

/// Marker of the start of the entropy-coded scan; no metadata follows it.
const SOS: u8 = 0xda;

/// Marker of the end of the image.
const EOI: u8 = 0xd9;

/// Identifier at the start of each FLIR APP1 segment.
const FLIR_ID: &[u8; 5] = b"FLIR\0";

/// Length of the per-part header, identifier included.
const PART_HEADER_LEN: usize = 8;

/// Whether `bytes` look like a JPEG at all.
///
/// This is only the two-byte signature; use [`extract_fff`] to find out whether
/// the JPEG is radiometric.
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&SOI)
}

/// Reassembles the FFF frame embedded in a FLIR radiometric JPEG.
///
/// Fails with [`Error::NotRadiometricJpeg`] when the JPEG carries no FLIR
/// segments, or when one of the parts is missing.
pub fn extract_fff(jpeg: &[u8]) -> Result<Vec<u8>> {
    if !is_jpeg(jpeg) {
        return Err(not_radiometric("missing JPEG signature"));
    }

    let mut parts: Vec<Option<&[u8]>> = Vec::new();
    let mut offset = SOI.len();

    while offset + 4 <= jpeg.len() {
        if jpeg[offset] != 0xff {
            return Err(not_radiometric(format!(
                "expected a JPEG marker at byte offset {offset}"
            )));
        }
        let marker = jpeg[offset + 1];
        // Any number of 0xff fill bytes may precede a marker.
        if marker == 0xff {
            offset += 1;
            continue;
        }
        if marker == SOS || marker == EOI {
            break;
        }
        // Standalone markers carry no length field.
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            offset += 2;
            continue;
        }

        let length = usize::from(u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]));
        let body_start = offset + 4;
        let body_end = offset + 2 + length;
        if length < 2 || body_end > jpeg.len() {
            return Err(Error::Truncated {
                what: "JPEG segment",
                offset: offset as u64,
            });
        }
        let body = &jpeg[body_start..body_end];

        if marker == APP1 && body.len() >= PART_HEADER_LEN && body.starts_with(FLIR_ID) {
            let index = usize::from(body[6]);
            let last = usize::from(body[7]);
            if index > last {
                return Err(not_radiometric(format!(
                    "FLIR segment {index} claims to be past the last one ({last})"
                )));
            }
            if parts.is_empty() {
                parts.resize(last + 1, None);
            } else if parts.len() != last + 1 {
                return Err(not_radiometric(
                    "FLIR segments disagree on how many parts there are",
                ));
            }
            parts[index] = Some(&body[PART_HEADER_LEN..]);
        }

        offset = body_end;
    }

    if parts.is_empty() {
        return Err(not_radiometric("no FLIR APP1 segments"));
    }
    if let Some(missing) = parts.iter().position(Option::is_none) {
        return Err(not_radiometric(format!(
            "FLIR segment {missing} of {} is missing",
            parts.len()
        )));
    }

    let mut fff = Vec::with_capacity(parts.iter().flatten().map(|part| part.len()).sum());
    for part in parts.into_iter().flatten() {
        fff.extend_from_slice(part);
    }
    Ok(fff)
}

fn not_radiometric(detail: impl Into<String>) -> Error {
    Error::NotRadiometricJpeg {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One APP1 segment holding part `index` of `last + 1`.
    fn flir_segment(index: u8, last: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = FLIR_ID.to_vec();
        body.extend([1, index, last]);
        body.extend_from_slice(payload);
        segment(APP1, &body)
    }

    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, marker];
        bytes.extend(((body.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn jpeg(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = SOI.to_vec();
        for s in segments {
            bytes.extend(s);
        }
        // A scan, whose entropy-coded bytes must not be parsed as segments.
        bytes.extend(segment(SOS, &[0; 4]));
        bytes.extend([0xff, 0xe1, 0xff, 0xff, 0x12]);
        bytes.extend([0xff, EOI]);
        bytes
    }

    #[test]
    fn joins_parts_in_index_order() {
        let data = jpeg(&[
            segment(0xe0, b"JFIF\0"),
            segment(APP1, b"Exif\0\0not ours"),
            flir_segment(1, 2, b"middle-"),
            flir_segment(0, 2, b"first-"),
            flir_segment(2, 2, b"last"),
        ]);
        assert_eq!(extract_fff(&data).unwrap(), b"first-middle-last");
    }

    #[test]
    fn skips_fill_bytes_before_markers() {
        let mut data = SOI.to_vec();
        data.extend([0xff, 0xff]);
        data.extend(flir_segment(0, 0, b"only"));
        data.extend([0xff, EOI]);
        assert_eq!(extract_fff(&data).unwrap(), b"only");
    }

    #[test]
    fn rejects_a_plain_jpeg() {
        let data = jpeg(&[segment(0xe0, b"JFIF\0")]);
        assert!(matches!(
            extract_fff(&data),
            Err(Error::NotRadiometricJpeg { .. })
        ));
    }

    #[test]
    fn rejects_a_missing_part() {
        let data = jpeg(&[flir_segment(0, 2, b"a"), flir_segment(2, 2, b"c")]);
        let error = extract_fff(&data).unwrap_err().to_string();
        assert!(error.contains("segment 1 of 3"), "{error}");
    }

    #[test]
    fn rejects_a_truncated_segment() {
        let mut data = jpeg(&[flir_segment(0, 0, b"payload")]);
        data.truncate(12);
        assert!(matches!(extract_fff(&data), Err(Error::Truncated { .. })));
    }

    #[test]
    fn rejects_non_jpeg_input() {
        assert!(extract_fff(b"FFF\0").is_err());
        assert!(!is_jpeg(b"FFF\0"));
    }
}
