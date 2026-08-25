// omarchy-novad Quickshell entry point.
//
// Run standalone for testing:
//   qs -p quickshell
//
// Mirrors voxtype's own quickshell/shell.qml shape: thin composition
// root, one file per widget. Currently just the standalone assistant
// popup (OmarchyNovadPopup.qml); a tray/status widget may join it
// later.

import Quickshell

ShellRoot {
    OmarchyNovadPopup {
        id: popup
    }
}
