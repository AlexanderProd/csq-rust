//! Decoded frames and temperature images.

use crate::error::{Error, Result};
use crate::metadata::FrameMetadata;
use crate::thermal::TemperatureTable;

/// One decoded thermal frame: raw detector counts plus the metadata recorded
/// with them.
///
/// Raw counts are kept rather than temperatures because the conversion depends
/// on scene parameters the caller may want to change. Use
/// [`temperatures`](Self::temperatures) for a one-off conversion, or hold a
/// [`TemperatureTable`] and call [`temperatures_with`](Self::temperatures_with)
/// when walking many frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    metadata: FrameMetadata,
    raw: Vec<u16>,
    decoded_samples: usize,
}

impl Frame {
    pub(crate) fn new(metadata: FrameMetadata, raw: Vec<u16>, decoded_samples: usize) -> Self {
        debug_assert_eq!(raw.len(), metadata.width * metadata.height);
        Self {
            metadata,
            raw,
            decoded_samples,
        }
    }

    /// Whether this frame's image data was cut short in the file.
    ///
    /// Only ever true when decoding with [`DecodeOptions::tolerant`]; otherwise
    /// a truncated frame is an error. Pixels past
    /// [`decoded_rows`](Self::decoded_rows) are meaningless.
    ///
    /// [`DecodeOptions::tolerant`]: crate::DecodeOptions::tolerant
    pub fn is_truncated(&self) -> bool {
        self.decoded_samples < self.raw.len()
    }

    /// How many complete rows were decoded from real data.
    pub fn decoded_rows(&self) -> usize {
        if self.metadata.width == 0 {
            return 0;
        }
        self.decoded_samples / self.metadata.width
    }

    /// Image width in pixels.
    pub fn width(&self) -> usize {
        self.metadata.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> usize {
        self.metadata.height
    }

    /// Metadata recorded with this frame.
    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }

    /// Mutable access to the metadata, for overriding scene parameters such as
    /// emissivity before converting to temperatures.
    pub fn metadata_mut(&mut self) -> &mut FrameMetadata {
        &mut self.metadata
    }

    /// Raw detector counts in row-major order.
    pub fn raw(&self) -> &[u16] {
        &self.raw
    }

    /// Raw count at `(x, y)`, with the origin in the top-left corner.
    pub fn raw_at(&self, x: usize, y: usize) -> Result<u16> {
        self.index(x, y).map(|i| self.raw[i])
    }

    /// Consumes the frame and yields its raw counts.
    pub fn into_raw(self) -> Vec<u16> {
        self.raw
    }

    /// Converts the whole frame to °C.
    ///
    /// This builds a [`TemperatureTable`] internally. When converting many
    /// frames that share parameters, build the table once and use
    /// [`temperatures_with`](Self::temperatures_with) instead.
    pub fn temperatures(&self) -> TemperatureImage {
        let table = self.metadata.radiometric.temperature_table();
        self.temperatures_with(&table)
    }

    /// Converts the whole frame to °C using a prebuilt table.
    pub fn temperatures_with(&self, table: &TemperatureTable) -> TemperatureImage {
        let mut celsius = Vec::new();
        table.convert_into(&self.raw, &mut celsius);
        TemperatureImage {
            width: self.width(),
            height: self.height(),
            celsius,
        }
    }

    /// Temperature of a single pixel in °C, without converting the whole frame.
    pub fn temperature_at(&self, x: usize, y: usize) -> Result<f32> {
        let raw = self.raw_at(x, y)?;
        Ok(self.metadata.radiometric.raw_to_celsius(raw))
    }

    fn index(&self, x: usize, y: usize) -> Result<usize> {
        if x >= self.width() || y >= self.height() {
            return Err(Error::PixelOutOfRange {
                position: (x, y),
                size: (self.width(), self.height()),
            });
        }
        Ok(y * self.width() + x)
    }
}

/// A frame converted to temperatures, in °C.
///
/// Pixels whose raw count falls outside the calibration curve hold `NaN`; the
/// helpers here skip them rather than propagating the `NaN` outward.
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureImage {
    width: usize,
    height: usize,
    celsius: Vec<f32>,
}

impl TemperatureImage {
    #[cfg(test)]
    pub(crate) fn from_parts(width: usize, height: usize, celsius: Vec<f32>) -> Self {
        Self {
            width,
            height,
            celsius,
        }
    }

    /// Image width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Temperatures in row-major order, in °C.
    pub fn as_slice(&self) -> &[f32] {
        &self.celsius
    }

    /// Consumes the image and yields its temperatures.
    pub fn into_vec(self) -> Vec<f32> {
        self.celsius
    }

    /// Temperature at `(x, y)` in °C.
    pub fn at(&self, x: usize, y: usize) -> Result<f32> {
        if x >= self.width || y >= self.height {
            return Err(Error::PixelOutOfRange {
                position: (x, y),
                size: (self.width, self.height),
            });
        }
        Ok(self.celsius[y * self.width + x])
    }

    /// Coldest and warmest pixel, ignoring `NaN`.
    ///
    /// Returns `None` when every pixel is `NaN`.
    pub fn range(&self) -> Option<(f32, f32)> {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &value in &self.celsius {
            if value.is_nan() {
                continue;
            }
            min = min.min(value);
            max = max.max(value);
        }
        (min <= max).then_some((min, max))
    }

    /// Mean temperature, ignoring `NaN`.
    pub fn mean(&self) -> Option<f32> {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for &value in &self.celsius {
            if !value.is_nan() {
                sum += f64::from(value);
                count += 1;
            }
        }
        (count > 0).then(|| (sum / count as f64) as f32)
    }

    /// The `percentiles` quantiles of the temperature distribution, in °C.
    ///
    /// Each entry of `percentiles` is a fraction in `0.0..=1.0`. Useful for
    /// robust display scaling, where a couple of hot pixels should not decide
    /// the colour range. Returns `None` when every pixel is `NaN`.
    pub fn percentiles(&self, percentiles: &[f32]) -> Option<Vec<f32>> {
        let mut sorted: Vec<f32> = self
            .celsius
            .iter()
            .copied()
            .filter(|v| !v.is_nan())
            .collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_unstable_by(f32::total_cmp);

        Some(
            percentiles
                .iter()
                .map(|fraction| {
                    let position = (fraction.clamp(0.0, 1.0) * (sorted.len() - 1) as f32).round();
                    sorted[position as usize]
                })
                .collect(),
        )
    }

    /// Copies the temperatures into an [`ndarray::Array2`] indexed
    /// `[row, column]`.
    #[cfg(feature = "ndarray")]
    pub fn to_array2(&self) -> ndarray::Array2<f32> {
        ndarray::Array2::from_shape_vec((self.height, self.width), self.celsius.clone())
            .expect("temperature buffer always matches its dimensions")
    }
}

#[cfg(test)]
mod tests {
    use super::TemperatureImage;

    fn image() -> TemperatureImage {
        TemperatureImage {
            width: 3,
            height: 2,
            celsius: vec![10.0, 20.0, f32::NAN, 30.0, 40.0, 50.0],
        }
    }

    #[test]
    fn range_and_mean_skip_nan() {
        let image = image();
        assert_eq!(image.range(), Some((10.0, 50.0)));
        assert_eq!(image.mean(), Some(30.0));
    }

    #[test]
    fn percentiles_bracket_the_distribution() {
        let image = image();
        let values = image.percentiles(&[0.0, 0.5, 1.0]).unwrap();
        assert_eq!(values[0], 10.0);
        assert_eq!(values[2], 50.0);
        assert!(values[1] >= 20.0 && values[1] <= 40.0);
    }

    #[test]
    fn all_nan_image_has_no_range() {
        let image = TemperatureImage {
            width: 1,
            height: 2,
            celsius: vec![f32::NAN, f32::NAN],
        };
        assert_eq!(image.range(), None);
        assert_eq!(image.mean(), None);
        assert_eq!(image.percentiles(&[0.5]), None);
    }

    #[test]
    fn out_of_range_pixels_error() {
        assert!(image().at(3, 0).is_err());
        assert!(image().at(0, 2).is_err());
        assert!(image().at(2, 1).is_ok());
    }
}
