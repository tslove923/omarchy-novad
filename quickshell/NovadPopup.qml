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
// recording, strobe while transcribing, flash on ready) is ported for
// real via AnimatedBorder.qml (Qt5Compat.GraphicalEffects — confirmed
// present on this system's Quickshell/Qt build), not simplified.

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
    // the card via anchors.horizontalCenter/top below) makes the
    // ENTIRE screen swallow clicks while the popup is visible, not just
    // the small area actually drawn. Everything outside `card`'s bounds
    // must stay click-through to whatever's underneath. (The border's
    // glow paints outside this region too, same as nova's CSS box-shadow
    // did — a glow was never meant to be clickable.)
    mask: Region {
        x: card.x
        y: card.y
        width: card.width
        height: card.height
    }

    // ── Palette: OmarchyTheme.qml (~/.local/state/omarchy/current/
    //    theme/colors.toml, live-reloaded on `omarchy theme set`),
    //    falling back to Catppuccin Mocha -- nova's original popup
    //    palette -- when no Omarchy theme is readable. Only the
    //    colors.toml has a real per-theme equivalent for are pulled
    //    from it; labelColor/emptyColor/buttonBg/buttonBorder are
    //    derived shades of the theme's own background/foreground so
    //    they track the active theme's brightness without needing a
    //    dedicated TOML key for each.
    readonly property color bgColor: OmarchyTheme.background
    readonly property color textColor: OmarchyTheme.foreground
    readonly property color labelColor: Qt.lighter(OmarchyTheme.background, 2.2)
    readonly property color emptyColor: Qt.lighter(OmarchyTheme.background, 1.8)
    readonly property color buttonBg: Qt.lighter(OmarchyTheme.background, 1.4)
    readonly property color buttonBorder: Qt.lighter(OmarchyTheme.background, 1.8)
    readonly property color danger: OmarchyTheme.red
    readonly property color approve: OmarchyTheme.green
    readonly property color accent: OmarchyTheme.accent

    readonly property color phaseColor: {
        switch (popupState.phase) {
        case "recording": return OmarchyTheme.red;
        case "transcribing": return OmarchyTheme.accent;
        case "classifying": return OmarchyTheme.magenta;
        case "handing_off": return OmarchyTheme.accent;
        case "confirming": return OmarchyTheme.yellow;
        case "ready": return OmarchyTheme.green;
        default: return buttonBorder;
        }
    }

    // Maps PopupPhase (see popup/mod.rs) onto AnimatedBorder's animation
    // modes 1:1 — every phase but "idle" gets the ring; "listening" gets
    // no dedicated animation of its own (nova's CSS didn't define one
    // either) so it falls through to the ring's steady low-opacity look.
    // "handing_off" reuses the "transcribing" mode (strobing) rather than
    // getting its own AnimatedBorder mode — same "working, no ETA" look
    // fits an OpenClaw round-trip just as well as a local transcribe.
    readonly property string borderMode: {
        switch (popupState.phase) {
        case "recording": return "recording";
        case "transcribing": return "transcribing";
        case "classifying": return "classifying";
        case "handing_off": return "transcribing";
        case "confirming": return "confirming";
        case "ready": return "ready";
        default: return popupState.phase; // "idle" or "listening"
        }
    }

    readonly property string phaseLabel: {
        switch (popupState.phase) {
        case "listening": return "Listening…";
        case "recording": return "Recording…";
        case "transcribing": return "Transcribing…";
        case "classifying": return "Thinking…";
        case "handing_off": return "Asking OpenClaw…";
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

    // Sits directly behind `card`, centered on it — see
    // AnimatedBorder.qml's header for why this reproduces nova's CSS
    // z-index-0-ring-behind-a-z-index-1-card composition instead of
    // painting a border on the card's own edge.
    AnimatedBorder {
        anchors.centerIn: card
        holeWidth: card.width
        holeHeight: card.height
        holeRadius: card.radius
        mode: root.borderMode
    }

    Rectangle {
        id: card
        width: 420
        implicitHeight: contentColumn.implicitHeight + 24
        height: implicitHeight
        anchors.horizontalCenter: parent.horizontalCenter
        // Matches nova's Electron popup exactly (main.js createPopupWindow:
        // y = screenH * 0.3, centered horizontally) rather than the bottom
        // anchor the first QML pass used. Also keeps novad's popup clear of
        // voxtype's own OSD, which sits low (bottom-center, top_margin=0.85
        // in voxtype's config.toml) — novad on top, voxtype below.
        anchors.top: parent.top
        anchors.topMargin: Math.round(root.height * 0.3)

        radius: 10
        color: root.bgColor

        Behavior on implicitHeight {
            NumberAnimation { duration: 150; easing.type: Easing.OutQuad }
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
                        running: ["recording", "transcribing", "classifying", "handing_off", "confirming"].includes(popupState.phase)
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
