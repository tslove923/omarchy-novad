// omarchy-novad Quickshell entry point.
//
// Run standalone for testing:
//   qs -p quickshell
//
// Mirrors voxtype's own quickshell/shell.qml shape: thin composition
// root, one file per widget. The standalone assistant popup
// (OmarchyNovadPopup.qml) and the OpenClaw conversation transcript
// window (OpenClawConversation.qml) load side by side here -- two
// independent windows, each visible only while it has something to
// show; a tray/status widget may join them later.

import Quickshell

ShellRoot {
    OmarchyNovadPopup {
        id: popup
    }

    OpenClawConversation {
        id: conversation
    }
}
