import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ItemDelegate {
    id: row
    property string symbol: "folder"
    property string count: ""
    property bool selected: false
    implicitHeight: 28
    leftPadding: 7
    rightPadding: 7
    topPadding: 4
    bottomPadding: 4
    background: Rectangle { radius: 6; color: row.selected ? Theme.selection : row.hovered ? Theme.hover : "transparent" }
    contentItem: RowLayout {
        spacing: 7
        Icon { name: row.symbol; size: 14; color: Theme.secondary }
        Label { Layout.fillWidth: true; text: row.text; font.pixelSize: 11; color: Theme.text; elide: Text.ElideRight }
        Label { text: row.count; font.pixelSize: 10; color: Theme.tertiary }
    }
}
