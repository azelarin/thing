//! a background thread that decodes and encodes clipboard pngs.
//!
//! decodes and encodes each have one slot for a waiting job and one for a
//! finished result. the ui names the job it still wants: a newer request or a
//! cancellation drops the waiting job and any finished result at once, and a
//! result for a job that is no longer wanted is dropped when it finishes. at
//! most one image per kind is waiting, one is being worked on, and one is
//! finished, however long the ui takes to collect results.
//!
//! the thread is never joined, so a slow image cannot delay shutdown:
//! dropping the codec asks it to stop after the image in hand, and results
//! produced after that are discarded.

use std::{
    io,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};

use calloop::ping::Ping;
use thing_core::ImagePixmap;

pub(crate) struct Decoded {
    pub(crate) generation: u64,
    pub(crate) result: Result<ImagePixmap, String>,
}

pub(crate) struct Encoded {
    pub(crate) request: u64,
    pub(crate) result: Result<Vec<u8>, String>,
}

/// one kind of job: what waits, what the ui wants, and what finished.
struct Lane<I, O> {
    waiting: Option<(u64, I)>,
    wanted: Option<u64>,
    finished: Option<O>,
}

impl<I, O> Default for Lane<I, O> {
    fn default() -> Self {
        Self {
            waiting: None,
            wanted: None,
            finished: None,
        }
    }
}

impl<I, O> Lane<I, O> {
    fn request(&mut self, id: u64, input: I) {
        self.waiting = Some((id, input));
        self.wanted = Some(id);
        self.finished = None;
    }

    fn cancel(&mut self) {
        self.waiting = None;
        self.wanted = None;
        self.finished = None;
    }

    fn finish(&mut self, id: u64, output: O) -> bool {
        if self.wanted != Some(id) {
            return false;
        }
        self.wanted = None;
        self.finished = Some(output);
        true
    }
}

#[derive(Default)]
struct Shared {
    decode: Lane<Vec<u8>, Decoded>,
    encode: Lane<ImagePixmap, Encoded>,
    shutdown: bool,
}

type Channel = Arc<(Mutex<Shared>, Condvar)>;

pub(crate) struct Codec {
    channel: Channel,
}

fn lock(channel: &Channel) -> MutexGuard<'_, Shared> {
    channel
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Codec {
    pub(crate) fn start(ping: Ping) -> io::Result<Self> {
        let channel = Channel::default();
        let worker = channel.clone();
        thread::Builder::new()
            .name("thing-clipboard-images".into())
            .spawn(move || run(&worker, &ping))?;
        Ok(Self { channel })
    }

    /// decodes `bytes` for selection `generation`, replacing older decodes.
    pub(crate) fn decode(&self, generation: u64, bytes: Vec<u8>) {
        lock(&self.channel).decode.request(generation, bytes);
        self.channel.1.notify_one();
    }

    /// drops every decode, waiting, running, or finished.
    pub(crate) fn cancel_decode(&self) {
        lock(&self.channel).decode.cancel();
    }

    /// encodes `pixmap` for publish `request`, replacing older encodes.
    pub(crate) fn encode(&self, request: u64, pixmap: ImagePixmap) {
        lock(&self.channel).encode.request(request, pixmap);
        self.channel.1.notify_one();
    }

    /// drops every encode, waiting, running, or finished.
    pub(crate) fn cancel_encode(&self) {
        lock(&self.channel).encode.cancel();
    }

    pub(crate) fn take_decoded(&self) -> Option<Decoded> {
        lock(&self.channel).decode.finished.take()
    }

    pub(crate) fn take_encoded(&self) -> Option<Encoded> {
        lock(&self.channel).encode.finished.take()
    }

    /// whether a finished result waits; for tests that wait on results.
    #[cfg(test)]
    pub(crate) fn has_results(&self) -> bool {
        self.result_count() > 0
    }

    #[cfg(test)]
    pub(crate) fn result_count(&self) -> usize {
        let shared = lock(&self.channel);
        usize::from(shared.decode.finished.is_some())
            + usize::from(shared.encode.finished.is_some())
    }

    #[cfg(test)]
    pub(crate) fn pending_encode(&self) -> Option<u64> {
        lock(&self.channel)
            .encode
            .waiting
            .as_ref()
            .map(|(request, _)| *request)
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        let mut shared = lock(&self.channel);
        shared.shutdown = true;
        shared.decode.cancel();
        shared.encode.cancel();
        drop(shared);
        self.channel.1.notify_all();
    }
}

enum Job {
    Decode(u64, Vec<u8>),
    Encode(u64, ImagePixmap),
}

fn run(channel: &Channel, ping: &Ping) {
    loop {
        let job = {
            let mut shared = lock(channel);
            loop {
                if shared.shutdown {
                    return;
                }
                if let Some((generation, bytes)) = shared.decode.waiting.take() {
                    break Job::Decode(generation, bytes);
                }
                if let Some((request, pixmap)) = shared.encode.waiting.take() {
                    break Job::Encode(request, pixmap);
                }
                shared = channel
                    .1
                    .wait(shared)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let delivered = match job {
            Job::Decode(generation, bytes) => {
                let result = thing_ui::png::decode_png(&bytes)
                    .and_then(|image| ImagePixmap::new(image.width, image.height, image.rgba));
                drop(bytes);
                let mut shared = lock(channel);
                if shared.shutdown {
                    return;
                }
                shared
                    .decode
                    .finish(generation, Decoded { generation, result })
            }
            Job::Encode(request, pixmap) => {
                let result =
                    thing_ui::png::encode_png(pixmap.width(), pixmap.height(), pixmap.rgba());
                drop(pixmap);
                let mut shared = lock(channel);
                if shared.shutdown {
                    return;
                }
                shared.encode.finish(request, Encoded { request, result })
            }
        };
        if delivered {
            ping.ping();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn pixmap(side: u32) -> ImagePixmap {
        ImagePixmap::new(side, side, vec![7; (side * side * 4) as usize]).unwrap()
    }

    fn wait_for<T>(mut take: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(value) = take() {
                return value;
            }
            assert!(Instant::now() < deadline, "the codec never finished");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn lanes_hold_one_job_and_one_result_for_the_wanted_request() {
        let mut lane = Lane::<Vec<u8>, Vec<u8>>::default();
        lane.request(1, vec![1]);
        let (id, _) = lane.waiting.take().unwrap();
        assert!(lane.finish(id, vec![1; 64]));
        // a newer request releases the finished buffer at once.
        lane.request(2, vec![2]);
        assert!(lane.finished.is_none());
        lane.request(3, vec![3]);
        assert_eq!(lane.waiting.as_ref().unwrap().0, 3);
        // a job that was running for an older request finishes into nothing.
        assert!(!lane.finish(2, vec![2; 64]));
        assert!(lane.finished.is_none());
        lane.cancel();
        assert!(lane.waiting.is_none() && !lane.finish(3, vec![3]));
    }

    #[test]
    fn undrained_results_are_replaced_and_cancelled_work_is_dropped() {
        let (ping, _) = calloop::ping::make_ping().unwrap();
        let codec = Codec::start(ping).unwrap();

        // finished results wait in one slot while the ui is busy.
        codec.encode(1, pixmap(64));
        wait_for(|| (codec.result_count() == 1).then_some(()));
        codec.encode(2, pixmap(512));
        codec.encode(3, pixmap(2));
        let encoded = wait_for(|| codec.take_encoded());
        assert_eq!(encoded.request, 3);
        assert!(encoded.result.is_ok());

        // a cancelled decode, waiting or running, never produces a result,
        // while a later encode still completes behind it.
        let png = thing_ui::png::encode_png(2, 2, &[9; 16]).unwrap();
        codec.decode(4, png.clone());
        codec.cancel_decode();
        codec.encode(5, pixmap(2));
        assert_eq!(wait_for(|| codec.take_encoded()).request, 5);
        assert!(codec.take_decoded().is_none());

        codec.encode(6, pixmap(1024));
        codec.cancel_encode();
        codec.decode(7, png);
        let decoded = wait_for(|| codec.take_decoded());
        assert_eq!(decoded.generation, 7);
        assert_eq!(decoded.result.unwrap().rgba(), [9; 16]);
        assert!(codec.take_encoded().is_none());
    }
}
