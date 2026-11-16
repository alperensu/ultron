use ultron_core::Paths;
use ultron_core::client::{connect_or_start, locate_ultrond};

#[test]
fn locate_ultrond_prefers_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = tmp.path().join("ultrond.exe");
    std::fs::write(&daemon, []).unwrap();
    assert_eq!(locate_ultrond(&tmp.path().join("ultron.exe")).unwrap(), daemon);
}

#[tokio::test]
async fn connect_or_start_reports_missing_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("state"));
    let error = connect_or_start(&paths, &tmp.path().join("missing-ultrond.exe"))
        .await
        .err()
        .expect("olmayan servis başlatılamaz");
    assert!(format!("{error:#}").contains("ultrond"), "{error:#}");
}
