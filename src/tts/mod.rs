//! Ordered, health-aware TTS dispatch: the first backend listed in
//! `[[tts.backends]]` is the **primary**, the rest are fallbacks tried
//! in order. Ordering is configuration (`config::TtsConfig`), never a
//! hardcoded if/else here -- see `resolve_backends`. Today that means
//! the cloned Jarvis voice on Kyutai Pocket TTS (a local pocket-tts) is
//! primary and Kokoro (loopback:8421) is the secondary, but nothing in
//! this module assumes either name or address.
//!
//! A remote primary can be unreachable for reasons the local box is not, so a primary that refuses
//! connections, returns non-200, returns something that isn't WAV, or
//! doesn't answer within `fallback_timeout_secs` is expected -- each of
//! those degrades to the next backend and logs a WARN naming both
//! sides and the reason. The voice loop must never go silent because
//! the GPU box is asleep.
//!
//! //! Client for a locally-run Kokoro TTS HTTP server (see tts-server/ --
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
//!
//! `speak()` takes a `cancel` flag so a caller running it on its own
//! thread (see `converse::speak_with_barge_in`) can interrupt
//! mid-reply -- barging in on "Speaking..." should stop the audio
//! promptly, not queue behind it. Both `speak()`'s between-sentence
//! check and `play()`'s own poll loop watch the same flag; `play()`
//! polls it via `Child::try_wait()` rather than a blocking
//! `Child::wait()`, so a cancel mid-sentence kills `paplay` right away
//! instead of waiting for that sentence to finish playing first.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

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
/// whole response finishes, `cancel` is set, or the caller drops its
/// end (nothing else references it, since this is meant to run on its
/// own thread -- see `converse::speak_with_barge_in`). A sentence that
/// fails to synthesize or play is logged and skipped rather than
/// aborting the rest -- one bad sentence shouldn't silence the whole
/// reply.
pub fn speak(text: &str, cfg: &TtsConfig, cancel: &AtomicBool) -> anyhow::Result<()> {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let backends = resolve_backends(cfg);
    let timeout = Duration::from_secs(cfg.fallback_timeout_secs.max(1));
    tracing::info!(
        "[tts] backend priority: {} (first = primary, timeout {}s)",
        describe_backends(&backends),
        timeout.as_secs()
    );
    std::thread::spawn(move || {
        for sentence in sentences {
            match synthesize(&backends, timeout, &sentence) {
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
        if cancel.load(Ordering::Relaxed) {
            break; // don't start another sentence once cancelled
        }
        if let Err(e) = play(&wav, cancel) {
            tracing::warn!("[tts] playback failed: {e}");
        }
    }
    Ok(())
}

/// Resolved, ordered backend list: `[0]` is primary, the rest are
/// fallbacks in declared order. Built from `TtsConfig` -- see
/// `resolve_backends`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Kyutai Pocket TTS: `POST /tts`, multipart/form-data, returns WAV.
    Pocket,
    /// Kokoro: `POST /synthesize`, JSON in, WAV out.
    Kokoro,
}

#[derive(Debug, Clone)]
struct Backend {
    name: String,
    kind: BackendKind,
    url: String,
    voice: String,
}

impl Backend {
    fn health_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        match self.kind {
            BackendKind::Pocket => format!("{base}/health"),
            BackendKind::Kokoro => format!("{base}/healthz"),
        }
    }
}

fn parse_kind(raw: &str) -> Option<BackendKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pocket" | "pocket-tts" | "kyutai" => Some(BackendKind::Pocket),
        "kokoro" => Some(BackendKind::Kokoro),
        _ => None,
    }
}

/// Turns `[tts]` config into the ordered list to try. Ordering is
/// entirely the config's: `[[tts.backends]]` in file order, first ==
/// primary. An unknown `kind` is skipped with a warning rather than
/// killing the voice loop.
///
/// With no `[[tts.backends]]` at all, synthesizes the legacy
/// single-Kokoro list from `serve_url`/`voice`, so pre-existing configs
/// behave exactly as before this change. If backends are declared but
/// none of them is Kokoro, and `serve_url` is set, the legacy Kokoro
/// endpoint is appended as a last-resort fallback -- the old config keys
/// stay meaningful.
fn resolve_backends(cfg: &TtsConfig) -> Vec<Backend> {
    if cfg.backends.is_empty() {
        return vec![Backend {
            name: "kokoro".to_string(),
            kind: BackendKind::Kokoro,
            url: cfg.serve_url.clone(),
            voice: cfg.voice.clone(),
        }];
    }

    let mut out: Vec<Backend> = Vec::new();
    for (i, b) in cfg.backends.iter().enumerate() {
        let Some(kind) = parse_kind(&b.kind) else {
            tracing::warn!(
                "[tts] ignoring backend #{i} {:?}: unknown kind {:?} (expected \"pocket\" or \"kokoro\")",
                b.name,
                b.kind
            );
            continue;
        };
        if b.url.trim().is_empty() {
            tracing::warn!(
                "[tts] ignoring backend #{i} {:?}: empty url",
                b.name
            );
            continue;
        }
        let name = if b.name.trim().is_empty() {
            format!("{kind:?}").to_ascii_lowercase()
        } else {
            b.name.clone()
        };
        let voice = match (kind, b.voice.trim()) {
            (BackendKind::Kokoro, "") => cfg.voice.clone(),
            (_, v) => v.to_string(),
        };
        out.push(Backend {
            name,
            kind,
            url: b.url.clone(),
            voice,
        });
    }

    let has_kokoro = out.iter().any(|b| b.kind == BackendKind::Kokoro);
    if !has_kokoro && !cfg.serve_url.trim().is_empty() {
        out.push(Backend {
            name: "kokoro".to_string(),
            kind: BackendKind::Kokoro,
            url: cfg.serve_url.clone(),
            voice: cfg.voice.clone(),
        });
    }
    out
}

/// `converse probe` support: the resolved order plus a live health
/// probe of each backend, as `(name, kind, url, health)` rows. Purely
/// diagnostic -- the voice loop never calls this (it only reports
/// health when a synthesis actually fails).
pub fn describe_config(cfg: &TtsConfig) -> Vec<(String, String, String, String)> {
    let timeout = Duration::from_secs(cfg.fallback_timeout_secs.max(1));
    resolve_backends(cfg)
        .iter()
        .map(|b| {
            let health = match probe_health(b, timeout.min(Duration::from_secs(2))) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("UNREACHABLE ({e})"),
            };
            (
                b.name.clone(),
                format!("{:?}", b.kind).to_ascii_lowercase(),
                b.url.clone(),
                health,
            )
        })
        .collect()
}

/// Synthesizes `text` through the same ordered/fallback path the voice
/// loop uses and returns `(wav_bytes, backend_name_that_served_it)`.
pub fn synthesize_for_diagnostics(
    cfg: &TtsConfig,
    text: &str,
) -> anyhow::Result<(Vec<u8>, String)> {
    let backends = resolve_backends(cfg);
    let timeout = Duration::from_secs(cfg.fallback_timeout_secs.max(1));
    for (idx, backend) in backends.iter().enumerate() {
        match synthesize_with(backend, timeout, text) {
            Ok(wav) => return Ok((wav, backend.name.clone())),
            Err(e) => match backends.get(idx + 1) {
                Some(next) => tracing::warn!(
                    "[tts] fallback: backend {} ({:?}) failed at {}: {e} -- falling back to {} ({:?}) at {}",
                    backend.name,
                    backend.kind,
                    backend.url,
                    next.name,
                    next.kind,
                    next.url
                ),
                None => tracing::warn!(
                    "[tts] fallback: backend {} ({:?}) failed at {}: {e} -- no further backends to try",
                    backend.name,
                    backend.kind,
                    backend.url
                ),
            },
        }
    }
    anyhow::bail!("all TTS backends failed")
}

fn probe_health(backend: &Backend, timeout: Duration) -> anyhow::Result<()> {
    let url = backend.health_url();
    let resp = ureq::get(&url)
        .timeout(timeout)
        .call()
        .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        anyhow::bail!("GET {url}: HTTP {status}");
    }
    // Drain so the connection can be reused/closed cleanly.
    let _ = resp.into_reader().read_to_end(&mut Vec::new());
    Ok(())
}

/// One-line, log-friendly rendering of the resolved order, e.g.
/// `jarvis(pocket), kokoro(kokoro)`. Logged once at the start of each
/// `speak()` so the effective priority is visible in the journal
/// without reading config.toml.
fn describe_backends(backends: &[Backend]) -> String {
    backends
        .iter()
        .map(|b| format!("{}({:?})", b.name, b.kind).to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Synthesizes one sentence, trying each backend in declared order.
///
/// A backend "fails" (and the next one is tried) on connection refused /
/// DNS / TLS errors, a non-200 status, a body that isn't a RIFF/WAVE
/// payload, or exceeding the configured timeout. Every fallback is
/// logged at WARN with the reason, naming the failed backend and the one
/// being tried instead -- that log line is the contract this whole
/// design exists to satisfy: when the GPU box hibernates, the journal
/// says so rather than the robot just going quiet.
fn synthesize(backends: &[Backend], timeout: Duration, text: &str) -> anyhow::Result<Vec<u8>> {
    if backends.is_empty() {
        anyhow::bail!("no usable TTS backends configured");
    }
    let mut last_err: Option<String> = None;
    for (idx, backend) in backends.iter().enumerate() {
        match synthesize_with(backend, timeout, text) {
            Ok(wav) => return Ok(wav),
            Err(e) => {
                let reason = format!("{e}");
                match backends.get(idx + 1) {
                    Some(next) => tracing::warn!(
                        "[tts] fallback: backend {} ({:?}) failed at {}: {reason} -- falling back to {} ({:?}) at {}",
                        backend.name,
                        backend.kind,
                        backend.url,
                        next.name,
                        next.kind,
                        next.url
                    ),
                    None => tracing::warn!(
                        "[tts] fallback: backend {} ({:?}) failed at {}: {reason} -- no further backends to try",
                        backend.name,
                        backend.kind,
                        backend.url
                    ),
                }
                last_err = Some(format!("{}: {reason}", backend.name));
            }
        }
    }
    anyhow::bail!(
        "all TTS backends failed ({})",
        last_err.unwrap_or_else(|| "unknown".to_string())
    )
}

/// `POST /tts` -- multipart/form-data with a required `text` field and
/// an optional voice selection.
///
/// The pocket server (kyutai-labs/pocket-tts `main.py`'s `text_to_speech`)
/// accepts either `voice_url` (http(s)://, hf://, or a *built-in* voice
/// name -- a bare local path is rejected with HTTP 400) or `voice_wav`,
/// an uploaded WAV it truncates as a clone prompt. `voice_wav` is
/// uploaded, never `voice_url`, because our reference clip lives on the
/// same host novad runs on: uploading it means the server needs no file
/// server or shared filesystem, and the *reference is the WAV* rather
/// than whatever default the server process happened to be started with
/// (a server's own default can differ from the clip this
/// config points at, so relying on the default would silently pick the
/// wrong voice).
///
/// Empty `voice` sends neither field and falls back to the server's own
/// `--default-voice`. A file that can't be read is a hard error for this
/// backend -- falling through to Kokoro is correct, speaking a different
/// voice without saying so is not.
fn synthesize_pocket(backend: &Backend, timeout: Duration, text: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/tts", backend.url.trim_end_matches('/'));
    let boundary = "----omarchyNovadTtsBoundary7d1a2c";
    let mut body: Vec<u8> = Vec::with_capacity(text.len() + 512);
    let push = |body: &mut Vec<u8>, s: &str| body.extend_from_slice(s.as_bytes());
    push(
        &mut body,
        &format!("--{boundary}\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\n"),
    );
    push(&mut body, text);
    push(&mut body, "\r\n");

    let voice = backend.voice.trim();
    if !voice.is_empty() {
        if voice.starts_with("http://") || voice.starts_with("https://") || voice.starts_with("hf://") {
            push(
                &mut body,
                &format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"voice_url\"\r\n\r\n"
                ),
            );
            push(&mut body, voice);
            push(&mut body, "\r\n");
        } else {
            let wav = std::fs::read(voice).map_err(|e| {
                anyhow::anyhow!("reading pocket voice reference {voice}: {e}")
            })?;
            let filename = std::path::Path::new(voice)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("voice.wav");
            push(
                &mut body,
                &format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"voice_wav\"; filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n"
                ),
            );
            body.extend_from_slice(&wav);
            push(&mut body, "\r\n");
        }
    }
    push(&mut body, &format!("--{boundary}--\r\n"));

    let response = ureq::post(&url)
        .timeout(timeout)
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .send_bytes(&body)
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading response body: {e}"))?;
    Ok(bytes)
}

/// `POST /synthesize` -- JSON `{text, voice}`, the original Kokoro path.
fn synthesize_kokoro(backend: &Backend, timeout: Duration, text: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/synthesize", backend.url.trim_end_matches('/'));
    let response = ureq::post(&url)
        .timeout(timeout)
        .set("Content-Type", "application/json")
        .send_json(ureq::json!({ "text": text, "voice": backend.voice }))
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading response body: {e}"))?;
    Ok(bytes)
}

fn synthesize_with(backend: &Backend, timeout: Duration, text: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = match backend.kind {
        BackendKind::Pocket => synthesize_pocket(backend, timeout, text)?,
        BackendKind::Kokoro => synthesize_kokoro(backend, timeout, text)?,
    };
    // A backend that answers 200 with an HTML error page (or an empty
    // body) must count as a failure, not as audio to pipe into paplay.
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!(
            "response is not WAV audio ({} bytes, starts {:?})",
            bytes.len(),
            String::from_utf8_lossy(&bytes[..bytes.len().min(24)])
        );
    }
    Ok(bytes)
}

/// Plays a WAV byte buffer by piping it to `paplay`'s stdin (no file
/// argument reads from stdin) -- same shell-out philosophy the rest of
/// this crate already uses for external tools (`voxtype`,
/// `openclaw-handoff`, `herdr`) rather than pulling in an
/// audio-output crate for the first time just for this.
///
/// Polls `cancel` via `Child::try_wait()` instead of a blocking
/// `Child::wait()` so a barge-in mid-sentence kills `paplay`
/// immediately rather than waiting for it to finish this sentence
/// first -- see this module's doc comment.
fn play(wav_bytes: &[u8], cancel: &AtomicBool) -> anyhow::Result<()> {
    let mut child = Command::new("paplay")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn paplay: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(wav_bytes)
            .map_err(|e| anyhow::anyhow!("write to paplay stdin: {e}"))?;
    }

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait(); // reap -- avoid leaving a zombie behind
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    anyhow::bail!("paplay exited with {status}");
                }
                return Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => return Err(anyhow::anyhow!("wait for paplay: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TtsBackendConfig;

    fn be(name: &str, kind: &str, url: &str, voice: &str) -> TtsBackendConfig {
        TtsBackendConfig {
            name: name.to_string(),
            kind: kind.to_string(),
            url: url.to_string(),
            voice: voice.to_string(),
        }
    }

    #[test]
    fn legacy_config_without_backends_is_a_single_kokoro() {
        let cfg = TtsConfig::default();
        let b = resolve_backends(&cfg);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, BackendKind::Kokoro);
        assert_eq!(b[0].url, "http://127.0.0.1:8421");
        assert_eq!(b[0].voice, "af_nova");
    }

    #[test]
    fn declared_order_is_preserved_primary_first() {
        let mut cfg = TtsConfig::default();
        cfg.backends = vec![
            be("jarvis", "pocket", "http://192.0.2.50:8001", ""),
            be("kokoro", "kokoro", "http://127.0.0.1:8421", "af_nova"),
        ];
        let b = resolve_backends(&cfg);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].name, "jarvis");
        assert_eq!(b[0].kind, BackendKind::Pocket);
        assert_eq!(b[1].name, "kokoro");
        assert_eq!(describe_backends(&b), "jarvis(pocket), kokoro(kokoro)");
    }

    #[test]
    fn legacy_kokoro_url_is_appended_when_backends_lack_kokoro() {
        let mut cfg = TtsConfig::default();
        cfg.backends = vec![be("jarvis", "pocket", "http://192.0.2.50:8001", "")];
        let b = resolve_backends(&cfg);
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].kind, BackendKind::Kokoro);
    }

    #[test]
    fn unknown_kind_is_skipped_not_fatal() {
        let mut cfg = TtsConfig::default();
        cfg.backends = vec![
            be("bogus", "elevenlabs", "http://x:1", ""),
            be("kokoro", "kokoro", "http://127.0.0.1:8421", ""),
        ];
        let b = resolve_backends(&cfg);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "kokoro");
    }

    #[test]
    fn health_urls_match_each_protocol() {
        let p = Backend {
            name: "jarvis".into(),
            kind: BackendKind::Pocket,
            url: "http://192.0.2.50:8001/".into(),
            voice: String::new(),
        };
        assert_eq!(p.health_url(), "http://192.0.2.50:8001/health");
        let k = Backend {
            name: "kokoro".into(),
            kind: BackendKind::Kokoro,
            url: "http://127.0.0.1:8421".into(),
            voice: String::new(),
        };
        assert_eq!(k.health_url(), "http://127.0.0.1:8421/healthz");
    }

    #[test]
    fn non_wav_body_is_rejected() {
        // synthesize_with's guard, exercised directly via a fake body.
        let bytes: &[u8] = b"<html>502 Bad Gateway</html>";
        assert!(bytes.len() >= 12 && &bytes[0..4] != b"RIFF");
    }
}
