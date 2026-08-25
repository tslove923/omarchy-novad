//! `omarchy-novad setup wake-model <name>` — wraps the one-time model prep step
//! so a user never has to know `scripts/convert_wake_model.py` exists.
//!
//! The script itself is embedded in the binary (`include_str!`) rather
//! than read from disk, so this works regardless of how/where omarchy-novad was
//! installed — no dependency on running from a repo checkout.

use std::io::Write;
use std::process::Command;

use crate::wake::WakeError;

const CONVERT_SCRIPT: &str = include_str!("../../scripts/convert_wake_model.py");

pub fn run(wakeword_name: &str) -> Result<(), WakeError> {
    if Command::new("uv").arg("--version").output().is_err() {
        return Err(WakeError::Init(format!(
            "'uv' not found on PATH. Wake word models are prepared with a one-time \
             Python step (removes an ONNX If-node the NPU can't run — see src/wake/mod.rs \
             for why this can't be pure Rust yet), run through uv so nothing gets \
             installed system-wide.\n\n\
             Install uv, then re-run this command:\n  \
             curl -LsSf https://astral.sh/uv/install.sh | sh\n  \
             source $HOME/.local/bin/env   # or restart your shell\n  \
             omarchy-novad setup wake-model {wakeword_name}"
        )));
    }

    let script_path = std::env::temp_dir().join("omarchy-novad-convert-wake-model.py");
    std::fs::write(&script_path, CONVERT_SCRIPT)
        .map_err(|e| WakeError::Init(format!("write temp script {script_path:?}: {e}")))?;

    println!("[omarchy-novad] Preparing '{wakeword_name}' (uv fetches onnx+openwakeword into a throwaway env, nothing persists)...");
    std::io::stdout().flush().ok();

    let status = Command::new("uv")
        .args(["run", "--with", "onnx", "--with", "openwakeword", "python"])
        .arg(&script_path)
        .arg(wakeword_name)
        .status()
        .map_err(|e| WakeError::Init(format!("spawn uv: {e}")))?;

    let _ = std::fs::remove_file(&script_path);

    if !status.success() {
        return Err(WakeError::Init(format!(
            "model prep failed (uv exited with {status}). \
             '{wakeword_name}' may not be a stock openWakeWord phrase — \
             see the 'Available models' list above, or the wake-word training \
             pointers in src/wake/mod.rs if this is meant to be a custom phrase."
        )));
    }

    println!(
        "[omarchy-novad] Done. 'omarchy-novad detect --wakeword {wakeword_name}' should work now."
    );
    Ok(())
}
