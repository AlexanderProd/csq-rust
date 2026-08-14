//! Turning one FFF frame's bytes into a [`Frame`].

use crate::error::{Error, Result};
use crate::fff::{FrameLayout, RecordType};
use crate::frame::Frame;
use crate::jpegls::JpegLsDecoder;
use crate::metadata::{FrameMetadata, GpsInfo, Palette};

/// Length of the header that precedes the encoded image in a raw-data record.
const RAW_HEADER_LEN: usize = 32;

/// Geometry taken from a raw-data record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawImageHeader {
    width: usize,
    height: usize,
}

impl RawImageHeader {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < RAW_HEADER_LEN {
            return Err(Error::Truncated {
                what: "raw data record header",
                offset: 0,
            });
        }
        Ok(Self {
            width: usize::from(u16::from_le_bytes([bytes[2], bytes[3]])),
            height: usize::from(u16::from_le_bytes([bytes[4], bytes[5]])),
        })
    }
}

/// Which parts of a frame to decode.
///
/// Skipping the image is worthwhile when only metadata is needed — probing a
/// file's camera model or frame rate then costs a few microseconds per frame
/// instead of several milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeOptions {
    /// Decode the radiometric image. When false the frame's raw buffer is
    /// empty.
    pub image: bool,
    /// Parse the GPS record, when present.
    pub gps: bool,
    /// Parse the display palette, when present.
    pub palette: bool,
    /// Return a partially decoded image instead of failing when a frame's
    /// entropy-coded data ends early.
    ///
    /// Off by default. Damaged recordings do occur — a stream cut short decodes
    /// into plausible-looking but meaningless pixels — so opting in is the
    /// caller's decision. Check [`Frame::decoded_rows`] to see how much of a
    /// frame is real.
    ///
    /// [`Frame::decoded_rows`]: crate::Frame::decoded_rows
    pub allow_truncated: bool,
}

impl DecodeOptions {
    /// Everything: image, GPS and palette. This is what the readers use by
    /// default.
    pub const fn all() -> Self {
        Self {
            image: true,
            gps: true,
            palette: true,
            allow_truncated: false,
        }
    }

    /// Everything except the image, which is the only expensive part.
    pub const fn metadata_only() -> Self {
        Self {
            image: false,
            gps: true,
            palette: true,
            allow_truncated: false,
        }
    }

    /// Same as [`all`](Self::all), but salvages frames whose image data was cut
    /// short instead of rejecting them.
    pub const fn tolerant() -> Self {
        Self {
            allow_truncated: true,
            ..Self::all()
        }
    }
}

/// Decodes FFF frames, reusing its scratch buffers between calls.
///
/// A decoder is cheap to create but holds the JPEG-LS line buffers and context
/// statistics, so keeping one around while walking a file avoids reallocating
/// for every frame.
#[derive(Default)]
pub struct FrameDecoder {
    jpegls: JpegLsDecoder,
    pixels: Vec<u16>,
}

impl FrameDecoder {
    /// Creates a decoder with empty scratch buffers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes a single FFF frame.
    ///
    /// `frame` must span exactly one frame, starting at its `FFF\0` signature.
    /// `offset` is only used to make error messages point into the file.
    pub fn decode(&mut self, frame: &[u8], offset: u64, options: DecodeOptions) -> Result<Frame> {
        let layout = FrameLayout::parse(frame, offset)?;

        let raw_record = layout.record_data(frame, RecordType::RawData)?;
        let geometry = RawImageHeader::parse(raw_record)?;

        let camera_record = layout.record_data(frame, RecordType::CameraInfo)?;
        let mut metadata =
            FrameMetadata::parse_camera_info(camera_record, geometry.width, geometry.height)?;

        if options.gps {
            metadata.gps = layout
                .record(RecordType::GpsInfo)
                .and_then(|record| record.data(frame).ok())
                .and_then(GpsInfo::parse);
        }
        if options.palette {
            metadata.palette = layout
                .record(RecordType::PaletteInfo)
                .and_then(|record| record.data(frame).ok())
                .and_then(Palette::parse);
        }

        if !options.image {
            return Ok(Frame::new(
                FrameMetadata {
                    width: 0,
                    height: 0,
                    ..metadata
                },
                Vec::new(),
                0,
            ));
        }

        let encoded = &raw_record[RAW_HEADER_LEN..];
        self.jpegls.set_allow_truncated(options.allow_truncated);
        let decoded_samples = self.decode_image(encoded, geometry)?;

        Ok(Frame::new(
            metadata,
            std::mem::take(&mut self.pixels),
            decoded_samples,
        ))
    }

    /// Decodes the embedded image into `self.pixels`, dispatching on the
    /// container the camera used.
    fn decode_image(&mut self, encoded: &[u8], geometry: RawImageHeader) -> Result<usize> {
        let expected = (geometry.width, geometry.height);

        match ImageFormat::sniff(encoded) {
            ImageFormat::JpegLs => {
                let info = self.jpegls.decode_into(encoded, &mut self.pixels)?;
                if (info.width, info.height) != expected {
                    return Err(Error::DimensionMismatch {
                        expected,
                        actual: (info.width, info.height),
                    });
                }
                Ok(info.decoded_samples)
            }
            #[cfg(feature = "png")]
            ImageFormat::Png => self.decode_png(encoded, expected),
            #[cfg(not(feature = "png"))]
            ImageFormat::Png => Err(Error::UnsupportedRawImage {
                detail: "PNG-encoded thermal image, but the `png` feature is disabled".into(),
            }),
            ImageFormat::Raw16 => {
                let expected_len = geometry.width * geometry.height * 2;
                if encoded.len() < expected_len {
                    return Err(Error::Truncated {
                        what: "uncompressed thermal image",
                        offset: 0,
                    });
                }
                self.pixels.clear();
                self.pixels.extend(
                    encoded[..expected_len]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
                Ok(self.pixels.len())
            }
        }
    }

    #[cfg(feature = "png")]
    fn decode_png(&mut self, encoded: &[u8], expected: (usize, usize)) -> Result<usize> {
        let decoder = png::Decoder::new(encoded);
        let mut reader = decoder.read_info()?;
        let (color_type, bit_depth, actual) = {
            let info = reader.info();
            (
                info.color_type,
                info.bit_depth,
                (info.width as usize, info.height as usize),
            )
        };

        if color_type != png::ColorType::Grayscale {
            return Err(Error::UnsupportedRawImage {
                detail: format!("PNG thermal image with colour type {color_type:?}"),
            });
        }

        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buffer)?;
        let bytes = &buffer[..frame.buffer_size()];

        self.pixels.clear();
        match bit_depth {
            // PNG stores 16-bit samples big-endian.
            png::BitDepth::Sixteen => self.pixels.extend(
                bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]])),
            ),
            png::BitDepth::Eight => self.pixels.extend(bytes.iter().map(|&b| u16::from(b))),
            depth => {
                return Err(Error::UnsupportedRawImage {
                    detail: format!("PNG thermal image with bit depth {depth:?}"),
                })
            }
        }

        if actual != expected {
            return Err(Error::DimensionMismatch { expected, actual });
        }
        Ok(self.pixels.len())
    }
}

/// How the raw thermal image is encoded inside its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    /// JPEG-LS, what every CSQ recording observed so far uses.
    JpegLs,
    /// 16-bit grayscale PNG, used by some radiometric still formats.
    Png,
    /// Uncompressed little-endian 16-bit samples.
    Raw16,
}

impl ImageFormat {
    fn sniff(bytes: &[u8]) -> Self {
        const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        if bytes.starts_with(&PNG_MAGIC) {
            Self::Png
        } else if bytes.starts_with(&[0xff, 0xd8]) {
            Self::JpegLs
        } else {
            Self::Raw16
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_container_formats() {
        assert_eq!(
            ImageFormat::sniff(&[0xff, 0xd8, 0xff, 0xf7]),
            ImageFormat::JpegLs
        );
        assert_eq!(
            ImageFormat::sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]),
            ImageFormat::Png
        );
        assert_eq!(ImageFormat::sniff(&[0x00, 0x01, 0x02]), ImageFormat::Raw16);
    }

    #[test]
    fn decode_options_presets() {
        assert!(DecodeOptions::all().image);
        assert!(!DecodeOptions::metadata_only().image);
        assert!(DecodeOptions::metadata_only().palette);
        assert!(!DecodeOptions::default().image);
        assert!(!DecodeOptions::all().allow_truncated);
        assert!(DecodeOptions::tolerant().allow_truncated);
        assert!(DecodeOptions::tolerant().image);
    }
}
