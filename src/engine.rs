//! The capture engine: process loopback (EXCLUDE one tree) -> exclusive output, on a
//! background thread. Target and output can be changed while it runs; output failures
//! (device missing, taken exclusively by someone else, unplugged) are reported and
//! retried instead of ending the engine.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::eRender;

use crate::procs;
use crate::wasapi::{self, CaptureStream, ExclusiveRender, CHANNELS};

/// Never let more than this much audio queue up in the output buffer (latency cap).
const MAX_QUEUE_MS: u32 = 30;
/// How often a name/path target is re-resolved (to follow exits and restarts).
const RESOLVE_EVERY: Duration = Duration::from_millis(250);
/// Delay between attempts to (re)open a failed output or capture.
const RETRY_EVERY: Duration = Duration::from_secs(2);

/// What to exclude.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// Exclude nothing: pass all system audio through.
    None,
    Pid(u32),
    /// Executable file name, e.g. `Discord.exe` (root process of that name).
    Name(String),
    /// Full executable path (root process running that exe).
    Path(String),
}

impl Target {
    /// The PID to exclude right now, if any.
    pub fn resolve(&self) -> Option<u32> {
        match self {
            Target::None => None,
            Target::Pid(p) => Some(*p),
            Target::Name(n) => procs::find_root(n),
            Target::Path(p) => procs::find_root_by_path(p),
        }
    }

    fn is_dynamic(&self) -> bool {
        matches!(self, Target::Name(_) | Target::Path(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Output opened (`Ok(description)`) or failed (`Err(message)`, will be retried).
    Output(Result<String, String>),
    /// Now excluding this PID's tree (`None`: target not running / nothing selected).
    Target(Option<u32>),
    /// Audio is flowing (sent after each successful (re)start of the streams).
    Running,
    /// Process loopback could not be started (will be retried).
    CaptureError(String),
}

enum Cmd {
    SetTarget(Target),
    SetOutput(String),
    Stop,
}

pub struct Engine {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(target: Target, output: String, on_event: impl FnMut(Event) + Send + 'static) -> Engine {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || run(target, output, rx, on_event));
        Engine { tx, thread: Some(thread) }
    }

    pub fn set_target(&self, t: Target) {
        let _ = self.tx.send(Cmd::SetTarget(t));
    }

    pub fn set_output(&self, name: String) {
        let _ = self.tx.send(Cmd::SetOutput(name));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Why the inner streaming loop ended.
enum Exit {
    Stop,
    /// Reopen everything (output changed or failed).
    Restart,
}

struct State<F: FnMut(Event)> {
    target: Target,
    output: String,
    emit: F,
    last_output: Option<Result<String, String>>,
}

impl<F: FnMut(Event)> State<F> {
    fn report_output(&mut self, r: Result<String, String>) {
        // only report changes, so a retry loop doesn't spam notifications
        if self.last_output.as_ref() != Some(&r) {
            self.last_output = Some(r.clone());
            (self.emit)(Event::Output(r));
        }
    }

    /// Applies a command; returns false on Stop. Sets `*restart` if the output changed.
    fn apply(&mut self, c: Cmd, restart: &mut bool) -> bool {
        match c {
            Cmd::Stop => return false,
            Cmd::SetTarget(t) => self.target = t,
            Cmd::SetOutput(o) => {
                if o != self.output {
                    self.output = o;
                    self.last_output = None;
                    *restart = true;
                }
            }
        }
        true
    }

    /// Sleeps up to `d`, handling commands. Returns false on Stop.
    fn idle(&mut self, rx: &Receiver<Cmd>, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(c) => {
                    let mut restart = false;
                    if !self.apply(c, &mut restart) {
                        return false;
                    }
                    if restart {
                        return true; // retry right away with the new output
                    }
                }
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }
}

fn open_output(name: &str) -> Result<ExclusiveRender, String> {
    let dev = wasapi::find_device(name, eRender)?;
    // Exclusive mode is essential, see ExclusiveRender: a shared-mode stream into the
    // cable would be captured by our own process loopback and recirculate.
    let mut out = ExclusiveRender::open(&dev, 40).map_err(|e| format!("open output {name:?}: {e}"))?;
    // Pre-fill a little silence so the first packets don't underrun.
    let silence = vec![0.0f32; (out.rate as usize / 200) * CHANNELS];
    out.write(&silence).map_err(|e| format!("output write: {e}"))?;
    out.start().map_err(|e| format!("start output: {e}"))?;
    Ok(out)
}

fn open_loopback(target: Option<u32>, rate: u32) -> Result<CaptureStream, String> {
    // EXCLUDE mode needs some PID; with nothing to exclude we exclude ourselves, which
    // removes nothing (our only output is an exclusive stream that loopback can't see).
    let pid = target.unwrap_or_else(std::process::id);
    CaptureStream::process_loopback(pid, false, 20, rate)
        .map_err(|e| format!("process loopback activation (pid {pid}) failed: {e}"))
}

fn run(target: Target, output: String, rx: Receiver<Cmd>, emit: impl FnMut(Event)) {
    if let Err(e) = wasapi::init_com() {
        let mut emit = emit;
        emit(Event::CaptureError(format!("CoInitializeEx: {e}")));
        return;
    }
    let mut st = State { target, output, emit, last_output: None };
    loop {
        let mut out = match open_output(&st.output) {
            Ok(o) => o,
            Err(e) => {
                st.report_output(Err(e));
                if !st.idle(&rx, RETRY_EVERY) {
                    return;
                }
                continue;
            }
        };
        st.report_output(Ok(format!("{} ({} Hz {:?})", st.output, out.rate, out.kind)));
        match stream(&mut st, &rx, &mut out) {
            Exit::Stop => return,
            Exit::Restart => {}
        }
    }
}

fn stream<F: FnMut(Event)>(st: &mut State<F>, rx: &Receiver<Cmd>, out: &mut ExclusiveRender) -> Exit {
    let rate = out.rate;
    let max_queue = (rate * MAX_QUEUE_MS / 1000).min(out.buffer_frames) as usize;
    let mut pid = st.target.resolve();
    let mut cap = loop {
        match open_loopback(pid, rate) {
            Ok(c) => break c,
            Err(e) => {
                (st.emit)(Event::CaptureError(e));
                if !st.idle(rx, RETRY_EVERY) {
                    return Exit::Stop;
                }
                pid = st.target.resolve();
            }
        }
    };
    (st.emit)(Event::Target(pid));
    (st.emit)(Event::Running);

    let mut last_resolve = Instant::now();
    loop {
        // commands
        let mut retarget = false;
        loop {
            match rx.try_recv() {
                Ok(c) => {
                    let before = st.target.clone();
                    let mut restart = false;
                    if !st.apply(c, &mut restart) {
                        return Exit::Stop;
                    }
                    if restart {
                        return Exit::Restart;
                    }
                    retarget |= st.target != before;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Exit::Stop,
            }
        }
        if retarget || (st.target.is_dynamic() && last_resolve.elapsed() > RESOLVE_EVERY) {
            last_resolve = Instant::now();
            let now = st.target.resolve();
            if now != pid || retarget {
                drop(cap);
                pid = now;
                cap = match open_loopback(pid, rate) {
                    Ok(c) => c,
                    Err(e) => {
                        (st.emit)(Event::CaptureError(e));
                        return Exit::Restart;
                    }
                };
                (st.emit)(Event::Target(pid));
            }
        }

        // audio
        if cap.wait(Duration::from_millis(50)) {
            let mut err = None;
            let r = cap.read(|p| {
                if err.is_some() {
                    return;
                }
                let mut data = p.samples;
                let frames = data.len() / CHANNELS;
                match out.padding() {
                    Ok(pad) => {
                        // Drop the oldest part of the packet if the queue would exceed the
                        // cap (clock drift between the loopback and the output device).
                        let over = (pad as usize + frames).saturating_sub(max_queue);
                        if over > 0 {
                            data = &data[over.min(frames) * CHANNELS..];
                        }
                        if let Err(e) = out.write(data) {
                            err = Some(e);
                        }
                    }
                    Err(e) => err = Some(e),
                }
            });
            if let Err(e) = r {
                (st.emit)(Event::CaptureError(format!("capture read: {e}")));
                return Exit::Restart;
            }
            if let Some(e) = err {
                st.report_output(Err(format!("output {:?} failed: {e}", st.output)));
                if !st.idle(rx, RETRY_EVERY) {
                    return Exit::Stop;
                }
                return Exit::Restart;
            }
        }
    }
}
