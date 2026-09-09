//! Command line surface: everything `clap` needs to know, and nothing else.
//!
//! Keeping the argument types in one module means the rest of the binary deals
//! in plain values, and that no `clap` type ever reaches a function that does
//! real work.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const EXAMPLES: &str = "\
Examples:
  csq-reader info rec.csq                            metadata as key<TAB>value
  csq-reader info rec.csq -F json | jq .camera.model  one metadata field
  csq-reader frames rec.csq | awk -F'\\t' '$5 > 80'    frames with a hot pixel
  csq-reader temps rec.csq -f 12 > frame12.tsv       one frame as a number grid
  csq-reader temps rec.csq -f -1 -F raw > f.f32      last frame as binary f32
  csq-reader pixel rec.csq -x 512 -y 384 --all       a pixel over time
  csq-reader pixel rec.csq -x 512 -y 384 --bare      just the number
  csq-reader temps rec.csq -f 12 --normalize         0..1 as a renderer sees it
  cat rec.csq | csq-reader info -                    read from a pipe

Data goes to stdout, diagnostics to stderr. Exit status is 0 on success, 1 on
failure and 2 on a usage error.";

/// Read temperatures and metadata out of FLIR CSQ thermal recordings.
#[derive(Parser)]
#[command(
    name = "csq-reader",
    version,
    about = "Read temperatures and metadata out of FLIR CSQ thermal recordings",
    after_help = EXAMPLES,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Print the recording's metadata as `key<TAB>value` lines.
    Info(InfoArgs),
    /// Print one line of temperature statistics per frame.
    Frames(FramesArgs),
    /// Print every temperature of one frame.
    Temps(TempsArgs),
    /// Print the temperature of one pixel.
    Pixel(PixelArgs),
}

/// The recording to read.
#[derive(Args)]
pub struct Input {
    /// CSQ recording to read; `-` reads the whole recording from stdin.
    #[arg(value_name = "FILE")]
    pub path: PathBuf,
}

/// Picks a single frame, by index or by playback position.
#[derive(Args)]
pub struct FrameSelect {
    /// Frame to read, counted from zero; negative counts back from the end.
    #[arg(
        short = 'f',
        long,
        value_name = "N",
        allow_negative_numbers = true,
        allow_hyphen_values = true
    )]
    pub frame: Option<i64>,

    /// Read whichever frame is on screen this many seconds in.
    #[arg(long, value_name = "SECONDS", conflicts_with = "frame")]
    pub time: Option<f64>,
}

/// Picks a run of frames.
#[derive(Args)]
pub struct FrameRange {
    /// First frame to report on.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub start: usize,

    /// How many frames to report on; the default is all of them.
    #[arg(long, value_name = "N")]
    pub count: Option<usize>,

    /// Report on every Nth frame only.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = at_least_one)]
    pub step: usize,
}

/// Overrides for the scene parameters the camera recorded.
///
/// Every one of these changes the temperatures the tool prints, because the
/// radiometric model runs on them; leaving them alone reproduces what the
/// camera itself would display.
#[derive(Args, Clone, Default)]
#[command(next_help_heading = "Scene parameters")]
pub struct Scene {
    /// Emissivity of the object, 0 to 1.
    #[arg(long, value_name = "0..1", value_parser = fraction)]
    pub emissivity: Option<f32>,

    /// Distance to the object, in metres.
    #[arg(long = "distance", value_name = "METRES")]
    pub object_distance: Option<f32>,

    /// Apparent temperature of the reflected surroundings, in °C.
    #[arg(long = "reflected-temp", value_name = "CELSIUS")]
    pub reflected_temperature: Option<f32>,

    /// Air temperature along the path, in °C.
    #[arg(long = "atmospheric-temp", value_name = "CELSIUS")]
    pub atmospheric_temperature: Option<f32>,

    /// Relative humidity along the path, in percent.
    #[arg(long, value_name = "PERCENT")]
    pub humidity: Option<f32>,

    /// Temperature of an IR window in front of the lens, in °C.
    #[arg(long = "window-temp", value_name = "CELSIUS")]
    pub window_temperature: Option<f32>,

    /// Transmission of an IR window in front of the lens, 0 to 1.
    #[arg(long = "window-transmission", value_name = "0..1", value_parser = fraction)]
    pub window_transmission: Option<f32>,
}

impl Scene {
    /// True when nothing was overridden, so the camera's own values stand.
    pub fn is_empty(&self) -> bool {
        self.emissivity.is_none()
            && self.object_distance.is_none()
            && self.reflected_temperature.is_none()
            && self.atmospheric_temperature.is_none()
            && self.humidity.is_none()
            && self.window_temperature.is_none()
            && self.window_transmission.is_none()
    }
}

/// Turning a temperature into the position a renderer would give it on the
/// colour ramp.
#[derive(Args, Clone, Default)]
#[command(next_help_heading = "Rendering scale")]
pub struct Ramp {
    /// Print the position on the colour ramp instead of a temperature: 0 at
    /// the frame's coldest pixel, 1 at its warmest, which is the value a
    /// renderer would turn into a colour.
    #[arg(long)]
    pub normalize: bool,

    /// Temperature to place at 0, in °C. Implies `--normalize`, and is what
    /// makes values comparable between frames.
    #[arg(long, value_name = "CELSIUS", requires = "max")]
    pub min: Option<f32>,

    /// Temperature to place at 1, in °C. Implies `--normalize`.
    #[arg(long, value_name = "CELSIUS", requires = "min")]
    pub max: Option<f32>,
}

impl Ramp {
    /// True when normalising was asked for; a fixed span implies it.
    pub fn wanted(&self) -> bool {
        self.normalize || self.min.is_some() || self.max.is_some()
    }
}

/// Rejects a step of zero, which would otherwise mean "never advance".
fn at_least_one(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(step) if step >= 1 => Ok(step),
        Ok(_) => Err("must be at least 1".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// Rejects the emissivities and transmissions the radiometric model divides by,
/// so a typo is a usage error rather than a column of `nan`.
fn fraction(value: &str) -> Result<f32, String> {
    match value.parse::<f32>() {
        Ok(fraction) if fraction > 0.0 && fraction <= 1.0 => Ok(fraction),
        Ok(_) => Err("must be greater than 0 and at most 1".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// Output shapes for a flat list of `key`/`value` pairs.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Default)]
pub enum PairFormat {
    /// One `key<TAB>value` line per field.
    #[default]
    Tsv,
    /// One `key,value` line per field.
    Csv,
    /// A single object, nested on the dots in the keys.
    Json,
}

/// Output shapes for a table of rows.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Default)]
pub enum RowFormat {
    /// Tab-separated columns.
    #[default]
    Tsv,
    /// Comma-separated columns.
    Csv,
    /// One array of objects.
    Json,
    /// One object per line (JSON Lines), for streaming into `jq`.
    Jsonl,
}

/// Output shapes for a whole frame of temperatures.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Default)]
pub enum GridFormat {
    /// One image row per line, tab-separated.
    #[default]
    Tsv,
    /// One image row per line, comma-separated.
    Csv,
    /// One pixel per line as `x<TAB>y<TAB>celsius`.
    Long,
    /// An object holding the values row-major in a flat array.
    Json,
    /// Little-endian `f32`, row-major, no header: `numpy.fromfile(dtype='<f4')`.
    Raw,
}

#[derive(Args)]
pub struct InfoArgs {
    #[command(flatten)]
    pub input: Input,

    #[command(flatten)]
    pub select: FrameSelect,

    /// Output format.
    #[arg(short = 'F', long, value_enum, default_value_t = PairFormat::Tsv)]
    pub format: PairFormat,
}

#[derive(Args)]
pub struct FramesArgs {
    #[command(flatten)]
    pub input: Input,

    #[command(flatten)]
    pub range: FrameRange,

    /// Output format.
    #[arg(short = 'F', long, value_enum, default_value_t = RowFormat::Tsv)]
    pub format: RowFormat,

    /// Emit a column-name header line.
    #[arg(short = 'H', long)]
    pub header: bool,

    /// Decimal places for temperatures.
    #[arg(short = 'p', long, value_name = "N", default_value_t = 2)]
    pub precision: usize,

    /// Salvage frames whose image data was cut short instead of failing.
    #[arg(long)]
    pub tolerant: bool,

    #[command(flatten)]
    pub scene: Scene,
}

#[derive(Args)]
pub struct TempsArgs {
    #[command(flatten)]
    pub input: Input,

    #[command(flatten)]
    pub select: FrameSelect,

    /// Output format.
    #[arg(short = 'F', long, value_enum, default_value_t = GridFormat::Tsv)]
    pub format: GridFormat,

    /// Emit a `#` comment naming the frame and its dimensions.
    #[arg(short = 'H', long)]
    pub header: bool,

    /// Decimal places; ignored by `--format raw`. [default: 2, or 4 with
    /// `--normalize`]
    #[arg(short = 'p', long, value_name = "N")]
    pub precision: Option<usize>,

    /// Print raw 16-bit detector counts instead of temperatures; under
    /// `--format raw` this writes little-endian `u16` rather than `f32`.
    #[arg(long, conflicts_with = "normalize")]
    pub raw_counts: bool,

    /// Salvage a frame whose image data was cut short instead of failing.
    #[arg(long)]
    pub tolerant: bool,

    #[command(flatten)]
    pub scene: Scene,

    #[command(flatten)]
    pub ramp: Ramp,
}

#[derive(Args)]
pub struct PixelArgs {
    #[command(flatten)]
    pub input: Input,

    /// Column, counted from the left edge.
    #[arg(short = 'x', long, value_name = "X")]
    pub x: usize,

    /// Row, counted from the top edge.
    #[arg(short = 'y', long, value_name = "Y")]
    pub y: usize,

    #[command(flatten)]
    pub select: FrameSelect,

    /// Follow the pixel through every frame instead of reading just one.
    #[arg(long, conflicts_with_all = ["frame", "time"])]
    pub all: bool,

    /// Output format.
    #[arg(short = 'F', long, value_enum, default_value_t = RowFormat::Tsv)]
    pub format: RowFormat,

    /// Emit a column-name header line.
    #[arg(short = 'H', long)]
    pub header: bool,

    /// Print only the temperature, one value per line.
    #[arg(long, conflicts_with_all = ["format", "header"])]
    pub bare: bool,

    /// Decimal places. [default: 2, or 4 with `--normalize`]
    #[arg(short = 'p', long, value_name = "N")]
    pub precision: Option<usize>,

    /// Add a column with the raw 16-bit detector count.
    #[arg(long)]
    pub raw_counts: bool,

    /// Salvage frames whose image data was cut short instead of failing.
    #[arg(long)]
    pub tolerant: bool,

    #[command(flatten)]
    pub scene: Scene,

    #[command(flatten)]
    pub ramp: Ramp,
}
