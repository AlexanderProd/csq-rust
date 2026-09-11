# csq-rs

FLIR CSQ thermal recordings in pure Rust: per-pixel temperatures, frame
indexing, seeking, rendering and recording. No `exiftool` subprocess, no C++
toolchain, no system libraries.

| Crate                             |         |                                                                                    |
| --------------------------------- | ------- | ---------------------------------------------------------------------------------- |
| [`csq`](crates/csq)               | library | the FFF container parser, the JPEG-LS codec and the radiometric model              |
| [`csq-reader`](crates/csq-reader) | binary  | a shell front end — temperatures and metadata on stdout, for `awk`, `cut` and `jq` |

```rust
let file = csq::CsqFile::open("recording.csq")?;
let frame = file.frame(0)?;
println!("centre: {:.1} °C", frame.temperatures().at(512, 384)?);
```

Recordings can be written as well as read, from frames of temperatures, at
rates a live 60 fps feed keeps up with:

```rust
let mut writer = csq::write::create("out.csq", metadata)?;
writer.write_celsius(&celsius)?;
writer.finish()?;
```

```console
$ csq-reader pixel recording.csq -x 512 -y 384 --all | cut -f3
18.00
17.96
17.94
```

They are separate crates, the CLI being a wrapper around the library, check the README in each directory for more details.

```
cargo build --workspace          # both
cargo test  --workspace          # decoder ground truth, pipeline, output formats
cargo install csq-reader         # just the tool
```

## Licence

MIT or Apache-2.0, at your option.
