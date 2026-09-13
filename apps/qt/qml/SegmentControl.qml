import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

Control {
    id: control
    property var labels: []
    property int currentIndex: 0
    signal activated(int index)
    implicitHeight: 26
    implicitWidth: row.implicitWidth + 4
    padding: 2
    background: Rectangle { color: Theme.hover; radius: height / 2 }
    contentItem: RowLayout {
        id: row
        spacing: 0
        Repeater {
            model: control.labels
            delegate: AbstractButton {
                id: segment
                required property string modelData
                required property int index
                implicitHeight: 22
                implicitWidth: Math.max(42, label.implicitWidth + 22)
                hoverEnabled: true
                Accessible.name: modelData
                onClicked: control.activated(index)
                contentItem: Text {
                    id: label
                    text: segment.modelData
                    color: Theme.text
                    font.family: Theme.fontFamily
                    font.pixelSize: 11
                    font.weight: segment.index === control.currentIndex ? Font.Medium : Font.Normal
                    horizontalAlignment: Text.AlignHCenter
                    verticalAlignment: Text.AlignVCenter
                }
                background: Rectangle {
                    radius: height / 2
                    color: segment.index === control.currentIndex ? Theme.selection : segment.hovered ? Theme.divider : "transparent"
                    border.width: segment.visualFocus ? 1 : 0
                    border.color: Theme.accent
                }
            }
        }
    }
}
