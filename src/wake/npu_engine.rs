//! NPU inference engine for the 3-stage wake word pipeline.
//!
//! Port of nova-npu's `npu_engine.py`. Manages OpenVINO model compilation
//! and inference for openWakeWord's pre-trained ONNX models (converted
//! offline to NPU-compatible OpenVINO IR — see `model_paths.rs`), with
//! automatic fallback to CPU if NPU is unavailable.
//!
//! The pipeline:
//!   melspectrogram: `[1, 1760]`        -> `[1, 1, N_frames, 32]`
//!   embedding:      `[1, 76, 32, 1]`   -> `[1, 1, 1, 96]`
//!   wakeword:       `[1, 16, 96]`      -> p1 `[1, 1]`, p2 `[1, 1]`
//!
//! For the wakeword model, the conversion step (not yet ported — see
//! Phase roadmap) removes the ONNX If-node and always computes both the
//! primary (p1) and verifier (p2) scores; the threshold logic below
//! (`score = p2 if p1 >= VERIFIER_THRESHOLD else p1`) applies in Rust
//! instead of inside the graph.

use openvino::{Core, DeviceType, InferRequest, PartialShape, RwPropertyKey, Shape, Tensor};

use super::model_paths::WakeModelPaths;
use crate::wake::WakeError;

/// Verifier threshold extracted from the original model's `GreaterOrEqual`
/// node. Used by `wakeword()`; `Detector` currently inlines the same
/// check itself against raw scores rather than calling that method —
/// kept here since it's the pipeline's documented contract either way.
#[allow(dead_code)]
const VERIFIER_THRESHOLD: f32 = 0.5;

pub struct NpuEngine {
    pub device: String,
    // Kept alive for its properties (CACHE_DIR etc. stay in effect for
    // as long as this Core exists) rather than actively called again
    // after setup — not dead weight, just not read again.
    #[allow(dead_code)]
    core: Core,
    mel_request: InferRequest,
    emb_request: InferRequest,
    ww_request: InferRequest,
    // Reserved for the `novad detect --stats` diagnostics surface (see
    // Detector::avg_inference_ms).
    #[allow(dead_code)]
    pub compile_times: [(&'static str, std::time::Duration); 3],
}

impl NpuEngine {
    pub fn new(
        paths: &WakeModelPaths,
        device: &str,
        cache_dir: &std::path::Path,
    ) -> Result<Self, WakeError> {
        let mut core = Core::new().map_err(|e| WakeError::Init(format!("ov::Core::new: {e}")))?;

        let available = core
            .available_devices()
            .map_err(|e| WakeError::Init(format!("available_devices: {e}")))?;
        let available_names: Vec<String> =
            available.iter().map(|d| d.as_ref().to_string()).collect();
        let device = if available_names.iter().any(|d| d == device) {
            device.to_string()
        } else {
            tracing::warn!(
                "Device '{device}' not available (have: {available_names:?}), falling back to CPU"
            );
            "CPU".to_string()
        };

        // Persist compiled device blobs to disk (same CACHE_DIR mechanism
        // as voxtype's OpenVINO Whisper backend — see
        // src/transcribe/openvino.rs there). Set once on the Core; every
        // subsequent compile_model on this device honors it.
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| WakeError::Init(format!("create cache dir {cache_dir:?}: {e}")))?;
        let device_type = DeviceType::from(device.as_str());
        let cache_dir_str = cache_dir.to_string_lossy();
        core.set_property(&device_type, &RwPropertyKey::CacheDir, &cache_dir_str)
            .map_err(|e| WakeError::Init(format!("set CACHE_DIR: {e}")))?;

        // Melspectrogram must run on CPU — NPU's FP16 path corrupts the
        // FFT output. The model is tiny (~2ms) so CPU overhead is
        // negligible; embedding and wakeword run on the target device.
        let (mel_compiled, mel_t) =
            compile(&mut core, &paths.melspectrogram, "CPU", Some(&[1, 1760]))?;
        let (emb_compiled, emb_t) =
            compile(&mut core, &paths.embedding, &device, Some(&[1, 76, 32, 1]))?;
        let (ww_compiled, ww_t) = compile(&mut core, &paths.wakeword, &device, None)?;

        tracing::info!(
            "All models compiled on {device} in {:.2}s (mel={:.2}s, emb={:.2}s, ww={:.2}s)",
            (mel_t + emb_t + ww_t).as_secs_f32(),
            mel_t.as_secs_f32(),
            emb_t.as_secs_f32(),
            ww_t.as_secs_f32(),
        );

        let mut mel_compiled = mel_compiled;
        let mut emb_compiled = emb_compiled;
        let mut ww_compiled = ww_compiled;
        let mel_request = mel_compiled
            .create_infer_request()
            .map_err(|e| WakeError::Init(format!("mel infer request: {e}")))?;
        let emb_request = emb_compiled
            .create_infer_request()
            .map_err(|e| WakeError::Init(format!("embedding infer request: {e}")))?;
        let ww_request = ww_compiled
            .create_infer_request()
            .map_err(|e| WakeError::Init(format!("wakeword infer request: {e}")))?;

        Ok(Self {
            device,
            core,
            mel_request,
            emb_request,
            ww_request,
            compile_times: [
                ("melspectrogram", mel_t),
                ("embedding", emb_t),
                ("wakeword", ww_t),
            ],
        })
    }

    /// Compute mel spectrogram from raw audio samples.
    ///
    /// `audio`: `[1, 1760]` float32, int16-range values (NOT normalized
    /// to ±1.0 — matches openwakeword's convention).
    ///
    /// Returns a flat `N_frames * 32` buffer, transform applied
    /// (`raw_output / 10 + 2`, matching the Python engine).
    pub fn melspectrogram(&mut self, audio: &[f32]) -> Result<Vec<f32>, WakeError> {
        let mut raw = infer_1in_1out(&mut self.mel_request, audio, &[1, audio.len() as i64])?;
        for v in raw.iter_mut() {
            *v = *v / 10.0 + 2.0;
        }
        Ok(raw)
    }

    /// Compute an embedding from a mel spectrogram window.
    ///
    /// `melspec_window`: flat `[1, 76, 32, 1]` float32.
    /// Returns a `[96]` embedding vector.
    pub fn embedding(&mut self, melspec_window: &[f32]) -> Result<Vec<f32>, WakeError> {
        infer_1in_1out(&mut self.emb_request, melspec_window, &[1, 76, 32, 1])
    }

    /// Run wakeword detection, returning both raw scores (primary, verifier).
    pub fn wakeword_raw(&mut self, features: &[f32]) -> Result<(f32, f32), WakeError> {
        let shape =
            Shape::new(&[1, 16, 96]).map_err(|e| WakeError::Infer(format!("shape: {e}")))?;
        let mut input = Tensor::new(openvino::ElementType::F32, &shape)
            .map_err(|e| WakeError::Infer(format!("tensor: {e}")))?;
        input
            .get_data_mut::<f32>()
            .map_err(|e| WakeError::Infer(format!("tensor data: {e}")))?
            .copy_from_slice(features);

        self.ww_request
            .set_input_tensor(&input)
            .map_err(|e| WakeError::Infer(format!("set input: {e}")))?;
        self.ww_request
            .infer()
            .map_err(|e| WakeError::Infer(format!("infer: {e}")))?;

        let p1 = self
            .ww_request
            .get_output_tensor_by_index(0)
            .map_err(|e| WakeError::Infer(format!("output 0: {e}")))?
            .get_data::<f32>()
            .map_err(|e| WakeError::Infer(format!("output 0 data: {e}")))?[0];
        let p2 = self
            .ww_request
            .get_output_tensor_by_index(1)
            .map_err(|e| WakeError::Infer(format!("output 1: {e}")))?
            .get_data::<f32>()
            .map_err(|e| WakeError::Infer(format!("output 1 data: {e}")))?[0];
        Ok((p1, p2))
    }

    /// Applies the verifier threshold: p2 when p1 >= [`VERIFIER_THRESHOLD`], else p1.
    /// `Detector::process` inlines this same check against `wakeword_raw`
    /// directly (matching nova's own `detector.py`, which does the same);
    /// kept as a convenience method for any other caller that just wants
    /// the thresholded score.
    #[allow(dead_code)]
    pub fn wakeword(&mut self, features: &[f32]) -> Result<f32, WakeError> {
        let (p1, p2) = self.wakeword_raw(features)?;
        Ok(if p1 >= VERIFIER_THRESHOLD { p2 } else { p1 })
    }
}

fn compile(
    core: &mut Core,
    model_path: &std::path::Path,
    device: &str,
    static_shape: Option<&[i64]>,
) -> Result<(openvino::CompiledModel, std::time::Duration), WakeError> {
    let t0 = std::time::Instant::now();
    let path_str = model_path
        .to_str()
        .ok_or_else(|| WakeError::Init(format!("non-utf8 model path: {model_path:?}")))?;
    // Empty weights_path: OpenVINO derives `<same-basename>.bin` next to the .xml.
    let mut model = core
        .read_model_from_file(path_str, "")
        .map_err(|e| WakeError::Init(format!("read_model {path_str}: {e}")))?;

    if let Some(dims) = static_shape {
        let partial = PartialShape::new_static(dims.len() as i64, dims)
            .map_err(|e| WakeError::Init(format!("shape: {e}")))?;
        model
            .reshape_single_input(&partial)
            .map_err(|e| WakeError::Init(format!("reshape {path_str}: {e}")))?;
    }

    let device_type = DeviceType::from(device);
    let compiled = core
        .compile_model(&model, device_type)
        .map_err(|e| WakeError::Init(format!("compile {path_str} on {device}: {e}")))?;
    let elapsed = t0.elapsed();
    tracing::info!(
        "Compiled {} on {device} ({:.2}s)",
        model_path.display(),
        elapsed.as_secs_f32()
    );
    Ok((compiled, elapsed))
}

/// Run a single-input, single-output inference and return the flat output buffer.
fn infer_1in_1out(
    request: &mut InferRequest,
    input_data: &[f32],
    shape: &[i64],
) -> Result<Vec<f32>, WakeError> {
    let shape = Shape::new(shape).map_err(|e| WakeError::Infer(format!("shape: {e}")))?;
    let mut input = Tensor::new(openvino::ElementType::F32, &shape)
        .map_err(|e| WakeError::Infer(format!("tensor: {e}")))?;
    input
        .get_data_mut::<f32>()
        .map_err(|e| WakeError::Infer(format!("tensor data: {e}")))?
        .copy_from_slice(input_data);

    request
        .set_input_tensor(&input)
        .map_err(|e| WakeError::Infer(format!("set input: {e}")))?;
    request
        .infer()
        .map_err(|e| WakeError::Infer(format!("infer: {e}")))?;

    let out = request
        .get_output_tensor()
        .map_err(|e| WakeError::Infer(format!("output: {e}")))?;
    Ok(out
        .get_data::<f32>()
        .map_err(|e| WakeError::Infer(format!("output data: {e}")))?
        .to_vec())
}
