//! Writes encoded packets to a file on its own thread, so a slow disk never stalls capture or encoding.
//! MKV is the default: after a crash everything up to the last cluster still plays.
use crate::ff::{self, *};
use anyhow::{anyhow, Result};
use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ContainerFormat {
    #[default]
    Mkv,
    Mp4,
    Mov,
}
impl ContainerFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mkv => ".mkv",
            Self::Mp4 => ".mp4",
            Self::Mov => ".mov",
        }
    }
    fn ff_name(self) -> &'static str {
        match self {
            Self::Mkv => "matroska",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
        }
    }
}

struct Item {
    stream: usize,
    packet: Packet,
    src_tb: AVRational,
}

struct FmtPtr(*mut AVFormatContext);
unsafe impl Send for FmtPtr {}

#[derive(Default)]
pub struct MuxStats {
    pub bytes_written: AtomicI64,
    pub queue_depth: AtomicUsize,
    pub dropped_packets: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub first_warning: Mutex<Option<String>>,
}

pub struct Muxer {
    fmt: *mut AVFormatContext,
    pub path: PathBuf,
    tx: Option<Sender<Item>>,
    thread: Option<JoinHandle<()>>,
    stream_tb: Vec<AVRational>,
    header_written: bool,
    pub stats: Arc<MuxStats>,
}
unsafe impl Send for Muxer {}

/// Thread-safe handle for queueing packets.
#[derive(Clone)]
pub struct MuxWriter {
    tx: Sender<Item>,
    stats: Arc<MuxStats>,
}
impl MuxWriter {
    /// Queues a packet (ownership moves to the muxer).
    pub fn write(&self, stream: usize, packet: Packet, src_tb: AVRational) {
        self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(Item { stream, packet, src_tb }).is_err() {
            self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn path_c(p: &Path) -> std::ffi::CString {
    cstr(&p.to_string_lossy())
}

impl Muxer {
    pub fn new(path: &Path, format: ContainerFormat) -> Result<Muxer> {
        ff::init()?;
        let mut fmt = std::ptr::null_mut();
        let name = cstr(format.ff_name());
        let p = path_c(path);
        check(unsafe { avformat_alloc_output_context2(&mut fmt, std::ptr::null(), name.as_ptr(), p.as_ptr()) }, "alloc output")?;
        Ok(Muxer { fmt, path: path.to_path_buf(), tx: None, thread: None, stream_tb: Vec::new(), header_written: false, stats: Arc::default() })
    }

    /// Adds a stream mirroring an opened encoder. Returns the stream index.
    pub fn add_stream(&mut self, enc: *mut AVCodecContext, title: Option<&str>) -> Result<usize> {
        unsafe {
            let st = avformat_new_stream(self.fmt, std::ptr::null());
            if st.is_null() {
                return Err(anyhow!("avformat_new_stream failed"));
            }
            check(avcodec_parameters_from_context(get(st, AVStream_codecpar), enc), "stream params")?;
            let tb: AVRational = get(enc, AVCodecContext_time_base);
            set(st, AVStream_time_base, tb);
            let ct: i32 = get(enc, AVCodecContext_codec_type);
            if ct == AVMEDIA_TYPE_VIDEO {
                set::<AVRational>(st, AVStream_avg_frame_rate, get(enc, AVCodecContext_framerate));
            }
            if let Some(t) = title {
                let (k, v) = (cstr("title"), cstr(t));
                av_dict_set(field(st, AVStream_metadata), k.as_ptr(), v.as_ptr(), 0);
            }
            self.stream_tb.push(tb);
            Ok(get::<i32>(st, AVStream_index) as usize)
        }
    }

    pub fn start(&mut self) -> Result<MuxWriter> {
        unsafe {
            let (k, v) = (cstr("encoder"), cstr("EBS"));
            av_dict_set(field(self.fmt, AVFormatContext_metadata), k.as_ptr(), v.as_ptr(), 0);
            let p = path_c(&self.path);
            check(avio_open(field(self.fmt, AVFormatContext_pb), p.as_ptr(), AVIO_FLAG_WRITE), format!("open {}", self.path.display()))?;
            let mut opts = Dict::new();
            let ofmt: *mut c_void = get(self.fmt, AVFormatContext_oformat);
            let oname: *const std::ffi::c_char = get(ofmt, AVOutputFormat_name);
            if !oname.is_null() && CStr::from_ptr(oname).to_string_lossy().contains("mp4") {
                opts.set("movflags", "+faststart");
            }
            // Flush clusters to disk regularly so a crash loses at most ~1 s.
            opts.set("flush_packets", "1");
            check(avformat_write_header(self.fmt, &mut opts.0), "write header")?;
            self.header_written = true;
            let n: u32 = get(self.fmt, AVFormatContext_nb_streams);
            let streams: *mut *mut c_void = get(self.fmt, AVFormatContext_streams);
            for i in 0..n as usize {
                self.stream_tb[i] = get(*streams.add(i), AVStream_time_base);
            }
        }
        let (tx, rx) = channel::<Item>();
        let fmt = FmtPtr(self.fmt);
        let tbs = self.stream_tb.clone();
        let stats = self.stats.clone();
        self.thread = Some(std::thread::Builder::new().name("EBS mux".into()).spawn(move || run(fmt, rx, tbs, stats))?);
        self.tx = Some(tx.clone());
        Ok(MuxWriter { tx, stats: self.stats.clone() })
    }

    /// Drains the queue, writes the index/trailer and closes the file. All MuxWriters must be dropped first.
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.fmt.is_null() {
            return;
        }
        // The trailer is only valid after a header; writing one on a half-started muxer crashes inside FFmpeg.
        if self.header_written {
            let r = unsafe { av_write_trailer(self.fmt) };
            if r < 0 {
                self.stats.error.lock().unwrap().get_or_insert_with(|| format!("write trailer: {}", err_str(r)));
            }
            self.header_written = false;
        }
        unsafe {
            let pb: *mut c_void = get(self.fmt, AVFormatContext_pb);
            if !pb.is_null() {
                self.stats.bytes_written.store(avio_tell(pb), Ordering::Relaxed);
                avio_closep(field(self.fmt, AVFormatContext_pb));
            }
        }
    }

    pub fn error(&self) -> Option<String> {
        self.stats.error.lock().unwrap().clone()
    }
}

fn run(fmt: FmtPtr, rx: Receiver<Item>, tbs: Vec<AVRational>, stats: Arc<MuxStats>) {
    let fmt = fmt.0;
    for it in rx {
        stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
        if stats.error.lock().unwrap().is_some() {
            continue;
        }
        unsafe {
            let p = it.packet.0;
            set(p, AVPacket_stream_index, it.stream as i32);
            let (pts, dts): (i64, i64) = (get(p, AVPacket_pts), get(p, AVPacket_dts));
            av_packet_rescale_ts(p, it.src_tb, tbs[it.stream]);
            let r = av_interleaved_write_frame(fmt, p);
            if r == AVERROR_EINVAL {
                // A rejected packet (e.g. timestamp out of order) costs one packet, never the recording.
                stats.dropped_packets.fetch_add(1, Ordering::Relaxed);
                stats.first_warning.lock().unwrap().get_or_insert_with(|| format!("dropped packet (stream {}, pts {pts}, dts {dts}): {}", it.stream, err_str(r)));
            } else if r < 0 {
                *stats.error.lock().unwrap() = Some(format!("write packet (stream {}, pts {pts}, dts {dts}): {}", it.stream, err_str(r)));
            }
            let pb: *mut c_void = get(fmt, AVFormatContext_pb);
            if !pb.is_null() {
                stats.bytes_written.store(avio_tell(pb), Ordering::Relaxed);
            }
        }
        drop(it.packet);
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        if self.thread.is_some() || self.header_written {
            self.finish();
        }
        if !self.fmt.is_null() {
            unsafe { avformat_free_context(self.fmt) };
            self.fmt = std::ptr::null_mut();
        }
    }
}

/// Stream-copies a recording into another container (MKV to MP4 after recording).
pub fn remux(input: &Path, output: &Path) -> Result<()> {
    ff::init()?;
    unsafe {
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let mut out_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let mut pkt = av_packet_alloc();
        let r = (|| -> Result<()> {
            let ip = path_c(input);
            let op = path_c(output);
            check(avformat_open_input(&mut in_ctx, ip.as_ptr(), std::ptr::null(), std::ptr::null_mut()), "open input")?;
            check(avformat_find_stream_info(in_ctx, std::ptr::null_mut()), "stream info")?;
            check(avformat_alloc_output_context2(&mut out_ctx, std::ptr::null(), std::ptr::null(), op.as_ptr()), "alloc output")?;
            let n: u32 = get(in_ctx, AVFormatContext_nb_streams);
            let istreams: *mut *mut c_void = get(in_ctx, AVFormatContext_streams);
            let mut map = vec![-1i32; n as usize];
            let mut next = 0;
            for i in 0..n as usize {
                let ist = *istreams.add(i);
                let par: *mut c_void = get(ist, AVStream_codecpar);
                let ty: i32 = get(par, AVCodecParameters_codec_type);
                if ty != AVMEDIA_TYPE_VIDEO && ty != AVMEDIA_TYPE_AUDIO {
                    continue;
                }
                let ost = avformat_new_stream(out_ctx, std::ptr::null());
                let opar: *mut c_void = get(ost, AVStream_codecpar);
                check(avcodec_parameters_copy(opar, par), "copy params")?;
                set(opar, AVCodecParameters_codec_tag, 0u32);
                av_dict_copy(field(ost, AVStream_metadata), get(ist, AVStream_metadata), 0);
                map[i] = next;
                next += 1;
            }
            check(avio_open(field(out_ctx, AVFormatContext_pb), op.as_ptr(), AVIO_FLAG_WRITE), "open output")?;
            let mut opts = Dict::new();
            opts.set("movflags", "+faststart");
            check(avformat_write_header(out_ctx, &mut opts.0), "write header")?;
            let ostreams: *mut *mut c_void = get(out_ctx, AVFormatContext_streams);
            while av_read_frame(in_ctx, pkt) >= 0 {
                let si: i32 = get(pkt, AVPacket_stream_index);
                if si as usize >= map.len() || map[si as usize] < 0 {
                    av_packet_unref(pkt);
                    continue;
                }
                let o = map[si as usize];
                av_packet_rescale_ts(pkt, get(*istreams.add(si as usize), AVStream_time_base), get(*ostreams.add(o as usize), AVStream_time_base));
                set(pkt, AVPacket_stream_index, o);
                set(pkt, AVPacket_pos, -1i64);
                check(av_interleaved_write_frame(out_ctx, pkt), "remux write")?;
            }
            av_write_trailer(out_ctx);
            Ok(())
        })();
        av_packet_free(&mut pkt);
        if !in_ctx.is_null() {
            avformat_close_input(&mut in_ctx);
        }
        if !out_ctx.is_null() {
            let pb: *mut c_void = get(out_ctx, AVFormatContext_pb);
            if !pb.is_null() {
                avio_closep(field(out_ctx, AVFormatContext_pb));
            }
            avformat_free_context(out_ctx);
        }
        r
    }
}
