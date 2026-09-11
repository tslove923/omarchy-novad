//! The OpenClaw voice-conversation loop -- `omarchy-novad converse
//! start` (see main.rs). Unlike pipeline.rs's one-shot wake-word
//! session, this runs turn after turn until stopped, but -- unlike an
//! even earlier version of this module -- never activates voxtype on
//! its own between turns. Each turn is: the user explicitly triggers a
//! recording (`converse listen`, e.g. a "Record" button or a talk-key
//! bind, or the first turn's utterance arriving already-captured from
//! a wake word), or types a message into the panel's always-present
//! chat box (`converse send-text`, see `ConversationAction::SendText`
//! -- the typed text skips the recording step); voxtype records until
//! it hits its own silence-timeout or the user manually ends it early
//! (`converse stop-listening`, a "toggle" button/talk-key release
//! while recording); the resulting transcript is trusted and handed to
//! OpenClaw immediately, no review/confirm gate in between -- OpenClaw
//! replies, the reply is shown and spoken -- then the loop goes back
//! to waiting for the user to explicitly trigger the next recording.
//! Pressing the talk key again (`Listen`) barges in on either
//! "Thinking..." or "Speaking...": an in-flight handoff is abandoned
//! (see `run_handoff_with_progress`'s doc comment) or in-flight TTS
//! playback is killed (see `speak_with_barge_in`/`tts::speak`'s
//! `cancel` flag), and the next recording starts immediately --
//! talking over Jarvis means redirect, not "wait and then redirect".
//!
//! Two things this deliberately still isn't, both found live on
//! earlier versions of this module (see git history) and kept out on
//! purpose:
//! - **Fully automatic relisten.** voxtype firing itself up again
//!   right after every reply, unprompted, was more disruptive than
//!   helpful -- the fix for "too much friction" is a fast trigger (see
//!   `docs/design-notes/conversation-flow-redesign.md`), not removing
//!   the trigger and letting the mic decide on its own when to listen.
//! - **A blocking "does this look good?" gate before every send.**
//!   That one *is* new as of this revision -- see this module's own
//!   git history for the review-step version this replaced. Auto-stop
//!   already means the turn is over; sending it should follow
//!   immediately, the same way a person doesn't pause a conversation
//!   to silently reread what they just said before letting the other
//!   person respond. A bad transcript gets corrected the way humans do
//!   it -- say something else, or type a correction into the panel's
//!   chat box -- not through a modal in front of every single turn.
//!
//! The OpenClaw reply streams into the panel as it's produced: the
//! handoff connects straight to the gateway WebSocket
//! (`router::openclaw::handoff_streaming`) instead of the final-only
//! `openclaw agent` CLI, and each delta is written to
//! `ConversationState::streaming_text` as it arrives (see
//! `run_handoff_with_progress`), so the panel shows output live rather
//! than all at once when the turn completes.
//!
//! Reuses existing pieces rather than inventing new ones:
//! `router::openclaw::handoff_streaming` keeps every turn on this
//! loop's one `ConverseConfig::session_key` OpenClaw session (see
//! `crate::sessions`), so context carries across turns the same way it
//! already does for the wake-word path.
//! `pipeline::listen_and_transcribe` is the exact record+transcribe
//! round-trip `pipeline::run_session` uses for its own single turn,
//! just callable again per turn here, wrapped in `listen_interruptibly`
//! below so a `converse stop-listening` can end it early without
//! needing a change to that shared function (used by the plain
//! wake-word pipeline too, which has no such button).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::chime::{self, Chime};
use crate::config::TtsConfig;
use crate::conversation::{
    self, ConversationAction, ConversationPhase, ConversationState, ConversationTurn,
};
use crate::{pipeline, router, sessions, tts};

/// Appended to the utterance before handing off to OpenClaw -- asks it
/// to answer the way it'd actually *say* the answer out loud, not
/// write a document and have a second, context-blind model condense it
/// after the fact. This is the real ask; `spoken_text_for` only falls
/// back to a second summarizing call (`summarize_for_speech`) for the
/// replies that come back long anyway (code, a multi-paragraph
/// explanation, a real report) despite this instruction -- an ask, not
/// a hard constraint on the model.
const CONVERSATIONAL_INSTRUCTION: &str = "\n\n(Answer the way you'd actually say it out loud in a \
conversation -- direct, natural spoken language, not a written report. Keep it to a few sentences \
unless the question genuinely needs more detail to answer it.)";

/// Default for `ConverseConfig::idle_timeout` -- a starting guess (3-5
/// minutes was the design doc's range), not a measured value; tune
/// live. Exposed as a `converse start --idle-timeout-secs` override;
/// the wake-word path (`pipeline.rs`) just uses this default directly,
/// same as it does for TTS voice/model with no per-path override.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 240;

pub struct ConverseConfig {
    /// Which OpenClaw session (`crate::sessions`) every turn of this
    /// loop hands off to -- resolved once, before `run` is called
    /// (`main.rs::run_converse_start` mints a fresh one via
    /// `sessions::new_session()` unless `converse start --session` gave
    /// an existing key to resume). Fixed for this loop's whole
    /// lifetime: switching sessions means stopping this loop and
    /// starting a new one, not changing this mid-conversation -- see
    /// `plugin/Service.qml`'s session-picker doc comment.
    pub session_key: String,
    pub voxtype_binary: String,
    pub transcript_path: std::path::PathBuf,
    pub voxtype_state_path: std::path::PathBuf,
    /// voxtype's live in-progress transcript for the recording in
    /// flight -- see `pipeline::listen_and_transcribe`'s `on_partial`
    /// doc comment. Not yet surfaced anywhere in this loop's own UI
    /// (the conversation panel doesn't show a live listening
    /// transcript today, only the popup does) -- threaded through so
    /// `listen_interruptibly` can pass the real path regardless, ready
    /// for whenever that's wired up.
    pub voxtype_partial_path: std::path::PathBuf,
    /// Reuses the already-running LLM `serve` instance (see
    /// `classify::Classifier`'s identical `base_url`/`model_id` shape)
    /// as a fallback summarizer -- see `spoken_text_for`.
    pub classify_base_url: String,
    pub classify_model_id: String,
    pub tts: TtsConfig,
    /// How long to wait, idle between turns, for the user to trigger a
    /// new recording or send a chat-box message before the
    /// conversation ends on its own (same as `ConversationAction::Stop`
    /// -- see `wait_for_listen_or_stop`). Without this, a forgotten
    /// conversation blocks wake-word detection forever: `run_detect`'s
    /// mic-capture loop doesn't return to feeding the wake-word
    /// detector until `converse::run` itself returns (see
    /// `docs/design-notes/conversation-flow-redesign.md`'s "Idle
    /// auto-end" section).
    pub idle_timeout: std::time::Duration,
    /// Mirrors `config::ChimeConfig::enabled` -- whether the
    /// listen-start/sent/reply-ready cues (`chime::play`) actually
    /// play. Found live: distracting during testing, so this defaults
    /// off (`false`) unlike most of this crate's other opt-out
    /// toggles -- see `config::ChimeConfig`'s doc comment.
    pub chimes_enabled: bool,
}

/// Plays `chime` only if `cfg.chimes_enabled` -- see that field's doc
/// comment. Every one of this module's chime calls goes through here
/// rather than `chime::play` directly, so there's exactly one place
/// that can forget the check.
fn maybe_chime(cfg: &ConverseConfig, chime_kind: Chime) {
    if cfg.chimes_enabled {
        chime::play(chime_kind);
    }
}

/// Runs the conversation loop until stopped. `initial_utterance`, when
/// given, skips straight to the first turn's handoff with it already
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
    // between, overwriting it with `{phase:thinking, pending_user_text:
    // ...}` -- found live, the QML FileView watching this file doesn't
    // necessarily catch up between two such rapid writes.
    let mut state = match &initial_utterance {
        Some(u) => ConversationState {
            active: true,
            session_key: cfg.session_key.clone(),
            phase: Some(ConversationPhase::Thinking),
            pending_user_text: Some(u.clone()),
            turns: Vec::new(),
            thinking_elapsed_secs: Some(0),
            streaming_text: None,
        },
        None => ConversationState {
            active: true,
            session_key: cfg.session_key.clone(),
            phase: None,
            pending_user_text: None,
            turns: Vec::new(),
            thinking_elapsed_secs: None,
            streaming_text: None,
        },
    };
    conversation::write_state(&state);
    if initial_utterance.is_some() {
        // Same "heard you, working on it" cue any other turn's
        // auto-send gets -- the wake-word path already has its
        // utterance in hand, so it skips straight to Thinking above
        // with no separate "sent" moment of its own to hang this on.
        maybe_chime(cfg, Chime::Sent);
    }

    let mut pending_utterance = initial_utterance;

    'session: loop {
        // Get this turn's utterance: either already in hand (the first
        // turn, from a wake word), or wait -- no auto-relisten, but
        // bounded by `idle_timeout` so a forgotten conversation ends
        // itself rather than blocking wake-word detection forever --
        // for the user to explicitly trigger a new recording. `phase:
        // None` here doubles as "idle, waiting for you to press
        // Record" (same convention `run` used for "before the very
        // first listen" before this rewrite).
        let utterance = match pending_utterance.take() {
            Some(u) => u,
            None => {
                state.phase = None;
                state.pending_user_text = None;
                conversation::write_state(&state);

                match wait_for_listen_or_stop(&control_rx, cfg.idle_timeout) {
                    WaitOutcome::Stop => break 'session,
                    // A chat-box message while idle: use it as this
                    // turn's utterance, skipping the recording step.
                    WaitOutcome::Text(t) => t,
                    WaitOutcome::Proceed => {
                        state.phase = Some(ConversationPhase::Listening);
                        conversation::write_state(&state);
                        maybe_chime(cfg, Chime::ListenStart);

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
                }
            }
        };

        // Auto-send: no review/confirm gate. Trust the transcript the
        // moment it's in hand and go straight to the handoff --
        // `pending_user_text` is set purely so the panel can show the
        // outgoing bubble immediately, not to block on anything (see
        // this module's doc comment and
        // `docs/design-notes/conversation-flow-redesign.md`).
        state.phase = Some(ConversationPhase::Thinking);
        state.pending_user_text = Some(utterance.clone());
        state.thinking_elapsed_secs = Some(0);
        conversation::write_state(&state);
        maybe_chime(cfg, Chime::Sent);
        // Bumps recency every turn (not just at session creation) so
        // the picker's ordering reflects real activity, and fills in
        // the label from whichever turn happens to be first -- the
        // wake-word path seeds `initial_utterance` before this loop's
        // first iteration, so that's usually turn one; a `--session`
        // resume of an existing conversation just re-labels it the
        // same way its first turn ever did (a no-op, see `sessions::touch`).
        sessions::touch(&cfg.session_key, &utterance);

        let handoff_text = format!("{utterance}{CONVERSATIONAL_INSTRUCTION}");
        let outcome = run_handoff_with_progress(&handoff_text, &cfg.session_key, &mut state, &control_rx);
        state.thinking_elapsed_secs = None;

        let (ok, full_response) = match outcome {
            HandoffOutcome::Stopped => break 'session,
            HandoffOutcome::BargedIn => {
                // The talk key again means "redirect", not "queue
                // behind the reply" -- go straight into the next
                // recording instead of waiting for a Listen that
                // already happened (see run_handoff_with_progress's
                // doc comment). The abandoned turn leaves no residue:
                // it never reaches `turns`, and its outgoing bubble is
                // cleared here rather than lingering under the new
                // Listening phase. Same Listening steps as the idle
                // Proceed branch above, just entered mid-turn.
                state.phase = Some(ConversationPhase::Listening);
                state.pending_user_text = None;
                conversation::write_state(&state);
                maybe_chime(cfg, Chime::ListenStart);

                match listen_interruptibly(cfg, &control_rx) {
                    ListenOutcome::Stop => break 'session,
                    ListenOutcome::Text(t) if t.is_empty() => continue 'session, // nothing said
                    ListenOutcome::Text(t) => {
                        pending_utterance = Some(t);
                        continue 'session;
                    }
                    ListenOutcome::Err(e) => {
                        tracing::warn!("[converse] listen failed: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                        continue 'session;
                    }
                }
            }
            HandoffOutcome::Done(ok, full_response) => (ok, full_response),
        };
        if !ok {
            tracing::warn!("[converse] openclaw handoff failed: {full_response}");
        }

        let spoken_summary = spoken_text_for(&full_response, cfg);

        state.pending_user_text = None;
        state.turns.push(ConversationTurn {
            user_text: utterance,
            full_response: full_response.clone(),
            spoken_summary: spoken_summary.clone(),
        });
        conversation::write_state(&state);

        let to_speak = spoken_summary.unwrap_or(full_response);
        state.phase = Some(ConversationPhase::Speaking);
        conversation::write_state(&state);
        maybe_chime(cfg, Chime::ReplyReady);

        match speak_with_barge_in(&to_speak, cfg, &control_rx) {
            SpeakOutcome::Stopped => break 'session,
            SpeakOutcome::BargedIn => {
                // Same handling as a Thinking-phase barge-in: go
                // straight into the next recording instead of waiting
                // for a Listen that already happened.
                state.phase = Some(ConversationPhase::Listening);
                conversation::write_state(&state);
                maybe_chime(cfg, Chime::ListenStart);

                match listen_interruptibly(cfg, &control_rx) {
                    ListenOutcome::Stop => break 'session,
                    ListenOutcome::Text(t) if t.is_empty() => continue 'session, // nothing said
                    ListenOutcome::Text(t) => pending_utterance = Some(t),
                    ListenOutcome::Err(e) => {
                        tracing::warn!("[converse] listen failed: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                        continue 'session;
                    }
                }
            }
            SpeakOutcome::Done => {}
        }
        // Back to the top -- pending_utterance is either already None
        // (normal end of turn or a Stopped/Stop mid-listen already
        // broke out above) or freshly set by a Speaking-phase barge-in,
        // in which case the next iteration picks it up immediately
        // instead of landing in the "wait for you to press Record"
        // branch. No automatic relisten otherwise.
    }

    state.active = false;
    state.phase = None;
    state.pending_user_text = None;
    conversation::write_state(&state);
    Ok(())
}

enum WaitOutcome {
    Proceed,
    /// A chat-box message arrived while idle -- use it as the next
    /// turn's utterance without recording.
    Text(String),
    Stop,
}

/// Blocks until the user triggers a new recording (`converse listen`),
/// types a chat-box message (`converse send-text`), ends the
/// conversation (`converse stop`), or `idle_timeout` elapses with none
/// of those happening -- the idle state between turns. A timeout is
/// treated exactly like an explicit `Stop`: the conversation ends
/// gracefully (see `run`'s doc comment on why an unbounded wait here
/// would otherwise block wake-word detection indefinitely). A
/// `StopListening` arriving here (nothing is waiting on it right now)
/// is ignored.
fn wait_for_listen_or_stop(
    control_rx: &Option<mpsc::Receiver<ConversationAction>>,
    idle_timeout: Duration,
) -> WaitOutcome {
    let Some(rx) = control_rx else { return WaitOutcome::Stop };
    loop {
        match rx.recv_timeout(idle_timeout) {
            Ok(ConversationAction::Listen) => return WaitOutcome::Proceed,
            Ok(ConversationAction::SendText { text }) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue; // empty chat-box send is a no-op
                }
                return WaitOutcome::Text(text);
            }
            Ok(ConversationAction::Stop) => return WaitOutcome::Stop,
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::info!(
                    "[converse] idle for {idle_timeout:?} with no new turn -- ending the conversation"
                );
                return WaitOutcome::Stop;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return WaitOutcome::Stop, // sender dropped -- nothing left to wait for
        }
    }
}

/// What the handoff thread sends back over the channel: either a
/// streaming text update (the cumulative reply-so-far, see
/// `router::openclaw::handoff_streaming`'s `on_text` callback) or the
/// final `(ok, full_response)` result.
enum StreamEvent {
    Text(String),
    Done((bool, String)),
}

/// How `run_handoff_with_progress` ended: the handoff actually
/// finished (`Done`), or it was preempted mid-flight by the talk key
/// (`BargedIn`, see that function's doc comment) or `Stop`
/// (`Stopped`) -- talking over a "Thinking..." reply means redirect,
/// not "wait for it and then redirect".
enum HandoffOutcome {
    Done(bool, String),
    BargedIn,
    Stopped,
}

/// Runs `router::openclaw::handoff_streaming` on a background thread
/// and, while waiting, writes each streamed text chunk to
/// `state.streaming_text` (so the panel renders OpenClaw's reply live
/// as it's produced) and ticks `state.thinking_elapsed_secs` up once a
/// second. The handoff call itself has no timeout (a real agent turn
/// can legitimately run for minutes), so the elapsed-seconds tick is
/// still the "it's alive, not hung" signal -- see
/// `ConversationState::thinking_elapsed_secs`'s doc comment -- and the
/// streaming text is now the actual content. All state writes happen
/// on this (the loop's) thread, never the handoff thread, so there's
/// no locking: the handoff thread only sends over the channel.
///
/// Also watches `control_rx` for a barge-in: a `Listen` (the talk key
/// pressed again while still "Thinking...") or `Stop` preempts the
/// wait -- the handoff thread above is left running to completion in
/// the background regardless (there's no way to cancel the in-flight
/// WebSocket call itself), but its eventual result is simply dropped
/// once this function returns and `rx` goes out of scope, same "abort
/// promptly, no flush required" contract voxtype's own
/// `cancel_streaming_to_idle` uses for the equivalent problem on the
/// transcription side.
fn run_handoff_with_progress(
    handoff_text: &str,
    session_key: &str,
    state: &mut ConversationState,
    control_rx: &Option<mpsc::Receiver<ConversationAction>>,
) -> HandoffOutcome {
    let text = handoff_text.to_string();
    let session_key = session_key.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = router::openclaw::handoff_streaming(&text, &session_key, |chunk| {
            let _ = tx.send(StreamEvent::Text(chunk.to_string()));
        });
        let _ = tx.send(StreamEvent::Done(result));
    });

    let start = std::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(StreamEvent::Text(chunk)) => {
                state.streaming_text = Some(chunk);
                conversation::write_state(state);
            }
            Ok(StreamEvent::Done((ok, full_response))) => {
                state.streaming_text = None;
                return HandoffOutcome::Done(ok, full_response);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // Handoff thread died without sending Done (a panic in
                // the WebSocket code, say) -- don't spin forever on a
                // dead channel.
                state.streaming_text = None;
                return HandoffOutcome::Done(
                    false,
                    "The external assistant failed to respond".to_string(),
                );
            }
        }

        if let Some(crx) = control_rx {
            match crx.try_recv() {
                Ok(ConversationAction::Listen) => {
                    state.streaming_text = None;
                    return HandoffOutcome::BargedIn;
                }
                Ok(ConversationAction::Stop) => {
                    state.streaming_text = None;
                    return HandoffOutcome::Stopped;
                }
                _ => {} // SendText/StopListening don't apply mid-thinking
            }
        }

        let elapsed = start.elapsed().as_secs();
        if state.thinking_elapsed_secs != Some(elapsed) {
            state.thinking_elapsed_secs = Some(elapsed);
            conversation::write_state(state);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How `speak_with_barge_in` ended -- mirrors `HandoffOutcome`'s three
/// cases for the same reason (`Done`, or preempted by `Listen`/`Stop`).
enum SpeakOutcome {
    Done,
    BargedIn,
    Stopped,
}

/// Runs `tts::speak` on its own thread (so this thread is free to keep
/// watching `control_rx` while playback is underway -- `tts::speak`
/// itself blocks until the whole reply has played, `cancel` is set, or
/// the thread panics) and watches for a barge-in: `Listen` or `Stop`
/// arriving while still speaking sets `cancel`, which `tts::speak`
/// (and, more importantly, `play()` underneath it) polls to kill
/// `paplay` promptly rather than finishing the current sentence first
/// -- see `tts`'s module doc comment.
fn speak_with_barge_in(
    to_speak: &str,
    cfg: &ConverseConfig,
    control_rx: &Option<mpsc::Receiver<ConversationAction>>,
) -> SpeakOutcome {
    let cancel = Arc::new(AtomicBool::new(false));
    let text = to_speak.to_string();
    let tts_cfg = cfg.tts.clone();
    let thread_cancel = Arc::clone(&cancel);
    let handle = std::thread::spawn(move || {
        if let Err(e) = tts::speak(&text, &tts_cfg, &thread_cancel) {
            tracing::warn!("[converse] tts failed: {e}");
        }
    });

    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return SpeakOutcome::Done;
        }
        if let Some(crx) = control_rx {
            match crx.try_recv() {
                Ok(ConversationAction::Listen) => {
                    cancel.store(true, Ordering::Relaxed);
                    let _ = handle.join();
                    return SpeakOutcome::BargedIn;
                }
                Ok(ConversationAction::Stop) => {
                    cancel.store(true, Ordering::Relaxed);
                    let _ = handle.join();
                    return SpeakOutcome::Stopped;
                }
                _ => {} // SendText/StopListening don't apply mid-speaking
            }
        }
        std::thread::sleep(Duration::from_millis(50));
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
    let voxtype_partial_path = cfg.voxtype_partial_path.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = pipeline::listen_and_transcribe(
            &voxtype_binary,
            &transcript_path,
            &voxtype_state_path,
            &voxtype_partial_path,
            |_partial| {}, // no live-listening display in this loop yet -- see ConverseConfig::voxtype_partial_path
        );
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

/// A reply at or under this many words is already short enough to
/// speak verbatim -- no second model call needed. Past it, the reply
/// gets condensed first (see `spoken_text_for`). A rough starting
/// guess, not a measured value -- `CONVERSATIONAL_INSTRUCTION` asks
/// for "a few sentences" already, so most replies should land well
/// under this; tune live if it's cutting things too fine either way.
const VERBATIM_WORD_LIMIT: usize = 40;

/// Decides what to actually speak for `full_response`: `None` means
/// "speak it verbatim, it's already short/conversational" (the common
/// case, given `CONVERSATIONAL_INSTRUCTION`); `Some` is a condensed
/// version via `summarize_for_speech`, for the replies that come back
/// long anyway -- code, a multi-paragraph explanation, a real report.
/// `full_response` itself (shown in the panel) is always the complete,
/// unedited reply either way -- this only decides what gets spoken.
fn spoken_text_for(full_response: &str, cfg: &ConverseConfig) -> Option<String> {
    if full_response.split_whitespace().count() <= VERBATIM_WORD_LIMIT {
        return None;
    }
    summarize_for_speech(full_response, cfg)
}

/// Condenser for replies too long to speak verbatim (see
/// `spoken_text_for`) -- reuses the same local LLM
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

#[cfg(test)]
mod tests {
    use super::{
        spoken_text_for, wait_for_listen_or_stop, ConverseConfig, WaitOutcome,
        DEFAULT_IDLE_TIMEOUT_SECS, VERBATIM_WORD_LIMIT,
    };
    use crate::conversation::ConversationAction;
    use std::sync::mpsc;
    use std::time::Duration;

    // Generous enough that a test's spawned sender thread always wins
    // the race -- these tests are about the received-action branches,
    // not the timeout branch (see `wait_for_listen_or_stop_times_out_when_idle`
    // for that one).
    const TEST_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn wait_for_listen_or_stop_returns_typed_text() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ConversationAction::SendText {
                text: "hello there".to_string(),
            });
        });
        let outcome = wait_for_listen_or_stop(&Some(rx), TEST_IDLE_TIMEOUT);
        assert!(matches!(outcome, WaitOutcome::Text(t) if t == "hello there"));
    }

    #[test]
    fn wait_for_listen_or_stop_trims_and_ignores_empty_text() {
        // Empty chat-box sends are no-ops -- the loop keeps waiting.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ConversationAction::SendText {
                text: "   ".to_string(),
            });
            let _ = tx.send(ConversationAction::Listen);
        });
        let outcome = wait_for_listen_or_stop(&Some(rx), TEST_IDLE_TIMEOUT);
        assert!(matches!(outcome, WaitOutcome::Proceed));
    }

    #[test]
    fn wait_for_listen_or_stop_times_out_when_idle() {
        // Nothing ever sent -- the timeout itself is the only way this
        // returns, exactly like a forgotten conversation ending itself.
        let (_tx, rx) = mpsc::channel::<ConversationAction>();
        let outcome = wait_for_listen_or_stop(&Some(rx), Duration::from_millis(50));
        assert!(matches!(outcome, WaitOutcome::Stop));
    }

    // A throwaway config for `spoken_text_for`'s verbatim (short-reply)
    // path, which returns before touching any of these fields -- real
    // values only matter for the summarization fallback (a live
    // network call), which isn't exercised here. See this module's
    // doc comment on why that path isn't unit-tested.
    fn unused_converse_config() -> ConverseConfig {
        ConverseConfig {
            session_key: "test".to_string(),
            voxtype_binary: String::new(),
            transcript_path: std::path::PathBuf::new(),
            voxtype_state_path: std::path::PathBuf::new(),
            voxtype_partial_path: std::path::PathBuf::new(),
            classify_base_url: String::new(),
            classify_model_id: String::new(),
            tts: crate::config::TtsConfig::default(),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            chimes_enabled: false,
        }
    }

    #[test]
    fn spoken_text_for_speaks_a_short_reply_verbatim() {
        // At/under VERBATIM_WORD_LIMIT words: None means "speak
        // full_response itself" -- no summarization call needed.
        let response = "The lights are on in the kitchen.";
        assert_eq!(spoken_text_for(response, &unused_converse_config()), None);
    }

    #[test]
    fn spoken_text_for_word_count_uses_whitespace_splitting() {
        // Exactly at the limit still counts as verbatim (<=, not <).
        let response = "word ".repeat(VERBATIM_WORD_LIMIT);
        assert_eq!(
            spoken_text_for(response.trim(), &unused_converse_config()),
            None
        );
    }
}
