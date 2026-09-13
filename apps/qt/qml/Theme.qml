pragma Singleton
import QtQuick

QtObject {
    property SystemPalette systemPalette: SystemPalette {}
    readonly property bool dark: systemPalette.window.hslLightness < 0.5
    readonly property color background: dark ? "#202022" : "#ffffff"
    readonly property color sidebar: dark ? "#28282a" : "#f5f5f7"
    readonly property color text: dark ? "#eeeeef" : "#29292b"
    readonly property color secondary: dark ? "#a1a1a5" : "#858589"
    readonly property color tertiary: dark ? "#77777c" : "#a1a1a5"
    readonly property color divider: dark ? "#36363a" : "#ebebee"
    readonly property color hover: dark ? "#353538" : "#ededf0"
    readonly property color selection: dark ? "#424246" : "#e1e1e4"
    readonly property color accent: "#008bff"
    readonly property color codeBackground: dark ? "#29292c" : "#f6f6f7"
    readonly property string fontFamily: Qt.platform.os === "osx" ? ".AppleSystemUIFont" : "Adwaita Sans"
}
