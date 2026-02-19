use super::{normalize_database_url, prepare_database_url};

use std::{
    env, fs,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn normalizes_plain_file_path_to_sqlite_url() {
    assert_eq!(
        normalize_database_url("./data/test.db"),
        "sqlite://./data/test.db"
    );
}

#[test]
fn creates_parent_dir_for_relative_sqlite_url() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();

    let temp_root = env::temp_dir().join(format!("proto_rtc_server_test_{suffix}"));
    let db_path = temp_root.join("data").join("test.db");

    prepare_database_url(db_path.to_string_lossy().as_ref()).expect("prepare db url");
    assert!(temp_root.join("data").exists());

    fs::remove_dir_all(temp_root).expect("cleanup");
}

#[test]
fn keeps_windows_absolute_path_with_single_sqlite_colon() {
    assert_eq!(
        normalize_database_url("sqlite:C:\\Users\\alice\\test.db"),
        "sqlite:C:/Users/alice/test.db"
    );
}

#[test]
fn normalizes_windows_plain_path_with_single_sqlite_colon() {
    assert_eq!(
        normalize_database_url("C:\\Users\\alice\\test.db"),
        "sqlite:C:/Users/alice/test.db"
    );
}

#[test]
fn converts_sqlite_double_slash_windows_path() {
    assert_eq!(
        normalize_database_url("sqlite://C:/Users/alice/test.db"),
        "sqlite:C:/Users/alice/test.db"
    );
}

#[tokio::test]
async fn prepared_database_url_creates_openable_sqlite_file() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();

    let temp_root = env::temp_dir().join(format!("proto_rtc_server_open_test_{suffix}"));
    let db_path = temp_root.join("nested").join("server.db");

    let prepared = prepare_database_url(db_path.to_string_lossy().as_ref()).expect("prepare");
    let storage = storage::Storage::new(&prepared).await.expect("open sqlite");
    drop(storage);

    assert!(
        db_path.exists(),
        "database file should be created: {}",
        db_path.display()
    );

    fs::remove_dir_all(temp_root).expect("cleanup");
}
