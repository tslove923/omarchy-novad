// omarchy-novad's Omarchy plugin service.
//
// Owner of all shared omarchy-novad UI state -- a `service` is mounted
// once per session (see shell/services/PluginRegistry.qml's kind
// table and shell/shell.qml's `ensureService`), so the daemon's two
// state files live here, same shape as the `hass` plugin's
// Service.qml: "a `service` is mounted once per session, a
// `bar-widget` once per monitor... Widgets reach them through
// `bar.shell.serviceFor("hass")`". BarWidget.qml does exactly that;
// Overlay.qml gets `service` injected directly by the panel/overlay
// loader instead (shell.qml: `if ("service" in item) item.service =
// shell.serviceFor(panelEntry.pluginId)`).
//
// State comes from the same JSON files the daemon already writes for
// the standalone quickshell/ dev harness -- see
// quickshell/PopupState.qml and quickshell/ConversationState.qml,
// which this supersedes for the plugin build (that standalone harness
// keeps working unmodified for `qs -p quickshell` dev/testing; see
// this plugin's README for how the two relate).
//
// Actions go back out via `omarchy-novad respond <action>` / `omarchy-novad
// converse <action>` through Quickshell.Io.Process, same mechanism the
// standalone popup used (MeetingControls.qml-style: run a short-lived
// subprocess that talks to the daemon's own Unix control socket, see
// src/popup/mod.rs and src/conversation/mod.rs's module docs).

import QtQuick
import Quickshell
import Quickshell.Io

QtObject {
    id: root

    // Overridable if `omarchy-novad` isn't on the shell process's PATH
    // for some reason -- see the plugin README's troubleshooting note
    // (same caveat the main README documents for the standalone popup).
    property string novadBinary: "omarchy-novad"

    readonly property string runtimeDir: {
        const xdg = Quickshell.env("XDG_RUNTIME_DIR");
        return (xdg && xdg.length > 0) ? xdg : "/tmp";
    }

    // ────────────────────────── side-panel visibility ──────────────────────────
    // Whether the docked ConversationPanel is shown. Pure UI state (the
    // daemon neither knows nor cares about it) -- it lives here so the
    // bar widget's tray-icon click and the panel itself read/write one
    // shared value. Hidden by default: the panel only appears on a novad
    // activation (a conversation starting or a turn entering "confirming"
    // -- see the transition detection in _conversationStateView.onLoaded),
    // a tray-icon click, or the SUPER+H key bind (which drives the host's
    // `toggle` IPC through Overlay.qml's open()/close()). It never shows
    // just because the shell started.
    property bool panelVisible: false

    function togglePanel() {
        root.panelVisible = !root.panelVisible;
    }

    // ────────────────────────────── popup state ──────────────────────────────
    // Dictation review / command confirmation -- see src/popup/mod.rs's
    // PopupPhase. Mirrors quickshell/PopupState.qml's fields exactly.

    property string popupPhase: "idle"
    property string popupText: ""
    property string popupConfirmLabel: ""
    // Whether `popupText` should render as an editable box rather than
    // plain read-only text -- true only during "confirming" for a
    // Message (see src/popup/mod.rs's PopupState::editable).
    property bool popupEditable: false

    readonly property bool popupHasContent: popupPhase !== "idle"

    property FileView _popupStateView: FileView {
        path: root.runtimeDir + "/omarchy-novad/popup-state.json"
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                const parsed = JSON.parse(text());
                root.popupPhase = parsed.phase || "idle";
                root.popupText = parsed.text || "";
                root.popupConfirmLabel = parsed.confirm_label || "";
                root.popupEditable = parsed.editable || false;
            } catch (e) {
                // Daemon writes the file non-atomically; a torn read
                // during a write is possible and not worth logging.
            }
        }

        onLoadFailed: {
            root.popupPhase = "idle";
            root.popupText = "";
            root.popupConfirmLabel = "";
            root.popupEditable = false;
        }

        onFileChanged: reload()
    }

    // action: "insert" | "cancel" | "approve" | "deny" -- see
    // src/main.rs's `Respond` subcommand. `text`, when given, overrides
    // the parsed body before it goes out (only meaningful with
    // "approve" on an editable confirmation).
    function respond(action, text) {
        _respondProcess.command = (text !== undefined && text !== null)
            ? [root.novadBinary, "respond", action, "--text", text]
            : [root.novadBinary, "respond", action];
        _respondProcess.running = true;
    }

    property Process _respondProcess: Process { running: false }

    // ──────────────────────── conversation state (OpenClaw) ────────────────────────
    // Multi-turn spoken conversation loop -- see src/conversation/mod.rs's
    // ConversationState/ConversationPhase. Mirrors
    // quickshell/ConversationState.qml's fields exactly.

    property bool conversationActive: false
    // "listening" | "confirming" | "thinking" | "speaking" | "" (idle,
    // waiting for the user to trigger a recording, or absent/no phase
    // before the very first turn).
    property string conversationPhase: ""
    // The just-transcribed utterance awaiting review/send, or "" when
    // there's nothing pending -- only meaningful while
    // conversationPhase === "confirming".
    property string conversationPendingText: ""
    // Array of { user_text, full_response, spoken_summary } objects,
    // oldest first -- see src/conversation/mod.rs's ConversationTurn.
    property var conversationTurns: []
    // Seconds elapsed on the current OpenClaw handoff, or -1 when not
    // thinking -- see src/conversation/mod.rs's
    // ConversationState::thinking_elapsed_secs. The call has no
    // timeout, so this is the only sign of life the panel can show
    // during a long-running agent turn.
    property int conversationThinkingElapsedSecs: -1
    // The live, incrementally-streamed text of the current OpenClaw
    // reply, or "" when nothing is streaming -- see
    // src/conversation/mod.rs's ConversationState::streaming_text. The
    // panel renders this in place of a bare "Thinking…" so output
    // appears as the model produces it, not all at once when the turn
    // finishes.
    property string conversationStreamingText: ""

    // Previous-turn state for the auto-show transition detection in
    // _conversationStateView.onLoaded -- the panel pops open on a
    // *transition* (conversation starting, or a turn entering
    // "confirming"), not on a level, so an explicit tray/key-bind hide
    // sticks until the next activation.
    property bool _prevConversationActive: false
    property string _prevConversationPhase: ""

    readonly property var latestTurn: conversationTurns.length > 0
        ? conversationTurns[conversationTurns.length - 1] : null

    property FileView _conversationStateView: FileView {
        path: root.runtimeDir + "/omarchy-novad/conversation-state.json"
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                const parsed = JSON.parse(text());
                root.conversationActive = parsed.active || false;
                root.conversationPhase = parsed.phase || "";
                root.conversationPendingText = parsed.pending_text || "";
                root.conversationTurns = parsed.turns || [];
                root.conversationThinkingElapsedSecs = (parsed.thinking_elapsed_secs !== undefined
                    && parsed.thinking_elapsed_secs !== null) ? parsed.thinking_elapsed_secs : -1;
                root.conversationStreamingText = parsed.streaming_text || "";
                // Auto-show on a novad activation: a conversation starting
                // or a turn entering "confirming". Transition-based (not
                // level-based) so an explicit tray/key-bind hide sticks
                // until the *next* activation -- a running conversation
                // alone doesn't keep re-popping the panel.
                const wasActive = root._prevConversationActive;
                const wasPhase = root._prevConversationPhase;
                root._prevConversationActive = root.conversationActive;
                root._prevConversationPhase = root.conversationPhase;
                if ((root.conversationActive && !wasActive)
                    || (root.conversationPhase === "confirming" && wasPhase !== "confirming")) {
                    root.panelVisible = true;
                }
            } catch (e) {
                // Same non-atomic-write caveat as popup-state.json above.
            }
        }

        // File doesn't exist yet (no conversation has ever started) --
        // same as { active: false, turns: [] } per the state contract.
        onLoadFailed: {
            root.conversationActive = false;
            root.conversationPhase = "";
            root.conversationPendingText = "";
            root.conversationTurns = [];
            root.conversationThinkingElapsedSecs = -1;
            root.conversationStreamingText = "";
            root._prevConversationActive = false;
            root._prevConversationPhase = "";
        }

        onFileChanged: reload()
    }

    // Starts the OpenClaw voice-conversation loop -- see
    // src/conversation/mod.rs's ConverseCommand::Start. The
    // ConversationPanel (Overlay.qml) is a permanent docked window
    // (chat box always present, shown on a novad activation / tray /
    // key bind), so this just flips the daemon loop on -- typing in the
    // panel's chat box while no loop is running does the same thing via
    // `converse start --text` (see `sendText`). Added alongside the
    // pre-existing `stopConversation()` so BarWidget's context menu
    // (nova's ported "OpenClaw Chat" item) can toggle the loop on/off
    // through one Service action pair, same as every other daemon action
    // here.
    function startConversation() {
        // Own Process instance -- `converse start` is the long-running
        // loop itself (blocks until `converse stop`/Ctrl+C, same
        // process embodies the whole session), so it can't share a
        // Process with the one-shot stop/confirm/reject commands below.
        // Found live: it used to share one `_converseProcess` with all
        // four actions, so once `start`'s process was running, setting
        // `.running = true` again for a Confirm/Reject/Stop click was a
        // no-op on an already-running Process -- the click's command
        // never actually spawned. That's why Confirm/Stop appeared to
        // do nothing from the panel/tray even though the CLI itself
        // worked fine.
        _converseStartProcess.command = [root.novadBinary, "converse", "start"];
        _converseStartProcess.running = true;
    }

    function stopConversation() {
        _converseControlProcess.command = [root.novadBinary, "converse", "stop"];
        _converseControlProcess.running = true;
    }

    // Confirms (optionally with edited text) the pending transcript --
    // see src/conversation/mod.rs's ConversationAction.
    function confirmPending(text) {
        _converseControlProcess.command = (text !== undefined && text !== null && text.length > 0)
            ? [root.novadBinary, "converse", "confirm", "--text", text]
            : [root.novadBinary, "converse", "confirm"];
        _converseControlProcess.running = true;
    }

    function rejectPending() {
        _converseControlProcess.command = [root.novadBinary, "converse", "reject"];
        _converseControlProcess.running = true;
    }

    // Starts a new recording for the next turn -- the running loop
    // never starts one on its own, only when this is called (e.g. a
    // "Record" button).
    function startListening() {
        _converseControlProcess.command = [root.novadBinary, "converse", "listen"];
        _converseControlProcess.running = true;
    }

    // Ends an in-progress recording early -- a "toggle" button while
    // conversationPhase === "listening", same effect as voxtype's own
    // silence-timeout just user-triggered.
    function stopListening() {
        _converseControlProcess.command = [root.novadBinary, "converse", "stop-listening"];
        _converseControlProcess.running = true;
    }

    // Sends a typed chat-box message as the next turn's utterance --
    // see src/conversation/mod.rs's ConversationAction::SendText. If
    // the loop is already running, the message goes to its control
    // socket (`converse send-text`); if not, starting the loop with
    // `--text` seeds the first turn with it (see src/converse.rs's
    // `run`'s `initial_utterance`). Either way the typed text skips
    // the recording step and lands in the same review/edit step as a
    // transcript.
    function sendText(text) {
        if (root.conversationActive) {
            _converseControlProcess.command = [root.novadBinary, "converse", "send-text", "--text", text];
            _converseControlProcess.running = true;
        } else {
            _converseStartProcess.command = [root.novadBinary, "converse", "start", "--text", text];
            _converseStartProcess.running = true;
        }
    }

    property Process _converseStartProcess: Process { running: false }
    // Reused across stop/confirm/reject/listen/stop-listening -- each
    // is a quick one-shot CLI call (connects to the running session's
    // control socket, sends one action, exits), never overlapping with
    // another one in practice (a human can't click two of these
    // buttons in the same instant), unlike _converseStartProcess above.
    property Process _converseControlProcess: Process { running: false }
}
