//! Audio: WASAPI capture (desktop loopback, microphones) into absolute-time ring buffers on a shared
//! 48 kHz clock derived from QPC, and a mixer that produces up to six tracks in fixed 1024-sample blocks.
//! Every source is placed on the timeline by WASAPI's own QPC timestamp, which keeps mic, game audio and
//! video locked together.
use crate::util::{qpc, qpc_freq, AtomicF64, MmcssGuard};
use anyhow::{anyhow, Context as _, Result};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};

pub const SAMPLE_RATE: u32 = 48000;
pub const BLOCK: usize = 1024;
pub const MAX_TRACKS: usize = 6;

/// 48 kHz sample clock derived from QueryPerformanceCounter; shared by audio and video.
#[derive(Clone, Copy, Debug)]
pub struct AudioClock {
    pub epoch_qpc: i64,
}
impl AudioClock {
    pub fn sample_position(&self, q: i64) -> i64 {
        ((q - self.epoch_qpc) as i128 * SAMPLE_RATE as i128 / qpc_freq() as i128) as i64
    }
    pub fn qpc_of_sample(&self, s: i64) -> i64 {
        self.epoch_qpc + (s as i128 * qpc_freq() as i128 / SAMPLE_RATE as i128) as i64
    }
    pub fn now(&self) -> i64 {
        self.sample_position(qpc())
    }
}

const RING: usize = 1 << 16; // 1.36 s
const MASK: usize = RING - 1;

/// Stereo ring addressed by absolute sample position. Writers place audio where it belongs in time;
/// the mixer reads fixed windows. Gaps read as silence; audio arriving after its window was mixed is dropped.
pub struct AudioRing {
    inner: Mutex<RingInner>,
    pub resyncs: AtomicU64,
}
struct RingInner {
    l: Vec<f32>,
    r: Vec<f32>,
    write_pos: i64,
    read_pos: i64,
}
/// Packets within 40 ms of the expected position continue contiguously (absorbs timestamp jitter).
const RESYNC: i64 = SAMPLE_RATE as i64 * 40 / 1000;

impl Default for AudioRing {
    fn default() -> Self {
        Self { inner: Mutex::new(RingInner { l: vec![0.0; RING], r: vec![0.0; RING], write_pos: -1, read_pos: 0 }), resyncs: AtomicU64::new(0) }
    }
}

impl AudioRing {
    /// Writes interleaved stereo frames whose first sample was captured at `pos`.
    pub fn write(&self, interleaved: &[f32], pos: i64) {
        let frames = interleaved.len() / 2;
        let mut g = self.inner.lock().unwrap();
        if g.write_pos < 0 || (pos - g.write_pos).abs() > RESYNC {
            if g.write_pos >= 0 {
                self.resyncs.fetch_add(1, Ordering::Relaxed);
            }
            g.write_pos = pos;
        }
        let start = g.write_pos;
        let skip = if start < g.read_pos { ((g.read_pos - start) as usize).min(frames) } else { 0 };
        let mut n = frames;
        if start + frames as i64 - g.read_pos > RING as i64 {
            n = (RING as i64 - (start - g.read_pos)).max(0) as usize; // far ahead: drop overflow
        }
        for i in skip..n.min(frames) {
            let idx = (start + i as i64) as usize & MASK;
            g.l[idx] = interleaved[2 * i];
            g.r[idx] = interleaved[2 * i + 1];
        }
        g.write_pos = start + frames as i64;
    }

    /// Reads (and clears) the window [pos, pos+len). Unwritten samples are silence.
    pub fn read(&self, left: &mut [f32], right: &mut [f32], pos: i64) {
        let mut g = self.inner.lock().unwrap();
        for i in 0..left.len() {
            let idx = (pos + i as i64) as usize & MASK;
            left[i] = std::mem::take(&mut g.l[idx]);
            right[i] = std::mem::take(&mut g.r[idx]);
        }
        g.read_pos = pos + left.len() as i64;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind {
    DesktopAudio,
    Microphone,
}

#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub is_input: bool,
}

struct ComGuard;
impl ComGuard {
    fn new() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        ComGuard
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn device_name(d: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = d.OpenPropertyStore(STGM_READ) else { return String::new() };
        let Ok(v) = store.GetValue(&PKEY_Device_FriendlyName) else { return String::new() };
        v.to_string()
    }
}

pub fn list_devices(input: bool) -> Result<Vec<AudioDeviceInfo>> {
    let _com = ComGuard::new();
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let flow = if input { eCapture } else { eRender };
        let role = if input { eCommunications } else { eConsole };
        let def = en.GetDefaultAudioEndpoint(flow, role).ok().and_then(|d| d.GetId().ok()).map(pwstr_to_string);
        let coll = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = pwstr_to_string(d.GetId()?);
            out.push(AudioDeviceInfo { is_default: Some(&id) == def.as_ref(), name: device_name(&d), id, is_input: input });
        }
        Ok(out)
    }
}

/// f32 in an atomic.
#[derive(Default)]
pub struct AtomicF32(AtomicU32);
impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// One WASAPI capture converted to 48 kHz stereo float and written into its ring at capture time.
pub struct AudioSource {
    pub id: u64,
    pub name: Mutex<String>,
    pub kind: AudioSourceKind,
    /// None = default device (follows the Windows default).
    pub device_id: Option<String>,
    pub ring: AudioRing,
    /// Linear fader gain 0..~4.
    pub volume: AtomicF32,
    pub muted: AtomicBool,
    /// Bit i set = this source goes to recording track i+1.
    pub track_mask: AtomicU32,
    pub peak_l: AtomicF32,
    pub peak_r: AtomicF32,
    pub status: Mutex<String>,
    pub format: Mutex<String>,
    pub packets: AtomicU64,
    first_qpc: AtomicI64,
    last_qpc: AtomicI64,
    frames_since_first: AtomicU64,
    stop: AtomicBool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl AudioSource {
    pub fn desktop(clock: AudioClock, device_id: Option<String>, name: &str) -> Arc<AudioSource> {
        Self::start(clock, AudioSourceKind::DesktopAudio, device_id, name, 0b011)
    }
    pub fn microphone(clock: AudioClock, device_id: Option<String>, name: &str) -> Arc<AudioSource> {
        Self::start(clock, AudioSourceKind::Microphone, device_id, name, 0b101)
    }

    fn start(clock: AudioClock, kind: AudioSourceKind, device_id: Option<String>, name: &str, tracks: u32) -> Arc<AudioSource> {
        let s = Arc::new(AudioSource {
            id: crate::scene::next_id(),
            name: Mutex::new(name.to_string()),
            kind,
            device_id,
            ring: AudioRing::default(),
            volume: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            track_mask: AtomicU32::new(tracks),
            peak_l: AtomicF32::default(),
            peak_r: AtomicF32::default(),
            status: Mutex::new("Starting".into()),
            format: Mutex::new(String::new()),
            packets: AtomicU64::new(0),
            first_qpc: AtomicI64::new(0),
            last_qpc: AtomicI64::new(0),
            frames_since_first: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let s2 = s.clone();
        let t = std::thread::Builder::new()
            .name(format!("EBS audio {name}"))
            .spawn(move || {
                if let Err(e) = s2.capture_loop(clock) {
                    *s2.status.lock().unwrap() = format!("Stopped: {e}");
                }
            })
            .ok();
        *s.thread.lock().unwrap() = t;
        s
    }

    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }
    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    /// Measured input sample rate against QPC (device clock drift diagnostics).
    pub fn measured_rate(&self) -> f64 {
        let (a, b, n) = (self.first_qpc.load(Ordering::Relaxed), self.last_qpc.load(Ordering::Relaxed), self.frames_since_first.load(Ordering::Relaxed));
        if n == 0 || a == b {
            0.0
        } else {
            n as f64 * qpc_freq() as f64 / (b - a) as f64
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        *self.status.lock().unwrap() = "Removed".into();
    }

    fn capture_loop(&self, clock: AudioClock) -> Result<()> {
        let _com = ComGuard::new();
        let _mm = MmcssGuard::pro_audio();
        let loopback = self.kind == AudioSourceKind::DesktopAudio;
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = match &self.device_id {
                Some(id) => {
                    let w: Vec<u16> = id.encode_utf16().chain([0]).collect();
                    en.GetDevice(PCWSTR(w.as_ptr()))?
                }
                None if loopback => en.GetDefaultAudioEndpoint(eRender, eConsole)?,
                None => en.GetDefaultAudioEndpoint(eCapture, eCommunications)?,
            };
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let base_flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            // Ask Windows for 48 kHz stereo float directly (its resampler); fall back to the device format.
            let want = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: 2,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let buffer_hns = 200_000; // 20 ms
            let fmt: Format;
            if client.Initialize(AUDCLNT_SHAREMODE_SHARED, base_flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, buffer_hns, 0, &want, None).is_ok() {
                fmt = Format::from_wfx(&want);
            } else {
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, base_flags, buffer_hns, 0, mix, None).context("IAudioClient::Initialize")?;
                fmt = Format::from_wfx(&*mix);
            }
            CoTaskMemFree(Some(mix as *const _));
            *self.format.lock().unwrap() = fmt.describe();
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            *self.status.lock().unwrap() = "Active".into();
            let mut resampler = (fmt.rate != SAMPLE_RATE).then(|| Resampler::new(fmt.rate));
            let mut conv: Vec<f32> = Vec::new();
            let mut out: Vec<f32> = Vec::new();
            let freq = qpc_freq();
            while !self.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
                loop {
                    let n = cap.GetNextPacketSize()?;
                    if n == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    let mut qpc_pos = 0u64;
                    cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc_pos))?;
                    let now = qpc();
                    if frames > 0 {
                        conv.clear();
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            conv.resize(frames as usize * 2, 0.0);
                        } else {
                            fmt.to_stereo_f32(std::slice::from_raw_parts(data, frames as usize * fmt.block_align), &mut conv);
                        }
                    }
                    cap.ReleaseBuffer(frames)?;
                    if frames == 0 {
                        continue;
                    }
                    self.packets.fetch_add(1, Ordering::Relaxed);
                    if self.first_qpc.load(Ordering::Relaxed) == 0 {
                        self.first_qpc.store(now, Ordering::Relaxed);
                    } else {
                        self.frames_since_first.fetch_add(frames as u64, Ordering::Relaxed);
                    }
                    self.last_qpc.store(now, Ordering::Relaxed);
                    // WASAPI's QPC position (100 ns units) is when the packet's first sample was captured.
                    let first_qpc = if qpc_pos != 0 { (qpc_pos as i128 * freq as i128 / 10_000_000) as i64 } else { now - (frames as i64 * freq / fmt.rate as i64) };
                    let pos = clock.sample_position(first_qpc);
                    match resampler.as_mut() {
                        None => self.ring.write(&conv, pos),
                        Some(r) => {
                            out.clear();
                            r.process(&conv, &mut out);
                            self.ring.write(&out, pos);
                        }
                    }
                }
            }
            let _ = client.Stop();
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

struct Format {
    rate: u32,
    channels: usize,
    block_align: usize,
    kind: SampleKind,
}
impl Format {
    unsafe fn from_wfx(w: *const WAVEFORMATEX) -> Format {
        let w = &*w;
        let mut float = w.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT;
        if w.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && w.cbSize >= 22 {
            let ext = &*(w as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE);
            let sub: GUID = ext.SubFormat;
            float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            let _ = sub == KSDATAFORMAT_SUBTYPE_PCM;
        }
        let kind = match (float, w.wBitsPerSample) {
            (true, _) => SampleKind::F32,
            (false, 16) => SampleKind::I16,
            (false, 24) => SampleKind::I24,
            _ => SampleKind::I32,
        };
        Format { rate: w.nSamplesPerSec, channels: w.nChannels.max(1) as usize, block_align: w.nBlockAlign as usize, kind }
    }
    fn describe(&self) -> String {
        let k = match self.kind {
            SampleKind::F32 => "32-bit float",
            SampleKind::I16 => "16-bit",
            SampleKind::I24 => "24-bit",
            SampleKind::I32 => "32-bit",
        };
        format!("{} Hz, {} ch, {k}", self.rate, self.channels)
    }
    fn sample(&self, b: &[u8], frame: usize, ch: usize) -> f32 {
        let bytes = self.block_align / self.channels;
        let o = frame * self.block_align + ch * bytes;
        match self.kind {
            SampleKind::F32 => f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]),
            SampleKind::I16 => i16::from_le_bytes([b[o], b[o + 1]]) as f32 / 32768.0,
            SampleKind::I24 => (((b[o] as i32) << 8 | (b[o + 1] as i32) << 16 | (b[o + 2] as i32) << 24) >> 8) as f32 / 8_388_608.0,
            SampleKind::I32 => i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as f32 / 2_147_483_648.0,
        }
    }
    fn to_stereo_f32(&self, b: &[u8], out: &mut Vec<f32>) {
        let frames = b.len() / self.block_align;
        out.reserve(frames * 2);
        for f in 0..frames {
            let l = self.sample(b, f, 0);
            let r = if self.channels > 1 { self.sample(b, f, 1) } else { l };
            out.push(l);
            out.push(r);
        }
    }
}

/// Linear-interpolating stereo resampler to 48 kHz (only used when Windows can't convert for us).
struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
}
impl Resampler {
    fn new(rate: u32) -> Self {
        Self { step: rate as f64 / SAMPLE_RATE as f64, pos: 0.0, prev: [0.0; 2] }
    }
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / 2;
        let at = |i: isize, c: usize, prev: [f32; 2]| if i < 0 { prev[c] } else { input[i as usize * 2 + c] };
        while self.pos < n as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..2 {
                let a = at(i - 1, c, self.prev);
                let b = at(i, c, self.prev);
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.prev = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
        }
    }
}

/// Receives each mixed block on the mixer thread: (block start sample, per-track L, per-track R).
pub trait AudioSink: Send + Sync {
    fn on_block(&self, start: i64, l: &[Vec<f32>; MAX_TRACKS], r: &[Vec<f32>; MAX_TRACKS]);
}

pub struct AudioMixer {
    pub clock: AudioClock,
    sources: Mutex<Vec<Arc<AudioSource>>>,
    sinks: Mutex<Vec<Arc<dyn AudioSink>>>,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// How far behind real time blocks are mixed (absorbs WASAPI packet jitter).
    pub mix_delay_samples: AtomicI64,
    pub blocks_mixed: AtomicU64,
    pub late_blocks: AtomicU64,
    pub mix_ms: AtomicF64,
}

impl AudioMixer {
    pub fn new() -> AudioMixer {
        AudioMixer {
            clock: AudioClock { epoch_qpc: qpc() },
            sources: Mutex::new(Vec::new()),
            sinks: Mutex::new(Vec::new()),
            running: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
            mix_delay_samples: AtomicI64::new(SAMPLE_RATE as i64 * 50 / 1000),
            blocks_mixed: AtomicU64::new(0),
            late_blocks: AtomicU64::new(0),
            mix_ms: AtomicF64::default(),
        }
    }

    /// Starts the mixer thread (the mixer must live in an Arc, e.g. inside the engine).
    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        *self.thread.lock().unwrap() = std::thread::Builder::new().name("EBS audio mix".into()).spawn(move || me.run()).ok();
    }

    pub fn sources(&self) -> Vec<Arc<AudioSource>> {
        self.sources.lock().unwrap().clone()
    }
    pub fn add(&self, s: Arc<AudioSource>) {
        self.sources.lock().unwrap().push(s);
    }
    pub fn remove(&self, id: u64) {
        let gone: Vec<_> = {
            let mut v = self.sources.lock().unwrap();
            let (g, k): (Vec<_>, Vec<_>) = v.drain(..).partition(|s| s.id == id);
            *v = k;
            g
        };
        for s in gone {
            s.stop();
        }
    }
    pub fn add_sink(&self, s: Arc<dyn AudioSink>) {
        self.sinks.lock().unwrap().push(s);
    }
    pub fn remove_sink(&self, s: &Arc<dyn AudioSink>) {
        self.sinks.lock().unwrap().retain(|x| !Arc::ptr_eq(x, s));
    }

    fn run(&self) {
        let _mm = MmcssGuard::pro_audio();
        let mut next = self.clock.now() / BLOCK as i64 * BLOCK as i64;
        let mut tl: [Vec<f32>; MAX_TRACKS] = Default::default();
        let mut tr: [Vec<f32>; MAX_TRACKS] = Default::default();
        for t in 0..MAX_TRACKS {
            tl[t] = vec![0.0; BLOCK];
            tr[t] = vec![0.0; BLOCK];
        }
        let (mut sl, mut sr) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        let mut gains: std::collections::HashMap<u64, f32> = Default::default();
        while self.running.load(Ordering::Relaxed) {
            let ready = self.clock.now() - self.mix_delay_samples.load(Ordering::Relaxed);
            if ready < next + BLOCK as i64 {
                let wait_ms = (next + BLOCK as i64 - ready) * 1000 / SAMPLE_RATE as i64;
                std::thread::sleep(Duration::from_millis(wait_ms.clamp(1, 20) as u64));
                continue;
            }
            if ready - next > SAMPLE_RATE as i64 {
                // Stalled > 1 s: skip ahead.
                self.late_blocks.fetch_add(1, Ordering::Relaxed);
                next = ready / BLOCK as i64 * BLOCK as i64 - BLOCK as i64;
            }
            let t0 = qpc();
            for t in 0..MAX_TRACKS {
                tl[t].fill(0.0);
                tr[t].fill(0.0);
            }
            for s in self.sources() {
                s.ring.read(&mut sl, &mut sr, next);
                let target = if s.muted.load(Ordering::Relaxed) { 0.0 } else { s.volume.load() };
                let g0 = *gains.get(&s.id).unwrap_or(&target);
                let (mut pl, mut pr) = (0f32, 0f32);
                for i in 0..BLOCK {
                    // Ramp gain across the block: no clicks when moving faders or muting.
                    let g = g0 + (target - g0) * (i + 1) as f32 / BLOCK as f32;
                    sl[i] *= g;
                    sr[i] *= g;
                    pl = pl.max(sl[i].abs());
                    pr = pr.max(sr[i].abs());
                }
                gains.insert(s.id, target);
                s.peak_l.store(pl);
                s.peak_r.store(pr);
                let mask = s.track_mask.load(Ordering::Relaxed);
                for t in 0..MAX_TRACKS {
                    if mask & (1 << t) == 0 {
                        continue;
                    }
                    for i in 0..BLOCK {
                        tl[t][i] += sl[i];
                        tr[t][i] += sr[i];
                    }
                }
            }
            self.blocks_mixed.fetch_add(1, Ordering::Relaxed);
            let sinks = self.sinks.lock().unwrap().clone();
            for k in sinks {
                k.on_block(next, &tl, &tr);
            }
            self.mix_ms.store(crate::util::qpc_to_ms(qpc() - t0));
            next += BLOCK as i64;
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        for s in self.sources.lock().unwrap().drain(..) {
            s.stop();
        }
    }
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn err_unsupported() -> anyhow::Error {
    anyhow!("unsupported audio format")
}
