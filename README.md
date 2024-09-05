# CSQ

This is a library to decode CSQ files from FLIR thermal imaging cameras.

## Installation

[Exiftool](https://exiftool.org/) needs to be installed on your system which is required to get the metadata of the CSQ file.

## Example

The example directory contains an example of how to use the library to convert a CSQ file to a video file using the `ffmpeg` library.

## Optimizations

- [x] use (native) JPEG-LS deocder in Rust.
- [x] Use rayon in ndarray for parallelism.
- [ ] Use blas in ndarray for matrix multiplication.
- [ ] Run Exiftool with `-stay_open` it in a separate thread to avoid the overhead of starting the process every time.
