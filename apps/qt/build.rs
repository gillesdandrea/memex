fn main() {
    // CXX-Qt's initialization archives require a linker that rescans archives.
    // Use its own platform selector (LLD on Linux, Apple linker flags on macOS).
    qt_build_utils::QtPlatformLinker::init();
    println!("cargo::rerun-if-changed=build.rs");
}
