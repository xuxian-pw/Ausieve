//! The capture engine: process loopback (EXCLUDE one tree) -> exclusive output, on a
//! background thread. Target and output can be changed while it runs; output failures
//! (device missing, taken exclusively by someone else, unplugged) are reported and
//! retried instead of ending the engine.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::output::{Output, OutputStats};
use crate::procs;
use crate::wasapi::{self, CaptureStream};

/// Default jitter-buffer start level (see [`Output::open`]). Process loopback delivers
/// 10 ms packets, so this must be > 10; the margin above 10 absorbs timing jitter.
/// Measured end-to-end (t6): 11 ms ~28 ms, 12 ms ~29 ms, 15 ms ~32 ms; all glitch-free
/// in t10 on the development machine.
pub const DEFAULT_BUFFER_MS: u32 = 12;
/// Highest accepted value.
pub const MAX_BUFFER_MS: u32 = 200;
/// Lowest accepted value: below one capture packet the output would underrun constantly.
pub const MIN_BUFFER_MS: u32 = 11;
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
    SetBuffer(u32),
    Stop,
}

pub struct Engine {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(target: Target, output: String, on_event: impl FnMut(Event) + Send + 'static) -> Engine {
        Self::start_with(target, output, DEFAULT_BUFFER_MS, on_event)
    }

    /// Like [`Engine::start`] with an explicit jitter-buffer level in ms (clamped to
    /// [`MIN_BUFFER_MS`]..=200). Lower = less latency, higher = more robust under load.
    pub fn start_with(target: Target, output: String, buffer_ms: u32, on_event: impl FnMut(Event) + Send + 'static) -> Engine {
        let (tx, rx) = mpsc::channel();
        let buffer_ms = buffer_ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS);
        let thread = std::thread::spawn(move || run(target, output, buffer_ms, rx, on_event));
        Engine { tx, thread: Some(thread) }
    }

    pub fn set_target(&self, t: Target) {
        let _ = self.tx.send(Cmd::SetTarget(t));
    }

    /// Changes the jitter-buffer level (reopens the output, a short gap).
    pub fn set_buffer_ms(&self, ms: u32) {
        let _ = self.tx.send(Cmd::SetBuffer(ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS)));
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
    buffer_ms: u32,
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
            Cmd::SetBuffer(b) => {
                if b != self.buffer_ms {
                    self.buffer_ms = b;
                    self.last_output = None;
                    *restart = true;
                }
            }
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

fn open_loopback(target: Option<u32>, rate: u32) -> Result<CaptureStream, String> {
    // EXCLUDE mode needs some PID; with nothing to exclude we exclude ourselves, which
    // removes nothing (our only output is an exclusive stream that loopback can't see).
    let pid = target.unwrap_or_else(std::process::id);
    CaptureStream::process_loopback(pid, false, 20, rate)
        .map_err(|e| format!("process loopback activation (pid {pid}) failed: {e}"))
}

fn run(target: Target, output: String, buffer_ms: u32, rx: Receiver<Cmd>, emit: impl FnMut(Event)) {
    if let Err(e) = wasapi::init_com() {
        let mut emit = emit;
        emit(Event::CaptureError(format!("CoInitializeEx: {e}")));
        return;
    }
    wasapi::boost_thread_priority();
    let mut st = State { target, output, buffer_ms, emit, last_output: None };
    loop {
        let out = match Output::open(&st.output, st.buffer_ms) {
            Ok(o) => o,
            Err(e) => {
                st.report_output(Err(e));
                if !st.idle(&rx, RETRY_EVERY) {
                    return;
                }
                continue;
            }
        };
        st.report_output(Ok(out.description.clone()));
        match stream(&mut st, &rx, &out) {
            Exit::Stop => return,
            Exit::Restart => {}
        }
    }
}

fn stream<F: FnMut(Event)>(st: &mut State<F>, rx: &Receiver<Cmd>, out: &Output) -> Exit {
    let rate = out.rate;
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

    let mut stats = Stats::new(rate, std::env::var_os("AUDIO_EXCLUDE_STATS").is_some());
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

        if let Some(e) = out.error() {
            st.report_output(Err(e));
            if !st.idle(rx, RETRY_EVERY) {
                return Exit::Stop;
            }
            return Exit::Restart;
        }

        // audio
        if cap.wait(Duration::from_millis(50)) {
            let r = cap.read(|p| {
                stats.packet(p.samples.len() / wasapi::CHANNELS, p.qpc_100ns, p.flags);
                out.push(p.samples);
            });
            if let Err(e) = r {
                (st.emit)(Event::CaptureError(format!("capture read: {e}")));
                return Exit::Restart;
            }
            stats.maybe_print(out.stats());
        }
    }
}

/// Optional per-second diagnostics on stderr (`AUDIO_EXCLUDE_STATS=1`).
struct Stats {
    on: bool,
    rate: u32,
    since: Instant,
    packets: usize,
    min_pkt: usize,
    max_pkt: usize,
    last_pkt_at: Option<Instant>,
    max_gap: Duration,
    discontinuities: usize,
    max_cap_delay_100ns: u64,
    min_queue: usize,
    max_queue: usize,
    prev: OutputStats,
    /// Lines go to a printer thread: writing to a (piped) stderr from the capture
    /// thread can block it long enough to cause underruns.
    printer: Option<mpsc::Sender<String>>,
}

impl Stats {
    fn new(rate: u32, on: bool) -> Stats {
        let mut s = Stats {
            on,
            rate,
            since: Instant::now(),
            packets: 0,
            min_pkt: 0,
            max_pkt: 0,
            last_pkt_at: None,
            max_gap: Duration::ZERO,
            discontinuities: 0,
            max_cap_delay_100ns: 0,
            min_queue: 0,
            max_queue: 0,
            prev: OutputStats::default(),
            printer: None,
        };
        if on {
            let (tx, rx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                for l in rx {
                    eprintln!("{l}");
                }
            });
            s.printer = Some(tx);
        }
        s.reset();
        s
    }

    fn reset(&mut self) {
        self.since = Instant::now();
        self.packets = 0;
        self.min_pkt = usize::MAX;
        self.max_pkt = 0;
        self.max_gap = Duration::ZERO;
        self.discontinuities = 0;
        self.max_cap_delay_100ns = 0;
        self.min_queue = usize::MAX;
        self.max_queue = 0;
    }

    fn packet(&mut self, frames: usize, qpc: u64, flags: u32) {
        if !self.on {
            return;
        }
        self.packets += 1;
        self.min_pkt = self.min_pkt.min(frames);
        self.max_pkt = self.max_pkt.max(frames);
        let now = Instant::now();
        if let Some(t) = self.last_pkt_at {
            self.max_gap = self.max_gap.max(now - t);
        }
        self.last_pkt_at = Some(now);
        if flags & wasapi::BUFFERFLAGS_DISCONTINUITY != 0 {
            self.discontinuities += 1;
        }
        self.max_cap_delay_100ns = self.max_cap_delay_100ns.max(wasapi::qpc_now_100ns().saturating_sub(qpc));
    }

    fn maybe_print(&mut self, o: OutputStats) {
        if !self.on {
            return;
        }
        self.min_queue = self.min_queue.min(o.queued_frames);
        self.max_queue = self.max_queue.max(o.queued_frames);
        if self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let ms = |f: f64| f * 1000.0 / self.rate as f64;
        let line = format!(
            "stats: {} pkts {:.1}-{:.1} ms, max gap {:.1} ms, max capture delay {:.1} ms, discontinuities {} | jitter buffer after push {:.1}-{:.1} ms, underruns {}, silence {:.1} ms, dropped {:.1} ms, device pulled {:.1} ms",
            self.packets,
            ms(self.min_pkt as f64),
            ms(self.max_pkt as f64),
            self.max_gap.as_secs_f64() * 1000.0,
            self.max_cap_delay_100ns as f64 / 10_000.0,
            self.discontinuities,
            ms(self.min_queue as f64),
            ms(self.max_queue as f64),
            o.underruns - self.prev.underruns,
            ms((o.silent_frames - self.prev.silent_frames) as f64),
            ms((o.dropped_frames - self.prev.dropped_frames) as f64),
            ms((o.pulled_frames - self.prev.pulled_frames) as f64),
        );
        if let Some(p) = &self.printer {
            let _ = p.send(line);
        }
        self.prev = o;
        self.reset();
    }
}
