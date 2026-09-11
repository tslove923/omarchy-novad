//! Standalone conversation state + control channel for the OpenClaw
//! voice-conversation loop (see `crate::converse::run`) -- same JSON-
//! file + Unix-socket convention as `crate::popup` (see that module's
//! doc comment for why: no WebSocket/HTTP needed when the daemon and
//! its own Quickshell UI share a filesystem).
//!
//! - **Daemon -> UI**: serializes [`ConversationState`] to
//!   `$XDG_RUNTIME_DIR/omarchy-novad/conversation-state.json` on every
//!   change. A dedicated QML window
//!   (`quickshell/OpenClawConversation.qml`) watches it the same way
//!   the popup watches `popup-state.json`.
//! - **UI -> daemon**: `omarchy-novad converse
//!   {stop,listen,stop-listening,send-text}` connects to this module's
//!   control socket and sends one JSON line -- same mechanism as
//!   `popup::respond`, on a separate socket path so the two features'
//!   state never mixes. A "Record" button (or a talk-key bind, see
//!   `docs/design-notes/conversation-flow-redesign.md`) runs `converse
//!   listen` to start a turn's recording (the daemon never starts one
//!   on its own); a "stop recording" toggle runs `converse
//!   stop-listening` to end it early. There is no review/confirm step
//!   any more -- a transcript is sent to OpenClaw the moment it's
//!   heard (see `converse::run`'s doc comment) -- so the only other
//!   action is `send-text`, the panel's always-present chat box
//!   sending a typed message as a fresh turn.

use std::io::{BufRead as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPhase {
    Listening,
    Thinking,
    Speaking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub user_text: String,
    /// OpenClaw's full, unabridged reply -- shown verbatim in the
    /// conversation window.
    pub full_response: String,
    /// The shorter, spoken version derived from `full_response` (see
    /// `converse::spoken_text_for`) -- `None` when `full_response` was
    /// already short enough to speak verbatim, or when condensing a
    /// long one failed and it was spoken verbatim as a fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationState {
    pub active: bool,
    /// The bare `crate::sessions` key this loop is using (see
    /// `router::openclaw::gateway_session_key`) -- empty only for a
    /// still-`Default` state before `converse::run` has written its
    /// first real one. Lets the panel show which session is live and
    /// the session picker highlight it; `#[serde(default)]` covers a
    /// state file written by a daemon binary from before this field
    /// existed.
    #[serde(default)]
    pub session_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ConversationPhase>,
    /// The current turn's utterance, already sent to OpenClaw -- set
    /// the instant a transcript (or typed message) is handed off, so
    /// the panel can show the outgoing bubble right away instead of
    /// waiting for the reply to arrive before the user's own turn is
    /// visible at all. `None` once the turn completes (folded into a
    /// new `turns` entry) or while idle/listening. There is no
    /// review/confirm step any more -- see `crate::converse::run`'s
    /// doc comment -- this field is purely informational display
    /// state, not a gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_user_text: Option<String>,
    pub turns: Vec<ConversationTurn>,
    /// Seconds elapsed on the current OpenClaw handoff -- only
    /// meaningful while `phase == Some(Thinking)`. There's no timeout
    /// on that call any more (a real agent turn can legitimately run
    /// for minutes), so this is the only feedback the panel has that
    /// it's still alive rather than hung -- see
    /// `converse::run_handoff_with_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_elapsed_secs: Option<u64>,
    /// The live, incrementally-streamed text of the current OpenClaw
    /// reply -- only meaningful while `phase == Some(Thinking)`. The
    /// panel renders this in place of a bare "Thinking…" so output
    /// appears as the model produces it, not all at once when the turn
    /// finishes. `None` when nothing is streaming (idle, listening,
    /// speaking, or between turns). Cleared the moment the
    /// handoff returns; the full reply then lands in a new `turns`
    /// entry. See `converse::run_handoff_with_progress` and
    /// `router::openclaw::handoff_streaming`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_text: Option<String>,
}

pub fn state_path() -> PathBuf {
    runtime_dir().join("conversation-state.json")
}

pub fn control_socket_path() -> PathBuf {
    runtime_dir().join("conversation-control.sock")
}

fn runtime_dir() -> PathBuf {
    // Same fallback shape as popup::runtime_dir / main.rs's
    // transcript_path: prefer XDG_RUNTIME_DIR, fall back to the system
    // temp dir.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Writes the current conversation state to disk for the QML
/// `FileView` to pick up. Best-effort, same reasoning as
/// `popup::write_state`: a failed write just means the window shows
/// stale state, not a reason to interrupt the conversation loop.
///
/// Write-temp-then-rename, not a truncate-in-place -- see
/// `popup::write_state`'s doc comment for why (confirmed live: rapid
/// truncate-and-rewrite of the same inode can permanently wedge
/// Quickshell's `FileView` watch). This module's loop writes even more
/// frequently per turn (listening/thinking/speaking, plus streamed
/// deltas while thinking) than the popup's, so it's if anything more
/// exposed to the same bug.
pub fn write_state(state: &ConversationState) {
    let path = state_path();
    match serde_json::to_string(state) {
        Ok(json) => {
            let tmp_path = path.with_file_name(format!(
                "{}.tmp",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let result = std::fs::write(&tmp_path, json).and_then(|_| std::fs::rename(&tmp_path, &path));
            if let Err(e) = result {
                tracing::warn!("failed to write conversation state to {path:?}: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize conversation state: {e}"),
    }
}

/// Reads back whatever `write_state` last wrote -- `None` if no loop
/// has ever run this login session, or the file's momentarily mid-write
/// (see `write_state`'s rename-based approach; a `None` here is rare
/// and never a hang, just "nothing to report"). Used by
/// `main.rs::run_openclaw_continue_in_herdr` to find the presently
/// active session's key without duplicating the daemon's own state.
pub fn read_state() -> Option<ConversationState> {
    let content = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&content).ok()
}

/// Actions the conversation window (or `omarchy-novad converse
/// <action>`) can send back. `Listen` starts a new recording (e.g. a
/// "Record" button, or a talk-key bind) -- `converse::run` never
/// starts one on its own between turns. `StopListening` ends an
/// in-progress recording early (a "toggle" button while listening),
/// same effect as voxtype's own silence-timeout just user-triggered.
/// `SendText` is the panel's always-present chat box: a *fresh* user
/// message. The loop treats it exactly like a just-transcribed
/// utterance (skipping the recording step) and sends it immediately,
/// same as any other turn -- there is no review/confirm step to
/// preempt any more (see `converse::run`'s doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationAction {
    Stop,
    Listen,
    StopListening,
    SendText { text: String },
}

impl ConversationAction {
    fn from_wire(action: &str, text: Option<String>) -> Option<Self> {
        match action.trim() {
            "stop" => Some(Self::Stop),
            "listen" => Some(Self::Listen),
            "stop_listening" => Some(Self::StopListening),
            "send_text" => Some(Self::SendText {
                text: text.unwrap_or_default(),
            }),
            _ => None,
        }
    }
}

/// Wire format for a control-socket message: one JSON object per line,
/// same shape as `popup::ControlMessage`. Still accepts a bare action
/// word with no JSON wrapper too (see `read_one_action`), matching
/// `popup`'s socket for the same easy-manual-testing reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlMessage {
    action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

/// Listens on `control_socket_path()` for actions from
/// `omarchy-novad converse {stop,listen,stop-listening,send-text}` and
/// forwards them to `sender`. Runs until the listener errors (process
/// teardown); intended to run on its own thread from `converse::run`.
pub struct ControlServer;

impl ControlServer {
    /// Spawns the listener thread and returns a channel that yields
    /// each received [`ConversationAction`] in order.
    pub fn spawn() -> std::io::Result<mpsc::Receiver<ConversationAction>> {
        let path = control_socket_path();
        let _ = std::fs::remove_file(&path); // stale socket from a previous run
        let listener = UnixListener::bind(&path)?;
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                if let Some(action) = read_one_action(stream) {
                    if tx.send(action).is_err() {
                        break; // receiver dropped, loop is shutting down
                    }
                }
            }
        });

        Ok(rx)
    }
}

fn read_one_action(stream: UnixStream) -> Option<ConversationAction> {
    let mut line = String::new();
    std::io::BufReader::new(stream).read_line(&mut line).ok()?;
    let trimmed = line.trim();
    let msg: ControlMessage = serde_json::from_str(trimmed).unwrap_or_else(|_| ControlMessage {
        action: trimmed.to_string(),
        text: None,
    });
    let action = ConversationAction::from_wire(&msg.action, msg.text);
    if action.is_none() {
        tracing::warn!("conversation control socket got unrecognized action: {line:?}");
    }
    action
}

fn send_action(action: &str, text: Option<&str>) -> anyhow::Result<()> {
    let path = control_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!("connect to {path:?}: {e} (is 'omarchy-novad converse start' running?)")
    })?;
    let msg = ControlMessage {
        action: action.to_string(),
        text: text.map(String::from),
    };
    writeln!(stream, "{}", serde_json::to_string(&msg)?)?;
    Ok(())
}

/// `omarchy-novad converse stop` entry point: ask the running loop to
/// end after its current turn.
pub fn stop() -> anyhow::Result<()> {
    send_action("stop", None)
}

/// `omarchy-novad converse listen` entry point: start a new recording
/// for the next turn (e.g. a "Record" button) -- the running loop
/// never starts one on its own.
pub fn listen() -> anyhow::Result<()> {
    send_action("listen", None)
}

/// `omarchy-novad converse stop-listening` entry point: end an
/// in-progress recording early (a "toggle" button while listening),
/// same effect as voxtype's own silence-timeout just user-triggered.
pub fn stop_listening() -> anyhow::Result<()> {
    send_action("stop_listening", None)
}

/// `omarchy-novad converse send-text <text>` entry point: send `text`
/// as a new turn's utterance from the panel's always-present chat box
/// -- see `ConversationAction::SendText`. Fails (no socket) if no
/// `converse start` loop is running; the UI starts one with
/// `converse start --text` instead when the panel isn't active.
pub fn send_text(text: &str) -> anyhow::Result<()> {
    send_action("send_text", Some(text))
}

/// `omarchy-novad converse talk` entry point: a single-key toggle for
/// a `mode = "toggle"` talk-key bind (see
/// `docs/design-notes/conversation-flow-redesign.md`'s "Trigger"
/// section -- `push_to_talk` mode doesn't need this at all, it binds
/// `listen`/`stop-listening` directly to a key's press/release via
/// Hyprland's `bind`/`bindr`). Reads the daemon's own state file (the
/// same one the panel watches) to decide which action to send: press
/// while `Listening` sends `stop-listening` (end the recording early);
/// press in any other phase -- idle, Thinking, or Speaking -- sends
/// `listen` (start a new recording, or barge in on Thinking/Speaking,
/// same as the panel's Record button would). Fails (no socket) if no
/// `converse start` loop is running -- same as `listen`/
/// `stop-listening` themselves; there's no "also start a conversation"
/// fallback here, that's a separate `converse start` bind's job.
pub fn talk() -> anyhow::Result<()> {
    if should_stop_listening(read_phase().as_deref()) {
        stop_listening()
    } else {
        listen()
    }
}

/// The decision half of `talk()`, split out as a pure function so it's
/// testable without a real state file on disk.
fn should_stop_listening(phase: Option<&str>) -> bool {
    phase == Some("listening")
}

/// Reads just the `phase` field out of the current state file, best-
/// effort -- `None` on any read/parse failure (no conversation ever
/// started, a torn read mid-write, the file simply not existing yet)
/// is treated by `talk()` the same as "not listening", which is the
/// safe default (it sends `listen`, matching what happens when the
/// state genuinely is idle).
fn read_phase() -> Option<String> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("phase")?.as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::{
        should_stop_listening, ConversationAction, ConversationPhase, ConversationState,
        ConversationTurn,
    };

    #[test]
    fn should_stop_listening_only_when_actually_listening() {
        assert!(should_stop_listening(Some("listening")));
        assert!(!should_stop_listening(Some("thinking")));
        assert!(!should_stop_listening(Some("speaking")));
        assert!(!should_stop_listening(None)); // idle, or no state file yet
    }

    #[test]
    fn state_serializes_streaming_text_when_present() {
        let state = ConversationState {
            active: true,
            session_key: "voice-1".to_string(),
            phase: Some(ConversationPhase::Thinking),
            pending_user_text: None,
            turns: Vec::new(),
            thinking_elapsed_secs: Some(3),
            streaming_text: Some("The capital of France is Par".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"streaming_text\":\"The capital of France is Par\""));
        assert!(json.contains("\"thinking_elapsed_secs\":3"));
    }

    #[test]
    fn state_omits_streaming_text_when_absent() {
        let state = ConversationState {
            active: true,
            session_key: "voice-1".to_string(),
            phase: None,
            pending_user_text: None,
            turns: Vec::new(),
            thinking_elapsed_secs: None,
            streaming_text: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("streaming_text"));
        assert!(!json.contains("thinking_elapsed_secs"));
    }

    #[test]
    fn state_round_trips_turns_with_spoken_summary() {
        let state = ConversationState {
            active: true,
            session_key: "voice-1".to_string(),
            phase: Some(ConversationPhase::Speaking),
            pending_user_text: None,
            turns: vec![ConversationTurn {
                user_text: "hi".to_string(),
                full_response: "Hello!".to_string(),
                spoken_summary: Some("Hello!".to_string()),
            }],
            thinking_elapsed_secs: None,
            streaming_text: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["turns"][0]["user_text"], "hi");
        assert_eq!(parsed["turns"][0]["spoken_summary"], "Hello!");
    }

    #[test]
    fn from_wire_parses_send_text() {
        assert_eq!(
            ConversationAction::from_wire("send_text", Some("hello".to_string())),
            Some(ConversationAction::SendText {
                text: "hello".to_string()
            })
        );
        // Missing text defaults to empty (the loop treats empty as a
        // no-op, so this is safe).
        assert_eq!(
            ConversationAction::from_wire("send_text", None),
            Some(ConversationAction::SendText {
                text: String::new()
            })
        );
    }

    #[test]
    fn from_wire_parses_existing_actions() {
        assert_eq!(
            ConversationAction::from_wire("stop", None),
            Some(ConversationAction::Stop)
        );
        assert_eq!(
            ConversationAction::from_wire("listen", None),
            Some(ConversationAction::Listen)
        );
        assert_eq!(
            ConversationAction::from_wire("stop_listening", None),
            Some(ConversationAction::StopListening)
        );
        assert_eq!(ConversationAction::from_wire("bogus", None), None);
    }
}
