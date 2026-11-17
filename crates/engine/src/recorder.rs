//! Recording output: takes each canvas frame on the render thread, converts it to NV12 on the GPU and
//! hands it to the video encoder thread; takes each mixed audio block and encodes the enabled tracks;
//! everything goes to the muxer thread. NVENC on the engine's adapter gets textures straight from the GPU
//! (no CPU copy); other encoders read frames back through a staging ring a few frames late so the render
//! thread never waits for the GPU.
use crate::audio::{AudioSink, BLOCK, MAX_TRACKS, SAMPLE_RATE};
use crate::audio_encoder::AudioEncoder;
use crate::compositor::Nv12Target;
use crate::encoding::{self, Backend, VideoEncoderSettings};
use crate::engine::{EngineShared, FrameSink};
use crate::ff::{self, *};
use crate::gpu::Ctx;
use crate::muxer::{ContainerFormat, MuxWriter, Muxer};
use crate::util::{qpc, qpc_to_ms, AtomicF64};
use crate::video_encoder::{borrow_texture, GpuFramePool, VideoEncoder};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

pub const DEFAULT_TRACK_NAMES: [&str; MAX_TRACKS] = ["Mix", "Desktop", "Microphone", "Track 4", "Track 5", "Track 6"];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    pub output_dir: PathBuf,
    /// Tokens: yyyy MM dd HH mm ss.
    pub filename_format: String,
    pub container: ContainerFormat,
    /// After an MKV recording finishes, also write an MP4 copy (stream copy, no re-encode).
    pub remux_to_mp4: bool,
    pub video: VideoEncoderSettings,
    pub audio_codec: String,
    pub audio_bitrate_kbps: u32,
    /// Bit n = record mixer track n as its own audio stream.
    pub tracks: u8,
    pub track_names: Vec<String>,
    /// Delay audio by the measured video latency so sound lines up with the picture.
    pub auto_av_sync: bool,
    /// Extra audio delay in ms (negative = audio earlier).
    pub av_offset_ms: f64,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        let videos = std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Videos")).unwrap_or_else(|| PathBuf::from("."));
        Self {
            output_dir: videos,
            filename_format: "yyyy-MM-dd HH-mm-ss".into(),
            container: ContainerFormat::Mkv,
            remux_to_mp4: false,
            video: VideoEncoderSettings::default(),
            audio_codec: "aac".into(),
            audio_bitrate_kbps: 192,
            tracks: 0b111,
            track_names: DEFAULT_TRACK_NAMES.iter().map(|s| s.to_string()).collect(),
            auto_av_sync: true,
            av_offset_ms: 0.0,
        }
    }
}

#[derive(Default)]
pub struct RecorderStats {
    pub frames_submitted: AtomicU64,
    pub frames_encoded: AtomicU64,
    /// Frames skipped because the encoder fell behind (queue full or texture pool exhausted).
    pub frames_dropped: AtomicU64,
    pub encode_ticks_total: AtomicI64,
    pub encode_ticks_max: AtomicI64,
    pub queue: AtomicUsize,
    pub queue_max: AtomicUsize,
    pub audio_blocks: AtomicU64,
    pub audio_delay_ms: AtomicF64,
    pub paused: AtomicBool,
    pub error: Mutex<Option<String>>,
}
impl RecorderStats {
    pub fn average_encode_ms(&self) -> f64 {
        let n = self.frames_encoded.load(Ordering::Relaxed);
        if n == 0 {
            0.0
        } else {
            qpc_to_ms(self.encode_ticks_total.load(Ordering::Relaxed)) / n as f64
        }
    }
    pub fn max_encode_ms(&self) -> f64 {
        qpc_to_ms(self.encode_ticks_max.load(Ordering::Relaxed))
    }
    fn fail(&self, e: impl std::fmt::Display) {
        self.error.lock().unwrap().get_or_insert_with(|| e.to_string());
    }
}

#[derive(Debug, Clone)]
pub struct RecordingResult {
    pub path: PathBuf,
    pub remuxed: Option<PathBuf>,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub duration_seconds: f64,
    pub bytes: i64,
    pub warnings: Vec<String>,
}

struct Job(Frame, i64);

struct VideoState {
    nv12: Nv12Target,
    pool: Option<GpuFramePool>,
    staging: Vec<(ID3D11Texture2D, Option<i64>)>,
    staging_next: usize,
    tx: Option<Sender<Job>>,
    start_tick: Option<i64>,
    pause_tick: Option<i64>,
    paused_ticks: i64,
    stopped: bool,
    max_queue: usize,
    planar: bool,
}
unsafe impl Send for VideoState {}

struct AudioTrack {
    track: usize,
    stream: usize,
    enc: AudioEncoder,
}

struct AudioState {
    tracks: Vec<AudioTrack>,
    writer: Option<MuxWriter>,
    delay_samples: i64,
    start_sample: Option<i64>,
    /// Paused sample ranges [begin, end); end None while paused.
    pauses: Vec<(i64, Option<i64>)>,
    stop_sample: Option<i64>,
    done: bool,
}

pub struct Recorder {
    engine: Arc<EngineShared>,
    pub path: PathBuf,
    pub settings: RecordingSettings,
    pub encoder_name: String,
    pub gpu_input: bool,
    pub stats: Arc<RecorderStats>,
    video: Mutex<VideoState>,
    audio: Mutex<AudioState>,
    muxer: Mutex<Option<Muxer>>,
    encode_thread: Mutex<Option<JoinHandle<()>>>,
    want_pause: AtomicBool,
    want_stop: AtomicBool,
    started_at: Instant,
    fps: f64,
}

impl Recorder {
    /// Opens encoders and the output file, then attaches to the engine. Frames start with the next tick.
    pub fn start(engine: &Arc<EngineShared>, s: &RecordingSettings) -> Result<Arc<Recorder>> {
        ff::init()?;
        let canvas = engine.canvas();
        let mut vs = s.video.clone();
        vs.fps_num = canvas.fps_num;
        vs.fps_den = canvas.fps_den;
        if vs.width == 0 || vs.height == 0 {
            vs.width = canvas.width;
            vs.height = canvas.height;
        }
        vs.width &= !1;
        vs.height &= !1;
        let info = encoding::get(&vs.encoder_id);
        let gpu_input = info.backend == Backend::Nvenc && engine.gpu.adapter.is_nvidia();

        std::fs::create_dir_all(&s.output_dir).map_err(|e| anyhow!("cannot create {}: {e}", s.output_dir.display()))?;
        let path = unique_path(&s.output_dir, &format_filename(&s.filename_format), s.container.extension());

        let venc = VideoEncoder::new(Some(&engine.gpu), &vs, gpu_input, true, 0)?;
        let pool = venc.frame_pool();
        let planar = venc.planar;
        let mut tracks = Vec::new();
        for t in 0..MAX_TRACKS {
            if s.tracks & (1 << t) != 0 {
                tracks.push(AudioTrack { track: t, stream: 0, enc: AudioEncoder::new(&s.audio_codec, s.audio_bitrate_kbps, true)? });
            }
        }

        let mut mux = Muxer::new(&path, s.container)?;
        let built = (|| -> Result<MuxWriter> {
            mux.add_stream(venc.context(), Some("Video"))?;
            for t in tracks.iter_mut() {
                let name = s.track_names.get(t.track).cloned().unwrap_or_else(|| DEFAULT_TRACK_NAMES[t.track].into());
                t.stream = mux.add_stream(t.enc.context(), Some(&name))?;
            }
            mux.start()
        })();
        let writer = match built {
            Ok(w) => w,
            Err(e) => {
                drop(mux);
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };

        let gpu = &engine.gpu;
        let nv12 = Nv12Target::new(gpu, vs.width, vs.height)?;
        let mut staging = Vec::new();
        if !gpu_input {
            for _ in 0..3 {
                let t = gpu.texture_ex(vs.width, vs.height, DXGI_FORMAT_NV12, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32, 0)?;
                staging.push((t, None));
            }
        }

        let stats = Arc::new(RecorderStats::default());
        let (tx, rx) = channel::<Job>();
        let thread = {
            let stats = stats.clone();
            let writer = writer.clone();
            let mut venc = venc;
            std::thread::Builder::new().name("EBS video encode".into()).spawn(move || {
                let _mm = crate::util::MmcssGuard::capture();
                let tb = venc.time_base();
                let mut out = |p: Packet| writer.write(0, p, tb);
                for Job(frame, pts) in rx {
                    stats.queue.fetch_sub(1, Ordering::Relaxed);
                    if stats.error.lock().unwrap().is_some() {
                        continue;
                    }
                    let t0 = qpc();
                    match venc.encode(frame, pts, &mut out) {
                        Ok(()) => {
                            let dt = qpc() - t0;
                            stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                            stats.encode_ticks_total.fetch_add(dt, Ordering::Relaxed);
                            stats.encode_ticks_max.fetch_max(dt, Ordering::Relaxed);
                        }
                        Err(e) => stats.fail(e),
                    }
                }
                if let Err(e) = venc.flush(&mut out) {
                    stats.fail(e);
                }
            })?
        };

        let max_queue = if gpu_input { 6 } else { 8 };
        let rec = Arc::new(Recorder {
            engine: engine.clone(),
            path,
            settings: s.clone(),
            encoder_name: info.display_name.into(),
            gpu_input,
            stats,
            video: Mutex::new(VideoState {
                nv12,
                pool,
                staging,
                staging_next: 0,
                tx: Some(tx),
                start_tick: None,
                pause_tick: None,
                paused_ticks: 0,
                stopped: false,
                max_queue,
                planar,
            }),
            audio: Mutex::new(AudioState { tracks, writer: Some(writer), delay_samples: 0, start_sample: None, pauses: Vec::new(), stop_sample: None, done: false }),
            muxer: Mutex::new(Some(mux)),
            encode_thread: Mutex::new(Some(thread)),
            want_pause: AtomicBool::new(false),
            want_stop: AtomicBool::new(false),
            started_at: Instant::now(),
            fps: vs.fps(),
        });
        engine.audio.add_sink(rec.clone());
        engine.add_sink(rec.clone());
        Ok(rec)
    }

    pub fn set_paused(&self, p: bool) {
        self.want_pause.store(p, Ordering::SeqCst);
    }
    pub fn is_paused(&self) -> bool {
        self.stats.paused.load(Ordering::Relaxed)
    }
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
    /// Seconds of video recorded so far (excludes pauses and dropped frames are still counted as time).
    pub fn recorded_seconds(&self) -> f64 {
        self.stats.frames_submitted.load(Ordering::Relaxed) as f64 / self.fps
    }
    pub fn bytes_written(&self) -> i64 {
        self.muxer.lock().unwrap().as_ref().map(|m| m.stats.bytes_written.load(Ordering::Relaxed)).unwrap_or(0)
    }
    pub fn error(&self) -> Option<String> {
        let e = self.stats.error.lock().unwrap().clone();
        e.or_else(|| self.muxer.lock().unwrap().as_ref().and_then(|m| m.error()))
    }

    /// Stops at the next frame, waits for the audio up to that point, flushes the encoders and closes the
    /// file (then remuxes if asked).
    pub fn stop(self: &Arc<Self>) -> Result<RecordingResult> {
        self.want_stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.video.lock().unwrap().stopped && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let me: Arc<dyn FrameSink> = self.clone();
        self.engine.remove_sink(&me);
        {
            // If the render loop never delivered the stop frame, end here.
            let mut v = self.video.lock().unwrap();
            if !v.stopped {
                v.stopped = true;
                v.tx = None;
                let mut a = self.audio.lock().unwrap();
                if a.stop_sample.is_none() {
                    a.stop_sample = Some(self.engine.audio.clock.now() - a.delay_samples);
                }
            }
        }
        // Audio for the last frame arrives one mix delay (+ A/V delay) later.
        let wait = Duration::from_millis(1500) + Duration::from_secs_f64((self.stats.audio_delay_ms.load().max(0.0)) / 1000.0);
        let deadline = Instant::now() + wait;
        while !self.audio.lock().unwrap().done && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ame: Arc<dyn AudioSink> = self.clone();
        self.engine.audio.remove_sink(&ame);

        if let Some(t) = self.encode_thread.lock().unwrap().take() {
            let _ = t.join();
        }
        {
            let mut a = self.audio.lock().unwrap();
            if let Some(w) = a.writer.take() {
                for t in a.tracks.iter_mut() {
                    let st = t.stream;
                    let tb = t.enc.time_base();
                    if let Err(e) = t.enc.flush(&mut |p| w.write(st, p, tb)) {
                        self.stats.fail(e);
                    }
                }
            }
        }
        let mut mux = self.muxer.lock().unwrap().take().ok_or_else(|| anyhow!("recording already stopped"))?;
        mux.finish();
        let mut warnings = Vec::new();
        if let Some(w) = mux.stats.first_warning.lock().unwrap().clone() {
            warnings.push(w);
        }
        let bytes = mux.stats.bytes_written.load(Ordering::Relaxed);
        let mux_err = mux.error();
        drop(mux);
        if let Some(e) = self.stats.error.lock().unwrap().clone().or(mux_err) {
            return Err(anyhow!("recording failed: {e} (partial file: {})", self.path.display()));
        }
        let mut remuxed = None;
        if self.settings.remux_to_mp4 && self.settings.container == ContainerFormat::Mkv {
            let out = unique_path(self.path.parent().unwrap_or(Path::new(".")), &self.path.file_stem().unwrap_or_default().to_string_lossy(), ".mp4");
            match crate::muxer::remux(&self.path, &out) {
                Ok(()) => remuxed = Some(out),
                Err(e) => {
                    let _ = std::fs::remove_file(&out);
                    warnings.push(format!("MP4 remux failed (the MKV is fine): {e}"));
                }
            }
        }
        let frames = self.stats.frames_encoded.load(Ordering::Relaxed);
        Ok(RecordingResult {
            path: self.path.clone(),
            remuxed,
            frames_encoded: frames,
            frames_dropped: self.stats.frames_dropped.load(Ordering::Relaxed),
            duration_seconds: self.recorded_seconds(),
            bytes,
            warnings,
        })
    }

    fn submit_gpu(&self, ctx: &Ctx, v: &mut VideoState, pts: i64) {
        let Some(pool) = v.pool.as_ref() else { return };
        let Some((frame, tex, slice)) = pool.acquire() else {
            self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(dst) = borrow_texture(&tex) else {
            self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        unsafe { ctx.CopySubresourceRegion(dst, slice, 0, 0, 0, &v.nv12.texture, 0, None) };
        self.send(v, frame, pts);
    }

    fn submit_cpu(&self, ctx: &Ctx, v: &mut VideoState, pts: i64) {
        let i = v.staging_next;
        v.staging_next = (i + 1) % v.staging.len();
        if let Some(old) = v.staging[i].1.take() {
            self.read_staging(ctx, v, i, old);
        }
        unsafe { ctx.CopyResource(&v.staging[i].0, &v.nv12.texture) };
        v.staging[i].1 = Some(pts);
    }

    /// Sends every frame still waiting in the staging ring (oldest first).
    fn drain_staging(&self, ctx: &Ctx, v: &mut VideoState) {
        let n = v.staging.len();
        for k in 0..n {
            let i = (v.staging_next + k) % n;
            if let Some(pts) = v.staging[i].1.take() {
                self.read_staging(ctx, v, i, pts);
            }
        }
    }

    fn read_staging(&self, ctx: &Ctx, v: &mut VideoState, i: usize, pts: i64) {
        let (w, h) = (v.nv12.width as usize, v.nv12.height as usize);
        let frame = match alloc_frame(w as i32, h as i32, v.planar) {
            Ok(f) => f,
            Err(e) => return self.stats.fail(e),
        };
        unsafe {
            let tex = &v.staging[i].0;
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut m)) {
                return self.stats.fail(format!("map staging: {e}"));
            }
            let src = m.pData as *const u8;
            let pitch = m.RowPitch as usize;
            let data: [*mut u8; 8] = get(frame.0, AVFrame_data);
            let ls: [i32; 8] = get(frame.0, AVFrame_linesize);
            for y in 0..h {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), data[0].add(y * ls[0] as usize), w);
            }
            let uv = src.add(pitch * h);
            if v.planar {
                for y in 0..h / 2 {
                    let row = std::slice::from_raw_parts(uv.add(y * pitch), w);
                    let du = std::slice::from_raw_parts_mut(data[1].add(y * ls[1] as usize), w / 2);
                    let dv = std::slice::from_raw_parts_mut(data[2].add(y * ls[2] as usize), w / 2);
                    for x in 0..w / 2 {
                        du[x] = row[2 * x];
                        dv[x] = row[2 * x + 1];
                    }
                }
            } else {
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(uv.add(y * pitch), data[1].add(y * ls[1] as usize), w);
                }
            }
            ctx.Unmap(tex, 0);
        }
        self.send(v, frame, pts);
    }

    fn send(&self, v: &mut VideoState, frame: Frame, pts: i64) {
        let Some(tx) = v.tx.as_ref() else { return };
        let q = self.stats.queue.fetch_add(1, Ordering::Relaxed) + 1;
        self.stats.queue_max.fetch_max(q, Ordering::Relaxed);
        if tx.send(Job(frame, pts)).is_err() {
            self.stats.queue.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl FrameSink for Recorder {
    fn on_frame(&self, ctx: &Ctx, _canvas: &ID3D11Texture2D, canvas_srv: &ID3D11ShaderResourceView, tick: i64, tick_qpc: i64) {
        let mut guard = self.video.lock().unwrap();
        let v = &mut *guard;
        if v.stopped {
            return;
        }
        let clock = self.engine.audio.clock;
        if v.start_tick.is_none() {
            v.start_tick = Some(tick);
            let latency = if self.settings.auto_av_sync { self.engine.video_latency_ms() } else { 0.0 };
            let delay_ms = latency + self.settings.av_offset_ms;
            self.stats.audio_delay_ms.store(delay_ms);
            let mut a = self.audio.lock().unwrap();
            a.delay_samples = (delay_ms * SAMPLE_RATE as f64 / 1000.0).round() as i64;
            a.start_sample = Some(clock.sample_position(tick_qpc) - a.delay_samples);
        }
        let want_pause = self.want_pause.load(Ordering::SeqCst);
        if want_pause && v.pause_tick.is_none() {
            v.pause_tick = Some(tick);
            let mut a = self.audio.lock().unwrap();
            let s = clock.sample_position(tick_qpc) - a.delay_samples;
            a.pauses.push((s, None));
            self.stats.paused.store(true, Ordering::Relaxed);
        } else if !want_pause {
            if let Some(p) = v.pause_tick.take() {
                v.paused_ticks += tick - p;
                let mut a = self.audio.lock().unwrap();
                let s = clock.sample_position(tick_qpc) - a.delay_samples;
                if let Some(last) = a.pauses.last_mut() {
                    last.1 = Some(s.max(last.0));
                }
                self.stats.paused.store(false, Ordering::Relaxed);
            }
        }
        if self.want_stop.load(Ordering::SeqCst) {
            if !v.staging.is_empty() {
                self.drain_staging(ctx, v);
            }
            v.stopped = true;
            v.tx = None; // closes the encoder queue: the encode thread flushes and exits
            let mut a = self.audio.lock().unwrap();
            let s = clock.sample_position(tick_qpc) - a.delay_samples;
            a.stop_sample = Some(s);
            return;
        }
        if v.pause_tick.is_some() {
            return;
        }
        let pts = tick - v.start_tick.unwrap() - v.paused_ticks;
        self.stats.frames_submitted.fetch_add(1, Ordering::Relaxed);
        if self.stats.queue.load(Ordering::Relaxed) >= v.max_queue {
            self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let cs = self.settings.video.color_space;
        let cr = self.settings.video.color_range;
        self.engine.compositor.convert_to_nv12(ctx, canvas_srv, &v.nv12, cs, cr);
        if v.pool.is_some() {
            self.submit_gpu(ctx, v, pts);
        } else {
            self.submit_cpu(ctx, v, pts);
        }
    }
}

impl AudioSink for Recorder {
    fn on_block(&self, start: i64, l: &[Vec<f32>; MAX_TRACKS], r: &[Vec<f32>; MAX_TRACKS]) {
        let mut guard = self.audio.lock().unwrap();
        let a = &mut *guard;
        if a.done {
            return;
        }
        let Some(first) = a.start_sample else { return };
        let end = start + BLOCK as i64;
        let lo = start.max(first);
        let hi = a.stop_sample.map_or(end, |s| end.min(s));
        if hi > lo {
            let Some(w) = a.writer.clone() else { return };
            for (seg_lo, seg_hi, paused_before) in unpaused_segments(&a.pauses, lo, hi) {
                let (i0, i1) = ((seg_lo - start) as usize, (seg_hi - start) as usize);
                let pts = seg_lo - first - paused_before;
                for t in a.tracks.iter_mut() {
                    let (st, tb) = (t.stream, t.enc.time_base());
                    if let Err(e) = t.enc.encode(&l[t.track][i0..i1], &r[t.track][i0..i1], pts, &mut |p| w.write(st, p, tb)) {
                        self.stats.fail(e);
                    }
                }
            }
            self.stats.audio_blocks.fetch_add(1, Ordering::Relaxed);
        }
        if a.stop_sample.is_some_and(|s| end >= s) {
            a.done = true;
        }
    }
}

/// Splits [lo, hi) into the parts outside pauses, each with the total paused length before it.
fn unpaused_segments(pauses: &[(i64, Option<i64>)], lo: i64, hi: i64) -> Vec<(i64, i64, i64)> {
    let mut out = Vec::new();
    let mut cur = lo;
    let mut paused_before = 0i64;
    for &(b, e) in pauses {
        let e = e.unwrap_or(i64::MAX);
        if e <= cur {
            paused_before += e - b;
            continue;
        }
        if b >= hi {
            break;
        }
        if b > cur {
            out.push((cur, b, paused_before));
        }
        if e == i64::MAX {
            return out;
        }
        paused_before += e - b;
        cur = cur.max(e);
        if cur >= hi {
            return out;
        }
    }
    if cur < hi {
        out.push((cur, hi, paused_before));
    }
    out
}

fn alloc_frame(w: i32, h: i32, planar: bool) -> Result<Frame> {
    unsafe {
        let f = av_frame_alloc();
        let fr = Frame(f);
        set(f, AVFrame_format, if planar { AV_PIX_FMT_YUV420P } else { AV_PIX_FMT_NV12 });
        set(f, AVFrame_width, w);
        set(f, AVFrame_height, h);
        check(av_frame_get_buffer(f, 32), "alloc video frame")?;
        Ok(fr)
    }
}

/// Expands yyyy MM dd HH mm ss with local time and removes characters Windows forbids in file names.
pub fn format_filename(fmt: &str) -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let mut s = if fmt.trim().is_empty() { "yyyy-MM-dd HH-mm-ss".to_string() } else { fmt.to_string() };
    for (tok, val) in [
        ("yyyy", format!("{:04}", t.wYear)),
        ("MM", format!("{:02}", t.wMonth)),
        ("dd", format!("{:02}", t.wDay)),
        ("HH", format!("{:02}", t.wHour)),
        ("mm", format!("{:02}", t.wMinute)),
        ("ss", format!("{:02}", t.wSecond)),
    ] {
        s = s.replace(tok, &val);
    }
    let clean: String = s.chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '-' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() {
        "Recording".into()
    } else {
        clean
    }
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let p = dir.join(format!("{stem}{ext}"));
    if !p.exists() {
        return p;
    }
    (2..10000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn segments() {
        assert_eq!(unpaused_segments(&[], 0, 100), vec![(0, 100, 0)]);
        assert_eq!(unpaused_segments(&[(10, Some(20))], 0, 100), vec![(0, 10, 0), (20, 100, 10)]);
        assert_eq!(unpaused_segments(&[(10, None)], 0, 100), vec![(0, 10, 0)]);
        assert_eq!(unpaused_segments(&[(10, Some(20))], 50, 100), vec![(50, 100, 10)]);
        assert_eq!(unpaused_segments(&[(0, Some(200))], 50, 100), vec![]);
        assert!(format_filename("a:b yyyy").starts_with("a-b 2"));
    }
}
