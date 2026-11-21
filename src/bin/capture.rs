//! Captures all system audio except one process tree and plays it on another device
//! (by default the playback side of "Line 1 (Virtual Audio Cable)").
//!
//! ```text
//! capture [--exclude-name Discord.exe | --exclude-path C:\...\Discord.exe | --exclude-pid 1234]
//!         [--output "Line 1 (Virtual Audio Cable)"] [--buffer-ms 15] [--silence-dither]
//! capture --list-devices
//! ```
//!
//! Status lines on stdout (used by the tests):
//! * `TARGET <pid>`  now excluding this process tree (`TARGET none` = nothing to exclude)
//! * `READY`         first stream is running

use std::process::ExitCode;

use ausieve::engine::{Engine, Event, Target, DEFAULT_BUFFER_MS};
use ausieve::wasapi;
use windows::Win32::Media::Audio::{eCapture, eRender};

const DEFAULT_OUTPUT: &str = "Line 1 (Virtual Audio Cable)";

struct Args {
    target: Target,
    output: String,
    buffer_ms: u32,
    dither: bool,
}

const USAGE: &str = "usage: capture [--exclude-name NAME.exe | --exclude-path PATH | --exclude-pid PID] [--output DEVICE] [--buffer-ms N] [--silence-dither]\n       capture --list-devices";

fn parse() -> Result<Option<Args>, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args { target: Target::None, output: DEFAULT_OUTPUT.into(), buffer_ms: DEFAULT_BUFFER_MS, dither: false };
    while let Some(s) = it.next() {
        match s.as_str() {
            "--exclude-name" => a.target = Target::Name(it.next().ok_or("--exclude-name needs a value")?),
            "--exclude-path" => a.target = Target::Path(it.next().ok_or("--exclude-path needs a value")?),
            "--exclude-pid" => {
                a.target = Target::Pid(
                    it.next().and_then(|v| v.parse().ok()).ok_or("--exclude-pid needs a number")?,
                )
            }
            "--output" => a.output = it.next().ok_or("--output needs a value")?,
            "--buffer-ms" => {
                a.buffer_ms = it.next().and_then(|v| v.parse().ok()).ok_or("--buffer-ms needs a number")?
            }
            "--silence-dither" => a.dither = true,
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
    let mut ready = false;
    // The CLI keeps its original fail-fast behaviour: an output error ends the process
    // (the tray app instead lets the engine retry).
    let engine = Engine::start_with(args.target, args.output, args.buffer_ms, move |ev| match ev {
        Event::Output(Ok(desc)) => eprintln!("capture: output {desc}"),
        Event::Output(Err(e)) | Event::CaptureError(e) => {
            eprintln!("capture: {e}");
            std::process::exit(1);
        }
        Event::Target(Some(p)) => println!("TARGET {p}"),
        Event::Target(None) => println!("TARGET none"),
        Event::Running => {
            if !ready {
                ready = true;
                println!("READY");
            }
        }
    });
    engine.set_dither(args.dither);
    loop {
        std::thread::park();
    }
}
