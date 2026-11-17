//! Finds out which encoders and features really work on this PC by opening each one and encoding a few
//! frames. Nothing is assumed from the GPU name (GTX 1650 cards exist with Volta-class and Turing-class
//! NVENC). Some drivers crash inside the encoder on unsupported options (AMF pre-analysis on old AMD
//! drivers), so the app runs each probe in a child process: `run_isolated`.
use crate::encoding::{self, feature, Backend, Multipass, VideoEncoderSettings};
use crate::ff::*;
use crate::gpu::Gpu;
use crate::video_encoder::VideoEncoder;
use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProbeResult {
    pub encoder_id: String,
    pub works: bool,
    pub gpu_texture_input: bool,
    pub verified_features: u32,
    pub note: String,
}

fn shorten(s: &str) -> String {
    if s.chars().count() > 90 {
        s.chars().take(90).collect::<String>() + "..."
    } else {
        s.to_string()
    }
}

/// Probes one encoder in this process.
pub fn probe_one(gpu: &Gpu, encoder_id: &str, width: u32, height: u32) -> ProbeResult {
    let info = encoding::get(encoder_id);
    let gpu_input = info.backend == Backend::Nvenc && gpu.adapter.is_nvidia();
    let basic = VideoEncoderSettings {
        encoder_id: info.id.into(),
        width,
        height,
        b_frames: 0,
        b_frame_refs: false,
        look_ahead: 0,
        spatial_aq: false,
        temporal_aq: false,
        multipass: Multipass::Disabled,
        tune: None,
        ..Default::default()
    };
    if let Err(e) = try_encode(gpu, &basic, gpu_input, 6) {
        return ProbeResult { encoder_id: info.id.into(), works: false, gpu_texture_input: false, verified_features: 0, note: shorten(&e.to_string()) };
    }
    let mut verified = 0u32;
    let check = |f: u32, s: VideoEncoderSettings, verified: &mut u32| {
        if info.has(f) && try_encode(gpu, &s, gpu_input, 6).is_ok() {
            *verified |= f;
        }
    };
    check(feature::B_FRAMES, VideoEncoderSettings { b_frames: 2, ..basic.clone() }, &mut verified);
    if verified & feature::B_FRAMES != 0 {
        check(feature::B_FRAME_REFS, VideoEncoderSettings { b_frames: 2, b_frame_refs: true, ..basic.clone() }, &mut verified);
    }
    check(feature::LOOK_AHEAD, VideoEncoderSettings { look_ahead: 8, ..basic.clone() }, &mut verified);
    check(feature::SPATIAL_AQ, VideoEncoderSettings { spatial_aq: true, ..basic.clone() }, &mut verified);
    check(feature::TEMPORAL_AQ, VideoEncoderSettings { temporal_aq: true, ..basic.clone() }, &mut verified);
    check(feature::MULTIPASS, VideoEncoderSettings { multipass: Multipass::QuarterRes, ..basic.clone() }, &mut verified);
    check(feature::PRE_ANALYSIS, VideoEncoderSettings { look_ahead: 8, ..basic.clone() }, &mut verified);
    for f in [feature::PSYCHO_VISUAL, feature::TUNE] {
        if info.has(f) {
            verified |= f;
        }
    }
    let note = if gpu_input {
        "zero-copy D3D11 texture input"
    } else if info.hardware {
        "system-memory input (different GPU)"
    } else {
        "CPU encoder"
    };
    ProbeResult { encoder_id: info.id.into(), works: true, gpu_texture_input: gpu_input, verified_features: verified, note: note.into() }
}

/// Opens the encoder and pushes a handful of black frames through it.
pub fn try_encode(gpu: &Gpu, s: &VideoEncoderSettings, gpu_input: bool, frames: i64) -> Result<()> {
    let mut enc = VideoEncoder::new(Some(gpu), s, gpu_input, true, 0)?;
    if !enc.unused_options.is_empty() {
        return Err(anyhow!("unsupported options: {}", enc.unused_options.join(",")));
    }
    let pool = enc.frame_pool();
    let mut packets = 0;
    let mut count = |_p: Packet| packets += 1;
    for i in 0..frames {
        let frame = if let Some(pool) = pool.as_ref() {
            pool.acquire().ok_or_else(|| anyhow!("texture pool exhausted"))?.0
        } else {
            let f = enc.alloc_cpu_frame()?;
            unsafe {
                let data: [*mut u8; 8] = get(f.0, AVFrame_data);
                let ls: [i32; 8] = get(f.0, AVFrame_linesize);
                std::ptr::write_bytes(data[0], 16, ls[0] as usize * s.height as usize);
                std::ptr::write_bytes(data[1], 128, ls[1] as usize * s.height as usize / 2);
                if enc.planar {
                    std::ptr::write_bytes(data[2], 128, ls[2] as usize * s.height as usize / 2);
                }
            }
            f
        };
        enc.encode(frame, i, &mut count)?;
    }
    enc.flush(&mut count)?;
    if packets > 0 {
        Ok(())
    } else {
        Err(anyhow!("no output"))
    }
}

/// Runs `exe <args> <encoder_id>` which must print one ProbeResult as JSON on its last stdout line.
/// A crash or hang inside a driver becomes a failed result instead of taking the app down.
pub fn run_isolated(exe: &Path, args: &[&str], encoder_id: &str, timeout: Duration) -> ProbeResult {
    let failed = |note: String| ProbeResult { encoder_id: encoder_id.into(), works: false, gpu_texture_input: false, verified_features: 0, note };
    let mut child = match Command::new(exe).args(args).arg(encoder_id).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => return failed(format!("could not start probe: {e}")),
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return failed("probe timed out (driver hang)".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return failed(e.to_string()),
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return failed(e.to_string()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().rev().find(|l| l.trim_start().starts_with('{')).and_then(|l| serde_json::from_str::<ProbeResult>(l).ok()) {
        Some(r) => r,
        None => failed(format!("probe crashed (exit {})", out.status.code().map(|c| format!("{c:#x}")).unwrap_or("?".into()))),
    }
}
