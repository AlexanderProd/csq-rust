//! Random access to a CSQ file: the type to reach for when scrubbing.

use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

use crate::decode::{DecodeOptions, FrameDecoder};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::index::{FrameIndex, FrameLocation};
use crate::metadata::FrameMetadata;

/// Backing store for a file's bytes.
enum Storage {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl Deref for Storage {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped(map) => map,
            Self::Owned(bytes) => bytes,
        }
    }
}

/// An indexed CSQ recording.
///
/// Opening a file walks its frame headers, so `CsqFile` knows the frame count
/// up front and can jump straight to any frame. It holds no decoder state and
/// is `Send + Sync`, which lets several threads decode from the same recording
/// at once — each with its own [`CsqReader`] or [`FrameDecoder`].
///
/// ```no_run
/// # fn main() -> csq::Result<()> {
/// let file = csq::CsqFile::open("recording.csq")?;
/// println!("{} frames at {:?} fps", file.len(), file.frame_rate());
///
/// // Jump to five seconds in.
/// let index = file.frame_index_at(std::time::Duration::from_secs(5)).unwrap();
/// let frame = file.frame(index)?;
/// println!("centre pixel: {:.1} °C", frame.temperature_at(
///     frame.width() / 2,
///     frame.height() / 2,
/// )?);
/// # Ok(())
/// # }
/// ```
pub struct CsqFile {
    storage: Storage,
    index: FrameIndex,
    first_metadata: Option<FrameMetadata>,
}

impl CsqFile {
    /// Opens and indexes a CSQ file.
    ///
    /// With the `mmap` feature (on by default) the file is memory-mapped, so
    /// opening a multi-gigabyte recording costs the same as opening a small
    /// one and individual frames are paged in on demand.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        #[cfg(feature = "mmap")]
        {
            let file = std::fs::File::open(path)?;
            // Safety: the mapping is read-only and kept private to this value.
            // A concurrent external truncation could still fault, which is the
            // same caveat every mmap-based reader carries.
            let map = unsafe { memmap2::Mmap::map(&file)? };
            Self::from_storage(Storage::Mapped(map))
        }
        #[cfg(not(feature = "mmap"))]
        {
            Self::from_bytes(std::fs::read(path)?)
        }
    }

    /// Indexes a recording already held in memory.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_storage(Storage::Owned(bytes))
    }

    fn from_storage(storage: Storage) -> Result<Self> {
        let index = FrameIndex::build(&storage);

        // Parse the first frame's metadata once so frame rate, geometry and
        // camera identification are available without decoding an image.
        let first_metadata = index
            .get(0)
            .and_then(|location| {
                storage.get(location.range()).map(|bytes| {
                    FrameDecoder::new().decode(
                        bytes,
                        location.offset,
                        DecodeOptions::metadata_only(),
                    )
                })
            })
            .transpose()?
            .map(|frame| frame.metadata().clone());

        Ok(Self {
            storage,
            index,
            first_metadata,
        })
    }

    /// Number of frames in the recording.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the recording holds no frames.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The frame index built when the file was opened.
    pub fn index(&self) -> &FrameIndex {
        &self.index
    }

    /// The whole file as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.storage
    }

    /// The bytes of one frame, ready to hand to a [`FrameDecoder`].
    pub fn frame_bytes(&self, index: usize) -> Result<&[u8]> {
        let location = self.location(index)?;
        self.storage.get(location.range()).ok_or(Error::Truncated {
            what: "frame",
            offset: location.offset,
        })
    }

    fn location(&self, index: usize) -> Result<FrameLocation> {
        self.index.get(index).ok_or(Error::FrameOutOfRange {
            index,
            len: self.index.len(),
        })
    }

    /// Metadata of the first frame, parsed when the file was opened.
    ///
    /// Radiometric parameters, camera model and geometry are effectively
    /// constant across a recording, so this is usually all a caller needs.
    pub fn metadata(&self) -> Option<&FrameMetadata> {
        self.first_metadata.as_ref()
    }

    /// Image dimensions, as `(width, height)`.
    pub fn dimensions(&self) -> Option<(usize, usize)> {
        self.first_metadata
            .as_ref()
            .map(|_| self.frame_dimensions())
            .filter(|(w, h)| *w > 0 && *h > 0)
    }

    fn frame_dimensions(&self) -> (usize, usize) {
        // `metadata_only` decoding zeroes the geometry, so read it back from
        // the raw-data record header of the first frame.
        self.frame_bytes(0)
            .ok()
            .and_then(|bytes| {
                crate::fff::FrameLayout::parse(bytes, 0)
                    .ok()
                    .map(|l| (bytes, l))
            })
            .and_then(|(bytes, layout)| {
                layout
                    .record_data(bytes, crate::fff::RecordType::RawData)
                    .ok()
                    .filter(|record| record.len() >= 6)
                    .map(|record| {
                        (
                            usize::from(u16::from_le_bytes([record[2], record[3]])),
                            usize::from(u16::from_le_bytes([record[4], record[5]])),
                        )
                    })
            })
            .unwrap_or((0, 0))
    }

    /// Recording frame rate in frames per second.
    pub fn frame_rate(&self) -> Option<f32> {
        self.first_metadata
            .as_ref()
            .and_then(|metadata| metadata.frame_rate)
            .filter(|rate| *rate > 0.0)
    }

    /// Total duration, derived from the frame count and the frame rate.
    pub fn duration(&self) -> Option<Duration> {
        let rate = self.frame_rate()?;
        Some(Duration::from_secs_f64(self.len() as f64 / f64::from(rate)))
    }

    /// Presentation time of a frame, measured from the start of the recording.
    pub fn frame_time(&self, index: usize) -> Option<Duration> {
        let rate = self.frame_rate()?;
        (index < self.len()).then(|| Duration::from_secs_f64(index as f64 / f64::from(rate)))
    }

    /// The frame shown at `position`, for scrubbing a timeline.
    ///
    /// Positions past the end clamp to the last frame; `None` only when the
    /// file is empty or has no frame rate.
    pub fn frame_index_at(&self, position: Duration) -> Option<usize> {
        let rate = self.frame_rate()?;
        if self.is_empty() {
            return None;
        }
        let index = (position.as_secs_f64() * f64::from(rate)).floor();
        Some((index.max(0.0) as usize).min(self.len() - 1))
    }

    /// Creates a reader that reuses its decoding buffers across frames.
    ///
    /// This is the efficient way to walk or scrub a file; [`CsqFile::frame`]
    /// allocates a fresh decoder on every call.
    pub fn reader(&self) -> CsqReader<'_> {
        CsqReader::new(self)
    }

    /// Decodes a single frame.
    ///
    /// Convenient for one-off access. When reading more than a couple of
    /// frames, use [`CsqFile::reader`] instead.
    pub fn frame(&self, index: usize) -> Result<Frame> {
        FrameDecoder::new().decode(
            self.frame_bytes(index)?,
            self.location(index)?.offset,
            DecodeOptions::all(),
        )
    }

    /// Decodes a frame's metadata without decoding its image.
    pub fn frame_metadata(&self, index: usize) -> Result<FrameMetadata> {
        Ok(FrameDecoder::new()
            .decode(
                self.frame_bytes(index)?,
                self.location(index)?.offset,
                DecodeOptions::metadata_only(),
            )?
            .metadata()
            .clone())
    }

    /// Decodes every frame in parallel, in order.
    ///
    /// Each worker keeps its own decoder. Memory use scales with the number of
    /// frames, so prefer [`CsqFile::reader`] for long recordings.
    #[cfg(feature = "rayon")]
    pub fn decode_all(&self) -> Vec<Result<Frame>> {
        use rayon::prelude::*;
        (0..self.len())
            .into_par_iter()
            .map_init(FrameDecoder::new, |decoder, index| {
                let location = self.location(index)?;
                decoder.decode(
                    self.frame_bytes(index)?,
                    location.offset,
                    DecodeOptions::all(),
                )
            })
            .collect()
    }
}

impl std::fmt::Debug for CsqFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsqFile")
            .field("frames", &self.len())
            .field("dimensions", &self.dimensions())
            .field("frame_rate", &self.frame_rate())
            .finish_non_exhaustive()
    }
}

/// A cursor over a [`CsqFile`] that reuses its decoding buffers.
///
/// Readers are cheap to create and independent of each other, so a UI can keep
/// one for playback and another for a scrub preview.
///
/// ```no_run
/// # fn main() -> csq::Result<()> {
/// let file = csq::CsqFile::open("recording.csq")?;
/// let mut reader = file.reader();
///
/// // Sequential playback.
/// while let Some(frame) = reader.next_frame() {
///     let frame = frame?;
///     // ... render frame ...
///     # let _ = frame;
///     # break;
/// }
///
/// // Or jump straight to a frame.
/// let frame = reader.frame(120)?;
/// # let _ = frame;
/// # Ok(())
/// # }
/// ```
pub struct CsqReader<'a> {
    file: &'a CsqFile,
    decoder: FrameDecoder,
    options: DecodeOptions,
    position: usize,
}

impl<'a> CsqReader<'a> {
    fn new(file: &'a CsqFile) -> Self {
        Self {
            file,
            decoder: FrameDecoder::new(),
            options: DecodeOptions::all(),
            position: 0,
        }
    }

    /// Sets which parts of each frame to decode.
    pub fn with_options(mut self, options: DecodeOptions) -> Self {
        self.options = options;
        self
    }

    /// The file being read.
    pub fn file(&self) -> &'a CsqFile {
        self.file
    }

    /// Index of the frame [`next_frame`](Self::next_frame) will return.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Moves the cursor, without decoding anything.
    ///
    /// Seeking to `file.len()` is allowed and leaves the reader at the end.
    pub fn seek(&mut self, index: usize) -> Result<()> {
        if index > self.file.len() {
            return Err(Error::FrameOutOfRange {
                index,
                len: self.file.len(),
            });
        }
        self.position = index;
        Ok(())
    }

    /// Moves the cursor to the frame shown at `position` on the timeline.
    pub fn seek_to_time(&mut self, position: Duration) -> Result<usize> {
        let index = self
            .file
            .frame_index_at(position)
            .ok_or(Error::FrameOutOfRange {
                index: 0,
                len: self.file.len(),
            })?;
        self.position = index;
        Ok(index)
    }

    /// Decodes the frame at the cursor and advances by one.
    ///
    /// Returns `None` at the end of the file.
    pub fn next_frame(&mut self) -> Option<Result<Frame>> {
        if self.position >= self.file.len() {
            return None;
        }
        let index = self.position;
        self.position += 1;
        Some(self.decode(index))
    }

    /// Decodes an arbitrary frame and leaves the cursor just after it.
    pub fn frame(&mut self, index: usize) -> Result<Frame> {
        let frame = self.decode(index)?;
        self.position = index + 1;
        Ok(frame)
    }

    fn decode(&mut self, index: usize) -> Result<Frame> {
        let location = self.file.location(index)?;
        let bytes = self.file.frame_bytes(index)?;
        self.decoder.decode(bytes, location.offset, self.options)
    }

    /// Iterates over the remaining frames.
    pub fn frames(&mut self) -> Frames<'_, 'a> {
        Frames { reader: self }
    }
}

/// Iterator over the frames left in a [`CsqReader`].
pub struct Frames<'r, 'a> {
    reader: &'r mut CsqReader<'a>,
}

impl Iterator for Frames<'_, '_> {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next_frame()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.reader.file.len().saturating_sub(self.reader.position);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Frames<'_, '_> {}
