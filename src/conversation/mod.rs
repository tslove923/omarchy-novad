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
//!   {stop,confirm,reject,listen,stop-listening}` connects to this
//!   module's control socket and sends one JSON line -- same mechanism
//!   as `popup::respond`, on a separate socket path so the two
//!   features' state never mixes. A "Record" button runs `converse
//!   listen` to start a turn's recording (the daemon never starts one
//!   on its own); a "stop recording" toggle runs `converse
//!   stop-listening` to end it early. Once a transcript is up for
//!   review, the conversation window's pending-text box runs `converse
//!   confirm --text "<edited text>"` on Enter (an edit + send in one
//!   action) and a Reject button runs `converse reject`.

use std::io::{BufRead as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPhase {
    Listening,
    /// Showing a just-transcribed utterance in `pending_text` and
    /// asking "does this look good?" -- see `crate::converse`'s
    /// confirm-before-send step. Distinct from `Listening` even though
    /// it's also waiting on the mic, so the UI can show the pending
    /// text + prompt rather than a bare "listening" indicator.
    Confirming,
    Thinking,
    Speaking,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationTurn {
    pub user_text: String,
    /// OpenClaw's full, unabridged reply -- shown verbatim in the
    /// conversation window.
    pub full_response: String,
    /// The shorter, spoken version derived from `full_response` (see
    /// `converse`'s TL;DR parsing / `summarize_for_speech` fallback)
    /// -- `None` when both failed and `full_response` was spoken
    /// verbatim instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoken_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConversationState {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<ConversationPhase>,
    /// The most recent transcript, awaiting "does this look good?"
    /// confirmation -- shown distinctly from committed `turns` so the
    /// UI can offer an editable box for it. `None` once confirmed
    /// (folded into a new `turns` entry) or rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_text: Option<String>,
    pub turns: Vec<ConversationTurn>,
    /// Seconds elapsed on the current OpenClaw handoff -- only
    /// meaningful while `phase == Some(Thinking)`. There's no timeout
    /// on that call any more (a real agent turn can legitimately run
    /// for minutes), so this is the only feedback the panel has that
    /// it's still alive rather than hung -- see
    /// `converse::run_handoff_with_progress`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_elapsed_secs: Option<u64>,
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
/// frequently per turn (listening/confirming/thinking/speaking, plus a
/// fresh write per confirm-round correction) than the popup's, so it's
/// if anything more exposed to the same bug.
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

/// Actions the conversation window (or `omarchy-novad converse
/// <action>`) can send back. `Listen` starts a new recording (e.g. a
/// "Record" button) -- `converse::run` never starts one on its own
/// between turns. `StopListening` ends an in-progress recording early
/// (a "toggle" button while listening), same effect as voxtype's own
/// silence-timeout just user-triggered. `Confirm`/`Reject` answer
/// "send this?" for `pending_text` once a transcript is up for review
/// -- no timeout, no voice fallback (see `converse::wait_for_review`).
/// `Confirm`'s `text`, when present, is the edited pending text to
/// send instead of the original transcript -- an edit and a send in
/// one action, since the UI never needs to stage an edit without also
/// deciding whether to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationAction {
    Stop,
    Confirm { text: Option<String> },
    Reject,
    Listen,
    StopListening,
}

impl ConversationAction {
    fn from_wire(action: &str, text: Option<String>) -> Option<Self> {
        match action.trim() {
            "stop" => Some(Self::Stop),
            "confirm" => Some(Self::Confirm { text }),
            "reject" => Some(Self::Reject),
            "listen" => Some(Self::Listen),
            "stop_listening" => Some(Self::StopListening),
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
/// `omarchy-novad converse {stop,confirm,reject}` and forwards them to
/// `sender`. Runs until the listener errors (process teardown);
/// intended to run on its own thread from `converse::run`.
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

/// `omarchy-novad converse confirm [--text ...]` entry point: send the
/// pending transcript (optionally replaced with an edited version
/// first -- see `ConversationAction::Confirm`) to OpenClaw.
pub fn confirm(text: Option<&str>) -> anyhow::Result<()> {
    send_action("confirm", text)
}

/// `omarchy-novad converse reject` entry point: discard the pending
/// transcript without sending it.
pub fn reject() -> anyhow::Result<()> {
    send_action("reject", None)
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
