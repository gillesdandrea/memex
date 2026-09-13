import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ItemDelegate {
    id: row
    required property var session
    property bool selected: false
    height: 61
    leftPadding: 9
    rightPadding: 9
    topPadding: 6
    bottomPadding: 6
    background: Rectangle { radius: 7; color: row.selected ? Theme.selection : row.hovered ? Theme.hover : "transparent" }
    contentItem: ColumnLayout {
        spacing: 2
        RowLayout {
            Label {
                Layout.fillWidth: true
                text: (row.session.project || "").split("/").filter(Boolean).pop() || "No project"
                font.pixelSize: 12; font.weight: Font.DemiBold; color: Theme.text
                elide: Text.ElideRight
            }
            Label {
                text: row.session.last_at ? new Date(row.session.last_at).toLocaleDateString(Qt.locale(), "MMM d") : ""
                font.pixelSize: 10; color: Theme.tertiary
            }
        }
        Label {
            Layout.fillWidth: true
            text: row.session.label || "Untitled conversation"
            font.pixelSize: 12; color: Theme.text; elide: Text.ElideRight
        }
        Label {
            Layout.fillWidth: true
            text: [row.session.source, row.session.machine || "local", row.session.conversation_kind === "subagent" ? "Subagent" : ""].filter(Boolean).join(" · ")
            font.pixelSize: 10; color: Theme.secondary; elide: Text.ElideRight
        }
    }
}
