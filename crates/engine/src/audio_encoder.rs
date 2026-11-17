//! AAC (or Opus/FLAC) encoder for one audio track: 48 kHz stereo in, any codec frame size out.
use crate::ff::{self, *};
use anyhow::{anyhow, Result};

pub const AUDIO_CODECS: &[(&str, &str)] = &[("aac", "AAC (best compatibility)"), ("libopus", "Opus"), ("flac", "FLAC (lossless)")];

pub struct AudioEncoder {
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    pub frame_size: usize,
    pub codec_name: String,
    sample_fmt: i32,
    fifo_l: Vec<f32>,
    fifo_r: Vec<f32>,
    fifo_pts: Option<i64>,
}
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn new(codec_name: &str, bitrate_kbps: u32, global_header: bool) -> Result<AudioEncoder> {
        ff::init()?;
        let name = cstr(codec_name);
        let codec = unsafe { avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(anyhow!("audio encoder {codec_name} missing"));
        }
        let sample_fmt = match codec_name {
            "flac" => AV_SAMPLE_FMT_S32,
            "libopus" => AV_SAMPLE_FMT_FLT,
            _ => AV_SAMPLE_FMT_FLTP,
        };
        unsafe {
            let ctx = avcodec_alloc_context3(codec);
            let mut me = AudioEncoder { ctx, pkt: std::ptr::null_mut(), frame_size: 1024, codec_name: codec_name.into(), sample_fmt, fifo_l: Vec::new(), fifo_r: Vec::new(), fifo_pts: None };
            set(ctx, AVCodecContext_sample_rate, crate::audio::SAMPLE_RATE as i32);
            set(ctx, AVCodecContext_sample_fmt, sample_fmt);
            av_channel_layout_default(field(ctx, AVCodecContext_ch_layout), 2);
            set(ctx, AVCodecContext_bit_rate, bitrate_kbps as i64 * 1000);
            set(ctx, AVCodecContext_time_base, AVRational::new(1, crate::audio::SAMPLE_RATE as i32));
            if global_header {
                let f: i32 = get(ctx, AVCodecContext_flags);
                set(ctx, AVCodecContext_flags, f | AV_CODEC_FLAG_GLOBAL_HEADER);
            }
            check(avcodec_open2(ctx, codec, std::ptr::null_mut()), format!("open {codec_name}"))?;
            let fs: i32 = get(ctx, AVCodecContext_frame_size);
            me.frame_size = if fs > 0 { fs as usize } else { 1024 };
            me.pkt = av_packet_alloc();
            Ok(me)
        }
    }

    pub fn context(&self) -> *mut AVCodecContext {
        self.ctx
    }
    pub fn time_base(&self) -> AVRational {
        AVRational::new(1, crate::audio::SAMPLE_RATE as i32)
    }

    /// Queues samples starting at sample position `pts` and encodes every complete codec frame. Nothing is dropped.
    pub fn encode(&mut self, left: &[f32], right: &[f32], pts: i64, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        if self.fifo_l.is_empty() || self.fifo_pts.is_none() {
            self.fifo_pts = Some(pts - self.fifo_l.len() as i64);
        }
        self.fifo_l.extend_from_slice(left);
        self.fifo_r.extend_from_slice(right);
        let fs = self.frame_size;
        let mut off = 0;
        while self.fifo_l.len() - off >= fs {
            let p = self.fifo_pts.unwrap();
            let (l, r) = (self.fifo_l[off..off + fs].to_vec(), self.fifo_r[off..off + fs].to_vec());
            self.encode_frame(&l, &r, p, on_packet)?;
            self.fifo_pts = Some(p + fs as i64);
            off += fs;
        }
        self.fifo_l.drain(..off);
        self.fifo_r.drain(..off);
        Ok(())
    }

    fn encode_frame(&mut self, l: &[f32], r: &[f32], pts: i64, on_packet: &mut dyn FnMut(Packet)) -> Result<()> {
        unsafe {
            let f = av_frame_alloc();
            let frame = Frame(f);
            let n = l.len();
            set(f, AVFrame_nb_samples, n as i32);
            set(f, AVFrame_format, self.sample_fmt);
            set(f, AVFrame_sample_rate, crate::audio::SAMPLE_RATE as i32);
            av_channel_layout_copy(field(f, AVFrame_ch_layout), field(self.ctx, AVCodecContext_ch_layout));
            check(av_frame_get_buffer(f, 0), "alloc audio frame")?;
            let data: [*mut u8; 8] = get(f, AVFrame_data);
            match self.sample_fmt {
                AV_SAMPLE_FMT_FLTP => {
                    std::ptr::copy_nonoverlapping(l.as_ptr(), data[0] as *mut f32, n);
                    std::ptr::copy_nonoverlapping(r.as_ptr(), data[1] as *mut f32, n);
                }
                AV_SAMPLE_FMT_FLT => {
                    let d = std::slice::from_raw_parts_mut(data[0] as *mut f32, n * 2);
                    for i in 0..n {
                        d[2 * i] = l[i];
                        d[2 * i + 1] = r[i];
                    }
                }
                _ => {
                    let d = std::slice::from_raw_parts_mut(data[0] as *mut i32, n * 2);
                    for i in 0..n {
                        d[2 * i] = (l[i].clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32;
                        d[2 * i + 1] = (r[i].clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32;
                    }
                }
            }
            set(f, AVFrame_pts, pts);
            let ret = avcodec_send_frame(self.ctx, f);
            drop(frame);
            check(ret, "send audio frame")?;
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
            check(r, "receive audio packet")?;
            let p = unsafe { av_packet_alloc() };
            unsafe { av_packet_move_ref(p, self.pkt) };
            on_packet(Packet(p));
        }
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.pkt.is_null() {
                av_packet_free(&mut self.pkt);
            }
            if !self.ctx.is_null() {
                avcodec_free_context(&mut self.ctx);
            }
        }
    }
}
