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

use audio_exclude::config::Config;
use audio_exclude::dsp::{tone_level_db, tone_level_db_near};
use audio_exclude::engine::{Engine, Event, Target};
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

/// Capture buffer of the test's own loopback recorders (default: the same as capture's,
/// AUDIO_EXCLUDE_RAW_BUFFER_MS to compare).
fn raw_buffer_ms() -> i64 {
    std::env::var("AUDIO_EXCLUDE_RAW_BUFFER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(wasapi::LOOPBACK_BUFFER_MS)
}

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static WARM: OnceLock<()> = OnceLock::new();
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    WARM.get_or_init(warm_up);
    g
}

/// Some security software (seen with ESET) plays an alert sound on the default output
/// the first time a new executable opens a recording device. Process loopback captures
/// that sound into every measurement. So, once per test process: open the cable's
/// recording side, then wait until the whole system output has been silent for 0.5 s
/// (at most 10 s) before any test starts.
fn warm_up() {
    drop(record(Source::Cable, 200).join());
    let t0 = Instant::now();
    let mut quiet_since: Option<Instant> = None;
    while t0.elapsed() < Duration::from_secs(10) {
        let r = record(Source::ProcessLoopback(std::process::id()), 250).join().unwrap();
        if audio_exclude::dsp::rms_db(&r.samples) < -90.0 {
            let q = *quiet_since.get_or_insert_with(Instant::now);
            if q.elapsed() >= Duration::from_millis(500) {
                break;
            }
        } else {
            quiet_since = None;
        }
    }
    if t0.elapsed() > Duration::from_millis(1500) {
        println!("[warm-up] waited {:?} for other system sounds to stop", t0.elapsed());
    }
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
        let mut cmd = Command::new(exe);
        cmd.args(args);
        Proc::spawn_cmd(cmd)
    }

    fn spawn_cmd(mut cmd: Command) -> Proc {
        let mut child = cmd
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
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
    start_capture_env(args, &[])
}

fn start_capture_env(args: &[&str], env: &[(&str, &str)]) -> Proc {
    let cable = cable_name();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--output", cable.as_str()]);
    let buf = std::env::var("AUDIO_EXCLUDE_BUFFER_MS").unwrap_or_default();
    if !buf.is_empty() {
        all.extend(["--buffer-ms", buf.as_str()]);
    }
    // AUDIO_EXCLUDE_CAPTURE_EXE: run the tests against another build (A/B comparisons)
    let exe = std::env::var_os("AUDIO_EXCLUDE_CAPTURE_EXE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_capture")));
    let mut cmd = Command::new(&exe);
    cmd.args(&all).envs(env.iter().copied());
    let mut p = Proc::spawn_cmd(cmd);
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
    /// Recorder-side gaps (>2 ms holes or DATA_DISCONTINUITY flags): if non-zero, a
    /// glitch may be the recorder's, not capture's.
    gaps: usize,
}

#[derive(Clone, Copy)]
enum Source {
    /// The recording side of the virtual cable (what RustDesk would read).
    Cable,
    /// Loopback of the tone device: what the user actually hears.
    Local,
    /// Raw process loopback excluding this PID's tree: exactly what capture receives.
    ProcessLoopback(u32),
}

/// Starts recording in a background thread; returns once the stream is running.
fn record(src: Source, ms: u64) -> JoinHandle<Recording> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let h = thread::spawn(move || {
        wasapi::init_com().unwrap();
        let mut cap = match src {
            Source::Cable => {
                let dev = wasapi::find_device(&cable_name(), eCapture).unwrap();
                CaptureStream::from_device(&dev, false, raw_buffer_ms()).unwrap()
            }
            Source::Local => {
                let dev = match tone_device() {
                    Some(n) => wasapi::find_device(&n, eRender).unwrap(),
                    None => wasapi::default_device(eRender).unwrap(),
                };
                CaptureStream::from_device(&dev, true, raw_buffer_ms()).unwrap()
            }
            Source::ProcessLoopback(pid) => CaptureStream::process_loopback(pid, false, raw_buffer_ms(), SAMPLE_RATE).unwrap(),
        };
        ready_tx.send(()).unwrap();
        let mut samples: Vec<f32> = Vec::new();
        let mut start_qpc: Option<u64> = None;
        let mut gaps = 0usize;
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
                    if base != p.qpc_100ns {
                        gaps += 1;
                    }
                }
                if p.flags & wasapi::BUFFERFLAGS_DISCONTINUITY != 0 && base != p.qpc_100ns {
                    gaps += 1;
                }
                samples.extend(p.samples.chunks(CHANNELS).map(|f| f.iter().sum::<f32>() / CHANNELS as f32));
            })
            .unwrap();
        }
        Recording { samples, start_qpc: start_qpc.unwrap_or(0), gaps }
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).expect("recorder did not start");
    h
}

/// Records the cable and the local output at the same time.
fn record_both(ms: u64) -> (JoinHandle<Recording>, JoinHandle<Recording>) {
    (record(Source::Cable, ms), record(Source::Local, ms))
}

/// Records the cable and the local output for `ms`, repeating the measurement (up to 3
/// times) if a *recorder* had gaps: then the instrument glitched, not capture.
fn record_both_valid(label: &str, ms: u64) -> (Recording, Recording) {
    for attempt in 1..=3 {
        let (c, l) = record_both(ms);
        let (c, l) = (c.join().unwrap(), l.join().unwrap());
        if c.gaps == 0 {
            return (c, l);
        }
        println!("[{label}] attempt {attempt}: cable recorder had {} gaps; measurement discarded", c.gaps);
    }
    panic!("[{label}] cable recorder kept having gaps");
}

fn levels(r: &Recording) -> (f32, f32) {
    // +-0.6 %: capture may run its drift correction (at most 0.5 %) after an audio loss;
    // for the excluded tone, searching a band only makes the check stricter
    (tone_level_db_near(&r.samples, RATE, F_KEPT, 0.006), tone_level_db_near(&r.samples, RATE, F_EXCLUDED, 0.006))
}

/// Records 1 s and checks: the excluded tone is audible locally but ≥40 dB below the kept
/// tone in the cable, and the kept tone is present in the cable.
fn assert_excluded(label: &str) {
    let (cable, local) = record_both_valid(label, 1000);
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
    let (cable, local) = record_both_valid("t2 control", 1000);
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
    // AUDIO_EXCLUDE_T6_DELAY_MS: time between the burst's stream opening and the burst
    let delay: u64 = std::env::var("AUDIO_EXCLUDE_T6_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let (d, dur) = (delay.to_string(), (delay + 1000).to_string());
    for round in 0..3 {
        let (cable, local) = record_both(delay + 1300);
        let _b = start_tone(&tone_exe(), F_EXCLUDED, &["--delay", &d, "--burst", "30", "--duration", &dur]);
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

/// Copies tone.exe to `<tmp>/<dir>/same_name.exe`.
fn same_name_copy(dir: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(dir);
    std::fs::create_dir_all(&d).unwrap();
    let dst = d.join("same_name.exe");
    if let Err(e) = std::fs::copy(tone_exe(), &dst) {
        assert!(dst.exists(), "copy tone.exe -> {dst:?}: {e}");
    }
    dst
}

/// 7. Exclusion by full path: two programs with the same file name in different folders;
/// only the one whose path is given is excluded (by-name matching could not tell them
/// apart). Then the excluded program restarts and is still excluded.
#[test]
fn t7_exclusion_by_path() {
    let _g = serial();
    let excluded = same_name_copy("path_a");
    let kept = same_name_copy("path_b");
    let a = start_tone(&excluded, F_EXCLUDED, &["--duration", "15000"]);
    let _b = start_tone(&kept, F_KEPT, &["--duration", "15000"]);
    let path = excluded.to_str().unwrap();
    let mut cap = start_capture(&["--exclude-path", path]);
    cap.wait_target(a.pid(), Duration::from_secs(1));
    thread::sleep(Duration::from_millis(300));
    assert_excluded("t7 exclude path");

    let old = a.pid();
    drop(a);
    let a2 = start_tone(&excluded, F_EXCLUDED, &["--duration", "15000"]);
    cap.wait_target(a2.pid(), Duration::from_secs(3));
    println!("[t7] re-targeted {old} -> {} after restart", a2.pid());
    thread::sleep(Duration::from_millis(300));
    assert_excluded("t7 after restart");
}

// ---------------------------------------------------------------------------
// tray app
// ---------------------------------------------------------------------------

mod tray_win {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE, WM_COMMAND};

    /// The tray's hidden top-level window.
    pub fn find() -> Option<HWND> {
        unsafe { FindWindowW(w!("audio-exclude-tray"), None).ok() }
    }
    /// Same command IDs as the tray menu (see src/bin/tray.rs).
    pub const ID_NONE: usize = 1;
    pub const ID_PROGRAM: usize = 1000;
    pub fn command(h: HWND, id: usize) {
        unsafe { PostMessageW(Some(h), WM_COMMAND, WPARAM(id), LPARAM(0)).unwrap() }
    }
    /// Simulates a right click on the tray icon (opens the real popup menu), then
    /// cancels the menu. Exercises menu construction; the click itself is not automated.
    pub fn open_and_cancel_menu(h: HWND) {
        const WM_TRAY: u32 = 0x8000 + 1; // WM_APP + 1, see tray.rs
        const WM_RBUTTONUP: isize = 0x0205;
        const WM_CANCELMODE: u32 = 0x001F;
        unsafe {
            PostMessageW(Some(h), WM_TRAY, WPARAM(0), LPARAM(WM_RBUTTONUP)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(700));
            let _ = PostMessageW(Some(h), WM_CANCELMODE, WPARAM(0), LPARAM(0));
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    pub fn close(h: HWND) {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn wait_config(path: &Path, want_selected: Option<&str>) -> Config {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let c = Config::load(path).unwrap();
        if c.selected.as_deref() == want_selected || Instant::now() > deadline {
            return c;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// 8. Tray app: restores a saved config on start (excludes the saved program by path),
/// and switching the selection (same command IDs the menu sends) takes effect
/// immediately and is written back to the config file. The menu itself and the file
/// picker are not automated.
#[test]
fn t8_tray_restores_config_and_switches() {
    let _g = serial();
    assert!(tray_win::find().is_none(), "a tray instance is already running; quit it before running the tests");
    let excluded = same_name_copy("tray_a");
    let excluded_s = excluded.to_str().unwrap().to_string();
    let cfg_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tray_cfg").join("config.json");
    let mut cfg = Config { output: cable_name(), ..Config::default() };
    cfg.add_program(&excluded_s);
    cfg.select(Some(&excluded_s));
    cfg.save(&cfg_path).unwrap();

    let a = start_tone(&excluded, F_EXCLUDED, &["--duration", "15000"]);
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "15000"]);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tray"));
    cmd.env("AUDIO_EXCLUDE_CONFIG", &cfg_path);
    let mut tray = Proc::spawn_cmd(cmd);
    let deadline = Instant::now() + Duration::from_secs(5);
    let h = loop {
        if let Some(h) = tray_win::find() {
            break h;
        }
        assert!(Instant::now() < deadline, "tray window never appeared (exit: {:?})", tray.child.try_wait());
        thread::sleep(Duration::from_millis(50));
    };
    thread::sleep(Duration::from_millis(1000));
    assert_excluded("t8 restored from config");
    tray_win::open_and_cancel_menu(h);
    assert!(tray.child.try_wait().unwrap().is_none(), "tray exited while building/showing its menu");

    tray_win::command(h, tray_win::ID_NONE);
    assert_eq!(wait_config(&cfg_path, None).selected, None, "selection not saved");
    thread::sleep(Duration::from_millis(500));
    let (cable, _local) = record_both(1000);
    let (k, x) = levels(&cable.join().unwrap());
    println!("[t8 switched to none] cable: {F_KEPT} Hz {k:.1} dB, {F_EXCLUDED} Hz {x:.1} dB");
    assert!(k > PRESENT_DB && x > PRESENT_DB, "after selecting 'exclude nothing' both tones must pass");

    tray_win::command(h, tray_win::ID_PROGRAM);
    let c = wait_config(&cfg_path, Some(&excluded_s));
    assert_eq!(c.selected.as_deref(), Some(excluded_s.as_str()), "selection not saved");
    thread::sleep(Duration::from_millis(500));
    assert_excluded("t8 switched back");

    tray_win::close(h);
    let t = Instant::now();
    while tray.child.try_wait().unwrap().is_none() && t.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(tray.child.try_wait().unwrap().is_some(), "tray did not exit on WM_CLOSE");
    drop(a);
}

fn next_output_event(rx: &Receiver<Event>, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Output(r)) => return r,
            Ok(_) => {}
            Err(_) => panic!("no output event within {timeout:?}"),
        }
    }
}

/// 9. Output errors don't end the engine: a missing device and a device held
/// exclusively by another program are reported, and the engine recovers by itself once
/// the device is free (this is what the tray turns into notifications).
#[test]
fn t9_engine_retries_output() {
    let _g = serial();
    let (tx, rx) = mpsc::channel();
    // Not Target::None: that excludes the engine's own process tree, which here is the
    // test process and therefore includes the tone helpers.
    let dummy = start_dummy();
    let engine = Engine::start(Target::Pid(dummy.pid()), "No Such Output Device 1234".into(), move |e| {
        let _ = tx.send(e);
    });
    let e1 = next_output_event(&rx, Duration::from_secs(3)).unwrap_err();
    println!("[t9] missing device -> {e1}");

    let blocker = start_capture(&[]); // another process holds the cable exclusively
    engine.set_output(cable_name());
    let e2 = next_output_event(&rx, Duration::from_secs(3)).unwrap_err();
    println!("[t9] device busy -> {e2}");
    assert!(e2.contains("in use"), "expected an 'in use' error, got {e2}");

    let freed = Instant::now();
    drop(blocker);
    let ok = next_output_event(&rx, Duration::from_secs(6)).expect("engine did not recover");
    println!("[t9] recovered {:?} after the device was freed: {ok}", freed.elapsed());
    thread::sleep(Duration::from_millis(300));
    let _b = start_tone(&tone_exe(), F_KEPT, &["--duration", "3000"]);
    thread::sleep(Duration::from_millis(200));
    let (cable, _local) = record_both(800);
    let (k, _) = levels(&cable.join().unwrap());
    println!("[t9] audio after recovery: {F_KEPT} Hz {k:.1} dB");
    assert!(k > PRESENT_DB, "no audio after recovery");
    drop(engine);
}

/// Glitches in a recording of one pure sine of frequency `freq`: times (ms from the
/// start of `x`) where the 2nd-order sine predictor residual
/// `x[n] - 2cos(w)x[n-1] + x[n-2]` (≈0 for a clean sine) exceeds `thresh`. Hits closer
/// than 5 ms are merged into one event. Returns (event times, residual floor in dB).
fn sine_glitches(x: &[f32], freq: f32, thresh: f32) -> (Vec<f32>, f32) {
    let c = 2.0 * (2.0 * std::f32::consts::PI * freq / RATE).cos();
    let mut events: Vec<f32> = Vec::new();
    let mut last: Option<usize> = None;
    let mut sum = 0.0f64;
    for n in 2..x.len() {
        let r = (x[n] - c * x[n - 1] + x[n - 2]).abs();
        sum += (r as f64) * (r as f64);
        if r > thresh {
            // (the first exceedance is an event too; an earlier version missed it)
            if last.is_none_or(|l| n - l > RATE as usize / 200) {
                events.push(n as f32 * 1000.0 / RATE);
            }
            last = Some(n);
        }
    }
    let floor = audio_exclude::dsp::to_db((sum / x.len().max(1) as f64).sqrt() as f32);
    (events, floor)
}

/// The 5 largest sine-predictor residuals (one per 5 ms), as "time ms:value", time
/// offset by `offset_ms`: shows near-misses below the glitch threshold too.
fn top_residuals(x: &[f32], freq: f32, offset_ms: f32) -> String {
    let c = 2.0 * (2.0 * std::f32::consts::PI * freq / RATE).cos();
    let win = RATE as usize / 200;
    let mut peaks: Vec<(f32, usize)> = (2..x.len())
        .collect::<Vec<_>>()
        .chunks(win)
        .map(|ch| ch.iter().map(|&n| ((x[n] - c * x[n - 1] + x[n - 2]).abs(), n)).fold((0.0, 0), |m, v| if v.0 > m.0 { v } else { m }))
        .collect();
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    peaks.iter().take(5).map(|(v, n)| format!("{:.0}:{v:.4}", *n as f32 * 1000.0 / RATE + offset_ms)).collect::<Vec<_>>().join(" ")
}

/// 10. No clicks/dropouts in the first seconds after capture starts (and afterwards):
/// a continuous 1000 Hz sine plays, capture starts while the cable is being recorded,
/// and the cable waveform is checked sample by sample for discontinuities.
///
/// Reference: Windows' process loopback itself delivers a tiny step whenever any
/// process on the system opens or closes an audio stream (see diag_loopback_vs_other_
/// streams). Such steps are recorded by a second, raw process-loopback recorder; a cable
/// glitch only counts against us if the raw loopback has no step 0..60 ms before it.
#[test]
fn t10_no_glitches_after_start() {
    let _g = serial();
    for attempt in 1..=3 {
        let dummy = start_dummy();
        let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "9000"]);
        thread::sleep(Duration::from_millis(300));
        let cable = record(Source::Cable, 6000);
        let raw = record(Source::ProcessLoopback(dummy.pid()), 6000);
        thread::sleep(Duration::from_millis(300));
        let pid = dummy.pid().to_string();
        let cap = start_capture(&["--exclude-pid", &pid]);
        let (cable, raw) = (cable.join().unwrap(), raw.join().unwrap());
        drop(cap);
        if cable.gaps > 0 || raw.gaps > 0 {
            println!("[t10] attempt {attempt}: a recorder had gaps (system hiccup); measurement discarded");
            continue;
        }

        // threshold: 5% of the sine amplitude (0.1); a dropped or repeated sample of a
        // 1 kHz sine gives a residual of up to ~2x the amplitude
        let thresh = 0.005;
        let cable_al = align_to(&cable, &raw); // both on the raw recording's timeline
        let onset = cable_al.iter().position(|v| v.abs() > 0.01).expect("sine never reached the cable");
        let onset_ms = onset as f32 * 1000.0 / RATE;
        let raw_onset = raw.samples.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
        let (rg, rfloor) = sine_glitches(&raw.samples[raw_onset..], F_EXCLUDED, thresh);
        let rg: Vec<f32> = rg.iter().map(|t| t + raw_onset as f32 * 1000.0 / RATE).filter(|&t| t > raw_onset as f32 * 1000.0 / RATE + 2.0).collect();
        let (cg, cfloor) = sine_glitches(&cable_al[onset..], F_EXCLUDED, thresh);
        // the first sample after onset is a hard start (silence -> running sine, faded in)
        let cg: Vec<f32> = cg.iter().filter(|&&t| t > 2.0).map(|t| t + onset_ms).collect();
        let ours: Vec<f32> = cg.iter().cloned().filter(|&t| !rg.iter().any(|&r| r <= t + 5.0 && r >= t - 60.0)).collect();
        let early = ours.iter().filter(|&&t| t < onset_ms + 3000.0).count();
        let report = format!(
            "[t10 glitches] raw process loopback (reference): {} steps {:?}, floor {rfloor:.1} dB, top residuals {} | cable: onset at {onset_ms:.0} ms, {} events {:?}, floor {cfloor:.1} dB, top residuals {} | not explained by the reference: {} ({} in the first 3 s) | times relative to qpc {:.1} ms",
            rg.len(),
            &rg[..rg.len().min(8)],
            top_residuals(&raw.samples[raw_onset + 96..], F_EXCLUDED, (raw_onset + 96) as f32 * 1000.0 / RATE),
            cg.len(),
            &cg[..cg.len().min(8)],
            top_residuals(&cable_al[onset + 96..], F_EXCLUDED, (onset + 96) as f32 * 1000.0 / RATE),
            ours.len(),
            early,
            raw.start_qpc as f64 / 1e4
        );
        println!("{report}");
        assert!(ours.is_empty(), "{} glitches introduced by capture after it started: {:?}\n{report}", ours.len(), &ours[..ours.len().min(20)]);
        return;
    }
    panic!("no valid measurement in 3 attempts (recorders kept having gaps)");
}

/// Diagnostic (ignored): glitches at the start of (a) a tone played straight into the
/// cable, recorded from the cable, and (b) raw process-loopback capture.
#[test]
#[ignore]
fn diag_glitch_sources() {
    let _g = serial();
    // (a) VAC + recorder alone
    let cable = record(Source::Cable, 4000);
    thread::sleep(Duration::from_millis(300));
    let c = cable_name();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--device", &c, "--duration", "3500"]);
    let rec = cable.join().unwrap();
    let onset = rec.samples.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
    let (ev, floor) = sine_glitches(&rec.samples[onset..], F_EXCLUDED, 0.005);
    println!("[diag a: tone -> cable directly] {} events {:?} floor {floor:.1} dB", ev.len(), &ev[..ev.len().min(10)]);
    drop(_t);
    thread::sleep(Duration::from_millis(300));

    // (b) raw process loopback (exclude a dummy), tone on the default device
    let dummy = start_dummy();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "5000"]);
    thread::sleep(Duration::from_millis(300));
    wasapi::init_com().unwrap();
    let mut cap = CaptureStream::process_loopback(dummy.pid(), false, raw_buffer_ms(), SAMPLE_RATE).unwrap();
    let mut samples = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(4000) {
        cap.wait(Duration::from_millis(50));
        cap.read(|p| samples.extend(p.samples.chunks(CHANNELS).map(|f| f[0]))).unwrap();
    }
    let onset = samples.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
    let (ev, floor) = sine_glitches(&samples[onset..], F_EXCLUDED, 0.005);
    println!("[diag b: raw process loopback] onset {onset} {} events {:?} floor {floor:.1} dB", ev.len(), &ev[..ev.len().min(10)]);
}

/// Diagnostic (ignored): a synthetic sine pushed straight into our exclusive Output
/// (no process loopback), recorded from the cable. Periods via AUDIO_EXCLUDE_PERIOD_MS.
#[test]
#[ignore]
fn diag_exclusive_output_alone() {
    let _g = serial();
    let cable = record(Source::Cable, 4500);
    thread::sleep(Duration::from_millis(300));
    let out = audio_exclude::output::Output::open(&cable_name(), 15).unwrap();
    let w = 2.0 * std::f64::consts::PI * F_EXCLUDED as f64 / out.rate as f64;
    let mut n: u64 = 0;
    let t0 = Instant::now();
    // push 10 ms packets on a 10 ms schedule, like process loopback does
    while t0.elapsed() < Duration::from_millis(4000) {
        let due = (t0.elapsed().as_secs_f64() * out.rate as f64) as u64;
        while n + 480 <= due + 480 {
            let mut pkt = Vec::with_capacity(480 * CHANNELS);
            for i in 0..480u64 {
                let v = (0.1 * (w * (n + i) as f64).sin()) as f32;
                pkt.extend([v, v]);
            }
            out.push(&pkt);
            n += 480;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let rec = cable.join().unwrap();
    let onset = rec.samples.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
    let (ev, floor) = sine_glitches(&rec.samples[onset..], F_EXCLUDED, 0.005);
    println!("[diag exclusive alone] {} | {} events {:?} floor {floor:.1} dB | {:?}", out.description, ev.len(), &ev[..ev.len().min(12)], out.stats());
}

/// Diagnostic (ignored): does another stream starting/stopping on the system disturb raw
/// process loopback (no part of our output involved)?
#[test]
#[ignore]
fn diag_loopback_vs_other_streams() {
    let _g = serial();
    let dummy = start_dummy();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "6000"]);
    thread::sleep(Duration::from_millis(300));
    wasapi::init_com().unwrap();
    let mut cap = CaptureStream::process_loopback(dummy.pid(), false, raw_buffer_ms(), SAMPLE_RATE).unwrap();
    let mut samples = Vec::new();
    let t = Instant::now();
    let mut other: Option<Proc> = None;
    let mut marks = Vec::new();
    while t.elapsed() < Duration::from_millis(5000) {
        let ms = t.elapsed().as_millis();
        if ms > 1000 && other.is_none() && marks.is_empty() {
            marks.push(("start", ms));
            // a silent stream (freq 0) opened by another process
            other = Some(start_tone(&tone_exe(), 0.0, &["--duration", "1500"]));
        }
        if ms > 3500 && marks.len() == 1 {
            marks.push(("ended by now", ms));
            other = None;
        }
        cap.wait(Duration::from_millis(50));
        cap.read(|p| samples.extend(p.samples.chunks(CHANNELS).map(|f| f[0]))).unwrap();
    }
    let onset = samples.iter().position(|v| v.abs() > 0.01).unwrap_or(0);
    let (ev, floor) = sine_glitches(&samples[onset..], F_EXCLUDED, 0.005);
    println!("[diag c: loopback while another stream starts/stops] marks {marks:?} | {} events {:?} floor {floor:.1} dB", ev.len(), &ev[..ev.len().min(10)]);
    drop(other);
}

/// Diagnostic (ignored): does opening a capture device make some other program (e.g. an
/// antivirus microphone alert) play a sound? No test tone plays; the raw process
/// loopback is watched while the cable's recording side is opened at ~1 s by this
/// process and at ~3 s by a fresh child process.
#[test]
#[ignore]
fn diag_sound_on_capture_open() {
    let _g = serial();
    let dummy = start_dummy();
    let raw = record(Source::ProcessLoopback(dummy.pid()), 6000);
    let local = record(Source::Local, 6000);
    thread::sleep(Duration::from_millis(1000));
    let c1 = record(Source::Cable, 500);
    thread::sleep(Duration::from_millis(2000));
    // a new process opening a capture device: capture.exe itself only opens loopback, so
    // use a second copy of this test binary? Simpler: capture.exe (process loopback).
    let pid = dummy.pid().to_string();
    let cap = start_capture(&["--exclude-pid", &pid]);
    let (raw, local) = (raw.join().unwrap(), local.join().unwrap());
    drop(c1.join());
    drop(cap);
    let env = |r: &Recording| -> String {
        r.samples.chunks(RATE as usize / 10).map(|c| format!("{:.0}", audio_exclude::dsp::rms_db(c))).collect::<Vec<_>>().join(" ")
    };
    println!("[diag alert] raw loopback 100 ms RMS: {}", env(&raw));
    println!("[diag alert] local output 100 ms RMS: {}", env(&local));
}

/// Diagnostic (ignored): the real volume-preview "ding" (the `.Default` system sound,
/// `AUDIO_EXCLUDE_DING` or `C:\Windows\Media\Windows Background.wav`) after 5 s of
/// silence. Records the cable and the raw process loopback, aligns them sample-exactly
/// and reports peaks, clipped samples, the cable-minus-reference residual, and whether
/// the cable carries exact digital zeros while nothing plays.
#[test]
#[ignore]
fn diag_ding() {
    let _g = serial();
    let wav = std::env::var("AUDIO_EXCLUDE_DING").unwrap_or_else(|_| r"C:\Windows\Media\Windows Background.wav".into());
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    // AUDIO_EXCLUDE_USE_RUNNING=1: measure an already running capture/tray instead
    let cap = std::env::var_os("AUDIO_EXCLUDE_USE_RUNNING").is_none().then(|| start_capture(&["--exclude-pid", &pid]));
    for round in 1..=3 {
        thread::sleep(Duration::from_secs(5));
        let cable = record(Source::Cable, 4500);
        let raw = record(Source::ProcessLoopback(dummy.pid()), 4500);
        thread::sleep(Duration::from_millis(1000));
        let mut ps = Command::new("powershell");
        ps.args(["-NoProfile", "-Command", &format!("(New-Object Media.SoundPlayer '{wav}').PlaySync()")]);
        let st = ps.status().unwrap();
        assert!(st.success());
        let (cable, raw) = (cable.join().unwrap(), raw.join().unwrap());
        let cable_al = align_to(&cable, &raw);
        // silence before the ding: exact zeros?
        let pre = &cable_al[..(RATE as usize * 8 / 10).min(cable_al.len())];
        let pre_nonzero = pre.iter().filter(|v| **v != 0.0).count();
        let pre_max = pre.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let clipped = |x: &[f32]| x.iter().filter(|v| v.abs() >= 0.9999).count();
        // refine alignment by cross-correlation around the ding
        let Some(r_on) = raw.samples.iter().position(|v| v.abs() > 0.001) else {
            println!("[diag ding {round}] ding not in process loopback");
            continue;
        };
        let Some(c_on) = cable_al.iter().position(|v| v.abs() > 0.001) else {
            println!("[diag ding {round}] ding not in cable");
            continue;
        };
        let start = r_on.saturating_sub(480);
        let n = (RATE as usize * 2).min(raw.samples.len() - start);
        let reference = &raw.samples[start..start + n];
        let delayed = &cable_al[start..(start + n + 9600).min(cable_al.len())];
        let (lag, corr) = xcorr_lag(reference, delayed, 4800);
        let m = n.min(delayed.len() - lag);
        let (num, den) = (0..m).fold((0.0f64, 0.0f64), |(a, b), i| (a + (delayed[i + lag] * reference[i]) as f64, b + (reference[i] * reference[i]) as f64));
        let gain = (num / den) as f32;
        let res: Vec<f32> = (0..m).map(|i| delayed[i + lag] - gain * reference[i]).collect();
        let sig_rms = audio_exclude::dsp::rms_db(&reference[..m]);
        let res_rms = audio_exclude::dsp::rms_db(&res);
        let res_peak = peak(&res);
        // where is the residual largest (10 ms blocks above -60 dB)
        let bad: Vec<String> = res
            .chunks(480)
            .enumerate()
            .filter(|(_, c)| audio_exclude::dsp::rms_db(c) > -60.0)
            .map(|(i, c)| format!("{}ms:{:.0}", i * 10, audio_exclude::dsp::rms_db(c)))
            .take(12)
            .collect();
        println!(
            "[diag ding {round}] silence before: {pre_nonzero} non-zero samples (max {pre_max:e}) | raw peak {:.4} ({:.1} dBFS), clipped {} | cable peak {:.4}, clipped {} | lag {:.2} ms (cable after raw {:.1} ms), corr {corr:.5}, gain {gain:.4} | signal {sig_rms:.1} dB, residual {res_rms:.1} dB (peak {res_peak:.4}), loud residual blocks {bad:?} | recorder gaps cable {} raw {}",
            peak(&raw.samples),
            audio_exclude::dsp::to_db(peak(&raw.samples)),
            clipped(&raw.samples),
            peak(&cable_al),
            clipped(&cable_al),
            lag as f32 * 1000.0 / RATE,
            (c_on as f32 - r_on as f32) * 1000.0 / RATE,
            cable.gaps,
            raw.gaps
        );
    }
    drop(cap);
}

/// Cable glitches that the raw process-loopback reference does not explain (cable on the
/// raw recording's timeline; a reference step 0..60 ms earlier explains a cable glitch).
/// Returns (unexplained glitch times in ms, report line).
fn unexplained_glitches(cable: &Recording, raw: &Recording, freq: f32) -> (Vec<f32>, String) {
    let thresh = 0.005;
    let cable_al = align_to(cable, raw);
    let onset = cable_al.iter().position(|v| v.abs() > 0.01).expect("sine never reached the cable");
    let raw_onset = raw.samples.iter().position(|v| v.abs() > 0.01).expect("sine never reached process loopback");
    // Only judge the cable while the source is still playing: the sine's own (abrupt)
    // end is not a glitch, and the reference may miss its last packet.
    let raw_end = raw.samples.iter().rposition(|v| v.abs() > 0.01).unwrap_or(raw.samples.len());
    let cable_end = (raw_end.saturating_sub(RATE as usize / 200)).clamp(onset, cable_al.len());
    let ms = |i: usize| i as f32 * 1000.0 / RATE;
    let (rg, rfloor) = sine_glitches(&raw.samples[raw_onset..raw_end], freq, thresh);
    let rg: Vec<f32> = rg.iter().filter(|&&t| t > 2.0).map(|t| t + ms(raw_onset)).collect();
    let (cg, cfloor) = sine_glitches(&cable_al[onset..cable_end], freq, thresh);
    let cg: Vec<f32> = cg.iter().filter(|&&t| t > 2.0).map(|t| t + ms(onset)).collect();
    let ours: Vec<f32> = cg.iter().cloned().filter(|&t| !rg.iter().any(|&r| r <= t + 5.0 && r >= t - 60.0)).collect();
    let first = cable_al[onset..].iter().take(RATE as usize / 1000).fold(0.0f32, |m, v| m.max(v.abs()));
    let report = format!(
        "raw loopback: {} steps, floor {rfloor:.1} dB, top residuals {} | cable: starts {:.1} ms after raw loopback, peak in its first 1 ms {first:.3} (sine amplitude 0.1), {} events {:?}, floor {cfloor:.1} dB, top residuals {} | unexplained: {} {:?} | times relative to qpc {:.1} ms",
        rg.len(),
        top_residuals(&raw.samples[(raw_onset + 96).min(raw_end)..raw_end], freq, ms(raw_onset + 96)),
        ms(onset) - ms(raw_onset),
        cg.len(),
        &cg[..cg.len().min(8)],
        top_residuals(&cable_al[(onset + 96).min(cable_end)..cable_end], freq, ms(onset + 96)),
        ours.len(),
        &ours[..ours.len().min(8)],
        raw.start_qpc as f64 / 1e4
    );
    (ours, report)
}

/// 11. Silence -> sound, like the volume-preview "ding": capture has been running while
/// nothing played for 5 s (process loopback idles and delivers no packets), then a
/// sine starts. The cable must carry it without glitches from the first sample on.
#[test]
fn t11_sound_after_silence() {
    let _g = serial();
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    let _cap = start_capture(&["--exclude-pid", &pid]);
    for attempt in 1..=3 {
        thread::sleep(Duration::from_secs(5)); // nothing plays: loopback goes idle
        let cable = record(Source::Cable, 2500);
        let raw = record(Source::ProcessLoopback(dummy.pid()), 2500);
        thread::sleep(Duration::from_millis(200));
        let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "1800"]);
        let (cable, raw) = (cable.join().unwrap(), raw.join().unwrap());
        if cable.gaps > 0 {
            println!("[t11] attempt {attempt}: cable recorder had gaps; measurement discarded");
            continue;
        }
        let (ours, report) = unexplained_glitches(&cable, &raw, F_EXCLUDED);
        println!("[t11 silence -> sound] {report}");
        assert!(ours.is_empty(), "{} glitches in the cable after silence -> sound: {ours:?}\n{report}", ours.len());
        return;
    }
    panic!("no valid measurement in 3 attempts");
}

/// 12. Silence dither (`--silence-dither`): while nothing plays, the cable must never
/// carry exact digital zeros (receivers such as RustDesk's encoder pause after a run of
/// all-zero frames), yet stay inaudible (< -80 dBFS); real audio still passes cleanly.
/// Without the switch the cable stays at exact zeros (unchanged default behaviour).
#[test]
fn t12_silence_dither() {
    let _g = serial();
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    for dither in [false, true] {
        let mut args = vec!["--exclude-pid", pid.as_str()];
        if dither {
            args.push("--silence-dither");
        }
        let cap = start_capture(&args);
        thread::sleep(Duration::from_secs(1)); // nothing plays
        // a recorder-side gap would insert zeros of its own: measure again (up to 3 times)
        let rec = (1..=3)
            .map(|_| record(Source::Cable, 2000).join().unwrap())
            .find(|r| r.gaps == 0)
            .expect("cable recorder had gaps in 3 attempts");
        let s = &rec.samples[RATE as usize / 10..]; // skip the recorder's start-up
        let blocks: Vec<&[f32]> = s.chunks_exact(RATE as usize / 100).collect();
        let zero_samples = s.iter().filter(|v| **v == 0.0).count();
        let all_zero_blocks = blocks.iter().filter(|b| b.iter().all(|v| *v == 0.0)).count();
        let rms = audio_exclude::dsp::rms_db(s);
        let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        println!(
            "[t12 dither {}] {} samples: {zero_samples} exact zeros, {all_zero_blocks}/{} all-zero 10 ms blocks, rms {rms:.1} dBFS, peak {:.1} dBFS",
            if dither { "on" } else { "off" },
            s.len(),
            blocks.len(),
            audio_exclude::dsp::to_db(peak)
        );
        if dither {
            assert_eq!(all_zero_blocks, 0, "no 10 ms block may be all zeros");
            assert_eq!(zero_samples, 0, "no sample may be an exact zero");
            assert!(rms < -80.0 && audio_exclude::dsp::to_db(peak) < -80.0, "dither must be inaudible: rms {rms:.1}, peak {peak}");
            // and real audio still passes without glitches
            let cable = record(Source::Cable, 2500);
            let raw = record(Source::ProcessLoopback(dummy.pid()), 2500);
            thread::sleep(Duration::from_millis(200));
            let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "1800"]);
            let (cable, raw) = (cable.join().unwrap(), raw.join().unwrap());
            let (ours, report) = unexplained_glitches(&cable, &raw, F_EXCLUDED);
            println!("[t12 dither on, tone] {report}");
            assert!(ours.is_empty(), "glitches with dither on: {ours:?}\n{report}");
        } else {
            assert_eq!(zero_samples, s.len(), "without the switch silence stays exact zeros");
        }
        drop(cap);
        thread::sleep(Duration::from_millis(300));
    }
}

/// Diagnostic (ignored): a continuous 1000 Hz tone for `AUDIO_EXCLUDE_LONG_MS` (default
/// 20000) ms through capture; counts cable glitches the raw loopback doesn't explain.
/// Run with AUDIO_EXCLUDE_STATS=1 to see capture's per-second underruns.
#[test]
#[ignore]
fn diag_long_run() {
    let _g = serial();
    let ms: u64 = std::env::var("AUDIO_EXCLUDE_LONG_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(20000);
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    let _cap = start_capture(&["--exclude-pid", &pid]);
    let dur = (ms + 3000).to_string();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", &dur]);
    thread::sleep(Duration::from_millis(500));
    let cable = record(Source::Cable, ms);
    let raw = record(Source::ProcessLoopback(dummy.pid()), ms);
    // AUDIO_EXCLUDE_INJECT=1: every 1.5 s another process opens a (silent) stream for 0.5 s
    let mut opened = Vec::new();
    if std::env::var_os("AUDIO_EXCLUDE_INJECT").is_some() {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms.saturating_sub(1500)) {
            thread::sleep(Duration::from_millis(1000));
            opened.push(t0.elapsed().as_millis());
            let _o = start_tone(&tone_exe(), 0.0, &["--duration", "500"]);
            thread::sleep(Duration::from_millis(500));
        }
    }
    let (cable, raw) = (cable.join().unwrap(), raw.join().unwrap());
    let (ours, report) = unexplained_glitches(&cable, &raw, F_EXCLUDED);
    println!("[diag long run] other streams opened at {opened:?} ms");
    println!("[diag long run {ms} ms] recorder gaps cable {} raw {} | {report}", cable.gaps, raw.gaps);
    println!("[diag long run] unexplained glitches: {} = {:.1} per minute, at {:?}", ours.len(), ours.len() as f64 * 60000.0 / ms as f64, ours);
}

/// Diagnostic (ignored): per-packet log of raw process loopback while another process
/// opens and closes a stream: arrival time, QPC timestamp, frames, flags. Shows whether
/// such events lose audio (timestamp jumps) or only delay delivery.
#[test]
#[ignore]
fn diag_packets_on_stream_open() {
    let _g = serial();
    let dummy = start_dummy();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "6000"]);
    thread::sleep(Duration::from_millis(300));
    wasapi::init_com().unwrap();
    let mut cap = CaptureStream::process_loopback(dummy.pid(), false, raw_buffer_ms(), SAMPLE_RATE).unwrap();
    let t0 = Instant::now();
    let mut log: Vec<(f64, u64, usize, u32)> = Vec::new();
    let mut samples: Vec<f32> = Vec::new();
    let mut other: Option<Proc> = None;
    let mut marks = Vec::new();
    while t0.elapsed() < Duration::from_millis(3000) {
        let ms = t0.elapsed().as_millis();
        if ms > 1000 && marks.is_empty() {
            marks.push(("open", ms));
            other = Some(start_tone(&tone_exe(), 0.0, &["--duration", "800"]));
        }
        if ms > 2200 && marks.len() == 1 {
            marks.push(("closed by", ms));
            other = None;
        }
        cap.wait(Duration::from_millis(50));
        cap.read(|p| {
            log.push((t0.elapsed().as_secs_f64() * 1000.0, p.qpc_100ns, p.samples.len() / CHANNELS, p.flags));
            samples.extend(p.samples.chunks(CHANNELS).map(|f| f[0]));
        })
        .unwrap();
    }
    drop(other);
    println!("[diag packets] marks {marks:?}");
    let q0 = log[0].1;
    let mut frames_total = 0usize;
    for (i, (at, q, n, fl)) in log.iter().enumerate() {
        let dq = if i > 0 { (*q as f64 - log[i - 1].1 as f64) / 1e4 } else { 0.0 };
        let da = if i > 0 { at - log[i - 1].0 } else { 0.0 };
        let qms = (*q - q0) as f64 / 1e4;
        // data position vs timestamp: drift > 0 means the timestamp ran ahead of the data
        let drift = qms - frames_total as f64 * 1000.0 / RATE as f64;
        if dq > 10.5 || dq < 9.5 || da > 13.0 || *fl != 0 || drift.abs() > 1.0 {
            println!("  #{i} arrive {at:.1} (+{da:.1}) qpc +{qms:.1} (+{dq:.1}) frames {n} flags {fl:#x} ts-minus-data {drift:.1} ms");
        }
        frames_total += n;
    }
    let (ev, floor) = sine_glitches(&samples[RATE as usize / 100..], F_EXCLUDED, 0.005);
    println!("[diag packets] {} packets, data {:.1} ms vs timestamps {:.1} ms; contiguous-data glitches {} {:?} (floor {floor:.1} dB), top {}", log.len(), frames_total as f64 * 1000.0 / RATE as f64, (log.last().unwrap().1 - q0) as f64 / 1e4 + 10.0, ev.len(), ev, top_residuals(&samples[RATE as usize / 100..], F_EXCLUDED, 10.0));
}

/// A recording with its packet log, samples appended contiguously (no timestamp
/// placement): shows both lost audio (timestamp steps) and waveform jumps.
struct PacketRec {
    samples: Vec<f32>,
    /// (qpc 100 ns, frames, flags) per packet.
    log: Vec<(u64, usize, u32)>,
}

impl PacketRec {
    /// Packets whose timestamp is more than 1 ms past the end of the previous one:
    /// (sample index in `samples`, qpc ms, missing ms).
    fn timestamp_gaps(&self) -> Vec<(usize, f64, f64)> {
        let mut pos = 0usize;
        let mut out = Vec::new();
        for i in 0..self.log.len() {
            if i > 0 {
                let (q0, n0, _) = self.log[i - 1];
                let expect = q0 as f64 + n0 as f64 * 1e7 / RATE as f64;
                let missing = (self.log[i].0 as f64 - expect) / 1e4;
                if missing > 1.0 {
                    out.push((pos, self.log[i].0 as f64 / 1e4, missing));
                }
            }
            pos += self.log[i].1;
        }
        out
    }
    fn flagged(&self) -> usize {
        self.log.iter().skip(1).filter(|p| p.2 & wasapi::BUFFERFLAGS_DISCONTINUITY != 0).count()
    }
}

fn record_packets(src: Source, ms: u64) -> JoinHandle<PacketRec> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let h = thread::spawn(move || {
        wasapi::init_com().unwrap();
        let mut cap = match src {
            Source::Cable => CaptureStream::from_device(&wasapi::find_device(&cable_name(), eCapture).unwrap(), false, raw_buffer_ms()).unwrap(),
            Source::Local => {
                let dev = match tone_device() {
                    Some(n) => wasapi::find_device(&n, eRender).unwrap(),
                    None => wasapi::default_device(eRender).unwrap(),
                };
                CaptureStream::from_device(&dev, true, raw_buffer_ms()).unwrap()
            }
            Source::ProcessLoopback(pid) => CaptureStream::process_loopback(pid, false, raw_buffer_ms(), SAMPLE_RATE).unwrap(),
        };
        ready_tx.send(()).unwrap();
        let mut r = PacketRec { samples: Vec::new(), log: Vec::new() };
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            cap.wait(Duration::from_millis(50));
            cap.read(|p| {
                r.log.push((p.qpc_100ns, p.samples.len() / CHANNELS, p.flags));
                r.samples.extend(p.samples.chunks(CHANNELS).map(|f| f.iter().sum::<f32>() / CHANNELS as f32));
            })
            .unwrap();
        }
        r
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).expect("recorder did not start");
    h
}

/// Opens and closes a short shared-mode stream on the tone device `n` times, every
/// `every_ms`, like rapid clicks on the volume slider (each click plays a new ding).
/// `beep`: the stream plays a short 3 kHz blip instead of silence. Returns the open
/// times (qpc ms).
fn rapid_streams(n: usize, every_ms: u64, beep: bool) -> Vec<f64> {
    wasapi::init_com().unwrap();
    let dev = match tone_device() {
        Some(d) => wasapi::find_device(&d, eRender).unwrap(),
        None => wasapi::default_device(eRender).unwrap(),
    };
    let mut opened = Vec::new();
    for _ in 0..n {
        let t = Instant::now();
        opened.push(wasapi::qpc_now_100ns() as f64 / 1e4);
        let s = wasapi::RenderStream::open(&dev, 50).unwrap();
        let len = (RATE * 0.04) as usize;
        let buf: Vec<f32> = (0..len)
            .flat_map(|i| {
                let v = if beep { 0.05 * (2.0 * std::f32::consts::PI * 3000.0 * i as f32 / RATE).sin() } else { 0.0 };
                [v; CHANNELS]
            })
            .collect();
        s.write(&buf).unwrap();
        s.start().unwrap();
        thread::sleep(Duration::from_millis(60));
        drop(s);
        if let Some(rest) = Duration::from_millis(every_ms).checked_sub(t.elapsed()) {
            thread::sleep(rest);
        }
    }
    opened
}

/// Diagnostic (ignored): a continuous 1000 Hz sine plays; another stream is opened and
/// closed 20 times in a row (`AUDIO_EXCLUDE_EVERY_MS`, default 150). Raw process
/// loopback and device loopback of the tone device are recorded at the same time; for
/// each: timestamp gaps (lost audio), DATA_DISCONTINUITY flags, and waveform jumps.
#[test]
#[ignore]
fn diag_rapid_streams() {
    let _g = serial();
    let every: u64 = std::env::var("AUDIO_EXCLUDE_EVERY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(150);
    let dummy = start_dummy();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "12000"]);
    thread::sleep(Duration::from_millis(500));
    let ms = 20 * every + 2000;
    let raw = record_packets(Source::ProcessLoopback(dummy.pid()), ms);
    let dev = record_packets(Source::Local, ms);
    thread::sleep(Duration::from_millis(700));
    let opened = rapid_streams(20, every, std::env::var_os("AUDIO_EXCLUDE_BEEP").is_some());
    let (raw, dev) = (raw.join().unwrap(), dev.join().unwrap());
    println!("[diag rapid] streams opened every {every} ms at qpc {:.0}..{:.0} ms", opened[0], opened[opened.len() - 1]);
    for (name, r) in [("process loopback", &raw), ("device loopback", &dev)] {
        let skip = RATE as usize / 10;
        let (ev, floor) = sine_glitches(&r.samples[skip..], F_EXCLUDED, 0.005);
        let gaps = r.timestamp_gaps();
        let jumps_at_gaps = gaps.iter().filter(|(p, _, _)| ev.iter().any(|&t| ((t * RATE / 1000.0) as isize + skip as isize - *p as isize).abs() < 48)).count();
        println!(
            "[diag rapid] {name}: {} packets, {} DATA_DISCONTINUITY flags, {} timestamp gaps (missing ms {:?}), {} waveform jumps (floor {floor:.1} dB), {jumps_at_gaps} of the gaps have a jump within 1 ms",
            r.log.len(),
            r.flagged(),
            gaps.len(),
            gaps.iter().map(|g| (g.2 * 10.0).round() / 10.0).collect::<Vec<_>>(),
            ev.len(),
        );
        println!("[diag rapid] {name} top residuals {}", top_residuals(&r.samples[skip..], F_EXCLUDED, 0.0));
        let (ev, _) = sine_glitches(&r.samples[skip..], F_EXCLUDED, 0.005);
        let pos: Vec<f32> = gaps.iter().map(|g| g.0 as f32 * 1000.0 / RATE).collect();
        println!("[diag rapid] {name} jumps at {:?}
   gaps at {:?}", &ev[..ev.len().min(12)].iter().map(|t| t + 100.0).collect::<Vec<_>>(), &pos[..pos.len().min(6)]);
    }
    // the raw packets through the timeline logic (offline), like the engine does
    let mut tl = audio_exclude::timeline::Timeline::new(SAMPLE_RATE);
    let mut out: Vec<f32> = Vec::new();
    let mut i = 0;
    for &(q, n, _) in &raw.log {
        let pkt: Vec<f32> = raw.samples[i..i + n].iter().flat_map(|v| [*v; CHANNELS]).collect();
        let (hole, s) = tl.packet(&pkt, q);
        let mut tail: Vec<f32> = out[out.len().saturating_sub(hole.fade_out)..].iter().flat_map(|v| [*v; CHANNELS]).collect();
        audio_exclude::timeline::fade_out_tail(&mut tail, hole.fade_out);
        let at = out.len() - tail.len() / CHANNELS;
        out.truncate(at);
        out.extend(tail.chunks(CHANNELS).map(|f| f[0]));
        out.extend(std::iter::repeat(0.0).take(hole.frames));
        out.extend(s.chunks(CHANNELS).map(|f| f[0]));
        i += n;
    }
    let (ev, floor) = sine_glitches(&out[RATE as usize / 10..], F_EXCLUDED, 0.005);
    println!("[diag rapid] timeline offline: {} holes, {} cuts faded, {} hard steps left {:?} (floor {floor:.1} dB), top residuals {}", tl.holes, tl.cuts, ev.len(), &ev[..ev.len().min(8)], top_residuals(&out[RATE as usize / 10..], F_EXCLUDED, 100.0));
    for &t in ev.iter().take(3) {
        let c = ((t + 100.0) * RATE / 1000.0) as usize;
        let w: Vec<String> = out[c.saturating_sub(6)..(c + 6).min(out.len())].iter().map(|v| format!("{v:.4}")).collect();
        println!("   around {:.1} ms: {}", t + 100.0, w.join(" "));
    }
    // packets around the first two holes
    let mut pos = 0usize;
    let g: Vec<usize> = raw.timestamp_gaps().iter().map(|g| g.0).take(2).collect();
    for (k, &(q, n, f)) in raw.log.iter().enumerate() {
        if g.iter().any(|&gp| pos + 1000 >= gp && pos <= gp + 2500) {
            let (pq, pn, _) = if k > 0 { raw.log[k - 1] } else { (q, 0, 0) };
            let step = (q as f64 - pq as f64) / 1e4 - pn as f64 * 1000.0 / RATE as f64;
            // largest residual inside this packet
            let c = 2.0 * (2.0 * std::f32::consts::PI * F_EXCLUDED / RATE).cos();
            let (mut mr, mut at) = (0.0f32, 0usize);
            for j in pos.max(2)..pos + n {
                let r = (raw.samples[j] - c * raw.samples[j - 1] + raw.samples[j - 2]).abs();
                if r > mr { mr = r; at = j - pos; }
            }
            let pk = raw.samples[pos..pos + n].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let first_nz = raw.samples[pos..pos + n].iter().position(|v| *v != 0.0);
            println!("   pkt #{k} at sample {pos} ({:.1} ms): frames {n} flags {f:#x} ts step beyond prev end {step:+.2} ms | max residual {mr:.4} at +{at} | peak {pk:.4} first nonzero {first_nz:?}", pos as f32 * 1000.0 / RATE);
        }
        pos += n;
    }
}

/// One run of the "rapid volume clicks" scenario through capture: a continuous 1000 Hz
/// sine plays while another stream opens and closes 20 times, every 150 ms. Returns
/// (cable glitch times in ms, holes in the raw process loopback, report), or None if
/// the cable recorder had gaps.
fn rapid_clicks_through_capture(env: &[(&str, &str)]) -> Option<(Vec<f32>, usize, String)> {
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    let _t = start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "9000"]);
    let _cap = start_capture_env(&["--exclude-pid", &pid], env);
    thread::sleep(Duration::from_millis(1000));
    let cable = record(Source::Cable, 5000);
    let cable2 = record(Source::Cable, 5000);
    let raw = record_packets(Source::ProcessLoopback(dummy.pid()), 5000);
    thread::sleep(Duration::from_millis(700));
    rapid_streams(20, 150, false);
    let (cable, cable2, raw) = (cable.join().unwrap(), cable2.join().unwrap(), raw.join().unwrap());
    {
        let skip = RATE as usize / 10;
        let al = align_to(&cable2, &cable);
        let (e1, _) = sine_glitches(&cable.samples[skip..], F_EXCLUDED, 0.005);
        let (e2, _) = sine_glitches(&al[skip.min(al.len())..], F_EXCLUDED, 0.005);
        println!("   [two cable recorders] first: {e1:?} second (same timeline): {e2:?}");
    }
    if cable.gaps > 0 {
        return None;
    }
    let holes = raw.timestamp_gaps().len();
    let skip = RATE as usize / 10;
    let (ev, floor) = sine_glitches(&cable.samples[skip..], F_EXCLUDED, 0.005);
    let ev: Vec<f32> = ev.iter().map(|t| t + 100.0).collect();
    for &t in ev.iter().take(2) {
        let i = (t * RATE / 1000.0) as usize;
        let w: Vec<String> = cable.samples[i.saturating_sub(8)..(i + 8).min(cable.samples.len())].iter().map(|v| format!("{v:.4}")).collect();
        println!("   cable around {t:.1} ms: {}", w.join(" "));
    }
    let report = format!(
        "raw process loopback: {holes} holes ({} waveform jumps) | cable: {} glitches {:?}, floor {floor:.1} dB, top residuals {} | cable times relative to qpc {:.1} ms",
        sine_glitches(&raw.samples[skip..], F_EXCLUDED, 0.005).0.len(),
        ev.len(),
        &ev[..ev.len().min(8)],
        top_residuals(&cable.samples[skip..], F_EXCLUDED, 100.0),
        cable.start_qpc as f64 / 1e4
    );
    Some((ev, holes, report))
}

/// 13. Rapid clicks on the volume slider (each plays a new ding): another stream opens
/// and closes 20 times while a continuous sine plays. Each time, Windows' process
/// loopback loses ~40 ms (unrecoverable: capture inserts silence on the timeline), but
/// the cable must not carry a single hard step (sine-predictor residual above the
/// t10/t11 threshold): the edges of the lost audio are faded.
#[test]
fn t13_rapid_stream_open_close() {
    let _g = serial();
    let (ev, holes, report) = (1..=3).find_map(|_| rapid_clicks_through_capture(&[])).expect("cable recorder kept having gaps");
    println!("[t13] {report}");
    assert!(ev.is_empty(), "[t13] {} discontinuities in the cable ({holes} holes in raw process loopback): {ev:?}
{report}", ev.len());
}

/// Diagnostic (ignored): the real thing. A ding (`AUDIO_EXCLUDE_DING`, default
/// Windows Background.wav) restarted 15 times every 150 ms (`SoundPlayer.Play()`, like
/// rapid clicks on the volume slider), through capture.
/// The cable is compared with device loopback (what "System sound" sends). Counts 5 ms
/// dropout blocks (device loopback loud, cable >12 dB quieter) and reports the residual.
#[test]
#[ignore]
fn diag_rapid_dings() {
    let _g = serial();
    let wav = std::env::var("AUDIO_EXCLUDE_DING").unwrap_or_else(|_| r"C:\Windows\Media\Windows Background.wav".into());
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    for (label, env) in [("capture", &[][..])] {
        let _cap = start_capture_env(&["--exclude-pid", &pid], env);
        thread::sleep(Duration::from_millis(1500));
        let cable = record(Source::Cable, 6000);
        let local = record(Source::Local, 6000);
        thread::sleep(Duration::from_millis(300));
        let mut ps = Command::new("powershell");
        ps.args(["-NoProfile", "-Command", &format!("$p = New-Object Media.SoundPlayer '{wav}'; $p.Load(); for ($i = 0; $i -lt 15; $i++) {{ $p.Play(); Start-Sleep -Milliseconds 150 }}; Start-Sleep -Milliseconds 1500")]);
        assert!(ps.status().unwrap().success());
        let (cable, local) = (cable.join().unwrap(), local.join().unwrap());
        let cable_al = align_to(&cable, &local);
        let Some(on) = local.samples.iter().position(|v| v.abs() > 0.001) else {
            println!("[diag rapid dings {label}] no ding in device loopback");
            continue;
        };
        let start = on.saturating_sub(480);
        let n = (RATE as usize * 3).min(local.samples.len() - start);
        let reference = &local.samples[start..start + n];
        let delayed = &cable_al[start..(start + n + 9600).min(cable_al.len())];
        let (lag, corr) = xcorr_lag(reference, delayed, 4800);
        let m = n.min(delayed.len() - lag);
        let (num, den) = (0..m).fold((0.0f64, 0.0f64), |(a, b), i| (a + (delayed[i + lag] * reference[i]) as f64, b + (reference[i] * reference[i]) as f64));
        let gain = (num / den) as f32;
        let blk = 240;
        let mut dropouts = Vec::new();
        for (k, (r, c)) in reference[..m].chunks(blk).zip(delayed[lag..lag + m].chunks(blk)).enumerate() {
            let (lr, lc) = (audio_exclude::dsp::rms_db(r), audio_exclude::dsp::rms_db(c) - audio_exclude::dsp::to_db(gain));
            if lr > -40.0 && lc < lr - 12.0 {
                dropouts.push(k * 5);
            }
        }
        let res: Vec<f32> = (0..m).map(|i| delayed[i + lag] - gain * reference[i]).collect();
        println!(
            "[diag rapid dings {label}] lag {:.2} ms, corr {corr:.4}, gain {gain:.3} | signal {:.1} dB, residual {:.1} dB | dropout blocks (5 ms): {} at {:?} | recorder gaps cable {} local {}",
            lag as f32 * 1000.0 / RATE,
            audio_exclude::dsp::rms_db(&reference[..m]),
            audio_exclude::dsp::rms_db(&res),
            dropouts.len(),
            &dropouts[..dropouts.len().min(12)],
            cable.gaps,
            local.gaps
        );
    }
}

impl PacketRec {
    /// QPC time (ms) of sample `idx` (from the packet containing it).
    fn qpc_ms_at(&self, idx: usize) -> f64 {
        let mut pos = 0usize;
        for &(q, n, _) in &self.log {
            if idx < pos + n {
                return q as f64 / 1e4 + (idx - pos) as f64 * 1000.0 / RATE as f64;
            }
            pos += n;
        }
        self.log.last().map(|l| l.0 as f64 / 1e4).unwrap_or(0.0)
    }

    /// Runs of exact zeros (at least 1 ms) that start right after non-zero audio:
    /// (qpc ms of the first zero, length ms).
    fn zero_runs(&self) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        let mut i = 1;
        let x = &self.samples;
        while i < x.len() {
            if x[i] == 0.0 && x[i - 1] != 0.0 {
                let s = i;
                while i < x.len() && x[i] == 0.0 {
                    i += 1;
                }
                if i - s >= RATE as usize / 1000 && i < x.len() {
                    out.push((self.qpc_ms_at(s), (i - s) as f64 * 1000.0 / RATE as f64));
                }
            }
            i += 1;
        }
        out
    }
}

/// Diagnostic (ignored): ONE ding (`AUDIO_EXCLUDE_DING`, default Windows
/// Background.wav), (a) after silence and (b) while a continuous 1000 Hz sine plays.
/// Locates, on the QPC timeline relative to the ding's start/end in device loopback,
/// where process loopback has timestamp holes and runs of zeros. Then compares the
/// cable (through capture) with device loopback.
#[test]
#[ignore]
fn diag_single_ding() {
    let _g = serial();
    let wav = std::env::var("AUDIO_EXCLUDE_DING").unwrap_or_else(|_| r"C:\Windows\Media\Windows Background.wav".into());
    let dummy = start_dummy();
    let pid = dummy.pid().to_string();
    // AUDIO_EXCLUDE_CASE=sine: only the case over a sine
    let only_sine = std::env::var("AUDIO_EXCLUDE_CASE").is_ok_and(|v| v == "sine");
    for with_sine in [false, true] {
        if only_sine && !with_sine {
            continue;
        }
        for (label, env) in [("capture", &[][..])] {
            let case = format!("{} / {label}", if with_sine { "over a sine" } else { "after silence" });
            let _t = with_sine.then(|| start_tone(&tone_exe(), F_EXCLUDED, &["--duration", "9000"]));
            let _cap = start_capture_env(&["--exclude-pid", &pid], env);
            thread::sleep(Duration::from_millis(if with_sine { 1500 } else { 4000 }));
            let raw = record_packets(Source::ProcessLoopback(dummy.pid()), 4000);
            let dev = record_packets(Source::Local, 4000);
            let cable = record_packets(Source::Cable, 4000);
            thread::sleep(Duration::from_millis(500));
            let mut ps = Command::new("powershell");
            ps.args(["-NoProfile", "-Command", &format!("$p = New-Object Media.SoundPlayer '{wav}'; $p.Load(); $q0 = [Diagnostics.Stopwatch]::GetTimestamp(); $p.PlaySync(); $q1 = [Diagnostics.Stopwatch]::GetTimestamp(); Write-Output \"$q0 $q1 $([Diagnostics.Stopwatch]::Frequency)\"")]);
            let o = ps.output().unwrap();
            let v: Vec<f64> = String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|s| s.parse().ok()).collect();
            let (raw, dev, cable) = (raw.join().unwrap(), dev.join().unwrap(), cable.join().unwrap());
            // ding activity in device loopback: where the signal departs from the sine
            // (or from silence)
            let c = 2.0 * (2.0 * std::f32::consts::PI * F_EXCLUDED / RATE).cos();
            let active: Vec<usize> = (2..dev.samples.len())
                .filter(|&n| {
                    let x = &dev.samples;
                    if with_sine { (x[n] - c * x[n - 1] + x[n - 2]).abs() > 0.003 } else { x[n].abs() > 0.0005 }
                })
                .collect();
            let (Some(&a0), Some(&a1)) = (active.first(), active.last()) else {
                // locate the process-loopback holes relative to PlaySync's call/return
                if v.len() == 3 {
                    let (c0, c1) = (v[0] / v[2] * 1000.0, v[1] / v[2] * 1000.0);
                    let holes: Vec<String> = raw.timestamp_gaps().iter().map(|g| format!("call{:+.1} / return{:+.1} ({:.1} ms)", g.1 - c0, g.1 - c1, g.2)).collect();
                    let zeros: Vec<String> = raw.zero_runs().iter().filter(|z| z.1 < 200.0).map(|z| format!("call{:+.1} ({:.1} ms)", z.0 - c0, z.1)).collect();
                    println!("[diag single ding {case}] ding not found in device loopback; PlaySync took {:.0} ms; process loopback holes {holes:?}; zero runs {zeros:?}; device loopback holes {}", c1 - c0, dev.timestamp_gaps().len());
                }
                continue;
            };
            let (on, off) = (dev.qpc_ms_at(a0), dev.qpc_ms_at(a1));
            let rel = |q: f64| {
                if q < on + (off - on) / 2.0 { format!("onset{:+.1}", q - on) } else { format!("end{:+.1}", q - off) }
            };
            let holes: Vec<String> = raw.timestamp_gaps().iter().map(|g| format!("{} ({:.1} ms)", rel(g.1), g.2)).collect();
            let zeros: Vec<String> = raw.zero_runs().iter().filter(|z| z.1 < 200.0).map(|z| format!("{} ({:.1} ms)", rel(z.0), z.1)).collect();
            let first_raw = raw.log.first().map(|l| rel(l.0 as f64 / 1e4)).unwrap_or_default();
            let ps_times = if v.len() == 3 { format!("PlaySync called at {}, returned at {}", rel(v[0] / v[2] * 1000.0), rel(v[1] / v[2] * 1000.0)) } else { String::new() };
            println!("[diag single ding {case}] ding in device loopback {on:.1}..{off:.1} ms qpc ({:.0} ms long); {ps_times}; device loopback holes {}", off - on, dev.timestamp_gaps().len());
            println!("    process loopback: first packet {first_raw}; holes {holes:?}; zero runs after audio {zeros:?}");
            // cable vs device loopback: align by cross-correlation around the ding
            let ci = cable.samples.iter().position(|_| true).unwrap_or(0);
            let _ = ci;
            let cq0 = cable.log.first().map(|l| l.0 as f64 / 1e4).unwrap_or(0.0);
            let dq0 = dev.log.first().map(|l| l.0 as f64 / 1e4).unwrap_or(0.0);
            // place both on dev's sample timeline (contiguous; neither recorder had holes)
            let off_s = ((cq0 - dq0) * RATE as f64 / 1000.0).round() as i64;
            let start = a0.saturating_sub(2400);
            let n = (a1 + 2400).min(dev.samples.len()) - start;
            let reference = &dev.samples[start..start + n];
            let cab: Vec<f32> = (0..n + 9600).map(|i| {
                let j = (start + i) as i64 - off_s;
                if j >= 0 && (j as usize) < cable.samples.len() { cable.samples[j as usize] } else { 0.0 }
            }).collect();
            let (lag, corr) = xcorr_lag(reference, &cab, 4800);
            let (num, den) = (0..n).fold((0.0f64, 0.0f64), |(a, b), i| (a + (cab[i + lag] * reference[i]) as f64, b + (reference[i] * reference[i]) as f64));
            let gain = (num / den) as f32;
            let blk = 240;
            let mut dropouts = Vec::new();
            for (k, (r, cc)) in reference.chunks(blk).zip(cab[lag..lag + n].chunks(blk)).enumerate() {
                let (lr, lc) = (audio_exclude::dsp::rms_db(r), audio_exclude::dsp::rms_db(cc) - audio_exclude::dsp::to_db(gain));
                if lr > -45.0 && lc < lr - 10.0 {
                    dropouts.push(rel(dev.qpc_ms_at(start + k * blk)));
                }
            }
            let res: Vec<f32> = (0..n).map(|i| cab[i + lag] - gain * reference[i]).collect();
            let worst: Vec<String> = res.chunks(blk).enumerate()
                .map(|(k, b)| (audio_exclude::dsp::rms_db(b) - audio_exclude::dsp::rms_db(&reference[k * blk..(k * blk + blk).min(n)]), k))
                .filter(|(d, _)| *d > -10.0)
                .take(10)
                .map(|(d, k)| format!("{}:{d:+.0}dB", rel(dev.qpc_ms_at(start + k * blk))))
                .collect();
            println!(
                "    cable vs device loopback: lag {:.2} ms, corr {corr:.4}, gain {gain:.3}, residual {:.1} dB rel. signal | 5 ms dropout blocks {} {dropouts:?} | cable recorder holes {:?} | blocks with residual > -10 dB rel. {worst:?}",
                lag as f32 * 1000.0 / RATE,
                audio_exclude::dsp::rms_db(&res) - audio_exclude::dsp::rms_db(reference),
                dropouts.len(),
                cable.timestamp_gaps().iter().map(|g| (rel(g.1), (g.2 * 10.0).round() / 10.0)).collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(300));
        }
    }
}
