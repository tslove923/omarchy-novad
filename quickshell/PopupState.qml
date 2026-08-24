// novad daemon popup-state file watcher.
//
// Same mechanism as voxtype's StateReader.qml (FileView, watchChanges),
// just parsing JSON since novad's popup needs more than a single state
// word: { phase, text, confirm_label? }.
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
        return base + "/novad/popup-state.json";
    }

    // Individual properties rather than one "state" object so QML
    // bindings elsewhere (`popupState.phase === "recording"`) stay
    // simple — matches how Theme.qml/StateReader.qml expose plain
    // properties rather than a nested structure.
    property string phase: "idle"
    property string text: ""
    property string confirmLabel: ""

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
            } catch (e) {
                // Daemon writes the file non-atomically; a torn read
                // during a write is possible and not worth logging.
            }
        }

        onLoadFailed: {
            root.phase = "idle";
            root.text = "";
            root.confirmLabel = "";
        }

        onFileChanged: reload()
    }
}
