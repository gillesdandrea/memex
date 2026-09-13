import QtQuick
import QtQuick.Controls

ToolButton {
    id: control
    property string symbol: ""
    property bool emphasized: false
    implicitWidth: 30
    implicitHeight: 30
    padding: 6
    hoverEnabled: true
    Accessible.name: text
    contentItem: Icon {
        name: control.symbol
        color: !control.enabled ? Theme.tertiary : control.emphasized ? "white" : Theme.secondary
    }
    background: Rectangle {
        radius: width / 2
        color: control.emphasized ? Theme.accent : control.down || control.hovered ? Theme.hover : "transparent"
        border.width: control.visualFocus ? 2 : 0
        border.color: Theme.accent
    }
    ToolTip.visible: hovered
    ToolTip.text: text
    ToolTip.delay: 500
}
