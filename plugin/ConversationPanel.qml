// omarchy-novad's OpenClaw conversation transcript window -- this
// plugin's port of the standalone
// quickshell/OpenClawConversation.qml onto the Omarchy shell plugin
// host contract.
//
// Same layer-shell + visibility-toggle pattern as PopupCard.qml (a
// full-anchored PanelWindow, nested here as a plain child item inside
// Overlay.qml's host-injected `Item` root, that's only actually
// visible -- and only then receiving input -- while there's a
// conversation in progress, via a `mask` restricted to the panel's own
// rect, so it never intercepts clicks anywhere else on screen). This
// coexists with PopupCard rather than replacing it: PopupCard is
// nova's short-lived per-utterance confirm/review card (centered, near
// the top); this is the longer-lived multi-turn OpenClaw conversation
// log (docked to the right edge, tall). See this plugin's README for
// why both live under one `overlay` entry point instead of two.
//
// State comes from `service` (Overlay.qml's injected Service.qml
// instance) instead of a local ConversationState file-watcher -- the
// daemon <-> UI JSON-file contract is unchanged, Service.qml now owns
// the one FileView that reads it. The one action available here
// ("Stop") goes back out via `service.stopConversation()`, which runs
// `omarchy-novad converse stop` the same way this file used to run it
// directly.

import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Wayland
import Quickshell.Io

PanelWindow {
    id: root

    // Injected by Overlay.qml.
    property var service: null

    readonly property bool active: root.service ? root.service.conversationActive : false
    readonly property string phase: root.service ? root.service.conversationPhase : ""
    readonly property string pendingText: root.service ? root.service.conversationPendingText : ""
    readonly property var turns: root.service ? root.service.conversationTurns : []
    readonly property bool handsFree: root.service ? root.service.conversationHandsFree : false

    visible: active

    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore

    WlrLayershell.namespace: "omarchy-novad-conversation"
    WlrLayershell.layer: WlrLayer.Overlay
    // Same reasoning as PopupCard: no keyboard focus most of the time
    // (mouse clicks + wheel/drag scrolling only) -- the one exception
    // is the pending-transcript edit box below, exactly PopupCard's own
    // editBoxVisible exception.
    WlrLayershell.keyboardFocus: phase === "confirming"
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

    // ── Palette: OmarchyTheme, same mapping PopupCard.qml uses. ──
    readonly property color bgColor: OmarchyTheme.background
    readonly property color textColor: OmarchyTheme.foreground
    readonly property color mutedColor: OmarchyTheme.muted
    readonly property color accent: OmarchyTheme.accent
    readonly property color danger: OmarchyTheme.red
    readonly property color divider: Qt.rgba(textColor.r, textColor.g, textColor.b, 0.08)
    readonly property color userBubbleColor: Qt.rgba(accent.r, accent.g, accent.b, 0.18)

    readonly property color phaseColor: {
        switch (phase) {
        case "listening": return OmarchyTheme.accent;
        case "confirming": return OmarchyTheme.yellow;
        case "thinking": return OmarchyTheme.magenta;
        case "speaking": return OmarchyTheme.green;
        default: return root.mutedColor;
        }
    }

    readonly property string phaseLabel: {
        switch (phase) {
        case "listening": return "Listening…";
        case "confirming": return "Confirming…";
        case "thinking": return "Thinking…";
        case "speaking": return "Speaking…";
        default: return "";
        }
    }

    readonly property bool confirmBoxVisible: phase === "confirming"

    function stopConversation() {
        if (root.service) root.service.stopConversation();
    }

    // Confirms (optionally with edited text) or rejects the pending
    // transcript -- see src/conversation/mod.rs's ConversationAction.
    function confirmPending(text) {
        if (root.service) root.service.confirmPending(text);
    }

    function rejectPending() {
        if (root.service) root.service.rejectPending();
    }

    function toggleHandsFree() {
        if (root.service) root.service.setHandsFree(!root.handsFree);
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

            Row {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                spacing: 8

                // Hands-free: once on, each turn skips "does this look
                // good?" entirely -- talk back and forth with no
                // confirm step. Off by default; the first message in a
                // session is still reviewable unless this is already
                // on when it's transcribed. See
                // src/conversation/mod.rs's ConversationState::hands_free.
                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: root.handsFree ? "Hands-Free: On" : "Hands-Free"
                    tint: OmarchyTheme.green
                    primary: root.handsFree
                    onClicked: root.toggleHandsFree()
                }

                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "Stop"
                    tint: root.danger
                    onClicked: root.stopConversation()
                }
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
            //    animation style as PopupCard's status bar. Hidden
            //    entirely when phase is absent/idle-within-conversation
            //    (see Service.conversationPhase's docs). ──
            Row {
                id: statusRow
                height: visible ? 18 : 0
                spacing: 8
                visible: root.phase.length > 0

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
            //    PopupCard's own edit box; Enter confirms (with
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
                        text: root.pendingText
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
                            text = root.pendingText;
                            lastSyncedText = root.pendingText;
                        }
                        Connections {
                            target: root.service
                            enabled: root.service !== null
                            function onConversationPendingTextChanged() {
                                if (pendingField.text === pendingField.lastSyncedText) {
                                    pendingField.text = root.pendingText;
                                    pendingField.lastSyncedText = root.pendingText;
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
            model: root.turns
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
            // tick" trick as the card's Behavior-driven resizes.
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
