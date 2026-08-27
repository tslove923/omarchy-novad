// omarchy-novad's `overlay` entry point.
//
// This plugin's port of the standalone quickshell/shell.qml's
// composition root -- previously a `ShellRoot` running as its own
// `qs -p quickshell` process, now a plain host-injected `Item` (see
// e.g. the `io.github.spencerbull.omapilot` plugin's Ambient.qml,
// which is exactly this shape: "the plugin's `overlay` entry point,
// and because the manifest sets `keepLoaded`, Omarchy keeps it live
// from shell start"). `manifest.json` sets `keepLoaded: true` for the
// same reason: this overlay isn't summoned/hidden by the host at all
// (there's no host-level "open" trigger for it) -- it's always
// mounted, and PopupCard/ConversationPanel each independently decide
// when they have something to show, exactly as they did in the
// standalone build.
//
// PopupCard and ConversationPanel each nest their own `PanelWindow` +
// `WlrLayershell` -- declaring a layer-shell surface as a plain child
// item of a plugin's root `Item` is the pattern the shell's own
// first-party overlay plugins use too (see e.g.
// shell/plugins/emojis/Emojis.qml, shell/plugins/reminders/
// ReminderFlow.qml: an outer `Item` root holding a nested
// `PanelWindow`). This is *not* the "second shell process" the plugin
// contract forbids -- both surfaces live inside the one long-running
// `omarchy-shell` process now, the same as every other overlay plugin.
//
// Design decision: one `overlay` entry point for both surfaces rather
// than two. They already coexisted side by side in the standalone
// build (see quickshell/shell.qml's own doc comment: "two independent
// windows, each visible only while it has something to show"), share
// one Service instance, are never both fighting for the same screen
// space (popup centered near the top, conversation docked right), and
// OmaPilot's own Ambient.qml overlay is precedent for one overlay
// entry point owning multiple independent on-screen surfaces. Splitting
// them into two manifest kinds/entry points would only have meant two
// `keepLoaded` Loaders instead of one, for no behavioral difference --
// revisit this if a future surface here ever needs independent
// enable/disable from shell.json.

import QtQuick

Item {
    id: root

    // Injected by the host's panel/overlay loader (see
    // shell/shell.qml's Instantiator delegate, `panelLoader.onLoaded`).
    property var omarchyPath: ""
    property var shell: null
    property var manifest: null
    property var pluginRegistry: null
    property var barWidgetRegistry: null
    // The Service.qml singleton for this plugin -- injected because
    // this manifest also declares kind "service" with a matching
    // entry point (see shell/shell.qml: "Plugins that pair a panel UI
    // with a service entry read shared state off `service`").
    property var service: null

    PopupCard {
        service: root.service
    }

    ConversationPanel {
        service: root.service
    }
}
