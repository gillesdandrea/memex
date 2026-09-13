use qtbridge::QmlElement;
use qtbridge::qtbridge_type_lib::{QString, QVariant, QVariantMap};

fn main() {
    let root = tempfile::tempdir().expect("fixture root");
    std::fs::write(root.path().join("example.rs"), "// Fixture\nfn main() {}\n")
        .expect("source fixture");
    // This harness runs on the process main thread before Qt or workers start.
    unsafe {
        std::env::set_var("MEMEX_ROOT", root.path());
        std::env::set_var("MEMEX_CLI", env!("CARGO_BIN_EXE_memex-qt-fixture"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::env::set_var("XDG_CACHE_HOME", root.path().join("cache"));
    }
    memex_qt::register_qml_resources();
    memex_qt::Backend::register();
    let args = vec![
        "memex-qt-tests".into(),
        "-input".into(),
        format!("{}/tests_ui", env!("CARGO_MANIFEST_DIR")),
    ];
    let mut properties = QVariantMap::default();
    let captures = std::env::var("MEMEX_QT_CAPTURE_DIR").unwrap_or_default();
    if !captures.is_empty() {
        std::fs::create_dir_all(&captures).expect("screenshot directory");
    }
    properties.insert(
        "captureDirectory".into(),
        QVariant::from(&QString::from(captures)),
    );
    let code = quicktest::quick_test_main_with_properties(&args, &"Memex UI".into(), &properties);
    assert_eq!(code, 0, "Qt application tests failed");
}
