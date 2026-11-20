//! Test helper: plays a sine wave on the default playback device.
//!
//! ```text
//! tone <freq> [--duration ms] [--burst ms] [--delay ms] [--amp a] [--spawn-child] [--silent] [--device name]
//! ```
//! * `--duration` total lifetime of the stream (default 5000 ms).
//! * `--delay`    play silence for this long first (default 0).
//! * `--burst`    after the delay, play the tone only for this long, then silence until
//!   `--duration` ends (the stream stays open). Default: tone for the whole duration.
//! * `--spawn-child` don't open any audio stream; start a child copy of this exe (same
//!   arguments) that plays, and wait for it. Used to test process-*tree* exclusion.
//! * `--silent`   open no audio stream at all, just live for `--duration` (a dummy PID).
//! * `--device`   play on this playback device instead of the default one.
//!
//! Prints `PLAYING <pid>` once the stream runs (and `CHILD <pid>` for `--spawn-child`).

use std::f64::consts::PI;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use audio_exclude::procs::is_alive;
use audio_exclude::wasapi::{self, CHANNELS, SAMPLE_RATE};
use windows::Win32::Media::Audio::eRender;

struct Args {
    freq: f64,
    duration_ms: u64,
    burst_ms: Option<u64>,
    delay_ms: u64,
    amp: f64,
    spawn_child: bool,
    silent: bool,
    parent_pid: Option<u32>,
    device: Option<String>,
}

fn parse() -> Result<(Args, Vec<String>), String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut it = raw.iter();
    let mut a = Args {
        freq: 0.0,
        duration_ms: 5000,
        burst_ms: None,
        delay_ms: 0,
        amp: 0.1,
        spawn_child: false,
        silent: false,
        parent_pid: None,
        device: None,
    };
    let mut freq = None;
    fn num<T: std::str::FromStr>(v: Option<&String>, flag: &str) -> Result<T, String> {
        v.and_then(|s| s.parse().ok()).ok_or_else(|| format!("{flag} needs a number"))
    }
    while let Some(s) = it.next() {
        match s.as_str() {
            "--duration" => a.duration_ms = num(it.next(), s)?,
            "--burst" => a.burst_ms = Some(num(it.next(), s)?),
            "--delay" => a.delay_ms = num(it.next(), s)?,
            "--amp" => a.amp = num(it.next(), s)?,
            "--parent-pid" => a.parent_pid = Some(num(it.next(), s)?),
            "--device" => a.device = Some(it.next().ok_or("--device needs a name")?.clone()),
            "--spawn-child" => a.spawn_child = true,
            "--silent" => a.silent = true,
            x if !x.starts_with("--") && freq.is_none() => {
                freq = Some(x.parse().map_err(|_| format!("bad frequency {x:?}"))?)
            }
            x => return Err(format!("unknown argument {x:?}")),
        }
    }
    a.freq = freq.ok_or("usage: tone <freq> [--duration ms] [--burst ms] [--delay ms] [--amp a] [--spawn-child] [--silent]")?;
    Ok((a, raw))
}

fn main() -> ExitCode {
    let (args, raw) = match parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tone: {e}");
            return ExitCode::from(2);
        }
    };
    let me = std::process::id();

    if args.spawn_child {
        let child_args: Vec<String> = raw.into_iter().filter(|s| s != "--spawn-child").collect();
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args(&child_args)
            .args(["--parent-pid", &me.to_string()])
            .spawn()
            .expect("spawn child");
        println!("CHILD {}", child.id());
        let st = child.wait().expect("wait child");
        return if st.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    if args.silent {
        println!("PLAYING {me}");
        std::thread::sleep(Duration::from_millis(args.duration_ms));
        return ExitCode::SUCCESS;
    }

    match play(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tone: {e}");
            ExitCode::FAILURE
        }
    }
}

fn play(a: &Args) -> windows::core::Result<()> {
    wasapi::init_com()?;
    let dev = match &a.device {
        Some(n) => wasapi::find_device(n, eRender).map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e))?,
        None => wasapi::default_device(eRender)?,
    };
    // RAW: bypass endpoint enhancements so the test signal stays a clean sine/burst.
    let out = wasapi::RenderStream::open_opts(&dev, 40, true)?;
    let rate = SAMPLE_RATE as u64;
    let total = a.duration_ms * rate / 1000;
    let on_start = a.delay_ms * rate / 1000;
    let on_end = a.burst_ms.map_or(u64::MAX, |b| on_start + b * rate / 1000);
    let step = 2.0 * PI * a.freq / rate as f64;
    // keep ~20 ms queued
    let target = (rate / 50) as u32;
    let mut pos: u64 = 0;
    let mut buf = Vec::new();

    out.start()?;
    println!("PLAYING {}", std::process::id());
    let t0 = Instant::now();
    let mut last_parent_check = t0;
    while pos < total {
        let pad = out.padding()?;
        if pad < target {
            let n = ((target - pad) as u64).min(total - pos) as usize;
            buf.clear();
            for i in 0..n as u64 {
                let k = pos + i;
                let v = if k >= on_start && k < on_end {
                    (a.amp * (step * (k - on_start) as f64).sin()) as f32
                } else {
                    0.0
                };
                buf.extend(std::iter::repeat_n(v, CHANNELS));
            }
            pos += out.write(&buf)? as u64;
        }
        std::thread::sleep(Duration::from_millis(3));
        if let Some(pp) = a.parent_pid {
            if last_parent_check.elapsed() > Duration::from_millis(100) {
                last_parent_check = Instant::now();
                if !is_alive(pp) {
                    return Ok(());
                }
            }
        }
    }
    // let the queued tail play out
    while out.padding()? > 0 && t0.elapsed() < Duration::from_millis(a.duration_ms + 500) {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
