import QtQuick
import QtQuick.Controls

TextField {
    id: field
    implicitHeight: 40
    leftPadding: 34
    rightPadding: 12
    topPadding: 8
    bottomPadding: 8
    placeholderText: "Search conversations"
    font.family: Theme.fontFamily
    font.pixelSize: 13
    color: Theme.text
    placeholderTextColor: Theme.secondary
    selectByMouse: true
    selectionColor: Theme.accent
    selectedTextColor: "white"
    background: Rectangle {
        color: Theme.background
        radius: 8
        border.color: field.activeFocus ? Theme.accent : Theme.divider
        border.width: 1
    }
    Icon { anchors.left: parent.left; anchors.leftMargin: 12; anchors.verticalCenter: parent.verticalCenter; name: "search"; size: 14 }
}
