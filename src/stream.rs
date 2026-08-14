//! Sequential decoding from any [`Read`] source.
//!
//! Use this when the recording is not a seekable file — a socket, a pipe, or a
//! decompressor. Frames arrive in order and nothing is buffered beyond the
//! frame currently being decoded. When the source *is* a file and random access
//! matters, [`CsqFile`](crate::CsqFile) is the better fit.

use std::io::{ErrorKind, Read};

use crate::decode::{DecodeOptions, FrameDecoder};
use crate::error::{Error, Result};
use crate::fff::{self, HEADER_LEN, MAGIC};
use crate::frame::Frame;

/// Streams frames out of a CSQ byte source.
///
/// ```no_run
/// # fn main() -> csq::Result<()> {
/// let file = std::fs::File::open("recording.csq")?;
/// let mut stream = csq::CsqStream::new(std::io::BufReader::new(file));
///
/// while let Some(frame) = stream.next_frame() {
///     let frame = frame?;
///     println!("{}x{}", frame.width(), frame.height());
///     # break;
/// }
/// # Ok(())
/// # }
/// ```
pub struct CsqStream<R> {
    source: R,
    decoder: FrameDecoder,
    options: DecodeOptions,
    /// Scratch buffer holding the frame currently being decoded.
    buffer: Vec<u8>,
    offset: u64,
    frames_read: usize,
    finished: bool,
}

impl<R: Read> CsqStream<R> {
    /// Wraps a byte source.
    ///
    /// Frames are read in whole, so an unbuffered source will see one large
    /// read per frame; wrapping it in a [`BufReader`](std::io::BufReader) is
    /// still worthwhile for the 64-byte header reads.
    pub fn new(source: R) -> Self {
        Self {
            source,
            decoder: FrameDecoder::new(),
            options: DecodeOptions::all(),
            buffer: Vec::new(),
            offset: 0,
            frames_read: 0,
            finished: false,
        }
    }

    /// Sets which parts of each frame to decode.
    pub fn with_options(mut self, options: DecodeOptions) -> Self {
        self.options = options;
        self
    }

    /// How many frames have been returned so far.
    pub fn frames_read(&self) -> usize {
        self.frames_read
    }

    /// Byte offset of the next frame within the source.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads and decodes the next frame.
    ///
    /// Returns `None` at the end of the source. An error leaves the stream
    /// finished, because a partially consumed frame makes the following bytes
    /// impossible to interpret.
    pub fn next_frame(&mut self) -> Option<Result<Frame>> {
        if self.finished {
            return None;
        }
        match self.read_frame() {
            Ok(None) => {
                self.finished = true;
                None
            }
            Ok(Some(frame)) => {
                self.frames_read += 1;
                Some(Ok(frame))
            }
            Err(e) => {
                self.finished = true;
                Some(Err(e))
            }
        }
    }

    /// Skips the next frame without decoding its image.
    ///
    /// Returns `false` at the end of the source.
    pub fn skip_frame(&mut self) -> Result<bool> {
        if self.finished {
            return Ok(false);
        }
        match self.read_frame_bytes()? {
            None => {
                self.finished = true;
                Ok(false)
            }
            Some(()) => {
                self.frames_read += 1;
                Ok(true)
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<Frame>> {
        let start = self.offset;
        if self.read_frame_bytes()?.is_none() {
            return Ok(None);
        }
        let frame = self.decoder.decode(&self.buffer, start, self.options)?;
        Ok(Some(frame))
    }

    /// Fills `self.buffer` with the next complete frame.
    fn read_frame_bytes(&mut self) -> Result<Option<()>> {
        self.buffer.clear();
        self.buffer.resize(HEADER_LEN, 0);

        match read_exact_or_eof(&mut self.source, &mut self.buffer)? {
            Filled::Eof => return Ok(None),
            Filled::Partial(n) => {
                return Err(Error::Truncated {
                    what: "FFF frame header",
                    offset: self.offset + n as u64,
                })
            }
            Filled::Full => {}
        }

        if self.buffer[..4] != MAGIC {
            return Err(Error::NotFff {
                offset: self.offset,
            });
        }

        // Read through the container's own byte order; some cameras write the
        // header big-endian.
        let length = fff::frame_length(&self.buffer).unwrap_or(0) as usize;

        if length < HEADER_LEN {
            return Err(Error::InvalidFrameLength {
                offset: self.offset,
                length: length as u32,
            });
        }

        self.buffer.resize(length, 0);
        match read_exact_or_eof(&mut self.source, &mut self.buffer[HEADER_LEN..])? {
            Filled::Full => {}
            _ => {
                return Err(Error::Truncated {
                    what: "FFF frame",
                    offset: self.offset,
                })
            }
        }

        self.offset += length as u64;
        Ok(Some(()))
    }

    /// Consumes the stream and yields the wrapped source.
    pub fn into_inner(self) -> R {
        self.source
    }

    /// Iterates over the remaining frames.
    pub fn frames(&mut self) -> StreamFrames<'_, R> {
        StreamFrames { stream: self }
    }
}

enum Filled {
    Full,
    Partial(usize),
    Eof,
}

/// Like `read_exact`, but distinguishes a clean end of input from a short read.
fn read_exact_or_eof(source: &mut impl Read, buffer: &mut [u8]) -> Result<Filled> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(match filled {
        0 => Filled::Eof,
        n if n == buffer.len() => Filled::Full,
        n => Filled::Partial(n),
    })
}

/// Iterator over the frames left in a [`CsqStream`].
pub struct StreamFrames<'a, R> {
    stream: &'a mut CsqStream<R>,
}

impl<R: Read> Iterator for StreamFrames<'_, R> {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        self.stream.next_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_yields_no_frames() {
        let mut stream = CsqStream::new(&[][..]);
        assert!(stream.next_frame().is_none());
    }

    #[test]
    fn rejects_data_without_a_signature() {
        let mut stream = CsqStream::new(&[0u8; HEADER_LEN][..]);
        let err = stream.next_frame().unwrap().unwrap_err();
        assert!(matches!(err, Error::NotFff { offset: 0 }));
        // The stream stops rather than trying to resynchronise.
        assert!(stream.next_frame().is_none());
    }

    #[test]
    fn reports_a_truncated_frame() {
        let mut data = [0u8; HEADER_LEN].to_vec();
        data[..4].copy_from_slice(&MAGIC);
        data[0x18..0x1c].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        data[0x1c..0x20].copy_from_slice(&1u32.to_le_bytes());
        data[0x34..0x38].copy_from_slice(&1024u32.to_le_bytes());

        let mut stream = CsqStream::new(&data[..]);
        let err = stream.next_frame().unwrap().unwrap_err();
        assert!(matches!(err, Error::Truncated { .. }));
    }
}
