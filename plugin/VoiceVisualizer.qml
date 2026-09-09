// Ambient voice visualizer for omarchy-novad -- wires the ported
// `voice-node/VoiceNode.qml` (see that directory's README for
// provenance) to this plugin's own `Service.qml` state instead of
// OmaPilot's broker/store. Everything in `voice-node/` is presentational
// and generic; everything here is the omarchy-novad-specific mapping
// from this project's own phases onto the four VoiceNode understands
// (dormant | listening | thinking | answering | error).
//
// Sits alongside PopupCard/ConversationPanel in Overlay.qml -- those
// stay the interactive surfaces (confirm box, chat history, buttons);
// this is purely ambient, no input region, no keyboard focus (see
// VoiceNode.qml's own doc comment on why that's non-negotiable for a
// voice surface).

import QtQuick
import Quickshell
import Quickshell.Hyprland
import "voice-node" as VoiceNode

Item {
    id: root

    property var service: null

    // Same "follow the focused output, fall back to the first screen
    // rather than nowhere" pattern OmaPilot's Ambient.qml uses --
    // Hyprland reports nothing briefly at startup.
    readonly property string focusedScreenName:
        Hyprland.focusedMonitor ? String(Hyprland.focusedMonitor.name || "") : ""
    readonly property var activeScreen: {
        var screens = Quickshell.screens || [];
        for (var i = 0; i < screens.length; i++)
            if (String(screens[i].name || "") === root.focusedScreenName) return screens[i];
        return screens.length > 0 ? screens[0] : null;
    }

    readonly property bool conversationActive: service ? service.conversationActive : false
    readonly property string conversationPhase: service ? service.conversationPhase : ""
    readonly property string popupPhase: service ? service.popupPhase : ""

    // The OpenClaw conversation is the richer, multi-turn interaction --
    // it drives the node whenever it's running. The wake-word popup
    // (one-shot dictation/local command routing) only drives the node
    // the rest of the time, so the two state machines never fight over
    // one visual.
    readonly property string phase: {
        if (root.conversationActive) {
            switch (root.conversationPhase) {
            case "listening": return "listening";
            case "thinking": return "thinking";
            case "speaking": return "answering";
            default: return "dormant"; // idle between turns -- waiting on the talk key
            }
        }
        switch (root.popupPhase) {
        case "listening":
        case "recording":
            return "listening";
        case "transcribing":
        case "classifying":
        case "handing_off":
            return "thinking";
        case "confirming":
            // PopupCard's own confirm box is the real UI for this one;
            // "thinking" just keeps the node lit rather than blinking
            // dormant for the handful of seconds a confirmation is up.
            return "thinking";
        case "ready":
            return "answering";
        default:
            return "dormant";
        }
    }

    readonly property var latestTurn: service ? service.latestTurn : null

    // What the caption shows -- the live transcript/streaming text while
    // something's actively arriving, otherwise the most recent settled
    // text for the current source.
    readonly property string transcript: {
        if (root.conversationActive) {
            if (root.conversationPhase === "thinking" && service && service.conversationStreamingText !== "")
                return service.conversationStreamingText;
            if (root.conversationPhase === "speaking" && root.latestTurn)
                return String(root.latestTurn.spoken_summary || root.latestTurn.full_response || "");
            return service ? service.conversationPendingUserText : "";
        }
        return service ? service.popupText : "";
    }

    VoiceNode.VoiceNode {
        phase: root.phase
        transcript: root.transcript
        // No real-time TTS envelope on this project yet (tts::speak just
        // shells out to paplay, no level metering) -- VoiceNode's own
        // calm fallback for unmetered speaking covers this, same as it
        // does for OmaPilot without FFmpeg.
        speaking: root.phase === "answering" && root.conversationPhase === "speaking"
        playbackMetered: false
        playbackLevel: 0
        targetScreen: root.activeScreen
        motionEnabled: true
    }
}
