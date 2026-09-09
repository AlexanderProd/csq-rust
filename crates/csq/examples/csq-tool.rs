//! A small command line tool over the `csq` crate.
//!
//! ```text
//! csq-tool info      recording.csq                  # camera, calibration, timing
//! csq-tool frames    recording.csq --limit 5        # per-frame temperature stats
//! csq-tool export    recording.csq -o out/          # frames as PNG
//! csq-tool scrub     recording.csq -o out/ -n 12    # evenly spaced thumbnails
//! csq-tool video     recording.csq -o out.mp4       # encode with ffmpeg
//! csq-tool probe     recording.csq -x 512 -y 384    # temperature of one pixel
//! csq-tool csv       recording.csq -o frame.csv     # temperatures as CSV
//! ```

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use csq::render::{ColorMap, Renderer, Scale};
use csq::{CsqFile, TemperatureImage};

#[derive(Parser)]
#[command(
    name = "csq-tool",
    about = "Inspect and export FLIR CSQ thermal recordings"
)]
struct Cli {
    #[command(subcommand)]
    command: Job,
}

#[derive(Subcommand)]
enum Job {
    /// Print camera, calibration and timing information.
    Info { input: PathBuf },

    /// Print per-frame temperature statistics.
    Frames {
        input: PathBuf,
        /// Stop after this many frames.
        #[arg(long)]
        limit: Option<usize>,
        /// Salvage frames whose image data was cut short instead of failing.
        #[arg(long)]
        tolerant: bool,
    },

    /// Write frames as PNG images.
    Export {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// First frame to write.
        #[arg(long, default_value_t = 0)]
        start: usize,
        /// How many frames to write; defaults to all of them.
        #[arg(long)]
        count: Option<usize>,
        #[command(flatten)]
        look: Look,
    },

    /// Write evenly spaced thumbnails, seeking rather than decoding everything.
    Scrub {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// How many thumbnails to take.
        #[arg(short = 'n', long, default_value_t = 10)]
        count: usize,
        #[command(flatten)]
        look: Look,
    },

    /// Encode the recording to a video file by piping frames through ffmpeg.
    Video {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Override the recording's own frame rate.
        #[arg(long)]
        fps: Option<f32>,
        /// Constant rate factor handed to libx264; lower is better quality.
        #[arg(long, default_value_t = 18)]
        crf: u8,
        #[command(flatten)]
        look: Look,
    },

    /// Print the temperature of a single pixel.
    Probe {
        input: PathBuf,
        #[arg(short = 'f', long, default_value_t = 0)]
        frame: usize,
        #[arg(short)]
        x: usize,
        #[arg(short)]
        y: usize,
    },

    /// Write one frame's temperatures as CSV, one row per image row.
    Csv {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(short = 'f', long, default_value_t = 0)]
        frame: usize,
    },
}

/// Options shared by every command that produces pictures.
#[derive(Args, Clone)]
struct Look {
    #[arg(long, value_enum, default_value_t = Palette::Ironbow)]
    colormap: Palette,
    /// Lowest temperature on the colour ramp, in °C. Implies a fixed scale.
    #[arg(long)]
    min: Option<f32>,
    /// Highest temperature on the colour ramp, in °C. Implies a fixed scale.
    #[arg(long)]
    max: Option<f32>,
    /// Clip this fraction off each end of the distribution instead of using the
    /// absolute extremes.
    #[arg(long, default_value_t = 0.02)]
    clip: f32,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Palette {
    Gray,
    GrayInverted,
    Ironbow,
    Rainbow,
    Inferno,
    Lava,
    /// Whatever palette the camera had selected.
    Camera,
}

impl Look {
    fn renderer(&self, file: &CsqFile) -> Renderer {
        let colormap = match self.colormap {
            Palette::Gray => ColorMap::Grayscale,
            Palette::GrayInverted => ColorMap::GrayscaleInverted,
            Palette::Ironbow => ColorMap::Ironbow,
            Palette::Rainbow => ColorMap::Rainbow,
            Palette::Inferno => ColorMap::Inferno,
            Palette::Lava => ColorMap::Lava,
            Palette::Camera => file
                .metadata()
                .and_then(|metadata| metadata.palette.clone())
                .map(|palette| ColorMap::Camera(Box::new(palette)))
                .unwrap_or(ColorMap::Grayscale),
        };

        // A fixed scale keeps colours stable from frame to frame, which matters
        // for video; without one, clip the tails so a few outliers cannot
        // flatten the whole picture.
        let scale = match (self.min, self.max) {
            (Some(min), Some(max)) => Scale::Fixed { min, max },
            _ => Scale::Percentile {
                low: self.clip,
                high: 1.0 - self.clip,
            },
        };

        Renderer::new().with_colormap(colormap).with_scale(scale)
    }
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Job::Info { input } => info(&input),
        Job::Frames {
            input,
            limit,
            tolerant,
        } => frames(&input, limit, tolerant),
        Job::Export {
            input,
            output,
            start,
            count,
            look,
        } => export(&input, &output, start, count, &look),
        Job::Scrub {
            input,
            output,
            count,
            look,
        } => scrub(&input, &output, count, &look),
        Job::Video {
            input,
            output,
            fps,
            crf,
            look,
        } => video(&input, &output, fps, crf, &look),
        Job::Probe { input, frame, x, y } => probe(&input, frame, x, y),
        Job::Csv {
            input,
            output,
            frame,
        } => csv(&input, &output, frame),
    }
}

fn info(input: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let file = CsqFile::open(input)?;
    let indexed_in = started.elapsed();

    let metadata = match file.metadata() {
        Some(metadata) => metadata,
        None => {
            println!("{}: no frames found", input.display());
            return Ok(());
        }
    };
    let (width, height) = file.dimensions().unwrap_or((0, 0));

    println!("File");
    println!("  path            {}", input.display());
    println!("  frames          {}", file.len());
    println!("  resolution      {width} x {height}");
    if let Some(rate) = file.frame_rate() {
        println!("  frame rate      {rate} fps");
    }
    if let Some(duration) = file.duration() {
        println!("  duration        {:.2} s", duration.as_secs_f64());
    }
    println!("  indexed in      {indexed_in:?}");
    if file.index().resynchronisations() > 0 {
        println!(
            "  damaged frames  {} (recovered by resynchronising)",
            file.index().resynchronisations()
        );
    }
    if file.index().padded_frames() > 0 {
        println!(
            "  padded frames   {} (padded to a fixed slot size)",
            file.index().padded_frames()
        );
    }
    if file.index().trailing_bytes() > 0 {
        println!(
            "  trailing bytes  {} (recording was cut off)",
            file.index().trailing_bytes()
        );
    }

    let camera = &metadata.camera;
    println!("\nCamera");
    println!("  model           {}", camera.model);
    println!("  serial          {}", camera.serial_number);
    println!("  firmware        {}", camera.software);
    println!(
        "  lens            {} ({})",
        camera.lens_model, camera.lens_part_number
    );
    println!("  field of view   {:.1}°", metadata.field_of_view);
    println!("  focus distance  {:.2} m", metadata.focus_distance);
    if !camera.filter_model.is_empty() {
        println!("  filter          {}", camera.filter_model);
    }

    let radiometric = &metadata.radiometric;
    println!("\nScene parameters");
    println!("  emissivity      {:.2}", radiometric.emissivity);
    println!("  object distance {:.1} m", radiometric.object_distance);
    println!(
        "  reflected temp  {:.1} °C",
        radiometric.reflected_apparent_temperature
    );
    println!(
        "  atmospheric     {:.1} °C",
        radiometric.atmospheric_temperature
    );
    println!("  humidity        {:.0} %", radiometric.relative_humidity);
    println!(
        "  window transm.  {:.2}",
        radiometric.ir_window_transmission
    );

    println!("\nCalibration");
    println!("  Planck R1       {}", radiometric.planck.r1);
    println!("  Planck B        {}", radiometric.planck.b);
    println!("  Planck F        {}", radiometric.planck.f);
    println!("  Planck O        {}", radiometric.planck.o);
    println!("  Planck R2       {}", radiometric.planck.r2);
    println!(
        "  measuring range {:.0} °C to {:.0} °C",
        metadata.temperature_range.min, metadata.temperature_range.max
    );

    if let Some(timestamp) = metadata.timestamp {
        println!("\nRecorded         {timestamp}");
    }
    if let Some(gps) = &metadata.gps {
        println!(
            "Position         {:.6}, {:.6} at {:.0} m",
            gps.latitude, gps.longitude, gps.altitude
        );
        if let Some(direction) = gps.image_direction {
            println!(
                "Heading          {direction:.0}° {}",
                gps.image_direction_ref
            );
        }
    }
    if let Some(palette) = &metadata.palette {
        println!(
            "Camera palette   {} ({} colours)",
            palette.name,
            palette.colors.len()
        );
    }

    Ok(())
}

fn frames(
    input: &Path,
    limit: Option<usize>,
    tolerant: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    let table = file
        .metadata()
        .ok_or("file has no frames")?
        .radiometric
        .temperature_table();

    let count = limit.unwrap_or(file.len()).min(file.len());
    let mut reader = file.reader().with_options(if tolerant {
        csq::DecodeOptions::tolerant()
    } else {
        csq::DecodeOptions::all()
    });
    let started = Instant::now();

    println!(
        "{:>6}  {:>10}  {:>8}  {:>8}  {:>8}",
        "frame", "time", "min °C", "mean °C", "max °C"
    );
    for index in 0..count {
        let frame = reader.next_frame().ok_or("unexpected end of file")??;
        let temperatures = frame.temperatures_with(&table);
        let (min, max) = temperatures.range().unwrap_or((f32::NAN, f32::NAN));
        let time = file.frame_time(index).unwrap_or(Duration::ZERO);
        println!(
            "{index:>6}  {:>9.3}s  {min:>8.2}  {:>8.2}  {max:>8.2}{}",
            time.as_secs_f64(),
            temperatures.mean().unwrap_or(f32::NAN),
            if frame.is_truncated() {
                format!(
                    "   TRUNCATED after {} of {} rows",
                    frame.decoded_rows(),
                    frame.height()
                )
            } else {
                String::new()
            },
        );
    }

    let elapsed = started.elapsed();
    eprintln!(
        "\n{count} frames in {elapsed:?} ({:.1} fps)",
        count as f64 / elapsed.as_secs_f64()
    );
    Ok(())
}

fn export(
    input: &Path,
    output: &Path,
    start: usize,
    count: Option<usize>,
    look: &Look,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    fs::create_dir_all(output)?;

    let renderer = look.renderer(&file);
    let table = file
        .metadata()
        .ok_or("file has no frames")?
        .radiometric
        .temperature_table();

    let last = count
        .map(|count| (start + count).min(file.len()))
        .unwrap_or(file.len());
    let mut reader = file.reader();
    reader.seek(start.min(file.len()))?;

    let mut rgb = Vec::new();
    for index in start..last {
        let frame = reader.next_frame().ok_or("unexpected end of file")??;
        let temperatures = frame.temperatures_with(&table);
        renderer.render_into(&temperatures, &mut rgb);

        let path = output.join(format!("frame_{index:06}.png"));
        write_png(&path, &rgb, frame.width(), frame.height())?;
    }

    println!(
        "wrote {} frames to {}",
        last.saturating_sub(start),
        output.display()
    );
    Ok(())
}

fn scrub(
    input: &Path,
    output: &Path,
    count: usize,
    look: &Look,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    fs::create_dir_all(output)?;

    if file.is_empty() {
        return Err("file has no frames".into());
    }
    let renderer = look.renderer(&file);
    let table = file.metadata().unwrap().radiometric.temperature_table();

    // Seeking straight to each position is what makes this cheap: the frames
    // in between are never touched.
    let mut reader = file.reader();
    let mut rgb = Vec::new();
    let started = Instant::now();

    for step in 0..count.max(1) {
        let index = if count <= 1 {
            0
        } else {
            step * (file.len() - 1) / (count - 1)
        };
        let frame = reader.frame(index)?;
        let temperatures = frame.temperatures_with(&table);
        renderer.render_into(&temperatures, &mut rgb);

        let path = output.join(format!("scrub_{step:03}_frame_{index:06}.png"));
        write_png(&path, &rgb, frame.width(), frame.height())?;
        println!(
            "{path:?}  frame {index} at {:.2}s",
            file.frame_time(index)
                .unwrap_or(Duration::ZERO)
                .as_secs_f64()
        );
    }

    eprintln!("{count} thumbnails in {:?}", started.elapsed());
    Ok(())
}

fn video(
    input: &Path,
    output: &Path,
    fps: Option<f32>,
    crf: u8,
    look: &Look,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    let (width, height) = file.dimensions().ok_or("file has no frames")?;
    let rate = fps.or_else(|| file.frame_rate()).unwrap_or(30.0);

    let renderer = look.renderer(&file);
    let table = file.metadata().unwrap().radiometric.temperature_table();

    let mut ffmpeg = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .args(["-f", "rawvideo", "-pixel_format", "rgb24"])
        .args(["-video_size", &format!("{width}x{height}")])
        .args(["-framerate", &rate.to_string()])
        .args(["-i", "-"])
        .args(["-c:v", "libx264", "-preset", "medium"])
        .args(["-crf", &crf.to_string()])
        .args(["-pix_fmt", "yuv420p"])
        .arg(output)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg ({e}); install it and try again"))?;

    let mut stdin = ffmpeg.stdin.take().ok_or("ffmpeg stdin unavailable")?;
    let mut reader = file.reader();
    let mut rgb = Vec::new();
    let started = Instant::now();
    let mut written = 0usize;

    while let Some(frame) = reader.next_frame() {
        let frame = frame?;
        let temperatures = frame.temperatures_with(&table);
        renderer.render_into(&temperatures, &mut rgb);
        stdin.write_all(&rgb)?;
        written += 1;

        if written % 100 == 0 {
            eprint!("\r{written}/{} frames", file.len());
        }
    }
    drop(stdin);

    let status = ffmpeg.wait()?;
    if !status.success() {
        return Err(format!("ffmpeg exited with {status}").into());
    }

    let elapsed = started.elapsed();
    eprintln!(
        "\rwrote {written} frames to {} in {elapsed:?} ({:.1} fps)",
        output.display(),
        written as f64 / elapsed.as_secs_f64()
    );
    Ok(())
}

fn probe(input: &Path, index: usize, x: usize, y: usize) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    let frame = file.frame(index)?;
    println!(
        "frame {index} pixel ({x}, {y}): raw {} -> {:.2} °C",
        frame.raw_at(x, y)?,
        frame.temperature_at(x, y)?
    );
    Ok(())
}

fn csv(input: &Path, output: &Path, index: usize) -> Result<(), Box<dyn std::error::Error>> {
    let file = CsqFile::open(input)?;
    let temperatures = file.frame(index)?.temperatures();
    write_csv(output, &temperatures)?;
    println!(
        "wrote {}x{} temperatures to {}",
        temperatures.width(),
        temperatures.height(),
        output.display()
    );
    Ok(())
}

fn write_csv(path: &Path, image: &TemperatureImage) -> std::io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    for row in image.as_slice().chunks(image.width()) {
        let mut first = true;
        for value in row {
            if !first {
                out.write_all(b",")?;
            }
            write!(out, "{value:.3}")?;
            first = false;
        }
        out.write_all(b"\n")?;
    }
    out.flush()
}

fn write_png(path: &Path, rgb: &[u8], width: usize, height: usize) -> std::io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(rgb))
        .map_err(std::io::Error::other)
}
