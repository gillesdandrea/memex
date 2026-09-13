import QtQuick

Canvas {
    id: icon
    property string name: ""
    property color color: Theme.secondary
    property real size: 16
    implicitWidth: size
    implicitHeight: size
    onNameChanged: requestPaint()
    onColorChanged: requestPaint()
    onWidthChanged: requestPaint()
    onHeightChanged: requestPaint()
    onPaint: {
        let ctx = getContext("2d");
        ctx.reset();
        ctx.scale(width / 20, height / 20);
        ctx.strokeStyle = color;
        ctx.fillStyle = color;
        ctx.lineWidth = 1.45;
        ctx.lineCap = "round";
        ctx.lineJoin = "round";
        function line(points) {
            ctx.beginPath();
            ctx.moveTo(points[0], points[1]);
            for (let i = 2; i < points.length; i += 2) ctx.lineTo(points[i], points[i + 1]);
            ctx.stroke();
        }
        function box(x, y, w, h) { ctx.strokeRect(x, y, w, h); }
        function circle(x, y, r) { ctx.beginPath(); ctx.arc(x, y, r, 0, Math.PI * 2); ctx.stroke(); }
        switch (name) {
        case "home":
            line([2, 9, 10, 3, 18, 9]);
            line([4, 8, 4, 17, 8, 17, 8, 12, 12, 12, 12, 17, 16, 17, 16, 8]);
            break;
        case "folder":
            line([2, 6, 2, 16, 18, 16, 18, 6, 9, 6, 7, 4, 2, 4, 2, 6, 18, 6]);
            break;
        case "chat":
            line([3, 3, 14, 3, 14, 12, 8, 12, 4, 16, 4, 12, 2, 12, 2, 3, 3, 3]);
            line([16, 7, 18, 7, 18, 16, 15, 16, 13, 18, 11, 16, 8, 16]);
            break;
        case "search":
            circle(8, 8, 5.5); line([12, 12, 17, 17]); break;
        case "filter":
            line([3, 5, 17, 5]); line([6, 10, 14, 10]); line([8, 15, 12, 15]); break;
        case "sidebar":
            box(2, 3, 16, 14); line([7, 3, 7, 17]); break;
        case "refresh":
            ctx.beginPath(); ctx.arc(10, 10, 6, -.6, Math.PI * 1.6); ctx.stroke();
            line([10, 1, 13, 4, 9, 5]); break;
        case "copy":
            box(6, 6, 10, 11); line([3, 13, 3, 3, 12, 3]); break;
        case "file":
            line([5, 2, 11, 2, 16, 7, 16, 18, 4, 18, 4, 2, 5, 2]); line([11, 2, 11, 7, 16, 7]); break;
        case "link":
            ctx.save(); ctx.translate(10, 10); ctx.rotate(-.7);
            ctx.strokeRect(-3, -8, 6, 7); ctx.strokeRect(-3, 1, 6, 7); line([0, -3, 0, 3]); ctx.restore(); break;
        case "terminal":
            box(2, 3, 16, 14); line([5, 7, 8, 10, 5, 13]); line([10, 13, 14, 13]); break;
        case "chevron": line([7, 5, 12, 10, 7, 15]); break;
        case "down": line([5, 7, 10, 12, 15, 7]); break;
        case "close": line([5, 5, 15, 15]); line([15, 5, 5, 15]); break;
        case "more":
            [4, 10, 16].forEach(x => { ctx.beginPath(); ctx.arc(x, 10, 1, 0, Math.PI * 2); ctx.fill(); }); break;
        }
    }
}
