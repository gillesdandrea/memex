import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

Rectangle {
    id: chart
    property var activity: []
    property string timeframe: "all"
    property string metric: "sessions"
    property bool busy: false
    property int pending: 0
    property bool partial: false
    property bool tokenUsageEnabled: true
    readonly property var sources: [...new Set([].concat.apply([], activity.map(bucket => Object.keys(bucket.sources))))].sort()
    readonly property real maximum: Math.max(1, ...activity.map(bucket => bucket.value))
    readonly property real ceiling: {
        let scale = Math.pow(10, Math.floor(Math.log10(maximum)));
        let normalized = maximum / scale;
        return (normalized <= 1 ? 1 : normalized <= 2 ? 2 : normalized <= 5 ? 5 : 10) * scale;
    }
    readonly property string status: !tokenUsageEnabled && metric === "tokens"
                                    ? "Token totals are incomplete · usage disabled on some machines"
                                    : busy ? "Loading activity · " + pending + " machine(s) remaining"
                                    : partial ? "Partial activity · some sources could not be read" : ""
    color: Theme.codeBackground
    radius: 12

    function providerColor(source) {
        return ({codex: "#438de1", claude: "#54ab76", cursor: "#ad7acc", copilot: "#e6a34f"})[source] || "#9a9ba2";
    }
    function compact(value) {
        return value >= 1000000 ? (value / 1000000).toLocaleString(Qt.locale(), "f", 1) + "M"
             : value >= 1000 ? (value / 1000).toLocaleString(Qt.locale(), "f", value % 1000 ? 1 : 0) + "K"
             : value.toLocaleString(Qt.locale(), "f", value % 1 ? 1 : 0);
    }
    function dateLabel(index) {
        if (!activity.length) return "";
        let date = new Date(activity[index].timestamp);
        return timeframe === "day" ? String(date.getUTCHours()).padStart(2, "0") + ":00"
                                  : date.toLocaleDateString(Qt.locale(), "MMM d");
    }
    Item {
        id: plot
        anchors { left: parent.left; right: parent.right; top: parent.top; bottom: parent.bottom; leftMargin: 43; rightMargin: 20; topMargin: 20; bottomMargin: chart.status ? 65 : 47 }
        Repeater {
            model: 3
            delegate: Item {
                required property int index
                width: plot.width
                y: plot.height * index / 2
                Rectangle { width: parent.width; height: 1; color: Theme.divider; opacity: 0.6 }
                Label {
                    anchors.right: parent.left
                    anchors.rightMargin: 8
                    y: -height / 2
                    text: chart.compact(chart.ceiling * (1 - index / 2))
                    font.pixelSize: 9
                    color: Theme.secondary
                }
            }
        }
        Row {
            anchors.fill: parent
            Repeater {
                model: chart.activity
                delegate: Item {
                    id: bar
                    required property var modelData
                    width: plot.width / Math.max(1, chart.activity.length)
                    height: plot.height
                    Column {
                        anchors.bottom: parent.bottom
                        anchors.horizontalCenter: parent.horizontalCenter
                        width: Math.max(1, Math.min(20, bar.width * 0.8))
                        Repeater {
                            model: chart.sources
                            delegate: Rectangle {
                                required property string modelData
                                width: parent.width
                                height: (bar.modelData.sources[modelData] || 0) / chart.ceiling * plot.height
                                color: chart.providerColor(modelData)
                            }
                        }
                    }
                    HoverHandler { id: hover }
                    ToolTip.visible: hover.hovered
                    ToolTip.text: new Date(modelData.timestamp).toISOString().slice(0, 16) + " UTC\n"
                                  + modelData.value.toLocaleString() + " " + chart.metric
                }
            }
        }
        Repeater {
            model: Math.min(5, chart.activity.length)
            delegate: Label {
                required property int index
                readonly property int count: Math.min(5, chart.activity.length)
                x: count <= 1 ? 0 : index * (plot.width - width) / (count - 1)
                y: plot.height + 7
                text: chart.dateLabel(count <= 1 ? 0 : Math.round(index * (chart.activity.length - 1) / (count - 1)))
                font.pixelSize: 9
                color: Theme.secondary
            }
        }
        Label {
            anchors.centerIn: parent
            visible: !chart.activity.length
            text: chart.busy ? "Loading activity…" : "No activity in this timeframe"
            font.pixelSize: 12
            color: Theme.secondary
        }
    }
    RowLayout {
        anchors.left: parent.left
        anchors.leftMargin: 20
        anchors.bottom: parent.bottom
        anchors.bottomMargin: chart.status ? 30 : 12
        spacing: 12
        Repeater {
            model: chart.sources
            delegate: RowLayout {
                required property string modelData
                spacing: 4
                Rectangle { implicitWidth: 6; implicitHeight: 6; radius: 3; color: chart.providerColor(modelData) }
                Label {
                    text: ({codex: "Codex", claude: "Claude", cursor: "Cursor", copilot: "Copilot"})[modelData] || modelData
                    font.pixelSize: 10
                    color: Theme.secondary
                }
            }
        }
    }
    Label {
        anchors { left: parent.left; right: parent.right; bottom: parent.bottom; margins: 12; leftMargin: 20 }
        visible: !!chart.status
        text: chart.status
        font.pixelSize: 10
        color: Theme.secondary
        elide: Text.ElideRight
    }
}
