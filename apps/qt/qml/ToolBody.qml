import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import "Presentation.js" as Presentation

ColumnLayout {
    id: root
    property var record: ({})
    property var viewState: null
    property string sourceId: ""
    property int localSectionLimit: 24
    property int sectionLimit: viewState ? viewState.stateValue(sourceId,"toolSectionLimit",24) : localSectionLimit
    readonly property var sections: Presentation.toolSections(record)
    signal copyText(string text)
    signal openLink(string url)
    spacing: 8
    Repeater {
        model: root.sections.slice(0, root.sectionLimit)
        delegate: ColumnLayout {
            required property var modelData
            required property int index
            Layout.fillWidth: true
            Label { text: modelData.label; visible: !!text; font.family: Theme.fontFamily; font.pixelSize: 11; font.weight: Font.Medium; color: Theme.secondary }
            ToolButton { visible: !!modelData.path; text: modelData.text; font.family: Theme.fontFamily; font.pixelSize: 12; palette.buttonText: Theme.accent; onClicked: root.openLink(modelData.path) }
            CodeBlock {
                Layout.fillWidth: true
                text: modelData.text
                language: modelData.language
                viewState: root.viewState
                sourceId: root.sourceId
                stateField: "tool:"+index+":showAll"
                onCopyText: function(text) { root.copyText(text); }
            }
        }
    }
    ToolButton {
        visible: root.sections.length > root.sectionLimit
        text: "Show more fields (" + (root.sections.length - root.sectionLimit) + " remaining)"
        font.family: Theme.fontFamily
        font.pixelSize: 11
        palette.buttonText: Theme.secondary
        onClicked: {
            if (root.viewState) root.viewState.updateState(root.sourceId,"toolSectionLimit",root.sectionLimit+24);
            else root.localSectionLimit+=24;
        }
    }
}
