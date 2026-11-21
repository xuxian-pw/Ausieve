//! Concealment of the audio that process loopback loses.
//!
//! Whenever any program on the system opens or closes an audio stream (e.g. every
//! click on the volume slider plays a new "ding"), Windows' process loopback loses
//! audio. The QPC timestamp of the next packet jumps 1-12 ms ahead, with no
//! `DATA_DISCONTINUITY` flag, and the packets after it carry about 30 ms of exact
//! digital zeros before the sound resumes. Played as is, that is a ~40 ms dropout with
//! hard steps at both ends, audible as a crackle on the receiving side (RustDesk etc.).
//! Device loopback ("System sound") loses nothing in the same situation.
//!
//! [`Concealer`] detects the hole from the timestamps and bridges it the way
//! packet-loss concealment in VoIP does. It repeats the last period of the preceding
//! audio (found by autocorrelation) for the missing time and for the zeros that
//! follow, then crossfades into the real audio as soon as it resumes. The timeline is
//! kept (the fill is as long as what it replaces), so the jitter buffer downstream
//! doesn't lose level either. Ordinary silence (no timestamp hole) is never touched.

use std::collections::VecDeque;

use crate::wasapi::CHANNELS;

/// Holes shorter than this are timestamp jitter, not lost audio.
const MIN_GAP_MS: f64 = 0.5;
/// Longer holes are a stream that went idle (process loopback delivers nothing while
/// nothing plays) and resumed: nothing to conceal.
const MAX_GAP_MS: f64 = 40.0;
/// The fill plays at full level this long (measured: hole plus the zeros after it is
/// 35-42 ms). If the audio still hasn't resumed, the fill then fades out over
/// FILL_FADE_MS, because the sound really stopped.
const FILL_HOLD_MS: f64 = 50.0;
const FILL_FADE_MS: f64 = 20.0;
/// Crossfade from the fill into the real audio.
const XFADE_MS: f64 = 3.0;
/// Period search range of the fill, and the correlation window.
const MIN_PERIOD_MS: f64 = 2.5;
const MAX_PERIOD_MS: f64 = 15.0;
const WINDOW_MS: f64 = 4.0;
/// The start of the fill is pulled onto the last real sample over this time.
const JOIN_MS: f64 = 1.0;
/// The fill's length may deviate this much from what it replaces, to line up with the
/// audio after it (timestamps are not sample-exact, periods are not integers).
const ALIGN_MS: f64 = 0.5;
/// History kept for the period search.
const HISTORY_MS: f64 = 40.0;

/// A running fill: the periodic extension of the audio before a hole.
struct Fill {
    /// One period (interleaved).
    pattern: Vec<f32>,
    period: usize,
    /// Offset that pulls the start onto the last real sample, per channel.
    join: [f32; CHANNELS],
    join_len: usize,
    hold: usize,
    fade: usize,
    /// Frames of fill emitted so far.
    pos: usize,
}

impl Fill {
    fn value(&self, i: usize, c: usize) -> f32 {
        let mut v = self.pattern[(i % self.period) * CHANNELS + c];
        if i < self.join_len {
            v += self.join[c] * (1.0 - (i + 1) as f32 / self.join_len as f32);
        }
        if i >= self.hold {
            v *= 1.0 - ((i - self.hold) as f32 / self.fade as f32).min(1.0);
        }
        v
    }

    fn mono(&self, i: usize) -> f32 {
        (0..CHANNELS).map(|c| self.value(i, c)).sum()
    }

    fn exhausted(&self) -> bool {
        self.pos >= self.hold + self.fade
    }
}

pub struct Concealer {
    rate: f64,
    /// Last frames handed out (interleaved).
    hist: VecDeque<f32>,
    /// Expected QPC timestamp (100 ns) of the next packet.
    next_qpc: Option<f64>,
    fill: Option<Fill>,
    out: Vec<f32>,
    /// Holes concealed, and the frames of fill inserted (for the holes and the zeros
    /// after them).
    pub concealed: u64,
    pub concealed_frames: u64,
}

impl Concealer {
    pub fn new(rate: u32) -> Concealer {
        Concealer {
            rate: rate as f64,
            hist: VecDeque::new(),
            next_qpc: None,
            fill: None,
            out: Vec::new(),
            concealed: 0,
            concealed_frames: 0,
        }
    }

    fn ms(&self, ms: f64) -> usize {
        (ms * self.rate / 1000.0).round() as usize
    }

    fn hist_frames(&self) -> usize {
        self.hist.len() / CHANNELS
    }

    fn h(&self, frame: usize, c: usize) -> f32 {
        self.hist[frame * CHANNELS + c]
    }

    /// Mono value of history frame `i`.
    fn mono(&self, i: usize) -> f32 {
        (0..CHANNELS).map(|c| self.h(i, c)).sum()
    }

    /// Takes one captured packet (interleaved, with its QPC timestamp in 100 ns) and
    /// returns what to play: the packet itself, or, around a hole, the fill with the
    /// packet's real audio crossfaded in.
    pub fn process(&mut self, samples: &[f32], qpc_100ns: u64) -> &[f32] {
        let n = samples.len() / CHANNELS;
        let qpc = qpc_100ns as f64;
        let missing = self.next_qpc.map(|e| (qpc - e) * self.rate / 1e7).unwrap_or(0.0);
        self.next_qpc = Some(qpc + n as f64 * 1e7 / self.rate);
        self.out.clear();
        let missing_ms = missing * 1000.0 / self.rate;
        if missing_ms > MAX_GAP_MS {
            // the stream went idle: start over
            self.hist.clear();
            self.fill = None;
        }
        let mut pending = 0usize; // fill frames for the hole, before the packet's audio
        if (MIN_GAP_MS..=MAX_GAP_MS).contains(&missing_ms) {
            if self.fill.is_none() && self.hist_frames() >= self.ms(MAX_PERIOD_MS + WINDOW_MS) + 2 {
                self.fill = Some(self.start_fill());
                self.concealed += 1;
            }
            pending = missing.round() as usize;
        }
        if self.fill.is_some() && n > 0 {
            // the zeros after a hole are lost audio too
            let zeros = samples.chunks(CHANNELS).take_while(|f| f.iter().all(|v| *v == 0.0)).count();
            self.emit_fill(samples, pending, zeros);
        } else {
            self.out.extend_from_slice(samples);
        }
        // remember the end of what was handed out
        let keep = self.ms(HISTORY_MS) * CHANNELS;
        self.hist.extend(self.out.iter().copied());
        if self.hist.len() > keep {
            let d = self.hist.len() - keep;
            self.hist.drain(..d);
        }
        &self.out
    }

    /// The period whose repetition best continues the history (normalised
    /// autocorrelation of the last WINDOW_MS against the same window P frames earlier).
    fn period(&self) -> usize {
        let h = self.hist_frames();
        let w = self.ms(WINDOW_MS);
        let tail: Vec<f32> = (h - w..h).map(|i| self.mono(i)).collect();
        let e_tail: f32 = tail.iter().map(|v| v * v).sum();
        let (pmin, pmax) = (self.ms(MIN_PERIOD_MS), self.ms(MAX_PERIOD_MS));
        let mut best = (pmin, f32::MIN);
        for p in pmin..=pmax {
            let (mut xy, mut yy) = (0.0f32, 0.0f32);
            for (k, t) in tail.iter().enumerate() {
                let y = self.mono(h - w + k - p);
                xy += t * y;
                yy += y * y;
            }
            let c = if e_tail > 0.0 && yy > 0.0 { xy / (e_tail * yy).sqrt() } else { 0.0 };
            if c > best.1 + 1e-4 {
                best = (p, c);
            }
        }
        best.0
    }

    fn start_fill(&self) -> Fill {
        let h = self.hist_frames();
        let p = self.period();
        let mut pattern = Vec::with_capacity(p * CHANNELS);
        for i in h - p..h {
            for c in 0..CHANNELS {
                pattern.push(self.h(i, c));
            }
        }
        let mut join = [0.0; CHANNELS];
        for (c, j) in join.iter_mut().enumerate() {
            *j = self.h(h - 1, c) - self.h(h - 1 - p, c);
        }
        Fill { pattern, period: p, join, join_len: self.ms(JOIN_MS), hold: self.ms(FILL_HOLD_MS), fade: self.ms(FILL_FADE_MS), pos: 0 }
    }

    fn push_fill(&mut self, f: &Fill, from: usize, len: usize) {
        for i in from..from + len {
            for c in 0..CHANNELS {
                self.out.push(f.value(i, c));
            }
        }
        self.concealed_frames += len as u64;
    }

    /// Emits `pending` frames of fill for the hole, then `zeros` more in place of the
    /// packet's leading zeros. If real audio follows, crossfades into it and ends the fill.
    fn emit_fill(&mut self, samples: &[f32], pending: usize, zeros: usize) {
        let n = samples.len() / CHANNELS;
        let mut f = self.fill.take().unwrap();
        let want = pending + zeros;
        if zeros == n {
            // all zeros: keep filling, also in the next packet
            self.push_fill(&f, f.pos, want);
            f.pos += want;
            if !f.exhausted() {
                self.fill = Some(f);
            }
            return;
        }
        // Audio resumes at frame `zeros`. The fill length is `want`, adjusted by up to
        // +-ALIGN_MS so that the fill lines up best with the audio.
        let xf = self.ms(XFADE_MS).min(n - zeros);
        let align = self.ms(ALIGN_MS);
        let mono_new = |i: usize| (0..CHANNELS).map(|c| samples[(zeros + i) * CHANNELS + c]).sum::<f32>();
        let mut best = (want, f32::MIN);
        for len in want.saturating_sub(align)..=want + align {
            let (mut xy, mut xx, mut yy) = (0.0f32, 0.0f32, 0.0f32);
            for i in 0..xf {
                let (x, y) = (f.mono(f.pos + len + i), mono_new(i));
                xy += x * y;
                xx += x * x;
                yy += y * y;
            }
            let c = if xx > 0.0 && yy > 0.0 { xy / (xx * yy).sqrt() } else { 0.0 };
            // prefer the nominal length unless another lines up clearly better
            let c = c - 1e-3 * (len as f32 - want as f32).abs() / align.max(1) as f32;
            if c > best.1 {
                best = (len, c);
            }
        }
        let len = best.0;
        self.push_fill(&f, f.pos, len);
        let at = f.pos + len;
        for i in 0..xf {
            let w = (std::f32::consts::FRAC_PI_2 * (i as f32 + 0.5) / xf as f32).sin().powi(2);
            for c in 0..CHANNELS {
                self.out.push(f.value(at + i, c) * (1.0 - w) + samples[(zeros + i) * CHANNELS + c] * w);
            }
        }
        self.out.extend_from_slice(&samples[(zeros + xf) * CHANNELS..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Residual of the 2nd-order sine predictor (≈0 for a clean sine of `freq`).
    fn max_residual(x: &[f32], freq: f32) -> f32 {
        let c = 2.0 * (2.0 * std::f32::consts::PI * freq / RATE as f32).cos();
        (2..x.len()).map(|n| (x[n] - c * x[n - 1] + x[n - 2]).abs()).fold(0.0, f32::max)
    }

    /// Feeds a stereo signal `f(t)` in 10 ms packets through the concealer, the way
    /// process loopback delivers it around another stream opening. For each
    /// `(packet, lost, zeros)` in `holes`, `lost` frames are missing before that packet
    /// (its timestamp jumps), and its first `zeros` frames are exact zeros. Returns the
    /// left channel of the output.
    fn run(f: impl Fn(f64) -> f32, packets: usize, holes: &[(usize, usize, usize)]) -> (Vec<f32>, Concealer) {
        let mut cc = Concealer::new(RATE);
        let mut out = Vec::new();
        let mut pos = 0usize; // source frame position
        let mut zeros_left = 0usize;
        for k in 0..packets {
            if let Some(&(_, lost, zeros)) = holes.iter().find(|h| h.0 == k) {
                pos += lost;
                zeros_left = zeros;
            }
            let pkt: Vec<f32> = (pos..pos + 480)
                .flat_map(|i| {
                    let v = if i - pos < zeros_left { 0.0 } else { f(i as f64 / RATE as f64) };
                    [v; CHANNELS]
                })
                .collect();
            zeros_left = zeros_left.saturating_sub(480);
            let qpc = (pos as f64 * 1e7 / RATE as f64).round() as u64 + 123_456_789;
            out.extend(cc.process(&pkt, qpc).chunks(CHANNELS).map(|fr| fr[0]));
            pos += 480;
        }
        (out, cc)
    }

    #[test]
    fn continuous_audio_passes_unchanged() {
        let sine = |t: f64| (0.1 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32;
        let (out, cc) = run(sine, 50, &[]);
        assert_eq!(cc.concealed, 0);
        let want: Vec<f32> = (0..50 * 480).map(|i| sine(i as f64 / RATE as f64)).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn hole_in_a_sine_becomes_inaudible_and_keeps_the_timeline() {
        // (freq, lost frames, zeros after the hole); 210 + 1504 is what was measured
        for (freq, lose, zeros) in [(1000.0, 417usize, 0usize), (440.0, 89, 0), (440.0, 530, 0), (1000.0, 60, 0), (3150.0, 250, 0), (1000.0, 210, 1504), (440.0, 530, 1504), (440.0, 70, 960)] {
            let sine = |t: f64| (0.1 * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32;
            let (out, cc) = run(sine, 40, &[(10, lose, zeros), (25, lose, zeros)]);
            let what = format!("{freq} Hz, {lose} frames lost, then {zeros} zeros");
            assert_eq!(cc.concealed, 2, "{what}");
            // output is as long as the source time span (the hole is filled, not skipped),
            // up to the alignment of +-0.5 ms per hole
            let want = (40 * 480 + 2 * lose) as i64;
            assert!((out.len() as i64 - want).abs() <= 48, "{what}: {} frames, want {want}", out.len());
            let r = max_residual(&out, freq as f32);
            // a raw splice gives up to ~0.17; the glitch threshold of the e2e tests is 0.005
            assert!(r < 0.002, "{what}: residual {r}");
            // and no dropout: the level never sags
            let min_peak = out[480..out.len() - 480].chunks(240).map(|c| c.iter().fold(0.0f32, |m, v| m.max(v.abs()))).fold(1.0, f32::min);
            assert!(min_peak > 0.09, "{what}: a 5 ms block peaks at only {min_peak}");
        }
    }

    #[test]
    fn hole_in_a_chord_has_no_hard_step() {
        // not periodic within the search range: the fill is an approximation, but the
        // crossfade must not leave a step (the sample-to-sample change stays at the
        // level of the signal's own)
        let chord = |t: f64| {
            let w = 2.0 * std::f64::consts::PI * t;
            (0.05 * (w * 523.25).sin() + 0.04 * (w * 659.25).sin() + 0.03 * (w * 1318.5).sin() + 0.02 * (w * 2637.0).sin()) as f32
        };
        let (clean, _) = run(chord, 30, &[]);
        let max_step = |x: &[f32]| x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        let own = max_step(&clean);
        for (lose, zeros) in [(50, 0), (200, 0), (420, 0), (576, 0), (210, 1504)] {
            let (out, cc) = run(chord, 30, &[(15, lose, zeros)]);
            assert_eq!(cc.concealed, 1);
            let s = max_step(&out);
            assert!(s < own * 1.3, "{lose} frames lost, {zeros} zeros: max step {s} vs the signal's own {own}");
        }
    }

    #[test]
    fn idle_gaps_jitter_and_ordinary_silence_are_not_touched() {
        let sine = |t: f64| (0.1 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
        // a 1 s pause (loopback idle), a 10-frame (0.2 ms) timestamp wobble, and zeros
        // without a hole (the source really went quiet)
        let (out, cc) = run(sine, 40, &[(10, 48_000, 0), (20, 10, 0), (30, 0, 1000)]);
        assert_eq!(cc.concealed, 0);
        assert_eq!(out.len(), 40 * 480);
        assert!(out[30 * 480..30 * 480 + 1000].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn sound_that_really_stops_at_a_hole_fades_out() {
        // the sound ends with the hole: the fill must not loop forever
        let sine = |t: f64| (0.1 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
        let (out, cc) = run(sine, 40, &[(10, 200, 480 * 30)]);
        assert_eq!(cc.concealed, 1);
        let start = 10 * 480;
        let peak = |a: usize, b: usize| out[start + a..start + b].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak(0, 2000) > 0.09, "fill holds the level first");
        assert!(peak(4000, 6000) < 1e-6, "after 70 ms the fill has faded out: {}", peak(4000, 6000));
    }
}
