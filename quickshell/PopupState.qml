// omarchy-novad daemon popup-state file watcher.
//
// Same mechanism as voxtype's StateReader.qml (FileView, watchChanges),
// just parsing JSON since omarchy-novad's popup needs more than a
// single state word: { phase, text, confirm_label? }.
//
// Usage:
//   PopupState { id: popupState }
//   Text { text: popupState.text }

import QtQuick
import Quickshell
import Quickshell.Io

QtObject {
    id: root

    property string statePath: {
        const xdg = Quickshell.env("XDG_RUNTIME_DIR");
        const base = (xdg && xdg.length > 0) ? xdg : "/tmp";
        return base + "/omarchy-novad/popup-state.json";
    }

    // Individual properties rather than one "state" object so QML
    // bindings elsewhere (`popupState.phase === "recording"`) stay
    // simple — matches how Theme.qml/StateReader.qml expose plain
    // properties rather than a nested structure.
    property string phase: "idle"
    property string text: ""
    property string confirmLabel: ""
    // Whether `text` should render as an editable box rather than plain
    // read-only text -- true only during "confirming" for a Message (see
    // popup::PopupState::editable on the daemon side).
    property bool editable: false
    // Mirrors config::PopupConfig -- whether "confirming" should run
    // the auto-approve countdown (slider fill + timed "approve"), and
    // how long it takes. See OmarchyNovadPopup.qml's confirm box.
    property bool autoApprove: true
    property real autoApproveTimeoutSecs: 3.0

    property FileView _fileView: FileView {
        path: root.statePath
        watchChanges: true
        printErrors: false

        onLoaded: {
            try {
                const parsed = JSON.parse(text());
                root.phase = parsed.phase || "idle";
                root.text = parsed.text || "";
                root.confirmLabel = parsed.confirm_label || "";
                root.editable = parsed.editable || false;
                root.autoApprove = parsed.auto_approve !== undefined ? parsed.auto_approve : true;
                root.autoApproveTimeoutSecs = parsed.auto_approve_timeout_secs !== undefined
                    ? parsed.auto_approve_timeout_secs : 3.0;
            } catch (e) {
                // Daemon writes the file non-atomically; a torn read
                // during a write is possible and not worth logging.
            }
        }

        onLoadFailed: {
            root.phase = "idle";
            root.text = "";
            root.confirmLabel = "";
            root.editable = false;
            root.autoApprove = true;
            root.autoApproveTimeoutSecs = 3.0;
        }

        onFileChanged: reload()
    }
}
