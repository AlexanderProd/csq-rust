//! The FLIR FFF container.
//!
//! A `.csq` file is simply a concatenation of FFF frames with no outer wrapper.
//! Each frame starts with a 64-byte header:
//!
//! ```text
//! 0x00  4    "FFF\0" signature
//! 0x04  16   creator software, NUL padded (CSQ recordings say "RTP")
//! 0x14  4    format version
//! 0x18  4    offset of the record directory
//! 0x1c  4    number of records in use
//! 0x20  4    number of record slots
//! 0x24  4    next free record id
//! 0x34  4    total length of this frame in bytes
//! 0x3c  4    checksum
//! ```
//!
//! The length at `0x34` is what makes seeking cheap: walking a file means
//! reading 64 bytes per frame rather than scanning for signatures.
//!
//! The record directory that follows is a list of 32-byte entries, each
//! pointing at a record elsewhere in the same frame.
//!
//! # Byte order
//!
//! Most cameras write the container little-endian, but some — the T450sc and
//! T650sc among them — write the header and the record directory big-endian.
//! The record *bodies* stay little-endian either way, so only the structures in
//! this module vary. [`FrameHeader::parse`] detects which is in use by checking
//! whether the directory offset and record count land in a sensible place.

use std::fmt;

use crate::error::{Error, Result};

/// Signature every FFF frame starts with.
pub const MAGIC: [u8; 4] = *b"FFF\0";

/// Length of the fixed frame header.
pub const HEADER_LEN: usize = 0x40;

/// Length of one record directory entry.
pub const RECORD_ENTRY_LEN: usize = 32;

/// Offset of the frame length field within the header.
const LENGTH_OFFSET: usize = 0x34;

/// Byte order of an FFF header and its record directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Little-endian, what most FLIR cameras write.
    Little,
    /// Big-endian, written by the T450sc and T650sc among others.
    Big,
}

impl ByteOrder {
    #[inline]
    fn u16(self, bytes: &[u8], offset: usize) -> u16 {
        let raw = [bytes[offset], bytes[offset + 1]];
        match self {
            Self::Little => u16::from_le_bytes(raw),
            Self::Big => u16::from_be_bytes(raw),
        }
    }

    #[inline]
    fn u32(self, bytes: &[u8], offset: usize) -> u32 {
        let raw = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        match self {
            Self::Little => u32::from_le_bytes(raw),
            Self::Big => u32::from_be_bytes(raw),
        }
    }

    /// Whether reading the header this way yields a self-consistent frame.
    ///
    /// The record directory is the reliable part: a real header points it
    /// somewhere past itself and claims a sane number of entries. The length
    /// field is only checked when it is set, because some producers leave it at
    /// zero and expect the reader to find the next frame by other means.
    fn is_plausible(self, header: &[u8]) -> bool {
        let index_offset = self.u32(header, 0x18) as u64;
        let count = self.u32(header, 0x1c) as u64;
        let length = self.u32(header, LENGTH_OFFSET) as u64;

        if index_offset < HEADER_LEN as u64 || !(1..=4096).contains(&count) {
            return false;
        }
        length == 0
            || (length >= HEADER_LEN as u64
                && index_offset + count * RECORD_ENTRY_LEN as u64 <= length)
    }
}

/// Detects whether a frame header is little- or big-endian.
///
/// Returns `None` when neither reading makes sense, which means the bytes are
/// not a usable frame header.
pub fn byte_order(header: &[u8]) -> Option<ByteOrder> {
    if header.len() < HEADER_LEN || header[..4] != MAGIC {
        return None;
    }
    [ByteOrder::Little, ByteOrder::Big]
        .into_iter()
        .find(|order| order.is_plausible(header))
}

/// Reads the frame length out of a 64-byte frame header without validating the
/// rest of it.
///
/// Returns `None` if `header` is too short, lacks the FFF signature, or is not
/// self-consistent in either byte order.
pub fn frame_length(header: &[u8]) -> Option<u32> {
    byte_order(header).map(|order| order.u32(header, LENGTH_OFFSET))
}

/// The fixed part of an FFF frame header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// Software that produced the file; CSQ recordings report `RTP`.
    pub creator: String,
    /// Container format version.
    pub version: u32,
    /// Offset of the record directory, relative to the start of the frame.
    pub index_offset: u32,
    /// Number of directory entries that are in use.
    pub record_count: u32,
    /// Number of directory slots that were allocated.
    pub record_capacity: u32,
    /// Total frame length in bytes, including this header.
    ///
    /// Zero for producers that do not fill the field in; the frame's extent has
    /// to be found by other means then, which is what
    /// [`FrameIndex`](crate::FrameIndex) does.
    pub length: u32,
    /// Header checksum as stored in the file (not verified).
    pub checksum: u32,
    /// Byte order of this header and its record directory.
    pub byte_order: ByteOrder,
}

impl FrameHeader {
    /// Parses a frame header from the first [`HEADER_LEN`] bytes of a frame.
    ///
    /// `offset` is only used to make errors point at the right place in the
    /// file.
    pub fn parse(bytes: &[u8], offset: u64) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Truncated {
                what: "FFF frame header",
                offset,
            });
        }
        if bytes[..4] != MAGIC {
            return Err(Error::NotFff { offset });
        }

        let order = byte_order(bytes).ok_or(Error::InvalidFrameLength {
            offset,
            length: ByteOrder::Little.u32(bytes, LENGTH_OFFSET),
        })?;

        let length = order.u32(bytes, LENGTH_OFFSET);
        if length != 0 && (length as usize) < HEADER_LEN {
            return Err(Error::InvalidFrameLength { offset, length });
        }

        Ok(Self {
            creator: cstr(&bytes[0x04..0x14]),
            version: order.u32(bytes, 0x14),
            index_offset: order.u32(bytes, 0x18),
            record_count: order.u32(bytes, 0x1c),
            record_capacity: order.u32(bytes, 0x20),
            length,
            checksum: order.u32(bytes, 0x3c),
            byte_order: order,
        })
    }
}

/// The kind of payload a record holds.
///
/// Values follow FLIR's numbering; anything this crate does not name is kept as
/// [`RecordType::Other`] so unusual files still round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordType {
    /// The radiometric image itself.
    RawData,
    /// Sensor gain map.
    GainMap,
    /// Sensor offset map.
    OffsetMap,
    /// Bad pixel map.
    BadPixelMap,
    /// Per-pixel open-circuit map.
    OpenMap,
    /// Per-pixel dead-pixel map.
    DeadMap,
    /// Calibration parameters, Planck constants and camera identification.
    CameraInfo,
    /// Spot meters, boxes and other measurement tools.
    MeasurementInfo,
    /// Display palette and out-of-range colours.
    PaletteInfo,
    /// Free-form text annotations.
    TextInfo,
    /// Recorded voice annotation.
    EmbeddedAudio,
    /// Picture-in-picture placement.
    PictureInPicture,
    /// Position fix taken with the frame.
    GpsInfo,
    /// Data from a linked measurement instrument.
    MeterLink,
    /// Additional camera parameters.
    ParameterInfo,
    /// Visual-camera or fused image accompanying the thermal frame.
    EmbeddedImage,
    /// A record type this crate does not name.
    Other(u16),
}

impl RecordType {
    /// Maps a raw record type code.
    pub fn from_code(code: u16) -> Self {
        match code {
            1 => Self::RawData,
            2 => Self::GainMap,
            3 => Self::OffsetMap,
            4 => Self::BadPixelMap,
            5 => Self::OpenMap,
            6 => Self::DeadMap,
            0x20 => Self::CameraInfo,
            0x21 => Self::MeasurementInfo,
            0x22 => Self::PaletteInfo,
            0x23 => Self::TextInfo,
            0x24 => Self::EmbeddedAudio,
            0x2a => Self::PictureInPicture,
            0x2b => Self::GpsInfo,
            0x2c => Self::MeterLink,
            0x2e => Self::ParameterInfo,
            0x70 => Self::EmbeddedImage,
            other => Self::Other(other),
        }
    }

    /// The raw record type code.
    pub fn code(self) -> u16 {
        match self {
            Self::RawData => 1,
            Self::GainMap => 2,
            Self::OffsetMap => 3,
            Self::BadPixelMap => 4,
            Self::OpenMap => 5,
            Self::DeadMap => 6,
            Self::CameraInfo => 0x20,
            Self::MeasurementInfo => 0x21,
            Self::PaletteInfo => 0x22,
            Self::TextInfo => 0x23,
            Self::EmbeddedAudio => 0x24,
            Self::PictureInPicture => 0x2a,
            Self::GpsInfo => 0x2b,
            Self::MeterLink => 0x2c,
            Self::ParameterInfo => 0x2e,
            Self::EmbeddedImage => 0x70,
            Self::Other(code) => code,
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawData => f.write_str("raw data"),
            Self::GainMap => f.write_str("gain map"),
            Self::OffsetMap => f.write_str("offset map"),
            Self::BadPixelMap => f.write_str("bad pixel map"),
            Self::OpenMap => f.write_str("open map"),
            Self::DeadMap => f.write_str("dead map"),
            Self::CameraInfo => f.write_str("camera info"),
            Self::MeasurementInfo => f.write_str("measurement info"),
            Self::PaletteInfo => f.write_str("palette info"),
            Self::TextInfo => f.write_str("text info"),
            Self::EmbeddedAudio => f.write_str("embedded audio"),
            Self::PictureInPicture => f.write_str("picture in picture"),
            Self::GpsInfo => f.write_str("GPS info"),
            Self::MeterLink => f.write_str("meter link"),
            Self::ParameterInfo => f.write_str("parameter info"),
            Self::EmbeddedImage => f.write_str("embedded image"),
            Self::Other(code) => write!(f, "record type {code:#x}"),
        }
    }
}

/// One entry of a frame's record directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// What the record holds.
    pub kind: RecordType,
    /// Subtype, interpreted per record kind.
    pub subtype: u16,
    /// Layout version of the record body.
    pub version: u32,
    /// Identifier unique within the frame.
    pub id: u32,
    /// Offset of the body, relative to the start of the frame.
    pub offset: u32,
    /// Length of the body in bytes.
    pub length: u32,
}

impl Record {
    /// Borrows this record's body out of the frame it belongs to.
    pub fn data<'a>(&self, frame: &'a [u8]) -> Result<&'a [u8]> {
        let start = self.offset as usize;
        let end = start
            .checked_add(self.length as usize)
            .ok_or(Error::Truncated {
                what: "record body",
                offset: self.offset as u64,
            })?;
        frame.get(start..end).ok_or(Error::Truncated {
            what: "record body",
            offset: self.offset as u64,
        })
    }
}

/// A parsed FFF frame: its header plus its record directory.
///
/// The record bodies are not copied; use [`Record::data`] with the same frame
/// slice to reach them.
#[derive(Debug, Clone)]
pub struct FrameLayout {
    /// The frame header.
    pub header: FrameHeader,
    /// The record directory, in file order.
    pub records: Vec<Record>,
}

impl FrameLayout {
    /// Parses the header and record directory of a single frame.
    pub fn parse(frame: &[u8], offset: u64) -> Result<Self> {
        let header = FrameHeader::parse(frame, offset)?;

        let directory_start = header.index_offset as usize;
        let entries = header.record_count as usize;
        let directory_len = entries
            .checked_mul(RECORD_ENTRY_LEN)
            .ok_or(Error::Truncated {
                what: "record directory",
                offset,
            })?;
        let directory = frame
            .get(directory_start..directory_start + directory_len)
            .ok_or(Error::Truncated {
                what: "record directory",
                offset,
            })?;

        let order = header.byte_order;
        let records = directory
            .chunks_exact(RECORD_ENTRY_LEN)
            .map(|entry| Record {
                kind: RecordType::from_code(order.u16(entry, 0)),
                subtype: order.u16(entry, 2),
                version: order.u32(entry, 4),
                id: order.u32(entry, 8),
                offset: order.u32(entry, 12),
                length: order.u32(entry, 16),
            })
            .collect();

        Ok(Self { header, records })
    }

    /// Finds the first record of the given kind that carries a body.
    ///
    /// Empty records are skipped: FLIR leaves zero-length placeholders in the
    /// directory for features a given frame does not use, and two records can
    /// even share an offset when one of them is empty.
    pub fn record(&self, kind: RecordType) -> Option<&Record> {
        self.records
            .iter()
            .find(|record| record.kind == kind && record.length > 0)
    }

    /// Borrows the body of the first non-empty record of the given kind.
    pub fn record_data<'a>(&self, frame: &'a [u8], kind: RecordType) -> Result<&'a [u8]> {
        self.record(kind)
            .ok_or(Error::MissingRecord(kind))?
            .data(frame)
    }
}

/// Reads a NUL-terminated, NUL-padded string out of a fixed-size field.
pub(crate) fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/t1020_2frames.csq"
        ))
        .expect("fixture missing")
    }

    #[test]
    fn parses_frame_header() {
        let data = fixture();
        let header = FrameHeader::parse(&data, 0).unwrap();
        assert_eq!(header.creator, "RTP");
        assert_eq!(header.index_offset, 0x40);
        assert_eq!(header.record_count, 6);
        assert_eq!(header.length, 246_900);
        assert_eq!(frame_length(&data), Some(246_900));
    }

    #[test]
    fn parses_record_directory() {
        let data = fixture();
        let layout = FrameLayout::parse(&data, 0).unwrap();
        assert_eq!(layout.records.len(), 6);

        let raw = layout.record(RecordType::RawData).unwrap();
        assert_eq!(raw.offset, 3788);
        assert_eq!(raw.length, 243_112);

        let camera = layout.record(RecordType::CameraInfo).unwrap();
        assert_eq!(camera.length, 2476);

        // MeterLink is present but empty, so it must not be handed out.
        assert!(layout.record(RecordType::MeterLink).is_none());
        assert!(layout
            .records
            .iter()
            .any(|r| r.kind == RecordType::MeterLink));
    }

    #[test]
    fn detects_big_endian_headers() {
        // A T650sc header: same fields, opposite byte order.
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[0x18..0x1c].copy_from_slice(&0x40u32.to_be_bytes());
        header[0x1c..0x20].copy_from_slice(&6u32.to_be_bytes());
        header[0x34..0x38].copy_from_slice(&9000u32.to_be_bytes());

        assert_eq!(byte_order(&header), Some(ByteOrder::Big));
        assert_eq!(frame_length(&header), Some(9000));

        let parsed = FrameHeader::parse(&header, 0).unwrap();
        assert_eq!(parsed.byte_order, ByteOrder::Big);
        assert_eq!(parsed.index_offset, 0x40);
        assert_eq!(parsed.record_count, 6);
        assert_eq!(parsed.length, 9000);
    }

    #[test]
    fn little_endian_headers_are_not_mistaken_for_big_endian() {
        let data = fixture();
        assert_eq!(byte_order(&data), Some(ByteOrder::Little));
        assert_eq!(
            FrameHeader::parse(&data, 0).unwrap().byte_order,
            ByteOrder::Little
        );
    }

    #[test]
    fn accepts_headers_that_leave_the_length_unset() {
        // Some producers interleave a second frame type whose length field is
        // zero; the record directory still describes the frame.
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[0x18..0x1c].copy_from_slice(&(HEADER_LEN as u32).to_be_bytes());
        header[0x1c..0x20].copy_from_slice(&2u32.to_be_bytes());

        let parsed = FrameHeader::parse(&header, 0).unwrap();
        assert_eq!(parsed.length, 0);
        assert_eq!(parsed.record_count, 2);
    }

    #[test]
    fn rejects_data_without_signature() {
        let err = FrameHeader::parse(&[0u8; HEADER_LEN], 128).unwrap_err();
        assert!(matches!(err, Error::NotFff { offset: 128 }));
    }

    #[test]
    fn rejects_short_header() {
        let err = FrameHeader::parse(b"FFF\0", 0).unwrap_err();
        assert!(matches!(err, Error::Truncated { .. }));
    }
}
