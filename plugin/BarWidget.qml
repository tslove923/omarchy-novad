// omarchy-novad's `bar-widget` entry point -- an at-a-glance status
// pill for the wake-word daemon, plus a small click-to-open popup with
// quick controls. Mirrors the `hass` plugin's Panel.qml shape: "Bar
// button plus popup panel. Owns [its own UI state]; the service owns
// [the shared state]" -- extends the shared `Panel` base type from
// `qs.Ui` (see shell/Ui/Panel.qml) the same way hass's Panel.qml does,
// which provides the open/close/toggle lifecycle and an IpcHandler for
// free.
//
// This is intentionally a *small* companion to Overlay.qml's
// PopupCard/ConversationPanel, not a duplicate of them: the full
// review card and conversation transcript already show themselves
// automatically whenever there's something to review (Overlay.qml is
// always mounted). This widget's job is the at-a-glance piece the
// hass template calls out -- a bar-level indicator of whether anything
// is going on -- plus one-click access to the two actions worth a
// shortcut (dismiss a pending confirmation, stop a running
// conversation) without having to find the popup/panel on screen.

import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Ui

Panel {
    id: root

    // `moduleName` is injected by the host bar (see shell/plugins/bar/
    // Bar.qml's `injectProps`) to this widget's canonical plugin id;
    // this default only matters for the (unsupported) case of loading
    // this file outside the bar host, e.g. a qmllint pass.
    moduleName: "io.github.tslove923.omarchy-novad"
    ipcTarget: "omarchy-novad"

    // Service.qml is mounted once per session; this widget is mounted
    // once per monitor, same relationship the hass plugin's Panel.qml
    // documents: "Widgets reach them through `bar.shell.serviceFor(...)`".
    readonly property var novad: bar && bar.shell ? bar.shell.serviceFor(root.moduleName) : null
    readonly property bool serviceReady: novad !== null

    readonly property string popupPhase: serviceReady ? novad.popupPhase : "idle"
    readonly property bool conversationActive: serviceReady && novad.conversationActive
    readonly property string conversationPhase: serviceReady ? novad.conversationPhase : ""
    readonly property string conversationPendingText: serviceReady ? novad.conversationPendingText : ""

    // "Busy" whenever there's anything happening worth a glance: a
    // popup phase other than idle, or an active OpenClaw conversation.
    readonly property bool busy: popupPhase !== "idle" || conversationActive

    readonly property color dotColor: {
        if (conversationActive) {
            switch (conversationPhase) {
            case "listening": return OmarchyTheme.accent;
            case "confirming": return OmarchyTheme.yellow;
            case "thinking": return OmarchyTheme.magenta;
            case "speaking": return OmarchyTheme.green;
            default: return OmarchyTheme.accent;
            }
        }
        switch (popupPhase) {
        case "recording": return OmarchyTheme.red;
        case "transcribing": return OmarchyTheme.accent;
        case "classifying": return OmarchyTheme.magenta;
        case "handing_off": return OmarchyTheme.accent;
        case "confirming": return OmarchyTheme.yellow;
        case "ready": return OmarchyTheme.green;
        default: return OmarchyTheme.muted;
        }
    }

    readonly property string statusLabel: {
        if (conversationActive) {
            switch (conversationPhase) {
            case "listening": return "Listening…";
            case "confirming": return "Confirming…";
            case "thinking": return "Thinking…";
            case "speaking": return "Speaking…";
            default: return "Conversing";
            }
        }
        switch (popupPhase) {
        case "listening": return "Listening…";
        case "recording": return "Recording…";
        case "transcribing": return "Transcribing…";
        case "classifying": return "Thinking…";
        case "handing_off": return "Asking OpenClaw…";
        case "confirming": return "Confirm pending";
        case "ready": return "Ready to insert";
        default: return "Idle";
        }
    }

    implicitWidth: row.implicitWidth + 16
    implicitHeight: 22

    Row {
        id: row
        anchors.centerIn: parent
        spacing: 6

        Rectangle {
            width: 8; height: 8; radius: 4
            color: root.dotColor
            anchors.verticalCenter: parent.verticalCenter

            SequentialAnimation on opacity {
                running: root.busy
                loops: Animation.Infinite
                NumberAnimation { to: 0.5; duration: 600 }
                NumberAnimation { to: 1.0; duration: 600 }
            }
        }

        Text {
            text: "Jarvis"
            color: root.barForeground
            font.pixelSize: 12
            font.weight: Font.Medium
            anchors.verticalCenter: parent.verticalCenter
        }
    }

    MouseArea {
        anchors.fill: parent
        cursorShape: Qt.PointingHandCursor
        onClicked: root.toggle()
    }

    // ── Quick-status popup -- self-managed layer-shell surface, same
    //    pattern PopupCard.qml/ConversationPanel.qml use (a plain
    //    `PanelWindow` child, gated on `root.opened` from the `Panel`
    //    base instead of the host's own KeyboardPanel dropdown helper
    //    -- kept intentionally simple since this popup only ever shows
    //    a couple of lines of status and two buttons). Docks near the
    //    top-right corner rather than tracking the bar icon's exact
    //    position (`qs.Ui.KeyboardPanel` does that properly for
    //    first-party widgets, at the cost of real integration with the
    //    bar's own popout-coordination internals) -- reasonable for a
    //    single small third-party widget, revisit if that ever feels
    //    imprecise in practice. ──
    PanelWindow {
        id: statusPopup
        visible: root.opened

        anchors { top: true; right: true }
        color: "transparent"
        exclusionMode: ExclusionMode.Ignore

        WlrLayershell.namespace: "omarchy-novad-bar-status"
        WlrLayershell.layer: WlrLayer.Overlay
        WlrLayershell.keyboardFocus: WlrKeyboardFocus.None

        implicitWidth: card.width + 24
        implicitHeight: card.height + 24

        mask: Region {
            x: card.x
            y: card.y
            width: card.width
            height: card.height
        }

        Rectangle {
            id: card
            width: 280
            implicitHeight: column.implicitHeight + 24
            height: implicitHeight
            x: 12
            y: 12
            radius: 10
            color: OmarchyTheme.background

            MouseArea { anchors.fill: parent } // swallow clicks so they don't fall through

            Column {
                id: column
                width: parent.width - 28
                x: 14
                y: 12
                spacing: 8

                Row {
                    spacing: 8
                    Rectangle {
                        width: 8; height: 8; radius: 4
                        color: root.dotColor
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    Text {
                        text: root.statusLabel
                        color: OmarchyTheme.foreground
                        font.pixelSize: 13
                        font.weight: Font.Medium
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }

                Text {
                    width: parent.width
                    text: root.conversationActive && root.conversationPendingText.length > 0
                        ? root.conversationPendingText
                        : (root.serviceReady && root.novad.popupText.length > 0 ? root.novad.popupText : "Nothing pending.")
                    color: OmarchyTheme.muted
                    font.pixelSize: 12
                    wrapMode: Text.Wrap
                    maximumLineCount: 4
                    elide: Text.ElideRight
                }

                Row {
                    spacing: 6
                    anchors.right: parent.right

                    PopupButton {
                        label: "Dismiss"
                        tint: OmarchyTheme.red
                        visible: root.popupPhase !== "idle"
                        onClicked: if (root.serviceReady) root.novad.respond("deny")
                    }
                    PopupButton {
                        label: "Stop conversation"
                        tint: OmarchyTheme.red
                        visible: root.conversationActive
                        onClicked: if (root.serviceReady) root.novad.stopConversation()
                    }
                }
            }
        }
    }
}
