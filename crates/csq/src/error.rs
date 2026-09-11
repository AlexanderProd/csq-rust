//! Error type for the crate.

use std::fmt;

use crate::fff::RecordType;
use crate::jpegls::JpegLsError;

/// Convenience alias for results produced by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Anything that can go wrong while reading a CSQ or FFF file.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An underlying I/O operation failed.
    Io(std::io::Error),

    /// The data at `offset` does not begin with the `FFF\0` signature.
    NotFff {
        /// Byte offset within the file where a frame was expected.
        offset: u64,
    },

    /// A structure extended past the end of the available data.
    Truncated {
        /// What was being read.
        what: &'static str,
        /// Byte offset the read started from.
        offset: u64,
    },

    /// A frame header declared a length that cannot be valid.
    InvalidFrameLength {
        /// Byte offset of the frame.
        offset: u64,
        /// The declared length.
        length: u32,
    },

    /// A record the caller needs is not present in the frame.
    MissingRecord(RecordType),

    /// The JPEG-LS payload could not be decoded.
    JpegLs(JpegLsError),

    /// The raw thermal image uses an encoding this build cannot decode.
    UnsupportedRawImage {
        /// Human-readable description of what was found.
        detail: String,
    },

    /// A PNG-encoded raw thermal image could not be decoded.
    #[cfg(feature = "png")]
    Png(png::DecodingError),

    /// The decoded image does not have the dimensions the record advertised.
    DimensionMismatch {
        /// Dimensions taken from the raw-data record header.
        expected: (usize, usize),
        /// Dimensions the decoded image actually has.
        actual: (usize, usize),
    },

    /// A frame index was past the end of the file.
    FrameOutOfRange {
        /// The requested index.
        index: usize,
        /// How many frames the file holds.
        len: usize,
    },

    /// A frame handed to a writer does not hold the expected number of pixels.
    FrameSizeMismatch {
        /// Pixels the recording's geometry calls for.
        expected: usize,
        /// Pixels the frame actually held.
        actual: usize,
    },

    /// A recording cannot be written as described.
    Unwritable {
        /// Human-readable description of what is in the way.
        detail: &'static str,
    },

    /// Pixel coordinates fell outside the image.
    PixelOutOfRange {
        /// Requested coordinates.
        position: (usize, usize),
        /// Image dimensions.
        size: (usize, usize),
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::NotFff { offset } => {
                write!(f, "no FLIR FFF signature at byte offset {offset}")
            }
            Self::Truncated { what, offset } => {
                write!(f, "{what} at byte offset {offset} is truncated")
            }
            Self::InvalidFrameLength { offset, length } => write!(
                f,
                "frame at byte offset {offset} declares an invalid length of {length}"
            ),
            Self::MissingRecord(kind) => write!(f, "frame has no {kind} record"),
            Self::JpegLs(e) => write!(f, "raw thermal image: {e}"),
            Self::UnsupportedRawImage { detail } => {
                write!(f, "unsupported raw thermal image encoding: {detail}")
            }
            #[cfg(feature = "png")]
            Self::Png(e) => write!(f, "raw thermal image (PNG): {e}"),
            Self::DimensionMismatch { expected, actual } => write!(
                f,
                "raw thermal image is {}x{} but the record header says {}x{}",
                actual.0, actual.1, expected.0, expected.1
            ),
            Self::FrameOutOfRange { index, len } => {
                write!(
                    f,
                    "frame index {index} is out of range (file has {len} frames)"
                )
            }
            Self::FrameSizeMismatch { expected, actual } => write!(
                f,
                "frame holds {actual} pixels but the recording is {expected} pixels a frame"
            ),
            Self::Unwritable { detail } => write!(f, "cannot write this recording: {detail}"),
            Self::PixelOutOfRange { position, size } => write!(
                f,
                "pixel ({}, {}) is outside the {}x{} image",
                position.0, position.1, size.0, size.1
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::JpegLs(e) => Some(e),
            #[cfg(feature = "png")]
            Self::Png(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<JpegLsError> for Error {
    fn from(e: JpegLsError) -> Self {
        Self::JpegLs(e)
    }
}

#[cfg(feature = "png")]
impl From<png::DecodingError> for Error {
    fn from(e: png::DecodingError) -> Self {
        Self::Png(e)
    }
}
