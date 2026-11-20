//! Low-latency, glitch-safe output: an event-driven exclusive-mode stream on its own
//! thread, fed through a jitter buffer.
//!
//! Why: process loopback delivers audio in 10 ms packets with some timing jitter, while
//! the output device consumes continuously. Writing packets straight into a polled
//! exclusive buffer (the first design) ran it dry several times per second right after
//! start-up; an exclusive stream that runs dry replays stale buffer contents, which is
//! audible as crackle. Here the device pulls one period at a time from [`Jitter`], which
//! starts playing only once it holds `start` frames, plays silence (never stale data) if
//! it runs dry, and then waits for `start` frames again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::eRender;

use crate::wasapi::{self, ExclusiveMode, ExclusiveRender, CHANNELS, HNS_PER_MS};

/// Device (exclusive, timer-driven) buffer length: slack for late wake-ups of the
/// output thread. Only LOW_WATER_MS..FILL_MS of it is normally in use.
const DEVICE_BUFFER_MS: i64 = 30;
/// Top the device buffer up when it holds less than this...
const LOW_WATER_MS: u32 = 4;
/// ...to this level. Average device-side latency is about (LOW_WATER_MS + FILL_MS) / 2.
const FILL_MS: u32 = 6;
/// Below this the device is about to run dry: pad with silence rather than let it
/// replay stale buffer contents.
const CRITICAL_MS: u32 = 2;

/// Fade-in after (re)starting playback, so audio never starts with a step.
const FADE_MS: u32 = 3;

/// Interleaved-stereo FIFO between the capture thread (push) and the device (pull).
#[derive(Debug)]
pub struct Jitter {
    buf: VecDeque<f32>,
    /// Frames to accumulate before playback (re)starts.
    pub start: usize,
    /// If more than this many frames queue up (clock drift), drop down to `start`.
    pub max: usize,
    primed: bool,
    fade_len: usize,
    fade_pos: usize,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub silent_frames: u64,
    pub pulled_frames: u64,
}

impl Jitter {
    pub fn new(start: usize, max: usize, rate: u32) -> Jitter {
        Jitter {
            buf: VecDeque::new(),
            start,
            max: max.max(start + 1),
            primed: false,
            fade_len: (rate * FADE_MS / 1000) as usize,
            fade_pos: 0,
            underruns: 0,
            dropped_frames: 0,
            silent_frames: 0,
            pulled_frames: 0,
        }
    }

    pub fn frames(&self) -> usize {
        self.buf.len() / CHANNELS
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        self.buf.extend(interleaved);
        let n = self.frames();
        if n > self.max {
            let drop = n - self.start;
            self.buf.drain(..drop * CHANNELS);
            self.dropped_frames += drop as u64;
        }
    }

    /// Starts playback. Starts exactly at the start level: whatever arrived beyond it
    /// would only add latency (packets are 10 ms, so it could be up to 10 ms more).
    fn prime(&mut self) {
        self.primed = true;
        self.fade_pos = 0;
        let extra = self.frames().saturating_sub(self.start);
        self.buf.drain(..extra * CHANNELS);
    }

    /// Fills `out` (interleaved stereo) with the next frames, or silence.
    pub fn pull(&mut self, out: &mut [f32]) {
        self.pull_upto(out, true);
    }

    /// Like [`pull`](Self::pull), but with `must_fill == false` it only hands out what is
    /// queued (possibly fewer frames than `out` holds) instead of padding with silence.
    /// Returns the number of frames written to `out`.
    pub fn pull_upto(&mut self, out: &mut [f32], must_fill: bool) -> usize {
        let want = out.len() / CHANNELS;
        if !must_fill {
            if !self.primed {
                if self.frames() < self.start {
                    return 0;
                }
                self.prime();
            }
            let n = want.min(self.frames());
            self.pull_upto(&mut out[..n * CHANNELS], true);
            return n;
        }
        self.pulled_frames += want as u64;
        if !self.primed {
            if self.frames() >= self.start {
                self.prime();
            } else {
                out.fill(0.0);
                self.silent_frames += want as u64;
                return want;
            }
        }
        let take = want.min(self.frames());
        for f in 0..take {
            let g = if self.fade_pos < self.fade_len {
                self.fade_pos += 1;
                self.fade_pos as f32 / self.fade_len as f32
            } else {
                1.0
            };
            for c in 0..CHANNELS {
                out[f * CHANNELS + c] = self.buf.pop_front().unwrap_or(0.0) * g;
            }
        }
        if take < want {
            out[take * CHANNELS..].fill(0.0);
            self.silent_frames += (want - take) as u64;
            self.underruns += 1;
            self.primed = false;
        }
        want
    }
}

/// Counters for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputStats {
    pub underruns: u64,
    pub dropped_frames: u64,
    pub silent_frames: u64,
    pub pulled_frames: u64,
    pub queued_frames: usize,
}

/// A running exclusive output stream.
pub struct Output {
    jitter: Arc<Mutex<Jitter>>,
    error: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub rate: u32,
    pub description: String,
}

impl Output {
    /// Opens `name` exclusively and starts it.
    /// `buffer_ms` is the jitter-buffer start level: higher = more latency, more
    /// tolerance for timing jitter. It must exceed the 10 ms capture packet size.
    pub fn open(name: &str, buffer_ms: u32) -> Result<Output, String> {
        let name = name.to_string();
        let (init_tx, init_rx) = mpsc::channel::<Result<(u32, String, Arc<Mutex<Jitter>>), String>>();
        let error = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (error2, stop2) = (error.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let r = (|| -> Result<ExclusiveRender, String> {
                wasapi::init_com().map_err(|e| e.to_string())?;
                wasapi::boost_thread_priority();
                let dev = wasapi::find_device(&name, eRender)?;
                // Exclusive mode is essential, see ExclusiveRender: a shared-mode stream into
                // the cable would be captured by our own process loopback and recirculate.
                let mut out = ExclusiveRender::open_mode(&dev, ExclusiveMode::Timer { buffer_ms: DEVICE_BUFFER_MS })
                    .map_err(|e| format!("open output {name:?}: {e}"))?;
                let silence = vec![0.0f32; (out.rate * FILL_MS / 1000) as usize * CHANNELS];
                out.write(&silence).map_err(|e| format!("output write: {e}"))?;
                out.start().map_err(|e| format!("start output: {e}"))?;
                Ok(out)
            })();
            let mut out = match r {
                Ok(v) => v,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            let rate = out.rate;
            let start = (rate * buffer_ms / 1000) as usize;
            let jitter = Arc::new(Mutex::new(Jitter::new(start, start + (rate * 20 / 1000) as usize, rate)));
            let low = (rate * LOW_WATER_MS / 1000) as usize;
            let fill = (rate * FILL_MS / 1000) as usize;
            let critical = (rate * CRITICAL_MS / 1000) as usize;
            let desc = format!(
                "{name} ({rate} Hz {:?}, device period {:.2} ms, buffer {buffer_ms} ms)",
                out.kind,
                out.period_hns as f64 / HNS_PER_MS as f64
            );
            let _ = init_tx.send(Ok((rate, desc, jitter.clone())));
            let _timer = wasapi::HighResTimer::new();
            let mut chunk = Vec::new();
            while !stop2.load(Ordering::Relaxed) {
                // Keep the device buffer topped up to FILL_MS whenever it drops below
                // LOW_WATER_MS. The device buffer itself is DEVICE_BUFFER_MS long, so a late
                // wake-up of this thread eats into that slack instead of glitching.
                let pad = match out.padding() {
                    Ok(p) => p as usize,
                    Err(e) => {
                        *error2.lock().unwrap() = Some(format!("output {name:?} failed: {e}"));
                        return;
                    }
                };
                if pad < low {
                    chunk.resize((fill - pad) * CHANNELS, 0.0);
                    // While the device still holds more than CRITICAL_MS, hand over only
                    // real audio (a late capture packet is about to arrive); pad with
                    // silence only when the device is about to run dry.
                    let n = jitter.lock().unwrap().pull_upto(&mut chunk, pad < critical);
                    if let Err(e) = out.write(&chunk[..n * CHANNELS]) {
                        *error2.lock().unwrap() = Some(format!("output {name:?} failed: {e}"));
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok((rate, description, jitter))) => {
                Ok(Output { jitter, error, stop, thread: Some(thread), rate, description })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("output did not start within 5 s".into())
            }
        }
    }

    pub fn push(&self, interleaved: &[f32]) {
        self.jitter.lock().unwrap().push(interleaved);
    }

    /// Set once the output thread has failed (device removed, etc.).
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn stats(&self) -> OutputStats {
        let j = self.jitter.lock().unwrap();
        OutputStats {
            underruns: j.underruns,
            dropped_frames: j.dropped_frames,
            silent_frames: j.silent_frames,
            pulled_frames: j.pulled_frames,
            queued_frames: j.frames(),
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize, v: f32) -> Vec<f32> {
        vec![v; n * CHANNELS]
    }

    #[test]
    fn waits_for_start_level_then_plays_with_fade_in() {
        let mut j = Jitter::new(100, 1000, 48_000); // fade 144 frames
        let mut out = frames(48, 9.0);
        j.push(&frames(60, 1.0));
        j.pull(&mut out);
        assert!(out.iter().all(|&v| v == 0.0), "must not start below the start level");
        j.push(&frames(60, 1.0)); // 120 >= 100
        j.pull(&mut out);
        assert!(out[0] > 0.0 && out[0] < 0.05, "first sample is faded in: {}", out[0]);
        assert!(out.windows(2).all(|w| w[1] >= w[0]), "fade rises monotonically");
        assert_eq!(j.frames(), 52, "priming drops what exceeds the start level (120 -> 100), then 48 played");
    }

    #[test]
    fn underrun_gives_silence_not_stale_data_and_reprimes() {
        let mut j = Jitter::new(10, 1000, 48_000);
        j.push(&frames(20, 1.0)); // priming keeps the newest 10
        let mut out = frames(48, 9.0);
        j.pull(&mut out);
        assert!(out[..10 * CHANNELS].iter().all(|&v| v > 0.0));
        assert!(out[10 * CHANNELS..].iter().all(|&v| v == 0.0), "silence after running dry, never stale data");
        assert_eq!(j.underruns, 1);
        j.push(&frames(5, 1.0)); // below start level again: stays silent
        j.pull(&mut out);
        assert!(out.iter().all(|&v| v == 0.0));
        assert_eq!(j.frames(), 5);
    }

    #[test]
    fn pull_upto_without_must_fill_never_inserts_silence() {
        let mut j = Jitter::new(10, 1000, 48_000);
        let mut out = frames(48, 9.0);
        assert_eq!(j.pull_upto(&mut out, false), 0, "not primed yet");
        j.push(&frames(30, 1.0)); // primes at 10
        assert_eq!(j.pull_upto(&mut out, false), 10);
        assert_eq!(j.underruns, 0);
        assert_eq!(j.pull_upto(&mut out, false), 0);
        assert_eq!(j.underruns, 0, "an empty queue is not an underrun while the device has slack");
    }

    #[test]
    fn overflow_drops_oldest_down_to_start_level() {
        let mut j = Jitter::new(100, 200, 48_000);
        j.push(&frames(150, 1.0));
        j.push(&frames(100, 2.0)); // 250 > 200 -> keep newest 100
        assert_eq!(j.frames(), 100);
        assert_eq!(j.dropped_frames, 150);
        let mut out = frames(100, 0.0);
        j.pull(&mut out);
        // (values are scaled by the start-up fade-in; the 1.0 batch would stay below 1)
        assert!(out[out.len() - 1] > 1.0, "newest data kept: {}", out[out.len() - 1]);
    }
}
