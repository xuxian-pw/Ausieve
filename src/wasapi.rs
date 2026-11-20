//! Thin WASAPI wrappers.
//!
//! Every stream in this crate uses one fixed format: 48 kHz, 2 channels, 32-bit float.
//! Shared-mode endpoint streams use `AUTOCONVERTPCM` so the engine converts to/from the
//! device mix format; the process-loopback virtual device accepts the format directly.

use std::time::Duration;

use windows::core::{implement, Interface, Ref, Result, HRESULT, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantToStringAlloc, PROPVARIANT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
    STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// 100-ns units ("REFERENCE_TIME") per millisecond.
pub const HNS_PER_MS: i64 = 10_000;

const BUFFERFLAGS_SILENT: u32 = 0x2;
pub const BUFFERFLAGS_DISCONTINUITY: u32 = 0x1;

/// Initialises COM (multithreaded apartment) for the calling thread.
pub fn init_com() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
}

pub fn float_format() -> WAVEFORMATEX {
    float_format_at(SAMPLE_RATE)
}

pub fn float_format_at(rate: u32) -> WAVEFORMATEX {
    let block_align = (CHANNELS * 4) as u16;
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

pub fn friendly_name(dev: &IMMDevice) -> Result<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ)?;
        let pv = store.GetValue(&PKEY_Device_FriendlyName)?; // cleared by Drop
        let s: Result<PWSTR> = PropVariantToStringAlloc(&pv);
        let s = s?;
        let out = s.to_string().unwrap_or_default();
        CoTaskMemFree(Some(s.0 as _));
        Ok(out)
    }
}

/// Active endpoints of one direction (`eRender` or `eCapture`) with their friendly names.
pub fn list_devices(flow: EDataFlow) -> Result<Vec<(String, IMMDevice)>> {
    unsafe {
        let coll = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            out.push((friendly_name(&d)?, d));
        }
        Ok(out)
    }
}

/// Finds an active endpoint by friendly name: exact (case-insensitive) match first,
/// then unique substring match.
pub fn find_device(name: &str, flow: EDataFlow) -> std::result::Result<IMMDevice, String> {
    let devs = list_devices(flow).map_err(|e| format!("enumerating devices: {e}"))?;
    if let Some((_, d)) = devs.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Ok(d.clone());
    }
    let needle = name.to_lowercase();
    let subs: Vec<_> = devs.iter().filter(|(n, _)| n.to_lowercase().contains(&needle)).collect();
    if subs.len() == 1 {
        return Ok(subs[0].1.clone());
    }
    let names: Vec<_> = devs.iter().map(|(n, _)| n.as_str()).collect();
    let dir = if flow == eRender { "playback" } else { "recording" };
    Err(format!("no unique {dir} device matching {name:?}; available: {names:?}"))
}

pub fn default_device(flow: EDataFlow) -> Result<IMMDevice> {
    unsafe { enumerator()?.GetDefaultAudioEndpoint(flow, eConsole) }
}

fn activate_client(dev: &IMMDevice) -> Result<IAudioClient> {
    unsafe { dev.Activate(CLSCTX_ALL, None) }
}

/// Owned Win32 event handle.
struct Event(HANDLE);
impl Event {
    fn new(manual_reset: bool) -> Result<Self> {
        unsafe { Ok(Event(CreateEventW(None, manual_reset, false, None)?)) }
    }
    fn wait(&self, timeout: Duration) -> bool {
        unsafe { WaitForSingleObject(self.0, timeout.as_millis() as u32) == WAIT_OBJECT_0 }
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
// Event handles may be used from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

// ---------------------------------------------------------------------------
// Process loopback activation
// ---------------------------------------------------------------------------

/// Completion handler for `ActivateAudioInterfaceAsync`. `#[implement]` makes the object
/// agile (`IAgileObject` + free-threaded marshaler) by default, which the API requires.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: std::sync::Arc<Event>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        unsafe { SetEvent(self.done.0) }
    }
}

/// Activates an `IAudioClient` on the process-loopback virtual device.
/// With `include == false` the client captures all system audio *except* the process tree
/// rooted at `pid`; with `include == true` it captures only that tree.
pub fn activate_process_loopback(pid: u32, include: bool) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    // PROPVARIANT{VT_BLOB} pointing at `params`. The blob is borrowed, so the PROPVARIANT
    // must never be PropVariantClear'ed -- hence ManuallyDrop (PROPVARIANT's Drop clears).
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }

    let done = std::sync::Arc::new(Event::new(true)?);
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: done.clone() }.into();
    unsafe {
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv as *const PROPVARIANT),
            &handler,
        )?;
        if !done.wait(Duration::from_secs(5)) {
            return Err(windows::core::Error::new(E_FAIL, "process loopback activation timed out"));
        }
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.ok_or_else(|| windows::core::Error::new(E_FAIL, "activation returned no interface"))?
            .cast()
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// One packet read from a capture stream.
pub struct Packet<'a> {
    /// Interleaved stereo samples (zeros if the engine flagged the packet silent).
    pub samples: &'a [f32],
    /// Stream position of the first frame, in 100-ns units on the QPC clock.
    pub qpc_100ns: u64,
    pub flags: u32,
}

/// Event-driven shared-mode capture stream.
pub struct CaptureStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    scratch: Vec<f32>,
}

impl CaptureStream {
    /// Records from a capture endpoint, or (`loopback == true`) from what a render endpoint plays.
    pub fn from_device(dev: &IMMDevice, loopback: bool, buffer_ms: i64) -> Result<Self> {
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        Self::init(activate_client(dev)?, flags, buffer_ms, SAMPLE_RATE)
    }

    /// Process-loopback capture (see [`activate_process_loopback`]) at sample rate `rate`.
    pub fn process_loopback(pid: u32, include: bool, buffer_ms: i64, rate: u32) -> Result<Self> {
        let client = activate_process_loopback(pid, include)?;
        Self::init(client, AUDCLNT_STREAMFLAGS_LOOPBACK, buffer_ms, rate)
    }

    fn init(client: IAudioClient, flags: u32, buffer_ms: i64, rate: u32) -> Result<Self> {
        let fmt = float_format_at(rate);
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_ms * HNS_PER_MS,
                0,
                &fmt,
                None,
            )?;
            let event = Event::new(false)?;
            client.SetEventHandle(event.0)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture, event, scratch: Vec::new() })
        }
    }

    /// Waits until the engine signals new data (or the timeout expires).
    pub fn wait(&self, timeout: Duration) -> bool {
        self.event.wait(timeout)
    }

    /// Drains all currently available packets, calling `f` for each.
    pub fn read(&mut self, mut f: impl FnMut(Packet)) -> Result<()> {
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let n = frames as usize * CHANNELS;
                self.scratch.clear();
                if flags & BUFFERFLAGS_SILENT != 0 || data.is_null() {
                    self.scratch.resize(n, 0.0);
                } else {
                    self.scratch.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
                }
                self.capture.ReleaseBuffer(frames)?;
                f(Packet { samples: &self.scratch, qpc_100ns: qpc, flags });
            }
        }
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Shared-mode, polled render stream.
pub struct RenderStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    pub buffer_frames: u32,
}

impl RenderStream {
    pub fn open(dev: &IMMDevice, buffer_ms: i64) -> Result<Self> {
        Self::open_opts(dev, buffer_ms, false)
    }

    /// `raw == true` requests `AUDCLNT_STREAMOPTIONS_RAW`, which bypasses the endpoint's
    /// signal-processing effects (enhancements such as loudness equalisation or virtual
    /// surround) if the device supports raw mode; otherwise it is silently ignored.
    pub fn open_opts(dev: &IMMDevice, buffer_ms: i64, raw: bool) -> Result<Self> {
        let client = activate_client(dev)?;
        let fmt = float_format();
        unsafe {
            if raw {
                if let Ok(c2) = client.cast::<IAudioClient2>() {
                    let props = AudioClientProperties {
                        cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
                        bIsOffload: false.into(),
                        eCategory: AudioCategory_Other,
                        Options: AUDCLNT_STREAMOPTIONS_RAW,
                    };
                    let _ = c2.SetClientProperties(&props);
                }
            }
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                buffer_ms * HNS_PER_MS,
                0,
                &fmt,
                None,
            )?;
            let buffer_frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            Ok(Self { client, render, buffer_frames })
        }
    }

    pub fn start(&self) -> Result<()> {
        unsafe { self.client.Start() }
    }

    /// Frames queued in the endpoint buffer but not yet played.
    pub fn padding(&self) -> Result<u32> {
        unsafe { self.client.GetCurrentPadding() }
    }

    /// Writes as many whole frames of `interleaved` as currently fit; returns frames written.
    pub fn write(&self, interleaved: &[f32]) -> Result<usize> {
        let frames = interleaved.len() / CHANNELS;
        let free = (self.buffer_frames - self.padding()?) as usize;
        let n = frames.min(free);
        if n == 0 {
            return Ok(0);
        }
        unsafe {
            let p = self.render.GetBuffer(n as u32)? as *mut f32;
            std::ptr::copy_nonoverlapping(interleaved.as_ptr(), p, n * CHANNELS);
            self.render.ReleaseBuffer(n as u32, 0)?;
        }
        Ok(n)
    }
}

impl Drop for RenderStream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Exclusive-mode render
// ---------------------------------------------------------------------------

/// Sample encodings an exclusive-mode device may require.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleKind {
    F32,
    /// 32-bit container, `valid` significant bits.
    I32 { valid: u16 },
    /// Packed 24-bit.
    I24,
    I16,
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            SampleKind::F32 | SampleKind::I32 { .. } => 4,
            SampleKind::I24 => 3,
            SampleKind::I16 => 2,
        }
    }
}

fn extensible(rate: u32, kind: SampleKind) -> WAVEFORMATEXTENSIBLE {
    use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    let bytes = kind.bytes() as u16;
    let block_align = bytes * CHANNELS as u16;
    let valid = match kind {
        SampleKind::I32 { valid } => valid,
        _ => bytes * 8,
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
            nChannels: CHANNELS as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: bytes * 8,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        dwChannelMask: 0x3, // FRONT_LEFT | FRONT_RIGHT
        SubFormat: if kind == SampleKind::F32 { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM },
    }
}

/// Exclusive-mode, timer-driven (polled) render stream.
///
/// Exclusive streams bypass the Windows audio engine's mixer. That matters here: process
/// loopback in EXCLUDE mode taps *every* shared-mode stream on *every* endpoint, so a
/// shared-mode stream into the virtual cable would be captured again and recirculate
/// forever. An exclusive stream is invisible to process loopback.
pub struct ExclusiveRender {
    client: IAudioClient,
    render: IAudioRenderClient,
    pub buffer_frames: u32,
    pub rate: u32,
    pub kind: SampleKind,
    /// Device period actually used (100-ns units).
    pub period_hns: i64,
    /// Minimum period the device supports (100-ns units).
    pub min_period_hns: i64,
    /// Set in event-driven mode: signalled each time the device wants `buffer_frames`.
    event: Option<Event>,
    scratch: Vec<u8>,
}

/// How an exclusive stream is driven.
#[derive(Debug, Clone, Copy)]
pub enum ExclusiveMode {
    /// The app polls `padding()` and writes into a buffer of this length.
    Timer { buffer_ms: i64 },
    /// The device signals every period and the app fills exactly one period
    /// (`buffer_frames`) each time. `None` = the device's minimum period.
    Event { period_hns: Option<i64> },
}

impl ExclusiveRender {
    /// Opens `dev` exclusively, trying common formats (48 kHz first, then 44.1 kHz).
    pub fn open(dev: &IMMDevice, buffer_ms: i64) -> std::result::Result<Self, String> {
        Self::open_mode(dev, ExclusiveMode::Timer { buffer_ms })
    }

    pub fn open_mode(dev: &IMMDevice, mode: ExclusiveMode) -> std::result::Result<Self, String> {
        let probe = activate_client(dev).map_err(|e| e.to_string())?;
        let kinds = [SampleKind::F32, SampleKind::I32 { valid: 32 }, SampleKind::I32 { valid: 24 }, SampleKind::I24, SampleKind::I16];
        let mut chosen = None;
        'outer: for rate in [48_000, 44_100, 96_000] {
            for kind in kinds {
                let f = extensible(rate, kind);
                let hr = unsafe { probe.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &f.Format, None) };
                if hr.is_ok() {
                    chosen = Some((rate, kind));
                    break 'outer;
                }
            }
        }
        drop(probe);
        let (rate, kind) = chosen.ok_or(
            "device supports none of the tried exclusive-mode formats (stereo 48/44.1/96 kHz) \
             or exclusive mode is disabled for it",
        )?;
        let fmt = extensible(rate, kind);
        let mut forced: Option<i64> = None;
        for attempt in 0..2 {
            let client = activate_client(dev).map_err(|e| e.to_string())?;
            unsafe {
                let mut min_period = 0i64;
                client.GetDevicePeriod(None, Some(&mut min_period)).map_err(|e| e.to_string())?;
                let (flags, buffer, period) = match mode {
                    ExclusiveMode::Timer { buffer_ms } => {
                        let period = min_period.max(3 * HNS_PER_MS);
                        (0, forced.unwrap_or(buffer_ms * HNS_PER_MS).max(period), period)
                    }
                    ExclusiveMode::Event { period_hns } => {
                        // event mode requires buffer duration == periodicity
                        let p = forced.unwrap_or(period_hns.unwrap_or(min_period).max(min_period));
                        (AUDCLNT_STREAMFLAGS_EVENTCALLBACK, p, p)
                    }
                };
                let r = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, buffer, period, &fmt.Format, None);
                match r {
                    Ok(()) => {
                        let buffer_frames = client.GetBufferSize().map_err(|e| e.to_string())?;
                        let render: IAudioRenderClient = client.GetService().map_err(|e| e.to_string())?;
                        let event = if flags != 0 {
                            let ev = Event::new(false).map_err(|e| e.to_string())?;
                            client.SetEventHandle(ev.0).map_err(|e| e.to_string())?;
                            Some(ev)
                        } else {
                            None
                        };
                        return Ok(Self {
                            client,
                            render,
                            buffer_frames,
                            rate,
                            kind,
                            period_hns: period,
                            min_period_hns: min_period,
                            event,
                            scratch: Vec::new(),
                        });
                    }
                    Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED && attempt == 0 => {
                        let frames = client.GetBufferSize().map_err(|e| e.to_string())? as i64;
                        forced = Some((10_000_000 * frames + rate as i64 / 2) / rate as i64);
                    }
                    Err(e) if e.code() == AUDCLNT_E_DEVICE_IN_USE => {
                        return Err("output device is in use by another application in exclusive mode".into())
                    }
                    Err(e) => return Err(format!("exclusive Initialize ({rate} Hz {kind:?}): {e}")),
                }
            }
        }
        Err("could not align exclusive-mode buffer".into())
    }

    pub fn start(&self) -> Result<()> {
        unsafe { self.client.Start() }
    }

    pub fn padding(&self) -> Result<u32> {
        unsafe { self.client.GetCurrentPadding() }
    }

    /// Event mode: writes exactly one period (`buffer_frames` frames of interleaved
    /// stereo f32).
    pub fn write_period(&mut self, interleaved: &[f32]) -> Result<()> {
        let n = self.buffer_frames as usize;
        debug_assert_eq!(interleaved.len(), n * CHANNELS);
        self.encode(&interleaved[..n * CHANNELS]);
        unsafe {
            let p = self.render.GetBuffer(n as u32)?;
            std::ptr::copy_nonoverlapping(self.scratch.as_ptr(), p, self.scratch.len());
            self.render.ReleaseBuffer(n as u32, 0)
        }
    }

    fn encode(&mut self, interleaved: &[f32]) {
        self.scratch.clear();
        for &v in interleaved {
            let v = v.clamp(-1.0, 1.0);
            match self.kind {
                SampleKind::F32 => self.scratch.extend_from_slice(&v.to_le_bytes()),
                SampleKind::I32 { .. } => self.scratch.extend_from_slice(&((v as f64 * 2147483647.0) as i32).to_le_bytes()),
                SampleKind::I24 => self.scratch.extend_from_slice(&((v as f64 * 8388607.0) as i32).to_le_bytes()[..3]),
                SampleKind::I16 => self.scratch.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
            }
        }
    }

    /// Event mode: waits until the device wants the next period. Returns false on timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        self.event.as_ref().is_some_and(|e| e.wait(timeout))
    }

    /// Writes as many frames of interleaved stereo f32 as fit; returns frames written.
    pub fn write(&mut self, interleaved: &[f32]) -> Result<usize> {
        let frames = interleaved.len() / CHANNELS;
        let free = (self.buffer_frames - self.padding()?) as usize;
        let n = frames.min(free);
        if n == 0 {
            return Ok(0);
        }
        self.scratch.clear();
        for &v in &interleaved[..n * CHANNELS] {
            let v = v.clamp(-1.0, 1.0);
            match self.kind {
                SampleKind::F32 => self.scratch.extend_from_slice(&v.to_le_bytes()),
                SampleKind::I32 { .. } => self.scratch.extend_from_slice(&((v as f64 * 2147483647.0) as i32).to_le_bytes()),
                SampleKind::I24 => self.scratch.extend_from_slice(&((v as f64 * 8388607.0) as i32).to_le_bytes()[..3]),
                SampleKind::I16 => self.scratch.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
            }
        }
        unsafe {
            let p = self.render.GetBuffer(n as u32)?;
            std::ptr::copy_nonoverlapping(self.scratch.as_ptr(), p, self.scratch.len());
            self.render.ReleaseBuffer(n as u32, 0)?;
        }
        Ok(n)
    }
}

impl Drop for ExclusiveRender {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Current time on the QPC clock in 100-ns units (the clock of `Packet::qpc_100ns`).
pub fn qpc_now_100ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f.max(1) as i128) as u64
}

/// Registers the calling thread with MMCSS ("Pro Audio") so audio deadlines are met
/// under CPU load. Best effort.
pub fn boost_thread_priority() {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut task = 0u32;
    unsafe {
        let _ = AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task);
    }
}

/// Raises the system timer resolution to 1 ms while alive, so `thread::sleep(1 ms)`
/// really sleeps ~1 ms (the default granularity is ~15.6 ms).
pub struct HighResTimer;

impl HighResTimer {
    pub fn new() -> HighResTimer {
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        HighResTimer
    }
}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}
