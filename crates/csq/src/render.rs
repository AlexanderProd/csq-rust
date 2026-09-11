//! Turning temperatures into pictures.
//!
//! Rendering is deliberately split from decoding: a [`Renderer`] takes a
//! [`TemperatureImage`] and produces a plain RGB8 buffer, which every image and
//! video crate accepts without this crate having to depend on one.
//!
//! The two decisions a thermal image needs are which temperature span maps onto
//! the colour ramp ([`Scale`]) and which ramp to use ([`ColorMap`]). Getting the
//! span right matters more than the ramp: a single reflective hot spot can
//! flatten an entire scene, which is why [`Scale::Percentile`] exists.

use crate::frame::TemperatureImage;
use crate::metadata::Palette;

/// How temperatures map onto the colour ramp.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Scale {
    /// Stretch the ramp across the coldest and warmest pixel of each frame.
    ///
    /// Good for stills, but it makes video flicker, because the mapping shifts
    /// whenever the extremes do.
    #[default]
    Auto,
    /// Map a fixed span, in °C. Stable across frames, and the only option that
    /// makes colours comparable between recordings.
    Fixed {
        /// Temperature shown as the coldest colour.
        min: f32,
        /// Temperature shown as the warmest colour.
        max: f32,
    },
    /// Stretch the ramp between two quantiles of each frame, ignoring outliers.
    ///
    /// `Percentile { low: 0.02, high: 0.98 }` is a good default for scenes with
    /// a few very hot or very cold pixels.
    Percentile {
        /// Lower quantile, 0.0..=1.0.
        low: f32,
        /// Upper quantile, 0.0..=1.0.
        high: f32,
    },
}

impl Scale {
    /// Resolves the scale against a specific image.
    ///
    /// Returns `None` when the image holds no usable pixels.
    pub fn range_for(&self, image: &TemperatureImage) -> Option<(f32, f32)> {
        let (min, max) = match *self {
            Self::Auto => image.range()?,
            Self::Fixed { min, max } => (min, max),
            Self::Percentile { low, high } => {
                let values = image.percentiles(&[low.min(high), high.max(low)])?;
                (values[0], values[1])
            }
        };
        // A degenerate span would divide by zero; widen it slightly instead.
        if (max - min).abs() < f32::EPSILON {
            Some((min - 0.5, max + 0.5))
        } else {
            Some((min, max))
        }
    }
}

/// A colour ramp.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ColorMap {
    /// Black through white.
    Grayscale,
    /// White through black, the "white-hot inverted" look.
    GrayscaleInverted,
    /// FLIR's classic black-purple-red-yellow-white ramp.
    Ironbow,
    /// Blue through cyan, yellow and red — high contrast, poor for print.
    Rainbow,
    /// Perceptually uniform black-purple-orange-yellow ramp.
    Inferno,
    /// Black through red to yellow and white.
    Lava,
    /// The palette the camera itself had selected, taken from the frame's
    /// metadata.
    Camera(Box<Palette>),
    /// A ramp supplied by the caller, sampled from cold to hot.
    Custom(Vec<[u8; 3]>),
}

/// Control points of the built-in ramps, as `(position, r, g, b)`.
const IRONBOW: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 0]),
    (0.15, [30, 12, 90]),
    (0.35, [120, 20, 130]),
    (0.55, [200, 55, 80]),
    (0.75, [245, 130, 20]),
    (0.9, [253, 210, 40]),
    (1.0, [255, 255, 235]),
];

const RAINBOW: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 140]),
    (0.25, [0, 160, 220]),
    (0.5, [40, 190, 90]),
    (0.75, [240, 220, 40]),
    (1.0, [190, 20, 20]),
];

const INFERNO: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 4]),
    (0.2, [40, 11, 84]),
    (0.4, [101, 21, 110]),
    (0.6, [159, 42, 99]),
    (0.8, [212, 72, 66]),
    (0.9, [245, 125, 21]),
    (1.0, [252, 255, 164]),
];

const LAVA: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 0]),
    (0.35, [140, 0, 0]),
    (0.65, [235, 90, 0]),
    (0.85, [255, 200, 40]),
    (1.0, [255, 255, 255]),
];

impl ColorMap {
    /// Samples the ramp at `position`, clamped to `0.0..=1.0`.
    pub fn sample(&self, position: f32) -> [u8; 3] {
        let position = if position.is_nan() {
            0.0
        } else {
            position.clamp(0.0, 1.0)
        };

        match self {
            Self::Grayscale => {
                let v = (position * 255.0).round() as u8;
                [v, v, v]
            }
            Self::GrayscaleInverted => {
                let v = 255 - (position * 255.0).round() as u8;
                [v, v, v]
            }
            Self::Ironbow => interpolate(IRONBOW, position),
            Self::Rainbow => interpolate(RAINBOW, position),
            Self::Inferno => interpolate(INFERNO, position),
            Self::Lava => interpolate(LAVA, position),
            Self::Camera(palette) => sample_list(
                &palette
                    .colors
                    .iter()
                    .map(|c| c.to_rgb())
                    .collect::<Vec<_>>(),
                position,
            ),
            Self::Custom(colors) => sample_list(colors, position),
        }
    }

    /// Precomputes the ramp as a 256-entry table.
    fn lookup_table(&self) -> [[u8; 3]; 256] {
        let mut table = [[0u8; 3]; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            *entry = self.sample(index as f32 / 255.0);
        }
        table
    }
}

/// Linearly interpolates between control points.
fn interpolate(stops: &[(f32, [u8; 3])], position: f32) -> [u8; 3] {
    let mut previous = stops[0];
    for &stop in &stops[1..] {
        if position <= stop.0 {
            let span = stop.0 - previous.0;
            let t = if span > 0.0 {
                (position - previous.0) / span
            } else {
                0.0
            };
            return [
                lerp(previous.1[0], stop.1[0], t),
                lerp(previous.1[1], stop.1[1], t),
                lerp(previous.1[2], stop.1[2], t),
            ];
        }
        previous = stop;
    }
    stops[stops.len() - 1].1
}

/// Samples an evenly spaced colour list with linear interpolation.
fn sample_list(colors: &[[u8; 3]], position: f32) -> [u8; 3] {
    match colors.len() {
        0 => [0, 0, 0],
        1 => colors[0],
        n => {
            let scaled = position * (n - 1) as f32;
            let index = scaled.floor() as usize;
            if index + 1 >= n {
                return colors[n - 1];
            }
            let t = scaled - index as f32;
            [
                lerp(colors[index][0], colors[index + 1][0], t),
                lerp(colors[index][1], colors[index + 1][1], t),
                lerp(colors[index][2], colors[index + 1][2], t),
            ]
        }
    }
}

#[inline]
fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Renders temperature images to RGB8.
///
/// ```
/// use csq::render::{ColorMap, Renderer, Scale};
///
/// let renderer = Renderer::new()
///     .with_colormap(ColorMap::Ironbow)
///     .with_scale(Scale::Fixed { min: 10.0, max: 40.0 });
/// # let _ = renderer;
/// ```
#[derive(Debug, Clone)]
pub struct Renderer {
    colormap: ColorMap,
    scale: Scale,
    table: [[u8; 3]; 256],
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// A renderer with a grayscale ramp and per-frame auto scaling.
    pub fn new() -> Self {
        let colormap = ColorMap::Grayscale;
        let table = colormap.lookup_table();
        Self {
            colormap,
            scale: Scale::Auto,
            table,
        }
    }

    /// Sets the colour ramp.
    pub fn with_colormap(mut self, colormap: ColorMap) -> Self {
        self.table = colormap.lookup_table();
        self.colormap = colormap;
        self
    }

    /// Sets how temperatures map onto the ramp.
    pub fn with_scale(mut self, scale: Scale) -> Self {
        self.scale = scale;
        self
    }

    /// The colour ramp in use.
    pub fn colormap(&self) -> &ColorMap {
        &self.colormap
    }

    /// The scaling mode in use.
    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// Renders to a freshly allocated RGB8 buffer, three bytes per pixel in
    /// row-major order.
    pub fn render(&self, image: &TemperatureImage) -> Vec<u8> {
        let mut out = Vec::new();
        self.render_into(image, &mut out);
        out
    }

    /// Renders into an existing buffer, reusing its allocation.
    ///
    /// The buffer is resized to `width * height * 3`.
    pub fn render_into(&self, image: &TemperatureImage, out: &mut Vec<u8>) {
        let (min, max) = self.scale.range_for(image).unwrap_or((0.0, 1.0));
        let inverse_span = 1.0 / (max - min);

        out.clear();
        out.reserve(image.width() * image.height() * 3);
        for &celsius in image.as_slice() {
            let position = ((celsius - min) * inverse_span).clamp(0.0, 1.0);
            // `NaN` compares false against both bounds, so guard it explicitly.
            let index = if position.is_nan() {
                0
            } else {
                (position * 255.0) as usize
            };
            out.extend_from_slice(&self.table[index.min(255)]);
        }
    }

    /// The temperature span this renderer would use for `image`.
    ///
    /// Useful for drawing a colour bar next to the picture.
    pub fn range_for(&self, image: &TemperatureImage) -> Option<(f32, f32)> {
        self.scale.range_for(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_run_from_cold_to_hot() {
        for map in [
            ColorMap::Grayscale,
            ColorMap::Ironbow,
            ColorMap::Inferno,
            ColorMap::Lava,
        ] {
            let cold = map.sample(0.0);
            let hot = map.sample(1.0);
            let cold_sum: u32 = cold.iter().map(|&v| u32::from(v)).sum();
            let hot_sum: u32 = hot.iter().map(|&v| u32::from(v)).sum();
            assert!(hot_sum > cold_sum, "{map:?} is not brighter at the hot end");
        }
    }

    #[test]
    fn samples_clamp_outside_the_unit_interval() {
        let map = ColorMap::Grayscale;
        assert_eq!(map.sample(-5.0), map.sample(0.0));
        assert_eq!(map.sample(5.0), map.sample(1.0));
        assert_eq!(map.sample(f32::NAN), map.sample(0.0));
    }

    #[test]
    fn inverted_grayscale_is_the_mirror_image() {
        assert_eq!(ColorMap::GrayscaleInverted.sample(0.0), [255, 255, 255]);
        assert_eq!(ColorMap::GrayscaleInverted.sample(1.0), [0, 0, 0]);
    }

    #[test]
    fn custom_ramps_interpolate_between_entries() {
        let map = ColorMap::Custom(vec![[0, 0, 0], [100, 100, 100]]);
        assert_eq!(map.sample(0.0), [0, 0, 0]);
        assert_eq!(map.sample(1.0), [100, 100, 100]);
        assert_eq!(map.sample(0.5), [50, 50, 50]);
    }

    #[test]
    fn empty_custom_ramp_is_black() {
        assert_eq!(ColorMap::Custom(Vec::new()).sample(0.5), [0, 0, 0]);
    }

    #[test]
    fn fixed_scale_ignores_frame_content() {
        let scale = Scale::Fixed {
            min: -10.0,
            max: 60.0,
        };
        let image = crate::test_support::temperature_image(2, 1, &[0.0, 100.0]);
        assert_eq!(scale.range_for(&image), Some((-10.0, 60.0)));
    }

    #[test]
    fn degenerate_span_is_widened() {
        let image = crate::test_support::temperature_image(2, 1, &[20.0, 20.0]);
        let (min, max) = Scale::Auto.range_for(&image).unwrap();
        assert!(max > min);
    }

    #[test]
    fn renders_three_bytes_per_pixel() {
        let image = crate::test_support::temperature_image(2, 2, &[0.0, 10.0, 20.0, 30.0]);
        let rgb = Renderer::new().render(&image);
        assert_eq!(rgb.len(), 12);
        // Auto scaling puts the coldest pixel at the bottom of the ramp.
        assert_eq!(&rgb[..3], &[0, 0, 0]);
        assert_eq!(&rgb[9..], &[255, 255, 255]);
    }

    #[test]
    fn nan_pixels_render_as_the_cold_end() {
        let image = crate::test_support::temperature_image(2, 1, &[f32::NAN, 30.0]);
        let rgb = Renderer::new().render(&image);
        assert_eq!(&rgb[..3], &[0, 0, 0]);
    }
}
