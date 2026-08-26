//! External handoff to OmaPilot -- sibling to openclaw.rs, wired the
//! same way into `is_external_handoff`/`route`, but structurally
//! different in one important way: OpenClaw's CLI bridge is a real
//! request/reply round-trip (it returns the agent's answer as text);
//! OmaPilot's `askText` IPC command is fire-and-forget. It opens
//! OmaPilot's own panel and starts it answering there -- the answer
//! streams into OmaPilot's UI directly, never back through this
//! process, so `ask` can only report whether the *handoff* succeeded,
//! never the answer's content. Callers that want to show something in
//! omarchy-novad's own popup (see pipeline.rs) get an acknowledgement
//! message, not a reply.
//!
//! `askText` isn't part of OmaPilot's upstream API -- it's a local
//! patch (`patch/omapilot-asktext-ipc` in the plugin's own git clone
//! at `~/.config/omarchy/plugins/io.github.spencerbull.omapilot`,
//! committed but not upstreamed). Without that patch this whole module
//! degrades to "OmaPilot isn't available right now" for every call,
//! since `omarchy-shell ... askText ...` would just fail with
//! "Function not found" -- see that commit's message for why `askText`
//! exists and what it does on OmaPilot's side.
//!
//! Also worth knowing before enabling `[omapilot] fallback`/
//! `direct_target`: OmaPilot's configured provider can be a real
//! tool-capable agent (its "pi" harness), and if OmaPilot's own
//! `configuredDangerousAutoApprove` setting is on, `askText` hands it
//! a transcript that gets acted on without a human confirming each
//! tool call -- omarchy-novad has no visibility into that setting and
//! can't override it from here. Confirmed live (2026-08-26) that a
//! plain question produces zero tool calls when auto-approve is off;
//! this isn't a guarantee about every possible utterance or every
//! possible OmaPilot configuration.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::OmaPilotConfig;

/// Generous timeout for the `omarchy-shell` round-trip itself (queuing
/// the request and getting `askText`'s "ok"/"busy" acknowledgement
/// back) -- this is NOT how long OmaPilot takes to finish answering;
/// that happens asynchronously in its own panel after this returns.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Hands `utterance` to OmaPilot via `askText` and returns
/// `(success, message)` -- `message` is an acknowledgement for
/// omarchy-novad's own popup, never OmaPilot's actual answer (see
/// module docs for why).
pub fn ask(utterance: &str, config: &OmaPilotConfig) -> (bool, String) {
    let clean = utterance.trim();
    if clean.is_empty() {
        return (false, "Nothing to hand off".to_string());
    }

    tracing::debug!("[router:omapilot] askText: {clean:?}");

    let mut child = match Command::new("omarchy-shell")
        .args([&config.plugin_id, "askText", clean])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[router:omapilot] failed to spawn omarchy-shell: {e}");
            return (false, "OmaPilot isn't available right now".to_string());
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
                    tracing::warn!("[router:omapilot] askText timed out after {HANDOFF_TIMEOUT:?}");
                    return (false, "OmaPilot took too long to respond".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tracing::warn!("[router:omapilot] wait failed: {e}");
                return (false, "OmaPilot failed to respond".to_string());
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

    if !status.success() {
        // "Function not found" (askText patch not applied), the plugin
        // not enabled, or omarchy-shell itself unavailable -- all land
        // here via a non-zero exit and a message on stderr.
        let msg = stderr.lines().last().unwrap_or("").trim();
        let msg = if msg.is_empty() {
            "OmaPilot isn't available right now"
        } else {
            msg
        };
        tracing::warn!("[router:omapilot] askText failed: {msg}");
        return (false, msg.to_string());
    }

    match stdout.trim() {
        "ok" => (true, "Handed off to OmaPilot".to_string()),
        "busy" => (false, "OmaPilot is busy with another request".to_string()),
        "empty" => (false, "Nothing to hand off".to_string()),
        other => {
            tracing::warn!("[router:omapilot] askText returned unexpected output: {other:?}");
            (
                false,
                "OmaPilot returned an unexpected response".to_string(),
            )
        }
    }
}

/// Strips `config.direct_target_prefix` from the start of `utterance`
/// (case-insensitive), along with one optional trailing `:`/`,` and
/// surrounding whitespace, if present. Returns `None` when the prefix
/// isn't there or nothing follows it -- an empty remainder ("hey
/// jarvis, pilot") has nothing to hand off, so it falls through to
/// normal classification instead of asking OmaPilot to answer silence.
pub fn strip_direct_target_prefix<'a>(
    utterance: &'a str,
    config: &OmaPilotConfig,
) -> Option<&'a str> {
    let trimmed = utterance.trim_start();
    let prefix = &config.direct_target_prefix;
    if prefix.is_empty() || trimmed.len() < prefix.len() {
        return None;
    }
    let (head, rest) = trimmed.split_at(prefix.len());
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    // Require a word boundary right after the prefix -- "pilotage"
    // must not match just because it starts with "pilot".
    if rest.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix(':')
        .or_else(|| rest.strip_prefix(','))
        .unwrap_or(rest);
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OmaPilotConfig {
        OmaPilotConfig {
            fallback: true,
            direct_target: true,
            direct_target_prefix: "pilot".to_string(),
            plugin_id: "io.github.spencerbull.omapilot".to_string(),
        }
    }

    #[test]
    fn strips_prefix_with_colon() {
        assert_eq!(
            strip_direct_target_prefix("pilot: what's the capital of France", &cfg()),
            Some("what's the capital of France")
        );
    }

    #[test]
    fn strips_prefix_case_insensitively_with_comma() {
        assert_eq!(
            strip_direct_target_prefix("Pilot, tell me a joke", &cfg()),
            Some("tell me a joke")
        );
    }

    #[test]
    fn strips_bare_prefix_with_no_punctuation() {
        assert_eq!(
            strip_direct_target_prefix("pilot what's the weather", &cfg()),
            Some("what's the weather")
        );
    }

    #[test]
    fn no_match_without_prefix() {
        assert_eq!(
            strip_direct_target_prefix("what's the capital of France", &cfg()),
            None
        );
    }

    #[test]
    fn no_match_on_partial_word() {
        // "pilotage" must not match just because it starts with "pilot".
        assert_eq!(strip_direct_target_prefix("pilotage report", &cfg()), None);
    }

    #[test]
    fn prefix_alone_with_nothing_after_falls_through() {
        assert_eq!(strip_direct_target_prefix("pilot", &cfg()), None);
        assert_eq!(strip_direct_target_prefix("pilot:", &cfg()), None);
    }
}
