//! libavcodec video encoder. For NVENC on the engine's own adapter the input is a pool of D3D11 NV12
//! textures (zero CPU copies, like OBS's texture encoder). Other encoders receive NV12 frames read back
//! from the GPU.
use crate::compositor::{ColorRange, ColorSpace};
use crate::encoding::{self, Backend, EncoderInfo, Multipass, RateControl, VideoEncoderSettings, VideoCodec};
use crate::ff::{self, *};
use crate::gpu::Gpu;
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use windows::core::Interface;

pub struct VideoEncoder {
    ctx: *mut AVCodecContext,
    hw_device: *mut AVBufferRef,
    hw_frames: *mut AVBufferRef,
    pkt: *mut AVPacket,
    pub info: &'static EncoderInfo,
    pub settings: VideoEncoderSettings,
    pub gpu_input: bool,
    pub pool_size: i32,
    /// System-memory frames are YUV420P (three planes) instead of NV12 (SVT-AV1 only takes planar input).
    pub planar: bool,
    /// Options the encoder did not recognise (a bug in our option table if not empty).
    pub unused_options: Vec<String>,
}
unsafe impl Send for VideoEncoder {}

/// The encoder's D3D11 texture pool, usable from the render thread while the encoder runs on its own thread
/// (AVBufferPool is thread-safe).
pub struct GpuFramePool(*mut AVBufferRef);
unsafe impl Send for GpuFramePool {}
unsafe impl Sync for GpuFramePool {}
impl Drop for GpuFramePool {
    fn drop(&mut self) {
        unsafe { av_buffer_unref(&mut self.0) }
    }
}
impl GpuFramePool {
    /// An empty frame from the pool, or None when the pool is exhausted (encoder lagging).
    /// Returns (frame, texture pointer, array slice).
    pub fn acquire(&self) -> Option<(Frame, *mut c_void, u32)> {
        unsafe {
            let f = av_frame_alloc();
            if av_hwframe_get_buffer(self.0, f, 0) < 0 {
                let mut f = f;
                av_frame_free(&mut f);
                return None;
            }
            let data: [*mut u8; 8] = get(f, AVFrame_data);
            Some((Frame(f), data[0] as *mut c_void, data[1] as usize as u32))
        }
    }
}

/// Pixel format for system-memory input.
pub fn cpu_pix_fmt(info: &EncoderInfo) -> i32 {
    if info.backend == Backend::SvtAv1 {
        AV_PIX_FMT_YUV420P
    } else {
        AV_PIX_FMT_NV12
    }
}

fn rational(num: i32, den: i32) -> AVRational {
    AVRational::new(num, den)
}

impl VideoEncoder {
    /// `global_header`: the container wants codec extradata (MKV/MP4 do).
    pub fn new(gpu: Option<&Gpu>, s: &VideoEncoderSettings, gpu_input: bool, global_header: bool, pool_size: i32) -> Result<VideoEncoder> {
        ff::init()?;
        let info = encoding::get(&s.encoder_id);
        let gpu_input = gpu_input && gpu.is_some();
        let name = cstr(info.ff_name);
        let codec = unsafe { avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(anyhow!("{} is not in this FFmpeg build", info.ff_name));
        }
        let ctx = unsafe { avcodec_alloc_context3(codec) };
        let mut enc = VideoEncoder {
            ctx,
            hw_device: std::ptr::null_mut(),
            hw_frames: std::ptr::null_mut(),
            pkt: std::ptr::null_mut(),
            info,
            settings: s.clone(),
            gpu_input,
            pool_size: 0,
            planar: cpu_pix_fmt(info) == AV_PIX_FMT_YUV420P,
            unused_options: Vec::new(),
        };
        let b_frames = if info.has(encoding::feature::B_FRAMES) { s.b_frames.clamp(0, 4) } else { 0 };
        unsafe {
            set(ctx, AVCodecContext_width, s.width as i32);
            set(ctx, AVCodecContext_height, s.height as i32);
            set(ctx, AVCodecContext_time_base, rational(s.fps_den as i32, s.fps_num as i32));
            set(ctx, AVCodecContext_framerate, rational(s.fps_num as i32, s.fps_den as i32));
            set(ctx, AVCodecContext_sample_aspect_ratio, rational(1, 1));
            set(ctx, AVCodecContext_gop_size, ((s.keyframe_seconds * s.fps()).round() as i32).max(1));
            set(ctx, AVCodecContext_max_b_frames, b_frames);
            if global_header {
                let f: i32 = get(ctx, AVCodecContext_flags);
                set(ctx, AVCodecContext_flags, f | AV_CODEC_FLAG_GLOBAL_HEADER);
            }
            let bt709 = s.color_space == ColorSpace::Rec709;
            set(ctx, AVCodecContext_color_primaries, if bt709 { AVCOL_PRI_BT709 } else { AVCOL_PRI_SMPTE170M });
            set(ctx, AVCodecContext_color_trc, if bt709 { AVCOL_TRC_BT709 } else { AVCOL_TRC_SMPTE170M });
            set(ctx, AVCodecContext_colorspace, if bt709 { AVCOL_SPC_BT709 } else { AVCOL_SPC_SMPTE170M });
            set(ctx, AVCodecContext_color_range, if s.color_range == ColorRange::Full { AVCOL_RANGE_JPEG } else { AVCOL_RANGE_MPEG });
            set(ctx, AVCodecContext_chroma_sample_location, AVCHROMA_LOC_LEFT);
        }
        // Frames queued for the encoder + look-ahead + B-frame reordering + slack.
        enc.pool_size = if pool_size > 0 { pool_size } else { 8 + s.look_ahead.max(0) + b_frames + 4 };
        if gpu_input {
            enc.setup_hw_frames(gpu.unwrap(), s.width as i32, s.height as i32, enc.pool_size)?;
            unsafe {
                set(ctx, AVCodecContext_pix_fmt, AV_PIX_FMT_D3D11);
                set(ctx, AVCodecContext_sw_pix_fmt, AV_PIX_FMT_NV12);
                set(ctx, AVCodecContext_hw_frames_ctx, av_buffer_ref(enc.hw_frames));
            }
        } else {
            unsafe { set(ctx, AVCodecContext_pix_fmt, cpu_pix_fmt(info)) };
        }
        let mut opts = Dict::new();
        for (k, v) in build_options(info, s, ctx, b_frames) {
            opts.set(&k, &v);
        }
        check(unsafe { avcodec_open2(ctx, codec, &mut opts.0) }, format!("open {}", info.ff_name))?;
        enc.unused_options = opts.keys();
        enc.pkt = unsafe { av_packet_alloc() };
        Ok(enc)
    }

    fn setup_hw_frames(&mut self, gpu: &Gpu, w: i32, h: i32, pool: i32) -> Result<()> {
        unsafe {
            self.hw_device = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
            if self.hw_device.is_null() {
                return Err(anyhow!("av_hwdevice_ctx_alloc failed"));
            }
            let dev_ctx: *mut c_void = get(self.hw_device, AVBufferRef_data);
            let d3d: *mut c_void = get(dev_ctx, AVHWDeviceContext_hwctx);
            // FFmpeg releases the device when the context is freed, so it gets its own reference.
            let dev = gpu.device.clone();
            set(d3d, AVD3D11VADeviceContext_device, dev.into_raw());
            check(av_hwdevice_ctx_init(self.hw_device), "init d3d11 hw device")?;
            self.hw_frames = av_hwframe_ctx_alloc(self.hw_device);
            let fc: *mut c_void = get(self.hw_frames, AVBufferRef_data);
            set(fc, AVHWFramesContext_format, AV_PIX_FMT_D3D11);
            set(fc, AVHWFramesContext_sw_format, AV_PIX_FMT_NV12);
            set(fc, AVHWFramesContext_width, w);
            set(fc, AVHWFramesContext_height, h);
            set(fc, AVHWFramesContext_initial_pool_size, pool);
            let d3d_frames: *mut c_void = get(fc, AVHWFramesContext_hwctx);
            // D3D11_BIND_DECODER: the only bind flag NV12 texture arrays accept on this driver.
            set(d3d_frames, AVD3D11VAFramesContext_BindFlags, 0x200u32);
            check(av_hwframe_ctx_init(self.hw_frames), "init d3d11 frame pool")?;
        }
        Ok(())
    }

    pub fn frame_pool(&self) -> Option<GpuFramePool> {
        (!self.hw_frames.is_null()).then(|| GpuFramePool(unsafe { av_buffer_ref(self.hw_frames) }))
    }

    pub fn context(&self) -> *mut AVCodecContext {
        self.ctx
    }
    pub fn time_base(&self) -> AVRational {
        unsafe { get(self.ctx, AVCodecContext_time_base) }
    }

    /// Allocates a system-memory NV12 (or YUV420P) frame for CPU-input encoders.
    pub fn alloc_cpu_frame(&self) -> Result<Frame> {
        unsafe {
            let f = av_frame_alloc();
            set(f, AVFrame_format, cpu_pix_fmt(self.info));
            set(f, AVFrame_width, self.settings.width as i32);
            set(f, AVFrame_height, self.settings.height as i32);
            let fr = Frame(f);
            check(av_frame_get_buffer(f, 32), "alloc NV12 frame")?;
            Ok(fr)
        }
    }

    /// Sends a frame and passes every finished packet to `on_packet`.
    pub fn encode(&mut self, frame: Frame, pts: i64, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe {
            let f = frame.0;
            set(f, AVFrame_pts, pts);
            set::<i32>(f, AVFrame_color_range, get(self.ctx, AVCodecContext_color_range));
            set::<i32>(f, AVFrame_colorspace, get(self.ctx, AVCodecContext_colorspace));
            set::<i32>(f, AVFrame_color_primaries, get(self.ctx, AVCodecContext_color_primaries));
            set::<i32>(f, AVFrame_color_trc, get(self.ctx, AVCodecContext_color_trc));
            let r = avcodec_send_frame(self.ctx, f);
            drop(frame);
            check(r, "send video frame")?;
        }
        self.drain(on_packet)
    }

    pub fn flush(&mut self, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe { avcodec_send_frame(self.ctx, std::ptr::null()) };
        self.drain(on_packet)
    }

    fn drain(&mut self, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        loop {
            let r = unsafe { avcodec_receive_packet(self.ctx, self.pkt) };
            if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                return Ok(());
            }
            check(r, "receive video packet")?;
            let p = unsafe { av_packet_alloc() };
            unsafe { av_packet_move_ref(p, self.pkt) };
            on_packet(Packet(p));
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null() {
                av_packet_free(&mut self.pkt);
            }
            if !self.ctx.is_null() {
                avcodec_free_context(&mut self.ctx);
            }
            if !self.hw_frames.is_null() {
                av_buffer_unref(&mut self.hw_frames);
            }
            if !self.hw_device.is_null() {
                av_buffer_unref(&mut self.hw_device);
            }
        }
    }
}

/// Encoder private options for the chosen settings. Only options the encoder supports are produced.
pub fn build_options(info: &EncoderInfo, s: &VideoEncoderSettings, ctx: *mut AVCodecContext, b_frames: i32) -> Vec<(String, String)> {
    let mut o: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: String| o.push((k.into(), v));
    let preset = s.preset.as_deref().filter(|p| info.presets.contains(p)).unwrap_or(info.default_preset).to_string();
    let bitrate = s.bitrate_kbps as i64 * 1000;
    let maxrate = s.max_bitrate_kbps.max(s.bitrate_kbps) as i64 * 1000;
    let buf = |v: i64| v.saturating_mul(2).min(i32::MAX as i64) as i32;
    let set_rates = |br: i64, max: i64, bufsize: Option<i32>| unsafe {
        set(ctx, AVCodecContext_bit_rate, br);
        set(ctx, AVCodecContext_rc_max_rate, max);
        if let Some(b) = bufsize {
            set(ctx, AVCodecContext_rc_buffer_size, b);
        }
    };
    match info.backend {
        Backend::Nvenc => {
            add("preset", preset);
            if let Some(t) = s.tune.as_deref().filter(|t| info.tunes.contains(t)) {
                add("tune", t.into());
            }
            if let Some(p) = s.profile.as_deref().filter(|p| info.profiles.contains(p) && *p != "main10") {
                add("profile", p.into());
            }
            match s.rate_control {
                RateControl::ConstantQuality => {
                    add("rc", "vbr".into());
                    add("cq", s.quality.to_string());
                    set_rates(0, maxrate, Some(buf(maxrate)));
                }
                RateControl::Cqp => {
                    add("rc", "constqp".into());
                    add("qp", s.quality.to_string());
                }
                RateControl::Vbr => {
                    add("rc", "vbr".into());
                    set_rates(bitrate, maxrate, Some(buf(maxrate)));
                }
                RateControl::Cbr => {
                    add("rc", "cbr".into());
                    set_rates(bitrate, bitrate, Some(buf(bitrate)));
                }
            }
            if b_frames > 0 && s.b_frame_refs && info.has(encoding::feature::B_FRAME_REFS) {
                add("b_ref_mode", if b_frames >= 2 { "middle" } else { "disabled" }.into());
            }
            if s.look_ahead > 0 {
                add("rc-lookahead", s.look_ahead.clamp(1, 32).to_string());
            }
            if s.spatial_aq {
                add("spatial-aq", "1".into());
                add("aq-strength", s.aq_strength.clamp(1, 15).to_string());
            }
            if s.temporal_aq {
                add("temporal-aq", "1".into());
            }
            add("multipass", match s.multipass {
                Multipass::QuarterRes => "qres",
                Multipass::FullRes => "fullres",
                Multipass::Disabled => "disabled",
            }
            .into());
            add("forced-idr", "1".into());
        }
        Backend::Amf => {
            add("quality", preset);
            add("usage", "transcoding".into());
            match s.rate_control {
                RateControl::ConstantQuality => {
                    add("rc", "qvbr".into());
                    add("qvbr_quality_level", (51 - s.quality).clamp(1, 51).to_string());
                    set_rates(bitrate, maxrate, None);
                }
                RateControl::Cqp => {
                    add("rc", "cqp".into());
                    add("qp_i", s.quality.to_string());
                    add("qp_p", s.quality.to_string());
                    if info.codec == VideoCodec::H264 {
                        add("qp_b", s.quality.to_string());
                    }
                }
                RateControl::Vbr => {
                    add("rc", "vbr_peak".into());
                    set_rates(bitrate, maxrate, None);
                }
                RateControl::Cbr => {
                    add("rc", "cbr".into());
                    set_rates(bitrate, bitrate, Some(buf(bitrate)));
                }
            }
            if s.look_ahead > 0 && info.has(encoding::feature::PRE_ANALYSIS) {
                add("preanalysis", "1".into());
            }
        }
        Backend::X264 => {
            add("preset", preset);
            if let Some(t) = s.tune.as_deref().filter(|t| info.tunes.contains(t)) {
                add("tune", t.into());
            }
            if let Some(p) = s.profile.as_deref().filter(|p| info.profiles.contains(p)) {
                add("profile", p.into());
            }
            match s.rate_control {
                RateControl::ConstantQuality => {
                    add("crf", s.quality.to_string());
                    set_rates(0, maxrate, Some(buf(maxrate)));
                }
                RateControl::Cqp => add("qp", s.quality.to_string()),
                RateControl::Vbr => set_rates(bitrate, maxrate, Some(buf(maxrate))),
                RateControl::Cbr => {
                    set_rates(bitrate, bitrate, Some(buf(bitrate)));
                    add("nal-hrd", "cbr".into());
                }
            }
            if s.look_ahead > 0 {
                add("rc-lookahead", s.look_ahead.clamp(1, 60).to_string());
            }
            if !s.psycho_visual {
                add("x264-params", "psy=0".into());
            }
        }
        Backend::SvtAv1 => {
            add("preset", preset);
            if s.rate_control == RateControl::ConstantQuality {
                add("crf", (s.quality + 10).clamp(1, 63).to_string());
            } else {
                set_rates(bitrate, 0, None);
            }
        }
    }
    o
}

/// Wraps a raw ID3D11Texture2D pointer from an FFmpeg frame without taking a reference.
pub fn borrow_texture(ptr: &*mut c_void) -> Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    unsafe { windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(ptr) }
}
