//! Short synthesized audio cues for conversation-loop state
//! transitions -- see `docs/design-notes/conversation-flow-redesign.md`'s
//! "Audio cues" section. Plain sine tones generated in-process rather
//! than round-tripping through Kokoro (`tts::synthesize`) for a
//! ~150-200ms blip -- that's real HTTP + neural-synthesis latency to
//! pay for something that should feel instant, and these three tones
//! never change, so there's nothing a real voice would add. Each tone
//! is synthesized once (`std::sync::OnceLock`) and reused for the rest
//! of the process's life; playback reuses the same
//! `paplay`-over-stdin shell-out `tts::mod` uses for real speech, just
//! without that module's cancellation machinery -- these cues are
//! short enough (and never gate anything) that letting one finish
//! playing is never worth the complexity of interrupting it.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const SAMPLE_RATE: u32 = 24_000;

/// Which state transition this cue marks.
#[derive(Debug, Clone, Copy)]
pub enum Chime {
    /// The talk key was pressed (or a barge-in re-entered Listening)
    /// -- recording is about to start. A short rising two-note "go
    /// ahead", the walkie-talkie convention: wait for the beep, then
    /// talk.
    ListenStart,
    /// A non-empty transcript (or typed message) was just handed to
    /// OpenClaw. A single short, low-key tone -- "heard you, working
    /// on it" -- distinct from silence meaning "did that register?"
    Sent,
    /// The reply is in and playback is about to start. A soft falling
    /// two-note cue, timed to land inside the ~0.7s gap before
    /// Kokoro's first synthesized audio (see `tts` module doc
    /// comment) rather than leaving it silent.
    ReplyReady,
}

/// Plays `chime`, logging (not failing) on any error -- same
/// best-effort philosophy as the rest of this crate's audio/state
/// writes: a missed cue is a minor annoyance, never a reason to stall
/// or abort the conversation loop. Blocks until playback finishes
/// (a few hundred ms at most) -- see this module's doc comment on why
/// that's not worth making cancellable.
pub fn play(chime: Chime) {
    if let Err(e) = play_wav(wav_for(chime)) {
        tracing::warn!("[chime] playback failed: {e}");
    }
}

fn wav_for(chime: Chime) -> &'static [u8] {
    match chime {
        Chime::ListenStart => {
            static CACHED: OnceLock<Vec<u8>> = OnceLock::new();
            CACHED.get_or_init(|| two_note(880.0, 1175.0))
        }
        Chime::Sent => {
            static CACHED: OnceLock<Vec<u8>> = OnceLock::new();
            CACHED.get_or_init(|| single_note(660.0, 90))
        }
        Chime::ReplyReady => {
            static CACHED: OnceLock<Vec<u8>> = OnceLock::new();
            CACHED.get_or_init(|| two_note(880.0, 660.0))
        }
    }
}

/// A single sine tone at `freq_hz`, `duration_ms` long, with a short
/// linear fade in/out so it doesn't click at the start/end.
fn synthesize_samples(freq_hz: f32, duration_ms: u32) -> Vec<i16> {
    let n = (SAMPLE_RATE as u64 * duration_ms as u64 / 1000) as usize;
    let fade_samples = ((SAMPLE_RATE / 200) as usize).clamp(1, (n / 2).max(1));

    (0..n)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let mut amp = (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            if i < fade_samples {
                amp *= i as f32 / fade_samples as f32;
            } else if i >= n - fade_samples {
                amp *= (n - i) as f32 / fade_samples as f32;
            }
            // 0.4: a gentle cue, not full-volume -- these interrupt
            // whatever the user is doing, so they shouldn't startle.
            (amp * i16::MAX as f32 * 0.4) as i16
        })
        .collect()
}

fn single_note(freq_hz: f32, duration_ms: u32) -> Vec<u8> {
    pcm_to_wav(&synthesize_samples(freq_hz, duration_ms))
}

/// Two short notes back-to-back with a brief silent gap -- the
/// "rising"/"falling" two-tone chirps `ListenStart`/`ReplyReady` use.
fn two_note(freq1_hz: f32, freq2_hz: f32) -> Vec<u8> {
    const NOTE_MS: u32 = 90;
    const GAP_MS: u32 = 20;
    let gap_samples = (SAMPLE_RATE as u64 * GAP_MS as u64 / 1000) as usize;

    let mut samples = synthesize_samples(freq1_hz, NOTE_MS);
    samples.extend(std::iter::repeat_n(0i16, gap_samples));
    samples.extend(synthesize_samples(freq2_hz, NOTE_MS));
    pcm_to_wav(&samples)
}

/// Wraps mono 16-bit PCM samples in a minimal 44-byte WAV header --
/// same format `tts::synthesize`'s Kokoro response already comes back
/// as, just built by hand here instead of fetched over HTTP.
fn pcm_to_wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = SAMPLE_RATE * 2; // 16-bit mono: 2 bytes/sample

    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// Same shell-out-to-paplay-over-stdin pattern as `tts::play` (before
/// that module's cancellation rework) -- blocking, no `cancel` flag,
/// see this module's doc comment on why these cues don't need one.
fn play_wav(wav_bytes: &[u8]) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_to_wav_header_matches_sample_data() {
        let samples = synthesize_samples(440.0, 50);
        let wav = pcm_to_wav(&samples);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + samples.len() * 2);
        let declared_data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(declared_data_len as usize, samples.len() * 2);
    }

    #[test]
    fn synthesize_samples_fades_in_and_out_to_avoid_clicks() {
        let samples = synthesize_samples(440.0, 100);
        assert_eq!(samples[0], 0, "should start at zero amplitude");
        // Not necessarily exactly zero at the very last sample
        // depending on rounding, but should be small relative to the
        // tone's peak.
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(samples.last().unwrap().unsigned_abs() < peak / 4);
    }

    #[test]
    fn two_note_is_longer_than_either_single_note() {
        let a = single_note(880.0, 90);
        let b = two_note(880.0, 1175.0);
        assert!(b.len() > a.len());
    }

    #[test]
    fn chime_wav_bytes_are_cached_across_calls() {
        // Same pointer/identity on repeated calls -- confirms
        // `OnceLock` is actually reused, not resynthesized each time.
        let first = wav_for(Chime::Sent).as_ptr();
        let second = wav_for(Chime::Sent).as_ptr();
        assert_eq!(first, second);
    }
}
