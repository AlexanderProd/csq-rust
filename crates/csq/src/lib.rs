//! Read and write FLIR CSQ thermal recordings: per-pixel temperatures, frame
//! indexing, seeking and rendering — in pure Rust, with no `exiftool` process
//! and no C++ toolchain.
//!
//! # What a CSQ file is
//!
//! A `.csq` recording is a bare concatenation of FLIR FFF frames. Each frame
//! carries a record directory pointing at its radiometric image (a JPEG-LS
//! stream of 16-bit detector counts) and at the calibration record needed to
//! turn those counts into temperatures. There is no container index, but every
//! frame header states its own length, which is enough to index a recording by
//! reading 64 bytes per frame.
//!
//! # Reading a recording
//!
//! [`CsqFile`] indexes a file on open and decodes any frame on demand:
//!
//! ```no_run
//! # fn main() -> csq::Result<()> {
//! let file = csq::CsqFile::open("recording.csq")?;
//! println!("{} frames, {:?}", file.len(), file.duration());
//!
//! let frame = file.frame(0)?;
//! let temperatures = frame.temperatures();
//! let (coldest, warmest) = temperatures.range().unwrap();
//! println!("{coldest:.1} °C to {warmest:.1} °C");
//! # Ok(())
//! # }
//! ```
//!
//! # Playback and scrubbing
//!
//! [`CsqReader`] keeps its decoding buffers between frames, and seeking is just
//! moving a cursor — no re-scanning, no keyframe hunting, because every CSQ
//! frame is independent.
//!
//! ```no_run
//! # fn main() -> csq::Result<()> {
//! use std::time::Duration;
//!
//! let file = csq::CsqFile::open("recording.csq")?;
//! let mut reader = file.reader();
//!
//! reader.seek_to_time(Duration::from_secs(12))?;
//! let frame = reader.next_frame().unwrap()?;
//! # let _ = frame;
//! # Ok(())
//! # }
//! ```
//!
//! For sources that cannot seek, [`CsqStream`] decodes from any [`Read`].
//!
//! # Temperatures
//!
//! Frames hold raw detector counts, because converting them depends on scene
//! parameters — emissivity, distance, humidity — that a caller may well want to
//! change after the fact. [`Frame::temperatures`] uses the values the camera
//! recorded; overriding them is a matter of editing
//! [`RadiometricParameters`] and building a [`TemperatureTable`]:
//!
//! ```no_run
//! # fn main() -> csq::Result<()> {
//! let file = csq::CsqFile::open("recording.csq")?;
//! let mut reader = file.reader();
//!
//! let mut parameters = file.metadata().unwrap().radiometric;
//! parameters.emissivity = 0.95;
//! parameters.object_distance = 3.0;
//! let table = parameters.temperature_table();
//!
//! while let Some(frame) = reader.next_frame() {
//!     let celsius = frame?.temperatures_with(&table);
//!     # let _ = celsius;
//!     # break;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Reusing one [`TemperatureTable`] matters: it turns the per-pixel
//! exponential and logarithm into a table lookup.
//!
//! # Writing
//!
//! [`write::CsqWriter`] runs the whole thing backwards: frames of temperatures
//! in, a recording FLIR's own tools will open out. Encoding is spread over a
//! thread pool by default, so a live 60 fps feed at a large sensor size does
//! not have to wait for it.
//!
//! ```no_run
//! # fn main() -> csq::Result<()> {
//! # let radiometric = csq::CsqFile::open("reference.csq")?.metadata().unwrap().radiometric;
//! # let feed: Vec<Vec<f32>> = Vec::new();
//! let mut metadata = csq::FrameMetadata::new(640, 480, radiometric);
//! metadata.frame_rate = Some(60.0);
//!
//! let mut writer = csq::write::create("out.csq", metadata)?;
//! for celsius in &feed {
//!     writer.write_celsius(celsius)?;
//! }
//! writer.finish()?;
//! # Ok(())
//! # }
//! ```
//!
//! The [`RadiometricParameters`] are what make the file mean something: they
//! are how a reader turns the stored counts back into °C, so writing needs a
//! calibration the same way reading does. See [`write`] for the details.
//!
//! # Rendering
//!
//! [`render::Renderer`] maps temperatures onto a colour ramp and writes plain
//! RGB8, which any image or video crate will accept.
//!
//! # Damaged and unusual recordings
//!
//! Real recordings are not always tidy. Files cut off mid-write, frames padded
//! to a fixed slot size, containers written big-endian by some camera models,
//! and frames whose image data simply stops early all occur in the wild, and
//! all are handled — see [`FrameIndex`] for what the index reports and
//! [`DecodeOptions::tolerant`] for salvaging a frame whose image was cut short.
//!
//! # Feature flags
//!
//! | Feature | Default | Effect |
//! |---------|---------|--------|
//! | `mmap` | yes | Memory-map files instead of reading them into memory |
//! | `png` | yes | Decode FLIR files whose thermal image is PNG rather than JPEG-LS |
//! | `rayon` | no | [`CsqFile::decode_all`] for parallel decoding |
//! | `serde` | no | `Serialize`/`Deserialize` for the metadata types |
//! | `ndarray` | no | [`TemperatureImage::to_array2`] |
//!
//! [`Read`]: std::io::Read

#![warn(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod fff;
pub mod jpegls;
pub mod metadata;
pub mod render;
pub mod write;

mod decode;
mod error;
mod file;
mod frame;
mod index;
mod stream;
mod thermal;

pub use decode::{DecodeOptions, FrameDecoder};
pub use error::{Error, Result};
pub use file::{CsqFile, CsqReader, Frames};
pub use frame::{Frame, TemperatureImage};
pub use index::{FrameIndex, FrameLocation};
pub use metadata::FrameMetadata;
pub use stream::{CsqStream, StreamFrames};
pub use thermal::{
    AtmosphericTransmission, PlanckConstants, RadiometricParameters, RawConversion,
    TemperatureTable,
};
pub use write::CsqWriter;

#[cfg(test)]
pub(crate) mod test_support {
    use crate::frame::TemperatureImage;
    use crate::thermal::{AtmosphericTransmission, PlanckConstants, RadiometricParameters};

    /// Calibration of the FLIR T1020 the test fixtures came from.
    pub fn parameters() -> RadiometricParameters {
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

    /// Builds a `TemperatureImage` directly, for tests that do not need a file.
    pub fn temperature_image(width: usize, height: usize, celsius: &[f32]) -> TemperatureImage {
        assert_eq!(width * height, celsius.len());
        // Round-tripping through a Frame would need a full metadata record, so
        // reuse the crate-internal constructor instead.
        crate::frame::TemperatureImage::from_parts(width, height, celsius.to_vec())
    }
}
