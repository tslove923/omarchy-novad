//! Standalone popup state + control channel.
//!
//! Mirrors voxtype's own Quickshell OSD conventions exactly (see
//! `voxtype/quickshell/voxtype-shared/StateReader.qml` and
//! `MeetingControls.qml`), just with richer state than a single word:
//!
//! - **Daemon -> UI**: this module serializes [`PopupState`] to a JSON
//!   file at `$XDG_RUNTIME_DIR/omarchy-novad/popup-state.json` on every state
//!   change. The QML side watches it with `Quickshell.Io.FileView`
//!   (`watchChanges: true`), same mechanism as voxtype's `StateReader`.
//! - **UI -> daemon**: button clicks in the popup run `omarchy-novad respond
//!   <action>` via `Quickshell.Io.Process`, same mechanism as
//!   `MeetingControls.qml`'s `voxtype meeting show` calls. That
//!   subprocess connects to a Unix socket this module's [`ControlServer`]
//!   listens on and sends one line, then exits.
//!
//! No WebSocket, no HTTP, no bearer tokens — nova-npu's Electron app
//! needed those because Electron and the Python service were two
//! separate processes with no shared filesystem convention. omarchy-novad and
//! its own popup are both native to this daemon; a JSON file and a
//! Unix socket are enough.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

/// Mirrors nova's popup.js state machine (`idle`, `listening`,
/// `recording`, `transcribing`, `classifying`, `confirming`, `ready`),
/// plus `handing_off` -- novad-specific, no nova equivalent (nova's
/// OpenClaw handoff reused its "processing"/strobing state; omarchy-novad
/// gives it a distinct phase since it runs meaningfully longer than
/// local classification and deserves its own status label).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupPhase {
    Idle,
    Listening,
    Recording,
    Transcribing,
    Classifying,
    HandingOff,
    Confirming,
    Ready,
}

#[derive(Debug, Clone, Serialize)]
pub struct PopupState {
    pub phase: PopupPhase,
    /// Transcript / AI response text shown in the text area. Empty
    /// string renders the "empty" placeholder state in QML. When
    /// `editable` is true, this is the box's *initial* content -- the
    /// user may change it before Approve (see `PopupAction::Approve`'s
    /// `edited_text`).
    pub text: String,
    /// Set only during `Confirming` — a short header shown above `text`
    /// (e.g. "Text Jessica"), distinct from the body itself. See
    /// `router::RouteResult::NeedsConfirmation`'s `label` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_label: Option<String>,
    /// Whether `text` should render as an editable box rather than
    /// plain read-only text. Only meaningful (and only ever true) during
    /// `Confirming` for a `Message` — see
    /// `router::RouteResult::NeedsConfirmation`'s `editable` field.
    pub editable: bool,
}

impl Default for PopupState {
    fn default() -> Self {
        Self {
            phase: PopupPhase::Idle,
            text: String::new(),
            confirm_label: None,
            editable: false,
        }
    }
}

pub fn state_path() -> PathBuf {
    runtime_dir().join("popup-state.json")
}

pub fn control_socket_path() -> PathBuf {
    runtime_dir().join("popup-control.sock")
}

fn runtime_dir() -> PathBuf {
    // Same fallback shape as the rest of omarchy-novad (cache_dir(), etc.):
    // prefer XDG_RUNTIME_DIR, fall back to the system temp dir rather
    // than trying to reconstruct /run/user/<uid> ourselves.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Write the current popup state to disk for the QML `FileView` to pick
/// up. Best-effort: a failed write just means the popup shows stale
/// state, not a reason to fail whatever triggered the state change.
///
/// Writes to a sibling temp file and `rename()`s it into place rather
/// than truncating `state_path()` in place -- confirmed live (Omarchy
/// plugin packaging) that a plain truncate-and-rewrite of the same
/// inode, done rapidly across the several phase transitions one
/// session produces, can permanently wedge Quickshell's
/// `FileView(watchChanges: true)` inotify watch: it just stops firing
/// reload events for the rest of the session, with no error and no
/// self-recovery -- only deleting and recreating the file (a fresh
/// inode) brought it back. `rename()` gives every write a fresh inode
/// the same way, and also fixes the (previously accepted, see the QML
/// side's own comment) torn-read possibility of a reader catching the
/// file mid-truncate.
pub fn write_state(state: &PopupState) {
    let path = state_path();
    match serde_json::to_string(state) {
        Ok(json) => {
            let tmp_path = path.with_file_name(format!(
                "{}.tmp",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let result = std::fs::write(&tmp_path, json).and_then(|_| std::fs::rename(&tmp_path, &path));
            if let Err(e) = result {
                tracing::warn!("failed to write popup state to {path:?}: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize popup state: {e}"),
    }
}

/// Actions the popup can send back. `Insert`/`Cancel` apply to plain
/// dictation review; `Approve`/`Deny` apply to a pending command
/// confirmation. Matches the two button bars in the ported popup UI.
/// `Approve` carries `edited_text` -- whatever was in the popup's
/// editable box at click time (see `PopupState::editable`), `None` when
/// the confirmation wasn't editable or the user didn't change it. Not
/// `Copy` any more now that a variant owns a `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupAction {
    Insert,
    Cancel,
    Approve { edited_text: Option<String> },
    Deny,
}

impl PopupAction {
    /// Builds the [`PopupAction`] a wire `action` name (+ optional
    /// payload `text`, only meaningful for "approve") refers to, or
    /// `None` if `action` isn't one of the four recognized words.
    fn from_wire(action: &str, text: Option<String>) -> Option<Self> {
        match action.trim() {
            "insert" => Some(Self::Insert),
            "cancel" => Some(Self::Cancel),
            "approve" => Some(Self::Approve { edited_text: text }),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Wire format for a control-socket message: one JSON object per line.
/// `text` only ever carries anything for `action: "approve"` on an
/// editable confirmation (see `PopupState::editable`) -- everything else
/// leaves it `None`. Still accepts a bare action word with no JSON
/// wrapper too (see `read_one_action`), so `printf 'deny\n' | socat -
/// UNIX-CONNECT:$XDG_RUNTIME_DIR/omarchy-novad/popup-control.sock`-style
/// manual testing still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlMessage {
    action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

/// Listens on `control_socket_path()` for one-line actions from `omarchy-novad
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
    let trimmed = line.trim();
    // A bare action word (no JSON) is accepted too -- see
    // `ControlMessage`'s docs.
    let msg: ControlMessage = serde_json::from_str(trimmed).unwrap_or_else(|_| ControlMessage {
        action: trimmed.to_string(),
        text: None,
    });
    let action = PopupAction::from_wire(&msg.action, msg.text);
    if action.is_none() {
        tracing::warn!("popup control socket got unrecognized action: {line:?}");
    }
    action
}

/// `omarchy-novad respond <action>` entry point: connect to the running
/// daemon's control socket and send one action (+ optional edited
/// message text, only meaningful with `approve` on an editable
/// confirmation), then exit.
pub fn respond(action: &str, text: Option<&str>) -> anyhow::Result<()> {
    if PopupAction::from_wire(action, None).is_none() {
        anyhow::bail!("unknown action '{action}' (expected: insert, cancel, approve, deny)");
    }
    let path = control_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!("connect to {path:?}: {e} (is 'omarchy-novad detect' running?)")
    })?;
    let msg = ControlMessage {
        action: action.to_string(),
        text: text.map(String::from),
    };
    writeln!(stream, "{}", serde_json::to_string(&msg)?)?;
    Ok(())
}
