/// Give each test an owned data root. A nested sessions directory keeps
/// recorder audit files and the capture admission lock inside this fixture.
pub fn scratch(name: &str) -> tempfile::TempDir {
    let root = tempfile::Builder::new().prefix(name).tempdir().unwrap();
    std::fs::create_dir(root.path().join("sessions")).unwrap();
    root
}
