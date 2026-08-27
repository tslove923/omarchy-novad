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
    // "listening" | "confirming" | "thinking" | "speaking" | "" (absent/no phase).
    property string conversationPhase: ""
    // The just-transcribed utterance awaiting "does this look good?"
    // confirmation, or "" when there's nothing pending -- only
    // meaningful while conversationPhase === "confirming".
    property string conversationPendingText: ""
    // Array of { user_text, full_response, spoken_summary } objects,
    // oldest first -- see src/conversation/mod.rs's ConversationTurn.
    property var conversationTurns: []
    // Skips per-turn "does this look good?" confirmation when on -- see
    // src/conversation/mod.rs's ConversationState::hands_free.
    property bool conversationHandsFree: false

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
                root.conversationHandsFree = parsed.hands_free || false;
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
            root.conversationHandsFree = false;
        }

        onFileChanged: reload()
    }

    // Starts the OpenClaw voice-conversation loop -- see
    // src/conversation/mod.rs's ConverseCommand::Start. The
    // ConversationPanel (Overlay.qml) shows itself automatically the
    // moment `conversationActive` flips true, so this is also the
    // real equivalent of "open the chat window": there is no separate
    // "show the panel" action, starting the loop *is* what makes it
    // appear. Added alongside the pre-existing `stopConversation()` so
    // BarWidget's context menu (nova's ported "OpenClaw Chat" item)
    // can toggle the loop on/off through one Service action pair,
    // same as every other daemon action here.
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

    function setHandsFree(enabled) {
        _converseControlProcess.command = [root.novadBinary, "converse", "hands-free", enabled ? "on" : "off"];
        _converseControlProcess.running = true;
    }

    property Process _converseStartProcess: Process { running: false }
    // Reused across stop/confirm/reject -- each is a quick one-shot
    // CLI call (connects to the running session's control socket,
    // sends one action, exits), never overlapping with another one in
    // practice (a human can't click Confirm and Reject in the same
    // instant), unlike _converseStartProcess above.
    property Process _converseControlProcess: Process { running: false }
}
