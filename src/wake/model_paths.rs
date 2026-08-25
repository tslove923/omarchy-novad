//! Resolves prepared wake-word model files on disk.
//!
//! This is deliberately *not* a port of nova-npu's `model_converter.py`.
//! That file does real ONNX graph surgery (removing an If-node) using
//! Python's `onnx` package — a one-time offline step, not something
//! omarchy-novad needs to redo at runtime (`omarchy-novad setup
//! wake-model`, see wake/setup.rs, runs an equivalent conversion once
//! via a throwaway `uv` env, no persistent Python install needed).
//!
//! The files stay plain `.onnx`, not OpenVINO IR (`.xml`/`.bin`) —
//! `openvino::Core::read_model_from_file` reads ONNX directly via
//! OpenVINO's built-in ONNX frontend, same as nova's own
//! `core.read_model(str(model_path))` in `npu_engine.py`. No separate
//! IR conversion step exists for this pipeline; melspectrogram.onnx and
//! embedding.onnx aren't even modified from the stock openWakeWord
//! package files, just copied.

use std::path::PathBuf;

use crate::wake::WakeError;

pub struct WakeModelPaths {
    pub melspectrogram: PathBuf,
    pub embedding: PathBuf,
    pub wakeword: PathBuf,
}

/// `$XDG_DATA_HOME/omarchy-novad/wake-models` (falls back to `~/.local/share`).
pub fn wake_models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad")
        .join("wake-models")
}

/// Locate the mel/embedding/wakeword `.onnx` files for `wakeword_name`
/// (e.g. `"hey_jarvis"`) under `<wake_models_dir>/<wakeword_name>/`.
///
/// `embedding.onnx` and `melspectrogram.onnx` are identical across all
/// wake phrases (openWakeWord's frontend is phrase-agnostic); only
/// `wakeword.onnx` is specific to `wakeword_name`. Both live under the
/// phrase's own directory for now — simplest layout, revisit if
/// shipping many phrases makes the duplication wasteful.
pub fn find_pipeline_models(wakeword_name: &str) -> Result<WakeModelPaths, WakeError> {
    let dir = wake_models_dir().join(wakeword_name);
    let melspectrogram = dir.join("melspectrogram.onnx");
    let embedding = dir.join("embedding.onnx");
    let wakeword = dir.join("wakeword.onnx");

    let missing: Vec<&PathBuf> = [&melspectrogram, &embedding, &wakeword]
        .into_iter()
        .filter(|p| !p.exists())
        .collect();

    if !missing.is_empty() {
        return Err(WakeError::ModelNotFound(format!(
            "Wake word model '{wakeword_name}' not found. Missing:\n{}\n\n\
             Expected the prepared model files at:\n  {}\n\n\
             Run:\n  \
             uv run --with onnx --with openwakeword python \
             scripts/convert_wake_model.py {wakeword_name}\n\
             to produce them from the stock openWakeWord package — no \
             persistent Python install required.",
            missing
                .iter()
                .map(|p| format!("  - {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n"),
            dir.display(),
        )));
    }

    Ok(WakeModelPaths {
        melspectrogram,
        embedding,
        wakeword,
    })
}
