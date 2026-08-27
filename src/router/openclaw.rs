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
//!
//! ## `continue_in_herdr`: opening a real interactive session
//!
//! `handoff` below is a one-shot `openclaw agent --message` call --
//! fast, and (confirmed live) exempt from the device-pairing gate
//! described next. `openclaw tui`, the *interactive* terminal UI, is
//! not: it's a persistent "operator" WebSocket session, and the
//! gateway requires a human (or a scripted stand-in, see
//! `crate::config::OpenClawConfig::approve_device_command`) to approve
//! its device identity once before it'll connect -- confirmed live as
//! a known, currently-unresolved upstream limitation for
//! token-authenticated remote clients
//! (<https://github.com/openclaw/openclaw/issues/29908>), not a local
//! misconfiguration; the one documented workaround
//! (`gateway.controlUi.allowInsecureAuth`) is itself reported buggy
//! for reverse-proxied deployments like this one
//! (<https://github.com/openclaw/openclaw/issues/1679>).
//!
//! That gate is exactly why this is a separate, explicit "continue in
//! Herdr" action rather than folded into the automatic wake-word
//! handoff: a hands-free trigger that can silently need a human to
//! approve a device somewhere isn't hands-free. Once approved, though,
//! the device identity persists for future launches from the same
//! machine (confirmed live) -- it's a one-time bootstrap cost, not a
//! per-session tax, so `approve_device_command` only fires when the
//! gateway actually reports a pending request.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Recovery check, same shape as `bluebubbles::looks_like_message_command`
/// / `telegram::looks_like_telegram_command`: does `text` look like it's
/// addressing OpenClaw specifically, even through ASR noise, so
/// `pipeline.rs` can recover a misclassified MEMORY_RETURN back into
/// `Intent::External`? Observed live: voxtype transcribed "...ask
/// openclaw what..." as "Ask open. Claw. what..." -- a stray sentence
/// break inserted mid-word -- which the classifier read as MEMORY_RETURN
/// since punctuation-mangled "open. claw." doesn't visually resemble its
/// own name. Strips punctuation first so that exact case is caught, and
/// checks a leading window of words rather than requiring the trigger to
/// be literally the first word -- unlike "text"/"message", people
/// naturally lead with a verb ("ask openclaw...", "tell openclaw...",
/// "hey openclaw...") rather than putting "openclaw" itself first.
pub fn looks_like_external_command(text: &str) -> bool {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    // Anywhere in the utterance, not just the leading words -- found
    // live: "What's the status with Home Assistant? Ask OpenClaw."
    // says it as a trailing afterthought, not a leading trigger word,
    // and still unambiguously means "hand this off". A leading-only
    // window was the right shape for the original ASR-mangled-trigger-
    // word failure this was built for ("Ask open. Claw. what..."), but
    // it's too narrow for how people actually phrase these.
    if cleaned.contains("openclaw") || cleaned.contains("open claw") {
        return true;
    }

    // "open <word> claw" -- observed live: voxtype transcribed "ask
    // OpenClaw to give a status on the home assistant" as "ask open
    // cloud claw to give it a status on a status on the home
    // assistant", inserting a word between the two halves of the name.
    // A general "open X claw" window would also match "open her claw"
    // (a real false positive -- the cat opens its claw), so the
    // intervening token is only allowed when it isn't a
    // pronoun/possessive/article: "cloud" isn't one, "her" is. One
    // intervening token covers the observed case; widen if a longer
    // insertion ever shows up live.
    const FILLER_EXCLUSIONS: &[&str] = &[
        "her", "his", "its", "my", "your", "our", "their", "the", "a", "an", "this", "that",
        "these", "those",
    ];
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(2) {
        if tokens[i] == "open" && tokens[i + 2] == "claw" && !FILLER_EXCLUSIONS.contains(&tokens[i + 1])
        {
            return true;
        }
    }
    false
}

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

    // No timeout, deliberately -- a real agent turn (file edits,
    // service restarts, multi-step tool use) can legitimately run for
    // minutes, and there's no way to distinguish "still working" from
    // "hung" from out here. Found live: an earlier 60s cap (later
    // raised to 660s) killed real, still-working tasks with a false
    // "took too long" failure well before they'd actually have
    // finished successfully. `scripts/openclaw-handoff` passes its own
    // generous `--timeout` to the `openclaw` CLI itself as the actual
    // backstop against a truly wedged gateway -- this call just waits
    // for that process to exit, however long it takes.
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            tracing::warn!("[router:openclaw] wait failed: {e}");
            return (
                false,
                "The external assistant failed to respond".to_string(),
            );
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

/// How long to wait after launching/relaunching `openclaw tui` before
/// checking whether it connected -- generous enough to cover a real
/// WebSocket handshake + gateway auth round-trip, short enough that a
/// hung launch doesn't stall this indefinitely (this only ever blocks
/// a synchronous CLI/popup call, same budget class as `HANDOFF_TIMEOUT`
/// above, not the daemon's own event loop).
const CONNECT_SETTLE: Duration = Duration::from_secs(3);

/// Substring `openclaw tui` prints when the gateway needs a human (or
/// `approve_device_command`) to approve this device's pairing request
/// before it'll connect -- see this module's doc comment.
const DEVICE_APPROVAL_MARKER: &str = "Device approval needed";

/// Opens `openclaw tui` in a new Herdr tab, attached to the same
/// gateway session `handoff` uses (`agent:main:novad:CONVERSATION_ID`)
/// so it picks up right where the automatic handoff's reply left off --
/// an explicit "continue this conversation" action (see this module's
/// doc comment for why it's not part of the automatic handoff path).
/// Mirrors OmaPilot's own `continueInHerdr` in spirit: hand authority
/// to a real interactive session instead of a flash-and-gone popup
/// summary.
pub fn continue_in_herdr(cfg: Option<&crate::config::OpenClawConfig>) -> (bool, String) {
    let Some((url, token)) = gateway_credentials() else {
        return (
            false,
            "No OpenClaw gateway credentials found (checked $OPENCLAW_NOVAD_ENV or \
             ~/.config/openclaw-novad.env)"
                .to_string(),
        );
    };

    let Some(script_path) = write_launch_script(&url, &token) else {
        return (false, "Couldn't write the Herdr launch script".to_string());
    };

    let Some(pane_id) = open_herdr_tab() else {
        return (
            false,
            "Couldn't open a Herdr tab -- is herdr running?".to_string(),
        );
    };

    run_in_pane(&pane_id, &script_path);
    std::thread::sleep(CONNECT_SETTLE);

    if pane_shows(&pane_id, DEVICE_APPROVAL_MARKER) {
        match cfg.and_then(|c| c.approve_device_command.as_deref()) {
            Some(approve_cmd) => {
                tracing::info!(
                    "[router:openclaw] device approval pending -- running configured \
                     approve_device_command"
                );
                match Command::new("sh").arg("-c").arg(approve_cmd).status() {
                    Ok(status) if status.success() => {
                        // Relaunch to actually connect with the
                        // now-approved identity -- the pending tui
                        // process left disconnected by the pairing
                        // gate doesn't retry on its own.
                        std::thread::sleep(Duration::from_secs(1));
                        run_in_pane(&pane_id, &script_path);
                        std::thread::sleep(CONNECT_SETTLE);
                    }
                    Ok(status) => {
                        tracing::warn!("[router:openclaw] approve_device_command exited {status}");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[router:openclaw] failed to run approve_device_command: {e}"
                        );
                    }
                }
            }
            None => {
                tracing::info!(
                    "[router:openclaw] device approval pending, no approve_device_command \
                     configured -- left in Herdr for the user to approve"
                );
            }
        }
    }

    (true, "Opened in Herdr".to_string())
}

/// Reads `OPENCLAW_GATEWAY_URL`/`OPENCLAW_GATEWAY_TOKEN` from the same
/// env file `openclaw-handoff` sources (`$OPENCLAW_NOVAD_ENV`, default
/// `~/.config/openclaw-novad.env`) -- not duplicated into
/// `config.toml`, since that would just be a second place for the
/// same credential to drift out of sync.
fn gateway_credentials() -> Option<(String, String)> {
    let path = std::env::var_os("OPENCLAW_NOVAD_ENV")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".config/openclaw-novad.env")
        });
    let content = std::fs::read_to_string(&path)
        .inspect_err(|e| tracing::warn!("[router:openclaw] reading {path:?}: {e}"))
        .ok()?;

    let mut url = None;
    let mut token = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("OPENCLAW_GATEWAY_URL=") {
            url = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("OPENCLAW_GATEWAY_TOKEN=") {
            token = Some(v.trim_matches('"').to_string());
        }
    }
    Some((url?, token?))
}

/// Writes a small self-contained launch script (mode 700 -- it embeds
/// the gateway token, same sensitivity as `openclaw-novad.env` itself)
/// under `$XDG_RUNTIME_DIR/omarchy-novad/`, same convention
/// `main.rs::transcript_path` already uses. A real file rather than an
/// inline command string: `herdr pane run` re-lexes its trailing
/// arguments at the target shell, which mangles quoting for anything
/// containing its own `--flag value` pairs (found live).
fn write_launch_script(url: &str, token: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("openclaw-herdr.sh");

    let script = format!(
        "#!/usr/bin/env bash\nexec openclaw tui --session agent:main:novad:{CONVERSATION_ID} \
         --url {url:?} --token {token:?}\n"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    file.write_all(script.as_bytes()).ok()?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }

    Some(path)
}

/// Creates a new Herdr tab and returns its root pane id, or `None` if
/// `herdr` isn't running/reachable.
fn open_herdr_tab() -> Option<String> {
    let output = Command::new("herdr")
        .args(["tab", "create", "--label", "OpenClaw", "--focus"])
        .output()
        .inspect_err(|e| tracing::warn!("[router:openclaw] failed to spawn herdr: {e}"))
        .ok()?;
    if !output.status.success() {
        tracing::warn!(
            "[router:openclaw] herdr tab create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
}

/// Runs `script_path` in `pane_id` -- fire-and-forget, same as a user
/// typing the command and pressing Enter.
fn run_in_pane(pane_id: &str, script_path: &std::path::Path) {
    let status = Command::new("herdr")
        .args(["pane", "run", pane_id])
        .arg(script_path)
        .status();
    if let Err(e) = status {
        tracing::warn!("[router:openclaw] herdr pane run failed: {e}");
    }
}

/// Whether `pane_id`'s current terminal content contains `needle` --
/// used to check for the device-approval prompt after a launch.
fn pane_shows(pane_id: &str, needle: &str) -> bool {
    match Command::new("herdr")
        .args(["pane", "read", pane_id])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(needle),
        Err(e) => {
            tracing::warn!("[router:openclaw] herdr pane read failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_external_command;

    #[test]
    fn catches_the_actual_live_mishearing() {
        // Observed live: voxtype transcribed this exact utterance, and
        // the classifier read it as MEMORY_RETURN.
        assert!(looks_like_external_command(
            "Ask open. Claw. what photo is on the photo frame right now When and where was it \
             taken and who is in it?"
        ));
    }

    #[test]
    fn catches_common_phrasings() {
        assert!(looks_like_external_command("openclaw what's the capital of France"));
        assert!(looks_like_external_command("ask openclaw to write a python script"));
        assert!(looks_like_external_command("hey openclaw, check the cluster"));
        assert!(looks_like_external_command("tell open claw to check the logs"));
    }

    #[test]
    fn does_not_fire_on_unrelated_text() {
        assert!(!looks_like_external_command("turn on the living room lights"));
        assert!(!looks_like_external_command("text mom I'm running late"));
        assert!(!looks_like_external_command(
            "what's the weather like today"
        ));
    }

    #[test]
    fn catches_a_trailing_mention_too() {
        // Observed live, reproduced three times in a row: saying
        // "openclaw" as a trailing afterthought rather than a leading
        // trigger word still unambiguously means "hand this off" --
        // and the classifier read every one of these as HOME_ASSISTANT
        // (not MEMORY_RETURN), since the utterance also names a topic
        // that intent handles.
        assert!(looks_like_external_command(
            "What's the status with Home Assistant? Ask OpenClaw."
        ));
        assert!(looks_like_external_command(
            "Ask OpenClaw what's the status with Home Assistant."
        ));
    }

    #[test]
    fn catches_an_inserted_word_between_open_and_claw() {
        // Observed live: voxtype transcribed "ask OpenClaw to give a
        // status on the home assistant" as "ask open cloud claw to give
        // it a status on a status on the home assistant" -- an inserted
        // word between the two halves of the name. The classifier read
        // it as HOME_ASSISTANT (the utterance also names that topic) and
        // the pre-fix matcher missed it entirely, so the request was
        // handled as a device command instead of handed off.
        assert!(looks_like_external_command(
            "ask open cloud claw to give it a status on a status on the home assistant"
        ));
    }

    #[test]
    fn does_not_fire_on_words_that_only_coincidentally_appear_in_sequence() {
        // "open" and "claw" appear back to back in spirit but not as
        // the literal substring "open claw" -- "her" splits them --
        // shouldn't false-positive as an OpenClaw command.
        assert!(!looks_like_external_command(
            "remind me to buy a new cat scratching post because the cat likes to open her claw \
             on the couch"
        ));
    }
}
