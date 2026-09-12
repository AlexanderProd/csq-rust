//! Assembling one FFF frame.
//!
//! The layout follows what FLIR's own recorder writes, down to the order of the
//! record directory and the placeholders for features a frame does not use: a
//! reader that only ever saw camera output should find nothing unfamiliar.
//!
//! ```text
//! 0x0000  frame header, 64 bytes
//! 0x0040  record directory, 7 slots of 32 bytes
//! 0x0120  camera info
//!         measurement info  (empty)
//!         palette           (when the recording has one)
//!         meter link        (empty)
//!         GPS               (when the frame has a fix)
//!         raw data          — last, because it is the part that varies in size
//! ```

use crate::error::{Error, Result};
use crate::fff::{RecordType, HEADER_LEN, MAGIC, RECORD_ENTRY_LEN};
use crate::jpegls::{EncodeOptions, JpegLsEncoder};
use crate::metadata::{FrameMetadata, RawValueRange};
use crate::thermal::RawConversion;

use super::records::{self, RecordVersions, IMAGE_HEADER_LEN};
use super::{Compression, FramePixels, WriteFrame, WriteOptions};

/// Container format version every CSQ recording declares.
const FORMAT_VERSION: u32 = 101;

/// Directory slots to allocate. Cameras leave one spare, so this does too.
const RECORD_CAPACITY: u32 = 7;

/// The record directory, in the order cameras write it.
///
/// Entries a given frame has nothing for stay in place with a length of zero,
/// which is how a camera marks a feature it did not use.
const DIRECTORY: [RecordType; 6] = [
    RecordType::CameraInfo,
    RecordType::RawData,
    RecordType::MeasurementInfo,
    RecordType::PaletteInfo,
    RecordType::MeterLink,
    RecordType::GpsInfo,
];

/// Offset of the frame length field within the header.
const LENGTH_OFFSET: usize = 0x34;

/// Offset of the header checksum.
const CHECKSUM_OFFSET: usize = 0x3c;

/// Where the record bodies start.
const BODY_START: usize = HEADER_LEN + RECORD_CAPACITY as usize * RECORD_ENTRY_LEN;

/// Encodes frames into their on-disk form, reusing its buffers between calls.
///
/// One of these per thread is all the state a writer needs: the JPEG-LS
/// encoder, the count buffer, and the camera-info template that only has a few
/// fields patched per frame.
pub(crate) struct FrameEncoder {
    width: usize,
    height: usize,
    compression: Compression,
    creator: [u8; 16],
    /// Camera info as far as it is constant across the recording.
    camera_info: Vec<u8>,
    /// The display palette, already in record form, if the recording has one.
    palette: Option<Vec<u8>>,
    /// Counts the calibration saturates at, which do not change per frame.
    saturation: (u16, u16),
    /// Display level and span, when the caller set one.
    display_range: Option<(u16, u16)>,
    conversion: RawConversion,
    jpegls: JpegLsEncoder,
    counts: Vec<u16>,
}

impl FrameEncoder {
    pub(crate) fn new(metadata: &FrameMetadata, options: &WriteOptions) -> Result<Self> {
        if metadata.width == 0 || metadata.height == 0 {
            return Err(Error::Unwritable {
                detail: "a recording needs a non-zero width and height",
            });
        }
        if metadata.width > u16::MAX as usize || metadata.height > u16::MAX as usize {
            return Err(Error::Unwritable {
                detail: "frames wider or taller than 65535 pixels cannot be represented",
            });
        }

        let mut creator = [0u8; 16];
        let name = options.creator.as_bytes();
        let len = name.len().min(creator.len() - 1);
        creator[..len].copy_from_slice(&name[..len]);

        // The camera-recorded counts are metadata like anything else, so a
        // caller who has them — from the recording being copied, say — has them
        // written verbatim. Left blank, they are derived.
        let stored = metadata.raw_value_range;
        let supplied = stored != RawValueRange::default();
        Ok(Self {
            width: metadata.width,
            height: metadata.height,
            compression: options.compression,
            creator,
            camera_info: records::camera_info(metadata),
            palette: metadata.palette.as_ref().map(records::palette),
            saturation: if supplied {
                (stored.min, stored.max)
            } else {
                saturation_counts(metadata)
            },
            display_range: supplied.then_some((stored.median, stored.range)),
            conversion: metadata.radiometric.raw_conversion(),
            jpegls: JpegLsEncoder::new(),
            counts: Vec::new(),
        })
    }

    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Encodes one frame into `out`, replacing its contents.
    pub(crate) fn encode_into(
        &mut self,
        frame: &WriteFrame<'_>,
        sequence: u32,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // Destructured so the scratch buffer holding the converted counts and
        // the encoder that reads from it can be borrowed at the same time.
        let Self {
            width,
            height,
            compression,
            creator,
            camera_info,
            palette,
            saturation,
            display_range,
            conversion,
            jpegls,
            counts: scratch,
        } = self;
        let (width, height) = (*width, *height);
        let samples = width * height;

        let conversion = frame
            .radiometric
            .as_ref()
            .map_or(*conversion, RawConversion::new);
        let counts: &[u16] = match frame.pixels {
            FramePixels::Raw(raw) => {
                check_len(raw.len(), samples)?;
                raw
            }
            FramePixels::Celsius(celsius) => {
                check_len(celsius.len(), samples)?;
                conversion.convert_into(celsius, scratch);
                scratch
            }
            FramePixels::Kelvin(kelvin) => {
                check_len(kelvin.len(), samples)?;
                scratch.clear();
                scratch.reserve(kelvin.len());
                scratch.extend(
                    kelvin
                        .iter()
                        .map(|&k| conversion.raw(k - crate::metadata::KELVIN_OFFSET)),
                );
                scratch
            }
        };

        // Both image-bearing records open with the geometry; the camera info
        // also carries this frame's capture time and display range.
        records::put_image_header(camera_info, width, height, sequence);
        if let Some(parameters) = &frame.radiometric {
            records::put_radiometric(camera_info, parameters);
        }
        records::put_frame_fields(
            camera_info,
            frame.timestamp,
            &raw_value_range(counts, *saturation, *display_range),
        );

        let gps = frame.gps.as_ref().map(records::gps);
        let bodies: [(RecordType, &[u8]); 5] = [
            (RecordType::CameraInfo, camera_info),
            (RecordType::MeasurementInfo, &[]),
            (RecordType::PaletteInfo, palette.as_deref().unwrap_or(&[])),
            (RecordType::MeterLink, &[]),
            (RecordType::GpsInfo, gps.as_deref().unwrap_or(&[])),
        ];

        out.clear();
        out.resize(BODY_START, 0);
        let mut placements = [(0u32, 0u32); DIRECTORY.len()];
        for (kind, body) in bodies {
            placements[slot_of(kind)] = (out.len() as u32, body.len() as u32);
            out.extend_from_slice(body);
        }

        // The image is encoded straight into the frame buffer, right after the
        // space its record header needs, so it is never copied twice.
        let image_start = out.len();
        out.resize(image_start + IMAGE_HEADER_LEN, 0);
        records::put_image_header(&mut out[image_start..], width, height, sequence);

        let near = match compression {
            Compression::Lossless => 0,
            Compression::NearLossless { near } => *near,
        };
        out.extend_from_slice(
            jpegls.encode(counts, EncodeOptions::near_lossless(width, height, near))?,
        );

        // Cameras round the image record up to a multiple of eight bytes.
        let padding = (8 - (out.len() - image_start) % 8) % 8;
        out.resize(out.len() + padding, 0);
        placements[slot_of(RecordType::RawData)] =
            (image_start as u32, (out.len() - image_start) as u32);

        // The directory goes in before the header, which checksums it.
        write_directory(out, &placements);
        write_header(out, creator);
        Ok(())
    }
}

/// Where a record kind sits in the directory.
fn slot_of(kind: RecordType) -> usize {
    DIRECTORY
        .iter()
        .position(|entry| *entry == kind)
        .expect("every record a frame carries has a directory slot")
}

/// The counts a camera records alongside a frame.
///
/// Not statistics of the image, despite how they read: `min` and `max` are
/// where the calibration saturates, and `median` and `range` are the level and
/// span a viewer centres its palette on. Only the latter two are derived from
/// the frame, and only when the caller did not supply them.
fn raw_value_range(
    counts: &[u16],
    saturation: (u16, u16),
    display: Option<(u16, u16)>,
) -> RawValueRange {
    let (median, range) = display.unwrap_or_else(|| {
        let (low, high) = counts
            .iter()
            .fold((u16::MAX, u16::MIN), |(low, high), &count| {
                (low.min(count), high.max(count))
            });
        match counts.is_empty() {
            true => (0, 0),
            // Centred on the frame, which is the only reading of a display
            // range that is not simply wrong when nobody has chosen one.
            false => (((u32::from(low) + u32::from(high)) / 2) as u16, high - low),
        }
    });

    RawValueRange {
        min: saturation.0,
        max: saturation.1,
        median,
        range,
    }
}

/// The counts at which a calibration saturates, cold and hot.
///
/// Cameras record the Planck curve evaluated at the saturation temperatures —
/// the bare curve, without the scene corrections, which is what makes these
/// constant for a given camera rather than for a given measurement.
fn saturation_counts(metadata: &FrameMetadata) -> (u16, u16) {
    let planck = metadata.radiometric.planck;
    let count = |celsius: f32| {
        let raw = planck.radiance_at(celsius);
        match raw.is_nan() {
            true => 0,
            false => raw.round().clamp(0.0, f32::from(u16::MAX)) as u16,
        }
    };
    let range = &metadata.temperature_range;
    (count(range.min_saturated), count(range.max_saturated))
}

fn check_len(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::FrameSizeMismatch { expected, actual })
    }
}

/// Fills in the frame header, including the checksum over it.
fn write_header(frame: &mut [u8], creator: &[u8; 16]) {
    let length = frame.len() as u32;

    frame[..4].copy_from_slice(&MAGIC);
    frame[4..0x14].copy_from_slice(creator);
    frame[0x14..0x18].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    frame[0x18..0x1c].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    frame[0x1c..0x20].copy_from_slice(&(DIRECTORY.len() as u32).to_le_bytes());
    frame[0x20..0x24].copy_from_slice(&RECORD_CAPACITY.to_le_bytes());
    frame[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
    frame[LENGTH_OFFSET..LENGTH_OFFSET + 4].copy_from_slice(&length.to_le_bytes());

    // The checksum covers the header and the directory entries in use, with
    // its own field read as zero. Verified against recordings from four
    // camera generations.
    frame[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].fill(0);
    let covered = HEADER_LEN + DIRECTORY.len() * RECORD_ENTRY_LEN;
    let checksum = crc32(&frame[..covered]);
    frame[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
}

/// Writes the record directory. Called before the header, which checksums it.
fn write_directory(frame: &mut [u8], placements: &[(u32, u32); DIRECTORY.len()]) {
    for (index, (kind, &(offset, length))) in DIRECTORY.iter().zip(placements).enumerate() {
        let entry = HEADER_LEN + index * RECORD_ENTRY_LEN;
        let slot = &mut frame[entry..entry + RECORD_ENTRY_LEN];
        slot.fill(0);
        slot[0..2].copy_from_slice(&kind.code().to_le_bytes());
        slot[2..4].copy_from_slice(&RecordVersions::subtype_of(*kind).to_le_bytes());
        slot[4..8].copy_from_slice(&RecordVersions::of(*kind).to_le_bytes());
        slot[8..12].copy_from_slice(&(index as u32 + 1).to_le_bytes());
        slot[12..16].copy_from_slice(&offset.to_le_bytes());
        slot[16..20].copy_from_slice(&length.to_le_bytes());
    }
}

/// CRC-32 as in zlib and PNG, the one FLIR's frame headers carry.
///
/// Only ever run over the 256 or so bytes of a header, so a table would cost
/// more to build than the loop costs to run.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_reference_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_checksum_matches_what_cameras_write() {
        // A T1020 frame header and its directory, with the recorded checksum.
        let frame = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/t1020_2frames.csq"
        ))
        .expect("fixture missing");

        let covered = HEADER_LEN + 6 * RECORD_ENTRY_LEN;
        let mut header = frame[..covered].to_vec();
        let recorded = u32::from_le_bytes(
            header[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        header[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].fill(0);

        assert_eq!(crc32(&header), recorded);
    }
}
