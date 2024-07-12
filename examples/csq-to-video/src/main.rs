use anyhow::Result;
use clap::Parser;
use csq::CSQReader;
use image::{Rgb, RgbImage};
use ndarray::Array2;
use opencv::{
    core::{Mat, CV_8UC3},
    prelude::MatTraitManual,
    videoio::{VideoWriter, VideoWriterTrait},
};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
};

type Frame = Array2<f32>;

#[derive(Parser)]
struct Cli {
    #[clap(short = 'i', long = "input-file")]
    input_file: PathBuf,
    #[clap(short = 'o', long = "output-dir")]
    output_dir: Option<PathBuf>,
}

pub fn create_image_from_frame(frame: &Frame) -> Result<RgbImage> {
    let (min_temp, max_temp) = frame.iter().fold(
        (f32::INFINITY, f32::NEG_INFINITY),
        |(min_temp, max_temp), value| (min_temp.min(*value), max_temp.max(*value)),
    );

    let normalized_values = frame.mapv(|v| {
        let min = min_temp;
        let max = max_temp;
        (v - min) / (max - min)
    });

    let width = normalized_values.shape()[1] as u32;
    let height = normalized_values.shape()[0] as u32;

    let mut img = RgbImage::new(width, height);

    let gradient = colorgrad::rainbow();

    for (x, column) in normalized_values.columns().into_iter().enumerate() {
        for (y, &value) in column.iter().enumerate() {
            let color = gradient.at(value.into());
            img.put_pixel(
                x as u32,
                y as u32,
                Rgb([
                    (color.r * 255.0) as u8,
                    (color.g * 255.0) as u8,
                    (color.b * 255.0) as u8,
                ]),
            );
        }
    }

    Ok(img)
}

fn main() -> Result<()> {
    let args = Cli::parse();

    let mut reader = CSQReader::new(&args.input_file);

    let (frames_tx, frames_rx) = mpsc::channel::<Frame>();

    let video_writer = Arc::new(Mutex::new(
        VideoWriter::new(
            "test.mp4",
            VideoWriter::fourcc('a', 'v', 'c', '1').expect("Failed to create fourcc"),
            30.0,
            (1024, 768).into(),
            true,
        )
        .expect("Failed to create video writer"),
    ));

    let video_writer_clone = video_writer.clone();
    let processing_thread = thread::spawn(move || {
        while let Ok(data) = frames_rx.recv() {
            let img = create_image_from_frame(&data).unwrap();

            let (width, height) = (&img.width(), &img.height());
            let raw = img.into_raw();

            let mut mat =
                Mat::new_rows_cols_with_default(*height as i32, *width as i32, CV_8UC3, 0.into())
                    .unwrap();
            mat.data_bytes_mut().unwrap().copy_from_slice(&raw);

            let mut locked_writer = video_writer_clone.lock().unwrap();

            let _ = locked_writer.write(&mat);
        }
    });

    for (i, frame) in reader.frames().enumerate() {
        match frame {
            Ok(frame) => {
                frames_tx.send(*frame).unwrap();
                println!("Frame: {}", i + 1);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        }
    }

    // Close the sending side of the frames channel to indicate no more frames are coming
    drop(frames_tx);

    // Wait for the processing thread to finish
    processing_thread.join().unwrap();

    let mut locked_writer = video_writer.lock().unwrap();
    locked_writer.release()?;

    Ok(())
}
