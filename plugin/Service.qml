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
    // activation (a conversation starting or a turn entering "thinking"
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
    // Mirrors config::PopupConfig -- whether "confirming" should run
    // the auto-approve countdown (slider fill + timed "approve"), and
    // how long it takes. See PopupCard.qml's confirm box.
    property bool popupAutoApprove: true
    property real popupAutoApproveTimeoutSecs: 3.0

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
                root.popupAutoApprove = parsed.auto_approve !== undefined ? parsed.auto_approve : true;
                root.popupAutoApproveTimeoutSecs = parsed.auto_approve_timeout_secs !== undefined
                    ? parsed.auto_approve_timeout_secs : 3.0;
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
            root.popupAutoApprove = true;
            root.popupAutoApproveTimeoutSecs = 3.0;
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
    // "listening" | "thinking" | "speaking" | "" (idle, waiting for
    // the user to trigger a recording, or absent/no phase before the
    // very first turn).
    property string conversationPhase: ""
    // The current turn's utterance, already sent to OpenClaw -- shown
    // as the outgoing chat bubble immediately, before the reply lands
    // (there's no review/confirm step any more, see
    // src/converse.rs's doc comment). "" once the turn completes
    // (folded into conversationTurns) or while idle/listening.
    property string conversationPendingUserText: ""
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
    // The bare crate::sessions key this loop is using -- see
    // src/conversation/mod.rs's ConversationState::session_key. "" when
    // no loop has ever run this login session. Lets the session
    // picker (below) highlight which entry is the live one.
    property string conversationSessionKey: ""

    // Previous-turn state for the auto-show transition detection in
    // _conversationStateView.onLoaded -- the panel pops open on a
    // *transition* (conversation starting, or a turn entering
    // "thinking"), not on a level, so an explicit tray/key-bind hide
    // sticks until the next activation.
    property bool _prevConversationActive: false
    property string _prevConversationPhase: ""
    // Whether _conversationStateView has completed its first real load
    // yet -- the auto-show trigger below is skipped entirely for that
    // one load (just syncs the prev-state trackers to the file's
    // actual contents, silently). Found live: conversation-state.json
    // can be left with `active: true` by a `converse start` process
    // that didn't exit cleanly (a crash, or the shell/machine going
    // down mid-conversation, rather than a real `converse stop`) --
    // with no guard, every quickshell restart after that reads the
    // same stale `active: true`, sees `_prevConversationActive`'s
    // freshly-defaulted `false`, and treats stale leftover state as a
    // brand new activation, popping the panel open on every single
    // load with nothing behind it. A transition can only be real once
    // this instance has already observed a baseline to transition
    // *from* -- the very first load establishes that baseline, it
    // doesn't witness a transition.
    property bool _conversationStateEverLoaded: false

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
                root.conversationPendingUserText = parsed.pending_user_text || "";
                root.conversationTurns = parsed.turns || [];
                root.conversationThinkingElapsedSecs = (parsed.thinking_elapsed_secs !== undefined
                    && parsed.thinking_elapsed_secs !== null) ? parsed.thinking_elapsed_secs : -1;
                root.conversationStreamingText = parsed.streaming_text || "";
                root.conversationSessionKey = parsed.session_key || "";
                // Auto-show on a novad activation. Transition-based (not
                // level-based) so an explicit tray/key-bind hide sticks
                // until the *next* activation -- a running conversation
                // alone doesn't keep re-popping the panel. Found live: a
                // long OpenClaw turn is worth dismissing (panel +
                // VoiceVisualizer.qml's ambient node both key off
                // panelVisible) and getting back to whatever else you were
                // doing, as long as it reliably comes back once there's
                // something to look at -- so this fires on every point
                // that's true, not just "a turn was just sent":
                //   - a conversation starting
                //   - a turn entering "thinking" (transcript/message sent)
                //   - a turn entering "speaking" (the reply is ready)
                //   - back to idle mid-conversation after thinking/speaking
                //     (nothing left to show -- the loop is now waiting on
                //     you to trigger the next turn)
                const wasActive = root._prevConversationActive;
                const wasPhase = root._prevConversationPhase;
                const isFirstLoad = !root._conversationStateEverLoaded;
                root._conversationStateEverLoaded = true;
                root._prevConversationActive = root.conversationActive;
                root._prevConversationPhase = root.conversationPhase;
                if (!isFirstLoad) {
                    const enteredThinking = root.conversationPhase === "thinking" && wasPhase !== "thinking";
                    const enteredSpeaking = root.conversationPhase === "speaking" && wasPhase !== "speaking";
                    const backToIdleAwaitingInput = root.conversationActive && root.conversationPhase === ""
                        && (wasPhase === "thinking" || wasPhase === "speaking");
                    if ((root.conversationActive && !wasActive)
                        || enteredThinking || enteredSpeaking || backToIdleAwaitingInput) {
                        root.panelVisible = true;
                    }
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
            root.conversationPendingUserText = "";
            root.conversationTurns = [];
            root.conversationThinkingElapsedSecs = -1;
            root.conversationStreamingText = "";
            root.conversationSessionKey = "";
            root._prevConversationActive = false;
            root._prevConversationPhase = "";
            root._conversationStateEverLoaded = false;
        }

        onFileChanged: reload()
    }

    // ─────────────────────── session picker (OpenClaw) ───────────────────────
    // Recent OpenClaw sessions this daemon has created -- see
    // src/sessions.rs's doc comment for why *something* has to track
    // these locally (the gateway has no "list sessions" of its own).
    // Recency-ordered, most recently active first, same order the JSON
    // file itself is already written in.

    // Array of { key, label, created_at_ms, last_active_ms }.
    property var sessionList: []

    property FileView _sessionsView: FileView {
        path: root.runtimeDir + "/omarchy-novad/sessions.json"
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                root.sessionList = JSON.parse(text()) || [];
            } catch (e) {
                // Same non-atomic-write caveat as the other state files.
            }
        }

        onLoadFailed: {
            // No conversation has ever started this login session.
            root.sessionList = [];
        }

        onFileChanged: reload()
    }

    // Stops the running loop (if any) and starts a fresh one pointed at
    // `key` -- the session picker's "resume this one" action. A
    // `converse stop` only asks the loop to end after its current turn
    // (see conversation::stop's doc comment), so this can't just fire
    // both commands back to back: `_sessionSwitchPoll` below waits for
    // `conversationActive` to actually drop before starting the new
    // loop, same "poll until it's really true" shape
    // `autoApproveTicker` (PopupCard.qml) uses for its own timed state.
    // Picking the *current* session, or picking one while nothing is
    // running, just starts it immediately -- no stop to wait for.
    function switchToSession(key) {
        if (root.conversationActive && root.conversationSessionKey !== key) {
            root._pendingSessionSwitch = key;
            root.stopConversation();
            _sessionSwitchPoll.running = true;
        } else {
            root._startConversationWithSession(key);
        }
    }

    property string _pendingSessionSwitch: ""

    property Timer _sessionSwitchPoll: Timer {
        interval: 150
        repeat: true
        onTriggered: {
            if (!root.conversationActive) {
                _sessionSwitchPoll.running = false;
                const key = root._pendingSessionSwitch;
                root._pendingSessionSwitch = "";
                if (key === "") {
                    root.startConversation(); // "mint fresh" -- see startNewSession
                } else {
                    root._startConversationWithSession(key);
                }
            }
        }
    }

    function _startConversationWithSession(key) {
        _converseStartProcess.command = [root.novadBinary, "converse", "start", "--session", key];
        _converseStartProcess.running = true;
    }

    // The picker's "New Session" entry -- same stop-then-start shape as
    // switchToSession, just with no --session flag so the daemon mints
    // a fresh one (see sessions::new_session). Distinct from
    // startConversation() below: that one's for BarWidget's "start the
    // loop" toggle when nothing is running yet and never needs to stop
    // an existing one first.
    function startNewSession() {
        if (root.conversationActive) {
            root._pendingSessionSwitch = ""; // "" means "mint fresh" to the poll handler below
            root.stopConversation();
            _sessionSwitchPoll.running = true;
        } else {
            root.startConversation();
        }
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
        // Process with the one-shot control commands below. Found
        // live: it used to share one `_converseProcess` with all
        // actions, so once `start`'s process was running, setting
        // `.running = true` again for a Stop click was a no-op on an
        // already-running Process -- the click's command never
        // actually spawned. That's why Stop appeared to do nothing
        // from the panel/tray even though the CLI itself worked fine.
        _converseStartProcess.command = [root.novadBinary, "converse", "start"];
        _converseStartProcess.running = true;
    }

    function stopConversation() {
        _converseControlProcess.command = [root.novadBinary, "converse", "stop"];
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
    // the recording step and is sent immediately, same as any other
    // turn.
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
    // Reused across stop/listen/stop-listening/send-text -- each
    // is a quick one-shot CLI call (connects to the running session's
    // control socket, sends one action, exits), never overlapping with
    // another one in practice (a human can't click two of these
    // buttons in the same instant), unlike _converseStartProcess above.
    property Process _converseControlProcess: Process { running: false }
}
