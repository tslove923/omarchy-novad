// The rotating conic-gradient border from nova's Electron popup
// (electron/renderer/popup.css: `.border-glow`), ported for real via
// Qt5Compat.GraphicalEffects (confirmed present: quickshell-git 0.3.0 /
// Qt 6.11 ship ConicalGradient.qml, OpacityMask.qml, RectangularGlow.qml)
// instead of the placeholder opacity-pulse the first QML pass used.
//
// Geometry mirrors nova's CSS composition exactly: `.popup-inner`
// (the card) sits at z-index 1 with its own opaque fill; `.border-glow`
// sits at z-index 0, sized via `inset:-2px` so it forms a strokeWidth
// ring framing the card from *outside* rather than a border painted
// over the card's own edge pixels. This component reproduces that by
// sizing itself to `holeWidth`/`holeHeight` (the card's size) plus
// `strokeWidth` for the ring and `glowSpread` for the outward blur, and
// cutting a card-shaped transparent hole out of the ring via
// OpacityMask. The caller centers this behind the card (see
// NovadPopup.qml) so the card's own opaque fill covers the hole,
// leaving only the ring-and-glow frame visible around it.
//
// Animation per mode, matching nova's CSS keyframes:
//   - "recording": gradient spins at 40deg/s (9s/revolution), blue glow
//   - "transcribing"/"classifying": opacity strobes 0.4<->1.0 twice a
//     second, purple glow
//   - "ready": one flash-and-fade — full opacity + glow easing to 30%
//     opacity / no glow over 1.2s (CSS: result-flash, forwards)
//   - "breathing": three slow opacity pulses, blue glow (nova's edit-
//     window / waiting look; not currently reached by any novad
//     PopupPhase, kept for a future phase that wants it)
//   - "confirming": ring static (no spin) at reduced opacity, amber
//     glow — nova's CSS reused
//     "rotating" here ("mic is active during confirm" in the Electron
//     build), but novad's Confirming phase isn't listening for
//     anything (see PopupState.qml), so a spinning ring would
//     misleadingly suggest it's still recording.
//   - "idle" (or unrecognized): ring and glow hidden.

import QtQuick
import Qt5Compat.GraphicalEffects

Item {
    id: root

    property real holeWidth: 100
    property real holeHeight: 100
    property real holeRadius: 10
    property real strokeWidth: 2
    property real glowSpread: 10
    property string mode: "idle"

    readonly property bool active: mode !== "idle"
    readonly property real outerRadius: holeRadius + strokeWidth

    implicitWidth: holeWidth + 2 * (strokeWidth + glowSpread)
    implicitHeight: holeHeight + 2 * (strokeWidth + glowSpread)

    // ── Ring mask: opaque outer rounded-rect minus a card-shaped
    //    transparent hole, used as an OpacityMask source so the conic
    //    gradient only shows through the strokeWidth-thick frame. ──
    Item {
        id: ringMaskSource
        anchors.centerIn: parent
        width: root.holeWidth + 2 * root.strokeWidth
        height: root.holeHeight + 2 * root.strokeWidth
        layer.enabled: true
        visible: false

        Rectangle {
            anchors.fill: parent
            radius: root.outerRadius
            color: "black"
        }
        Rectangle {
            anchors.centerIn: parent
            width: root.holeWidth
            height: root.holeHeight
            radius: root.holeRadius
            color: "white"
            layer.enabled: true
        }
    }

    ConicalGradient {
        id: gradient
        anchors.fill: ringMaskSource
        cached: false

        property real spinAngle: 0

        angle: spinAngle
        gradient: Gradient {
            GradientStop { position: 0.0; color: "#0294f2" }
            GradientStop { position: 0.5; color: "#5d13df" }
            GradientStop { position: 0.85; color: "#eb31e9" }
            GradientStop { position: 1.0; color: "#0294f2" }
        }

        RotationAnimation on spinAngle {
            running: root.mode === "recording"
            loops: Animation.Infinite
            from: 0; to: 360
            duration: 9000 // 40deg/s, matches nova's CSS exactly
        }
    }

    OpacityMask {
        id: ring
        anchors.fill: ringMaskSource
        source: gradient
        maskSource: ringMaskSource
        cached: false
        visible: root.active
        opacity: 0.3
    }

    // ── Outer glow (CSS box-shadow equivalent), traces the ring's
    //    outer rounded rect and bleeds outward into glowSpread. ──
    RectangularGlow {
        id: glow
        anchors.fill: ringMaskSource
        cornerRadius: root.outerRadius
        glowRadius: root.glowSpread
        spread: 0.15
        color: Qt.rgba(0, 0, 0, 0)
        visible: root.active
    }

    function glowColorFor(m) {
        switch (m) {
        case "recording": return Qt.rgba(2 / 255, 148 / 255, 242 / 255, 0.4);
        case "transcribing":
        case "classifying": return Qt.rgba(93 / 255, 19 / 255, 223 / 255, 0.5);
        case "ready": return Qt.rgba(166 / 255, 227 / 255, 161 / 255, 0.8);
        case "breathing": return Qt.rgba(137 / 255, 180 / 255, 250 / 255, 0.6);
        case "confirming": return Qt.rgba(250 / 255, 179 / 255, 135 / 255, 0.35);
        default: return Qt.rgba(0, 0, 0, 0);
        }
    }

    // `ready` and `breathing` are one-shot/finite (nova's `result-flash
    // 1.2s forwards` and `breathe 1s * 3`); everything else is a steady
    // state or an infinite loop, handled by the `states` block below.
    SequentialAnimation {
        running: root.mode === "ready"
        ScriptAction { script: { ring.opacity = 1.0; glow.color = root.glowColorFor("ready"); } }
        PauseAnimation { duration: 360 } // ~30% of 1.2s at full opacity
        ParallelAnimation {
            NumberAnimation { target: ring; property: "opacity"; to: 0.3; duration: 840; easing.type: Easing.OutQuad }
            ColorAnimation { target: glow; property: "color"; to: Qt.rgba(0, 0, 0, 0); duration: 840 }
        }
    }

    SequentialAnimation {
        running: root.mode === "breathing"
        loops: 3
        ParallelAnimation {
            NumberAnimation { target: ring; property: "opacity"; from: 0.3; to: 1.0; duration: 500; easing.type: Easing.InOutQuad }
            ColorAnimation { target: glow; property: "color"; from: Qt.rgba(0, 0, 0, 0); to: root.glowColorFor("breathing"); duration: 500 }
        }
        ParallelAnimation {
            NumberAnimation { target: ring; property: "opacity"; from: 1.0; to: 0.3; duration: 500; easing.type: Easing.InOutQuad }
            ColorAnimation { target: glow; property: "color"; from: root.glowColorFor("breathing"); to: Qt.rgba(0, 0, 0, 0); duration: 500 }
        }
    }

    SequentialAnimation {
        running: root.mode === "transcribing" || root.mode === "classifying"
        loops: Animation.Infinite
        ScriptAction { script: glow.color = root.glowColorFor(root.mode) }
        NumberAnimation { target: ring; property: "opacity"; to: 0.4; duration: 500; easing.type: Easing.InOutQuad }
        NumberAnimation { target: ring; property: "opacity"; to: 1.0; duration: 500; easing.type: Easing.InOutQuad }
    }

    states: [
        State {
            name: "recording"
            when: root.mode === "recording"
            PropertyChanges { target: ring; opacity: 1.0 }
            PropertyChanges { target: glow; color: root.glowColorFor("recording") }
        },
        State {
            name: "confirming"
            when: root.mode === "confirming"
            PropertyChanges { target: ring; opacity: 0.6 }
            PropertyChanges { target: glow; color: root.glowColorFor("confirming") }
        },
        State {
            name: "idle"
            when: root.mode === "idle"
            PropertyChanges { target: ring; opacity: 0.0 }
            PropertyChanges { target: glow; color: Qt.rgba(0, 0, 0, 0) }
        }
    ]
}
