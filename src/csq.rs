use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use ndarray::Array2;
use pcre2::bytes::Regex;
use peck_exif::exif::{exiftool_available, Exif, Mode};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::Command;
use std::str;
use std::time::Instant;
use tempfile::NamedTempFile;

use crate::utils::raw_to_temp;
use crate::{types::CSQExifData, utils::decode_jpeg_py};

const BLOCKSIZE: usize = 1000000;

lazy_static! {
    static ref MAGIC_SEQUENCE: Regex =
        Regex::new(str::from_utf8(b"\x46\x46\x46\x00\x52\x54").unwrap()).unwrap();
}

pub struct CSQReader {
    reader: BufReader<File>,
    leftover: Vec<u8>,
    imgs: Vec<Vec<u8>>,
    index: usize,
}

impl CSQReader {
    pub fn new(filename: &Path) -> Self {
        if !exiftool_available() {
            panic!("Exiftool not available for execution.");
        }

        let file = File::open(filename).unwrap_or_else(|_| {
            panic!("Failed to open file: {}", filename.display());
        });
        let reader = BufReader::new(file);

        Self {
            reader,
            leftover: vec![],
            imgs: vec![],
            index: 0,
        }
    }

    fn populate_list(&mut self) -> Result<()> {
        self.imgs.clear();
        self.index = 0;

        let mut buffer = [0; BLOCKSIZE];
        let read_amount = self.reader.read(&mut buffer[..])?;

        if read_amount == 0 {
            return Ok(());
        }

        if buffer.is_empty() {
            return Err(anyhow!("File is empty"));
        }

        let matches: Vec<pcre2::bytes::Match> = MAGIC_SEQUENCE
            .find_iter(&buffer)
            .filter_map(|x| x.ok())
            .collect();

        if matches.is_empty() {
            return Err(anyhow!("No matches found"));
        }

        let start = matches[0].start();

        if !self.leftover.is_empty() {
            self.imgs
                .push([&self.leftover[..], &buffer[..start]].concat());
        }

        if matches[1..].is_empty() {
            return Err(anyhow!("No more matches found"));
        }

        let mut end: usize = usize::default();
        for (m1, m2) in matches.iter().zip(matches[1..].iter()) {
            let start = m1.start();
            end = m2.start();

            let img = buffer[start..end].to_vec();
            self.imgs.push(img);
        }

        self.leftover = buffer[end..].to_vec();

        Ok(())
    }

    fn extract_data(&self, im: &[u8]) -> Result<(CSQExifData, Array2<f32>)> {
        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(im)?;
        temp_file.flush()?;

        let binary = Command::new("exiftool")
            .arg("-b")
            .arg("-RawThermalImage")
            .arg(temp_file.path())
            .output()?
            .stdout;

        let decoded = decode_jpeg_py(&binary)?;

        let now = Instant::now();
        let csq_exif_data = match Exif::new(temp_file.path(), Mode::All) {
            Ok(exif) => {
                let value = serde_json::to_value(exif.attributes)?;
                let csq_exif_data: CSQExifData = serde_json::from_value(value)?;
                let box_exif_data = Box::new(csq_exif_data);

                temp_file.close()?;
                Ok(*box_exif_data)
            }
            Err(e) => {
                temp_file.close()?;
                Err(anyhow!("Error extracting exif data: {}", e))
            }
        }?;
        println!("creating exif took: {:?}", now.elapsed());

        Ok((csq_exif_data, decoded))
    }

    pub fn next_frame(&mut self) -> Result<Option<Box<Array2<f32>>>> {
        if self.index >= self.imgs.len() {
            self.populate_list()?;

            if self.imgs.is_empty() {
                return Ok(None);
            }
        }

        let img = &self.imgs[self.index];

        let (metadata, decoded) = self.extract_data(img)?;

        let temps = raw_to_temp(&decoded, metadata)?;

        self.index += 1;

        Ok(Some(temps))
    }

    pub fn frames(&mut self) -> impl Iterator<Item = Result<Box<Array2<f32>>>> + '_ {
        std::iter::from_fn(move || match self.next_frame() {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
    }
}
