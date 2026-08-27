// omarchy-novad conversation-state file watcher.
//
// Same FileView + watchChanges + onLoaded pattern as PopupState.qml,
// just pointed at conversation-state.json (see src/conversation/mod.rs's
// module doc comment for the full daemon <-> UI contract) instead of
// popup-state.json.
//
// Usage:
//   ConversationState { id: conversationState }
//   Text { text: conversationState.phase }

import QtQuick
import Quickshell
import Quickshell.Io

QtObject {
    id: root

    property string statePath: {
        const xdg = Quickshell.env("XDG_RUNTIME_DIR");
        const base = (xdg && xdg.length > 0) ? xdg : "/tmp";
        return base + "/omarchy-novad/conversation-state.json";
    }

    property bool active: false
    // "listening" | "confirming" | "thinking" | "speaking" | "" (absent/no phase).
    property string phase: ""
    // The just-transcribed utterance awaiting "does this look good?"
    // confirmation, or "" when there's nothing pending -- only
    // meaningful while phase === "confirming".
    property string pendingText: ""
    // Array of { user_text, full_response, spoken_summary } objects,
    // oldest first -- see src/conversation/mod.rs's ConversationTurn.
    property var turns: []

    property FileView _fileView: FileView {
        path: root.statePath
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                const parsed = JSON.parse(text());
                root.active = parsed.active || false;
                root.phase = parsed.phase || "";
                root.pendingText = parsed.pending_text || "";
                root.turns = parsed.turns || [];
            } catch (e) {
                // Daemon writes the file non-atomically; a torn read
                // during a write is possible and not worth logging.
            }
        }

        // File doesn't exist yet (no conversation has ever started) --
        // same as { active: false, turns: [] } per the state contract.
        onLoadFailed: {
            root.active = false;
            root.phase = "";
            root.pendingText = "";
            root.turns = [];
        }

        onFileChanged: reload()
    }
}
