// One row in BarWidget.qml's context menu -- the ported (and
// adapted, see BarWidget.qml's header comment) version of nova's
// Electron tray right-click menu.
//
// Full-width with a left-aligned label and a themed hover fill,
// rather than PopupButton.qml's small pill shape: PopupButton is a
// row of equal-weight action buttons (Dismiss / Stop conversation);
// this is a top-to-bottom menu list, where the conventional shape is
// a full-width row that highlights on hover. `tint` colors the label
// itself (not just the hover border the way PopupButton does) so a
// row like "Quit" can read as red at rest, not only on hover.

import QtQuick

Rectangle {
    id: root

    property string label: ""
    property color tint: OmarchyTheme.foreground
    signal clicked()

    // OmarchyTheme's colors are plain `string` properties (see that
    // file's own doc comment on why), so `.r`/`.g`/`.b` only resolve
    // once coerced into a real `color`-typed value -- same trick
    // ConversationPanel.qml's `accent`/`danger` properties use.
    readonly property color hoverAccent: OmarchyTheme.accent

    width: parent ? parent.width : (labelText.implicitWidth + 28)
    implicitHeight: 30
    radius: 6
    color: mouseArea.containsMouse
        ? Qt.rgba(hoverAccent.r, hoverAccent.g, hoverAccent.b, 0.14)
        : "transparent"

    Behavior on color {
        ColorAnimation { duration: 100 }
    }

    Text {
        id: labelText
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: 12
        anchors.rightMargin: 12
        anchors.verticalCenter: parent.verticalCenter
        text: root.label
        color: root.tint
        font.pixelSize: 13
        font.weight: Font.Medium
        elide: Text.ElideRight
    }

    MouseArea {
        id: mouseArea
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: root.clicked()
    }
}
