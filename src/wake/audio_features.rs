//! Streaming audio feature extraction for NPU-accelerated wake word detection.
//!
//! Direct port of nova-npu's `audio_features.py`. Mirrors openwakeword's
//! `AudioFeatures` but delegates mel spectrogram and embedding computation
//! to [`super::npu_engine::NpuEngine`]. All buffers are pre-allocated to
//! avoid per-frame allocation in the 80ms hot loop.

use super::npu_engine::NpuEngine;
use crate::wake::WakeError;

pub const SAMPLE_RATE: u32 = 16_000;
/// 80ms — the standard processing chunk.
pub const CHUNK_SAMPLES: usize = 1280;
/// 160 * 3 — overlap samples for melspec continuity.
const MELSPEC_OVERLAP: usize = 480;
/// 1760 — what the melspectrogram model expects.
const MELSPEC_INPUT_LEN: usize = CHUNK_SAMPLES + MELSPEC_OVERLAP;
const MELSPEC_BINS: usize = 32;
/// Mel frames needed for one embedding.
const EMBEDDING_WINDOW: usize = 76;
/// Mel frames between embeddings (from 1280-sample chunks).
const EMBEDDING_STEP: usize = 8;
const EMBEDDING_DIM: usize = 96;
/// 10 seconds of audio history.
const MAX_RAW_SAMPLES: usize = SAMPLE_RATE as usize * 10;
/// ~10 seconds of mel frames.
const MAX_MELSPEC_FRAMES: usize = 97 * 10;
/// Max embedding history.
const MAX_FEATURE_FRAMES: usize = 120;

/// Streaming audio feature extraction using the NPU.
///
/// All internal buffers are pre-allocated `Vec`s with write cursors,
/// matching the Python implementation's numpy ring buffers.
pub struct AudioFeatures {
    // Raw audio ring buffer (int16 samples).
    raw_buf: Vec<i16>,
    raw_len: usize,

    // Mel spectrogram ring buffer. Pre-filled with 1.0 to simulate
    // silence, matching openwakeword's convention.
    mel_buf: Vec<f32>, // MAX_MELSPEC_FRAMES x MELSPEC_BINS, row-major
    mel_len: usize,

    // Embedding ring buffer.
    feat_buf: Vec<f32>, // MAX_FEATURE_FRAMES x EMBEDDING_DIM, row-major
    feat_len: usize,

    // Scratch buffers reused every call, no per-frame allocation.
    mel_input: Vec<f32>, // [1, MELSPEC_INPUT_LEN]
    emb_input: Vec<f32>, // [1, EMBEDDING_WINDOW, MELSPEC_BINS, 1]

    accumulated_samples: usize,
}

impl AudioFeatures {
    pub fn new() -> Self {
        Self {
            raw_buf: vec![0i16; MAX_RAW_SAMPLES],
            raw_len: 0,
            mel_buf: vec![1.0f32; MAX_MELSPEC_FRAMES * MELSPEC_BINS],
            mel_len: EMBEDDING_WINDOW, // pre-filled silence
            feat_buf: vec![0.0f32; MAX_FEATURE_FRAMES * EMBEDDING_DIM],
            feat_len: 0,
            mel_input: vec![0.0f32; MELSPEC_INPUT_LEN],
            emb_input: vec![0.0f32; EMBEDDING_WINDOW * MELSPEC_BINS],
            accumulated_samples: 0,
        }
    }

    pub fn reset(&mut self) {
        self.raw_buf.fill(0);
        self.raw_len = 0;
        self.mel_buf.fill(1.0);
        self.mel_len = EMBEDDING_WINDOW;
        self.feat_buf.fill(0.0);
        self.feat_len = 0;
        self.accumulated_samples = 0;
    }

    /// Process a chunk of raw audio samples (int16 @ 16kHz, typically
    /// [`CHUNK_SAMPLES`] samples / 80ms, but any length is accepted).
    pub fn process(&mut self, engine: &mut NpuEngine, audio: &[i16]) -> Result<(), WakeError> {
        let n = audio.len();

        // Append to raw ring buffer, shifting left if full — keeps the
        // most recent MAX_RAW_SAMPLES samples.
        if self.raw_len + n > MAX_RAW_SAMPLES {
            let mut keep = MAX_RAW_SAMPLES.saturating_sub(n);
            if keep > 0 && self.raw_len > 0 {
                let src_start = self.raw_len.saturating_sub(keep);
                let src_start = if src_start == 0 && keep > self.raw_len {
                    keep = self.raw_len;
                    0
                } else {
                    src_start
                };
                self.raw_buf.copy_within(src_start..src_start + keep, 0);
                self.raw_len = keep;
            } else {
                self.raw_len = 0;
            }
        }
        self.raw_buf[self.raw_len..self.raw_len + n].copy_from_slice(audio);
        self.raw_len += n;
        self.accumulated_samples += n;

        if self.accumulated_samples >= CHUNK_SAMPLES {
            self.compute_features(engine)?;
            self.accumulated_samples = 0;
        }
        Ok(())
    }

    /// Run mel spectrogram and embedding computation on accumulated audio.
    fn compute_features(&mut self, engine: &mut NpuEngine) -> Result<(), WakeError> {
        let n_samples = self.accumulated_samples;
        let total_needed = n_samples + MELSPEC_OVERLAP;
        let available = self.raw_len.min(total_needed);

        self.mel_input.fill(0.0);
        let src_start = self.raw_len - available;
        let tail_len = available.min(MELSPEC_INPUT_LEN);
        let dst_start = MELSPEC_INPUT_LEN - tail_len;
        for (i, &s) in self.raw_buf[src_start..src_start + tail_len]
            .iter()
            .enumerate()
        {
            self.mel_input[dst_start + i] = s as f32;
        }

        // spec: n_new_frames x MELSPEC_BINS, row-major.
        let spec = engine.melspectrogram(&self.mel_input)?;
        let n_new_frames = spec.len() / MELSPEC_BINS;

        // Append to mel ring buffer, shifting left if full.
        if self.mel_len + n_new_frames > MAX_MELSPEC_FRAMES {
            let keep = MAX_MELSPEC_FRAMES - n_new_frames;
            let src_start = (self.mel_len - keep) * MELSPEC_BINS;
            self.mel_buf
                .copy_within(src_start..src_start + keep * MELSPEC_BINS, 0);
            self.mel_len = keep;
        }
        let dst = self.mel_len * MELSPEC_BINS;
        self.mel_buf[dst..dst + spec.len()].copy_from_slice(&spec);
        self.mel_len += n_new_frames;

        // Compute embeddings from the mel sliding window. One new
        // embedding per CHUNK_SAMPLES-worth of new audio, oldest first.
        let n_new_embeddings = (n_samples / CHUNK_SAMPLES).max(1);
        for i in (0..n_new_embeddings).rev() {
            let offset = EMBEDDING_STEP * i;
            if offset > self.mel_len {
                continue;
            }
            let end = self.mel_len - offset;
            if end < EMBEDDING_WINDOW {
                continue;
            }
            let start = end - EMBEDDING_WINDOW;

            let window = &self.mel_buf[start * MELSPEC_BINS..end * MELSPEC_BINS];
            self.emb_input.copy_from_slice(window);
            let embedding = engine.embedding(&self.emb_input)?; // [EMBEDDING_DIM]

            if self.feat_len >= MAX_FEATURE_FRAMES {
                self.feat_buf.copy_within(EMBEDDING_DIM.., 0);
                self.feat_len = MAX_FEATURE_FRAMES - 1;
            }
            let dst = self.feat_len * EMBEDDING_DIM;
            self.feat_buf[dst..dst + EMBEDDING_DIM].copy_from_slice(&embedding);
            self.feat_len += 1;
        }
        Ok(())
    }

    /// Get the last `n_frames` embedding frames for wakeword detection,
    /// zero-padded at the front if not enough history yet. Returns a
    /// flat `n_frames * EMBEDDING_DIM` buffer (shape `[1, n_frames, 96]`).
    pub fn get_features(&self, n_frames: usize) -> Vec<f32> {
        let mut result = vec![0.0f32; n_frames * EMBEDDING_DIM];
        if self.feat_len >= n_frames {
            let start = self.feat_len - n_frames;
            result.copy_from_slice(
                &self.feat_buf[start * EMBEDDING_DIM..self.feat_len * EMBEDDING_DIM],
            );
        } else if self.feat_len > 0 {
            let dst_start = (n_frames - self.feat_len) * EMBEDDING_DIM;
            result[dst_start..].copy_from_slice(&self.feat_buf[..self.feat_len * EMBEDDING_DIM]);
        }
        result
    }
}

impl Default for AudioFeatures {
    fn default() -> Self {
        Self::new()
    }
}
