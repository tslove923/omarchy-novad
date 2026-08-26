//! The standalone wake-word -> dictation -> classify -> route -> popup
//! flow. Port of nova-npu's `tray/tray_app.py` `CoreProcessThread`
//! (record -> transcribe -> route), scoped to what `router` (see
//! router/mod.rs) actually implements.
//!
//! One session per wake-word detection, run synchronously on the
//! detect loop's own thread (see `main.rs::run_detect`) -- unlike
//! nova's Python, which ran this on a background thread so the Qt/
//! Electron UI thread stayed responsive, omarchy-novad's popup is a separate
//! process (Quickshell) driven entirely by the `PopupState` file, so
//! there's no UI thread to keep unblocked here. Wake-word detection
//! itself is naturally paused for the session's duration since the
//! caller doesn't feed it more audio until this returns.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::classify::{Classifier, Intent};
use crate::config::{BlueBubblesConfig, HomeAssistantConfig};
use crate::popup::{self, PopupAction, PopupPhase, PopupState};
use crate::router::{self, RouteResult};

/// How often to poll voxtype's state file while waiting for it to
/// finish recording/transcribing. voxtype's own silence-timeout
/// (`external_trigger_silence_timeout_secs` in its config.toml)
/// decides when a recording actually ends -- this loop just watches
/// for that, it doesn't do its own silence detection like the
/// earlier RMS-gate approach in VoxtypeDictation's caller did before
/// this module existed.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Hard ceiling on how long one session (record + transcribe) is
/// allowed to run before giving up -- a safety net if voxtype's own
/// max_duration_secs cap and silence-timeout somehow both fail to
/// return it to idle. Generous: transcription of a long recording on
/// a loaded NPU can legitimately take several seconds.
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);

pub struct PipelineConfig {
    pub classify_base_url: String,
    pub classify_model_id: String,
    pub voxtype_binary: String,
    pub transcript_path: PathBuf,
    pub voxtype_state_path: PathBuf,
    /// `None` when `[home_assistant]` isn't configured (see
    /// config.rs) -- `Intent::HomeAssistant` falls back to
    /// `RouteResult::Unhandled` in that case, same as before this
    /// intent had a handler at all.
    pub home_assistant: Option<HomeAssistantConfig>,
    /// `None` when `[bluebubbles]` isn't configured -- `Intent::Message`
    /// falls back to `RouteResult::Unhandled` in that case, same shape
    /// as `home_assistant` above.
    pub bluebubbles: Option<BlueBubblesConfig>,
}

/// Run one full session after a wake-word detection: start voxtype
/// recording to a known file, wait for it to finish (record + auto
/// transcribe, driven by voxtype's own silence-timeout), classify the
/// result, route it, and drive the popup through the whole thing.
///
/// Every early-return path leaves the popup back at `Idle` before
/// returning, so the caller never has to clean up popup state itself.
pub fn run_session(cfg: &PipelineConfig) {
    popup::write_state(&PopupState {
        phase: PopupPhase::Recording,
        text: String::new(),
        confirm_label: None,
        editable: false,
    });

    if let Err(e) = start_recording(cfg) {
        tracing::error!("[pipeline] failed to start recording: {e}");
        popup::write_state(&PopupState::default());
        return;
    }

    if !wait_for_recording_to_start(cfg) {
        tracing::warn!("[pipeline] voxtype never left idle after record start -- giving up");
        popup::write_state(&PopupState::default());
        return;
    }

    if !wait_for_idle(cfg) {
        tracing::warn!("[pipeline] session timed out waiting for voxtype to return to idle");
        popup::write_state(&PopupState::default());
        return;
    }

    let transcript = match std::fs::read_to_string(&cfg.transcript_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            tracing::error!(
                "[pipeline] failed to read transcript at {:?}: {e}",
                cfg.transcript_path
            );
            popup::write_state(&PopupState::default());
            return;
        }
    };

    if transcript.is_empty() {
        tracing::debug!("[pipeline] empty transcript, nothing to do");
        popup::write_state(&PopupState::default());
        return;
    }

    popup::write_state(&PopupState {
        phase: PopupPhase::Classifying,
        text: transcript.clone(),
        confirm_label: None,
        editable: false,
    });

    let classifier = Classifier::new(cfg.classify_base_url.clone(), cfg.classify_model_id.clone());
    let result = match classifier.classify(&transcript) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "[pipeline] classification failed: {e} -- showing raw transcript instead"
            );
            show_ready_and_wait(&transcript);
            return;
        }
    };

    tracing::info!(
        "[pipeline] intent={} argument={:?} ({:.2}s)",
        result.intent,
        result.argument,
        result.latency.as_secs_f32()
    );

    if router::is_external_handoff(result.intent) {
        // Full transcript, not result.argument -- see
        // router::handoff_to_openclaw's docs for why a
        // coding/reasoning handoff needs the whole utterance rather
        // than the classifier's (often keyword-stripped) extraction.
        popup::write_state(&PopupState {
            phase: PopupPhase::HandingOff,
            text: transcript.clone(),
            confirm_label: None,
            editable: false,
        });
        let (success, message) = router::handoff_to_openclaw(&transcript);
        tracing::info!("[pipeline] handed off to openclaw: success={success} message={message:?}");
        show_ready_and_wait(&message);
        return;
    }

    // Recovery: MEMORY_RETURN has no local handler (see router::route's
    // doc comment), so it's a safe bucket to double-check rather than a
    // risk of overriding a confident, actionable classification. If the
    // *original* transcript's leading word looks like a MESSAGE trigger
    // verb -- including a plausible ASR mis-hearing, e.g. "tax" for
    // "text" (observed live: "text Jessica is this working?" came back
    // transcribed as "Tax is this working.", which the classifier had no
    // reason to read as anything but MEMORY_RETURN) -- route the full
    // transcript as a Message instead of the classifier's own argument
    // extraction, which had already lost whatever didn't survive
    // transcription. See router::looks_like_message_command's docs.
    let (route_intent, route_argument) = if result.intent == Intent::MemoryReturn
        && cfg.bluebubbles.is_some()
        && router::looks_like_message_command(&transcript)
    {
        tracing::info!(
            "[pipeline] recovering misclassified MEMORY_RETURN as MESSAGE (leading word looks \
             like a mis-heard message trigger): {transcript:?}"
        );
        (Intent::Message, transcript.clone())
    } else {
        (result.intent, result.argument.clone())
    };

    match router::route(
        route_intent,
        &route_argument,
        cfg.home_assistant.as_ref(),
        cfg.bluebubbles.as_ref(),
    ) {
        RouteResult::Done { success, message } => {
            tracing::info!("[pipeline] routed: success={success} message={message:?}");
            show_ready_and_wait(&message);
        }
        RouteResult::NeedsConfirmation {
            label,
            body,
            editable,
            kind,
        } => {
            tracing::info!(
                "[pipeline] awaiting confirmation ({kind:?}, editable={editable}): \
                 label={label:?} body={body:?}"
            );
            popup::write_state(&PopupState {
                phase: PopupPhase::Confirming,
                text: body,
                confirm_label: label,
                editable,
            });
            match wait_for_action() {
                Some(PopupAction::Approve { edited_text }) => {
                    let (ok, message) = router::run_confirmed(
                        kind,
                        &route_argument,
                        edited_text.as_deref(),
                        cfg.bluebubbles.as_ref(),
                    );
                    tracing::info!("[pipeline] confirmed and ran: success={ok} message={message:?}");
                    show_ready_and_wait(&message);
                }
                Some(other) => {
                    tracing::info!("[pipeline] confirmation denied ({other:?}) -- not running");
                    popup::write_state(&PopupState::default());
                }
                None => {
                    tracing::warn!(
                        "[pipeline] no response to confirmation within {}s (or control socket \
                         failed to spawn) -- giving up silently, popup back to idle",
                        30
                    );
                    popup::write_state(&PopupState::default());
                }
            }
        }
        RouteResult::Unhandled => {
            // No local handler for this intent -- fall back to
            // treating it as plain dictation text the user can review
            // and insert, same as nova's edit-window path for
            // anything the router couldn't act on.
            show_ready_and_wait(&transcript);
        }
    }
}

fn start_recording(cfg: &PipelineConfig) -> std::io::Result<()> {
    // Best-effort: stale content from a previous session shouldn't be
    // mistaken for this one's transcript if voxtype fails to write a
    // fresh one for some reason.
    let _ = std::fs::remove_file(&cfg.transcript_path);

    let file_arg = format!("--file={}", cfg.transcript_path.display());
    let status = std::process::Command::new(&cfg.voxtype_binary)
        .args(["record", "start", &file_arg])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "voxtype record start exited with {status}"
        )));
    }
    Ok(())
}

/// Waits for voxtype's state file to read something other than
/// "idle" -- confirms the daemon actually processed the `record
/// start` signal and began recording, rather than racing ahead on the
/// stale "idle" the file still holds from *before* this session
/// started. Without this, `wait_for_idle` below could observe that
/// same pre-existing "idle" on its very first poll and return
/// immediately, as if the (not-yet-started) session had already
/// finished -- exactly what happened in the first live test: the
/// pipeline read a nonexistent transcript file a few milliseconds
/// after telling voxtype to start.
fn wait_for_recording_to_start(cfg: &PipelineConfig) -> bool {
    let start = Instant::now();
    loop {
        if start.elapsed() >= SESSION_TIMEOUT {
            return false;
        }
        match std::fs::read_to_string(&cfg.voxtype_state_path) {
            Ok(s) if s.trim() != "idle" => return true,
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Polls voxtype's state file until it reads "idle" again (recording
/// and any transcription finished) or `SESSION_TIMEOUT` elapses.
/// Returns false on timeout. Only meaningful after
/// `wait_for_recording_to_start` has confirmed the session is
/// actually underway -- see that function's docs for why calling this
/// alone right after `record start` is a race.
fn wait_for_idle(cfg: &PipelineConfig) -> bool {
    let start = Instant::now();
    loop {
        if start.elapsed() >= SESSION_TIMEOUT {
            return false;
        }
        match std::fs::read_to_string(&cfg.voxtype_state_path) {
            Ok(s) if s.trim() == "idle" => return true,
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

fn show_ready_and_wait(text: &str) {
    popup::write_state(&PopupState {
        phase: PopupPhase::Ready,
        text: text.to_string(),
        confirm_label: None,
        editable: false,
    });
    // Ready is dismiss-on-timeout, not dismiss-on-action -- nova's own
    // popup auto-hid the result after a few seconds rather than
    // waiting on a button (Insert/Cancel are there for the user to
    // act sooner if they want, see NovadPopup.qml's review bar).
    std::thread::sleep(Duration::from_secs(4));
    popup::write_state(&PopupState::default());
}

/// Blocks for one `PopupAction` from the popup's control socket, or
/// gives up after a while so an unattended confirmation prompt
/// doesn't hang the whole detect loop forever.
fn wait_for_action() -> Option<PopupAction> {
    let rx = popup::ControlServer::spawn().ok()?;
    rx.recv_timeout(Duration::from_secs(30)).ok()
}
