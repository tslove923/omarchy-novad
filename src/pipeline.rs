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
use crate::config::{BlueBubblesConfig, HomeAssistantConfig, OmaPilotConfig, TelegramConfig};
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
    /// `None` when `[telegram]` isn't configured -- `Intent::Telegram`
    /// falls back to `RouteResult::Unhandled` in that case, same shape
    /// as `bluebubbles` above.
    pub telegram: Option<TelegramConfig>,
    /// `None` (or present with both `fallback`/`direct_target` false)
    /// disables OmaPilot entirely: `direct_target`'s prefix is never
    /// checked, and `Intent::External`/`Coding`'s handoff only ever
    /// tries OpenClaw. See `router::omapilot`.
    pub omapilot: Option<OmaPilotConfig>,
    /// Voice/model for `crate::converse`'s TTS playback -- see that
    /// module's docs. Used whenever `Intent::External`/`Coding`
    /// hands off to OpenClaw: see this file's `is_external_handoff`
    /// branch, which now enters the full conversation loop rather
    /// than a one-shot handoff.
    pub tts: crate::config::TtsConfig,
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

    let transcript = match listen_and_transcribe(
        &cfg.voxtype_binary,
        &cfg.transcript_path,
        &cfg.voxtype_state_path,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("[pipeline] {e}");
            popup::write_state(&PopupState::default());
            return;
        }
    };

    if transcript.is_empty() {
        tracing::debug!("[pipeline] empty transcript, nothing to do");
        popup::write_state(&PopupState::default());
        return;
    }

    // Direct target: "hey jarvis, pilot: ..." bypasses classification
    // (and `route`) entirely -- checked before anything else touches
    // `transcript`, same principle as `is_external_handoff` bypassing
    // `route`'s argument-only signature: the full remainder after the
    // prefix goes to OmaPilot verbatim, not through the classifier's
    // keyword extraction.
    if let Some(cfg) = cfg.omapilot.as_ref().filter(|c| c.direct_target) {
        if let Some(remainder) = router::strip_direct_target_prefix(&transcript, cfg) {
            popup::write_state(&PopupState {
                phase: PopupPhase::HandingOff,
                text: remainder.to_string(),
                confirm_label: None,
                editable: false,
            });
            let (success, message) = router::ask_omapilot(remainder, cfg);
            tracing::info!(
                "[pipeline] direct-target handoff to omapilot: success={success} message={message:?}"
            );
            show_ready_and_wait(&message);
            return;
        }
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

    // Recovery, two tiers:
    //
    // 1. An explicit "openclaw"/"open claw" mention anywhere in the
    //    transcript overrides *any* classifier guess (checked first,
    //    below) -- found live that the classifier isn't just prone to
    //    MEMORY_RETURN on these, it can land on almost any intent that
    //    matches a topic word also present in the utterance (e.g. "Ask
    //    OpenClaw what's the status with Home Assistant" -> HOME_ASSISTANT,
    //    reproduced three times in a row). A literal "ask openclaw" is
    //    about as unambiguous as this daemon's input ever gets.
    //
    // 2. MEMORY_RETURN has no local handler (see router::route's doc
    //    comment) and only reaches this without the transcript
    //    mentioning OpenClaw by name, so it's a safe bucket to
    //    double-check rather than a risk of overriding a confident,
    //    actionable classification: if the *original* transcript's
    //    leading word(s) look like a MESSAGE or TELEGRAM trigger --
    //    including a plausible ASR mis-hearing, e.g. "tax" for "text"
    //    (observed live: "text Jessica is this working?" came back
    //    transcribed as "Tax is this working.", which the classifier
    //    had no reason to read as anything but MEMORY_RETURN) -- route
    //    the full transcript instead of the classifier's own argument
    //    extraction, which had already lost whatever didn't survive
    //    transcription. Plus MediaControl/HomeAssistant (reusing the
    //    exact checkers route()'s own SYSTEM_CONTROL arm already uses
    //    one level down for the same confusion -- see
    //    router::looks_like_media_command / looks_like_home_assistant_command).
    //    Checked in this order only because Message was the one
    //    actually observed misfiring live first; the trigger word sets
    //    don't overlap so the order otherwise doesn't matter.
    //    Deliberately does NOT attempt every intent -- OpenApp/
    //    WebSearch/OpenWebsite/Terminal have no equivalent checker
    //    because none has an observed live misclassification to model
    //    one on; every checker in this codebase is built from a
    //    specific real failure, not a guess at a hypothetical one (see
    //    e.g. the Levenshtein comment in bluebubbles.rs), since an
    //    unvalidated heuristic risks recovering an already-correct
    //    MEMORY_RETURN into the wrong intent instead.
    //
    // This has to happen before `is_external_handoff` below, not after
    // -- Coding/External never go through `route()` at all, so a
    // recovery into `Intent::External` needs to reach that check, not
    // this one's `route()` call.
    let (route_intent, route_argument) = if !router::is_external_handoff(result.intent)
        && router::looks_like_external_command(&transcript)
    {
        // Explicit "openclaw"/"open claw" anywhere in the transcript
        // overrides *any* classifier guess, not just MEMORY_RETURN --
        // found live, three times in a row: "Ask OpenClaw what's the
        // status with Home Assistant" and its variants all came back
        // HOME_ASSISTANT, not EXTERNAL, because the utterance also
        // names a topic that intent handles. The classifier can
        // apparently pick almost any intent this way when the
        // utterance mentions a matching topic word; a literal "ask
        // openclaw" is about as unambiguous a signal as this daemon
        // ever gets, so it wins over whatever else got guessed. This
        // subsumes (and runs before) the MEMORY_RETURN-specific
        // OpenClaw check that used to be the only place this fired.
        tracing::info!(
            "[pipeline] overriding classifier's {:?} with EXTERNAL (transcript explicitly \
             mentions openclaw): {transcript:?}",
            result.intent
        );
        (Intent::External, transcript.clone())
    } else if result.intent == Intent::MemoryReturn
        && cfg.bluebubbles.is_some()
        && router::looks_like_message_command(&transcript)
    {
        tracing::info!(
            "[pipeline] recovering misclassified MEMORY_RETURN as MESSAGE (leading word looks \
             like a mis-heard message trigger): {transcript:?}"
        );
        (Intent::Message, transcript.clone())
    } else if result.intent == Intent::MemoryReturn
        && cfg.telegram.is_some()
        && router::looks_like_telegram_command(&transcript)
    {
        tracing::info!(
            "[pipeline] recovering misclassified MEMORY_RETURN as TELEGRAM (leading word looks \
             like a mis-heard telegram trigger): {transcript:?}"
        );
        (Intent::Telegram, transcript.clone())
    } else if result.intent == Intent::MemoryReturn && router::looks_like_media_command(&transcript) {
        // Reuses the same checker route()'s own SYSTEM_CONTROL arm
        // already uses one level down -- extended here so it also
        // catches the case where the classifier missed it entirely
        // (MEMORY_RETURN) rather than only the SYSTEM_CONTROL/MEDIA_CONTROL
        // mix-up that arm was built for.
        tracing::info!(
            "[pipeline] recovering misclassified MEMORY_RETURN as MEDIA_CONTROL (leading word \
             looks like a media transport command): {transcript:?}"
        );
        (Intent::MediaControl, transcript.clone())
    } else if result.intent == Intent::MemoryReturn
        && cfg.home_assistant.is_some()
        && router::looks_like_home_assistant_command(&transcript)
    {
        // Same reuse, Home Assistant side -- guarded on `home_assistant`
        // being configured for the same reason route()'s own check is:
        // don't claim a false "Home Assistant not configured" for a
        // phrase that only coincidentally shares a verb.
        tracing::info!(
            "[pipeline] recovering misclassified MEMORY_RETURN as HOME_ASSISTANT (leading word \
             looks like a device-control command): {transcript:?}"
        );
        (Intent::HomeAssistant, transcript.clone())
    } else if result.intent == Intent::MemoryReturn {
        // MEMORY_RETURN has no local handler -- there's no recall/notes
        // feature backing it, so it always fell through to
        // RouteResult::Unhandled's silent 4s transcript flash regardless
        // of what recovery checks above matched. Rather than a dead-end
        // intent, treat every MEMORY_RETURN as a request for OpenClaw:
        // it's a strictly better fallback than a no-op flash, and the
        // recovery checks above already skim off the cases that clearly
        // belong to a *different* local handler first.
        (Intent::External, transcript.clone())
    } else {
        (result.intent, result.argument.clone())
    };

    if router::is_external_handoff(route_intent) {
        // Full transcript, not route_argument -- see
        // router::handoff_to_openclaw's docs for why a
        // coding/reasoning handoff needs the whole utterance rather
        // than the classifier's (often keyword-stripped) extraction.
        //
        // Enters the full conversation loop (crate::converse) rather
        // than a one-shot handoff-and-show -- the wake word now starts
        // a back-and-forth with OpenClaw instead of a single exchange;
        // subsequent turns keep listening without needing the wake
        // word again, until a stop phrase or `converse stop`. This
        // path is deliberately OpenClaw-only, no `[omapilot] fallback`
        // -- OmaPilot's `askText` handoff never returns a real reply
        // (see router::omapilot's docs), so it has no way to power a
        // conversation loop even in principle; OmaPilot is still
        // reachable via its own `direct_target`/`direct_target_prefix`
        // trigger (checked above, before classification), unaffected
        // by this.
        tracing::info!("[pipeline] external/coding request -- entering the conversation loop");
        popup::write_state(&PopupState::default());
        let converse_cfg = crate::converse::ConverseConfig {
            voxtype_binary: cfg.voxtype_binary.clone(),
            transcript_path: cfg.transcript_path.clone(),
            voxtype_state_path: cfg.voxtype_state_path.clone(),
            classify_base_url: cfg.classify_base_url.clone(),
            classify_model_id: cfg.classify_model_id.clone(),
            tts: cfg.tts.clone(),
        };
        if let Err(e) = crate::converse::run(&converse_cfg, Some(transcript.clone())) {
            tracing::error!("[pipeline] conversation loop ended with an error: {e}");
        }
        return;
    }

    match router::route(
        route_intent,
        &route_argument,
        cfg.home_assistant.as_ref(),
        cfg.bluebubbles.as_ref(),
        cfg.telegram.as_ref(),
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
                        cfg.telegram.as_ref(),
                    );
                    tracing::info!(
                        "[pipeline] confirmed and ran: success={ok} message={message:?}"
                    );
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

/// Runs one voxtype record+transcribe round-trip and returns the
/// resulting transcript, trimmed (empty means nothing was said --
/// callers decide whether that's "try again" or "give up," same as
/// `run_session` treating it as "nothing to do"). Factored out of
/// `run_session` so a caller that needs more than one turn (see
/// `crate::converse`'s multi-turn loop) can call this again for each
/// follow-up reply without going through classify/route at all.
pub fn listen_and_transcribe(
    voxtype_binary: &str,
    transcript_path: &std::path::Path,
    voxtype_state_path: &std::path::Path,
) -> anyhow::Result<String> {
    start_recording(voxtype_binary, transcript_path)
        .map_err(|e| anyhow::anyhow!("failed to start recording: {e}"))?;

    if !wait_for_recording_to_start(voxtype_state_path) {
        anyhow::bail!("voxtype never left idle after record start -- giving up");
    }
    if !wait_for_idle(voxtype_state_path) {
        anyhow::bail!("session timed out waiting for voxtype to return to idle");
    }

    let transcript = std::fs::read_to_string(transcript_path)
        .map_err(|e| anyhow::anyhow!("failed to read transcript at {transcript_path:?}: {e}"))?;
    Ok(transcript.trim().to_string())
}

fn start_recording(
    voxtype_binary: &str,
    transcript_path: &std::path::Path,
) -> std::io::Result<()> {
    // Best-effort: stale content from a previous session shouldn't be
    // mistaken for this one's transcript if voxtype fails to write a
    // fresh one for some reason.
    let _ = std::fs::remove_file(transcript_path);

    let file_arg = format!("--file={}", transcript_path.display());
    let status = std::process::Command::new(voxtype_binary)
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
fn wait_for_recording_to_start(voxtype_state_path: &std::path::Path) -> bool {
    let start = Instant::now();
    loop {
        if start.elapsed() >= SESSION_TIMEOUT {
            return false;
        }
        match std::fs::read_to_string(voxtype_state_path) {
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
fn wait_for_idle(voxtype_state_path: &std::path::Path) -> bool {
    let start = Instant::now();
    loop {
        if start.elapsed() >= SESSION_TIMEOUT {
            return false;
        }
        match std::fs::read_to_string(voxtype_state_path) {
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
