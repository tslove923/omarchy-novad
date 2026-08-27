//! The OpenClaw voice-conversation loop -- `omarchy-novad converse
//! start` (see main.rs). Unlike pipeline.rs's one-shot wake-word
//! session, this runs turn after turn until stopped, but -- unlike an
//! earlier version of this module -- never activates voxtype on its
//! own between turns. Each turn is: the user explicitly triggers a
//! recording (`converse listen`, e.g. a "Record" button, or the first
//! turn's utterance arriving already-captured from a wake word),
//! voxtype records until it hits its own silence-timeout or the user
//! manually ends it early (`converse stop-listening`, a "toggle"
//! button while recording), the transcript appears in an editable box
//! with no timeout and no spoken confirmation prompt, the user edits
//! it if they want and presses Enter (or clicks Confirm) to send it or
//! Reject to discard it, OpenClaw replies, the reply is shown and
//! spoken -- then the loop goes back to waiting for the user to
//! explicitly trigger the next recording. Found live: a fully
//! automatic listen-confirm-listen loop is more disruptive than
//! helpful -- voxtype firing up on its own after every reply, and a
//! spoken "does this look good, yes or no" round-trip on every turn,
//! wasn't actually what anyone wanted day to day. This module used to
//! have both (see git history); removed in favor of the user staying
//! in control of when the mic is ever listening.
//!
//! Reuses existing pieces rather than inventing new ones:
//! `router::handoff_external` (the same call the automatic wake-word
//! handoff makes, `omapilot: None` so it never falls back to OmaPilot
//! -- this is explicitly an OpenClaw conversation) keeps every turn on
//! the same `CONVERSATION_ID` OpenClaw session (see
//! `router::openclaw`'s module docs), so context carries across turns
//! the same way it already does for the wake-word path.
//! `pipeline::listen_and_transcribe` is the exact record+transcribe
//! round-trip `pipeline::run_session` uses for its own single turn,
//! just callable again per turn here, wrapped in `listen_interruptibly`
//! below so a `converse stop-listening` can end it early without
//! needing a change to that shared function (used by the plain
//! wake-word pipeline too, which has no such button).

use std::sync::mpsc;
use std::time::Duration;

use crate::config::TtsConfig;
use crate::conversation::{
    self, ConversationAction, ConversationPhase, ConversationState, ConversationTurn,
};
use crate::{pipeline, router, tts};

/// Appended to the confirmed utterance before handing off to OpenClaw
/// -- asks for a machine-parseable "TL;DR:" line up front so the
/// spoken summary comes from OpenClaw itself (full context, its own
/// words) rather than a second, context-blind model guessing what
/// mattered after the fact. `split_tldr` below extracts it;
/// `summarize_for_speech` (the local-LLM condensing call) is only a
/// fallback for when OpenClaw doesn't comply with the format -- an
/// instruction, not a hard constraint on the model.
const TLDR_INSTRUCTION: &str = "\n\n(Structure your reply in two parts. First, one line starting \
with exactly \"TL;DR:\" -- a very direct, spoken-friendly summary: the key info, your \
recommendation, and, only if you genuinely need something from me to continue, a specific \
follow-up question. Then, after a blank line, your normal full detailed response.)";

pub struct ConverseConfig {
    pub voxtype_binary: String,
    pub transcript_path: std::path::PathBuf,
    pub voxtype_state_path: std::path::PathBuf,
    /// Reuses the already-running LLM `serve` instance (see
    /// `classify::Classifier`'s identical `base_url`/`model_id` shape)
    /// as a fallback summarizer -- see `TLDR_INSTRUCTION`.
    pub classify_base_url: String,
    pub classify_model_id: String,
    pub tts: TtsConfig,
}

/// Runs the conversation loop until stopped. `initial_utterance`, when
/// given, skips straight to the review/edit step with it already
/// filled in -- e.g. when this is launched from a wake-word trigger
/// that already captured one (see `Command::Detect`'s `on_detect`
/// custom-command escape hatch in main.rs, which can point at
/// `omarchy-novad converse start` directly).
pub fn run(cfg: &ConverseConfig, initial_utterance: Option<String>) -> anyhow::Result<()> {
    let control_rx = conversation::ControlServer::spawn()
        .inspect_err(|e| tracing::warn!("[converse] control socket failed to start: {e}"))
        .ok();
    if control_rx.is_none() {
        // Nothing left to drive this loop at all -- there's no more
        // voice-only fallback path (see this module's doc comment),
        // every step now waits on a control action. Fail fast instead
        // of writing active:true and then hanging forever with no way
        // for anything to ever progress it.
        anyhow::bail!("conversation control socket failed to start -- can't run without it");
    }

    // Seeded fully correct up front when an utterance is already in
    // hand (the real wake-word path: pipeline.rs always calls this
    // with `Some(transcript)`) instead of writing `{active:true,
    // phase:null}` and then, microseconds later with no delay in
    // between, overwriting it with `{phase:confirming, pending_text:
    // ...}` -- found live, the QML FileView watching this file doesn't
    // necessarily catch up between two such rapid writes.
    let mut state = match &initial_utterance {
        Some(u) => ConversationState {
            active: true,
            phase: Some(ConversationPhase::Confirming),
            pending_text: Some(u.clone()),
            turns: Vec::new(),
            thinking_elapsed_secs: None,
        },
        None => ConversationState {
            active: true,
            phase: None,
            pending_text: None,
            turns: Vec::new(),
            thinking_elapsed_secs: None,
        },
    };
    conversation::write_state(&state);

    let mut pending_utterance = initial_utterance;

    'session: loop {
        // Get this turn's utterance: either already in hand (the first
        // turn, from a wake word), or wait -- with no timeout, no
        // auto-relisten -- for the user to explicitly trigger a new
        // recording. `phase: None` here doubles as "idle, waiting for
        // you to press Record" (same convention `run` used for "before
        // the very first listen" before this rewrite).
        let utterance = match pending_utterance.take() {
            Some(u) => u,
            None => {
                state.phase = None;
                state.pending_text = None;
                conversation::write_state(&state);

                match wait_for_listen_or_stop(&control_rx) {
                    WaitOutcome::Stop => break 'session,
                    WaitOutcome::Proceed => {}
                }

                state.phase = Some(ConversationPhase::Listening);
                conversation::write_state(&state);

                match listen_interruptibly(cfg, &control_rx) {
                    ListenOutcome::Stop => break 'session,
                    ListenOutcome::Text(t) if t.is_empty() => continue 'session, // nothing said
                    ListenOutcome::Text(t) => t,
                    ListenOutcome::Err(e) => {
                        tracing::warn!("[converse] listen failed: {e}");
                        std::thread::sleep(Duration::from_secs(1)); // avoid hammering a persistently-broken voxtype
                        continue 'session;
                    }
                }
            }
        };

        state.phase = Some(ConversationPhase::Confirming);
        state.pending_text = Some(utterance);
        conversation::write_state(&state);

        // Waits indefinitely -- no grace window, no spoken prompt, no
        // voice fallback. The user reviews (and can edit) the
        // transcript in the panel's text box for as long as they want,
        // then Enter/Confirm sends it, Reject discards it, Stop ends
        // the whole conversation.
        let confirmed = match wait_for_review(&control_rx, state.pending_text.as_deref().unwrap_or_default()) {
            ReviewOutcome::Stop => break 'session,
            ReviewOutcome::Discard => {
                state.pending_text = None;
                conversation::write_state(&state);
                continue 'session;
            }
            ReviewOutcome::Send(text) => text,
        };
        state.pending_text = None;

        state.phase = Some(ConversationPhase::Thinking);
        state.thinking_elapsed_secs = Some(0);
        conversation::write_state(&state);

        let handoff_text = format!("{confirmed}{TLDR_INSTRUCTION}");
        let (ok, full_response) = run_handoff_with_progress(&handoff_text, &mut state);
        state.thinking_elapsed_secs = None;
        if !ok {
            tracing::warn!("[converse] openclaw handoff failed: {full_response}");
        }

        let (tldr, display_response) = split_tldr(&full_response);
        let spoken_summary = tldr.or_else(|| summarize_for_speech(&full_response, cfg));

        state.turns.push(ConversationTurn {
            user_text: confirmed,
            full_response: display_response,
            spoken_summary: spoken_summary.clone(),
        });
        conversation::write_state(&state);

        let to_speak = spoken_summary.unwrap_or(full_response);
        state.phase = Some(ConversationPhase::Speaking);
        conversation::write_state(&state);
        if let Err(e) = tts::speak(&to_speak, &cfg.tts) {
            tracing::warn!("[converse] tts failed: {e}");
        }
        // Back to the top -- pending_utterance is already None, so the
        // next iteration lands in the "wait for you to press Record"
        // branch above. No automatic relisten.
    }

    state.active = false;
    state.phase = None;
    state.pending_text = None;
    conversation::write_state(&state);
    Ok(())
}

enum WaitOutcome {
    Proceed,
    Stop,
}

/// Blocks with no timeout until the user triggers a new recording
/// (`converse listen`) or ends the conversation (`converse stop`) --
/// the idle state between turns. A `Confirm`/`Reject`/`StopListening`
/// arriving here (nothing is waiting on them right now) is ignored.
fn wait_for_listen_or_stop(control_rx: &Option<mpsc::Receiver<ConversationAction>>) -> WaitOutcome {
    let Some(rx) = control_rx else { return WaitOutcome::Stop };
    loop {
        match rx.recv() {
            Ok(ConversationAction::Listen) => return WaitOutcome::Proceed,
            Ok(ConversationAction::Stop) => return WaitOutcome::Stop,
            Ok(_) => continue,
            Err(_) => return WaitOutcome::Stop, // sender dropped -- nothing left to wait for
        }
    }
}

enum ReviewOutcome {
    Send(String),
    Discard,
    Stop,
}

/// Blocks with no timeout until the user sends (Enter/Confirm),
/// discards (Reject), or stops the conversation while a transcript is
/// up for review. `Confirm { text: None }` means "send as-is" -- falls
/// back to `current` (whatever's already in `pending_text`); `Some`
/// text is the edited version from the panel's text box.
fn wait_for_review(
    control_rx: &Option<mpsc::Receiver<ConversationAction>>,
    current: &str,
) -> ReviewOutcome {
    let Some(rx) = control_rx else { return ReviewOutcome::Stop };
    loop {
        match rx.recv() {
            Ok(ConversationAction::Confirm { text }) => {
                let text = text
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| current.to_string());
                return ReviewOutcome::Send(text);
            }
            Ok(ConversationAction::Reject) => return ReviewOutcome::Discard,
            Ok(ConversationAction::Stop) => return ReviewOutcome::Stop,
            Ok(_) => continue, // Listen/StopListening don't apply mid-review
            Err(_) => return ReviewOutcome::Stop,
        }
    }
}

/// Runs `router::handoff_external` on a background thread and, while
/// waiting, ticks `state.thinking_elapsed_secs` up once a second and
/// writes it out -- the only progress feedback available (see
/// `ConversationState::thinking_elapsed_secs`'s doc comment for why:
/// no incremental-output mode to tap into, and the call itself has no
/// timeout any more, so a real multi-minute agent turn needs *some*
/// visible sign of life or it's indistinguishable from a hang).
fn run_handoff_with_progress(handoff_text: &str, state: &mut ConversationState) -> (bool, String) {
    let text = handoff_text.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(router::handoff_external(&text, None));
    });

    let start = std::time::Instant::now();
    loop {
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        let elapsed = start.elapsed().as_secs();
        if state.thinking_elapsed_secs != Some(elapsed) {
            state.thinking_elapsed_secs = Some(elapsed);
            conversation::write_state(state);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

enum ListenOutcome {
    Text(String),
    Stop,
    Err(anyhow::Error),
}

/// Runs `pipeline::listen_and_transcribe` on a background thread (that
/// function's own blocking poll loop isn't ours to change -- the plain
/// wake-word pipeline uses it too, with no button to interrupt it) and
/// meanwhile watches the control channel for `StopListening` (send
/// `voxtype record stop` to end the recording early, same "activate
/// voxtype once, exit on silence or on the toggle" contract as letting
/// it run to its own silence-timeout) or `Stop` (also end the
/// recording, then end the whole conversation once the transcript's
/// been read -- not before, so the thread doesn't outlive the process
/// with `record start` never matched by a `stop`).
fn listen_interruptibly(
    cfg: &ConverseConfig,
    control_rx: &Option<mpsc::Receiver<ConversationAction>>,
) -> ListenOutcome {
    let voxtype_binary = cfg.voxtype_binary.clone();
    let transcript_path = cfg.transcript_path.clone();
    let voxtype_state_path = cfg.voxtype_state_path.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = pipeline::listen_and_transcribe(&voxtype_binary, &transcript_path, &voxtype_state_path);
        let _ = tx.send(result);
    });

    let stop_voxtype_now = |cfg: &ConverseConfig| {
        let _ = std::process::Command::new(&cfg.voxtype_binary)
            .args(["record", "stop"])
            .status();
    };

    loop {
        if let Ok(result) = rx.try_recv() {
            return match result {
                Ok(t) => ListenOutcome::Text(t),
                Err(e) => ListenOutcome::Err(e),
            };
        }
        if let Some(crx) = control_rx {
            match crx.try_recv() {
                Ok(ConversationAction::StopListening) => stop_voxtype_now(cfg),
                Ok(ConversationAction::Stop) => {
                    stop_voxtype_now(cfg);
                    // Wait for the background thread to actually
                    // finish (voxtype needs a moment to transcribe and
                    // write the file) rather than leaving it dangling.
                    let _ = rx.recv();
                    return ListenOutcome::Stop;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Splits OpenClaw's reply into `(spoken_summary, displayed_body)` by
/// extracting the first `"TL;DR:"` line (see `TLDR_INSTRUCTION`),
/// matched case-insensitively and tolerant of a leading markdown
/// bullet/heading marker since instruction-following on formatting
/// isn't guaranteed. The TL;DR line itself is dropped from
/// `displayed_body` since it's already shown separately as the spoken
/// summary; `displayed_body` is `full_response` unchanged when no
/// TL;DR line was found.
fn split_tldr(full_response: &str) -> (Option<String>, String) {
    let mut summary = None;
    let mut remaining = Vec::with_capacity(full_response.lines().count());
    let mut found = false;

    for line in full_response.lines() {
        if !found {
            let trimmed = line.trim().trim_start_matches(['-', '*', '#']).trim();
            let lower = trimmed.to_lowercase();
            if let Some(idx) = lower.find("tl;dr:") {
                let text = trimmed[idx + "tl;dr:".len()..].trim();
                if !text.is_empty() {
                    summary = Some(text.to_string());
                }
                found = true;
                continue; // drop this line from the displayed body
            }
        }
        remaining.push(line);
    }

    if summary.is_none() {
        return (None, full_response.to_string());
    }
    let body = remaining.join("\n").trim().to_string();
    (
        summary,
        if body.is_empty() {
            full_response.to_string()
        } else {
            body
        },
    )
}

/// Fallback condenser for when OpenClaw's reply didn't include a
/// parseable TL;DR line -- reuses the same local LLM
/// `classify::Classifier` already talks to (see that module's
/// `ureq`/`/v1/chat/completions` call for the pattern this mirrors).
/// Returns `None` on any failure -- including an unreachable serve
/// instance -- so the caller falls back to speaking `full_response`
/// verbatim rather than losing the turn.
fn summarize_for_speech(full_response: &str, cfg: &ConverseConfig) -> Option<String> {
    const SUMMARIZE_SYSTEM_PROMPT: &str = "You are turning an AI assistant's response into a \
short, natural reply for a spoken voice conversation. Rewrite the following response as 1 to 3 \
short conversational sentences a person would actually say out loud: keep the key information, \
but drop code blocks, file paths, markdown formatting, and any meta-commentary about the task. \
Reply with only the spoken sentences, nothing else.";

    let url = format!(
        "{}/v1/chat/completions",
        cfg.classify_base_url.trim_end_matches('/')
    );
    let user_content = format!("{full_response}{}", crate::classify::NO_THINK_SUFFIX);

    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(ureq::json!({
            "model": cfg.classify_model_id,
            "messages": [
                {"role": "system", "content": SUMMARIZE_SYSTEM_PROMPT},
                {"role": "user", "content": user_content},
            ],
            "max_tokens": 200,
            "temperature": 0.3,
        }))
        .inspect_err(|e| tracing::warn!("[converse] summarize request failed: {e}"))
        .ok()?;

    let json: serde_json::Value = response
        .into_json()
        .inspect_err(|e| tracing::warn!("[converse] summarize response wasn't JSON: {e}"))
        .ok()?;

    let content = json["choices"][0]["message"]["content"].as_str()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
