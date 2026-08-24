// novad standalone assistant popup — QML port of nova-npu's Electron
// popup (electron/renderer/popup.{html,css,js}).
//
// Same layer-shell + visibility-toggle pattern as voxtype's
// EnginePicker.qml: a full-anchored PanelWindow that's only actually
// visible (and only then receiving input) while there's something to
// show, so it never intercepts clicks elsewhere on screen.
//
// State comes from PopupState (a JSON file the daemon rewrites on every
// transition); actions go back out via `novad respond <action>` run
// through Quickshell.Io.Process, same mechanism as MeetingControls.qml.
//
// The animated conic-gradient border from nova's CSS (rotate while
// recording, strobe while transcribing, flash on ready) is intentionally
// simplified here to a color + pulse animation rather than a true
// rotating conic gradient — safer bet without live-testing exotic Shape
// gradients against this system's actual Quickshell/Qt version. Revisit
// once this is confirmed working end to end.

import QtQuick
import Quickshell
import Quickshell.Wayland
import Quickshell.Io

PanelWindow {
    id: root

    PopupState {
        id: popupState
    }

    readonly property bool hasContent: popupState.phase !== "idle"
    visible: hasContent

    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore

    WlrLayershell.namespace: "novad-popup"
    WlrLayershell.layer: WlrLayer.Overlay
    // Never grab keyboard focus — the popup only needs mouse clicks on
    // its own buttons, and OnDemand focus with no dismiss key/click-away
    // handling left it stealing input with no way to get it back short
    // of killing the process. Real bug, found the hard way.
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.None

    // Restrict the actually-interactive input region to the card itself
    // — without this, the full-screen anchor above (needed to position
    // the card via anchors.horizontalCenter/bottom below) makes the
    // ENTIRE screen swallow clicks while the popup is visible, not just
    // the small area actually drawn. Everything outside `card`'s bounds
    // must stay click-through to whatever's underneath.
    mask: Region {
        x: card.x
        y: card.y
        width: card.width
        height: card.height
    }

    // ── Palette (Catppuccin Mocha, matching nova's original popup —
    //    not Omarchy-theme-driven yet; see roadmap for follow-up) ──
    readonly property color bgColor: "#1e1e2e"
    readonly property color textColor: "#cdd6f4"
    readonly property color labelColor: "#a6adc8"
    readonly property color emptyColor: "#6c7086"
    readonly property color buttonBg: "#313244"
    readonly property color buttonBorder: "#45475a"
    readonly property color danger: "#f38ba8"
    readonly property color approve: "#a6e3a1"
    readonly property color accent: "#89b4fa"

    readonly property color phaseColor: {
        switch (popupState.phase) {
        case "recording": return "#f38ba8";
        case "transcribing": return "#89b4fa";
        case "classifying": return "#cba6f7";
        case "confirming": return "#fab387";
        case "ready": return "#a6e3a1";
        default: return "#45475a";
        }
    }

    readonly property string phaseLabel: {
        switch (popupState.phase) {
        case "listening": return "Listening…";
        case "recording": return "Recording…";
        case "transcribing": return "Transcribing…";
        case "classifying": return "Thinking…";
        case "confirming": return "Confirm";
        case "ready": return "Ready";
        default: return "";
        }
    }

    function respond(action) {
        respondProcess.command = [novadBinary, "respond", action];
        respondProcess.running = true;
    }

    property string novadBinary: "novad"

    Process {
        id: respondProcess
        running: false
    }

    Rectangle {
        id: card
        width: 420
        implicitHeight: contentColumn.implicitHeight + 24
        height: implicitHeight
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 48

        radius: 10
        color: root.bgColor
        border.width: 2
        border.color: root.phaseColor

        Behavior on border.color {
            ColorAnimation { duration: 250 }
        }
        Behavior on implicitHeight {
            NumberAnimation { duration: 150; easing.type: Easing.OutQuad }
        }

        // Pulse the border while something is actively happening —
        // stands in for nova's rotating/strobing conic gradient.
        SequentialAnimation on opacity {
            running: ["recording", "transcribing", "classifying"].includes(popupState.phase)
            loops: Animation.Infinite
            NumberAnimation { to: 0.55; duration: 550; easing.type: Easing.InOutQuad }
            NumberAnimation { to: 1.0; duration: 550; easing.type: Easing.InOutQuad }
        }

        Column {
            id: contentColumn
            width: parent.width - 32
            x: 16
            y: 12
            spacing: 8

            // ── Status bar ──
            Row {
                spacing: 8
                height: 20

                Rectangle {
                    width: 8; height: 8; radius: 4
                    color: root.phaseColor
                    anchors.verticalCenter: parent.verticalCenter

                    SequentialAnimation on opacity {
                        running: ["recording", "transcribing", "classifying", "confirming"].includes(popupState.phase)
                        loops: Animation.Infinite
                        NumberAnimation { to: 0.5; duration: 600 }
                        NumberAnimation { to: 1.0; duration: 600 }
                    }
                }

                Text {
                    text: root.phaseLabel
                    color: root.labelColor
                    font.pixelSize: 12
                    font.weight: Font.Medium
                    font.letterSpacing: 0.5
                    anchors.verticalCenter: parent.verticalCenter
                }
            }

            // ── Transcript / response text ──
            Text {
                width: parent.width
                text: popupState.text.length > 0 ? popupState.text : "…"
                color: popupState.text.length > 0 ? root.textColor : root.emptyColor
                font.family: "JetBrains Mono"
                font.pixelSize: 14
                wrapMode: Text.Wrap
                visible: popupState.phase !== "idle"
            }

            // ── Confirm bar (Approve / Deny) ──
            Row {
                spacing: 6
                anchors.right: parent.right
                visible: popupState.phase === "confirming"
                width: visible ? implicitWidth : 0

                PopupButton {
                    label: "Deny"
                    tint: root.danger
                    onClicked: root.respond("deny")
                }
                PopupButton {
                    label: "Approve"
                    tint: root.approve
                    primary: true
                    onClicked: root.respond("approve")
                }
            }

            // ── Review bar (Insert / Cancel) — shown when there's a
            //    finished transcript to act on outside of a pending
            //    command confirmation. ──
            Row {
                spacing: 6
                anchors.right: parent.right
                visible: popupState.phase === "ready" && popupState.text.length > 0
                width: visible ? implicitWidth : 0

                PopupButton {
                    label: "Cancel"
                    tint: root.danger
                    onClicked: root.respond("cancel")
                }
                PopupButton {
                    label: "Insert"
                    tint: root.accent
                    primary: true
                    onClicked: root.respond("insert")
                }
            }
        }
    }
}
