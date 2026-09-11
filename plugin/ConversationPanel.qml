// omarchy-novad's OpenClaw conversation transcript window -- this
// plugin's port of the standalone
// quickshell/OpenClawConversation.qml onto the Omarchy shell plugin
// host contract.
//
// Same layer-shell pattern as PopupCard.qml (a full-anchored
// PanelWindow, nested here as a plain child item inside Overlay.qml's
// host-injected `Item` root, with a `mask` restricted to the panel's
// own rect so it never intercepts clicks anywhere else on screen).
// This coexists with PopupCard rather than replacing it: PopupCard is
// nova's short-lived per-utterance confirm/review card (centered, near
// the top); this is the longer-lived multi-turn OpenClaw conversation
// log (docked to the right edge, tall). See this plugin's README for
// why both live under one `overlay` entry point instead of two.
//
// Unlike PopupCard, this window is a permanent docked chat window, not
// a transient card -- but it's hidden by default and only appears on a
// novad activation (a conversation starting or a turn being sent), a
// tray-icon click, or the SUPER+H key bind (see Service.panelVisible and
// the transition detection in Service.qml's conversation-state watcher).
// The chat box at the bottom is always present (typing into it starts a
// conversation when none is running, or sends a new message
// mid-conversation -- see service.sendText()); a voice turn is sent to
// OpenClaw the instant it's transcribed, no review/confirm step (see
// src/converse.rs's doc comment), so its outgoing bubble and OpenClaw's
// reply both appear in the transcript area live as they happen (see
// Service.conversationPendingUserText/conversationStreamingText), rather
// than all at once when the turn completes.
//
// State comes from `service` (Overlay.qml's injected Service.qml
// instance) instead of a local ConversationState file-watcher -- the
// daemon <-> UI JSON-file contract is unchanged, Service.qml now owns
// the one FileView that reads it. Actions go back out via
// `service.stopConversation()`, `service.startListening()`, etc.,
// which run `omarchy-novad converse <action>` the same way this file
// used to run them directly.

import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Wayland

PanelWindow {
    id: root

    // Injected by Overlay.qml.
    property var service: null

    readonly property bool active: root.service ? root.service.conversationActive : false
    readonly property string phase: root.service ? root.service.conversationPhase : ""
    readonly property string pendingUserText: root.service ? root.service.conversationPendingUserText : ""
    readonly property var turns: root.service ? root.service.conversationTurns : []
    readonly property int thinkingElapsedSecs: root.service ? root.service.conversationThinkingElapsedSecs : -1
    readonly property string streamingText: root.service ? root.service.conversationStreamingText : ""

    // Visible only when the service says so: hidden by default, shown on
    // a novad activation (conversation starting or a turn entering
    // "thinking" -- Service.qml's transition detection), a tray-icon
    // click, or the SUPER+H key bind. The service owns the auto-show
    // logic so an explicit hide (tray/key bind) sticks until the next
    // activation; this binding just mirrors `service.panelVisible`.
    visible: root.service ? root.service.panelVisible : false

    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore

    WlrLayershell.namespace: "omarchy-novad-conversation"
    WlrLayershell.layer: WlrLayer.Overlay
    // Always OnDemand so the always-present chat box can be focused --
    // the panel is a permanent input surface.
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.OnDemand

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
        case "thinking": return OmarchyTheme.magenta;
        case "speaking": return OmarchyTheme.green;
        default: return root.mutedColor; // "" -- idle
        }
    }

    readonly property string phaseLabel: {
        switch (phase) {
        case "listening": return "Listening…";
        case "thinking": return root.thinkingElapsedSecs >= 0
            ? "Thinking… (" + root.thinkingElapsedSecs + "s)" : "Thinking…";
        case "speaking": return "Speaking…";
        default: return active ? "Ready" : "Idle";
        }
    }

    // "" (idle) is the only phase where a fresh recording can be
    // started -- listening/thinking/speaking are all already mid-turn.
    readonly property bool listenButtonVisible: active && phase === ""
    readonly property bool stopListeningButtonVisible: phase === "listening"

    // ── Session picker -- see src/sessions.rs's doc comment. Every new
    //    conversation gets its own fresh OpenClaw session by default;
    //    this dropdown is the opt-in "actually, revisit an old one"
    //    escape hatch. ──
    readonly property var sessionList: root.service ? root.service.sessionList : []
    readonly property string currentSessionKey: root.service ? root.service.conversationSessionKey : ""
    property bool sessionMenuOpen: false

    function sessionRowLabel(rec) {
        return (rec.label && rec.label.length > 0) ? rec.label : "New session";
    }

    function switchToSession(key) {
        root.sessionMenuOpen = false;
        if (root.service) root.service.switchToSession(key);
    }

    function startNewSession() {
        root.sessionMenuOpen = false;
        if (root.service) root.service.startNewSession();
    }

    function stopConversation() {
        if (root.service) root.service.stopConversation();
    }

    // Starts a new recording for the next turn -- the daemon never
    // starts one on its own between turns, see converse.rs's module
    // doc comment.
    function startListening() {
        if (root.service) root.service.startListening();
    }

    // Ends the in-progress recording early instead of waiting for
    // voxtype's own silence-timeout.
    function stopListeningNow() {
        if (root.service) root.service.stopListening();
    }

    // Sends the chat box's current text as a new turn's utterance --
    // trims it, ignores empty sends, clears the box. See
    // service.sendText() for the active-vs-start routing.
    function sendChatText(text) {
        const t = (text || "").trim();
        if (t.length === 0) return;
        if (root.service) root.service.sendText(t);
        chatField.text = "";
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

                // Sessions: opens the picker dropdown below (see the
                // Rectangle declared after turnsList) -- "usually a new
                // session" is the default (starting a conversation
                // never needs this), this is only for revisiting one.
                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "Sessions"
                    tint: OmarchyTheme.accent
                    primary: root.sessionMenuOpen
                    onClicked: root.sessionMenuOpen = !root.sessionMenuOpen
                }

                // Record: only shown once idle (phase === ""), waiting
                // for the user to start the next turn -- the daemon
                // never starts a recording on its own. See converse.rs's
                // module doc comment for why this whole flow is
                // manually triggered rather than an automatic loop.
                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    visible: root.listenButtonVisible
                    label: "Record"
                    tint: OmarchyTheme.accent
                    onClicked: root.startListening()
                }

                // Stop Listening: only shown while actually recording --
                // ends it early instead of waiting for voxtype's own
                // silence-timeout. Distinct from the "Stop" button
                // below, which ends the whole conversation.
                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    visible: root.stopListeningButtonVisible
                    label: "Stop Listening"
                    tint: OmarchyTheme.yellow
                    onClicked: root.stopListeningNow()
                }

                PopupButton {
                    anchors.verticalCenter: parent.verticalCenter
                    label: "Stop"
                    tint: root.danger
                    onClicked: root.stopConversation()
                }
            }
        }

        // ── Bottom bar: live phase indicator + the always-present chat
        //    box, pinned to the bottom of the window like a chat app's
        //    compose bar. History streams in the space above it
        //    (turnsList below), most-recent turn nearest this bar --
        //    normal chat-box layout, not read-then-compose. ──
        Column {
            id: bottomBar
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            anchors.margins: 16
            spacing: 8

            // ── Live phase indicator -- pulsing dot + label, same
            //    animation style as PopupCard's status bar. Always
            //    visible now (the panel is a permanent window); the dot
            //    only pulses while a phase is actually active. ──
            Row {
                id: statusRow
                height: 18
                spacing: 8

                Rectangle {
                    width: 8; height: 8; radius: 4
                    color: root.phaseColor
                    anchors.verticalCenter: parent.verticalCenter

                    SequentialAnimation on opacity {
                        running: root.phase.length > 0
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

            // ── Always-present chat box -- the compose bar. Typing a
            //    message here starts a conversation when none is
            //    running (service.sendText routes to `converse start
            //    --text`) or sends a new turn's message mid-
            //    conversation (`converse send-text`). Enter sends,
            //    Shift+Enter inserts a newline. Grows with content up
            //    to a cap, then scrolls internally. This is the panel's
            //    only input surface now -- a transcript from voice is
            //    sent to OpenClaw the instant it's heard (no
            //    review/confirm step, see src/converse.rs's doc
            //    comment), so this box is purely for typed messages,
            //    never blocked by anything else needing attention
            //    first. ──
            Rectangle {
                id: chatBox
                width: parent.width
                height: Math.min(Math.max(chatField.implicitHeight + 20, 40), 120)
                radius: 8
                color: Qt.darker(root.bgColor, 1.15)
                border.width: 1
                border.color: chatField.activeFocus ? root.accent : root.divider

                Behavior on border.color {
                    ColorAnimation { duration: 120 }
                }

                Row {
                    anchors.fill: parent
                    anchors.margins: 8
                    spacing: 8

                    Flickable {
                        id: chatFlickable
                        width: parent.width - sendButton.width - parent.spacing
                        height: parent.height
                        contentWidth: width
                        contentHeight: Math.max(chatField.implicitHeight, height)
                        clip: true
                        boundsBehavior: Flickable.StopAtBounds

                        TextEdit {
                            id: chatField
                            width: parent.width
                            height: Math.max(chatFlickable.height, implicitHeight)
                            text: ""
                            color: root.textColor
                            font.family: "JetBrains Mono"
                            font.pixelSize: 13
                            wrapMode: TextEdit.Wrap
                            selectByMouse: true

                            // Placeholder hint -- shown only while empty
                            // and unfocused, so it never blocks a click
                            // into the box itself.
                            Text {
                                anchors.fill: parent
                                text: root.active ? "Type a message…" : "Type a message to start…"
                                color: root.mutedColor
                                font.family: "JetBrains Mono"
                                font.pixelSize: 13
                                verticalAlignment: Text.AlignVCenter
                                wrapMode: Text.Wrap
                                visible: parent.text.length === 0 && !parent.activeFocus
                            }

                            // Enter sends -- Shift+Enter still inserts a
                            // newline for multi-line messages.
                            Keys.onReturnPressed: (event) => {
                                if (event.modifiers & Qt.ShiftModifier) {
                                    event.accepted = false;
                                } else {
                                    root.sendChatText(chatField.text);
                                    event.accepted = true;
                                }
                            }
                            Keys.onEnterPressed: (event) => {
                                root.sendChatText(chatField.text);
                                event.accepted = true;
                            }
                        }
                    }

                    PopupButton {
                        id: sendButton
                        width: 60
                        height: parent.height
                        label: "Send"
                        tint: root.accent
                        onClicked: root.sendChatText(chatField.text)
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
            footer: root.pendingUserText.length > 0 ? pendingTurnFooter : null

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

            // The pending turn's outgoing bubble and the streamed reply
            // both grow/appear as new state arrives -- keep the newest
            // text in view. (A Connections block rather than an
            // `onXChanged` handler, which would only fire for a signal
            // on the ListView itself.)
            Connections {
                target: root
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
            visible: turnsList.count === 0 && root.pendingUserText.length === 0
        }

        // ── Session picker dropdown -- declared last (after
        //    turnsList/bottomBar above) so it draws on top of them; a
        //    click anywhere outside it closes it. ──
        MouseArea {
            anchors.fill: parent
            visible: root.sessionMenuOpen
            enabled: root.sessionMenuOpen
            onClicked: root.sessionMenuOpen = false
        }

        Rectangle {
            id: sessionMenu
            visible: root.sessionMenuOpen
            anchors.top: header.bottom
            anchors.right: parent.right
            anchors.topMargin: 4
            anchors.rightMargin: 16
            width: 220
            height: Math.min(sessionMenuColumn.implicitHeight + 12, 320)
            radius: 8
            color: Qt.darker(root.bgColor, 1.2)
            border.width: 1
            border.color: root.divider
            clip: true

            Flickable {
                anchors.fill: parent
                anchors.margins: 6
                contentWidth: width
                contentHeight: sessionMenuColumn.implicitHeight
                clip: true
                boundsBehavior: Flickable.StopAtBounds

                Column {
                    id: sessionMenuColumn
                    width: parent.width
                    spacing: 2

                    Rectangle {
                        width: parent.width
                        height: 28
                        radius: 6
                        color: newSessionArea.containsMouse ? Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.18) : "transparent"

                        Text {
                            anchors.left: parent.left
                            anchors.leftMargin: 8
                            anchors.verticalCenter: parent.verticalCenter
                            text: "+ New Session"
                            color: root.accent
                            font.pixelSize: 12
                            font.weight: Font.Medium
                        }

                        MouseArea {
                            id: newSessionArea
                            anchors.fill: parent
                            hoverEnabled: true
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.startNewSession()
                        }
                    }

                    Rectangle {
                        width: parent.width
                        height: 1
                        color: root.divider
                        visible: root.sessionList.length > 0
                    }

                    Text {
                        width: parent.width
                        text: "No past sessions yet"
                        color: root.mutedColor
                        font.pixelSize: 11
                        visible: root.sessionList.length === 0
                        topPadding: 6
                        bottomPadding: 4
                        horizontalAlignment: Text.AlignHCenter
                    }

                    Repeater {
                        model: root.sessionList

                        delegate: Rectangle {
                            required property var modelData
                            readonly property bool isCurrent: modelData.key === root.currentSessionKey

                            width: sessionMenuColumn.width
                            height: 28
                            radius: 6
                            color: rowArea.containsMouse
                                ? Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.14)
                                : (isCurrent ? Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.08) : "transparent")

                            Text {
                                anchors.left: parent.left
                                anchors.right: parent.right
                                anchors.leftMargin: 8
                                anchors.rightMargin: 8
                                anchors.verticalCenter: parent.verticalCenter
                                text: (isCurrent ? "● " : "") + root.sessionRowLabel(modelData)
                                color: isCurrent ? root.textColor : root.mutedColor
                                font.pixelSize: 12
                                elide: Text.ElideRight
                            }

                            MouseArea {
                                id: rowArea
                                anchors.fill: parent
                                hoverEnabled: true
                                cursorShape: Qt.PointingHandCursor
                                onClicked: root.switchToSession(modelData.key)
                            }
                        }
                    }
                }
            }
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
                        width: pendingUserBubbleText.width + 24
                        height: pendingUserBubbleText.implicitHeight + 16

                        Text {
                            id: pendingUserBubbleText
                            anchors.centerIn: parent
                            text: root.pendingUserText
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
                    visible: root.streamingText.length > 0
                }

                Text {
                    width: parent.width
                    text: root.streamingText
                    color: root.textColor
                    font.family: "JetBrains Mono"
                    font.pixelSize: 13
                    wrapMode: Text.Wrap
                    visible: root.streamingText.length > 0
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
