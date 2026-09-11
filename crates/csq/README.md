# csq

Read FLIR CSQ thermal recordings in Rust: per-pixel temperatures, frame
indexing, seeking and rendering.

No `exiftool` subprocess, no C++ toolchain, no system libraries — the FFF
(FLIR File Format) container parser, the JPEG-LS decoder and the radiometric
model are all in this crate. The only mandatory dependency is `memmap2`.

```toml
[dependencies]
csq = "0.2"
```

## What it does

```rust
let file = csq::CsqFile::open("recording.csq")?;
println!("{} frames, {:?}", file.len(), file.duration());

let frame = file.frame(0)?;
let temperatures = frame.temperatures();
let (coldest, warmest) = temperatures.range().unwrap();
println!("{coldest:.1} °C to {warmest:.1} °C");
println!("centre: {:.1} °C", temperatures.at(512, 384)?);
```

Every CSQ frame is independently coded, so seeking is just moving a cursor —
there are no keyframes to hunt for and no state to rebuild:

```rust
use std::time::Duration;

let mut reader = file.reader();
reader.seek_to_time(Duration::from_secs(12))?;
let frame = reader.next_frame().unwrap()?;
```

A `CsqReader` keeps its decoding buffers between frames. Create as many as you
like from one `CsqFile` — one for playback, one for a scrub preview — since
`CsqFile` is `Send + Sync` and holds no decoder state.

For sources that cannot seek, `CsqStream` decodes from any `Read`:

```rust
let mut stream = csq::CsqStream::new(std::io::BufReader::new(file));
while let Some(frame) = stream.next_frame() {
    let frame = frame?;
    // ...
}
```

### Temperatures

Frames hold raw 16-bit detector counts, because converting them depends on
scene parameters you may want to change after the recording. `temperatures()`
uses the values the camera stored; overriding them means editing
`RadiometricParameters` and building a table:

```rust
let mut parameters = file.metadata().unwrap().radiometric;
parameters.emissivity = 0.95;
parameters.object_distance = 3.0;
let table = parameters.temperature_table();

while let Some(frame) = reader.next_frame() {
    let celsius = frame?.temperatures_with(&table);
}
```

Reuse one `TemperatureTable` across frames. Raw counts are 16-bit, so the whole
mapping fits in a 65 536-entry table — which is already twelve times less work
than evaluating the exponential and logarithm per pixel on a 1024×768 frame,
and free after the first frame.

### Metadata

Everything ExifTool reports is parsed natively: camera and lens identification,
Planck calibration constants, scene parameters, capture timestamp with UTC
offset, GPS fix, and the display palette the camera had selected.

```rust
let metadata = file.metadata().unwrap();
println!("{} #{}", metadata.camera.model, metadata.camera.serial_number);
println!("recorded {}", metadata.timestamp.unwrap());
if let Some(gps) = &metadata.gps {
    println!("at {:.5}, {:.5}", gps.latitude, gps.longitude);
}
```

### Rendering

`render::Renderer` maps temperatures onto a colour ramp and writes plain RGB8,
which any image or video crate will take.

```rust
use csq::render::{ColorMap, Renderer, Scale};

let renderer = Renderer::new()
    .with_colormap(ColorMap::Ironbow)
    .with_scale(Scale::Percentile { low: 0.02, high: 0.98 });
let rgb = renderer.render(&temperatures);
```

`Scale::Fixed` keeps colours comparable between frames and recordings, which is
what you want for video; `Scale::Percentile` clips outliers so one reflective
hot spot cannot flatten the scene. `ColorMap::Camera` renders with the palette
stored in the file.

## Command line

A shell-friendly front end lives beside this crate as
[`csq-reader`](https://crates.io/crates/csq-reader): metadata, per-frame statistics, whole frames and
single pixels, printed for `awk`, `cut` and `jq`.

```
cargo install csq-reader
csq-reader pixel recording.csq -x 512 -y 384 --all | cut -f3
csq-reader temps recording.csq -f 12 --normalize      # `render::Scale` as numbers
```

`examples/csq-tool.rs` covers the picture-producing side — PNG export,
thumbnails, `ffmpeg` video:

```
cargo run --release --example csq-tool -- export recording.csq -o out/
```

## Performance

Measured on an M-series Mac, 1024×768 frames from a FLIR T1020:

| Operation                             | Cost                      |
| ------------------------------------- | ------------------------- |
| Index a 361 MB / 2108-frame recording | 2.7 ms warm, ~350 ms cold |
| Decode + convert to °C                | ~60 fps (2× realtime)     |
| Seek + decode + render + write a PNG  | ~26 ms                    |

Indexing reads 64 bytes per frame — every FFF header states its own length — so
it costs one page fault per frame rather than a scan. Cold timings are
dominated by storage latency; a 3.9 GB recording on an external USB SSD indexes
in about 10 s cold and milliseconds thereafter.

## Feature flags

| Feature   | Default | Effect                                                           |
| --------- | ------- | ---------------------------------------------------------------- |
| `mmap`    | yes     | memory-map files instead of reading them in                      |
| `png`     | yes     | decode FLIR files whose thermal image is PNG rather than JPEG-LS |
| `rayon`   | no      | `CsqFile::decode_all` for parallel decoding                      |
| `serde`   | no      | `Serialize`/`Deserialize` for metadata types                     |
| `ndarray` | no      | `TemperatureImage::to_array2`                                    |

## File format notes

FFF is FLIR's per-frame container: one thermal image plus the records that go
with it, starting at the `FFF\0` signature. A `.csq` file is a bare
concatenation of those frames — no outer wrapper. Each frame has a 64-byte
header, a record directory, and records holding the radiometric image, the
calibration data, the palette and any GPS fix. The frame header states the
frame's own total length at offset `0x34`, which is what makes indexing cheap.

The radiometric image is normally a single-component 16-bit JPEG-LS stream, and
usually **near-lossless**. The `NEAR` bound varies from frame to frame within a
single file — a T1020 recording was seen switching between 5 and 16 to hit a
bitrate target, while a T560 held 4 throughout — so it is read per stream.
Uncompressed 16-bit and PNG-encoded thermal images are handled too.

`csq::fff` and `csq::jpegls` are public, so the container walker and the JPEG-LS
decoder can be used on their own.

### Format variations handled

These came out of a sweep over every recording listed under
[tested cameras](#tested-cameras). Each is covered by a test:

- **Big-endian containers.** The T450sc and T650sc write the frame header and
  record directory big-endian while leaving the record _bodies_ little-endian.
  The byte order is detected per frame from the record directory.
- **Zero length fields.** Some producers (`MTX IR`, `ATAU_RBFO`) leave the frame
  length unset; the frame's extent is recovered by scanning instead.
- **Frames padded to a fixed slot.** Reported through
  `FrameIndex::padded_frames`, which is deliberately separate from
  `resynchronisations` so that padding is not mistaken for damage.
- **Mixed frame types in one file**, including uncompressed 640×512 frames
  interleaved with compressed ones.

### Damaged recordings

Recordings cut off mid-write, or with a corrupt header in the middle, do occur.
When a length field does not lead to the next signature the index resynchronises
by scanning forward, so one bad frame costs one frame rather than the rest of
the file; `FrameIndex::resynchronisations` and `FrameIndex::trailing_bytes`
report what was found.

Individual frames can also have JPEG-LS data that simply stops early. By default
that is an error, because such a frame decodes into plausible-looking but
meaningless pixels — the wrong default for measurement data. Pass
`DecodeOptions::tolerant()` to salvage what is there instead, and check
`Frame::is_truncated` and `Frame::decoded_rows` to see how much of the frame is
real.

## Tested cameras

The library has been run over recordings from **34 camera models**,
covering eight sensor resolutions: 240×180, 320×240, 320×256, 384×288, 464×348,
480×640, 640×480 and 1024×768.

**T-series handhelds**

| Model   | Sensor   |
| ------- | -------- |
| T450sc  | 320×240  |
| T460    | 320×240  |
| T530    | 320×240  |
| T540    | 464×348  |
| T560    | 640×480  |
| T650sc  | 640×480  |
| T660    | 640×480  |
| T840    | 464×348  |
| T860    | 640×480  |
| T865    | 640×480  |
| T1020   | 1024×768 |
| T1030sc | 1024×768 |
| T1040   | 1024×768 |
| T1050sc | 1024×768 |

**E-series handhelds**

| Model   | Sensor           |
| ------- | ---------------- |
| E52     | 240×180          |
| E53     | 240×180          |
| E54     | 320×240          |
| E75     | 320×240, 464×348 |
| E76     | 320×240, 464×348 |
| E85     | 384×288, 464×348 |
| E86     | 464×348          |
| E86-EST | 464×348          |
| E95     | 464×348          |
| E96     | 640×480          |

**A-series fixed-mount**

| Model | Sensor  |
| ----- | ------- |
| AX5   | 320×256 |
| A50   | 464×348 |
| A70   | 640×480 |
| A615  | 640×480 |
| A700  | 640×480 |

**Optical gas imaging**

| Model | Sensor  |
| ----- | ------- |
| GF77  | 320×240 |
| Gx320 | 320×240 |
| G620  | 640×480 |
| Gx620 | 640×480 |

**Other**

| Model        | Sensor  |
| ------------ | ------- |
| ONE Edge Pro | 480×640 |

## Licence

MIT OR Apache-2.0
