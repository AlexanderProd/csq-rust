//use peck_exif::exif::{create_list_from_vec, exiftool_available, Exif, Mode};
use std::io::BufReader;
use std::fs::File;
use pcre2::bytes::Regex;
use std::str;

fn main() {
    
    fn populate_list() -> std::io::Result<()> {
        let file = File::open("FLIR0116.csq")?;
        let reader = BufReader::new(file);
        let magic_seq = Regex::new(str::from_utf8(b"\x46\x46\x46\x00\x52\x54").unwrap()).unwrap();

        let buf = reader.buffer();

        if buf.len() == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Empty file"));
        }

        let matches: Vec<pcre2::bytes::Match> = magic_seq.find_iter(buf).filter_map(|x| x.ok()).collect();
        if matches.len() == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "No matches found"));
        }


        Ok(())
    }

    let _ = match populate_list() {
        Ok(_) => println!("Ok"),
        Err(e) => println!("Err: {}", e),
    };
}
