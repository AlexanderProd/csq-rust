use anyhow::{anyhow, Result};
use charls::CharLS;
use lazy_static::lazy_static;
use ndarray::{Array2, ShapeBuilder};
use pcre2::bytes::Regex;
use peck_exif::exif::{exiftool_available, Exif, Mode};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::Command;
use std::str;
use tempfile::NamedTempFile;

use crate::types::CSQExifData;
use crate::utils::{raw_to_temp, vec_u8_to_f32};

const BLOCKSIZE: usize = 1000000;

lazy_static! {
    static ref MAGIC_SEQUENCE: Regex =
        Regex::new(str::from_utf8(b"\x46\x46\x46\x00\x52\x54").unwrap()).unwrap();
}

pub struct CSQReader {
    reader: BufReader<File>,
    leftover: Vec<u8>,
    imgs: Vec<Vec<u8>>,
    /// width, height
    image_size: (usize, usize),
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
            image_size: (0, 0),
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

    fn extract_data(&mut self) -> Result<(CSQExifData, Array2<f32>)> {
        let img = &self.imgs[self.index];

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(img)?;
        temp_file.flush()?;

        let binary = Command::new("exiftool")
            .arg("-b")
            .arg("-RawThermalImage")
            .arg(temp_file.path())
            .output()?
            .stdout;

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

        self.set_image_size(&csq_exif_data);

        let decoded = self.decode_jpeg(&binary)?;

        Ok((csq_exif_data, decoded))
    }

    fn set_image_size(&mut self, exif_data: &CSQExifData) {
        self.image_size = (
            exif_data.raw_thermal_image_width as usize,
            exif_data.raw_thermal_image_height as usize,
        );
    }

    pub fn next_frame(&mut self) -> Result<Option<Box<Array2<f32>>>> {
        if self.index >= self.imgs.len() {
            self.populate_list()?;

            if self.imgs.is_empty() {
                return Ok(None);
            }
        }

        let (metadata, decoded) = self.extract_data()?;

        let temps = raw_to_temp(&metadata, &decoded)?;

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

    pub fn get_metadata(&mut self) -> Result<CSQExifData> {
        if self.index >= self.imgs.len() {
            self.populate_list()?;

            if self.imgs.is_empty() {
                return Err(anyhow!("No images found"));
            }
        }

        let (metadata, _) = self.extract_data()?;

        Ok(metadata)
    }

    pub fn decode_jpeg(&self, img: &[u8]) -> Result<Array2<f32>> {
        let mut charls = CharLS::default();

        let decoded = charls
            .decode(img)
            .map_err(|e| anyhow::anyhow!("Error decoding JPEG-LS: {}", e))?;

        let rows = self.image_size.0;
        let cols = self.image_size.1;

        let radiance_values = vec_u8_to_f32(&decoded);

        let arr = Array2::from_shape_vec((rows, cols).f(), radiance_values)
            .map_err(|e| anyhow!("Failed to create ndarray: {e}"))?
            .reversed_axes();

        Ok(arr)
    }
}
