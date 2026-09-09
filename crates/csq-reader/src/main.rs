//! `csq-reader` — read temperatures and metadata out of FLIR CSQ recordings.
//!
//! The tool is deliberately plumbing rather than porcelain: data goes to
//! stdout in a shape `awk`, `cut` and `jq` can take apart, diagnostics go to
//! stderr, and the exit status is 0, 1 for a failure or 2 for a usage error.
//! Output streams as it is produced, so a `| head` closing the pipe stops the
//! decode rather than being ignored.
//!
//! It is a thin front end: everything it knows about CSQ files comes from the
//! [`csq`] library, and no parsing or radiometry lives here. The library is a
//! separate crate so that depending on it never drags in an argument parser,
//! and so that the two can release — and set their minimum Rust version —
//! independently.

mod cli;
mod output;

use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use csq::metadata::FrameMetadata;
use csq::render::Scale;
use csq::{
    CsqFile, CsqReader, DecodeOptions, Frame, RadiometricParameters, TemperatureImage,
    TemperatureTable,
};

use cli::{
    Cli, Command, FrameRange, FrameSelect, FramesArgs, GridFormat, InfoArgs, Input, PairFormat,
    PixelArgs, Ramp, RowFormat, Scene, TempsArgs,
};
use output::{Cell, Pairs, RowShape, Rows};

/// Anything that can end a run. The concrete type matters only so a broken
/// pipe can be told apart from a real failure.
type Failure = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let command = Cli::parse().command;

    // One locked, buffered handle for the whole run: unbuffered `println!` on a
    // million-pixel frame spends more time in write syscalls than in decoding.
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let result = run(command, &mut out).and_then(|()| Ok(out.flush()?));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // A closed pipe is how `head` says it has seen enough; that is not an
        // error, so exit quietly rather than reporting it.
        Err(error) if is_broken_pipe(error.as_ref()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("csq-reader: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, out: &mut impl Write) -> Result<(), Failure> {
    match command {
        Command::Info(args) => info(args, out),
        Command::Frames(args) => frames(args, out),
        Command::Temps(args) => temps(args, out),
        Command::Pixel(args) => pixel(args, out),
    }
}

fn is_broken_pipe(error: &(dyn std::error::Error + 'static)) -> bool {
    match error.downcast_ref::<io::Error>() {
        Some(error) => error.kind() == io::ErrorKind::BrokenPipe,
        None => error.source().is_some_and(is_broken_pipe),
    }
}

// ---------------------------------------------------------------- commands --

fn info(args: InfoArgs, out: &mut impl Write) -> Result<(), Failure> {
    let (file, label) = open(&args.input)?;
    let index = resolve_frame(&file, &args.select)?;
    // Metadata parsing does not touch the image, so this stays cheap even when
    // the frame is in the middle of a long recording.
    let metadata = file.frame_metadata(index)?;

    let pairs = describe(&label, &file, index, &metadata).into_vec();
    match args.format {
        PairFormat::Tsv => output::write_pairs(out, &pairs, '\t')?,
        PairFormat::Csv => output::write_pairs(out, &pairs, ',')?,
        PairFormat::Json => output::write_pairs_json(out, &pairs)?,
    }
    Ok(())
}

fn frames(args: FramesArgs, out: &mut impl Write) -> Result<(), Failure> {
    let (file, _) = open(&args.input)?;
    let table = scene_table(&file, 0, &args.scene)?;
    let mut reader = file.reader().with_options(decode_options(args.tolerant));

    let precision = args.precision;
    let columns = ["frame", "time_s", "min_c", "mean_c", "max_c"];
    let mut rows = Rows::new(row_shape(args.format, false), &columns, args.header);

    for index in selection(&file, &args.range)? {
        let frame = decode(&mut reader, index)?;

        let image = frame.temperatures_with(&table);
        let (min, max) = image.range().unwrap_or((f32::NAN, f32::NAN));
        let mean = image.mean().unwrap_or(f32::NAN);
        rows.write(
            out,
            &[
                Cell::Int(index as u64),
                seconds(&file, index),
                Cell::Fixed(min, precision),
                Cell::Fixed(mean, precision),
                Cell::Fixed(max, precision),
            ],
        )?;
    }
    rows.finish(out)?;
    Ok(())
}

fn temps(args: TempsArgs, out: &mut impl Write) -> Result<(), Failure> {
    let (file, _) = open(&args.input)?;
    let index = resolve_frame(&file, &args.select)?;

    let mut reader = file.reader().with_options(decode_options(args.tolerant));
    let frame = decode(&mut reader, index)?;

    // Counts are what the detector actually reported; skipping the conversion
    // means skipping the scene parameters entirely.
    let celsius;
    let positions;
    let grid = if args.raw_counts {
        Grid::Counts(frame.raw())
    } else {
        celsius = match args.scene.is_empty() {
            true => frame.temperatures(),
            false => frame.temperatures_with(&scene_table(&file, index, &args.scene)?),
        };
        if args.ramp.wanted() {
            let ramp = ColorRamp::across(&celsius, &args.ramp);
            positions = celsius
                .as_slice()
                .iter()
                .map(|c| ramp.position(*c))
                .collect::<Vec<_>>();
            Grid::Ramp(&positions)
        } else {
            Grid::Celsius(celsius.as_slice())
        }
    };

    write_grid(out, &grid, &frame, index, &file, &args)
}

fn pixel(args: PixelArgs, out: &mut impl Write) -> Result<(), Failure> {
    let (file, _) = open(&args.input)?;
    let table = scene_table(&file, 0, &args.scene)?;
    let mut reader = file.reader().with_options(decode_options(args.tolerant));

    let indices: Vec<usize> = if args.all {
        (0..file.len()).collect()
    } else {
        vec![resolve_frame(&file, &args.select)?]
    };

    let shape = row_shape(args.format, args.bare);
    let precision = decimals(args.precision, args.ramp.wanted());
    let mut columns = vec!["frame", "time_s"];
    if args.raw_counts {
        columns.push("raw");
    }
    columns.push("celsius");
    if args.ramp.wanted() {
        columns.push("normalized");
    }
    let mut rows = Rows::new(shape, &columns, args.header);

    for index in indices {
        let frame = decode(&mut reader, index)?;

        // `raw_at` is what reports an out-of-range pixel, and it reports it
        // against this frame's own dimensions.
        let raw = frame.raw_at(args.x, args.y)?;
        let celsius = table.celsius(raw);
        let mut cells = vec![Cell::Int(index as u64), seconds(&file, index)];
        if args.raw_counts {
            cells.push(Cell::Int(u64::from(raw)));
        }
        cells.push(Cell::Fixed(celsius, precision));
        if args.ramp.wanted() {
            // The ramp is stretched across this frame, so the whole image has
            // to be converted even though only one pixel is reported.
            let ramp = ColorRamp::across(&frame.temperatures_with(&table), &args.ramp);
            cells.push(Cell::Fixed(ramp.position(celsius), precision));
        }
        rows.write(out, &cells)?;
    }
    rows.finish(out)?;
    Ok(())
}

// ------------------------------------------------------------------ frames --

/// A frame's pixels, in whichever unit was asked for.
enum Grid<'a> {
    Celsius(&'a [f32]),
    Counts(&'a [u16]),
    /// Positions on the colour ramp, 0 to 1.
    Ramp(&'a [f32]),
}

impl Grid<'_> {
    fn len(&self) -> usize {
        match self {
            Grid::Celsius(values) | Grid::Ramp(values) => values.len(),
            Grid::Counts(values) => values.len(),
        }
    }

    fn unit(&self) -> &'static str {
        match self {
            Grid::Celsius(_) => "celsius",
            Grid::Counts(_) => "count",
            Grid::Ramp(_) => "normalized",
        }
    }

    fn cell(&self, at: usize, precision: usize) -> Cell {
        match self {
            Grid::Celsius(values) | Grid::Ramp(values) => Cell::Fixed(values[at], precision),
            Grid::Counts(values) => Cell::Int(u64::from(values[at])),
        }
    }

    /// The value as it goes into `--format raw`: little-endian, so a reader on
    /// another machine gets the same numbers.
    fn bytes(&self, at: usize) -> [u8; 4] {
        match self {
            Grid::Celsius(values) | Grid::Ramp(values) => values[at].to_le_bytes(),
            Grid::Counts(values) => {
                let [low, high] = values[at].to_le_bytes();
                [low, high, 0, 0]
            }
        }
    }

    fn byte_width(&self) -> usize {
        match self {
            Grid::Celsius(_) | Grid::Ramp(_) => 4,
            Grid::Counts(_) => 2,
        }
    }
}

fn write_grid(
    out: &mut impl Write,
    grid: &Grid<'_>,
    frame: &Frame,
    index: usize,
    file: &CsqFile,
    args: &TempsArgs,
) -> Result<(), Failure> {
    let (width, height) = (frame.width(), frame.height());
    let precision = decimals(args.precision, args.ramp.wanted());
    let unit = grid.unit();

    if args.header && !matches!(args.format, GridFormat::Json | GridFormat::Raw) {
        writeln!(
            out,
            "# frame={index} width={width} height={height} unit={unit}"
        )?;
    }

    match args.format {
        GridFormat::Tsv | GridFormat::Csv => {
            let separator = if args.format == GridFormat::Csv {
                ','
            } else {
                '\t'
            };
            for row in 0..height {
                for column in 0..width {
                    if column > 0 {
                        write!(out, "{separator}")?;
                    }
                    write!(out, "{}", grid.cell(row * width + column, precision).text())?;
                }
                out.write_all(b"\n")?;
            }
        }
        GridFormat::Long => {
            if args.header {
                writeln!(out, "x\ty\t{unit}")?;
            }
            for row in 0..height {
                for column in 0..width {
                    let value = grid.cell(row * width + column, precision).text();
                    writeln!(out, "{column}\t{row}\t{value}")?;
                }
            }
        }
        GridFormat::Json => {
            // Written by hand rather than through a `Value`, so a megapixel
            // frame does not have to exist twice in memory.
            write!(
                out,
                "{{\"frame\":{index},\"time_s\":{},\"width\":{width},\"height\":{height},\
                 \"unit\":\"{unit}\",\"values\":[",
                seconds(file, index).text()
            )?;
            for at in 0..grid.len() {
                if at > 0 {
                    out.write_all(b",")?;
                }
                grid.cell(at, precision).write_json_to(out)?;
            }
            out.write_all(b"]}\n")?;
        }
        GridFormat::Raw => {
            let stride = grid.byte_width();
            let mut buffer = Vec::with_capacity(stride * 4096);
            for at in 0..grid.len() {
                buffer.extend_from_slice(&grid.bytes(at)[..stride]);
                if buffer.len() >= stride * 4096 {
                    out.write_all(&buffer)?;
                    buffer.clear();
                }
            }
            out.write_all(&buffer)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- metadata --

fn describe(label: &str, file: &CsqFile, index: usize, metadata: &FrameMetadata) -> Pairs {
    let mut pairs = Pairs::new();

    pairs.text("file.path", label);
    pairs.int("file.bytes", file.as_bytes().len() as u64);
    pairs.int("file.frames", file.len() as u64);
    // Metadata-only decoding zeroes the geometry, so ask the file, which reads
    // it back out of the image record header.
    let (width, height) = file
        .dimensions()
        .unwrap_or((metadata.width, metadata.height));
    pairs.int("file.width", width as u64);
    pairs.int("file.height", height as u64);
    if let Some(rate) = file.frame_rate() {
        pairs.float("file.frame_rate_hz", rate);
    }
    if let Some(duration) = file.duration() {
        pairs.seconds("file.duration_s", duration.as_secs_f64());
    }
    // Everything below is zero for an intact recording, and worth seeing when
    // it is not.
    pairs.int(
        "file.resynchronisations",
        file.index().resynchronisations() as u64,
    );
    pairs.int("file.padded_frames", file.index().padded_frames() as u64);
    pairs.int("file.trailing_bytes", file.index().trailing_bytes());

    pairs.int("frame.index", index as u64);
    if let Some(time) = file.frame_time(index) {
        pairs.seconds("frame.time_s", time.as_secs_f64());
    }
    if let Some(timestamp) = metadata.timestamp {
        pairs.text("frame.timestamp", &timestamp.to_rfc3339());
    }

    let camera = &metadata.camera;
    pairs.text_if_set("camera.model", &camera.model);
    pairs.text_if_set("camera.part_number", &camera.part_number);
    pairs.text_if_set("camera.serial_number", &camera.serial_number);
    pairs.text_if_set("camera.software", &camera.software);
    pairs.text_if_set("lens.model", &camera.lens_model);
    pairs.text_if_set("lens.part_number", &camera.lens_part_number);
    pairs.text_if_set("lens.serial_number", &camera.lens_serial_number);
    pairs.text_if_set("filter.model", &camera.filter_model);
    pairs.text_if_set("filter.part_number", &camera.filter_part_number);
    pairs.text_if_set("filter.serial_number", &camera.filter_serial_number);

    pairs.float("optics.field_of_view_deg", metadata.field_of_view);
    pairs.float("optics.focus_distance_m", metadata.focus_distance);
    pairs.int(
        "optics.focus_step_count",
        u64::from(metadata.focus_step_count),
    );

    let scene = &metadata.radiometric;
    pairs.float("scene.emissivity", scene.emissivity);
    pairs.float("scene.object_distance_m", scene.object_distance);
    pairs.float(
        "scene.reflected_temperature_c",
        scene.reflected_apparent_temperature,
    );
    pairs.float(
        "scene.atmospheric_temperature_c",
        scene.atmospheric_temperature,
    );
    pairs.float("scene.relative_humidity_pct", scene.relative_humidity);
    pairs.float("scene.window_temperature_c", scene.ir_window_temperature);
    pairs.float("scene.window_transmission", scene.ir_window_transmission);

    pairs.float("planck.r1", scene.planck.r1);
    pairs.float("planck.b", scene.planck.b);
    pairs.float("planck.f", scene.planck.f);
    pairs.float("planck.o", scene.planck.o);
    pairs.float("planck.r2", scene.planck.r2);

    pairs.float("atmospheric.alpha1", scene.atmospheric.alpha1);
    pairs.float("atmospheric.alpha2", scene.atmospheric.alpha2);
    pairs.float("atmospheric.beta1", scene.atmospheric.beta1);
    pairs.float("atmospheric.beta2", scene.atmospheric.beta2);
    pairs.float("atmospheric.x", scene.atmospheric.x);

    let range = &metadata.temperature_range;
    pairs.float("range.min_c", range.min);
    pairs.float("range.max_c", range.max);
    pairs.float("range.min_clip_c", range.min_clip);
    pairs.float("range.max_clip_c", range.max_clip);
    pairs.float("range.min_warn_c", range.min_warn);
    pairs.float("range.max_warn_c", range.max_warn);
    pairs.float("range.min_saturated_c", range.min_saturated);
    pairs.float("range.max_saturated_c", range.max_saturated);

    let raw = &metadata.raw_value_range;
    pairs.int("raw.min", u64::from(raw.min));
    pairs.int("raw.max", u64::from(raw.max));
    pairs.int("raw.median", u64::from(raw.median));
    pairs.int("raw.range", u64::from(raw.range));

    if let Some(gps) = &metadata.gps {
        pairs.number("gps.latitude", format!("{:.7}", gps.latitude));
        pairs.number("gps.longitude", format!("{:.7}", gps.longitude));
        pairs.float("gps.altitude_m", gps.altitude);
        if let Some(dop) = gps.dop {
            pairs.float("gps.dop", dop);
        }
        if let Some(direction) = gps.image_direction {
            pairs.float("gps.image_direction_deg", direction);
        }
        pairs.text_if_set("gps.image_direction_ref", &gps.image_direction_ref);
        pairs.text_if_set("gps.map_datum", &gps.map_datum);
    }

    if let Some(palette) = &metadata.palette {
        pairs.text_if_set("palette.name", &palette.name);
        pairs.text_if_set("palette.file_name", &palette.file_name);
        pairs.int("palette.colors", palette.colors.len() as u64);
    }

    pairs
}

// ----------------------------------------------------------------- helpers --

/// Opens the recording, or reads all of stdin when the path is `-`.
///
/// Standard input has to be slurped because a CSQ carries no index: the file is
/// walked once on open so any frame can be reached afterwards.
fn open(input: &Input) -> Result<(CsqFile, String), Failure> {
    if input.path == Path::new("-") {
        let mut bytes = Vec::new();
        io::stdin().lock().read_to_end(&mut bytes)?;
        Ok((CsqFile::from_bytes(bytes)?, "-".to_owned()))
    } else {
        let file = CsqFile::open(&input.path)
            .map_err(|error| format!("{}: {error}", input.path.display()))?;
        Ok((file, input.path.display().to_string()))
    }
}

fn resolve_frame(file: &CsqFile, select: &FrameSelect) -> Result<usize, Failure> {
    if file.is_empty() {
        return Err("recording contains no frames".into());
    }

    if let Some(seconds) = select.time {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(format!("--time {seconds} is not a position in a recording").into());
        }
        return file
            .frame_index_at(Duration::from_secs_f64(seconds))
            .ok_or_else(|| {
                format!(
                    "no frame at {seconds}s; the recording is {:.3}s long",
                    file.duration().unwrap_or_default().as_secs_f64()
                )
                .into()
            });
    }

    let wanted = select.frame.unwrap_or(0);
    let index = if wanted < 0 {
        file.len().checked_sub(wanted.unsigned_abs() as usize)
    } else {
        Some(wanted as usize)
    };

    match index {
        Some(index) if index < file.len() => Ok(index),
        _ => Err(format!(
            "frame {wanted} is out of range; the recording has {} frames",
            file.len()
        )
        .into()),
    }
}

fn selection(file: &CsqFile, range: &FrameRange) -> Result<Vec<usize>, Failure> {
    if range.step == 0 {
        return Err("--step must be at least 1".into());
    }
    let end = range
        .count
        .map_or(file.len(), |count| range.start.saturating_add(count))
        .min(file.len());
    Ok((range.start..end).step_by(range.step).collect())
}

/// The temperature table to convert with: the camera's own values, unless the
/// caller overrode some of them.
///
/// One table serves a whole run. It is a 65 536-entry lookup, so building it
/// per frame would cost more than decoding the frames.
fn scene_table(
    file: &CsqFile,
    index: usize,
    overrides: &Scene,
) -> Result<TemperatureTable, Failure> {
    let mut parameters: RadiometricParameters = file
        .frame_metadata(index)
        .map(|metadata| metadata.radiometric)
        .or_else(|error| match file.metadata() {
            Some(metadata) => Ok(metadata.radiometric),
            None => Err(error),
        })?;

    if let Some(value) = overrides.emissivity {
        parameters.emissivity = value;
    }
    if let Some(value) = overrides.object_distance {
        parameters.object_distance = value;
    }
    if let Some(value) = overrides.reflected_temperature {
        parameters.reflected_apparent_temperature = value;
    }
    if let Some(value) = overrides.atmospheric_temperature {
        parameters.atmospheric_temperature = value;
    }
    if let Some(value) = overrides.humidity {
        parameters.relative_humidity = value;
    }
    if let Some(value) = overrides.window_temperature {
        parameters.ir_window_temperature = value;
    }
    if let Some(value) = overrides.window_transmission {
        parameters.ir_window_transmission = value;
    }
    Ok(parameters.temperature_table())
}

/// The span a renderer would stretch the colour ramp across, and where a
/// temperature falls on it.
///
/// Both the span and the mapping come from [`csq::render`], so `--normalize`
/// reports the same number the renderer would turn into a colour rather than a
/// second, quietly different, definition of "normalised".
struct ColorRamp {
    min: f32,
    span: f32,
}

impl ColorRamp {
    fn across(image: &TemperatureImage, options: &Ramp) -> Self {
        let scale = match (options.min, options.max) {
            (Some(min), Some(max)) => Scale::Fixed { min, max },
            // `Auto` is the coldest and the warmest pixel of this frame, which
            // is what `--normalize` on its own means.
            _ => Scale::Auto,
        };
        let (min, max) = scale.range_for(image).unwrap_or((0.0, 1.0));
        Self {
            min,
            span: max - min,
        }
    }

    /// Where `celsius` sits on the ramp, 0 to 1.
    ///
    /// Temperatures outside a fixed span clamp to the ends, as they would when
    /// rendered; a count with no temperature stays `NaN` rather than silently
    /// landing on the cold end.
    fn position(&self, celsius: f32) -> f32 {
        ((celsius - self.min) / self.span).clamp(0.0, 1.0)
    }
}

/// Decimal places to print. A temperature wants two; a 0-to-1 ramp position
/// wants enough to survive the 256 steps a renderer would quantise it to.
fn decimals(requested: Option<usize>, normalising: bool) -> usize {
    requested.unwrap_or(if normalising { 4 } else { 2 })
}

fn decode_options(tolerant: bool) -> DecodeOptions {
    if tolerant {
        DecodeOptions::tolerant()
    } else {
        DecodeOptions::all()
    }
}

fn row_shape(format: RowFormat, bare: bool) -> RowShape {
    if bare {
        return RowShape::Bare;
    }
    match format {
        RowFormat::Tsv => RowShape::Separated('\t'),
        RowFormat::Csv => RowShape::Separated(','),
        RowFormat::Json => RowShape::JsonArray,
        RowFormat::Jsonl => RowShape::JsonLines,
    }
}

fn seconds(file: &CsqFile, index: usize) -> Cell {
    Cell::Seconds(file.frame_time(index).unwrap_or_default().as_secs_f64())
}

/// Decodes one frame, naming it in anything that goes wrong.
///
/// A salvaged frame is mostly plausible-looking nonsense below the cut, so that
/// gets said on stderr rather than passing silently as data.
fn decode(reader: &mut CsqReader<'_>, index: usize) -> Result<Frame, Failure> {
    let frame = reader
        .frame(index)
        .map_err(|error| format!("frame {index}: {error}"))?;
    if frame.is_truncated() {
        eprintln!(
            "csq-reader: frame {index} is truncated: {} of {} rows decoded",
            frame.decoded_rows(),
            frame.height()
        );
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ramp_maps_its_span_onto_zero_to_one() {
        let ramp = ColorRamp {
            min: 10.0,
            span: 30.0,
        };
        assert_eq!(ramp.position(10.0), 0.0);
        assert_eq!(ramp.position(25.0), 0.5);
        assert_eq!(ramp.position(40.0), 1.0);
    }

    #[test]
    fn temperatures_outside_a_fixed_span_clamp_to_its_ends() {
        let ramp = ColorRamp {
            min: 10.0,
            span: 30.0,
        };
        assert_eq!(ramp.position(-40.0), 0.0);
        assert_eq!(ramp.position(150.0), 1.0);
    }

    #[test]
    fn a_count_with_no_temperature_has_no_position_either() {
        let ramp = ColorRamp {
            min: 10.0,
            span: 30.0,
        };
        assert!(ramp.position(f32::NAN).is_nan());
    }

    #[test]
    fn normalising_asks_for_more_decimals_unless_told_otherwise() {
        assert_eq!(decimals(None, false), 2);
        assert_eq!(decimals(None, true), 4);
        assert_eq!(decimals(Some(1), true), 1);
    }
}
