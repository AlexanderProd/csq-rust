//! Writing CSQ recordings.
//!
//! [`CsqWriter`] converts thermal frames into the FFF frame format used by FLIR
//! recordings and appends them to any [`Write`] destination. Each frame stores
//! the calibration data needed to convert detector counts back to temperatures.
//!
//! ```no_run
//! # fn main() -> csq::Result<()> {
//! # let radiometric = csq::CsqFile::open("reference.csq")?.metadata().unwrap().radiometric;
//! # let frames: Vec<Vec<f32>> = Vec::new();
//! use csq::write::CsqWriter;
//!
//! let mut metadata = csq::FrameMetadata::new(640, 480, radiometric);
//! metadata.camera.model = "Bench rig".into();
//! metadata.frame_rate = Some(60.0);
//!
//! let mut writer = csq::write::create("out.csq", metadata)?;
//! for celsius in &frames {
//!     writer.write_celsius(celsius)?;
//! }
//! writer.finish()?;
//! # Ok(())
//! # }
//! ```
//!
//! # What has to be supplied
//!
//! CSQ files store detector counts rather than temperatures. Writing
//! temperatures therefore requires [`RadiometricParameters`] to perform the
//! reverse conversion. These values also determine the temperatures shown by a
//! reader. When possible, copy them from a recording made by the same camera.
//!
//! Other metadata, such as camera details, timestamps, GPS, and palette, is
//! optional.
//!
//! # Keeping up with a live feed
//!
//! Frames are encoded independently and written in recording order. By default,
//! JPEG-LS encoding runs on a small worker pool for better throughput.
//!
//! [`write`](CsqWriter::write) normally returns after queuing the frame. It
//! blocks only when the worker queue is full. Because encoding is asynchronous,
//! an error may be reported by a later `write` call or by
//! [`finish`](CsqWriter::finish).
//!
//! Use [`WriteOptions::single_threaded`] to encode each frame before `write`
//! returns. Both modes produce the same output and reuse their buffers.

mod frame;
mod pipeline;
mod records;

use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::metadata::{FrameMetadata, GpsInfo, Timestamp};
use crate::thermal::RadiometricParameters;

use frame::FrameEncoder;
use pipeline::Pipeline;

/// Controls the accuracy and size of the encoded thermal image.
///
/// FLIR cameras commonly use near-lossless compression and adjust the error
/// bound to control bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// Preserves every detector count exactly.
    #[default]
    Lossless,
    /// Allows each detector count to change by at most `near` for better
    /// compression.
    ///
    /// The temperature effect depends on the camera calibration.
    NearLossless {
        /// The error bound, in detector counts.
        near: u16,
    },
}

/// Settings for a [`CsqWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOptions {
    /// Compression used for each thermal image.
    pub compression: Compression,
    /// Producer name stored in every frame header.
    ///
    /// The default is FLIR's `RTP`. Only the first 15 bytes are stored.
    pub creator: String,
    /// Number of encoder threads.
    ///
    /// `0` selects a worker count automatically. `1` encodes on the calling
    /// thread. Larger values create that many workers. Thread count affects
    /// performance and when errors are reported, but not the output bytes.
    pub threads: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Lossless,
            creator: "RTP".into(),
            threads: 0,
        }
    }
}

impl WriteOptions {
    /// Creates options that encode and write each frame on the calling thread.
    ///
    /// This is useful when the application manages its own threads or needs an
    /// error to be returned by the exact `write` call that caused it.
    pub fn single_threaded() -> Self {
        Self {
            threads: 1,
            ..Self::default()
        }
    }

    /// Near-lossless coding with the given bound in detector counts.
    pub fn near_lossless(mut self, near: u16) -> Self {
        self.compression = Compression::NearLossless { near };
        self
    }

    /// Sets the encoder thread count. `0` selects it automatically.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// Resolves the configured thread count to the number actually used.
    fn worker_count(&self) -> usize {
        match self.threads {
            0 => {
                std::thread::available_parallelism().map_or(1, |cores| default_workers(cores.get()))
            }
            n => n,
        }
    }
}

/// Number of CPU cores reserved for frame capture and other application work.
///
/// Using every core for encoding can delay a live camera's capture loop.
const PRODUCER_CORES: usize = 2;

/// Chooses the automatic worker count for a machine with `cores` CPU cores.
///
/// Multi-core systems use between two and eight workers while reserving
/// [`PRODUCER_CORES`] cores when possible.
fn default_workers(cores: usize) -> usize {
    match cores {
        1 => 1,
        cores => cores.saturating_sub(PRODUCER_CORES).clamp(2, 8),
    }
}

/// Pixel data accepted for one frame.
#[derive(Debug, Clone, Copy)]
pub enum FramePixels<'a> {
    /// Temperatures in °C. `NaN` marks a pixel with no reading.
    Celsius(&'a [f32]),
    /// Temperatures in kelvin.
    Kelvin(&'a [f32]),
    /// Raw detector counts in the format stored by CSQ.
    Raw(&'a [u16]),
}

/// A thermal frame and its optional per-frame metadata.
///
/// Per-frame metadata overrides the recording-wide defaults.
///
/// ```
/// # let celsius = vec![20.0f32; 16];
/// use csq::metadata::Timestamp;
/// use csq::write::WriteFrame;
///
/// let frame = WriteFrame::celsius(&celsius).at(Timestamp::now(0));
/// # let _ = frame;
/// ```
#[derive(Debug, Clone)]
pub struct WriteFrame<'a> {
    pixels: FramePixels<'a>,
    timestamp: Option<Timestamp>,
    gps: Option<GpsInfo>,
    radiometric: Option<RadiometricParameters>,
}

impl<'a> WriteFrame<'a> {
    /// Creates a frame from row-major temperatures in °C.
    pub fn celsius(celsius: &'a [f32]) -> Self {
        Self::new(FramePixels::Celsius(celsius))
    }

    /// Creates a frame from row-major temperatures in kelvin.
    pub fn kelvin(kelvin: &'a [f32]) -> Self {
        Self::new(FramePixels::Kelvin(kelvin))
    }

    /// Creates a frame from row-major detector counts.
    ///
    /// No temperature conversion is performed.
    pub fn raw(counts: &'a [u16]) -> Self {
        Self::new(FramePixels::Raw(counts))
    }

    /// Creates a frame from any supported pixel representation.
    pub fn new(pixels: FramePixels<'a>) -> Self {
        Self {
            pixels,
            timestamp: None,
            gps: None,
            radiometric: None,
        }
    }

    /// Sets the capture time.
    pub fn at(mut self, timestamp: Timestamp) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Sets the frame's GPS position.
    pub fn with_gps(mut self, gps: GpsInfo) -> Self {
        self.gps = Some(gps);
        self
    }

    /// Sets scene and calibration parameters for this frame.
    ///
    /// These parameters are used for temperature conversion and stored in the
    /// output frame.
    pub fn with_radiometric(mut self, parameters: RadiometricParameters) -> Self {
        self.radiometric = Some(parameters);
        self
    }

    /// Returns this frame's pixel data.
    pub fn pixels(&self) -> FramePixels<'a> {
        self.pixels
    }
}

/// Creates a buffered CSQ file using the default write options.
///
/// Buffering avoids a separate system call for every record.
pub fn create(path: impl AsRef<Path>, metadata: FrameMetadata) -> Result<CsqWriter<BufFile>> {
    create_with_options(path, metadata, WriteOptions::default())
}

/// Creates a buffered CSQ file using custom write options.
pub fn create_with_options(
    path: impl AsRef<Path>,
    metadata: FrameMetadata,
    options: WriteOptions,
) -> Result<CsqWriter<BufFile>> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    CsqWriter::with_options(file, metadata, options)
}

/// Buffered file type returned by [`create`] and [`create_with_options`].
pub type BufFile = std::io::BufWriter<std::fs::File>;

/// Streams thermal frames into a CSQ recording.
///
/// Call [`finish`] to encode pending frames, flush the sink, and receive any
/// final error. Dropping the writer attempts to finish but cannot report
/// failures.
///
/// [`finish`]: CsqWriter::finish
pub struct CsqWriter<W: Write> {
    sink: Option<W>,
    encoding: Encoding,
    /// Output buffer reused by single-threaded encoding.
    buffer: Vec<u8>,
    frames_written: u64,
    bytes_written: u64,
    /// Prevents further writes after an error leaves the recording incomplete.
    failed: bool,
}

/// Selects single-threaded or worker-pool encoding.
enum Encoding {
    Inline(Box<FrameEncoder>),
    Pooled(Box<Pipeline>),
}

impl<W: Write> CsqWriter<W> {
    /// Creates a lossless writer using the default thread settings.
    pub fn new(sink: W, metadata: FrameMetadata) -> Result<Self> {
        Self::with_options(sink, metadata, WriteOptions::default())
    }

    /// Creates a writer with custom compression and thread settings.
    pub fn with_options(sink: W, metadata: FrameMetadata, options: WriteOptions) -> Result<Self> {
        let workers = options.worker_count();
        let encoding = if workers <= 1 {
            Encoding::Inline(Box::new(FrameEncoder::new(&metadata, &options)?))
        } else {
            Encoding::Pooled(Box::new(Pipeline::start(&metadata, &options, workers)?))
        };

        Ok(Self {
            sink: Some(sink),
            encoding,
            buffer: Vec::new(),
            frames_written: 0,
            bytes_written: 0,
            failed: false,
        })
    }

    /// Returns the required frame width and height.
    pub fn dimensions(&self) -> (usize, usize) {
        match &self.encoding {
            Encoding::Inline(encoder) => encoder.dimensions(),
            Encoding::Pooled(pipeline) => pipeline.dimensions(),
        }
    }

    /// Returns the number of frames accepted by the writer.
    ///
    /// With encoder threads some of them may still be in flight; [`finish`]
    /// is what guarantees they have reached the sink.
    ///
    /// [`finish`]: CsqWriter::finish
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Returns the number of bytes written to the sink so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Writes one frame of row-major temperatures in °C.
    pub fn write_celsius(&mut self, celsius: &[f32]) -> Result<()> {
        self.write(WriteFrame::celsius(celsius))
    }

    /// Writes one frame of row-major detector counts.
    pub fn write_raw(&mut self, counts: &[u16]) -> Result<()> {
        self.write(WriteFrame::raw(counts))
    }

    /// Submits one frame for encoding and writing.
    ///
    /// With multiple encoder threads, this usually returns after queueing the
    /// frame and blocks only while the queue is full.
    pub fn write(&mut self, frame: WriteFrame<'_>) -> Result<()> {
        if self.failed {
            return Err(Error::Unwritable {
                detail: "the recording already failed and cannot take more frames",
            });
        }
        let result = self.write_inner(frame);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn write_inner(&mut self, frame: WriteFrame<'_>) -> Result<()> {
        let sequence = self.frames_written as u32;
        match &mut self.encoding {
            Encoding::Inline(encoder) => {
                encoder.encode_into(&frame, sequence, &mut self.buffer)?;
                let sink = self.sink.as_mut().expect("the sink outlives every write");
                sink.write_all(&self.buffer)?;
                self.bytes_written += self.buffer.len() as u64;
            }
            Encoding::Pooled(pipeline) => {
                let sink = self.sink.as_mut().expect("the sink outlives every write");
                self.bytes_written += pipeline.submit(&frame, sequence, sink)?;
            }
        }
        self.frames_written += 1;
        Ok(())
    }

    /// Finishes all pending work, flushes, and returns the output sink.
    ///
    /// Waits for any frames still being encoded, writes them, and flushes.
    pub fn finish(mut self) -> Result<W> {
        self.drain()?;
        let mut sink = self.sink.take().expect("finish is only reachable once");
        sink.flush()?;
        Ok(sink)
    }

    /// Writes all frames still being processed by the worker pool.
    fn drain(&mut self) -> Result<()> {
        let Some(sink) = self.sink.as_mut() else {
            return Ok(());
        };
        if let Encoding::Pooled(pipeline) = &mut self.encoding {
            self.bytes_written += pipeline.drain(sink)?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for CsqWriter<W> {
    fn drop(&mut self) {
        // Best-effort cleanup for callers that did not call finish. Errors
        // cannot be reported from Drop.
        if self.sink.is_some() && !self.failed {
            let _ = self.drain();
            if let Some(sink) = self.sink.as_mut() {
                let _ = sink.flush();
            }
        }
    }
}

impl<W: Write> std::fmt::Debug for CsqWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsqWriter")
            .field("dimensions", &self.dimensions())
            .field("frames_written", &self.frames_written)
            .field("bytes_written", &self.bytes_written)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::default_workers;

    #[test]
    fn the_default_leaves_cores_for_the_producer() {
        assert_eq!(default_workers(1), 1);
        assert_eq!(default_workers(2), 2);
        assert_eq!(default_workers(4), 2, "a Raspberry Pi 4");
        assert_eq!(default_workers(6), 4);
        assert_eq!(default_workers(10), 8);
        assert_eq!(default_workers(64), 8);
    }
}
