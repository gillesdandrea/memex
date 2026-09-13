import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import "Presentation.js" as Presentation

Item {
    id: root
    property var records: []
    property var session: null
    property bool rawMode: false
    property string findText: ""
    property bool hasEarlier: false
    property bool hasLater: false
    property bool loading: false
    property var items: Presentation.group(records || [], rawMode, !!findText)
    property var expandedItems: ({})
    property string targetRecord: ""
    property string readingAnchor: ""
    property real readingOffset: 0
    property int findOccurrence: 0
    signal earlier()
    signal later()
    signal openLink(string url)
    signal copyText(string text)
    signal readingPositionChanged(string recordId, real offset)

    function stateValue(sourceId, field, fallback) {
        var state=expandedItems[sourceId];
        if (typeof state === "boolean") return field === "open" ? state : fallback;
        return state && Object.prototype.hasOwnProperty.call(state,field) ? state[field] : fallback;
    }
    function updateState(sourceId, field, value) {
        var snapshot=Object.assign({},expandedItems), state=snapshot[sourceId];
        state=typeof state === "boolean" ? {open:state} : Object.assign({},state || {});
        state[field]=value;
        snapshot[sourceId]=state;
        expandedItems=snapshot;
    }
    function groupState(item, field, fallback) {
        for (var i=0;i<item.records.length;i++) {
            var value=stateValue(item.records[i].source_id,field,undefined);
            if (value!==undefined) return value;
        }
        return fallback;
    }
    function updateGroupState(item, field, value) {
        // A group can gain a new first record when an earlier page arrives.
        // Retain the preference against every source member, not its row index.
        var snapshot=Object.assign({},expandedItems);
        item.records.forEach(function(entry) {
            var state=snapshot[entry.source_id];
            state=typeof state === "boolean" ? {open:state} : Object.assign({},state || {});
            state[field]=value;
            snapshot[entry.source_id]=state;
        });
        expandedItems=snapshot;
    }

    function rememberPosition() {
        var index = list.indexAt(28, list.contentY + 2);
        if (index < 0 || index >= items.length) return;
        var delegate = list.itemAtIndex(index);
        readingAnchor = items[index].records[0].source_id;
        readingOffset = delegate ? list.contentY - delegate.y : 0;
        readingPositionChanged(readingAnchor, readingOffset);
    }
    function restoreAnchor(id, offset) {
        var index = Presentation.indexForRecord(items, id);
        if (index < 0) return false;
        list.positionViewAtIndex(index, ListView.Beginning);
        var delegate = list.itemAtIndex(index);
        if (delegate) list.contentY = delegate.y + (offset || 0);
        return true;
    }
    function positionRecord(id) {
        var index=Presentation.indexForRecord(items,id);
        if (index<0) return;
        list.forceLayout();
        list.positionViewAtIndex(index,ListView.Beginning);
        var delegate=list.itemAtIndex(index);
        if (delegate) {
            delegate.revealRecord(id);
            list.forceLayout();
            list.positionViewAtIndex(index,ListView.Beginning);
        }
    }
    function jumpToRecord(id, occurrence) {
        var index = Presentation.indexForRecord(items, id);
        if (index < 0) return false;
        targetRecord = id;
        findOccurrence = occurrence || 0;
        // A projected request shares its source ID with preceding context. Only
        // expand a disclosure when the target actually lives inside that group.
        if (items[index].collapsible) updateGroupState(items[index], "open", true);
        positionRecord(id);
        Qt.callLater(function() {
            if (root.targetRecord===id && !root.findText) root.positionRecord(id);
        });
        return true;
    }
    onSessionChanged: { targetRecord = ""; readingAnchor = ""; readingOffset = 0; }

    ListView {
        id: list
        objectName: "transcriptList"
        anchors.fill: parent
        clip: true
        spacing: 20
        // Rich delegates are already virtualized; background cache incubation
        // otherwise outlives hidden windows during session switches/teardown.
        cacheBuffer: 0
        reuseItems: false
        model: root.items
        ScrollBar.vertical: ScrollBar { }
        onMovementEnded: {
            root.rememberPosition();
            if (!root.loading && root.hasEarlier && contentY <= originY + 100) root.earlier();
            else if (!root.loading && root.hasLater && atYEnd) root.later();
        }
        onContentYChanged: positionTimer.restart()
        Timer { id: positionTimer; interval: 150; onTriggered: root.rememberPosition() }
        header: Item {
            width: list.width
            height: root.hasEarlier || root.loading ? 56 : 16
            Button {
                anchors.centerIn: parent
                visible: root.hasEarlier
                text: root.loading ? "Loading…" : "Load earlier messages"
                enabled: !root.loading
                onClicked: { root.rememberPosition(); root.earlier(); }
            }
        }
        footer: Item {
            width: list.width
            // Leave enough trailing reading space to position the final source
            // record at the top, instead of clamping a jump to several rows back.
            height: root.hasLater ? 56 : list.height
            Button {
                anchors.centerIn: parent
                visible: root.hasLater
                text: root.loading ? "Loading…" : "Load later messages"
                enabled: !root.loading
                onClicked: root.later()
            }
        }
        delegate: Rectangle {
            id: card
            required property var modelData
            required property int index
            objectName: "transcriptCard:" + modelData.id
            property bool expanded: !modelData.collapsible || root.groupState(modelData,"open",modelData.attention)
            property bool localRaw: !root.findText && root.groupState(modelData,"raw",root.rawMode)
            property int activityLimit: root.groupState(modelData,"activityLimit",12)
            property var visibleActivities: modelData.activities.slice(0, activityLimit)
            property bool isTarget: modelData.records.some(function(e) { return e.record_id === root.targetRecord || e.source_id === root.targetRecord; })
            width: list.width
            height: content.implicitHeight
            color: "transparent"
            activeFocusOnTab: true
            HoverHandler { id: cardHover }
            TapHandler {
                acceptedButtons: Qt.RightButton
                onTapped: messageMenu.popup()
            }
            Menu {
                id: messageMenu
                MenuItem { text: "Copy text"; onTriggered: root.copyText(Presentation.itemBody(card.modelData)) }
                MenuItem { text: "Copy source JSON"; onTriggered: root.copyText(Presentation.rawBody(card.modelData)) }
                MenuSeparator { }
                MenuItem {
                    text: card.localRaw ? "Show rendered message" : "Show source JSON"
                    onTriggered: root.updateGroupState(card.modelData,"raw",!card.localRaw)
                }
            }
            ToolButton {
                anchors.right: parent.right
                anchors.rightMargin: 26
                anchors.top: parent.top
                z: 2
                width: 23
                height: 23
                visible: cardHover.hovered || activeFocus || card.activeFocus
                Accessible.name: "Copy message"
                ToolTip.visible: hovered
                ToolTip.text: "Copy message"
                contentItem: Icon { name: "copy"; size: 13; color: Theme.secondary }
                background: Rectangle { color: parent.hovered ? Theme.hover : Theme.background; radius: 4 }
                onClicked: root.copyText(card.localRaw ? Presentation.rawBody(card.modelData) : Presentation.itemBody(card.modelData))
            }
            function revealRecord(id) {
                for (var i=0;i<modelData.activities.length;i++) {
                    if (modelData.activities[i].some(function(e) { return e.record_id===id || e.source_id===id; })) root.updateGroupState(modelData,"activityLimit",Math.max(activityLimit,i+1));
                }
            }
            ColumnLayout {
                id: content
                anchors { left: parent.left; right: parent.right; top: parent.top; leftMargin: 26; rightMargin: 26 }
                spacing: 8
                ToolButton {
                    Layout.fillWidth: true
                    visible: card.modelData.collapsible
                    implicitHeight: 24
                    leftPadding: 0
                    rightPadding: 28
                    topPadding: 2
                    bottomPadding: 2
                    Accessible.name: (card.expanded ? "Collapse " : "Expand ") + card.modelData.title
                    background: Item { }
                    onClicked: root.updateGroupState(card.modelData,"open",!card.expanded)
                    contentItem: RowLayout {
                        spacing: 7
                        Icon {
                            visible: card.modelData.activities.length === 1
                            name: Presentation.instruction(card.modelData.records[0].record) ? "file" : "terminal"
                            size: 13
                            color: card.modelData.attention ? "#aa713e" : Theme.secondary
                        }
                        Label {
                            Layout.maximumWidth: Math.max(0,content.width-64)
                            text: card.modelData.completed ? "Completed work" : (card.modelData.activities.length === 1 ? Presentation.title(card.modelData.activities[0]) : (Presentation.instruction(card.modelData.records[0].record) ? "Session context" : card.modelData.activities.length + " tool calls"))
                            font.family: Theme.fontFamily
                            font.pixelSize: 12
                            font.weight: Font.Normal
                            color: card.modelData.attention ? "#aa713e" : Theme.secondary
                            elide: Text.ElideRight
                        }
                        Icon { name: "chevron"; size: 10; color: Theme.tertiary; rotation: card.expanded ? 90 : 0 }
                        Item { Layout.fillWidth: true }
                    }
                }
                Loader {
                    Layout.fillWidth: true
                    active: card.expanded && card.localRaw
                    visible: active
                    sourceComponent: recordText
                    onLoaded: { item.textValue = Presentation.rawBody(card.modelData); item.code = true; item.sourceId=card.modelData.records[0].source_id; item.stateField="rawShowAll"; item.stateMembers=card.modelData; }
                }
                Repeater {
                    id: activities
                    model: card.expanded && !card.localRaw ? card.visibleActivities : []
                    delegate: ColumnLayout {
                        required property var modelData
                        required property int index
                        Layout.fillWidth: true
                        spacing: 8
                        Label {
                            visible: card.modelData.collapsible && card.modelData.activities.length > 1
                            Layout.fillWidth: true
                            text: Presentation.title(modelData)
                            font.family: Theme.fontFamily
                            font.pixelSize: 12
                            font.weight: Font.Medium
                            color: Theme.secondary
                            wrapMode: Text.Wrap
                        }
                        Repeater {
                            model: modelData
                            delegate: ColumnLayout {
                                id: entryView
                                required property var modelData
                                Layout.fillWidth: true
                                spacing: 6
                                Label {
                                    visible: ["tool_use", "tool_result"].indexOf(entryView.modelData.record.role) >= 0
                                    text: entryView.modelData.record.role === "tool_use" ? "Input" : "Output"
                                    color: Theme.tertiary
                                    font.pixelSize: 11
                                }
                                Loader {
                                    Layout.fillWidth: true
                                    visible: !Presentation.activity(entryView.modelData.record) || !!root.findText
                                    active: visible
                                    sourceComponent: recordText
                                    onLoaded: {
                                        item.textValue = Presentation.body(entryView.modelData);
                                        item.code = Presentation.activity(entryView.modelData.record) || Presentation.instruction(entryView.modelData.record);
                                        item.recordId = entryView.modelData.record_id;
                                        item.sourceId = entryView.modelData.source_id;
                                    }
                                }
                                ToolBody {
                                    Layout.fillWidth: true
                                    visible: Presentation.activity(entryView.modelData.record) && !root.findText
                                    record: visible ? entryView.modelData.record : ({})
                                    viewState: root
                                    sourceId: entryView.modelData.source_id
                                    onCopyText: function(text) { root.copyText(text); }
                                    onOpenLink: function(url) { root.openLink(url); }
                                }
                                Repeater {
                                    model: Presentation.attachments(entryView.modelData.record)
                                    delegate: ColumnLayout {
                                        required property var modelData
                                        required property int index
                                        Layout.fillWidth: true
                                        Label { text: modelData.label; font.bold: true }
                                        Image {
                                            id: thumbnail
                                            objectName: "attachmentThumbnail"
                                            visible: !!modelData.image && !!Presentation.inlineImageUrl(modelData.url, root.session)
                                            Layout.fillWidth: true
                                            Layout.preferredHeight: visible ? Math.min(360, implicitHeight || 240) : 0
                                            fillMode: Image.PreserveAspectFit
                                            source: visible ? Presentation.inlineImageUrl(modelData.url, root.session) : ""
                                            asynchronous: true
                                            sourceSize.width: 1200
                                            TapHandler {
                                                onTapped: { imagePreview.source = thumbnail.source; imageDialog.open(); }
                                            }
                                        }
                                        Button {
                                            visible: !!modelData.url && String(modelData.url).indexOf("data:") !== 0
                                            text: modelData.image ? "Open image" : "Open attachment"
                                            onClicked: root.openLink(modelData.url)
                                        }
                                        Label {
                                            visible: !!modelData.notice
                                            Layout.fillWidth: true
                                            text: modelData.notice || ""
                                            wrapMode: Text.Wrap
                                        }
                                        Loader {
                                            Layout.fillWidth: true
                                            active: !!modelData.code
                                            visible: active
                                            sourceComponent: recordText
                                            onLoaded: { item.textValue=modelData.code; item.code=true; item.sourceId=entryView.modelData.source_id; item.stateField="attachment:"+index+":showAll"; }
                                        }
                                    }
                                }
                            }
                        }
                        Item { Layout.fillWidth: true; height: 8; visible: index < card.visibleActivities.length - 1 }
                    }
                }
                ToolButton {
                    visible: card.expanded && !card.localRaw && card.activityLimit < card.modelData.activities.length
                    text: "Show more activities (" + (card.modelData.activities.length - card.activityLimit) + " remaining)"
                    font.family: Theme.fontFamily
                    font.pixelSize: 12
                    palette.buttonText: Theme.secondary
                    onClicked: root.updateGroupState(card.modelData,"activityLimit",card.activityLimit+12)
                }
            }
        }
    }
    Component {
        id: recordText
        ColumnLayout {
            id: textBlock
            property string textValue: ""
            property bool code: false
            property string stateField: "text:" + recordId + ":showAll"
            property var stateMembers: null
            property bool showAll: stateMembers ? root.groupState(stateMembers,stateField,false) : root.stateValue(sourceId,stateField,false)
            property string recordId: ""
            property string sourceId: ""
            property bool targeted: !!root.targetRecord && (recordId === root.targetRecord || sourceId === root.targetRecord)
            readonly property bool bounded: textValue.length > 16000 && !showAll && !(targeted && root.findText)
            Layout.fillWidth: true
            spacing: 4
            HoverHandler { id: textHover }
            function selectMatch() {
                if (!targeted || !root.findText) return;
                var haystack = editor.text.toLowerCase(), query=root.findText.toLowerCase(), start=-1;
                for (var i=0;i<=root.findOccurrence;i++) {
                    var next=haystack.indexOf(query,start<0 ? 0 : start+query.length);
                    if (next<0) break;
                    start=next;
                }
                if (start>=0) {
                    editor.select(start,start+query.length);
                    Qt.callLater(function() {
                        var position=editor.mapToItem(list.contentItem,0,editor.cursorRectangle.y);
                        list.contentY=Math.max(list.originY,Math.min(position.y-60,list.originY+Math.max(0,list.contentHeight-list.height)));
                    });
                }
            }
            onTargetedChanged: Qt.callLater(selectMatch)
            Connections {
                target: root
                function onFindTextChanged() { Qt.callLater(textBlock.selectMatch); }
                function onFindOccurrenceChanged() { Qt.callLater(textBlock.selectMatch); }
            }
            TextArea {
                id: editor
                objectName: "transcriptText"
                Layout.fillWidth: true
                visible: textBlock.code || !!root.findText
                readOnly: true
                selectByMouse: true
                persistentSelection: true
                wrapMode: TextEdit.Wrap
                textFormat: TextEdit.PlainText
                font.family: textBlock.code ? "monospace" : Theme.fontFamily
                font.pixelSize: 13
                color: Theme.text
                text: textBlock.bounded ? textBlock.textValue.slice(0,16000) : textBlock.textValue
                padding: textBlock.code ? 10 : 0
                background: Rectangle { radius: 6; color: textBlock.code ? Theme.codeBackground : "transparent" }
                onLinkActivated: function(link) { root.openLink(link); }
                onTextChanged: Qt.callLater(textBlock.selectMatch)
            }
            Repeater {
                model: !textBlock.code && !root.findText ? Presentation.markdownBlocks(textBlock.bounded ? textBlock.textValue.slice(0,16000) : textBlock.textValue) : []
                delegate: ColumnLayout {
                    required property var modelData
                    required property int index
                    readonly property var safeContent: modelData.code ? ({text:"",images:[]}) : Presentation.safeMarkdown(modelData.text)
                    Layout.fillWidth: true
                    TextArea {
                        visible: !modelData.code
                        Layout.fillWidth: true
                        readOnly: true
                        selectByMouse: true
                        persistentSelection: true
                        wrapMode: TextEdit.Wrap
                        textFormat: TextEdit.MarkdownText
                        objectName: "markdownText"
                        text: safeContent.text
                        font.family: Theme.fontFamily
                        font.pixelSize: 13
                        font.weight: Font.Normal
                        color: Theme.text
                        padding: 0
                        background: Item { }
                        onLinkActivated: function(link) { root.openLink(link); }
                    }
                    Repeater {
                        model: safeContent.images
                        delegate: ColumnLayout {
                            required property var modelData
                            Layout.fillWidth: true
                            Image {
                                id: markdownThumbnail
                                objectName: "markdownThumbnail"
                                visible: !!Presentation.inlineImageUrl(modelData.url,root.session)
                                source: visible ? Presentation.inlineImageUrl(modelData.url,root.session) : ""
                                Layout.fillWidth: true
                                Layout.preferredHeight: visible ? Math.min(360,implicitHeight || 240) : 0
                                fillMode: Image.PreserveAspectFit
                                asynchronous: true
                                sourceSize.width: 1200
                                TapHandler { onTapped: { imagePreview.source=markdownThumbnail.source; imageDialog.open(); } }
                            }
                            Button {
                                text: "Open " + modelData.label
                                onClicked: {
                                    var inlineUrl=Presentation.inlineImageUrl(modelData.url,root.session);
                                    if (inlineUrl.indexOf("data:")===0) { imagePreview.source=inlineUrl; imageDialog.open(); }
                                    else root.openLink(modelData.url);
                                }
                            }
                        }
                    }
                    CodeBlock {
                        visible: modelData.code
                        Layout.fillWidth: true
                        text: modelData.code ? modelData.text : ""
                        language: modelData.language
                        viewState: root
                        sourceId: textBlock.sourceId
                        stateField: "code:" + textBlock.recordId + ":" + index + ":showAll"
                        onCopyText: function(text) { root.copyText(text); }
                    }
                }
            }
            RowLayout {
                visible: textBlock.textValue.length > 16000
                Label { text: textBlock.bounded ? "Showing first 16,000 of " + textBlock.textValue.length + " characters" : textBlock.textValue.length + " characters"; color: Theme.tertiary; font.family: Theme.fontFamily; font.pixelSize: 11 }
                ToolButton {
                    text: textBlock.bounded ? "Show all" : "Show less"
                    font.family: Theme.fontFamily
                    font.pixelSize: 11
                    palette.buttonText: Theme.secondary
                    onClicked: {
                        if (textBlock.stateMembers) root.updateGroupState(textBlock.stateMembers,textBlock.stateField,!textBlock.showAll);
                        else root.updateState(textBlock.sourceId,textBlock.stateField,!textBlock.showAll);
                    }
                }
                ToolButton { text: "Copy all"; opacity: textHover.hovered || activeFocus ? 1 : 0; font.family: Theme.fontFamily; font.pixelSize: 11; palette.buttonText: Theme.secondary; onClicked: root.copyText(textBlock.textValue) }
            }
        }
    }
    Dialog {
        id: imageDialog
        objectName: "imageDialog"
        title: "Image"
        modal: true
        anchors.centerIn: parent
        width: Math.min(root.width - 24, 1100)
        height: Math.min(root.height - 24, 850)
        standardButtons: Dialog.Close
        Image { id: imagePreview; anchors.fill: parent; fillMode: Image.PreserveAspectFit; asynchronous: true }
    }
}
