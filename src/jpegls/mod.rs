//! A self-contained JPEG-LS (ITU-T T.87 / ISO 14495-1) decoder.
//!
//! FLIR stores the radiometric image of every CSQ frame as a single-component
//! JPEG-LS stream, usually 16 bit and near-lossless (`NEAR = 5`). Rather than
//! shelling out to `exiftool` and linking the `CharLS` C++ library, this module
//! implements the baseline decoding procedure directly, which keeps the crate
//! pure Rust and lets a decoder be reused across frames without reallocating.
//!
//! Only what CSQ actually needs is implemented: a single scan over a single
//! component with interleave mode 0. Anything else is rejected with
//! [`JpegLsError::Unsupported`] rather than silently mis-decoded.

mod bitreader;
mod decoder;

pub use decoder::{JpegLsDecoder, JpegLsInfo};

use std::fmt;

/// Errors produced while decoding a JPEG-LS stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpegLsError {
    /// The data did not start with a JPEG SOI marker.
    NotJpegLs,
    /// A marker segment ran past the end of the buffer.
    Truncated,
    /// No start-of-frame marker was found before the scan.
    MissingFrameHeader,
    /// The stream uses a feature this decoder does not implement.
    Unsupported(&'static str),
    /// A header field held a value outside its legal range.
    InvalidHeader(&'static str),
    /// The entropy-coded data ended before every sample was decoded.
    UnexpectedEndOfScan {
        /// Number of samples that were successfully decoded.
        decoded: usize,
        /// Number of samples the frame header promised.
        expected: usize,
    },
}

impl fmt::Display for JpegLsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJpegLs => write!(f, "not a JPEG-LS stream (missing SOI marker)"),
            Self::Truncated => write!(f, "JPEG-LS stream is truncated"),
            Self::MissingFrameHeader => write!(f, "JPEG-LS stream has no SOF55 frame header"),
            Self::Unsupported(what) => write!(f, "unsupported JPEG-LS feature: {what}"),
            Self::InvalidHeader(what) => write!(f, "invalid JPEG-LS header field: {what}"),
            Self::UnexpectedEndOfScan { decoded, expected } => write!(
                f,
                "JPEG-LS scan ended early: decoded {decoded} of {expected} samples"
            ),
        }
    }
}

impl std::error::Error for JpegLsError {}

/// Decodes a complete JPEG-LS stream into a freshly allocated sample buffer.
///
/// For repeated decoding (for example while walking a CSQ file) prefer
/// [`JpegLsDecoder`], which reuses its scratch buffers.
pub fn decode(data: &[u8]) -> Result<(JpegLsInfo, Vec<u16>), JpegLsError> {
    let mut out = Vec::new();
    let mut decoder = JpegLsDecoder::new();
    let info = decoder.decode_into(data, &mut out)?;
    Ok((info, out))
}
