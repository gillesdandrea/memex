import QtQuick

// The Rust controller owns data, criteria, request lifetimes and reader state.
// This adapter applies changed properties without replacing unchanged list models.
QtObject {
    id: adapter
    required property var backend
    property string query: ""
    property string project: ""
    property string machine: "all"
    property string provider: ""
    property string timeframe: "all"
    property string origin: "interactive"
    property string projectSort: "activity"
    property string metric: "sessions"
    property var machines: []
    property var projects: []
    property var sessions: []
    property var errors: ({})
    property var activity: []
    property bool tokenUsageEnabled: true
    property bool activityPartial: false
    property bool activityBusy: false
    property int activityPending: 0
    property bool activityComplete: false
    property var activityFailures: ({})
    property int total: -1
    property int pending: 0
    property int limit: 200
    property bool canLoadMore: false
    property var selected: null
    property var records: []
    property int offset: 0
    property int recordTotal: 0
    property bool readerBusy: false
    property string readerError: ""
    property bool metadataBusy: false
    property string metadataError: ""
    property string findText: ""
    property var matches: []
    property int matchIndex: -1
    property bool finding: false
    property string findError: ""
    property int scannedRecords: 0
    property string anchor: ""
    property real anchorOffset: 0
    property var expansion: ({})
    property var callbacks: ({})
    property int sequence: 0
    property int navigationSerial: 0
    property int readerOpenedSerial: 0
    property bool applying: false
    signal positionRecord(string id, int occurrence)
    signal readerOpened
    signal restorePosition(string id, real offset, var expansion)

    property Timer responsePump: Timer {
        interval: 25
        running: true
        repeat: true
        onTriggered: {
            let replies = JSON.parse(backend.poll());
            for (let reply of replies) {
                let callback = callbacks[reply.id];
                delete callbacks[reply.id];
                if (callback)
                    callback(reply.result, reply.error || "");
            }
            apply(backend.state());
        }
    }
    function apply(raw) {
        if (!raw)
            return;
        let next = JSON.parse(raw);
        applying = true;
        let opened = next.readerOpenedSerial !== readerOpenedSerial;
        let navigated = next.navigationSerial !== navigationSerial;
        // Preserve model identity when a count, progress or unrelated pane changes.
        for (let key of ["query", "project", "machine", "provider", "timeframe", "origin", "projectSort", "metric",
                         "machines", "projects", "sessions", "errors", "activity", "tokenUsageEnabled",
                         "activityBusy", "activityPending", "activityComplete", "activityFailures",
                         "activityPartial", "total", "pending", "limit", "canLoadMore", "selected", "records", "offset",
                         "recordTotal", "readerBusy", "readerError", "metadataBusy", "metadataError", "findText", "matches", "matchIndex", "finding",
                         "findError", "scannedRecords", "anchor", "anchorOffset", "expansion"]) {
            if (next[key] !== undefined && JSON.stringify(adapter[key]) !== JSON.stringify(next[key]))
                adapter[key] = next[key];
        }
        readerOpenedSerial = next.readerOpenedSerial;
        navigationSerial = next.navigationSerial;
        if (opened)
            readerOpened();
        applying = false;
        if (navigated)
            Qt.callLater(function () {
                if (next.navigationKind === "restore")
                    restorePosition(next.targetRecord || next.anchor, next.anchorOffset || 0, next.expansion || {});
                else if (next.targetRecord)
                    positionRecord(next.targetRecord, next.targetOccurrence || 0);
                else if (next.anchor)
                    restorePosition(next.anchor, next.anchorOffset || 0, next.expansion || {});
            });
    }
    function action(value) {
        apply(backend.action(JSON.stringify(value)));
    }
    function configure(changes) {
        action({
                   op: "configure",
                   changes: changes
               });
    }
    function initialize() {
        action({
                   op: "initialize"
               });
    }
    function refresh() {
        action({
                   op: "refresh"
               });
    }
    function refreshActivity() {
        action({
                   op: "refreshActivity"
               });
    }
    function moreSessions() {
        action({
                   op: "moreSessions"
               });
    }
    function resetFilters() {
        action({
                   op: "resetFilters"
               });
    }
    function openSession(session) {
        action({
                   op: "openSession",
                   session: session
               });
    }
    function earlier() {
        action({
                   op: "earlier"
               });
    }
    function later() {
        action({
                   op: "later"
               });
    }
    function find(text) {
        action({
                   op: "find",
                   text: text
               });
    }
    function navigateFind(direction) {
        action({
                   op: "navigateFind",
                   direction: direction
               });
    }
    function saveAnchor(id, offset, expansion) {
        action({
                   op: "saveAnchor",
                   id: id,
                   offset: offset,
                   expansion: expansion
               });
    }
    function identity(session) {
        return [session.machine || "local", session.source, session.session_id, session.source_path].join("\u001f");
    }
    function request(payload, callback) {
        let id = String(++sequence);
        callbacks[id] = callback;
        backend.request(id, JSON.stringify(payload));
    }
}
