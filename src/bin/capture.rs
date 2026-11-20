//! Captures all system audio except one process tree and plays it on another device
//! (by default the playback side of "Line 1 (Virtual Audio Cable)").
//!
//! ```text
//! capture [--exclude-name Discord.exe | --exclude-pid 1234] [--output "Line 1 (Virtual Audio Cable)"]
//! capture --list-devices
//! ```
//!
//! Status lines on stdout (used by the tests):
//! * `TARGET <pid>`  now excluding this process tree (`TARGET none` = nothing to exclude)
//! * `READY`         first stream is running

use std::process::ExitCode;
use std::time::{Duration, Instant};

use audio_exclude::procs;
use audio_exclude::wasapi::{self, CaptureStream, ExclusiveRender, CHANNELS};
use windows::Win32::Media::Audio::{eCapture, eRender};

const DEFAULT_OUTPUT: &str = "Line 1 (Virtual Audio Cable)";
/// Never let more than this much audio queue up in the output buffer (latency cap).
const MAX_QUEUE_MS: u32 = 30;

enum Target {
    None,
    Pid(u32),
    Name(String),
}

struct Args {
    target: Target,
    output: String,
}

const USAGE: &str = "usage: capture [--exclude-name NAME.exe | --exclude-pid PID] [--output DEVICE]\n       capture --list-devices";

fn parse() -> Result<Option<Args>, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args { target: Target::None, output: DEFAULT_OUTPUT.into() };
    while let Some(s) = it.next() {
        match s.as_str() {
            "--exclude-name" => a.target = Target::Name(it.next().ok_or("--exclude-name needs a value")?),
            "--exclude-pid" => {
                a.target = Target::Pid(
                    it.next().and_then(|v| v.parse().ok()).ok_or("--exclude-pid needs a number")?,
                )
            }
            "--output" => a.output = it.next().ok_or("--output needs a value")?,
            "--list-devices" => {
                list_devices();
                return Ok(None);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            x => return Err(format!("unknown argument {x:?}\n{USAGE}")),
        }
    }
    Ok(Some(a))
}

fn list_devices() {
    let _ = wasapi::init_com();
    for (flow, label) in [(eRender, "Playback"), (eCapture, "Recording")] {
        println!("{label} devices:");
        match wasapi::list_devices(flow) {
            Ok(devs) => devs.iter().for_each(|(n, _)| println!("  {n}")),
            Err(e) => println!("  error: {e}"),
        }
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("capture: {e}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("capture: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Which PID to exclude right now. `None` = nothing to exclude.
fn resolve(t: &Target) -> Option<u32> {
    match t {
        Target::None => None,
        Target::Pid(p) => Some(*p),
        Target::Name(n) => procs::find_root(n),
    }
}

fn open_loopback(target: Option<u32>, rate: u32) -> Result<CaptureStream, String> {
    // EXCLUDE mode needs some PID; with nothing to exclude we exclude ourselves, which
    // removes nothing (our only output is an exclusive stream that loopback can't see).
    let pid = target.unwrap_or_else(std::process::id);
    let s = CaptureStream::process_loopback(pid, false, 20, rate)
        .map_err(|e| format!("process loopback activation (pid {pid}) failed: {e}"))?;
    match target {
        Some(p) => println!("TARGET {p}"),
        None => println!("TARGET none"),
    }
    Ok(s)
}

fn run(args: Args) -> Result<(), String> {
    wasapi::init_com().map_err(|e| format!("CoInitializeEx: {e}"))?;
    let out_dev = wasapi::find_device(&args.output, eRender)?;
    // Exclusive mode is essential, see ExclusiveRender: a shared-mode stream into the
    // cable would be captured by our own process loopback and recirculate.
    let mut out = ExclusiveRender::open(&out_dev, 40).map_err(|e| format!("open output {:?}: {e}", args.output))?;
    let rate = out.rate;
    eprintln!(
        "capture: output {:?} exclusive {} Hz {:?}, buffer {} frames",
        args.output, rate, out.kind, out.buffer_frames
    );
    let max_queue = (rate * MAX_QUEUE_MS / 1000).min(out.buffer_frames);
    // Pre-fill a little silence so the first packets don't underrun.
    let silence = vec![0.0f32; (rate as usize / 200) * CHANNELS];
    out.write(&silence).map_err(|e| format!("output write: {e}"))?;
    out.start().map_err(|e| format!("start output: {e}"))?;

    let mut target = resolve(&args.target);
    let mut cap = open_loopback(target, rate)?;
    println!("READY");

    let mut last_check = Instant::now();
    loop {
        if cap.wait(Duration::from_millis(50)) {
            let mut err = None;
            cap.read(|p| {
                let mut data = p.samples;
                let frames = data.len() / CHANNELS;
                match out.padding() {
                    Ok(pad) => {
                        // Drop the oldest part of the packet if the queue would exceed the cap
                        // (clock drift between the loopback and output device).
                        let over = (pad as usize + frames).saturating_sub(max_queue as usize);
                        if over > 0 {
                            let skip = over.min(frames);
                            data = &data[skip * CHANNELS..];
                        }
                        if let Err(e) = out.write(data) {
                            err = Some(e);
                        }
                    }
                    Err(e) => err = Some(e),
                }
            })
            .map_err(|e| format!("capture read: {e}"))?;
            if let Some(e) = err {
                return Err(format!("output write: {e}"));
            }
        }

        if matches!(args.target, Target::Name(_)) && last_check.elapsed() > Duration::from_millis(250) {
            last_check = Instant::now();
            let now = resolve(&args.target);
            if now != target {
                drop(cap);
                target = now;
                cap = open_loopback(target, rate)?;
            }
        }
    }
}
