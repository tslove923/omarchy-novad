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
// Unlike PopupCard, this window is *always* visible -- it's a permanent
// docked chat window, not a transient card. The chat box at the bottom
// is always present (typing into it starts a conversation when none is
// running, or sends a new message mid-conversation -- see
// service.sendText()), and OpenClaw's reply streams into the transcript
// area live as it's produced (see Service.conversationStreamingText),
// so output appears as the model writes it rather than all at once when
// the turn completes.
//
// State comes from `service` (Overlay.qml's injected Service.qml
// instance) instead of a local ConversationState file-watcher -- the
// daemon <-> UI JSON-file contract is unchanged, Service.qml now owns
// the one FileView that reads it. Actions go back out via
// `service.stopConversation()`, `service.confirmPending()`, etc.,
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
    readonly property string pendingText: root.service ? root.service.conversationPendingText : ""
    readonly property var turns: root.service ? root.service.conversationTurns : []
    readonly property int thinkingElapsedSecs: root.service ? root.service.conversationThinkingElapsedSecs : -1
    readonly property string streamingText: root.service ? root.service.conversationStreamingText : ""

    // Visible unless the user hid it via the tray icon
    // (service.togglePanel) -- but always pops back when a turn needs
    // review, so hiding it never strands a pending transcript the user
    // can't confirm. The empty state (no conversation running) shows
    // the idle status + the always-present chat box.
    visible: root.service ? (root.service.panelVisible || root.phase === "confirming") : true

    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore

    WlrLayershell.namespace: "omarchy-novad-conversation"
    WlrLayershell.layer: WlrLayer.Overlay
    // Always OnDemand so the always-present chat box can be focused --
    // the panel is a permanent input surface now, not a transient card
    // that only needs keyboard focus while confirming.
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
        case "confirming": return OmarchyTheme.yellow;
        case "thinking": return OmarchyTheme.magenta;
        case "speaking": return OmarchyTheme.green;
        default: return root.mutedColor; // "" -- idle
        }
    }

    readonly property string phaseLabel: {
        switch (phase) {
        case "listening": return "Listening…";
        case "confirming": return "Reviewing…";
        case "thinking": return root.thinkingElapsedSecs >= 0
            ? "Thinking… (" + root.thinkingElapsedSecs + "s)" : "Thinking…";
        case "speaking": return "Speaking…";
        default: return active ? "Ready" : "Idle";
        }
    }

    readonly property bool confirmBoxVisible: phase === "confirming"
    // "" (idle) is the only phase where a fresh recording can be
    // started -- listening/confirming/thinking/speaking are all
    // already mid-turn.
    readonly property bool listenButtonVisible: active && phase === ""
    readonly property bool stopListeningButtonVisible: phase === "listening"

    function stopConversation() {
        if (root.service) root.service.stopConversation();
    }

    // Sends (optionally with edited text) or discards the pending
    // transcript -- see src/conversation/mod.rs's ConversationAction.
    function confirmPending(text) {
        if (root.service) root.service.confirmPending(text);
    }

    function rejectPending() {
        if (root.service) root.service.rejectPending();
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

        // ── Bottom bar: live phase indicator + pending-transcript
        //    confirmation + the always-present chat box, pinned to the
        //    bottom of the window like a chat app's compose bar.
        //    History streams in the space above it (turnsList below),
        //    most-recent turn nearest this bar -- normal chat-box
        //    layout, not read-then-compose. ──
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

            // ── Pending-transcript review -- no timeout, no voice
            //    fallback (see src/converse.rs's wait_for_review): sits
            //    here for as long as the user wants. Editable, same
            //    TextEdit-in-a-bordered-box pattern as PopupCard's own
            //    edit box; Enter sends (with whatever's currently in
            //    the box, edited or not) exactly like clicking Confirm. ──
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
                        text: "Edit if needed, then press Enter to send"
                        color: root.mutedColor
                        font.pixelSize: 12
                        font.weight: Font.Medium
                    }

                    TextEdit {
                        id: pendingField
                        width: parent.width
                        text: ""
                        color: root.textColor
                        font.family: "JetBrains Mono"
                        font.pixelSize: 13
                        wrapMode: TextEdit.Wrap
                        selectByMouse: true
                        focus: root.confirmBoxVisible

                        // The daemon's pending transcript is synced into
                        // this box explicitly (no `text:` binding) so a
                        // fresh transcript -- a new one, or a voice
                        // re-statement replacing an unclear reply, see
                        // converse.rs's confirm-round loop -- overwrites
                        // whatever's here, but only when the user isn't
                        // mid-edit. Found live: the old `text:
                        // root.pendingText` binding + onVisibleChanged
                        // assignment raced the daemon's state write --
                        // onVisibleChanged broke the binding while
                        // root.pendingText was still stale (""), and the
                        // Connections handler read the panel's not-yet-
                        // re-evaluated pendingText binding, so the box
                        // stayed blank even though the daemon's
                        // pending_text was correct.
                        property string lastSyncedText: ""
                        property bool userEdited: false

                        onTextChanged: {
                            if (text !== lastSyncedText) userEdited = true;
                        }

                        function syncFromDaemon() {
                            const t = root.service ? root.service.conversationPendingText : "";
                            pendingField.lastSyncedText = t;
                            pendingField.text = t;
                            pendingField.userEdited = false;
                        }

                        Component.onCompleted: syncFromDaemon()
                        onVisibleChanged: if (visible) syncFromDaemon()

                        Connections {
                            target: root.service
                            enabled: root.service !== null
                            function onConversationPendingTextChanged() {
                                if (!pendingField.userEdited) pendingField.syncFromDaemon();
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
                            label: "Discard"
                            tint: root.danger
                            onClicked: root.rejectPending()
                        }

                        PopupButton {
                            label: "Send"
                            tint: root.accent
                            onClicked: root.confirmPending(pendingField.text)
                        }
                    }
                }
            }

            // ── Always-present chat box -- the compose bar. Typing a
            //    message here starts a conversation when none is
            //    running (service.sendText routes to `converse start
            //    --text`) or sends a new turn's message mid-
            //    conversation (`converse send-text`). Enter sends,
            //    Shift+Enter inserts a newline. Grows with content up
            //    to a cap, then scrolls internally. Hidden while a
            //    pending transcript is up for review -- the confirm
            //    box above is the single input surface then, so the
            //    two never stack into a confusing double chat box. ──
            Rectangle {
                id: chatBox
                visible: !root.confirmBoxVisible
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
            footer: root.streamingText.length > 0 ? streamingFooter : null

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

            // The streamed reply grows as deltas arrive -- keep the
            // newest text in view. (A Connections block rather than an
            // `onStreamingTextChanged` handler, which would only fire
            // for a signal on the ListView itself.)
            Connections {
                target: root
                function onStreamingTextChanged() {
                    Qt.callLater(turnsList.positionViewAtEnd);
                }
            }
        }

        Text {
            anchors.centerIn: turnsList
            text: "Waiting for the first turn…"
            color: root.mutedColor
            font.pixelSize: 13
            visible: turnsList.count === 0 && root.streamingText.length === 0
        }

        // ── Live-streamed OpenClaw reply -- shown as a footer below
        //    the committed turns while the handoff is streaming, so
        //    output appears as the model produces it rather than all at
        //    once when the turn completes. Cleared the moment the
        //    handoff returns; the full reply then lands in a new turns
        //    entry. ──
        Component {
            id: streamingFooter

            Column {
                width: turnsList.width
                spacing: 8

                Text {
                    text: "OpenClaw is replying…"
                    color: root.mutedColor
                    font.pixelSize: 11
                    font.italic: true
                }

                Text {
                    width: parent.width
                    text: root.streamingText
                    color: root.textColor
                    font.family: "JetBrains Mono"
                    font.pixelSize: 13
                    wrapMode: Text.Wrap
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
