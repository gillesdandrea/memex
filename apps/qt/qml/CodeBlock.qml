import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import "Presentation.js" as Presentation

Control {
    id: root
    property string text: ""
    property string language: "text"
    property var viewState: null
    property string sourceId: ""
    property string stateField: "codeShowAll"
    property bool localShowAll: false
    property bool showAll: viewState ? viewState.stateValue(sourceId,stateField,false) : localShowAll
    readonly property string shownText: showAll ? text : text.slice(0,16000)
    signal copyText(string text)
    padding: 10
    background: Rectangle { color: Theme.codeBackground; radius: 6 }
    HoverHandler { id: hover }
    contentItem: ColumnLayout {
        spacing: 6
        RowLayout {
            Layout.fillWidth: true
            Layout.preferredHeight: 16
            Label {
                text: root.language || "code"
                color: Theme.tertiary
                font.family: Theme.fontFamily
                font.pixelSize: 10
                Layout.fillWidth: true
            }
            ToolButton {
                implicitWidth: 20
                implicitHeight: 20
                opacity: hover.hovered || activeFocus ? 1 : 0
                Accessible.name: "Copy code"
                ToolTip.visible: hovered
                ToolTip.text: "Copy code"
                contentItem: Icon { name: "copy"; size: 12; color: Theme.secondary }
                background: Rectangle { color: parent.hovered ? Theme.hover : "transparent"; radius: 3 }
                onClicked: root.copyText(root.text)
            }
        }
        ScrollView {
            id: scroll
            objectName: "codeScroll"
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(400, code.implicitHeight + 12)
            clip: true
            Flickable {
                contentWidth: Math.max(width, code.implicitWidth)
                contentHeight: code.implicitHeight
                boundsBehavior: Flickable.StopAtBounds
                TextArea {
                    id: code
                    objectName: "codeText"
                    readOnly: true
                    selectByMouse: true
                    persistentSelection: true
                    wrapMode: TextEdit.NoWrap
                    textFormat: TextEdit.RichText
                    font.family: "monospace"
                    font.pixelSize: 12
                    color: Theme.text
                    padding: 0
                    text: '<pre style="margin:0">' + Presentation.highlightCode(root.shownText, root.language) + '</pre>'
                    background: Item { }
                }
            }
        }
        ToolButton {
            visible: root.text.length > 16000
            text: root.showAll ? "Show less" : "Show all " + root.text.length + " characters"
            font.family: Theme.fontFamily
            font.pixelSize: 11
            palette.buttonText: Theme.secondary
            onClicked: {
                if (root.viewState) root.viewState.updateState(root.sourceId,root.stateField,!root.showAll);
                else root.localShowAll=!root.showAll;
            }
        }
    }
}
