//! Records a short real clip with every quality preset through the normal recording pipeline (same
//! encoder, audio tracks and muxer as a real recording), measures smoothness and load, and recommends the
//! best preset that stays smooth. Uses the live scene when it has a moving source, otherwise a
//! hard-to-encode moving test pattern.
use crate::encoding::QualityPreset;
use crate::engine::EngineShared;
use crate::monitor::SystemMonitor;
use crate::muxer::ContainerFormat;
use crate::recorder::{Recorder, RecordingSettings};
use crate::scene::Scene;
use crate::sources::pattern::TestPatternSource;
use crate::sources::VideoSource;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Measured outcome of one preset recorded for real on this PC.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PresetTestResult {
    pub preset: String,
    pub encoder_id: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// What was recorded: the live scene (e.g. the game) or the built-in moving test pattern.
    pub content: String,
    pub seconds: f64,
    pub frames_expected: u64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub render_lag_frames: u64,
    pub avg_encode_ms: f64,
    pub max_encode_ms: f64,
    pub max_queue: usize,
    pub queue_limit: usize,
    /// NVENC chip load (NVIDIA only, -1 when unknown).
    pub encoder_percent: f64,
    pub encoder_percent_peak: f64,
    pub gpu_percent: f64,
    pub gpu_percent_baseline: f64,
    /// Frames per second the captured game delivered before and during the test (-1 for the test pattern).
    pub capture_fps_baseline: f64,
    pub capture_fps_during: f64,
    pub app_cpu_percent: f64,
    pub bitrate_mbps: f64,
    pub passed: bool,
    pub verdict: String,
    /// Unix seconds.
    pub tested_at: u64,
}

impl PresetTestResult {
    pub fn preset(&self) -> Option<QualityPreset> {
        QualityPreset::parse(&self.preset)
    }
    pub fn matches(&self, encoder_id: &str, w: u32, h: u32, fps_num: u32, fps_den: u32) -> bool {
        self.encoder_id == encoder_id && self.width == w && self.height == h && self.fps_num == fps_num && self.fps_den == fps_den
    }
}

#[derive(Clone, Debug)]
pub struct BenchProgress {
    pub message: String,
    pub fraction: f64,
    pub completed: Option<PresetTestResult>,
}

pub struct PresetBenchmark {
    engine: Arc<EngineShared>,
    base: RecordingSettings,
    verified: u32,
    pub seconds_per_preset: u32,
    pub warmup_seconds: u32,
    pub baseline_seconds: u32,
    pub temp_dir: PathBuf,
    pub presets: Vec<QualityPreset>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Default)]
struct Window {
    capture_fps: f64,
    enc_pct: f64,
    enc_peak: f64,
    gpu_pct: f64,
    app_cpu: f64,
    lagged: u64,
    seconds: f64,
}

/// The heaviest preset that passed while every lighter preset passed too (smoothness first: a failure on
/// a lighter preset means heavier ones can't be trusted on this PC). SMOOTH when nothing passed.
pub fn recommend(results: &[PresetTestResult]) -> QualityPreset {
    let mut sorted: Vec<(QualityPreset, bool)> = results.iter().filter_map(|r| r.preset().map(|p| (p, r.passed))).collect();
    sorted.sort_by_key(|(p, _)| *p as u8);
    let mut best = QualityPreset::Smooth;
    for (p, passed) in sorted {
        if !passed {
            break;
        }
        best = p;
    }
    best
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl PresetBenchmark {
    pub fn new(engine: Arc<EngineShared>, base: RecordingSettings, verified: u32) -> Self {
        Self {
            engine,
            base,
            verified,
            seconds_per_preset: 12,
            warmup_seconds: 2,
            baseline_seconds: 3,
            temp_dir: std::env::temp_dir().join("EBS bench"),
            presets: QualityPreset::ALL.to_vec(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn wait(&self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("cancelled"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    pub fn run(&self, progress: &mut dyn FnMut(BenchProgress)) -> Result<Vec<PresetTestResult>> {
        let canvas = self.engine.canvas();
        let mut results = Vec::new();
        let mut mon = SystemMonitor::new();

        // Pick what to record: the live scene if something in it is delivering frames, else the test pattern.
        let live = largest_moving_source(&self.engine.current_scene());
        let mut original_scene = None;
        let mut pattern: Option<Arc<dyn VideoSource>> = None;
        let content = if let Some(l) = &live {
            mon.track_process(l.process_id());
            format!("Live scene ({})", l.name())
        } else {
            let p: Arc<dyn VideoSource> = Arc::new(TestPatternSource::new(&self.engine.gpu, canvas.width, canvas.height, "Benchmark test pattern")?);
            let scene = Scene::new("Benchmark");
            scene.add(p.clone());
            self.engine.add_source(p.clone());
            original_scene = Some(self.engine.current_scene());
            self.engine.set_current_scene(scene);
            pattern = Some(p);
            "Moving test pattern (no game was running)".to_string()
        };

        let r = (|| -> Result<()> {
            std::fs::create_dir_all(&self.temp_dir)?;
            let total = self.presets.len();
            for (i, &preset) in self.presets.iter().enumerate() {
                let f0 = i as f64 / total as f64;
                let label = format!("{} ({} of {})", preset.label(), i + 1, total);
                // A game's own frame rate changes with what is on screen, so each preset is compared with a
                // baseline measured right before it rather than one from the start of the test.
                progress(BenchProgress { message: format!("Measuring the game without recording before {label}..."), fraction: f0, completed: None });
                let baseline = self.sample(&mut mon, live.as_ref(), self.baseline_seconds, &mut |_| {})?;
                let msg = format!("Recording a test clip with {label}...");
                let (mut r, fps_only) = self.test_one(preset, &mut mon, live.as_ref(), baseline, &content, &mut |frac| {
                    progress(BenchProgress { message: msg.clone(), fraction: f0 + frac / total as f64, completed: None })
                })?;
                if !r.passed && fps_only {
                    // The only problem was a game frame-rate dip: retest once so a scene change in the game isn't
                    // blamed on the encoder. The retest result is the one kept, and it says so.
                    progress(BenchProgress { message: format!("Game frame rate dipped during {label}; testing again to be sure..."), fraction: f0, completed: None });
                    let baseline2 = self.sample(&mut mon, live.as_ref(), self.baseline_seconds, &mut |_| {})?;
                    let msg2 = format!("Re-testing {label}...");
                    let (r2, _) = self.test_one(preset, &mut mon, live.as_ref(), baseline2, &content, &mut |frac| {
                        progress(BenchProgress { message: msg2.clone(), fraction: f0 + frac / total as f64, completed: None })
                    })?;
                    let first = r.verdict.clone();
                    r = PresetTestResult { verdict: format!("{}{} (first try: {first})", if r2.passed { "Retest: " } else { "Confirmed on retest. " }, r2.verdict), ..r2 };
                }
                progress(BenchProgress { message: format!("{}: {}", preset.label(), r.verdict), fraction: (i + 1) as f64 / total as f64, completed: Some(r.clone()) });
                results.push(r);
            }
            Ok(())
        })();

        if let Some(s) = original_scene {
            self.engine.set_current_scene(s);
        }
        if let Some(p) = pattern {
            self.engine.remove_source(p.id());
        }
        r.map(|_| results)
    }

    fn sample(&self, mon: &mut SystemMonitor, src: Option<&Arc<dyn VideoSource>>, seconds: u32, tick: &mut dyn FnMut(f64)) -> Result<Window> {
        let (mut enc, mut gpu, mut cpu) = (Vec::new(), Vec::new(), Vec::new());
        mon.sample();
        let f0 = src.map(|s| s.frames_delivered()).unwrap_or(0);
        let lag0 = self.engine.stats.frames_lagged.load(Ordering::Relaxed);
        let t = Instant::now();
        for s in 0..seconds {
            self.wait(Duration::from_secs(1))?;
            let smp = mon.sample();
            if let Some(g) = smp.gpu {
                enc.push(g.encoder_percent as f64);
                gpu.push(g.gpu_percent as f64);
            }
            cpu.push(smp.app_cpu_percent);
            tick((s + 1) as f64 / seconds as f64);
        }
        let secs = t.elapsed().as_secs_f64();
        let avg = |v: &[f64]| if v.is_empty() { -1.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
        Ok(Window {
            capture_fps: src.map(|s| (s.frames_delivered() - f0) as f64 / secs).unwrap_or(-1.0),
            enc_pct: avg(&enc),
            enc_peak: enc.iter().cloned().fold(-1.0, f64::max),
            gpu_pct: avg(&gpu),
            app_cpu: avg(&cpu).max(0.0),
            lagged: self.engine.stats.frames_lagged.load(Ordering::Relaxed) - lag0,
            seconds: secs,
        })
    }

    fn test_one(
        &self,
        preset: QualityPreset,
        mon: &mut SystemMonitor,
        live: Option<&Arc<dyn VideoSource>>,
        baseline: Window,
        content: &str,
        frac: &mut dyn FnMut(f64),
    ) -> Result<(PresetTestResult, bool)> {
        let canvas = self.engine.canvas();
        let video = preset.build(&self.base.video.encoder_id, self.verified, &self.base.video);
        let rs = RecordingSettings {
            output_dir: self.temp_dir.clone(),
            filename_format: format!("bench {} yyyy-MM-dd HH-mm-ss", preset.label()),
            container: ContainerFormat::Mkv,
            remux_to_mp4: false,
            video: video.clone(),
            ..self.base.clone()
        };
        let mut result = PresetTestResult {
            preset: preset.label().into(),
            encoder_id: video.encoder_id.clone(),
            width: video.width,
            height: video.height,
            fps_num: canvas.fps_num,
            fps_den: canvas.fps_den,
            content: content.into(),
            tested_at: unix_now(),
            ..Default::default()
        };
        let rec = match Recorder::start(&self.engine, &rs) {
            Ok(r) => r,
            Err(e) => {
                result.verdict = format!("Could not start: {e}");
                return Ok((result, false));
            }
        };
        let total = (self.warmup_seconds + self.seconds_per_preset) as f64;
        let measured = (|| -> Result<Window> {
            // Warm-up: encoder start, look-ahead fill and the first keyframes are not representative.
            for s in 0..self.warmup_seconds {
                self.wait(Duration::from_secs(1))?;
                frac((s + 1) as f64 / total);
            }
            let w = self.warmup_seconds as f64;
            let n = self.seconds_per_preset as f64;
            self.sample(mon, live, self.seconds_per_preset, &mut |f| frac((w + f * n) / total))
        })();
        let stopped = rec.stop();
        let during = measured?;
        let _ = std::fs::remove_file(&rec.path);
        let st = &rec.stats;
        let (bytes, rec_err) = match &stopped {
            Ok(r) => (r.bytes, None),
            Err(e) => (0, Some(e.to_string())),
        };
        let fps = video.fps();
        let dropped = st.frames_dropped.load(Ordering::Relaxed); // whole clip, warm-up included: a drop anywhere is a fail
        let encoded = st.frames_encoded.load(Ordering::Relaxed);
        let clip_seconds = encoded as f64 / fps;
        let period_ms = 1000.0 / fps;
        let expected = (during.seconds * fps).round();
        let queue_limit = if rec.gpu_input { 6 } else { 8 };
        let max_queue = st.queue_max.load(Ordering::Relaxed);
        let round = |v: f64, d: i32| (v * 10f64.powi(d)).round() / 10f64.powi(d);
        result.seconds = round(during.seconds, 1);
        result.frames_expected = (clip_seconds * fps).round() as u64 + dropped;
        result.frames_encoded = encoded;
        result.frames_dropped = dropped;
        result.render_lag_frames = during.lagged;
        result.avg_encode_ms = round(st.average_encode_ms(), 2);
        result.max_encode_ms = round(st.max_encode_ms(), 1);
        result.max_queue = max_queue;
        result.queue_limit = queue_limit;
        result.encoder_percent = round(during.enc_pct, 1);
        result.encoder_percent_peak = round(during.enc_peak, 1);
        result.gpu_percent = round(during.gpu_pct, 1);
        result.gpu_percent_baseline = round(baseline.gpu_pct, 1);
        result.capture_fps_baseline = round(baseline.capture_fps, 1);
        result.capture_fps_during = round(during.capture_fps, 1);
        result.app_cpu_percent = round(during.app_cpu, 1);
        result.bitrate_mbps = if clip_seconds > 0.0 { round(bytes as f64 * 8.0 / clip_seconds / 1e6, 1) } else { 0.0 };

        // Smoothness rules. Every one must hold for the preset to count as tested OK on this PC.
        let mut problems = Vec::new();
        if let Some(e) = rec_err {
            problems.push(format!("recording error: {e}"));
        }
        if dropped > 0 {
            problems.push(format!("{dropped} frames dropped because the encoder fell behind"));
        }
        let baseline_lag_per_sec = if baseline.seconds > 0.0 { baseline.lagged as f64 / baseline.seconds } else { 0.0 };
        if during.lagged as f64 > baseline_lag_per_sec * during.seconds + (expected * 0.005).max(2.0) {
            problems.push(format!("{} frames rendered late (the PC was too busy)", during.lagged));
        }
        if max_queue > (queue_limit / 2).max(2) {
            problems.push(format!("encoder backlog reached {max_queue} of {queue_limit} frames"));
        }
        if during.enc_peak >= 0.0 && rec.gpu_input {
            if during.enc_peak > 90.0 {
                problems.push(format!("encoder chip peaked at {:.0}% (no headroom for busier scenes)", during.enc_peak));
            } else if during.enc_pct > 80.0 {
                problems.push(format!("encoder chip averaged {:.0}% (too little headroom)", during.enc_pct));
            }
        } else if st.average_encode_ms() > period_ms * 0.5 {
            problems.push(format!("encoding took {:.1} ms per frame (limit {:.1} ms for headroom)", st.average_encode_ms(), period_ms * 0.5));
        }
        let fps_drop = live.is_some() && baseline.capture_fps > 5.0 && during.capture_fps < baseline.capture_fps * 0.95 - 1.0;
        if fps_drop {
            problems.push(format!("the game's frame rate dropped from {:.0} to {:.0} fps", baseline.capture_fps, during.capture_fps));
        }
        let fps_only = fps_drop && problems.len() == 1;
        result.passed = problems.is_empty();
        let load = if during.enc_pct >= 0.0 && rec.gpu_input {
            format!("encoder chip {:.0}% (peak {:.0}%)", during.enc_pct, during.enc_peak)
        } else {
            format!("{:.1} ms per frame", st.average_encode_ms())
        };
        result.verdict = if result.passed {
            format!("Smooth: 0 dropped frames, {load}, {:.1} Mbps", result.bitrate_mbps)
        } else {
            format!("Not smooth: {}", problems.join("; "))
        };
        Ok((result, fps_only))
    }
}

/// The biggest visible source that is delivering new frames right now.
fn largest_moving_source(scene: &Scene) -> Option<Arc<dyn VideoSource>> {
    let mut items: Vec<_> = scene.snapshot().into_iter().filter(|i| i.visible && i.source.size().0 > 0 && i.source.kind() != "Test pattern").collect();
    items.sort_by(|a, b| (b.display_width() * b.display_height()).total_cmp(&(a.display_width() * a.display_height())));
    let mut cands: Vec<Arc<dyn VideoSource>> = Vec::new();
    for i in items {
        if !cands.iter().any(|c| c.id() == i.source.id()) {
            cands.push(i.source.clone());
        }
    }
    let before: Vec<u64> = cands.iter().map(|c| c.frames_delivered()).collect();
    std::thread::sleep(Duration::from_millis(500));
    cands.into_iter().zip(before).find(|(c, b)| (c.frames_delivered() - b) * 2 >= 10).map(|(c, _)| c)
}
