// omarchy-novad OpenClaw conversation transcript window.
//
// Same layer-shell + visibility-toggle pattern as OmarchyNovadPopup.qml
// (a full-anchored PanelWindow that's only actually visible -- and only
// then receiving input -- while there's a conversation in progress, via
// a `mask` restricted to the panel's own rect, so it never intercepts
// clicks anywhere else on screen). This is a second, independent
// window that coexists with the popup rather than replacing it: the
// popup is nova's short-lived per-utterance confirm/review card
// (centered, near the top); this is the longer-lived multi-turn
// OpenClaw conversation log (docked to the right edge, tall).
//
// State comes from ConversationState (a JSON file the daemon rewrites
// on every conversation-loop event -- see src/conversation/mod.rs's
// module doc comment); the one action available here ("Stop") goes
// back out via `omarchy-novad converse stop` run through
// Quickshell.Io.Process, same mechanism as OmarchyNovadPopup's
// `respond()`, just on the conversation module's own control socket
// rather than the popup's.

import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Wayland
import Quickshell.Io

PanelWindow {
    id: root

    ConversationState {
        id: conversationState
    }

    visible: conversationState.active

    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore

    WlrLayershell.namespace: "novad-conversation"
    WlrLayershell.layer: WlrLayer.Overlay
    // Same reasoning as OmarchyNovadPopup: mouse clicks + wheel/drag
    // scrolling only, no keyboard focus needed -- this standalone dev
    // window has no text input of its own (the plugin's
    // ConversationPanel has the always-present chat box; this one is
    // just a log + Stop button).
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.None

    // Restrict the actually-interactive input region to the panel
    // itself -- the full-screen anchors above exist only so the panel
    // can be positioned via anchors.right/top/bottom below; everywhere
    // outside `panel`'s bounds must stay click-through.
    mask: Region {
        x: panel.x
        y: panel.y
        width: panel.width
        height: panel.height
    }

    // ── Palette: OmarchyTheme, same mapping OmarchyNovadPopup.qml uses. ──
    readonly property color bgColor: OmarchyTheme.background
    readonly property color textColor: OmarchyTheme.foreground
    readonly property color mutedColor: OmarchyTheme.muted
    readonly property color accent: OmarchyTheme.accent
    readonly property color danger: OmarchyTheme.red
    readonly property color divider: Qt.rgba(textColor.r, textColor.g, textColor.b, 0.08)
    readonly property color userBubbleColor: Qt.rgba(accent.r, accent.g, accent.b, 0.18)

    readonly property color phaseColor: {
        switch (conversationState.phase) {
        case "listening": return OmarchyTheme.accent;
        case "thinking": return OmarchyTheme.magenta;
        case "speaking": return OmarchyTheme.green;
        default: return root.mutedColor;
        }
    }

    readonly property string phaseLabel: {
        switch (conversationState.phase) {
        case "listening": return "Listening…";
        case "thinking": return "Thinking…";
        case "speaking": return "Speaking…";
        default: return "";
        }
    }

    property string novadBinary: "omarchy-novad"

    function stopConversation() {
        converseProcess.command = [novadBinary, "converse", "stop"];
        converseProcess.running = true;
    }

    Process {
        id: converseProcess
        running: false
    }

    Rectangle {
        id: panel

        width: 400
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        anchors.right: parent.right
        anchors.topMargin: 24
        anchors.bottomMargin: 24
        anchors.rightMargin: 24

        radius: 10
        color: root.bgColor

        // ── Header: title + Stop button ──
        Item {
            id: header
            anchors.top: parent.top
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.margins: 16
            height: 26

            Text {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "OpenClaw"
                color: root.textColor
                font.pixelSize: 14
                font.weight: Font.Bold
            }

            PopupButton {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                label: "Stop"
                tint: root.danger
                onClicked: root.stopConversation()
            }
        }

        // ── Bottom bar: just the live phase indicator now -- there's
        //    no confirm/review step any more (a transcript is sent to
        //    OpenClaw the instant it's heard, see src/converse.rs's
        //    doc comment), so nothing here needs to gate on user
        //    input. History scrolls in the space above it (turnsList
        //    below), most-recent turn nearest this bar. ──
        Column {
            id: bottomBar
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            anchors.margins: 16
            spacing: 8

            // ── Live phase indicator -- pulsing dot + label, same
            //    animation style as OmarchyNovadPopup's status bar.
            //    Hidden entirely when phase is absent/idle-within-
            //    conversation (see ConversationState.phase's docs). ──
            Row {
                id: statusRow
                height: visible ? 18 : 0
                spacing: 8
                visible: conversationState.phase.length > 0

                Rectangle {
                    width: 8; height: 8; radius: 4
                    color: root.phaseColor
                    anchors.verticalCenter: parent.verticalCenter

                    SequentialAnimation on opacity {
                        running: statusRow.visible
                        loops: Animation.Infinite
                        NumberAnimation { to: 0.4; duration: 600 }
                        NumberAnimation { to: 1.0; duration: 600 }
                    }
                }

                Text {
                    text: root.phaseLabel
                    color: root.mutedColor
                    font.pixelSize: 12
                    font.weight: Font.Medium
                    font.letterSpacing: 0.5
                    anchors.verticalCenter: parent.verticalCenter
                }
            }
        }

        // ── Scrolling transcript -- oldest turn at top, newest at the
        //    bottom nearest bottomBar, auto-scrolled to the newest turn
        //    as they arrive (conventional chat-log behavior). A visible
        //    scrollbar makes it clear the whole session's history is
        //    scrollable, not just the latest turn. ──
        ListView {
            id: turnsList
            anchors.top: header.bottom
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: bottomBar.top
            anchors.margins: 16
            anchors.topMargin: 8
            anchors.bottomMargin: 8
            clip: true
            spacing: 14
            model: conversationState.turns
            delegate: turnDelegate
            footer: conversationState.pendingUserText.length > 0 ? pendingTurnFooter : null

            ScrollBar.vertical: ScrollBar {
                policy: ScrollBar.AsNeeded
                contentItem: Rectangle {
                    implicitWidth: 4
                    radius: 2
                    color: root.mutedColor
                    opacity: 0.5
                }
            }

            // New turn arrives (or the whole array is reloaded fresh
            // from a torn-read-free parse) -- jump to the bottom once
            // the delegate has actually been laid out, same "wait a
            // tick" trick as popup's Behavior-driven resizes.
            onCountChanged: Qt.callLater(turnsList.positionViewAtEnd)
            Component.onCompleted: Qt.callLater(turnsList.positionViewAtEnd)

            // The pending turn's outgoing bubble and the streamed reply
            // both grow/appear as the daemon writes new state -- keep
            // the newest text in view. (A Connections block rather
            // than an `onXChanged` handler, which would only fire for
            // a signal on the ListView itself.)
            Connections {
                target: conversationState
                function onStreamingTextChanged() {
                    Qt.callLater(turnsList.positionViewAtEnd);
                }
                function onPendingUserTextChanged() {
                    Qt.callLater(turnsList.positionViewAtEnd);
                }
            }
        }

        Text {
            anchors.centerIn: turnsList
            text: "Waiting for the first turn…"
            color: root.mutedColor
            font.pixelSize: 13
            visible: turnsList.count === 0 && conversationState.pendingUserText.length === 0
        }

        // ── The in-progress turn -- shown as a footer below the
        //    committed turns from the instant a transcript (or typed
        //    message) is sent, so the outgoing utterance is visible
        //    right away rather than only once the reply completes
        //    (there's no review/confirm step to wait through any
        //    more). OpenClaw's reply streams in underneath as it's
        //    produced. Cleared the moment the turn completes; both
        //    halves then land together in a new turns entry. ──
        Component {
            id: pendingTurnFooter

            Column {
                width: turnsList.width
                spacing: 8

                // ── Outgoing bubble -- same style as turnDelegate's,
                //    just driven by the not-yet-committed text. ──
                Item {
                    width: parent.width
                    height: pendingUserBubble.height

                    Rectangle {
                        id: pendingUserBubble
                        anchors.right: parent.right
                        radius: 12
                        color: root.userBubbleColor
                        width: pendingUserText.width + 24
                        height: pendingUserText.implicitHeight + 16

                        Text {
                            id: pendingUserText
                            anchors.centerIn: parent
                            text: conversationState.pendingUserText
                            color: root.textColor
                            font.pixelSize: 13
                            font.weight: Font.Medium
                            wrapMode: Text.Wrap
                            width: Math.min(implicitWidth, turnsList.width * 0.8)
                        }
                    }
                }

                Text {
                    text: "OpenClaw is replying…"
                    color: root.mutedColor
                    font.pixelSize: 11
                    font.italic: true
                    visible: conversationState.streamingText.length > 0
                }

                Text {
                    width: parent.width
                    text: conversationState.streamingText
                    color: root.textColor
                    font.family: "JetBrains Mono"
                    font.pixelSize: 13
                    wrapMode: Text.Wrap
                    visible: conversationState.streamingText.length > 0
                }
            }
        }

        Component {
            id: turnDelegate

            Column {
                id: turnRoot
                width: turnsList.width
                spacing: 8

                readonly property real maxBubbleWidth: width * 0.8

                // ── User's utterance -- outgoing chat bubble, right-
                //    aligned. ──
                Item {
                    width: parent.width
                    height: userBubble.height

                    Rectangle {
                        id: userBubble
                        anchors.right: parent.right
                        radius: 12
                        color: root.userBubbleColor
                        width: userText.width + 24
                        height: userText.implicitHeight + 16

                        Text {
                            id: userText
                            anchors.centerIn: parent
                            text: modelData.user_text || ""
                            color: root.textColor
                            font.pixelSize: 13
                            font.weight: Font.Medium
                            wrapMode: Text.Wrap
                            width: Math.min(implicitWidth, turnRoot.maxBubbleWidth)
                        }
                    }
                }

                // ── OpenClaw's full response -- main content, plain
                //    wrapped text (not markdown-rendered). ──
                Text {
                    width: parent.width
                    text: modelData.full_response || ""
                    color: root.textColor
                    font.family: "JetBrains Mono"
                    font.pixelSize: 13
                    wrapMode: Text.Wrap
                }

                // ── Spoken summary -- only present when summarization
                //    succeeded (absent means the full response above
                //    was spoken verbatim instead, so there's nothing
                //    distinct to show here). ──
                Text {
                    width: parent.width
                    text: "🔊 spoken: " + (modelData.spoken_summary || "")
                    color: root.mutedColor
                    font.pixelSize: 11
                    font.italic: true
                    wrapMode: Text.Wrap
                    visible: !!modelData.spoken_summary
                }

                // ── Divider between turns (not after the last one). ──
                Rectangle {
                    width: parent.width
                    height: 1
                    color: root.divider
                    visible: index < turnsList.count - 1
                }
            }
        }
    }
}
