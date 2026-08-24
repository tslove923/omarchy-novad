//! NPU-accelerated wake word detection.
//!
//! Ported from nova-npu's `src/nova/wake/`. See `docs/wake-word.md` (TODO)
//! for the pipeline overview; the short version is a 3-stage OpenVINO
//! pipeline (melspectrogram -> embedding -> wakeword classifier) run on
//! openWakeWord's pre-trained models, converted offline to NPU-friendly
//! static-shape IR.

pub mod audio_features;
pub mod detector;
pub mod model_paths;
pub mod npu_engine;

#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error("wake engine init failed: {0}")]
    Init(String),
    #[error("wake inference failed: {0}")]
    Infer(String),
    #[error("{0}")]
    ModelNotFound(String),
}
