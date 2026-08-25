//! External handoff to OpenClaw — port of nova-npu's
//! `ai/commands/coding_bridge.py` (routes anything the local
//! classifier can't handle itself to a real reasoning/coding agent),
//! scoped to what's actually available here: a CLI bridge script
//! (`openclaw-handoff`, `~/.local/bin/`) rather than nova's themed
//! Electron chat window + REST API popup integration.
//!
//! The bridge script (not this file) owns the actual OpenClaw
//! transport (gateway WebSocket URL + token, from
//! `~/.config/openclaw-novad.env`, chmod 600 -- the script's own
//! path, unrelated to this crate's rename to omarchy-novad) — this
//! module only knows how to invoke it and interpret its exit code,
//! matching the shell-out pattern `app_launcher`/`web` already use
//! for their own external processes.

use std::process::{Command, Stdio};
use std::time::Duration;

/// Generous timeout for one handoff round-trip. OpenClaw runs a real
/// agent turn (can involve tool calls, web fetches, etc.), not a
/// single classify-sized LLM call -- the ~10s PING smoke test was the
/// floor, not the ceiling. Long enough to cover a real request, short
/// enough that a hung gateway doesn't stall the popup indefinitely.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);

/// All wake-word-triggered handoffs share one conversation so OpenClaw
/// keeps context across turns (nova's own `coding_bridge.py` did the
/// same with its in-process `_history` list) -- omarchy-novad doesn't have a
/// per-session/per-user conversation concept yet, so this is the
/// simplest thing that gives real continuity today. Revisit if omarchy-novad
/// ever needs to distinguish separate voice "conversations" (e.g. a
/// timeout-based reset, or multiple concurrent users).
const CONVERSATION_ID: &str = "voice";

/// Hands `utterance` off to OpenClaw via the `openclaw-handoff` CLI
/// bridge and returns `(success, reply_or_error)`. `reply` is
/// OpenClaw's answer, ready to show as-is in the popup's Ready phase.
pub fn handoff(utterance: &str) -> (bool, String) {
    let clean = utterance.trim();
    if clean.is_empty() {
        return (false, "Nothing to hand off".to_string());
    }

    tracing::debug!("[router:openclaw] handoff: {clean:?}");

    let mut child = match Command::new("openclaw-handoff")
        .arg(clean)
        .arg(CONVERSATION_ID)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[router:openclaw] failed to spawn openclaw-handoff: {e}");
            return (
                false,
                "The external assistant isn't available right now".to_string(),
            );
        }
    };

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= HANDOFF_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!("[router:openclaw] handoff timed out after {HANDOFF_TIMEOUT:?}");
                    return (
                        false,
                        "The external assistant took too long to respond".to_string(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tracing::warn!("[router:openclaw] wait failed: {e}");
                return (
                    false,
                    "The external assistant failed to respond".to_string(),
                );
            }
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    {
        use std::io::Read;
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout);
        }
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
    }

    if status.success() {
        let reply = stdout.trim();
        if reply.is_empty() {
            (
                false,
                "The external assistant replied with nothing".to_string(),
            )
        } else {
            (true, reply.to_string())
        }
    } else {
        // openclaw-handoff writes its own user-facing fallback line to
        // stderr on failure (gateway unreachable, device unpaired,
        // etc.) -- surface that instead of a generic message so the
        // popup shows the real reason.
        let msg = stderr.lines().last().unwrap_or("").trim();
        let msg = if msg.is_empty() {
            "The external assistant is unavailable right now"
        } else {
            msg
        };
        tracing::warn!("[router:openclaw] handoff failed: {msg}");
        (false, msg.to_string())
    }
}
