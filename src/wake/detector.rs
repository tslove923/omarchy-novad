//! Streaming wake word detector — port of nova-npu's `detector.py`.
//!
//! Feeds audio chunks through [`AudioFeatures`] + [`NpuEngine`], applies
//! patience filtering (N consecutive frames above threshold) and an
//! initialization guard (suppress the first few predictions, which are
//! unreliable before the mel/embedding buffers have real history).

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use super::audio_features::{AudioFeatures, CHUNK_SAMPLES, SAMPLE_RATE};
use super::model_paths::find_pipeline_models;
use super::npu_engine::NpuEngine;
use crate::wake::WakeError;

pub const DEFAULT_THRESHOLD: f32 = 0.5;
pub const DEFAULT_PATIENCE: usize = 3;
const INIT_GUARD_FRAMES: u64 = 5;
const PREDICTION_BUFFER_SIZE: usize = 30;
const EMBEDDING_FRAMES_FOR_DETECTION: usize = 16; // matches hey_jarvis

/// A single wake word detection event.
#[derive(Debug, Clone)]
pub struct Detection {
    // Not read yet — reserved for the event log / popup UI in Phase 3,
    // mirroring nova's Detection.timestamp.
    #[allow(dead_code)]
    pub timestamp: SystemTime,
    pub score: f32,
    pub primary_score: f32,
    pub verifier_score: f32,
    pub model: String,
}

/// Streaming wake word detector with NPU acceleration.
///
/// ```ignore
/// let mut detector = Detector::new("hey_jarvis", "NPU", cache_dir)?;
/// loop {
///     let audio: [i16; CHUNK_SAMPLES] = mic.read();
///     if let Some(detection) = detector.process(&audio)? {
///         println!("Wake word detected! score={:.3}", detection.score);
///     }
/// }
/// ```
pub struct Detector {
    pub wakeword: String,
    pub threshold: f32,
    pub patience: usize,

    engine: NpuEngine,
    features: AudioFeatures,

    prediction_buffer: VecDeque<f32>,
    total_frames: u64,

    inference_times: VecDeque<Duration>,
}

impl Detector {
    pub fn new(
        wakeword: &str,
        device: &str,
        cache_dir: &std::path::Path,
        threshold: f32,
        patience: usize,
    ) -> Result<Self, WakeError> {
        tracing::info!("Initializing NPU engine on {device}...");
        let paths = find_pipeline_models(wakeword)?;
        let engine = NpuEngine::new(&paths, device, cache_dir)?;

        tracing::info!(
            "Detector ready: wakeword={wakeword}, device={}, threshold={threshold:.2}, patience={patience}",
            engine.device,
        );

        Ok(Self {
            wakeword: wakeword.to_string(),
            threshold,
            patience,
            engine,
            features: AudioFeatures::new(),
            prediction_buffer: VecDeque::with_capacity(PREDICTION_BUFFER_SIZE),
            total_frames: 0,
            inference_times: VecDeque::with_capacity(100),
        })
    }

    /// Process an audio chunk (int16 @ 16kHz, optimal size [`CHUNK_SAMPLES`]).
    /// Returns `Some(Detection)` when the wake word fires.
    pub fn process(&mut self, audio: &[i16]) -> Result<Option<Detection>, WakeError> {
        let t0 = Instant::now();

        self.features.process(&mut self.engine, audio)?;
        let features = self.features.get_features(EMBEDDING_FRAMES_FOR_DETECTION);
        let (p1, p2) = self.engine.wakeword_raw(&features)?;
        let mut score = if p1 >= 0.5 { p2 } else { p1 };

        self.total_frames += 1;
        if self.total_frames <= INIT_GUARD_FRAMES {
            score = 0.0;
        }

        if self.prediction_buffer.len() == PREDICTION_BUFFER_SIZE {
            self.prediction_buffer.pop_front();
        }
        self.prediction_buffer.push_back(score);

        if self.inference_times.len() == 100 {
            self.inference_times.pop_front();
        }
        self.inference_times.push_back(t0.elapsed());

        // Patience check: require the last `patience` scores all above
        // threshold before firing.
        let mut detection = None;
        if self.prediction_buffer.len() >= self.patience {
            let len = self.prediction_buffer.len();
            let all_above =
                (0..self.patience).all(|i| self.prediction_buffer[len - 1 - i] >= self.threshold);
            if all_above {
                detection = Some(Detection {
                    timestamp: SystemTime::now(),
                    score,
                    primary_score: p1,
                    verifier_score: p2,
                    model: self.wakeword.clone(),
                });
                self.prediction_buffer.clear();
            }
        }

        Ok(detection)
    }

    pub fn reset(&mut self) {
        self.features.reset();
        self.prediction_buffer.clear();
        self.total_frames = 0;
        self.inference_times.clear();
    }

    // Not wired to a CLI flag yet — mirrors nova's `Detector.info`
    // diagnostic property; will back a `novad detect --stats` flag.
    #[allow(dead_code)]
    pub fn avg_inference_ms(&self) -> f32 {
        if self.inference_times.is_empty() {
            return 0.0;
        }
        let total: Duration = self.inference_times.iter().sum();
        total.as_secs_f32() * 1000.0 / self.inference_times.len() as f32
    }
}

/// High-level entry point: opens the mic, feeds [`Detector`], and on
/// detection shells out to `voxtype record start` / `record stop`,
/// stopping on a short silence timeout (mirrors nova's own "wake ->
/// first text" flow rather than voxtype's push-to-talk hotkey path).
pub struct WakeWordListener {
    detector: Detector,
    on_detect_cmd: Option<String>,
}

impl WakeWordListener {
    pub fn new(detector: Detector, on_detect_cmd: Option<String>) -> Self {
        Self {
            detector,
            on_detect_cmd,
        }
    }

    /// Feed one chunk of audio; returns the detection, if any, and runs
    /// `on_detect_cmd` (if configured) as a fire-and-forget subprocess.
    pub fn feed(&mut self, audio: &[i16]) -> Result<Option<Detection>, WakeError> {
        let detection = self.detector.process(audio)?;
        if let Some(ref d) = detection {
            tracing::info!(
                "Wake word detected: model={} score={:.3} p1={:.3} p2={:.3}",
                d.model,
                d.score,
                d.primary_score,
                d.verifier_score
            );
            if let Some(cmd) = &self.on_detect_cmd {
                run_on_detect(cmd, d);
            }
        }
        Ok(detection)
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    pub fn chunk_samples(&self) -> usize {
        CHUNK_SAMPLES
    }

    /// Clear buffered audio/prediction state after a recording ends, so
    /// the next wake-word listen starts clean rather than carrying over
    /// stale embedding history from the utterance just spoken.
    pub fn reset(&mut self) {
        self.detector.reset();
    }
}

fn run_on_detect(cmd: &str, detection: &Detection) {
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("NOVA_WAKE_MODEL", &detection.model)
        .env("NOVA_WAKE_SCORE", format!("{:.4}", detection.score))
        .env("NOVA_WAKE_P1", format!("{:.4}", detection.primary_score))
        .env("NOVA_WAKE_P2", format!("{:.4}", detection.verifier_score))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(_) => tracing::info!("Launched on-detect command: {cmd}"),
        Err(e) => tracing::error!("Failed to run on-detect command: {e}"),
    }
}
