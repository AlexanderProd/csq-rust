//! Writing CSQ recordings.
//!
//! Two commands, because there are two ways to come by the calibration a
//! recording needs:
//!
//! ```text
//! # Recode an existing recording, keeping its camera's calibration.
//! cargo run --release --example csq-write -- recode in.csq out.csq --near 4
//!
//! # Synthesise one from nothing, with a calibration made up for the purpose.
//! cargo run --release --example csq-write -- synth out.csq --frames 120
//! ```
//!
//! Both write files that `exiftool` and FLIR's own viewers read.

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};

use csq::metadata::Timestamp;
use csq::write::{CsqWriter, WriteFrame, WriteOptions};
use csq::{
    AtmosphericTransmission, CsqFile, FrameMetadata, PlanckConstants, RadiometricParameters,
};

#[derive(Parser)]
#[command(about = "Write FLIR CSQ recordings", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Near-lossless bound in detector counts; 0 codes losslessly.
    #[arg(long, global = true, default_value_t = 0)]
    near: u16,

    /// Encoder threads; 0 picks a count from the machine.
    #[arg(long, global = true, default_value_t = 0)]
    threads: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Recode a recording, keeping every measurement and all its metadata.
    Recode {
        /// The recording to read.
        input: PathBuf,
        /// Where to write the result.
        output: PathBuf,
    },
    /// Write a synthetic recording: a hot spot moving across a cool field.
    Synth {
        /// Where to write the result.
        output: PathBuf,
        /// Frames to write.
        #[arg(long, default_value_t = 90)]
        frames: usize,
        /// Frame width in pixels.
        #[arg(long, default_value_t = 640)]
        width: usize,
        /// Frame height in pixels.
        #[arg(long, default_value_t = 480)]
        height: usize,
        /// Frame rate to record in the file.
        #[arg(long, default_value_t = 30.0)]
        fps: f32,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut options = WriteOptions::default().with_threads(cli.threads);
    if cli.near > 0 {
        options = options.near_lossless(cli.near);
    }

    match cli.command {
        Command::Recode { input, output } => recode(&input, &output, options),
        Command::Synth {
            output,
            frames,
            width,
            height,
            fps,
        } => synth(&output, frames, width, height, fps, options),
    }
}

/// Copies a recording frame by frame.
///
/// The counts go across untouched, so with the default lossless coding the
/// result measures exactly what the original did — only the file size changes.
fn recode(
    input: &PathBuf,
    output: &PathBuf,
    options: WriteOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = CsqFile::open(input)?;
    let metadata = source
        .metadata()
        .ok_or("the recording holds no frames")?
        .clone();

    let mut writer = csq::write::create_with_options(output, metadata, options)?;
    let started = Instant::now();

    let mut reader = source.reader();
    while let Some(frame) = reader.next_frame() {
        let frame = frame?;
        let mut out = WriteFrame::raw(frame.raw());
        if let Some(timestamp) = frame.metadata().timestamp {
            out = out.at(timestamp);
        }
        if let Some(gps) = frame.metadata().gps.clone() {
            out = out.with_gps(gps);
        }
        writer.write(out)?;
    }

    report(&writer, started);
    writer.finish()?;
    Ok(())
}

/// Writes a recording that no camera produced.
fn synth(
    output: &PathBuf,
    frames: usize,
    width: usize,
    height: usize,
    fps: f32,
    options: WriteOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut metadata = FrameMetadata::new(width, height, calibration());
    metadata.camera.model = "Synthetic".into();
    metadata.camera.software = env!("CARGO_PKG_VERSION").into();
    metadata.frame_rate = Some(fps);

    let mut writer = csq::write::create_with_options(output, metadata, options)?;
    let started = Instant::now();

    // One buffer, refilled per frame: nothing here needs to allocate after the
    // first pass, and neither does the writer.
    let mut celsius = vec![0.0f32; width * height];
    for index in 0..frames {
        paint(&mut celsius, width, height, index, frames);
        writer.write(
            WriteFrame::celsius(&celsius).at(Timestamp::from_system_time(
                std::time::SystemTime::now() + frame_offset(index, fps),
                0,
            )),
        )?;
    }

    report(&writer, started);
    writer.finish()?;
    Ok(())
}

/// A cool field with a hot spot orbiting the middle of it.
fn paint(celsius: &mut [f32], width: usize, height: usize, index: usize, frames: usize) {
    let angle = index as f32 / frames as f32 * std::f32::consts::TAU;
    let (cx, cy) = (
        width as f32 / 2.0 + angle.cos() * width as f32 * 0.3,
        height as f32 / 2.0 + angle.sin() * height as f32 * 0.3,
    );
    let radius = width.min(height) as f32 * 0.08;

    for (y, row) in celsius.chunks_exact_mut(width).enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let distance = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let spot = (-(distance / radius).powi(2)).exp() * 60.0;
            // A gentle gradient underneath, so the frame is not flat where the
            // spot is not.
            *pixel = 18.0 + y as f32 / height as f32 * 4.0 + spot;
        }
    }
}

fn frame_offset(index: usize, fps: f32) -> std::time::Duration {
    std::time::Duration::from_secs_f64(index as f64 / f64::from(fps).max(1.0))
}

/// A calibration for a camera that does not exist.
///
/// A reader recovers temperatures by inverting exactly these constants, so any
/// self-consistent set round-trips. What they cannot do is make the file agree
/// with some *other* camera's measurements — for that, the constants have to be
/// the ones that camera was calibrated with, which is why [`recode`] carries
/// them over rather than inventing new ones.
fn calibration() -> RadiometricParameters {
    RadiometricParameters {
        emissivity: 0.95,
        object_distance: 2.0,
        reflected_apparent_temperature: 20.0,
        atmospheric_temperature: 20.0,
        ir_window_temperature: 20.0,
        ir_window_transmission: 1.0,
        relative_humidity: 50.0,
        planck: PlanckConstants {
            r1: 18932.178,
            b: 1468.3,
            f: 1.0,
            o: -4388.0,
            r2: 0.033_596_758,
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

fn report<W: std::io::Write>(writer: &CsqWriter<W>, started: Instant) {
    let elapsed = started.elapsed();
    let frames = writer.frames_written();
    let (width, height) = writer.dimensions();
    eprintln!(
        "{frames} frames of {width}x{height} in {:.2} s ({:.0} fps)",
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64()
    );
}
