//! Keeps capture on the QPC timeline across the holes that process loopback has.
//!
//! Whenever any program on the system opens or closes an audio stream (each click on
//! the volume slider plays a new ding: one stream open and one close), process loopback
//! loses audio. The next packet's QPC timestamp jumps 1-12 ms ahead, with no
//! `DATA_DISCONTINUITY` flag, and while other sound is playing, about 31 ms of exact
//! zeros follow before the sound resumes. Device loopback loses nothing.
//!
//! Joining the packets end to end (as before) shortened the timeline by the missing
//! time. The jitter buffer lost that much level, and its drift control then resampled
//! the next 2-3 s of audio, e.g. the whole ding, to refill it. Instead, like OBS (which
//! places audio by its timestamps), [`Timeline`] reports the hole so that silence of the
//! same length is inserted. Around a hole, the hard steps into and out of the lost
//! audio are softened by a short fade-out / fade-in. No audio is ever synthesised.

use crate::wasapi::CHANNELS;

/// Holes shorter than this are rounding, not lost audio. Process loopback timestamps
/// are sample-exact otherwise (measured: 0.00 ms deviation), and losses as small as
/// 0.4 ms (without zeros after them) were seen.
const MIN_GAP_MS: f64 = 0.05;
/// Longer holes are a stream that went idle (process loopback delivers nothing while
/// nothing plays) and resumed: inserting that much silence would only add latency.
const MAX_GAP_MS: f64 = 40.0;
/// Length of the fade-out before and the fade-in after the lost audio.
pub const FADE_MS: f64 = 3.0;
/// A packet starting with at least this many frames of exact zeros right after sound
/// is a cut (lost audio), even without a visible timestamp jump.
const CUT_ZEROS: usize = 8;
/// The fade-in waits at most this long for the sound to resume after a hole.
const RESUME_WAIT_MS: f64 = 100.0;

/// What to do before handing out a packet.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Hole {
    /// Frames of silence to insert (the audio that was lost).
    pub frames: usize,
    /// Fade out this many frames of what was handed out before (still queued).
    pub fade_out: usize,
}

pub struct Timeline {
    rate: f64,
    /// Expected QPC timestamp (100 ns) of the next packet.
    next_qpc: Option<f64>,
    /// The last frame handed out had sound in it.
    sounding: bool,
    /// After a hole while sound played: frames still to wait for the sound to resume,
    /// and the fade-in position once it has.
    resume_wait: Option<usize>,
    fade_in_pos: Option<usize>,
    out: Vec<f32>,
    pub holes: u64,
    pub hole_frames: u64,
    /// Cut-offs whose edges were faded.
    pub cuts: u64,
}

fn ramp(i: usize, n: usize) -> f32 {
    // raised cosine 0 -> 1 over n frames (i = 0..n)
    let x = (i as f32 + 0.5) / n as f32;
    (std::f32::consts::FRAC_PI_2 * x).sin().powi(2)
}

/// Fades out the last `n` frames of `buf` (interleaved).
pub fn fade_out_tail(buf: &mut [f32], n: usize) {
    let frames = buf.len() / CHANNELS;
    let n = n.min(frames);
    for k in 0..n {
        let g = ramp(n - 1 - k, n);
        let f = frames - n + k;
        buf[f * CHANNELS..(f + 1) * CHANNELS].iter_mut().for_each(|v| *v *= g);
    }
}

impl Timeline {
    pub fn new(rate: u32) -> Timeline {
        Timeline { rate: rate as f64, next_qpc: None, sounding: false, resume_wait: None, fade_in_pos: None, out: Vec::new(), holes: 0, hole_frames: 0, cuts: 0 }
    }

    fn ms(&self, ms: f64) -> usize {
        (ms * self.rate / 1000.0).round() as usize
    }

    /// Takes one captured packet (interleaved, QPC timestamp in 100 ns). Returns the
    /// hole before it (if any) and the samples to hand out after the hole (the packet,
    /// faded in where the sound resumes after a hole).
    pub fn packet(&mut self, samples: &[f32], qpc_100ns: u64) -> (Hole, &[f32]) {
        let n = samples.len() / CHANNELS;
        let qpc = qpc_100ns as f64;
        let missing = self.next_qpc.map(|e| (qpc - e) * self.rate / 1e7).unwrap_or(0.0);
        self.next_qpc = Some(qpc + n as f64 * 1e7 / self.rate);
        let missing_ms = missing * 1000.0 / self.rate;
        let mut hole = Hole::default();
        // Sound cut off into exact zeros at a packet boundary: the zeros after lost
        // audio (the timestamp jump can be too small to see, < 0.5 ms). Real audio
        // doesn't jump from sound to a run of exact digital zeros.
        let cut = n >= CUT_ZEROS && samples[..CUT_ZEROS * CHANNELS].iter().all(|v| *v == 0.0);
        let in_range = (MIN_GAP_MS..=MAX_GAP_MS).contains(&missing_ms);
        if in_range {
            hole.frames = missing.round() as usize;
            self.holes += 1;
            self.hole_frames += hole.frames as u64;
        }
        if self.sounding && (in_range || cut) {
            // soften both edges of the lost audio
            hole.fade_out = self.ms(FADE_MS);
            self.resume_wait = Some(self.ms(RESUME_WAIT_MS));
            self.fade_in_pos = None;
            self.cuts += 1;
        }
        if missing_ms > MAX_GAP_MS {
            self.resume_wait = None;
            self.fade_in_pos = None;
        }
        self.out.clear();
        self.out.extend_from_slice(samples);
        let fade = self.ms(FADE_MS);
        for f in 0..n {
            let frame = &mut self.out[f * CHANNELS..(f + 1) * CHANNELS];
            if let Some(w) = self.resume_wait {
                if frame.iter().all(|v| *v == 0.0) {
                    // still the zeros after the hole
                    self.resume_wait = w.checked_sub(1).filter(|&w| w > 0);
                    continue;
                }
                self.resume_wait = None;
                self.fade_in_pos = Some(0);
            }
            if let Some(p) = self.fade_in_pos {
                let g = ramp(p, fade);
                frame.iter_mut().for_each(|v| *v *= g);
                self.fade_in_pos = (p + 1 < fade).then_some(p + 1);
            }
        }
        if n > 0 {
            self.sounding = samples[(n - 1) * CHANNELS..].iter().any(|v| *v != 0.0);
        }
        (hole, &self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Feeds a mono-valued signal in 10 ms packets the way process loopback delivers it
    /// around another stream opening: for each `(packet, lost, zeros)`, `lost` frames
    /// are missing before that packet (the timestamp jumps) and its first `zeros` frames
    /// are exact zeros. Plays the result into a FIFO the way the engine does (fade the
    /// queued tail, insert the hole as silence). Returns the left channel.
    fn run(f: impl Fn(usize) -> f32, packets: usize, holes: &[(usize, usize, usize)]) -> (Vec<f32>, Timeline) {
        let mut tl = Timeline::new(RATE);
        let mut out: Vec<f32> = Vec::new();
        let mut pos = 0usize;
        let mut zeros_left = 0usize;
        for k in 0..packets {
            if let Some(&(_, lost, zeros)) = holes.iter().find(|h| h.0 == k) {
                pos += lost;
                zeros_left = zeros;
            }
            let pkt: Vec<f32> = (pos..pos + 480).flat_map(|i| [if i - pos < zeros_left { 0.0 } else { f(i) }; CHANNELS]).collect();
            zeros_left = zeros_left.saturating_sub(480);
            let qpc = (pos as f64 * 1e7 / RATE as f64).round() as u64 + 987_654_321;
            let (hole, s) = tl.packet(&pkt, qpc);
            let mut il: Vec<f32> = out.iter().flat_map(|v| [*v; CHANNELS]).collect();
            fade_out_tail(&mut il, hole.fade_out);
            out = il.chunks(CHANNELS).map(|c| c[0]).collect();
            out.extend(std::iter::repeat(0.0).take(hole.frames));
            out.extend(s.chunks(CHANNELS).map(|c| c[0]));
            pos += 480;
        }
        (out, tl)
    }

    fn sine(i: usize) -> f32 {
        (0.1 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE as f64).sin()) as f32
    }

    #[test]
    fn continuous_audio_passes_bit_exact() {
        let (out, tl) = run(sine, 50, &[]);
        assert_eq!(tl.holes, 0);
        assert_eq!(out, (0..50 * 480).map(sine).collect::<Vec<_>>());
    }

    #[test]
    fn hole_is_inserted_as_silence_so_the_timeline_keeps_its_length() {
        let (out, tl) = run(sine, 40, &[(10, 417, 1504), (25, 60, 0)]);
        assert_eq!(tl.holes, 2);
        assert_eq!(out.len(), 40 * 480 + 417 + 60);
        // audio after the hole sits at its original time: from the resume point on
        // (after the fade-in) it is the source sample for sample
        let resume = 10 * 480 + 417 + 1504 + 144;
        for i in resume..resume + 2000 {
            assert_eq!(out[i], sine(i), "frame {i}");
        }
    }

    #[test]
    fn edges_of_lost_audio_are_soft() {
        let (out, _) = run(sine, 40, &[(10, 417, 1504), (25, 60, 0), (30, 200, 960)]);
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        let own = (0..2000).map(|i| (sine(i + 1) - sine(i)).abs()).fold(0.0f32, f32::max);
        // a hard cut of a 0.1 sine steps by up to 0.1; soft edges stay at the sine's own
        // steepest slope
        assert!(max_step <= own * 1.01, "max step {max_step} vs the sine's own {own}");
    }

    #[test]
    fn sound_starting_after_a_hole_in_silence_is_untouched() {
        // the ding case: the hole falls into silence, then the sound starts
        let ding = |i: usize| if i < 20 * 480 { 0.0 } else { sine(i) * 0.5 };
        let (out, tl) = run(ding, 40, &[(15, 500, 0)]);
        assert_eq!(tl.holes, 1);
        let src: Vec<f32> = (0..40 * 480 + 500).map(ding).collect();
        assert_eq!(out, src, "bit-exact apart from the inserted silence (which was silence anyway)");
    }

    #[test]
    fn cut_without_a_visible_timestamp_jump_is_softened_too() {
        // 1 frame jump (below the hole threshold), then 1504 zeros
        let (out, tl) = run(sine, 30, &[(10, 1, 1504)]);
        assert_eq!((tl.holes, tl.cuts), (0, 1));
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        let own = (0..2000).map(|i| (sine(i + 1) - sine(i)).abs()).fold(0.0f32, f32::max);
        assert!(max_step <= own * 1.01, "max step {max_step} vs the sine's own {own}");
    }

    #[test]
    fn tiny_loss_without_zeros_is_softened() {
        // 0.4 ms lost, the sound continues right away (seen once at a stream open)
        let (out, tl) = run(sine, 30, &[(10, 19, 0)]);
        assert_eq!((tl.holes, tl.cuts), (1, 1));
        let c = 2.0 * (2.0 * std::f32::consts::PI * 1000.0 / RATE as f32).cos();
        let r = (2..out.len()).map(|n| (out[n] - c * out[n - 1] + out[n - 2]).abs()).fold(0.0f32, f32::max);
        assert!(r < 0.005, "residual {r}");
    }

    #[test]
    fn idle_gaps_and_jitter_insert_nothing() {
        // 1 s pause (loopback idle) and a 1-frame (0.02 ms) timestamp rounding
        let (out, tl) = run(sine, 40, &[(10, 48_000, 0), (20, 1, 0)]);
        assert_eq!(tl.holes, 0);
        assert_eq!(out.len(), 40 * 480);
    }
}
