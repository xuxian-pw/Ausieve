//! End-to-end tests on real audio devices (no mocks).
//!
//! Signal path under test:
//!
//! ```text
//! tone.exe A (1000 Hz, excluded) ─┐
//! tone.exe B ( 440 Hz, kept)     ─┼─► tone device (default playback, or AUDIO_EXCLUDE_TONE_DEVICE)
//!                                 │
//!            process loopback (EXCLUDE A's tree) ─► capture.exe ─► cable playback (exclusive)
//!                                                                     │
//!                                       tests record the cable's recording side ◄┘
//! ```
//!
//! Processes are marked with distinct sine frequencies and measured with a Goertzel
//! filter / cross-correlation. As a reference, the tests also record a loopback of the
//! tone device ("local output" = what the user hears).
//!
//! Tests share the audio devices, so they are serialised by a global lock.
//! Run with `cargo test -- --nocapture` to see the measured values.
//!
//! Environment variables:
//! * `AUDIO_EXCLUDE_CABLE`        cable name (default `Line 1 (Virtual Audio Cable)`)
//! * `AUDIO_EXCLUDE_TONE_DEVICE`  playback device for the test tones (default: system
//!   default). Pointing this at a spare cable (e.g. `Line 2 (Virtual Audio Cable)`) makes
//!   the tests silent and avoids the endpoint's sound enhancements.
//! * `AUDIO_EXCLUDE_MAX_LATENCY_MS` latency threshold (default 50)
//! * `AUDIO_EXCLUDE_DUMP=1`       print 10 ms envelopes in the feedback test

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audio_exclude::dsp::tone_level_db;
use audio_exclude::wasapi::{self, CaptureStream, CHANNELS, SAMPLE_RATE};
use rustfft::{num_complex::Complex, FftPlanner};
use windows::Win32::Media::Audio::{eCapture, eRender};

const RATE: f32 = SAMPLE_RATE as f32;
const F_EXCLUDED: f32 = 1000.0;
const F_KEPT: f32 = 440.0;
/// A tone counts as "present" above this level (tones are generated at -20 dBFS).
const PRESENT_DB: f32 = -45.0;
const REQUIRED_SUPPRESSION_DB: f32 = 40.0;
const EXCLUDED_NAME: &str = "excluded_app.exe";

fn cable_name() -> String {
    std::env::var("AUDIO_EXCLUDE_CABLE").unwrap_or_else(|_| "Line 1 (Virtual Audio Cable)".into())
}

fn tone_device() -> Option<String> {
    std::env::var("AUDIO_EXCLUDE_TONE_DEVICE").ok().filter(|s| !s.is_empty())
}

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// child processes
// ---------------------------------------------------------------------------

/// A spawned helper process whose stdout lines are collected; killed on drop.
struct Proc {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Proc {
    fn spawn(exe: &Path, args: &[&str]) -> Proc {
        let mut child = Command::new(exe)
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {exe:?}: {e}"));
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Proc { child, lines, seen: Vec::new() }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for a stdout line starting with `prefix`; returns the rest of the line.
    fn wait_line(&mut self, prefix: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(l) => {
                    self.seen.push(l.clone());
                    if let Some(rest) = l.strip_prefix(prefix) {
                        return rest.trim().to_string();
                    }
                }
                Err(_) => panic!(
                    "timed out waiting for {prefix:?}; output so far: {:?}; exit: {:?}",
                    self.seen,
                    self.child.try_wait()
                ),
            }
        }
    }

    /// Waits until `capture` reports `TARGET <pid>` (also accepts an earlier such line).
    fn wait_target(&mut self, pid: u32, timeout: Duration) {
        let want = format!("TARGET {pid}");
        if self.seen.iter().any(|l| *l == want) {
            return;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("capture never targeted pid {pid}; output: {:?}", self.seen);
            }
            if self.wait_line("TARGET", left) == pid.to_string() {
                return;
            }
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tone_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tone"))
}

/// A copy of `tone.exe` under a unique name, so exclusion by *name* can be tested while
/// the "kept" tone keeps running as tone.exe.
fn excluded_exe() -> PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let dst = Path::new(env!("CARGO_TARGET_TMPDIR")).join(EXCLUDED_NAME);
        if let Err(e) = std::fs::copy(tone_exe(), &dst) {
            // a stale copy may still be running from an aborted run; it is identical
            assert!(dst.exists(), "copy tone.exe -> {dst:?}: {e}");
        }
        dst
    })
    .clone()
}

fn start_tone(exe: &Path, freq: f32, extra: &[&str]) -> Proc {
    let f = freq.to_string();
    let dev = tone_device();
    let mut args = vec![f.as_str()];
    args.extend_from_slice(extra);
    if let Some(d) = &dev {
        args.extend(["--device", d.as_str()]);
    }
    let mut p = Proc::spawn(exe, &args);
    p.wait_line("PLAYING", Duration::from_secs(5));
    p
}

/// A process that never opens an audio stream: the exclusion target for "exclude nothing".
fn start_dummy() -> Proc {
    start_tone(&tone_exe(), 0.0, &["--silent", "--duration", "60000"])
}

fn start_capture(args: &[&str]) -> Proc {
    let cable = cable_name();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--output", cable.as_str()]);
    let mut p = Proc::spawn(Path::new(env!("CARGO_BIN_EXE_capture")), &all);
    p.wait_line("READY", Duration::from_secs(5));
    p
}

// ---------------------------------------------------------------------------
// recording
// ---------------------------------------------------------------------------

/// A mono recording placed on the QPC timeline.
struct Recording {
    samples: Vec<f32>,
    /// QPC time (100 ns units) of `samples[0]`.
    start_qpc: u64,
}

#[derive(Clone, Copy)]
enum Source {
    /// The recording side of the virtual cable (what RustDesk would read).
    Cable,
    /// Loopback of the tone device: what the user actually hears.
    Local,
}

/// Starts recording in a background thread; returns once the stream is running.
fn record(src: Source, ms: u64) -> JoinHandle<Recording> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let h = thread::spawn(move || {
        wasapi::init_com().unwrap();
        let mut cap = match src {
            Source::Cable => {
                let dev = wasapi::find_device(&cable_name(), eCapture).unwrap();
                CaptureStream::from_device(&dev, false, 20).unwrap()
            }
            Source::Local => {
                let dev = match tone_device() {
                    Some(n) => wasapi::find_device(&n, eRender).unwrap(),
                    None => wasapi::default_device(eRender).unwrap(),
                };
                CaptureStream::from_device(&dev, true, 20).unwrap()
            }
        };
        ready_tx.send(()).unwrap();
        let mut samples: Vec<f32> = Vec::new();
        let mut start_qpc: Option<u64> = None;
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            cap.wait(Duration::from_millis(50));
            cap.read(|p| {
                let base = *start_qpc.get_or_insert(p.qpc_100ns);
                // Place packets by timestamp. Small jitter is ignored; real gaps (loopback
                // delivers nothing while nothing plays) become zeros.
                let at = (p.qpc_100ns.saturating_sub(base) as f64 * RATE as f64 / 1e7).round() as usize;
                if at > samples.len() + 96 {
                    samples.resize(at, 0.0);
                }
                samples.extend(p.samples.chunks(CHANNELS).map(|f| f.iter().sum::<f32>() / CHANNELS as f32));
            })
            .unwrap();
        }
        Recording { samples, start_qpc: start_qpc.unwrap_or(0) }
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).expect("recorder did not start");
    h
}

/// Records the cable and the local output at the same time.
fn record_both(ms: u64) -> (JoinHandle<Recording>, JoinHandle<Recording>) {
    (record(Source::Cable, ms), record(Source::Local, ms))
}

fn levels(r: &Recording) -> (f32, f32) {
    (tone_level_db(&r.samples, RATE, F_KEPT), tone_level_db(&r.samples, RATE, F_EXCLUDED))
}

/// Records 1 s and checks: the excluded tone is audible locally but ≥40 dB below the kept
/// tone in the cable, and the kept tone is present in the cable.
fn assert_excluded(label: &str) {
    let (cable, local) = record_both(1000);
    let (cable, local) = (cable.join().unwrap(), local.join().unwrap());
    let (k, x) = levels(&cable);
    let (lk, lx) = levels(&local);
    println!(
        "[{label}] cable: {F_KEPT} Hz {k:.1} dB, {F_EXCLUDED} Hz {x:.1} dB (suppression {:.1} dB) | local: {F_KEPT} Hz {lk:.1} dB, {F_EXCLUDED} Hz {lx:.1} dB",
        k - x
    );
    assert!(lx > PRESENT_DB, "[{label}] excluded tone is not even playing locally ({lx:.1} dB)");
    assert!(k > PRESENT_DB, "[{label}] kept tone missing from cable ({k:.1} dB)");
    assert!(
        k - x >= REQUIRED_SUPPRESSION_DB,
        "[{label}] excluded tone only {:.1} dB below kept tone (need {REQUIRED_SUPPRESSION_DB})",
        k - x
    );
}

/// Shifts `r` onto the timeline of `reference` (same start time) using QPC timestamps.
fn align_to(r: &Recording, reference: &Recording) -> Vec<f32> {
    let off = ((r.start_qpc as i128 - reference.start_qpc as i128) as f64 * RATE as f64 / 1e7).round() as i64;
    if off >= 0 {
        let mut v = vec![0.0; off as usize];
        v.extend_from_slice(&r.samples);
        v
    } else {
        r.samples.get((-off) as usize..).unwrap_or(&[]).to_vec()
    }
}

/// Lag (in samples, 0..=max_lag) at which `delayed` best matches `reference`, and the
/// normalised correlation coefficient at that lag.
fn xcorr_lag(reference: &[f32], delayed: &[f32], max_lag: usize) -> (usize, f32) {
    let n = (reference.len() + delayed.len()).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let to_c = |x: &[f32]| {
        let mut v: Vec<Complex<f32>> = x.iter().map(|&s| Complex::new(s, 0.0)).collect();
        v.resize(n, Complex::new(0.0, 0.0));
        v
    };
    let (mut a, mut b) = (to_c(reference), to_c(delayed));
    fwd.process(&mut a);
    fwd.process(&mut b);
    let mut c: Vec<Complex<f32>> = a.iter().zip(&b).map(|(x, y)| x.conj() * y).collect();
    inv.process(&mut c);
    let ea: f32 = reference.iter().map(|v| v * v).sum();
    let eb: f32 = delayed.iter().map(|v| v * v).sum();
    let norm = (ea * eb).sqrt() * n as f32;
    let (lag, peak) = (0..=max_lag.min(n - 1))
        .map(|k| (k, c[k].re))
        .fold((0, f32::MIN), |m, x| if x.1 > m.1 { x } else { m });
    (lag, peak / norm)
}

fn envelope(x: &[f32], win: usize, freq: f32) -> Vec<f32> {
    x.chunks(win).map(|c| tone_level_db(c, RATE, freq)).collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// 1. Exclusion works: with A (1000 Hz) excluded by PID, the cable carries B (440 Hz)
/// and A is ≥40 dB below it.
#[test]
fn t1_exclusion_by_pid() {
    let _g = serial();
    let a = start_tone(&excluded_exe(), F_EXCLUDED, &["--duration", "8000"]);
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "8000"]);
    let pid = a.pid().to_string();
    let mut cap = start_capture(&["--exclude-pid", &pid]);
    cap.wait_target(a.pid(), Duration::from_secs(1));
    // first test after a build: give the freshly opened exclusive stream time to settle
    thread::sleep(Duration::from_millis(600));
    assert_excluded("t1 exclude pid");
}

/// 2. Control: excluding an unrelated silent process, both tones reach the cable.
#[test]
fn t2_control_nothing_excluded() {
    let _g = serial();
    let dummy = start_dummy();
    let _a = start_tone(&excluded_exe(), F_EXCLUDED, &["--duration", "8000"]);
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "8000"]);
    let pid = dummy.pid().to_string();
    let _cap = start_capture(&["--exclude-pid", &pid]);
    thread::sleep(Duration::from_millis(300));
    let (cable, local) = record_both(1000);
    let (cable, local) = (cable.join().unwrap(), local.join().unwrap());
    let (k, x) = levels(&cable);
    let (lk, lx) = levels(&local);
    println!("[t2 control] cable: {F_KEPT} Hz {k:.1} dB, {F_EXCLUDED} Hz {x:.1} dB | local: {lk:.1} dB, {lx:.1} dB");
    assert!(k > PRESENT_DB, "440 Hz missing from cable ({k:.1} dB)");
    assert!(x > PRESENT_DB, "1000 Hz missing from cable although nothing relevant is excluded ({x:.1} dB)");
    // the cable should carry the same levels the user hears (unity gain path)
    assert!((k - lk).abs() < 3.0 && (x - lx).abs() < 3.0, "cable level differs from local output by >3 dB");
}

/// 3. No feedback loop: capture must not re-capture its own output into the cable.
///
/// B plays a 200 ms burst. A loop would make the cable louder than the local output and
/// keep ringing after the local output has gone quiet. The local output is the reference
/// because endpoint enhancements (e.g. "Microsoft Audio Home Theater Effects") may add a
/// reverb tail to the source itself, which process loopback captures post-effect; with a
/// clean tone device the reference ends exactly 200 ms after it starts.
#[test]
fn t3_no_feedback_loop() {
    let _g = serial();
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    let _cap = start_capture(&["--exclude-pid", &pid]);
    let (cable, local) = record_both(3500);
    let _b = start_tone(&tone_exe(), F_KEPT, &["--delay", "500", "--burst", "200", "--duration", "3000"]);
    let (cable, local) = (cable.join().unwrap(), local.join().unwrap());

    let win = (RATE / 100.0) as usize; // 10 ms
    let cable_al = align_to(&cable, &local);
    let env_c = envelope(&cable_al, win, F_KEPT);
    let env_l = envelope(&local.samples, win, F_KEPT);
    if std::env::var("AUDIO_EXCLUDE_DUMP").is_ok() {
        let f = |e: &[f32]| e.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(" ");
        println!("[t3] local 10 ms envelope: {}", f(&env_l));
        println!("[t3] cable 10 ms envelope: {}", f(&env_c));
    }
    let max = |e: &[f32]| e.iter().cloned().fold(-300.0f32, f32::max);
    let (peak_c, peak_l) = (max(&env_c), max(&env_l));
    assert!(peak_l > PRESENT_DB, "burst not audible locally (peak {peak_l:.1} dB)");
    assert!(peak_c > PRESENT_DB, "burst never arrived in the cable (peak {peak_c:.1} dB)");
    let start_l = env_l.iter().position(|&l| l > peak_l - 10.0).unwrap();
    let on_l = env_l.iter().rposition(|&l| l > peak_l - 10.0).unwrap() - start_l + 1;
    let on_c = env_c.iter().rposition(|&l| l > peak_c - 10.0).unwrap() - env_c.iter().position(|&l| l > peak_c - 10.0).unwrap() + 1;
    // end of the audible local signal (last window within 50 dB of its peak)
    let end_l = env_l.iter().rposition(|&l| l > peak_l - 50.0).unwrap();
    let end_c = env_c.iter().rposition(|&l| l > peak_c - 50.0).unwrap();
    let tail_from = end_l + 8; // 80 ms after the local signal ended
    let tail_c = max(env_c.get(tail_from..).unwrap_or(&[]));
    let clean_source = end_l - start_l < 30;
    println!(
        "[t3 no feedback] local: peak {peak_l:.1} dB, {on_l}0 ms within 10 dB of peak, audible (>-50 dB rel) for {}0 ms{} | cable: peak {peak_c:.1} dB, {on_c}0 ms within 10 dB, went quiet {}0 ms after local did; cable max from +80 ms: {tail_c:.1} dB ({:.1} dB rel)",
        end_l - start_l + 1,
        if clean_source { "" } else { " (tone device adds a tail: endpoint enhancements)" },
        end_c as i64 - end_l as i64,
        tail_c - peak_c,
    );
    assert!(peak_c < peak_l + 3.0, "cable peak {peak_c:.1} dB exceeds local {peak_l:.1} dB: audio is recirculating");
    assert!(on_c <= on_l + 4, "burst lasted {on_c}0 ms in the cable but {on_l}0 ms locally: audio is recirculating");
    assert!(
        tail_c < peak_c - 50.0,
        "cable still at {tail_c:.1} dB ({:.1} dB rel) 80 ms after local output went quiet: feedback loop",
        tail_c - peak_c
    );
}

/// 4. Process restart: excluded by *name*, A is killed and restarted (new PID) and is
/// still excluded.
#[test]
fn t4_exclusion_survives_restart() {
    let _g = serial();
    let a1 = start_tone(&excluded_exe(), F_EXCLUDED, &["--duration", "15000"]);
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "15000"]);
    let mut cap = start_capture(&["--exclude-name", EXCLUDED_NAME]);
    cap.wait_target(a1.pid(), Duration::from_secs(1));
    thread::sleep(Duration::from_millis(300));
    assert_excluded("t4 before restart");

    let old = a1.pid();
    drop(a1); // kill
    let a2 = start_tone(&excluded_exe(), F_EXCLUDED, &["--duration", "15000"]);
    assert_ne!(old, a2.pid());
    let t = Instant::now();
    cap.wait_target(a2.pid(), Duration::from_secs(3));
    println!("[t4] capture re-targeted {old} -> {} after {:?}", a2.pid(), t.elapsed());
    thread::sleep(Duration::from_millis(300));
    assert_excluded("t4 after restart");
}

/// 5. Child process plays: A only spawns a child that makes the sound; excluding A's
/// (root) process tree by name still removes it.
#[test]
fn t5_exclusion_covers_child_process() {
    let _g = serial();
    let a = start_tone(&excluded_exe(), F_EXCLUDED, &["--spawn-child", "--duration", "8000"]);
    let child: u32 = a
        .seen
        .iter()
        .find_map(|l| l.strip_prefix("CHILD "))
        .and_then(|s| s.trim().parse().ok())
        .expect("no CHILD line");
    let player: u32 = a.seen.last().and_then(|l| l.strip_prefix("PLAYING ")).unwrap().trim().parse().unwrap();
    assert_eq!(player, child, "the child, not the parent, must be the one playing");
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "8000"]);
    let mut cap = start_capture(&["--exclude-name", EXCLUDED_NAME]);
    cap.wait_target(a.pid(), Duration::from_secs(1));
    println!("[t5] root {} (silent) -> child {child} (playing); capture excludes root", a.pid());
    thread::sleep(Duration::from_millis(300));
    assert_excluded("t5 child process");
}

/// 6. Latency: a 30 ms burst is cross-correlated between the local output and the cable
/// (both on the QPC clock). Reports the added end-to-end latency of capture.
#[test]
fn t6_latency() {
    let _g = serial();
    let max_ms: f32 = std::env::var("AUDIO_EXCLUDE_MAX_LATENCY_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(50.0);
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    let _cap = start_capture(&["--exclude-pid", &pid]);
    thread::sleep(Duration::from_millis(300));
    let mut results = Vec::new();
    for round in 0..3 {
        let (cable, local) = record_both(1800);
        let _b = start_tone(&tone_exe(), F_EXCLUDED, &["--delay", "500", "--burst", "30", "--duration", "1500"]);
        let (cable, local) = (cable.join().unwrap(), local.join().unwrap());
        let cable_al = align_to(&cable, &local);
        let (lag, coef) = xcorr_lag(&local.samples, &cable_al, (RATE * 0.3) as usize);
        let ms = lag as f32 * 1000.0 / RATE;
        println!("[t6 latency] round {round}: {ms:.1} ms (correlation {coef:.2})");
        assert!(coef > 0.5, "burst not found in cable (correlation {coef:.2})");
        results.push(ms);
    }
    results.sort_by(f32::total_cmp);
    let median = results[1];
    println!("[t6 latency] median {median:.1} ms, all {results:?} (threshold {max_ms} ms)");
    assert!(median < max_ms, "end-to-end latency {median:.1} ms >= {max_ms} ms");
}
