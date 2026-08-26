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
    // Same reasoning as OmarchyNovadPopup: no keyboard focus most of
    // the time (mouse clicks + wheel/drag scrolling only) -- the one
    // exception is the pending-transcript edit box below, exactly
    // OmarchyNovadPopup's own editBoxVisible exception.
    WlrLayershell.keyboardFocus: conversationState.phase === "confirming"
        ? WlrKeyboardFocus.OnDemand
        : WlrKeyboardFocus.None

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
        case "confirming": return OmarchyTheme.yellow;
        case "thinking": return OmarchyTheme.magenta;
        case "speaking": return OmarchyTheme.green;
        default: return root.mutedColor;
        }
    }

    readonly property string phaseLabel: {
        switch (conversationState.phase) {
        case "listening": return "Listening…";
        case "confirming": return "Confirming…";
        case "thinking": return "Thinking…";
        case "speaking": return "Speaking…";
        default: return "";
        }
    }

    readonly property bool confirmBoxVisible: conversationState.phase === "confirming"

    property string novadBinary: "omarchy-novad"

    function stopConversation() {
        converseProcess.command = [novadBinary, "converse", "stop"];
        converseProcess.running = true;
    }

    // Confirms (optionally with edited text) or rejects the pending
    // transcript -- see src/conversation/mod.rs's ConversationAction.
    // Mirrors OmarchyNovadPopup.respond()'s optional-`--text` shape.
    function confirmPending(text) {
        converseProcess.command = (text !== undefined && text !== null && text.length > 0)
            ? [novadBinary, "converse", "confirm", "--text", text]
            : [novadBinary, "converse", "confirm"];
        converseProcess.running = true;
    }

    function rejectPending() {
        converseProcess.command = [novadBinary, "converse", "reject"];
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

        // ── Bottom bar: live phase indicator + pending-transcript
        //    confirmation, pinned to the bottom of the window like a
        //    chat app's compose bar. History scrolls in the space
        //    above it (turnsList below), most-recent turn nearest this
        //    bar -- normal chat-box layout, not read-then-compose. ──
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

            // ── Pending-transcript confirmation -- "does this look
            //    good?" (see src/converse.rs::confirm_utterance).
            //    Editable, same TextEdit-in-a-bordered-box pattern as
            //    OmarchyNovadPopup's own edit box; Enter confirms (with
            //    whatever's currently in the box, edited or not)
            //    exactly like clicking Confirm, so a fast edit-and-
            //    Enter pre-empts the spoken "yes/no" prompt (see
            //    converse.rs's UI_CONFIRM_GRACE window). ──
            Rectangle {
                id: confirmBox
                width: parent.width
                height: root.confirmBoxVisible ? (confirmColumn.implicitHeight + 20) : 0
                visible: root.confirmBoxVisible
                clip: true
                radius: 8
                color: Qt.darker(root.bgColor, 1.15)
                border.width: 1
                border.color: pendingField.activeFocus ? root.accent : root.divider

                Behavior on border.color {
                    ColorAnimation { duration: 120 }
                }

                // Grab focus fresh every time this box appears for a
                // new pending transcript -- it stays instantiated but
                // hidden the rest of the time, so Component.onCompleted
                // alone (fires once, ever) wouldn't refire here.
                onVisibleChanged: if (visible) pendingField.forceActiveFocus()

                Column {
                    id: confirmColumn
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.margins: 10
                    spacing: 8

                    Text {
                        text: "Does this look good?"
                        color: root.mutedColor
                        font.pixelSize: 12
                        font.weight: Font.Medium
                    }

                    TextEdit {
                        id: pendingField
                        width: parent.width
                        text: conversationState.pendingText
                        color: root.textColor
                        font.family: "JetBrains Mono"
                        font.pixelSize: 13
                        wrapMode: TextEdit.Wrap
                        selectByMouse: true
                        focus: root.confirmBoxVisible

                        // A fresh pendingText from the daemon (a new
                        // transcript, or a voice re-statement replacing
                        // an unclear reply -- see converse.rs's
                        // confirm-round loop) should overwrite
                        // whatever's here, but only when this box
                        // wasn't already mid-edit by the user.
                        property string lastSyncedText: ""
                        onVisibleChanged: if (visible) {
                            text = conversationState.pendingText;
                            lastSyncedText = conversationState.pendingText;
                        }
                        Connections {
                            target: conversationState
                            function onPendingTextChanged() {
                                if (pendingField.text === pendingField.lastSyncedText) {
                                    pendingField.text = conversationState.pendingText;
                                    pendingField.lastSyncedText = conversationState.pendingText;
                                }
                            }
                        }

                        // Enter confirms with the current (possibly
                        // edited) text -- Shift+Enter still inserts a
                        // newline for anyone who wants a multi-line edit.
                        Keys.onReturnPressed: (event) => {
                            if (event.modifiers & Qt.ShiftModifier) {
                                event.accepted = false;
                            } else {
                                root.confirmPending(pendingField.text);
                                event.accepted = true;
                            }
                        }
                        Keys.onEnterPressed: (event) => {
                            root.confirmPending(pendingField.text);
                            event.accepted = true;
                        }
                    }

                    Row {
                        spacing: 6
                        anchors.right: parent.right

                        PopupButton {
                            label: "Reject"
                            tint: root.danger
                            onClicked: root.rejectPending()
                        }

                        PopupButton {
                            label: "Confirm"
                            tint: root.accent
                            onClicked: root.confirmPending(pendingField.text)
                        }
                    }
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
        }

        Text {
            anchors.centerIn: turnsList
            text: "Waiting for the first turn…"
            color: root.mutedColor
            font.pixelSize: 13
            visible: turnsList.count === 0
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
