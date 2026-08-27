//! The OpenClaw voice-conversation loop -- `omarchy-novad converse
//! start` (see main.rs). Unlike pipeline.rs's one-shot wake-word
//! session, this runs indefinitely: listen, confirm the transcript,
//! hand off to OpenClaw, show the full reply in the conversation
//! window (see `crate::conversation`), speak a short derived summary,
//! listen for the reply, repeat -- until `omarchy-novad converse stop`
//! or a spoken stop phrase ends it.
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
//! just callable again per turn (and again for the confirm prompt)
//! here.

use std::time::Duration;

use crate::config::TtsConfig;
use crate::conversation::{
    self, ConversationAction, ConversationPhase, ConversationState, ConversationTurn,
};
use crate::{pipeline, router, tts};

/// Recognized (case-insensitive, substring) as ending the
/// conversation, same spirit as `omarchy-novad converse stop` but
/// reachable hands-free without a second terminal.
const STOP_PHRASES: &[&str] = &["stop conversation", "end conversation", "goodbye jarvis"];

const CONFIRM_PROMPT: &str = "Does this look good? Say yes or no.";
const CONFIRM_YES_WORDS: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "sounds good",
    "looks good",
    "correct",
    "that's right",
    "send it",
];
const CONFIRM_NO_WORDS: &[&str] = &["no", "nope", "not quite", "that's wrong", "redo"];
/// Caps how many times a confirm round can be re-asked after an
/// unclear reply gets treated as a correction (see `confirm_utterance`)
/// -- a safety net against looping forever on a persistently
/// mistranscribed reply, not a limit anyone should ordinarily hit.
const MAX_CONFIRM_ROUNDS: usize = 3;
/// How long to wait for a UI confirm/reject (e.g. Enter in the
/// conversation window's edit box) before falling back to the spoken
/// prompt -- short enough that a user who isn't looking at the screen
/// barely notices the pause, long enough that one who is typing an
/// edit and hits Enter right after `pending_text` appears pre-empts
/// the voice prompt instead of racing it.
const UI_CONFIRM_GRACE: Duration = Duration::from_secs(3);

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
/// given, skips the first listen and goes straight to confirming it --
/// e.g. when this is launched from a wake-word trigger that already
/// captured one (see `Command::Detect`'s `on_detect` custom-command
/// escape hatch in main.rs, which can point at `omarchy-novad converse
/// start` directly).
pub fn run(cfg: &ConverseConfig, initial_utterance: Option<String>) -> anyhow::Result<()> {
    let control_rx = conversation::ControlServer::spawn()
        .inspect_err(|e| tracing::warn!("[converse] control socket failed to start: {e}"))
        .ok();

    // Seeded fully correct up front when an utterance is already in
    // hand (the real wake-word path: pipeline.rs always calls this
    // with `Some(transcript)`, never listens fresh first) instead of
    // writing `{active:true, phase:null}` and then, microseconds
    // later with no delay in between, overwriting it with
    // `{phase:confirming, pending_text:...}` from confirm_utterance's
    // first loop iteration. Found live: the QML FileView watching this
    // file doesn't necessarily catch up between two such rapid writes,
    // so the panel could go straight from "just opened, empty" to
    // whatever phase came *after* confirming -- the user's own first
    // message never got a stable frame to render in, looking like it
    // "didn't land" even though the backend handled it correctly.
    // de-duped by `confirm_utterance`'s own write ending up identical
    // to this one on its first iteration -- redundant, not wrong.
    let mut state = match &initial_utterance {
        Some(u) => ConversationState {
            active: true,
            phase: Some(ConversationPhase::Confirming),
            pending_text: Some(u.clone()),
            turns: Vec::new(),
            hands_free: false,
        },
        None => ConversationState {
            active: true,
            phase: None,
            pending_text: None,
            turns: Vec::new(),
            hands_free: false,
        },
    };
    conversation::write_state(&state);

    let mut pending_utterance = initial_utterance;
    let mut hands_free = false;

    loop {
        let stop = drain_control(&control_rx, &mut hands_free);
        if state.hands_free != hands_free {
            state.hands_free = hands_free;
            conversation::write_state(&state);
        }
        if stop {
            break;
        }

        let utterance = match pending_utterance.take() {
            Some(u) => u,
            None => {
                state.phase = Some(ConversationPhase::Listening);
                state.pending_text = None;
                conversation::write_state(&state);
                match pipeline::listen_and_transcribe(
                    &cfg.voxtype_binary,
                    &cfg.transcript_path,
                    &cfg.voxtype_state_path,
                ) {
                    Ok(t) if t.is_empty() => continue, // nothing said, keep listening
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("[converse] listen failed: {e}");
                        std::thread::sleep(Duration::from_secs(1)); // avoid hammering a persistently-broken voxtype
                        continue;
                    }
                }
            }
        };

        let lower = utterance.to_lowercase();
        if STOP_PHRASES.iter().any(|p| lower.contains(p)) {
            tracing::info!("[converse] stop phrase heard, ending conversation");
            break;
        }

        // Hands-free skips the "does this look good?" step entirely --
        // straight from Listening to Thinking, no UI grace window, no
        // spoken confirm prompt. See ConversationState::hands_free.
        let confirmed = if hands_free {
            Some(utterance)
        } else {
            confirm_utterance(cfg, &control_rx, &mut state, &mut hands_free, utterance)
        };
        let Some(confirmed) = confirmed else {
            conversation::write_state(&state); // rejected or gave up -- listen fresh again
            continue;
        };

        state.phase = Some(ConversationPhase::Thinking);
        conversation::write_state(&state);

        let handoff_text = format!("{confirmed}{TLDR_INSTRUCTION}");
        let (ok, full_response) = router::handoff_external(&handoff_text, None);
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
    }

    state.active = false;
    state.phase = None;
    state.pending_text = None;
    state.hands_free = false;
    conversation::write_state(&state);
    Ok(())
}

/// Drains every control action queued right now (not just one) and
/// applies the ones that make sense outside a confirm step:
/// `HandsFree` toggles `hands_free` immediately regardless of which
/// phase the loop is in, `Stop` is reported back so the caller can end
/// the loop after its current turn. A stray `Confirm`/`Reject` that
/// arrives with nothing waiting on it (not currently in
/// `wait_for_ui_confirm`) is silently dropped, same as before this
/// function existed -- draining fully (instead of the single
/// `try_recv` this replaced) just means it no longer also swallows a
/// `HandsFree` toggle queued right alongside it.
fn drain_control(
    control_rx: &Option<std::sync::mpsc::Receiver<ConversationAction>>,
    hands_free: &mut bool,
) -> bool {
    let Some(rx) = control_rx else { return false };
    let mut stop = false;
    while let Ok(action) = rx.try_recv() {
        match action {
            ConversationAction::Stop => stop = true,
            ConversationAction::HandsFree(v) => *hands_free = v,
            ConversationAction::Confirm { .. } | ConversationAction::Reject => {}
        }
    }
    stop
}

/// Shows `initial` as `pending_text` and gets it confirmed, editable,
/// or rejected before it's ever sent to OpenClaw. Checks the UI
/// control channel first (a `converse confirm`/`reject` -- e.g. Enter
/// in the conversation window's edit box) for up to `UI_CONFIRM_GRACE`;
/// if nothing arrives, falls back to speaking `CONFIRM_PROMPT` and
/// listening for a reply. A reply that's neither a clear yes nor no is
/// treated as a correction -- the new wording becomes the pending text
/// and the question is re-asked, up to `MAX_CONFIRM_ROUNDS` times --
/// rather than discarded outright, since re-stating a mis-transcribed
/// utterance is the natural voice-only way to "edit" it.
///
/// Returns `Some(text)` to proceed with, or `None` if rejected, timed
/// out, or the round cap was hit.
fn confirm_utterance(
    cfg: &ConverseConfig,
    control_rx: &Option<std::sync::mpsc::Receiver<ConversationAction>>,
    state: &mut ConversationState,
    hands_free: &mut bool,
    initial: String,
) -> Option<String> {
    let mut current = initial;

    for _ in 0..MAX_CONFIRM_ROUNDS {
        // A HandsFree toggle can arrive mid-round (e.g. flipped on
        // right after this turn's transcript came back) -- honor it
        // immediately rather than finishing out this round's prompt.
        if *hands_free {
            state.pending_text = None;
            state.hands_free = true;
            conversation::write_state(state);
            return Some(current);
        }

        state.phase = Some(ConversationPhase::Confirming);
        state.pending_text = Some(current.clone());
        conversation::write_state(state);

        match wait_for_ui_confirm(control_rx, &mut current, state, hands_free) {
            Some(true) => {
                state.pending_text = None;
                return Some(current);
            }
            Some(false) => return None, // rejected via UI
            None => {}                  // no UI response in the grace window -- ask by voice
        }

        if let Err(e) = tts::speak(CONFIRM_PROMPT, &cfg.tts) {
            tracing::warn!("[converse] failed to speak confirmation prompt: {e}");
        }

        let reply = match pipeline::listen_and_transcribe(
            &cfg.voxtype_binary,
            &cfg.transcript_path,
            &cfg.voxtype_state_path,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[converse] confirmation listen failed: {e}");
                break;
            }
        };
        if reply.is_empty() {
            break; // nothing said -- give up rather than loop forever
        }

        let lower = reply.to_lowercase();
        let says_yes = CONFIRM_YES_WORDS.iter().any(|w| lower.contains(w));
        let says_no = CONFIRM_NO_WORDS.iter().any(|w| lower.contains(w));
        if says_yes && !says_no {
            state.pending_text = None;
            return Some(current);
        } else if says_no {
            break;
        } else {
            // Neither a clear yes nor no -- treat it as a correction:
            // re-ask with the new wording instead of discarding outright.
            current = reply;
        }
    }

    state.pending_text = None;
    None
}

/// Polls the control channel for up to `UI_CONFIRM_GRACE` for a
/// `Confirm`/`Reject` (a `Stop` bails the whole loop by returning
/// `Some(false)` -- the caller's next iteration of the outer loop will
/// see it's already been consumed here and `run`'s own `stop_requested`
/// check won't fire again for it, but ending the current confirm as a
/// rejection is the right immediate effect either way). `Some(true)`/
/// `Some(false)` is a definite UI answer; `None` means nothing arrived
/// in time, so the caller should fall back to the voice prompt.
fn wait_for_ui_confirm(
    control_rx: &Option<std::sync::mpsc::Receiver<ConversationAction>>,
    current: &mut String,
    state: &mut ConversationState,
    hands_free: &mut bool,
) -> Option<bool> {
    let Some(rx) = control_rx else { return None };
    let deadline = std::time::Instant::now() + UI_CONFIRM_GRACE;
    while std::time::Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ConversationAction::Confirm { text }) => {
                if let Some(edited) = text.filter(|t| !t.trim().is_empty()) {
                    *current = edited;
                }
                return Some(true);
            }
            Ok(ConversationAction::Reject) => return Some(false),
            Ok(ConversationAction::Stop) => return Some(false),
            Ok(ConversationAction::HandsFree(v)) => {
                *hands_free = v;
                state.hands_free = v;
                conversation::write_state(state);
                // Turning it on while a message is already pending
                // confirmation counts as confirming that message too --
                // "go hands-free from here" naturally includes the one
                // that's on screen right now, not just future turns.
                if v {
                    return Some(true);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
        }
    }
    None
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
