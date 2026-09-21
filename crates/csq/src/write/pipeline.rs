//! Encoding frames on a pool of threads while writing them in order.
//!
//! Workers can encode frames independently, but completed frames must still be
//! written in their original order. A bounded job queue limits memory use and
//! pauses the producer when it gets too far ahead.
//!
//! The calling thread only copies each frame. Workers convert temperatures to
//! detector counts and encode them. Pixel and output buffers are reused to
//! avoid repeated allocations.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::thread::JoinHandle;

use crate::error::{Error, Result};
use crate::metadata::{FrameMetadata, GpsInfo, Timestamp};
use crate::thermal::RadiometricParameters;

use super::frame::FrameEncoder;
use super::{FramePixels, WriteFrame, WriteOptions};

/// Maximum queued or active frames per worker.
///
/// Two keeps workers busy without buffering too much video.
const QUEUE_PER_WORKER: usize = 2;

/// An owned copy of a frame's pixels.
enum Pixels {
    Raw(Vec<u16>),
    Celsius(Vec<f32>),
    Kelvin(Vec<f32>),
}

/// A frame waiting to be encoded by a worker.
struct Job {
    sequence: u32,
    pixels: Pixels,
    timestamp: Option<Timestamp>,
    gps: Option<GpsInfo>,
    radiometric: Option<RadiometricParameters>,
    /// Output buffer the worker can reuse.
    out: Vec<u8>,
}

/// The result returned by a worker.
struct Done {
    sequence: u32,
    /// Pixel buffer returned for reuse.
    pixels: Pixels,
    encoded: Result<Vec<u8>>,
}

/// Encodes frames on worker threads and writes completed frames in order.
pub(crate) struct Pipeline {
    width: usize,
    height: usize,
    /// Dropping this sender tells workers that no more jobs are coming.
    jobs: Option<SyncSender<Job>>,
    done: Receiver<Done>,
    workers: Vec<JoinHandle<()>>,
    /// Completed frames waiting for earlier sequence numbers.
    held: HashMap<u32, Vec<u8>>,
    /// Buffers available for reuse by future jobs.
    spare_counts: Vec<Vec<u16>>,
    spare_temperatures: Vec<Vec<f32>>,
    spare_frames: Vec<Vec<u8>>,
    /// Sequence number of the next frame to write.
    next_to_write: u32,
    /// Submitted frames that have not reached the sink.
    in_flight: usize,
}

impl Pipeline {
    /// Starts an encoder pool using the supplied recording settings.
    pub(crate) fn start(
        metadata: &FrameMetadata,
        options: &WriteOptions,
        workers: usize,
    ) -> Result<Self> {
        // Validate the configuration before starting workers so errors are
        // returned directly to the caller.
        let prototype = FrameEncoder::new(metadata, options)?;
        let (width, height) = prototype.dimensions();

        let (jobs, job_queue) = sync_channel::<Job>(workers * QUEUE_PER_WORKER);
        let (finished, done) = std::sync::mpsc::channel::<Done>();
        let job_queue = std::sync::Arc::new(std::sync::Mutex::new(job_queue));

        let mut encoders = vec![prototype];
        for _ in 1..workers {
            encoders.push(FrameEncoder::new(metadata, options)?);
        }

        let handles = encoders
            .into_iter()
            .map(|mut encoder| {
                let job_queue = job_queue.clone();
                let finished = finished.clone();
                std::thread::Builder::new()
                    .name("csq-encoder".into())
                    .spawn(move || {
                        loop {
                            // Release the shared queue lock before encoding.
                            let job = {
                                let queue = job_queue.lock().unwrap_or_else(|e| e.into_inner());
                                queue.recv()
                            };
                            let Ok(mut job) = job else { break };

                            let mut out = std::mem::take(&mut job.out);
                            let encoded = {
                                let frame = build_frame(&job);
                                encoder
                                    .encode_into(&frame, job.sequence, &mut out)
                                    .map(|()| out)
                            };

                            if finished
                                .send(Done {
                                    sequence: job.sequence,
                                    pixels: job.pixels,
                                    encoded,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        Ok(Self {
            width,
            height,
            jobs: Some(jobs),
            done,
            workers: handles,
            held: HashMap::new(),
            spare_counts: Vec::new(),
            spare_temperatures: Vec::new(),
            spare_frames: Vec::new(),
            next_to_write: 0,
            in_flight: 0,
        })
    }

    /// Returns the frame dimensions accepted by this pipeline.
    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Queues a frame and writes any completed frames that are next in order.
    ///
    /// Returns how many bytes reached the sink during the call.
    pub(crate) fn submit(
        &mut self,
        frame: &WriteFrame<'_>,
        sequence: u32,
        sink: &mut impl Write,
    ) -> Result<u64> {
        // Validate and copy here. Conversion and encoding stay on the worker.
        let expected = self.width * self.height;
        let pixels = match frame.pixels {
            FramePixels::Raw(raw) => {
                check_len(raw.len(), expected)?;
                let mut counts = self.spare_counts.pop().unwrap_or_default();
                counts.clear();
                counts.extend_from_slice(raw);
                Pixels::Raw(counts)
            }
            FramePixels::Celsius(celsius) => {
                check_len(celsius.len(), expected)?;
                Pixels::Celsius(self.copy_temperatures(celsius))
            }
            FramePixels::Kelvin(kelvin) => {
                check_len(kelvin.len(), expected)?;
                Pixels::Kelvin(self.copy_temperatures(kelvin))
            }
        };

        let job = Job {
            sequence,
            pixels,
            timestamp: frame.timestamp,
            gps: frame.gps.clone(),
            radiometric: frame.radiometric,
            out: self.spare_frames.pop().unwrap_or_default(),
        };

        // Send first: the queue is bounded, so this is where a producer running
        // faster than the encoders waits. Workers never block on the results
        // channel, so the queue always drains.
        let jobs = self.jobs.as_ref().expect("the pool outlives every submit");
        jobs.send(job).map_err(|_| worker_gone())?;
        self.in_flight += 1;

        let mut written = self.collect(sink, Blocking::No)?;
        // If the pipeline is full, wait for completed work and keep the sink
        // moving before accepting another frame.
        while self.in_flight >= self.workers.len() * QUEUE_PER_WORKER {
            written += self.collect(sink, Blocking::Yes)?;
        }
        Ok(written)
    }

    /// Copies temperatures into an available buffer, allocating only when no
    /// spare buffer exists.
    fn copy_temperatures(&mut self, temperatures: &[f32]) -> Vec<f32> {
        let mut copy = self.spare_temperatures.pop().unwrap_or_default();
        copy.clear();
        copy.extend_from_slice(temperatures);
        copy
    }

    /// Stops accepting jobs, waits for all workers, and writes every frame.
    pub(crate) fn drain(&mut self, sink: &mut impl Write) -> Result<u64> {
        // Closing the job channel lets each worker exit after its final job.
        self.jobs = None;

        let mut written = 0;
        while self.in_flight > 0 {
            written += self.collect(sink, Blocking::Yes)?;
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        Ok(written)
    }

    /// Collects worker results and writes the consecutive frames now available.
    fn collect(&mut self, sink: &mut impl Write, blocking: Blocking) -> Result<u64> {
        loop {
            let done = match blocking {
                Blocking::Yes => self.done.recv().map_err(|_| worker_gone())?,
                Blocking::No => match self.done.try_recv() {
                    Ok(done) => done,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(worker_gone()),
                },
            };

            self.in_flight -= 1;
            match done.pixels {
                Pixels::Raw(counts) => self.spare_counts.push(counts),
                Pixels::Celsius(temperatures) | Pixels::Kelvin(temperatures) => {
                    self.spare_temperatures.push(temperatures);
                }
            }
            self.held.insert(done.sequence, done.encoded?);

            // In blocking mode, wait for only one result per call.
            if matches!(blocking, Blocking::Yes) {
                break;
            }
        }

        let mut written = 0u64;
        while let Some(frame) = self.held.remove(&self.next_to_write) {
            sink.write_all(&frame)?;
            written += frame.len() as u64;
            self.next_to_write += 1;
            self.spare_frames.push(frame);
        }
        Ok(written)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

enum Blocking {
    Yes,
    No,
}

/// Creates the borrowed `WriteFrame` view expected by `FrameEncoder`.
fn build_frame(job: &Job) -> WriteFrame<'_> {
    let mut frame = match &job.pixels {
        Pixels::Raw(counts) => WriteFrame::raw(counts),
        Pixels::Celsius(celsius) => WriteFrame::celsius(celsius),
        Pixels::Kelvin(kelvin) => WriteFrame::kelvin(kelvin),
    };
    if let Some(timestamp) = job.timestamp {
        frame = frame.at(timestamp);
    }
    if let Some(gps) = &job.gps {
        frame = frame.with_gps(gps.clone());
    }
    if let Some(radiometric) = job.radiometric {
        // Use these values for conversion and store them in the encoded frame.
        frame = frame.with_radiometric(radiometric);
    }
    frame
}

fn check_len(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::FrameSizeMismatch { expected, actual })
    }
}

fn worker_gone() -> Error {
    Error::Unwritable {
        detail: "an encoder thread stopped before the recording was finished",
    }
}

/// Tests also ensure that owned job data is rebuilt as the correct borrowed
/// frame type.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_frame_carries_the_jobs_details() {
        let job = Job {
            sequence: 7,
            pixels: Pixels::Raw(vec![1, 2, 3, 4]),
            timestamp: Some(Timestamp {
                unix_seconds: 42,
                milliseconds: 5,
                utc_offset_minutes: 60,
            }),
            gps: None,
            radiometric: None,
            out: Vec::new(),
        };

        let frame = build_frame(&job);
        assert!(matches!(frame.pixels(), FramePixels::Raw(counts) if counts == [1, 2, 3, 4]));
        assert_eq!(frame.timestamp, job.timestamp);
    }

    #[test]
    fn temperatures_reach_the_worker_unconverted() {
        let job = Job {
            sequence: 0,
            pixels: Pixels::Kelvin(vec![293.15, 300.0]),
            timestamp: None,
            gps: None,
            radiometric: None,
            out: Vec::new(),
        };

        let frame = build_frame(&job);
        assert!(matches!(frame.pixels(), FramePixels::Kelvin(kelvin) if kelvin == [293.15, 300.0]));
    }
}
