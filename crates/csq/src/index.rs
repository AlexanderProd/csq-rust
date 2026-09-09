//! Locating the frames inside a CSQ file.
//!
//! Every FFF header carries the frame's total length, so indexing a recording
//! means hopping from header to header and reading four bytes at each stop —
//! no signature scanning and no image decoding. A 360 MB recording indexes in
//! well under a millisecond, which is what makes seeking and scrubbing cheap.
//!
//! Two things derail a naive walk, and both are handled here. Some cameras pad
//! every frame out to a fixed slot with zeros, so the next frame starts later
//! than the declared length says. And a recording cut mid-write, or with a
//! corrupt header in the middle, breaks the chain outright. In either case the
//! walk scans forward for the next signature; only the second is reported as
//! damage.

use crate::fff::{self, HEADER_LEN, MAGIC};

/// Where one frame lives inside the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLocation {
    /// Byte offset of the frame's `FFF\0` signature.
    pub offset: u64,
    /// Length of the frame in bytes.
    pub length: u32,
}

impl FrameLocation {
    /// Byte range the frame occupies.
    pub fn range(&self) -> std::ops::Range<usize> {
        let start = self.offset as usize;
        start..start + self.length as usize
    }
}

/// The location of every frame in a file, in recording order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameIndex {
    frames: Vec<FrameLocation>,
    resynchronisations: usize,
    padded_frames: usize,
    trailing_bytes: u64,
}

impl FrameIndex {
    /// Walks `data` and records where each frame starts.
    pub fn build(data: &[u8]) -> Self {
        let mut frames = Vec::new();
        let mut resynchronisations = 0usize;
        let mut padded_frames = 0usize;
        let mut offset = 0usize;

        // Leading garbage before the first frame is skipped rather than
        // treated as a fatal error.
        if !data.starts_with(&MAGIC) {
            match find_magic(data, 0) {
                Some(start) => offset = start,
                None => {
                    return Self {
                        frames,
                        resynchronisations,
                        padded_frames,
                        trailing_bytes: data.len() as u64,
                    }
                }
            }
        }

        while offset + HEADER_LEN <= data.len() {
            if data[offset..offset + 4] != MAGIC {
                match find_magic(data, offset) {
                    Some(next) => {
                        resynchronisations += 1;
                        offset = next;
                        continue;
                    }
                    None => break,
                }
            }

            // The length is read through the container's own byte order, which
            // some camera models write big-endian.
            let length = fff::frame_length(&data[offset..]).unwrap_or(0) as usize;
            let end = offset.checked_add(length);
            let body_fits = length >= HEADER_LEN && end.is_some_and(|end| end <= data.len());

            if let (true, Some(end)) = (body_fits, end) {
                frames.push(FrameLocation {
                    offset: offset as u64,
                    length: length as u32,
                });

                // The common case: the next frame starts exactly where this one
                // ends.
                if end + 4 > data.len() || data[end..end + 4] == MAGIC {
                    offset = end;
                    continue;
                }

                // The frame itself is intact but something sits between it and
                // the next one — cameras that pad frames to a fixed slot size do
                // this. Keep the declared length and skip the filler.
                match find_magic(data, offset) {
                    Some(next) => {
                        padded_frames += 1;
                        offset = next;
                    }
                    None => {
                        offset = end;
                        break;
                    }
                }
                continue;
            }

            // The length field did not lead anywhere sensible. Find the next
            // signature and treat everything up to it as one frame.
            match find_magic(data, offset) {
                Some(next) => {
                    resynchronisations += 1;
                    frames.push(FrameLocation {
                        offset: offset as u64,
                        length: (next - offset) as u32,
                    });
                    offset = next;
                }
                None => break,
            }
        }

        Self {
            frames,
            resynchronisations,
            padded_frames,
            trailing_bytes: data.len().saturating_sub(offset) as u64,
        }
    }

    /// Number of frames found.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether the file holds no frames at all.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Location of one frame.
    pub fn get(&self, index: usize) -> Option<FrameLocation> {
        self.frames.get(index).copied()
    }

    /// All frame locations, in recording order.
    pub fn frames(&self) -> &[FrameLocation] {
        &self.frames
    }

    /// How often the walk had to fall back to scanning for a signature.
    ///
    /// Anything above zero means the file is damaged somewhere; the frames on
    /// either side are still usable.
    pub fn resynchronisations(&self) -> usize {
        self.resynchronisations
    }

    /// How many frames were followed by filler before the next frame started.
    ///
    /// Normal for cameras that pad frames to a fixed slot size, and not a sign
    /// of trouble — unlike [`resynchronisations`](Self::resynchronisations).
    pub fn padded_frames(&self) -> usize {
        self.padded_frames
    }

    /// Bytes after the last complete frame, typically a recording that was cut
    /// off mid-write.
    pub fn trailing_bytes(&self) -> u64 {
        self.trailing_bytes
    }
}

/// Finds the next `FFF\0` signature strictly after `from`.
fn find_magic(data: &[u8], from: usize) -> Option<usize> {
    let start = from.saturating_add(1);
    if start >= data.len() {
        return None;
    }
    data[start..]
        .windows(MAGIC.len())
        .position(|window| window == MAGIC)
        .map(|position| start + position)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but well-formed frame of `length` bytes.
    ///
    /// The directory offset and record count have to be present and sensible,
    /// because that is how the walker tells a real header from noise.
    fn frame(length: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; length as usize];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[0x18..0x1c].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        bytes[0x1c..0x20].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x34..0x38].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    #[test]
    fn indexes_a_clean_file() {
        let mut data = frame(100);
        data.extend(frame(200));
        data.extend(frame(150));

        let index = FrameIndex::build(&data);
        assert_eq!(index.len(), 3);
        assert_eq!(index.get(0).unwrap().offset, 0);
        assert_eq!(index.get(1).unwrap().offset, 100);
        assert_eq!(
            index.get(2).unwrap(),
            FrameLocation {
                offset: 300,
                length: 150
            }
        );
        assert_eq!(index.resynchronisations(), 0);
        assert_eq!(index.trailing_bytes(), 0);
    }

    #[test]
    fn resynchronises_past_a_corrupt_length() {
        let mut data = frame(100);
        // Claim a length that runs off the end of the file.
        data[0x34..0x38].copy_from_slice(&9_999_999u32.to_le_bytes());
        data.extend(frame(200));

        let index = FrameIndex::build(&data);
        assert_eq!(index.len(), 2);
        assert_eq!(index.get(0).unwrap().length, 100);
        assert_eq!(index.get(1).unwrap().offset, 100);
        assert_eq!(index.resynchronisations(), 1);
    }

    #[test]
    fn drops_a_truncated_trailing_frame() {
        let mut data = frame(100);
        data.extend(&frame(200)[..120]);

        let index = FrameIndex::build(&data);
        assert_eq!(index.len(), 1);
        assert_eq!(index.trailing_bytes(), 120);
    }

    #[test]
    fn skips_leading_garbage() {
        let mut data = vec![0xaau8; 37];
        data.extend(frame(100));

        let index = FrameIndex::build(&data);
        assert_eq!(index.len(), 1);
        assert_eq!(index.get(0).unwrap().offset, 37);
    }

    #[test]
    fn skips_padding_between_frames_without_reporting_damage() {
        // Some cameras pad every frame out to a fixed slot with zeros.
        let mut data = frame(100);
        data.extend(std::iter::repeat_n(0u8, 60));
        data.extend(frame(100));

        let index = FrameIndex::build(&data);
        assert_eq!(index.len(), 2);
        assert_eq!(index.get(0).unwrap().length, 100, "declared length is kept");
        assert_eq!(index.get(1).unwrap().offset, 160);
        assert_eq!(index.padded_frames(), 1);
        assert_eq!(
            index.resynchronisations(),
            0,
            "padding is not damage and must not be reported as such"
        );
    }

    #[test]
    fn indexes_big_endian_containers() {
        // Some camera models write the container header big-endian.
        let mut bytes = vec![0u8; 128];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[0x18..0x1c].copy_from_slice(&(HEADER_LEN as u32).to_be_bytes());
        bytes[0x1c..0x20].copy_from_slice(&1u32.to_be_bytes());
        bytes[0x34..0x38].copy_from_slice(&128u32.to_be_bytes());

        let index = FrameIndex::build(&bytes);
        assert_eq!(index.len(), 1);
        assert_eq!(index.get(0).unwrap().length, 128);
        assert_eq!(
            index.resynchronisations(),
            0,
            "big-endian lengths should be followed, not scanned past"
        );
    }

    #[test]
    fn empty_input_yields_no_frames() {
        assert!(FrameIndex::build(&[]).is_empty());
        assert!(FrameIndex::build(&[0u8; 8]).is_empty());
    }
}
