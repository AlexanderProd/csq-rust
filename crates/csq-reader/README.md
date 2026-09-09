# csq-reader

Read FLIR CSQ thermal recordings from a shell

```
cargo install csq-reader
```

It is a front end to the [`csq`](https://crates.io/crates/csq) library — a pure
Rust FFF container parser, JPEG-LS decoder and radiometric model, with no
`exiftool` subprocess and no C++ toolchain behind it.

| Command  | Purpose                                              |
| -------- | ---------------------------------------------------- |
| `info`   | metadata as `key<TAB>value` lines                    |
| `frames` | one line of temperature statistics per frame         |
| `temps`  | every temperature of one frame                       |
| `pixel`  | the temperature of one pixel, in one frame or in all |

```console
$ csq-reader info recording.csq
file.frames	22
file.width	1024
file.height	768
camera.model	FLIR T1020
scene.emissivity	1
...

$ csq-reader frames recording.csq -H
frame	time_s	min_c	mean_c	max_c
0	0.000000	15.59	18.81	33.37
1	0.033333	15.52	18.81	33.30

$ csq-reader pixel recording.csq -x 512 -y 384 --bare
18.00
```

## Formats

Every command takes `-F/--format`. `info` renders as `tsv`, `csv` or `json`;
`frames` and `pixel` add `jsonl`, one object per line for streaming into `jq`;
`temps` writes a `tsv`/`csv` grid, `long` (`x<TAB>y<TAB>celsius`, one pixel per
line), `json`, or `raw` — little-endian `f32` row-major, which
`numpy.fromfile(dtype='<f4')` reads directly.

The text and the JSON renderings of a value are produced from the same place,
so `-F tsv` and `-F json` agree digit for digit rather than drifting apart, and
JSON fields come out in a readable order rather than an alphabetical one.

```console
$ csq-reader temps recording.csq -f 12 -F raw \
    | python3 -c "import numpy,sys; print(numpy.fromfile(sys.stdin.buffer,'<f4').reshape(768,1024).max())"

$ csq-reader frames recording.csq | awk -F'\t' '$5 > 80 {print $1}'      # frames with a hot pixel
$ csq-reader pixel recording.csq -x 512 -y 384 --all | cut -f3           # one pixel over time
$ csq-reader info recording.csq -F json | jq -r .camera.model
$ cat recording.csq | csq-reader info -                                  # or read from a pipe
```

## Normalised values

`--normalize` prints each value as its position on the colour ramp instead of a
temperature: 0 at the frame's coldest pixel, 1 at its warmest. That is the
number a renderer turns into a colour, taken from the same `csq::render::Scale`
the library's own renderer uses, so a normalised frame and a rendered picture
agree pixel for pixel.

```console
$ csq-reader temps recording.csq -f 12 --normalize -F raw > frame12.f32
$ csq-reader pixel recording.csq -x 512 -y 384 --normalize -H
frame	time_s	celsius	normalized
12	0.400000	18.0700	0.1360
```

For `temps` it replaces the temperature; for `pixel` it adds a `normalized`
column beside it, so `--bare` gives the ramp position on its own.

The span is each frame's own extremes, which means values from different frames
are *not* comparable — a pixel can climb the ramp while its temperature falls,
because the frame's extremes moved underneath it. `--min` and `--max` fix the
span in °C instead (and imply `--normalize`), which is what makes a series
comparable; temperatures outside it clamp to the ends, exactly as they would
when rendered.

```console
$ csq-reader pixel recording.csq -x 512 -y 384 --all --normalize      # per frame
0	0.000000	17.9990	0.1356
1	0.033333	17.9635	0.1373

$ csq-reader pixel recording.csq -x 512 -y 384 --all --min 10 --max 40
0	0.000000	17.9990	0.2666
1	0.033333	17.9635	0.2655
```

Normalised output defaults to four decimals rather than two, since two would
quantise more coarsely than the 256 steps a renderer uses; `-p` overrides it.

## Picking a frame, and picking the physics

Frames are addressed by index (`-f 12`, `-f -1` for the last) or by playback
position (`--time 4.5`). Seeking is free: every CSQ frame is independently
coded, so nothing between the start and the frame you asked for is decoded.

A camera stores the scene parameters it happened to be set to, and those
decide what the raw detector counts mean. Every command that produces
temperatures takes `--emissivity`, `--distance`, `--reflected-temp`,
`--atmospheric-temp`, `--humidity`, `--window-temp` and
`--window-transmission` to reconvert under different assumptions.
`--raw-counts` skips the conversion entirely and prints the 16-bit values.

## Behaviour in a pipeline

Output streams as it is produced, so `| head` stops the decode instead of being
ignored, and a closed pipe is not an error. Exit status is 0 on success, 1 on
failure and 2 on a usage error. `-` as the file reads the recording from stdin.

Damaged recordings — cut off mid-write, or with a frame whose image data stops
early — fail loudly and name the frame, unless `--tolerant` is given, which
salvages what decoded and reports the short frames on stderr rather than
letting plausible-looking nonsense pass as data.

## Licence

MIT
