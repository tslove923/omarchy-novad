//! Resolves pre-converted wake-word model IR files on disk.
//!
//! This is deliberately *not* a port of nova-npu's `model_converter.py`.
//! That file does real ONNX graph surgery (removing an If-node, forcing
//! static shapes) using Python's `onnx` package — a one-time offline
//! step, not something novad needs to redo at runtime. Instead, novad
//! expects pre-converted IR files to already be on disk (shipped as a
//! download, same pattern as voxtype's OpenVINO Whisper models), and
//! this module just finds them.

use std::path::PathBuf;

use crate::wake::WakeError;

pub struct WakeModelPaths {
    pub melspectrogram: PathBuf,
    pub embedding: PathBuf,
    pub wakeword: PathBuf,
}

/// `$XDG_DATA_HOME/novad/wake-models` (falls back to `~/.local/share`).
pub fn wake_models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("novad")
        .join("wake-models")
}

/// Locate the mel/embedding/wakeword IR files for `wakeword_name`
/// (e.g. `"hey_jarvis"`) under `<wake_models_dir>/<wakeword_name>/`.
///
/// `embedding.xml`/`.bin` and `melspectrogram.xml`/`.bin` are shared
/// across all wake phrases (openWakeWord's frontend is phrase-agnostic);
/// only `wakeword.xml`/`.bin` is specific to `wakeword_name`. Both live
/// under the phrase's own directory for now — simplest layout, revisit
/// if shipping many phrases makes the duplication wasteful.
pub fn find_pipeline_models(wakeword_name: &str) -> Result<WakeModelPaths, WakeError> {
    let dir = wake_models_dir().join(wakeword_name);
    let melspectrogram = dir.join("melspectrogram.xml");
    let embedding = dir.join("embedding.xml");
    let wakeword = dir.join("wakeword.xml");

    let missing: Vec<&PathBuf> = [&melspectrogram, &embedding, &wakeword]
        .into_iter()
        .filter(|p| !p.exists())
        .collect();

    if !missing.is_empty() {
        return Err(WakeError::ModelNotFound(format!(
            "Wake word model '{wakeword_name}' not found. Missing:\n{}\n\n\
             Expected an OpenVINO IR directory at:\n  {}\n\n\
             Run 'novad setup wake-model {wakeword_name}' to download one \
             (not yet implemented — see novad roadmap Phase 1).",
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
