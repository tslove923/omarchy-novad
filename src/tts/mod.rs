//! Client for a locally-run Kokoro TTS HTTP server (see tts-server/ --
//! a small Python process doing ONNX Runtime CPU inference, started
//! independently, same relationship as `[serve]`'s `classify_base_url`
//! has to the LLM serve instance: novad only ever talks to it over
//! HTTP via `ureq`, never manages its process). It's a Python sidecar
//! rather than native Rust inference because Kokoro's G2P step
//! (misaki/espeak-based phonemization) has no realistic native-Rust
//! implementation -- reimplementing that from scratch would buy
//! nothing over calling the reference Python implementation over a
//! loopback HTTP call.
//!
//! Speaks sentence-by-sentence rather than the whole response at
//! once: Kokoro's CPU synthesis speed (roughly 0.3x real-time,
//! measured in tts-prototype/) can't keep a 10-20s response under a
//! second, but the *first sentence* of a typical response can --
//! see tts-prototype/stream_demo.py, which validated this exact
//! pipeline (time-to-first-audio ~0.7s, zero gaps between
//! sentences thereafter). `speak()` reproduces it here: one
//! background thread makes sequential HTTP calls to the TTS server
//! while this thread plays each finished sentence's audio via
//! `paplay` as it arrives, so playback of sentence N overlaps
//! synthesis of sentence N+1 instead of waiting for the whole
//! response to finish.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::config::TtsConfig;

/// Splits `text` into sentences the same naive way
/// tts-prototype/stream_demo.py does. Good enough for the short,
/// plain-prose summaries this speaks -- not meant to handle
/// abbreviations ("Dr. Smith") correctly, which a spoken conversation
/// summary is unlikely to contain.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                sentences.push(trimmed.to_string());
            }
            current.clear();
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        sentences.push(trimmed.to_string());
    }
    sentences
}

/// Synthesizes and speaks `text` sentence-by-sentence via the
/// configured TTS server + `paplay`. Blocks until playback of the
/// whole response finishes. A sentence that fails to synthesize or
/// play is logged and skipped rather than aborting the rest -- one bad
/// sentence shouldn't silence the whole reply.
pub fn speak(text: &str, cfg: &TtsConfig) -> anyhow::Result<()> {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let base_url = cfg.serve_url.clone();
    let voice = cfg.voice.clone();
    std::thread::spawn(move || {
        for sentence in sentences {
            match synthesize(&base_url, &voice, &sentence) {
                Ok(wav) => {
                    if tx.send(wav).is_err() {
                        break; // receiver dropped -- caller gave up
                    }
                }
                Err(e) => tracing::warn!("[tts] synthesis failed for {sentence:?}: {e}"),
            }
        }
    });

    for wav in rx {
        if let Err(e) = play(&wav) {
            tracing::warn!("[tts] playback failed: {e}");
        }
    }
    Ok(())
}

fn synthesize(base_url: &str, voice: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/synthesize", base_url.trim_end_matches('/'));
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(ureq::json!({ "text": text, "voice": voice }))
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading response body: {e}"))?;
    Ok(bytes)
}

/// Plays a WAV byte buffer by piping it to `paplay`'s stdin (no file
/// argument reads from stdin) -- same shell-out philosophy the rest of
/// this crate already uses for external tools (`voxtype`,
/// `openclaw-handoff`, `herdr`) rather than pulling in an
/// audio-output crate for the first time just for this.
fn play(wav_bytes: &[u8]) -> anyhow::Result<()> {
    let mut child = Command::new("paplay")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn paplay: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(wav_bytes)
            .map_err(|e| anyhow::anyhow!("write to paplay stdin: {e}"))?;
    }
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("wait for paplay: {e}"))?;
    if !status.success() {
        anyhow::bail!("paplay exited with {status}");
    }
    Ok(())
}
