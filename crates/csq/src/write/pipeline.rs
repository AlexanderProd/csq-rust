//! Encoding frames on a pool of threads while writing them in order.
//!
//! Frames are independent, so encoding parallelises perfectly — the only thing
//! that has to be sequenced is the order they reach the sink in. Jobs go out on
//! a bounded channel, which is what applies backpressure: a producer running
//! ahead of the encoders blocks on submitting rather than growing a queue until
//! memory runs out.
//!
//! The calling thread converts temperatures to counts and the workers do the
//! entropy coding, which is the expensive part by an order of magnitude. Both
//! the count buffers and the encoded frames are recycled, so a steady feed
//! settles into a fixed set of allocations.

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::thread::JoinHandle;

use crate::error::{Error, Result};
use crate::metadata::{FrameMetadata, GpsInfo, Timestamp};
use crate::thermal::{RadiometricParameters, RawConversion};

use super::frame::FrameEncoder;
use super::{FramePixels, WriteFrame, WriteOptions};

/// In-flight frames allowed per worker.
///
/// Two is enough to keep a worker fed while the writer is busy with the sink,
/// and small enough that the queue holds a fraction of a second of video.
const QUEUE_PER_WORKER: usize = 2;

/// One frame handed to a worker.
struct Job {
    sequence: u32,
    counts: Vec<u16>,
    timestamp: Option<Timestamp>,
    gps: Option<GpsInfo>,
    radiometric: Option<RadiometricParameters>,
    /// A recycled buffer for the worker to encode into.
    out: Vec<u8>,
}

/// One frame back from a worker.
struct Done {
    sequence: u32,
    counts: Vec<u16>,
    encoded: Result<Vec<u8>>,
}

/// A pool of encoder threads plus the reordering the sink needs.
pub(crate) struct Pipeline {
    width: usize,
    height: usize,
    conversion: RawConversion,
    /// Dropped to tell the workers to stop.
    jobs: Option<SyncSender<Job>>,
    done: Receiver<Done>,
    workers: Vec<JoinHandle<()>>,
    /// Frames that finished before the ones ahead of them.
    held: HashMap<u32, Vec<u8>>,
    /// Recycled buffers, handed back out with the next job.
    spare_counts: Vec<Vec<u16>>,
    spare_frames: Vec<Vec<u8>>,
    /// The next sequence number the sink expects.
    next_to_write: u32,
    /// Frames queued but not yet written.
    in_flight: usize,
}

impl Pipeline {
    pub(crate) fn start(
        metadata: &FrameMetadata,
        options: &WriteOptions,
        workers: usize,
    ) -> Result<Self> {
        // Built once here so a bad configuration is reported to the caller
        // rather than on a worker thread where nobody is listening.
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
                            // The lock is held only long enough to take a job,
                            // never across encoding.
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
                                    counts: std::mem::take(&mut job.counts),
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
            conversion: metadata.radiometric.raw_conversion(),
            jobs: Some(jobs),
            done,
            workers: handles,
            held: HashMap::new(),
            spare_counts: Vec::new(),
            spare_frames: Vec::new(),
            next_to_write: 0,
            in_flight: 0,
        })
    }

    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Queues a frame, writing out whatever has become writable.
    ///
    /// Returns how many bytes reached the sink during the call.
    pub(crate) fn submit(
        &mut self,
        frame: &WriteFrame<'_>,
        sequence: u32,
        sink: &mut impl Write,
    ) -> Result<u64> {
        // Converting here rather than on a worker keeps one buffer type in the
        // queue and halves what crosses it; it is a twentieth of the cost of
        // the entropy coding either way.
        let expected = self.width * self.height;
        let conversion = frame
            .radiometric
            .as_ref()
            .map_or(self.conversion, RawConversion::new);
        let mut counts = self.spare_counts.pop().unwrap_or_default();
        match frame.pixels {
            FramePixels::Raw(raw) => {
                check_len(raw.len(), expected)?;
                counts.clear();
                counts.extend_from_slice(raw);
            }
            FramePixels::Celsius(celsius) => {
                check_len(celsius.len(), expected)?;
                conversion.convert_into(celsius, &mut counts);
            }
            FramePixels::Kelvin(kelvin) => {
                check_len(kelvin.len(), expected)?;
                counts.clear();
                counts.reserve(kelvin.len());
                counts.extend(
                    kelvin
                        .iter()
                        .map(|&k| conversion.raw(k - crate::metadata::KELVIN_OFFSET)),
                );
            }
        }

        let job = Job {
            sequence,
            counts,
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
        // A queue that has filled up means the producer is ahead of the pool;
        // waiting here rather than in the next send keeps the sink busy.
        while self.in_flight >= self.workers.len() * QUEUE_PER_WORKER {
            written += self.collect(sink, Blocking::Yes)?;
        }
        Ok(written)
    }

    /// Waits for every queued frame and writes it out.
    pub(crate) fn drain(&mut self, sink: &mut impl Write) -> Result<u64> {
        // Dropping the sender is what tells the workers there is no more work.
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

    /// Takes finished frames and writes those the sink is ready for.
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
            self.spare_counts.push(done.counts);
            self.held.insert(done.sequence, done.encoded?);

            // One blocking wait is one frame; anything else already waiting is
            // picked up by the loop below on the next pass.
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

/// Rebuilds the borrowed frame a worker encodes from its owned job.
fn build_frame(job: &Job) -> WriteFrame<'_> {
    let mut frame = WriteFrame::raw(&job.counts);
    if let Some(timestamp) = job.timestamp {
        frame = frame.at(timestamp);
    }
    if let Some(gps) = &job.gps {
        frame = frame.with_gps(gps.clone());
    }
    if let Some(radiometric) = job.radiometric {
        // The counts are already converted; this only records the parameters a
        // reader needs to convert them back.
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

/// Recycled buffers hold the frame currently in flight, so a spare list that
/// grew without bound would be a leak; it cannot, because a buffer only goes
/// back on it when a frame leaves the queue.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_frame_carries_the_jobs_details() {
        let job = Job {
            sequence: 7,
            counts: vec![1, 2, 3, 4],
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
        assert!(matches!(frame.pixels(), FramePixels::Raw(counts) if counts == job.counts));
        assert_eq!(frame.timestamp, job.timestamp);
    }
}
