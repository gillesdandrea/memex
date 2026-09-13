import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import "." as Components

Item {
    id: home
    required property var store
    signal filterRequested()
    signal queryEdited(string text)
    property alias searchField: search

    function relativeAge(value) {
        if (!value) return "";
        let seconds = Math.max(0, (Date.now() - new Date(value).getTime()) / 1000);
        if (seconds < 60) return "now";
        if (seconds < 3600) return Math.floor(seconds / 60) + "m ago";
        if (seconds < 86400) return Math.floor(seconds / 3600) + "h ago";
        if (seconds < 604800) return Math.floor(seconds / 86400) + "d ago";
        return new Date(value).toLocaleDateString(Qt.locale(), "MMM d");
    }

    ColumnLayout {
        width: Math.min(768, Math.max(0, parent.width - 48))
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        anchors.topMargin: 24
        anchors.bottomMargin: 16
        spacing: 0
        RowLayout {
            Layout.fillWidth: true
            Layout.preferredHeight: 28
            spacing: 10
            Label { text: "Activity"; font.pixelSize: 14; font.weight: Font.DemiBold; color: Theme.text }
            Label {
                text: store.activity.reduce((sum, bucket) => sum + bucket.value, 0).toLocaleString() + " " + store.metric
                font.pixelSize: 11
                color: Theme.secondary
            }
            Item { Layout.fillWidth: true }
            SegmentControl {
                labels: ["Sessions", "Tokens"]
                currentIndex: store.metric === "tokens" ? 1 : 0
                onActivated: index => store.configure({metric: index ? "tokens" : "sessions"})
            }
            SegmentControl {
                labels: ["24H", "7D", "30D", "All"]
                currentIndex: ["day", "week", "month", "all"].indexOf(store.timeframe)
                onActivated: index => store.configure({timeframe: ["day", "week", "month", "all"][index]})
            }
        }
        ActivityChart {
            Layout.fillWidth: true
            Layout.preferredHeight: 198
            Layout.topMargin: 12
            activity: store.activity
            timeframe: store.timeframe
            metric: store.metric
            busy: store.activityBusy
            pending: store.activityPending
            partial: store.activityPartial
            tokenUsageEnabled: store.tokenUsageEnabled
        }
        Components.SearchField {
            id: search
            objectName: "homeSearchField"
            Layout.fillWidth: true
            Layout.preferredHeight: 42
            Layout.topMargin: 20
            placeholderText: "Search conversations…"
            text: store.query
            onTextEdited: home.queryEdited(text)
        }
        RowLayout {
            Layout.fillWidth: true
            Layout.topMargin: 24
            Layout.bottomMargin: 9
            Layout.preferredHeight: 26
            Label {
                text: store.query ? "Matching conversations" : "Recent conversations"
                font.pixelSize: 14
                font.weight: Font.DemiBold
                color: Theme.text
            }
            ToolButton {
                visible: !!store.project
                text: store.project.split("/").filter(Boolean).pop() || "No project"
                font.pixelSize: 11
                onClicked: projectMenu.popup()
            }
            Item { Layout.fillWidth: true }
            ToolButton {
                id: filterButton
                implicitWidth: 26
                implicitHeight: 26
                contentItem: Icon { name: "filter"; size: 16; color: Theme.secondary }
                background: Rectangle { radius: 13; color: filterButton.hovered ? Theme.hover : "transparent" }
                onClicked: home.filterRequested()
                onPressAndHold: projectMenu.popup()
                ToolTip.visible: hovered
                ToolTip.text: "Filter conversations"
                Accessible.name: "Filter conversations"
            }
            Menu {
                id: projectMenu
                MenuItem { text: "All projects"; onTriggered: store.configure({project: ""}) }
                Instantiator {
                    model: store.projects
                    delegate: MenuItem {
                        required property var modelData
                        text: modelData.project.split("/").filter(Boolean).pop() || "No project"
                        checkable: true
                        checked: store.project === modelData.project
                        onTriggered: store.configure({project: modelData.project})
                    }
                    onObjectAdded: (index, object) => projectMenu.insertItem(index + 1, object)
                    onObjectRemoved: (index, object) => projectMenu.removeItem(object)
                }
            }
        }
        ListView {
            id: list
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.minimumHeight: 80
            clip: true
            model: store.sessions
            ScrollBar.vertical: ScrollBar {}
            onAtYEndChanged: if (atYEnd && !store.applying)
                                 Qt.callLater(() => { if (!store.applying) store.moreSessions(); })
            delegate: ItemDelegate {
                id: row
                required property var modelData
                width: list.width
                height: 45
                leftPadding: 0
                rightPadding: 0
                topPadding: 5
                bottomPadding: 5
                background: Rectangle { radius: 6; color: row.hovered ? Theme.hover : "transparent" }
                contentItem: ColumnLayout {
                    spacing: 3
                    RowLayout {
                        spacing: 12
                        Label {
                            Layout.fillWidth: true
                            text: row.modelData.label || "Untitled conversation"
                            font.pixelSize: 13
                            font.weight: Font.DemiBold
                            color: Theme.text
                            elide: Text.ElideRight
                        }
                        Label { text: home.relativeAge(row.modelData.last_at); font.pixelSize: 10; color: Theme.tertiary }
                    }
                    Label {
                        Layout.fillWidth: true
                        text: [(row.modelData.project || "").split("/").filter(Boolean).pop(),
                               row.modelData.source, row.modelData.machine || "local"].filter(Boolean).join(" · ")
                        font.pixelSize: 10
                        color: Theme.secondary
                        elide: Text.ElideRight
                    }
                }
                onClicked: store.openSession(modelData)
            }
            footer: Label {
                width: list.width
                height: visible ? 36 : 0
                visible: store.pending > 0 || !store.sessions.length
                text: store.pending > 0 ? "Loading conversations…" : "No conversations match these filters."
                horizontalAlignment: Text.AlignHCenter
                verticalAlignment: Text.AlignVCenter
                font.pixelSize: 12
                color: Theme.secondary
            }
        }
    }
}
