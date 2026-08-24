// Small pill button used by NovadPopup's confirm/review bars.
// Mirrors nova's popup.css .btn / .btn-primary / .btn-danger styling.

import QtQuick

Rectangle {
    id: root

    property string label: ""
    property color tint: "#89b4fa"
    property bool primary: false
    signal clicked()

    implicitWidth: labelText.implicitWidth + 20
    implicitHeight: 26
    radius: 6
    color: primary ? Qt.rgba(tint.r, tint.g, tint.b, 0.18) : "#313244"
    border.width: 1
    border.color: mouseArea.containsMouse ? tint : "#45475a"

    Behavior on border.color {
        ColorAnimation { duration: 120 }
    }

    Text {
        id: labelText
        anchors.centerIn: parent
        text: root.label
        color: root.primary ? root.tint : "#cdd6f4"
        font.pixelSize: 12
        font.weight: Font.Medium
    }

    MouseArea {
        id: mouseArea
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: root.clicked()
    }
}
