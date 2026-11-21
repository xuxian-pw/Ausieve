//! Signal analysis helpers.

/// Level of a sine component at `freq` in `x`, in dBFS of sine *amplitude*
/// (a full-scale sine of amplitude 1.0 reads 0 dB, amplitude 0.1 reads -20 dB).
///
/// Uses a Hann window, so leakage from other, distant frequencies is small.
pub fn tone_level_db(x: &[f32], rate: f32, freq: f32) -> f32 {
    let n = x.len();
    if n < 2 {
        return -300.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
    let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &s) in x.iter().enumerate() {
        let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
        let v = s as f64 * hann;
        let ph = w * i as f64;
        re += v * ph.cos();
        im -= v * ph.sin();
        wsum += hann;
    }
    let amp = 2.0 * (re * re + im * im).sqrt() / wsum;
    to_db(amp as f32)
}

/// Like [`tone_level_db`], but the highest level within `freq * (1 +- rel)`: for
/// signals whose pitch may be shifted slightly (capture's drift correction plays up to
/// 0.5 % faster or slower for a few seconds after an audio loss).
pub fn tone_level_db_near(x: &[f32], rate: f32, freq: f32, rel: f32) -> f32 {
    // a quarter of the frequency resolution of an x.len()-long window
    let step = (rate / x.len().max(1) as f32 / 4.0).max(0.05);
    let (lo, hi) = (freq * (1.0 - rel), freq * (1.0 + rel));
    let mut best = tone_level_db(x, rate, freq);
    let mut f = lo;
    while f <= hi {
        best = best.max(tone_level_db(x, rate, f));
        f += step;
    }
    best
}

/// RMS of a block in dBFS (relative to amplitude 1.0).
pub fn rms_db(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -300.0;
    }
    let p: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64;
    to_db(p.sqrt() as f32)
}

pub fn to_db(amp: f32) -> f32 {
    if amp <= 1e-15 {
        -300.0
    } else {
        20.0 * amp.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin())
            .collect()
    }

    #[test]
    fn goertzel_reads_amplitude() {
        let x = sine(440.0, 0.1, 48000);
        let l = tone_level_db(&x, 48000.0, 440.0);
        assert!((l + 20.0).abs() < 0.2, "{l}");
        let other = tone_level_db(&x, 48000.0, 1000.0);
        assert!(other < -100.0, "{other}");
    }

    #[test]
    fn level_near_finds_a_slightly_shifted_tone() {
        let x = sine(440.0 * 1.005, 0.1, 48000);
        assert!(tone_level_db(&x, 48000.0, 440.0) < -30.0, "a 1 s Hann window is narrow");
        let l = tone_level_db_near(&x, 48000.0, 440.0, 0.006);
        assert!((l + 20.0).abs() < 0.3, "{l}");
        assert!(tone_level_db_near(&x, 48000.0, 1000.0, 0.006) < -100.0);
    }
}
