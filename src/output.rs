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
const LOW_WATER_MS: u32 = 5;
/// ...to this level. Average device-side latency is about (LOW_WATER_MS + FILL_MS) / 2.
const FILL_MS: u32 = 7;
/// Below this the device is about to run dry: pad with silence rather than let it
/// replay stale buffer contents.
const CRITICAL_MS: u32 = 3;

/// Fade-in after (re)starting playback, so audio never starts with a step.
const FADE_MS: u32 = 3;

/// Amplitude of the optional silence dither: 1.5 LSB of 16-bit audio. Float -> I16
/// conversion truncates toward zero, so every sample becomes +-1 LSB (about -90 dBFS):
/// never an exact digital zero, far below audibility.
pub const DITHER_AMPLITUDE: f32 = 1.5 / 32767.0;

/// Adds +-[`DITHER_AMPLITUDE`] (random sign per frame) to every sample of an interleaved
/// buffer. All channels of a frame get the same sign, so a receiver that downmixes to
/// mono by averaging doesn't get exact zeros either.
pub fn add_dither(buf: &mut [f32], rng: &mut u32) {
    for frame in buf.chunks_mut(CHANNELS) {
        // xorshift32
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        let d = if *rng & 1 == 0 { DITHER_AMPLITUDE } else { -DITHER_AMPLITUDE };
        frame.iter_mut().for_each(|v| *v += d);
    }
}

/// Largest playback-rate correction of the drift control (0.5 %, about 9 cents; only
/// reached for large deficits, i.e. right after a big audio loss: recovers 10 ms of
/// buffer in about 2-3 s).
const MAX_CORRECTION: f64 = 0.005;
/// Correction per ms of level error (1 ms off target -> 0.05 % faster/slower, so the
/// usual small deviations are corrected very gently).
const CORRECTION_PER_MS: f64 = 0.0005;
/// Within this distance from the target level no correction is applied (and playback
/// returns to exact 1:1 sample pass-through).
const DEADBAND_MS: f64 = 0.5;
/// Correction resumes only beyond this level error (hysteresis against toggling).
const UNLOCK_MS: f64 = 1.0;
/// Integral gain (per ms of error and second), and the error range it integrates in.
const CORRECTION_PER_MS_S: f64 = 0.0001;
const INTEGRATE_WITHIN_MS: f64 = 1.5;
/// Return to 1:1 only if the clocks don't drift apart by more than this.
const LOCK_MAX_DRIFT: f64 = 0.0002;
/// Time constant of the level average the drift control looks at.
const LEVEL_AVERAGE_S: f64 = 0.3;

/// Interleaved-stereo FIFO between the capture thread (push) and the device (pull).
///
/// Process loopback delivers 10 ms packets, and loses a few ms of audio whenever any
/// program on the system opens or closes a stream; the two device clocks also drift
/// apart slowly. A plain FIFO cannot recover from either: each loss permanently lowers
/// its fill level until it runs dry, which costs an audible gap. So the average fill
/// level is steered back to its target by playing slightly (at most
/// [`MAX_CORRECTION`]) slower or faster, with cubic interpolation. Close to the target
/// the correction stops and samples pass through unchanged.
#[derive(Debug)]
pub struct Jitter {
    buf: VecDeque<f32>,
    /// Frames to accumulate before playback (re)starts; the level right after a packet
    /// arrives is kept around this.
    pub start: usize,
    /// If more than this many frames queue up, drop down to `start` (last resort).
    pub max: usize,
    primed: bool,
    fade_len: usize,
    fade_pos: usize,
    rate: f64,
    /// The frame before the queue front (interpolation history).
    prev: [f32; CHANNELS],
    /// Read position between the queue front (0.0) and the next frame (1.0).
    phase: f64,
    /// Playback-rate factor: >1 consumes faster (queue too full), <1 slower.
    speed: f64,
    /// 1:1 pass-through (speed exactly 1, phase exactly 0).
    locked: bool,
    /// Integral part of the rate correction (follows clock drift).
    integral: f64,
    /// Average fill level (frames), and the average packet size (frames).
    avg_level: f64,
    avg_packet: f64,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub silent_frames: u64,
    pub pulled_frames: u64,
    /// Output frames produced while the rate correction was active.
    pub corrected_frames: u64,
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
            rate: rate as f64,
            prev: [0.0; CHANNELS],
            phase: 0.0,
            speed: 1.0,
            locked: true,
            integral: 0.0,
            avg_level: 0.0,
            avg_packet: rate as f64 / 100.0,
            underruns: 0,
            dropped_frames: 0,
            silent_frames: 0,
            pulled_frames: 0,
            corrected_frames: 0,
        }
    }

    pub fn frames(&self) -> usize {
        self.buf.len() / CHANNELS
    }

    /// Current playback-rate factor (1.0 = no correction).
    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        let n_in = interleaved.len() / CHANNELS;
        if n_in > 0 {
            self.avg_packet += (n_in as f64 - self.avg_packet) * 0.1;
        }
        self.buf.extend(interleaved);
        let n = self.frames();
        if n > self.max {
            let drop = n - self.start;
            self.buf.drain(..drop * CHANNELS);
            self.dropped_frames += drop as u64;
        }
    }

    /// A hole in the capture timeline (audio Windows lost): fades out the last
    /// `fade_out` queued frames (not yet played), then queues `frames` of silence in
    /// place of the lost audio, so the level (and thus the timeline) is kept.
    /// Returns the frames that were still queued (could be faded).
    pub fn push_hole(&mut self, frames: usize, fade_out: usize) -> usize {
        let q = self.frames();
        let n = fade_out.min(q);
        let (a, b) = self.buf.as_mut_slices();
        let mut tail: Vec<f32> = a.iter().chain(b.iter()).skip((q - n) * CHANNELS).copied().collect();
        crate::timeline::fade_out_tail(&mut tail, n);
        let start = (q - n) * CHANNELS;
        for (i, v) in tail.into_iter().enumerate() {
            self.buf[start + i] = v;
        }
        self.buf.extend(std::iter::repeat(0.0).take(frames * CHANNELS));
        let n = self.frames();
        if n > self.max {
            let drop = n - self.start;
            self.buf.drain(..drop * CHANNELS);
            self.dropped_frames += drop as u64;
        }
        q
    }

    /// Target of the average level: the level oscillates between `start` (packet just
    /// arrived) and `start - packet`, so its average is about half a packet lower.
    fn target_level(&self) -> f64 {
        (self.start as f64 - self.avg_packet / 2.0).max(0.0)
    }

    /// Starts playback. Starts exactly at the start level: whatever arrived beyond it
    /// would only add latency (packets are 10 ms, so it could be up to 10 ms more).
    fn prime(&mut self) {
        self.primed = true;
        self.fade_pos = 0;
        let extra = self.frames().saturating_sub(self.start);
        self.buf.drain(..extra * CHANNELS);
        self.prev = [0.0; CHANNELS];
        self.phase = 0.0;
        self.speed = 1.0;
        self.locked = true;
        self.integral = 0.0;
        self.avg_level = self.target_level();
    }

    /// Updates the level average and the playback rate (once per pull).
    fn steer(&mut self, frames_out: usize) {
        let dt = frames_out as f64 / self.rate;
        let a = (dt / LEVEL_AVERAGE_S).min(1.0);
        self.avg_level += (self.frames() as f64 - self.avg_level) * a;
        let err_ms = (self.avg_level - self.target_level()) * 1000.0 / self.rate;
        if self.locked {
            if err_ms.abs() <= UNLOCK_MS {
                return;
            }
            self.locked = false;
        }
        // Integral part: follows a constant clock drift. It only integrates near the
        // target, so a one-off loss (a large error) doesn't wind it up.
        if err_ms.abs() < INTEGRATE_WITHIN_MS {
            self.integral = (self.integral + err_ms * CORRECTION_PER_MS_S * dt).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        }
        if err_ms.abs() < DEADBAND_MS && self.integral.abs() < LOCK_MAX_DRIFT {
            // no lasting drift: keep the last direction (slowly) until the read position
            // reaches an exact sample, then lock (see `next_frame`)
            self.speed = 1.0 + (self.speed - 1.0).signum() * 0.0005;
        } else {
            self.speed = 1.0 + (err_ms * CORRECTION_PER_MS + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        }
    }

    fn frame(&self, i: usize, c: usize) -> Option<f32> {
        self.buf.get(i * CHANNELS + c).copied()
    }

    fn pop_frame(&mut self) {
        for c in 0..CHANNELS {
            self.prev[c] = self.buf.pop_front().unwrap_or(0.0);
        }
    }

    /// Frames needed in the queue to produce the next output frame.
    fn needed(&self) -> usize {
        if self.locked {
            1
        } else {
            3
        }
    }

    /// Writes the next output frame into `out` (len CHANNELS) and advances. The caller
    /// checks `needed()` first.
    fn next_frame(&mut self, out: &mut [f32]) {
        if self.locked {
            for c in 0..CHANNELS {
                out[c] = self.buf.pop_front().unwrap_or(0.0);
                self.prev[c] = out[c];
            }
            return;
        }
        // Catmull-Rom between prev, f0, f1, f2 at `phase` (between f0 and f1)
        let t = self.phase as f32;
        for c in 0..CHANNELS {
            let p0 = self.prev[c];
            let p1 = self.frame(0, c).unwrap_or(0.0);
            let p2 = self.frame(1, c).unwrap_or(p1);
            let p3 = self.frame(2, c).unwrap_or(p2);
            out[c] = p1 + 0.5 * t * (p2 - p0 + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)));
        }
        self.corrected_frames += 1;
        self.phase += self.speed;
        while self.phase >= 1.0 {
            self.pop_frame();
            self.phase -= 1.0;
        }
        // In the deadband the phase creeps by 0.0005 per frame: once it is within 0.001
        // of an exact sample position, snap to it and return to 1:1 pass-through.
        if (self.speed - 1.0).abs() <= 0.0005 + 1e-12 && (self.phase < 1e-3 || self.phase > 1.0 - 1e-3) {
            if self.phase > 0.5 {
                self.pop_frame();
            }
            self.phase = 0.0;
            self.speed = 1.0;
            self.locked = true;
        }
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
        if !self.primed {
            if self.frames() < self.start {
                if !must_fill {
                    return 0;
                }
                self.pulled_frames += want as u64;
                out.fill(0.0);
                self.silent_frames += want as u64;
                return want;
            }
            self.prime();
        }
        // Without must_fill, the last fade_len frames stay queued: if the next packet
        // reports lost audio, they can still be faded out (see push_hole); if the
        // device is about to run dry (must_fill), they are played, faded out.
        let reserve = if must_fill { 0 } else { self.fade_len };
        let mut n = 0;
        while n < want && self.frames() >= self.needed() + reserve {
            let g = if self.fade_pos < self.fade_len {
                self.fade_pos += 1;
                self.fade_pos as f32 / self.fade_len as f32
            } else {
                1.0
            };
            self.next_frame(&mut out[n * CHANNELS..(n + 1) * CHANNELS]);
            if g < 1.0 {
                out[n * CHANNELS..(n + 1) * CHANNELS].iter_mut().for_each(|v| *v *= g);
            }
            n += 1;
        }
        // the last frames of the queue while correcting: play them out unchanged
        while n < want && self.frames() > 0 && !self.locked && must_fill {
            for c in 0..CHANNELS {
                out[n * CHANNELS + c] = self.buf.pop_front().unwrap_or(0.0);
                self.prev[c] = out[n * CHANNELS + c];
            }
            n += 1;
        }
        if must_fill && n < want {
            // running dry: end softly instead of cutting off
            crate::timeline::fade_out_tail(&mut out[..n * CHANNELS], self.fade_len);
        }
        if n > 0 {
            self.steer(n);
        }
        if !must_fill {
            self.pulled_frames += n as u64;
            return n;
        }
        self.pulled_frames += want as u64;
        if n < want {
            out[n * CHANNELS..].fill(0.0);
            self.silent_frames += (want - n) as u64;
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
    pub speed: f64,
    pub avg_level: f64,
    pub target_level: f64,
    pub corrected_frames: u64,
}

/// A running exclusive output stream.
pub struct Output {
    jitter: Arc<Mutex<Jitter>>,
    error: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    dither: Arc<AtomicBool>,
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
        let dither = Arc::new(AtomicBool::new(false));
        let (error2, stop2, dither2) = (error.clone(), stop.clone(), dither.clone());
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
            let mut rng: u32 = 0x9E37_79B9;
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
                    if dither2.load(Ordering::Relaxed) {
                        add_dither(&mut chunk[..n * CHANNELS], &mut rng);
                    }
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
                Ok(Output { jitter, error, stop, dither, thread: Some(thread), rate, description })
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

    /// See [`Jitter::push_hole`].
    pub fn push_hole(&self, frames: usize, fade_out: usize) -> usize {
        let mut j = self.jitter.lock().unwrap();
        let q = j.push_hole(frames, fade_out);
        if !j.primed {
            return usize::MAX; // after an underrun
        }
        q
    }

    /// Silence dither: when on, a faint +-1 LSB noise (about -90 dBFS) is added to
    /// everything written, so the device never carries exact digital zeros. Some
    /// consumers (e.g. RustDesk's audio encoder) stop after a run of all-zero frames and
    /// restart when sound returns; this keeps them running. Takes effect immediately.
    pub fn set_dither(&self, on: bool) {
        self.dither.store(on, Ordering::Relaxed);
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
            speed: j.speed,
            avg_level: j.avg_level,
            target_level: j.target_level(),
            corrected_frames: j.corrected_frames,
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
    fn pull_upto_without_must_fill_never_inserts_silence_and_keeps_a_fade_reserve() {
        let mut j = Jitter::new(400, 10_000, 48_000);
        let mut out = frames(1000, 9.0);
        assert_eq!(j.pull_upto(&mut out, false), 0, "not primed yet");
        j.push(&frames(500, 1.0)); // primes at 400
        // the last 3 ms (144 frames) stay queued for a possible fade-out
        assert_eq!(j.pull_upto(&mut out, false), 400 - 144);
        assert_eq!(j.underruns, 0);
        assert_eq!(j.pull_upto(&mut out, false), 0);
        assert_eq!(j.underruns, 0, "an empty queue is not an underrun while the device has slack");
        // the device is about to run dry: the reserve is played, faded out, then silence
        let mut out = frames(300, 9.0);
        assert_eq!(j.pull_upto(&mut out, true), 300);
        assert_eq!(j.underruns, 1);
        let l: Vec<f32> = out.chunks(CHANNELS).map(|f| f[0]).collect();
        assert!(l[0] > 0.99 && l[143] < 0.01 && l[1..144].windows(2).all(|w| w[1] <= w[0]), "fade-out over the reserve");
        assert!(l[144..].iter().all(|v| *v == 0.0));
    }

    /// Simulates capture (10 ms packets of a 1 kHz sine, with optional losses) and the
    /// device (pulls every 1 ms, `drift` = how much faster its clock runs). Returns the
    /// output (left channel), the jitter buffer, and the level just before each push.
    fn simulate(start_ms: usize, secs: usize, drift: f64, lose_at_ms: &[(usize, usize)]) -> (Vec<f32>, Jitter, Vec<usize>) {
        let rate = 48_000u32;
        let mut j = Jitter::new(rate as usize * start_ms / 1000, rate as usize * (start_ms + 20) / 1000, rate);
        let w = 2.0 * std::f64::consts::PI * 1000.0 / rate as f64;
        let mut src_n: u64 = 0; // source sample index (losses skip ahead)
        let mut out_l = Vec::new();
        let mut levels = Vec::new();
        let mut owed = 0.0f64;
        let mut next_push = 0usize;
        let mut pending: Vec<(usize, usize)> = lose_at_ms.to_vec();
        for ms in 0..secs * 1000 {
            if ms == next_push {
                // As measured (diag_packets_on_stream_open): the engine stalls for `lost`
                // ms; the next packet arrives that much later, its audio continues
                // `lost` ms further on, and all later packets keep the shifted schedule.
                if let Some(i) = pending.iter().position(|(at, _)| *at <= ms) {
                    let (_, lost) = pending.remove(i);
                    src_n += (lost * 48) as u64;
                    next_push += lost;
                } else {
                    next_push += 10;
                    levels.push(j.frames());
                    let pkt: Vec<f32> = (0..480u64)
                        .flat_map(|i| {
                            let v = (0.1 * (w * (src_n + i) as f64).sin()) as f32;
                            [v, v]
                        })
                        .collect();
                    src_n += 480;
                    j.push(&pkt);
                }
            }
            owed += 48.0 * (1.0 + drift);
            let n = owed as usize;
            owed -= n as f64;
            let mut out = vec![0.0f32; n * CHANNELS];
            j.pull(&mut out);
            out_l.extend(out.chunks(CHANNELS).map(|f| f[0]));
        }
        (out_l, j, levels)
    }

    /// Sine-predictor residual peaks above 0.005 (a dropped/inserted gap is ~0.1).
    fn glitches(x: &[f32]) -> Vec<usize> {
        let c = 2.0 * (2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).cos();
        (2..x.len()).filter(|&n| (x[n] - c * x[n - 1] + x[n - 2]).abs() > 0.005).collect()
    }

    /// The real chain around lost audio: capture packets every 10 ms through
    /// [`Timeline`](crate::timeline::Timeline), the jitter buffer, and a device modelled
    /// like the output thread (topped up to FILL_MS below LOW_WATER_MS, must_fill below
    /// CRITICAL_MS). At each event in `events` (ms), the packet of that slot is lost
    /// (arrives as the next one, timestamp 10 ms further) and the 31 ms after it are exact
    /// zeros, as measured on stream open/close. Returns what the device played.
    fn simulate_device(start_ms: usize, ms_total: usize, events: &[usize], arrive: usize) -> (Vec<f32>, Jitter) {
        let rate = 48_000u32;
        let mut j = Jitter::new(rate as usize * start_ms / 1000, rate as usize * (start_ms + 20) / 1000, rate);
        let mut tl = crate::timeline::Timeline::new(rate);
        let w = 2.0 * std::f64::consts::PI * 1000.0 / rate as f64;
        let (low, fill, critical) = (LOW_WATER_MS as usize * 48, FILL_MS as usize * 48, CRITICAL_MS as usize * 48);
        let mut dev_level = fill; // frames queued in the device
        let mut played = Vec::new();
        let mut zeros_until = 0u64;
        for ms in 0..ms_total {
            if ms % 10 == arrive {
                let slot = ms / 10;
                let src = slot as u64 * 480;
                if !events.iter().any(|&e| e + arrive == ms) {
                    if events.iter().any(|&e| e + 10 + arrive == ms) {
                        zeros_until = src + 31 * 48;
                    }
                    let pkt: Vec<f32> = (src..src + 480).flat_map(|i| {
                        let v = if i < zeros_until { 0.0 } else { (0.1 * (w * i as f64).sin()) as f32 };
                        [v, v]
                    }).collect();
                    let qpc = src * 10_000_000 / 48_000 + 77_777;
                    let (hole, s) = tl.packet(&pkt, qpc);
                    if hole.frames > 0 || hole.fade_out > 0 {
                        j.push_hole(hole.frames, hole.fade_out);
                    }
                    j.push(s);
                }
            }
            // the device plays 1 ms, the output thread tops it up
            let take = dev_level.min(48);
            dev_level -= take;
            played.extend(std::iter::repeat(f32::NAN).take(take)); // placeholder, filled below
            if dev_level < low {
                let mut chunk = vec![0.0f32; (fill - dev_level) * CHANNELS];
                let n = j.pull_upto(&mut chunk, dev_level < critical);
                dev_queue_push(&mut played, &chunk[..n * CHANNELS]);
                dev_level += n;
            }
        }
        (played.into_iter().filter(|v| !v.is_nan()).collect(), j)
    }

    /// (Device output order is what matters: append the written frames.)
    fn dev_queue_push(played: &mut Vec<f32>, interleaved: &[f32]) {
        played.extend(interleaved.chunks(CHANNELS).map(|f| f[0]));
    }

    #[test]
    fn lost_audio_through_the_whole_chain_has_no_hard_step() {
        for phase in 0..10 {
            // open and close of another stream every 150 ms (as in t13)
            let events: Vec<usize> = (0..10).flat_map(|k| [1000 + k * 150, 1060 + k * 150]).collect();
            let (out, j) = simulate_device(20, 3500, &events, phase);
            let c = 2.0 * (2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).cos();
            let skip = 48 * 200;
            let worst = (skip..out.len()).map(|n| ((out[n] - c * out[n - 1] + out[n - 2]).abs(), n)).fold((0.0f32, 0), |m, v| if v.0 > m.0 { v } else { m });
            println!("phase {phase}: residual {} underruns {}", worst.0, j.underruns);
            assert!(worst.0 < 0.005, "phase {phase}: residual {} at frame {} ({} underruns)", worst.0, worst.1, j.underruns);
        }
    }

    #[test]
    fn steady_flow_passes_samples_through_unchanged() {
        let (out, j, _) = simulate(12, 3, 0.0, &[]);
        assert_eq!(j.underruns, 0);
        assert_eq!(j.corrected_frames, 0, "no rate correction needed");
        assert!(glitches(&out[48 * 20..]).is_empty());
    }

    #[test]
    fn lost_audio_is_absorbed_and_the_level_recovers() {
        // the engine loses 8 ms at 1 s and 12 ms at 5 s (seen when other programs
        // open/close streams). With a 25 ms start level there are 15 ms of margin; a
        // plain FIFO would then be left with 7 ms and 0 ms less margin for good.
        let (out, j, levels) = simulate(25, 12, 0.0, &[(1000, 8), (5000, 12)]);
        assert_eq!(j.underruns, 0, "no underrun; levels before push around the losses: {:?}", &levels[95..130]);
        let g: Vec<usize> = glitches(&out[48 * 20..]).iter().map(|n| (n + 48 * 20) / 48).collect();
        // the source itself jumps at the losses (that phase jump is in the audio we get)
        assert!(g.iter().all(|&ms| (1000..1040).contains(&ms) || (5000..5040).contains(&ms)), "glitches only where the source has them: {g:?}");
        assert!(j.speed() == 1.0, "back to 1:1 pass-through at the end: {}", j.speed());
        let end = &levels[levels.len() - 50..];
        let avg = end.iter().sum::<usize>() as f64 / end.len() as f64 / 48.0;
        assert!((13.5..=16.5).contains(&avg), "level before push back near start - packet (15 ms): {avg:.2} ms");
    }

    #[test]
    fn clock_drift_is_followed_without_underruns() {
        for (start, drift) in [(12, 0.0003), (12, -0.0003), (20, 0.0015), (20, -0.0015)] {
            let (out, j, _) = simulate(start, 40, drift, &[]);
            assert_eq!(j.underruns, 0, "drift {drift}");
            assert_eq!(j.dropped_frames, 0, "drift {drift}: no overflow drops either");
            assert!(glitches(&out[48 * 20..]).is_empty(), "drift {drift}: correction is glitch-free");
            assert!((j.speed() - 1.0 / (1.0 + drift)).abs() < 0.0005, "drift {drift}: speed {} (device fast -> consume slower)", j.speed());
        }
    }

    #[test]
    fn dither_is_never_zero_after_i16_truncation_and_stays_below_minus_80_db() {
        let mut buf = vec![0.0f32; 48_000 * CHANNELS];
        let mut rng = 1;
        add_dither(&mut buf, &mut rng);
        let ints: Vec<i16> = buf.iter().map(|v| (v * 32767.0) as i16).collect();
        assert!(ints.iter().all(|&i| i == 1 || i == -1), "every sample +-1 LSB");
        let pos = ints.iter().filter(|&&i| i > 0).count();
        assert!((20_000..76_000).contains(&pos), "random sign, roughly balanced: {pos}");
        let rms = (buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32).sqrt();
        assert!(20.0 * rms.log10() < -80.0);
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
