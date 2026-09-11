//! Writing CSQ recordings.
//!
//! A [`CsqWriter`] takes frames of temperatures and appends them to any
//! [`Write`] sink as complete FFF frames — the same container FLIR's own
//! recorder produces, with the calibration a viewer needs to turn the counts
//! back into °C.
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
//! Temperatures are not what a CSQ file stores. The camera records raw detector
//! counts, and a reader turns those into °C with the Planck constants and scene
//! parameters in the frame — so writing runs that backwards, and the
//! [`RadiometricParameters`] are not optional decoration. Whatever is written
//! there is what a viewer will measure with. The most reliable source is a
//! recording from the camera the data came from; failing that, the constants
//! decide the mapping and any self-consistent set round-trips exactly.
//!
//! Everything else — camera and lens identification, capture time, GPS,
//! display palette — is carried through if given and left empty if not.
//!
//! # Keeping up with a live feed
//!
//! Frames are independent, so writing is a streaming operation with no index to
//! fix up at the end and nothing buffered beyond the frames in hand. The cost
//! is dominated by JPEG-LS encoding, which by default runs on a small pool of
//! worker threads while frames still reach the sink in recording order. On a
//! ten-core M-series Mac that writes 640×480 at around 1000 fps and 1024×768 at
//! around 350, against 130 and 50 on one core — so the pool is the difference
//! between keeping up with a 60 fps feed at a large sensor and not.
//!
//! [`write`](CsqWriter::write) hands a frame over and returns, blocking only
//! when the encoders are already a couple of frames behind, so a producer
//! running at frame rate stays at frame rate. Errors from a frame that was
//! still being encoded surface on a later [`write`](CsqWriter::write) or on
//! [`finish`](CsqWriter::finish), which is the trade for not waiting.
//!
//! [`WriteOptions::single_threaded`] turns that off and encodes each frame
//! before returning. Both paths produce the same bytes, and neither allocates
//! per frame once warm — the encoder, the count buffer and the frame buffers
//! are all recycled.

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

/// How the thermal image of each frame is coded.
///
/// Cameras code near-lossless, and vary the bound from frame to frame to hold a
/// bitrate. A recording written here holds whatever bound it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// Every count comes back exactly as it went in.
    #[default]
    Lossless,
    /// No count moves by more than `near`, in exchange for a much smaller file.
    ///
    /// Cameras use 4 to 16 depending on the scene. What that is worth in
    /// temperature depends on the calibration: it is a bound on counts, and
    /// around room temperature a count is a small fraction of a degree.
    NearLossless {
        /// The error bound, in detector counts.
        near: u16,
    },
}

/// Settings for a [`CsqWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOptions {
    /// How to code the thermal image.
    pub compression: Compression,
    /// The producer name written into every frame header.
    ///
    /// Cameras write `RTP`, which is what a reader expecting camera output will
    /// find familiar. At most 15 bytes are kept.
    pub creator: String,
    /// Encoder threads.
    ///
    /// `0`, the default, picks a count from the machine. `1` encodes on the
    /// calling thread and writes each frame before returning; anything higher
    /// uses that many workers. Frames reach the sink in order and come out
    /// byte-for-byte the same whichever is chosen — only the throughput and the
    /// point at which errors surface differ.
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
    /// Encoding on the calling thread, with each frame written before
    /// [`write`](CsqWriter::write) returns.
    ///
    /// Worth choosing when the caller has its own idea about threads, or wants
    /// every error reported by the call that caused it. One core encodes a
    /// 640×480 frame in about seven milliseconds, so this keeps up with a
    /// 60 fps feed at that size but not at 1024×768.
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

    /// Sets the number of encoder threads; `0` picks a count from the machine.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// How many worker threads to actually start.
    fn worker_count(&self) -> usize {
        match self.threads {
            1 => 1,
            0 => std::thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 8)),
            n => n,
        }
    }
}

/// The pixels of one frame.
#[derive(Debug, Clone, Copy)]
pub enum FramePixels<'a> {
    /// Temperatures in °C. `NaN` marks a pixel with no reading.
    Celsius(&'a [f32]),
    /// Temperatures in kelvin.
    Kelvin(&'a [f32]),
    /// Raw detector counts, already in the form the file stores.
    Raw(&'a [u16]),
}

/// One frame on its way into a recording.
///
/// The pixels are required; everything else is per-frame detail that overrides
/// what the recording's metadata says.
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
    /// A frame of temperatures in °C, in row-major order.
    pub fn celsius(celsius: &'a [f32]) -> Self {
        Self::new(FramePixels::Celsius(celsius))
    }

    /// A frame of temperatures in kelvin, in row-major order.
    pub fn kelvin(kelvin: &'a [f32]) -> Self {
        Self::new(FramePixels::Kelvin(kelvin))
    }

    /// A frame of raw detector counts, in row-major order.
    ///
    /// The fastest and most faithful input when the counts are already at hand
    /// — copying frames out of another recording, for instance — because
    /// nothing is converted on the way in.
    pub fn raw(counts: &'a [u16]) -> Self {
        Self::new(FramePixels::Raw(counts))
    }

    /// A frame from any of the pixel forms.
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

    /// Attaches a position fix.
    pub fn with_gps(mut self, gps: GpsInfo) -> Self {
        self.gps = Some(gps);
        self
    }

    /// Overrides the scene and calibration parameters for this frame.
    ///
    /// Both the conversion from temperatures and the parameters recorded in the
    /// frame follow, so a recording whose emissivity or object distance changes
    /// part-way through stays readable.
    pub fn with_radiometric(mut self, parameters: RadiometricParameters) -> Self {
        self.radiometric = Some(parameters);
        self
    }

    /// The pixels this frame carries.
    pub fn pixels(&self) -> FramePixels<'a> {
        self.pixels
    }
}

/// Creates a CSQ file and returns a writer for it.
///
/// The file is buffered, so a caller handing over one frame at a time does not
/// pay for a write syscall per record.
pub fn create(path: impl AsRef<Path>, metadata: FrameMetadata) -> Result<CsqWriter<BufFile>> {
    create_with_options(path, metadata, WriteOptions::default())
}

/// [`create`], with settings.
pub fn create_with_options(
    path: impl AsRef<Path>,
    metadata: FrameMetadata,
    options: WriteOptions,
) -> Result<CsqWriter<BufFile>> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    CsqWriter::with_options(file, metadata, options)
}

/// The sink [`create`] writes through.
pub type BufFile = std::io::BufWriter<std::fs::File>;

/// Appends frames to a CSQ recording.
///
/// A recording is a bare concatenation of frames, so there is no header to
/// write up front and no index to fix up at the end — but [`finish`] still has
/// to be called to flush the sink and report anything that went wrong. Dropping
/// a writer flushes what it can and discards the errors.
///
/// [`finish`]: CsqWriter::finish
pub struct CsqWriter<W: Write> {
    sink: Option<W>,
    encoding: Encoding,
    /// Reused whenever frames are encoded on this thread.
    buffer: Vec<u8>,
    frames_written: u64,
    bytes_written: u64,
    /// Set once an error has been reported, because the recording is then
    /// missing a frame and every later one would be silently misplaced.
    failed: bool,
}

/// Where frames are encoded.
enum Encoding {
    Inline(Box<FrameEncoder>),
    Pooled(Box<Pipeline>),
}

impl<W: Write> CsqWriter<W> {
    /// Starts a recording with the default settings: lossless, encoded on a
    /// pool of worker threads.
    pub fn new(sink: W, metadata: FrameMetadata) -> Result<Self> {
        Self::with_options(sink, metadata, WriteOptions::default())
    }

    /// Starts a recording with the given settings.
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

    /// The frame geometry this recording was opened with.
    pub fn dimensions(&self) -> (usize, usize) {
        match &self.encoding {
            Encoding::Inline(encoder) => encoder.dimensions(),
            Encoding::Pooled(pipeline) => pipeline.dimensions(),
        }
    }

    /// How many frames have been handed over.
    ///
    /// With encoder threads some of them may still be in flight; [`finish`]
    /// is what guarantees they have reached the sink.
    ///
    /// [`finish`]: CsqWriter::finish
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// How many bytes have reached the sink so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Appends a frame of temperatures in °C.
    pub fn write_celsius(&mut self, celsius: &[f32]) -> Result<()> {
        self.write(WriteFrame::celsius(celsius))
    }

    /// Appends a frame of raw detector counts.
    pub fn write_raw(&mut self, counts: &[u16]) -> Result<()> {
        self.write(WriteFrame::raw(counts))
    }

    /// Appends a frame.
    ///
    /// With encoder threads this returns as soon as the frame has been queued,
    /// and blocks only when the queue is full — so a producer running at frame
    /// rate stays at frame rate.
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

    /// Finishes the recording and returns the sink.
    ///
    /// Waits for any frames still being encoded, writes them, and flushes.
    pub fn finish(mut self) -> Result<W> {
        self.drain()?;
        let mut sink = self.sink.take().expect("finish is only reachable once");
        sink.flush()?;
        Ok(sink)
    }

    /// Writes out everything still in flight.
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
        // A caller who never reached `finish` still gets a complete file where
        // that is possible; there is nowhere to report a failure to here.
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
