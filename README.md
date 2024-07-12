# CSQ

This is a library to decode CSQ files from FLIR thermal imaging cameras.

## Installation

`csq` requires a Python environment to run because it utilizes the `pylibjpeg` library to decode JPEG-LS images. Currently, there is no native Rust library available that can perform this task.

Also [exiftool](https://exiftool.org/) needs to be installed on your system required to get the metadata of the CSQ file.

## Example

The example directory contains an example of how to use the library to convert a CSQ file to a video file using the `ffmpeg` library.

## Optimizations

- use native JPEG-LS deocder in Rust
- use rayon in ndarray for parallelism
- use blas in ndarray for matrix multiplication
