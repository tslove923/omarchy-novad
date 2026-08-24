//! Standalone popup state + control channel.
//!
//! Mirrors voxtype's own Quickshell OSD conventions exactly (see
//! `voxtype/quickshell/voxtype-shared/StateReader.qml` and
//! `MeetingControls.qml`), just with richer state than a single word:
//!
//! - **Daemon -> UI**: this module serializes [`PopupState`] to a JSON
//!   file at `$XDG_RUNTIME_DIR/novad/popup-state.json` on every state
//!   change. The QML side watches it with `Quickshell.Io.FileView`
//!   (`watchChanges: true`), same mechanism as voxtype's `StateReader`.
//! - **UI -> daemon**: button clicks in the popup run `novad respond
//!   <action>` via `Quickshell.Io.Process`, same mechanism as
//!   `MeetingControls.qml`'s `voxtype meeting show` calls. That
//!   subprocess connects to a Unix socket this module's [`ControlServer`]
//!   listens on and sends one line, then exits.
//!
//! No WebSocket, no HTTP, no bearer tokens — nova-npu's Electron app
//! needed those because Electron and the Python service were two
//! separate processes with no shared filesystem convention. novad and
//! its own popup are both native to this daemon; a JSON file and a
//! Unix socket are enough.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

use serde::Serialize;

/// Mirrors nova's popup.js state machine (`idle`, `listening`,
/// `recording`, `transcribing`, `classifying`, `confirming`, `ready`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupPhase {
    Idle,
    Listening,
    Recording,
    Transcribing,
    Classifying,
    Confirming,
    Ready,
}

#[derive(Debug, Clone, Serialize)]
pub struct PopupState {
    pub phase: PopupPhase,
    /// Transcript / AI response text shown in the text area. Empty
    /// string renders the "empty" placeholder state in QML.
    pub text: String,
    /// Set only during `Confirming` — the action awaiting approve/deny.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_label: Option<String>,
}

impl Default for PopupState {
    fn default() -> Self {
        Self { phase: PopupPhase::Idle, text: String::new(), confirm_label: None }
    }
}

pub fn state_path() -> PathBuf {
    runtime_dir().join("popup-state.json")
}

pub fn control_socket_path() -> PathBuf {
    runtime_dir().join("popup-control.sock")
}

fn runtime_dir() -> PathBuf {
    // Same fallback shape as the rest of novad (cache_dir(), etc.):
    // prefer XDG_RUNTIME_DIR, fall back to the system temp dir rather
    // than trying to reconstruct /run/user/<uid> ourselves.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("novad");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Write the current popup state to disk for the QML `FileView` to pick
/// up. Best-effort: a failed write just means the popup shows stale
/// state, not a reason to fail whatever triggered the state change.
pub fn write_state(state: &PopupState) {
    let path = state_path();
    match serde_json::to_string(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("failed to write popup state to {path:?}: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize popup state: {e}"),
    }
}

/// Actions the popup can send back. `Insert`/`Cancel` apply to plain
/// dictation review; `Approve`/`Deny` apply to a pending command
/// confirmation. Matches the two button bars in the ported popup UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupAction {
    Insert,
    Cancel,
    Approve,
    Deny,
}

impl PopupAction {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "insert" => Some(Self::Insert),
            "cancel" => Some(Self::Cancel),
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Listens on `control_socket_path()` for one-line actions from `novad
/// respond <action>` and forwards them to `sender`. Runs until the
/// listener errors (process teardown); intended to run on its own
/// thread from the daemon's main loop.
pub struct ControlServer;

impl ControlServer {
    /// Spawns the listener thread and returns a channel that yields
    /// each received [`PopupAction`] in order.
    pub fn spawn() -> std::io::Result<mpsc::Receiver<PopupAction>> {
        let path = control_socket_path();
        let _ = std::fs::remove_file(&path); // stale socket from a previous run
        let listener = UnixListener::bind(&path)?;
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                if let Some(action) = read_one_action(stream) {
                    if tx.send(action).is_err() {
                        break; // receiver dropped, daemon is shutting down
                    }
                }
            }
        });

        Ok(rx)
    }
}

fn read_one_action(stream: UnixStream) -> Option<PopupAction> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let action = PopupAction::parse(&line);
    if action.is_none() {
        tracing::warn!("popup control socket got unrecognized action: {line:?}");
    }
    action
}

/// `novad respond <action>` entry point: connect to the running
/// daemon's control socket and send one action, then exit.
pub fn respond(action: &str) -> anyhow::Result<()> {
    if PopupAction::parse(action).is_none() {
        anyhow::bail!("unknown action '{action}' (expected: insert, cancel, approve, deny)");
    }
    let path = control_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| anyhow::anyhow!("connect to {path:?}: {e} (is 'novad detect' running?)"))?;
    writeln!(stream, "{action}")?;
    Ok(())
}
