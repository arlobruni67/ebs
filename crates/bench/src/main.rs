//! EBS command-line test harness for the Rust engine.
//!   ebs-bench hw
//!   ebs-bench record [--target pattern|game:EXE|monitor:N] [--seconds N] [--encoder ID] [--preset smooth|balanced|quality]
//!                    [--size WxH] [--fps N] [--container mkv|mp4] [--remux] [--no-audio] [--out DIR]
//!   ebs-bench shot [--target ...] [--out FILE.bmp]
use anyhow::{anyhow, Result};
use ebs_engine::audio::AudioSource;
use ebs_engine::capture;
use ebs_engine::encoding::{self, QualityPreset, VideoEncoderSettings};
use ebs_engine::gpu::Gpu;
use ebs_engine::muxer::ContainerFormat;
use ebs_engine::recorder::{Recorder, RecordingSettings};
use ebs_engine::scene::Scene;
use ebs_engine::sources::pattern::TestPatternSource;
use ebs_engine::sources::wgc::WgcSource;
use ebs_engine::sources::VideoSource;
use ebs_engine::{Engine, VideoCanvas};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        Some("hw") => hw(),
        Some("record") => record(&parse(&args[1..])),
        Some("shot") => shot(&parse(&args[1..])),
        Some("presets") => presets(&parse(&args[1..])),
        Some("collection") => collection(&parse(&args[1..])),
        Some("probe") => probe(&parse(&args[1..])),
        Some("probe-one") => probe_one(args.get(1).map(|s| s.as_str()).unwrap_or("nvenc_hevc")),
        _ => {
            eprintln!("usage: ebs-bench hw | record [options] | shot [options]");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn parse(a: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < a.len() {
        let k = a[i].trim_start_matches("--").to_string();
        if i + 1 < a.len() && !a[i + 1].starts_with("--") {
            m.insert(k, a[i + 1].clone());
            i += 2;
        } else {
            m.insert(k, "1".into());
            i += 1;
        }
    }
    m
}

fn hw() -> Result<()> {
    print!("{}", ebs_engine::hardware::HardwareInfo::detect());
    if let Some(s) = ebs_engine::nvml::read() {
        println!("NVML     : {} GPU {}% NVENC {}% NVDEC {}% VRAM {}/{} MB, {} encoder sessions, {} C",
            ebs_engine::nvml::device_name().unwrap_or_default(), s.gpu_percent, s.encoder_percent, s.decoder_percent,
            s.vram_used_mb, s.vram_total_mb, s.encoder_sessions, s.temp_c);
    }
    for g in capture::enumerate_games() {
        println!("game {} \"{}\" pid {}", g.exe_name, g.title, g.pid);
    }
    for e in encoding::CATALOG {
        println!("encoder {} ({})", e.id, e.display_name);
    }
    Ok(())
}

fn probe_one(id: &str) -> Result<()> {
    let adapters = Gpu::enumerate_adapters()?;
    let a = Gpu::choose_adapter(&adapters, None).ok_or_else(|| anyhow!("no GPU"))?;
    let gpu = Gpu::create(&a)?;
    let r = ebs_engine::probe::probe_one(&gpu, id, 1920, 1080);
    println!("{}", serde_json::to_string(&r)?);
    Ok(())
}

fn probe(o: &HashMap<String, String>) -> Result<()> {
    let exe = std::env::current_exe()?;
    for e in encoding::CATALOG {
        if o.get("only").is_some_and(|x| x != e.id) {
            continue;
        }
        let t = Instant::now();
        let r = ebs_engine::probe::run_isolated(&exe, &["probe-one"], e.id, Duration::from_secs(60));
        let feats: Vec<&str> = encoding::feature::NAMES.iter().filter(|(f, _)| r.verified_features & f != 0).map(|(_, n)| *n).collect();
        println!("{:<12} {:<5} {:>5} ms  {}  [{}]", e.id, if r.works { "OK" } else { "FAIL" }, t.elapsed().as_millis(), r.note, feats.join(","));
    }
    Ok(())
}

fn presets(o: &HashMap<String, String>) -> Result<()> {
    let (engine, src) = make_engine(o)?;
    if !o.contains_key("no-audio") {
        let clock = engine.audio.clock;
        engine.audio.add(AudioSource::desktop(clock, None, "Desktop Audio"));
    }
    wait_for_frames(&src, 5);
    let enc = o.get("encoder").cloned().unwrap_or_else(|| "nvenc_hevc".into());
    // Features verified on this PC by the out-of-process probe, as the app does.
    let exe = std::env::current_exe()?;
    let probe = ebs_engine::probe::run_isolated(&exe, &["probe-one"], &enc, Duration::from_secs(60));
    if !probe.works {
        return Err(anyhow!("{enc} does not work here: {}", probe.note));
    }
    let c = engine.canvas();
    let mut rs = RecordingSettings::default();
    rs.video = VideoEncoderSettings { encoder_id: enc, width: c.width, height: c.height, fps_num: c.fps_num, fps_den: c.fps_den, ..Default::default() };
    let mut b = ebs_engine::benchmark::PresetBenchmark::new(engine.shared.clone(), rs, probe.verified_features);
    if let Some(s) = o.get("seconds") {
        b.seconds_per_preset = s.parse()?;
    }
    if let Some(p) = o.get("only") {
        b.presets = vec![QualityPreset::parse(p).ok_or_else(|| anyhow!("bad --only"))?];
    }
    let res = b.run(&mut |p| {
        if p.completed.is_some() {
            println!("[{:>3.0}%] {}", p.fraction * 100.0, p.message);
        }
    })?;
    for r in &res {
        println!("{:<8} passed {} | frames {}/{} dropped {} lag {} | enc {:.2}/{:.1} ms queue {}/{} | NVENC {}% peak {}% GPU {}% | app CPU {}% | {} Mbps | capture fps {} -> {}",
            r.preset, r.passed, r.frames_encoded, r.frames_expected, r.frames_dropped, r.render_lag_frames, r.avg_encode_ms, r.max_encode_ms,
            r.max_queue, r.queue_limit, r.encoder_percent, r.encoder_percent_peak, r.gpu_percent, r.app_cpu_percent, r.bitrate_mbps,
            r.capture_fps_baseline, r.capture_fps_during);
    }
    println!("content: {}", res.first().map(|r| r.content.as_str()).unwrap_or(""));
    println!("recommended: {}", ebs_engine::benchmark::recommend(&res).label());
    Ok(())
}

fn collection(o: &HashMap<String, String>) -> Result<()> {
    use ebs_engine::collection::SceneCollection;
    let dir = PathBuf::from(o.get("dir").ok_or_else(|| anyhow!("--dir"))?);
    let engine = Engine::new(VideoCanvas::new(1920, 1080, 60, 1), None)?;
    println!("collections: {:?}", SceneCollection::list(&dir));
    let mut c = SceneCollection::new(&dir, "Default");
    let name = o.get("name").cloned().unwrap_or_else(|| "Default".into());
    let found = c.load(&engine.shared, &name, &mut |w| println!("warn: {w}"))?;
    println!("loaded {name}: {found}, transition {:?} {} ms, current {}", c.transition, c.transition_ms, engine.current_scene().name());
    std::thread::sleep(Duration::from_millis(1500));
    for s in &c.scenes {
        println!("scene {}", s.name());
        for it in s.snapshot() {
            println!("  {} [{}] {} | {}x{} pos {:.0},{:.0} scale {:.2} fit {:?} visible {}", it.source.name(), it.source.kind(), it.source.status(),
                it.source.size().0, it.source.size().1, it.x, it.y, it.scale_x, it.fit, it.visible);
        }
    }
    for a in engine.audio.sources() {
        println!("audio {} {:?} vol {} tracks {:#b} status {}", a.name(), a.kind, a.volume.load(), a.track_mask.load(Ordering::Relaxed), a.status());
    }
    if let Some(out) = o.get("save-to") {
        c.dir = PathBuf::from(out);
        c.save(&engine.shared)?;
        println!("saved to {}", SceneCollection::path_for(&c.dir, &c.name).display());
    }
    Ok(())
}

fn size(o: &HashMap<String, String>) -> Result<(u32, u32)> {
    let s = o.get("size").map(|s| s.as_str()).unwrap_or("1920x1080");
    let (w, h) = s.split_once('x').ok_or_else(|| anyhow!("--size WxH"))?;
    Ok((w.parse()?, h.parse()?))
}

fn make_engine(o: &HashMap<String, String>) -> Result<(Engine, Arc<dyn VideoSource>)> {
    let (w, h) = size(o)?;
    let fps: u32 = o.get("fps").map(|s| s.parse()).transpose()?.unwrap_or(60);
    let engine = Engine::new(VideoCanvas::new(w, h, fps, 1), None)?;
    let target = o.get("target").map(|s| s.as_str()).unwrap_or("pattern");
    let src: Arc<dyn VideoSource> = if target == "pattern" {
        Arc::new(TestPatternSource::new(&engine.gpu, w, h, "Test pattern")?)
    } else if let Some(exe) = target.strip_prefix("game:") {
        Arc::new(WgcSource::game(engine.gpu.clone(), exe, false, Some(exe)))
    } else if let Some(n) = target.strip_prefix("monitor:") {
        let mons = capture::enumerate_monitors();
        let m = mons.get(n.parse::<usize>()?).ok_or_else(|| anyhow!("no monitor {n}"))?;
        Arc::new(WgcSource::monitor(engine.gpu.clone(), m, true, None))
    } else {
        return Err(anyhow!("unknown target {target}"));
    };
    engine.add_source(src.clone());
    let scene = Scene::new("Scene");
    scene.add(src.clone());
    engine.set_current_scene(scene);
    Ok((engine, src))
}

fn wait_for_frames(src: &Arc<dyn VideoSource>, secs: u64) {
    let t = Instant::now();
    while src.frames_delivered() == 0 && t.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shot(o: &HashMap<String, String>) -> Result<()> {
    let (engine, src) = make_engine(o)?;
    wait_for_frames(&src, 5);
    std::thread::sleep(Duration::from_millis(300));
    let out = PathBuf::from(o.get("out").cloned().unwrap_or_else(|| "shot.bmp".into()));
    engine.save_canvas_snapshot(&out)?;
    println!("source {} status {} frames {}; saved {}", src.name(), src.status(), src.frames_delivered(), out.display());
    Ok(())
}

fn record(o: &HashMap<String, String>) -> Result<()> {
    let secs: f64 = o.get("seconds").map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let (engine, src) = make_engine(o)?;
    if !o.contains_key("no-audio") {
        let clock = engine.audio.clock;
        engine.audio.add(AudioSource::desktop(clock, None, "Desktop Audio"));
        if o.contains_key("mic") {
            engine.audio.add(AudioSource::microphone(clock, None, "Microphone"));
        }
    }
    wait_for_frames(&src, 5);
    let c = engine.canvas();
    let mut rs = RecordingSettings::default();
    rs.output_dir = PathBuf::from(o.get("out").cloned().unwrap_or_else(|| r"D:\EBS-rs\test-out".into()));
    rs.filename_format = format!("bench yyyy-MM-dd HH-mm-ss");
    rs.container = match o.get("container").map(|s| s.as_str()) {
        Some("mp4") => ContainerFormat::Mp4,
        _ => ContainerFormat::Mkv,
    };
    rs.remux_to_mp4 = o.contains_key("remux");
    let enc = o.get("encoder").cloned().unwrap_or_else(|| "nvenc_hevc".into());
    let base = VideoEncoderSettings { encoder_id: enc.clone(), width: c.width, height: c.height, fps_num: c.fps_num, fps_den: c.fps_den, ..Default::default() };
    let preset = QualityPreset::parse(o.get("preset").map(|s| s.as_str()).unwrap_or("balanced")).ok_or_else(|| anyhow!("bad --preset"))?;
    rs.video = preset.build(&enc, u32::MAX, &base);
    if o.contains_key("no-audio") {
        rs.tracks = 0b001;
    }
    println!("source {} status {} ({}x{}), encoder {} preset {}", src.name(), src.status(), src.size().0, src.size().1, enc, preset.label());

    engine.stats.reset_peaks();
    let rec = Recorder::start(&engine.shared, &rs)?;
    let open_stall = engine.stats.max_render_ms();
    let lag0 = engine.stats.frames_lagged.load(Ordering::Relaxed);
    engine.stats.reset_peaks();
    println!("recording to {} (gpu input {})", rec.path.display(), rec.gpu_input);
    if o.contains_key("trace") {
        let t = Instant::now();
        let mut last = 0.0;
        while t.elapsed() < Duration::from_millis(1000) {
            let m = engine.stats.max_render_ms();
            if m > last + 2.0 {
                println!("t={:>4} ms max render {:.1} ms, encoded {}", t.elapsed().as_millis(), m, rec.stats.frames_encoded.load(Ordering::Relaxed));
                last = m;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let t = Instant::now();
    let mut start_peak = None;
    while t.elapsed().as_secs_f64() < secs {
        std::thread::sleep(Duration::from_millis(250));
        if o.contains_key("pause") {
            let e = t.elapsed().as_secs_f64();
            rec.set_paused(e >= secs / 3.0 && e < secs / 3.0 + 2.0);
        }
        if start_peak.is_none() && t.elapsed() >= Duration::from_secs(1) {
            start_peak = Some((engine.stats.max_render_ms(), engine.stats.frames_lagged.load(Ordering::Relaxed) - lag0));
            engine.stats.reset_peaks();
        }
        if let Some(e) = rec.error() {
            return Err(anyhow!(e));
        }
    }
    let res = rec.stop()?;
    let st = &rec.stats;
    let lagged = engine.stats.frames_lagged.load(Ordering::Relaxed) - lag0;
    println!("frames submitted {} encoded {} dropped {} | render lagged {} avg render {:.2} ms max {:.2} ms",
        st.frames_submitted.load(Ordering::Relaxed), res.frames_encoded, res.frames_dropped, lagged,
        engine.stats.average_render_ms(), engine.stats.max_render_ms());
    println!("encode avg {:.2} ms max {:.2} ms, queue max {}, audio blocks {}, a/v delay {:.1} ms, source frames {}",
        st.average_encode_ms(), st.max_encode_ms(), st.queue_max.load(Ordering::Relaxed), st.audio_blocks.load(Ordering::Relaxed),
        st.audio_delay_ms.load(), src.frames_delivered());
    println!("file {} ({:.1} MB, {:.2} s){}", res.path.display(), res.bytes as f64 / 1e6, res.duration_seconds,
        res.remuxed.as_ref().map(|p| format!(", mp4 {}", p.display())).unwrap_or_default());
    println!("encoder open (before first recorded frame): longest render {open_stall:.1} ms");
    if let Some((m, l)) = start_peak {
        println!("first recorded second: max render {m:.2} ms, lagged {l}");
    }
    for w in &res.warnings {
        println!("warning: {w}");
    }
    for l in engine.drain_log() {
        println!("log: {l}");
    }
    Ok(())
}
