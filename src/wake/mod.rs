//! NPU-accelerated wake word detection.
//!
//! Ported from nova-npu's `src/nova/wake/`. The pipeline is 3 stages
//! (melspectrogram -> embedding -> wakeword classifier), running
//! openWakeWord's pre-trained models via `scripts/convert_wake_model.py`
//! (see `model_paths.rs`).
//!
//! Only stock phrases are supported — no custom wake-word training.
//! nova-npu had its own trainer (`wake/trainer.py`, ~1.1k lines) and it
//! never produced good results; not ported. For a real custom phrase,
//! openWakeWord's own training docs are the credible path: their
//! pretrained models (including hey_jarvis) are 100% synthetic
//! TTS-generated speech plus ~30,000 hours of negative data — see their
//! [training notebooks](https://github.com/dscripka/openWakeWord/blob/main/notebooks/automatic_model_training.ipynb)
//! and the companion
//! [synthetic_speech_dataset_generation](https://github.com/dscripka/synthetic_speech_dataset_generation)
//! repo. A model trained that way drops into
//! `scripts/convert_wake_model.py` the same as any stock phrase.

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
