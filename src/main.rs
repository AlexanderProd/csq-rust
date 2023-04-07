//use peck_exif::exif::{create_list_from_vec, exiftool_available, Exif, Mode};
#[macro_use]
extern crate lazy_static;

use std::io::Read;
use std::fs::File;
use pcre2::bytes::Regex;
use std::str;

const BLOCKSIZE: usize = 1000000;

lazy_static! {
    static ref MAGIC_SEQUENCE: Regex = Regex::new(str::from_utf8(b"\x46\x46\x46\x00\x52\x54").unwrap()).unwrap();
}

struct CSQReader {
    file: File,
    leftover: Vec<u8>,
    imgs: Vec<Vec<u8>>,
}

impl CSQReader {
    fn new (filename: String) -> Self {
        let file = File::open(filename.clone()).unwrap_or_else(|_| {
            panic!("Failed to open file: {}", filename);
        });
        Self {
            file,
            leftover: vec![],
            imgs: vec![],
        }
    }

    fn populate_list (&mut self) -> std::io::Result<()> {
        let mut buffer = [0; BLOCKSIZE];
        self.file.read(&mut buffer)?;

        if buffer.len() == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Empty file"));
        }

        let matches: Vec<pcre2::bytes::Match> = MAGIC_SEQUENCE.find_iter(&buffer).filter_map(|x| x.ok()).collect();
        if matches.len() == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "No matches found"));
        }

        let start = matches[0].start();

        if !self.leftover.is_empty() {
            self.imgs.push([self.leftover.to_vec(), buffer[0..start].to_vec()].concat());
        } 

        if matches[1..] == [] {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "No matches found"));
        }

        let mut end: Option<usize> = None;
        for (m1, m2) in matches.iter().zip(matches[1..].iter()) {
            let start = m1.start();
            end = Some(m2.start());

            let img = buffer[start..end.unwrap()].to_vec();
            self.imgs.push(img);
        }

        self.leftover = buffer[end.unwrap()..].to_vec();

        println!("Found {} matches", self.imgs.len());

        Ok(())
    }
}

fn main() {

    let mut reader = CSQReader::new("FLIR0116.csq".to_string());

    let _ = match reader.populate_list() {
        Ok(_) => (),
        Err(e) => println!("Err: {}", e),
    };
}
