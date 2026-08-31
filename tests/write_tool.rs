use std::fs;

use garde::Validate;
use moh::tools::{
    read::{ReadArgs, ReadConfig, ReadServiceFactory},
    write::{WriteArgs, WriteService, WriteServiceFactory, WriteToolError},
};
use serde_json::json;

fn unread_writer(directory: &std::path::Path) -> WriteService {
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.join("anchors.sqlite")));
    WriteServiceFactory::sharing_reads(&reads).for_cwd(directory.to_owned())
}

#[tokio::test]
async fn isolated_reader_factory_does_not_share_write_authority() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "original\n").unwrap();
    let root = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let first = root.isolated_session();
    let second = root.isolated_session();

    first
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();

    let error = WriteServiceFactory::sharing_reads(&second)
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, WriteToolError::NotRead));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn write_creates_a_new_file() {
    let directory = tempfile::tempdir().unwrap();
    let service = unread_writer(directory.path());

    let output = service
        .write(WriteArgs {
            path: "nested/note.txt".into(),
            content: "new contents\n".into(),
        })
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(directory.path().join("nested/note.txt")).unwrap(),
        "new contents\n"
    );
    assert_eq!(
        output.as_text(),
        Some("Successfully wrote 13 bytes to nested/note.txt")
    );
}

#[tokio::test]
async fn write_rejects_an_existing_file_that_was_not_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "original\n").unwrap();
    let service = unread_writer(directory.path());

    let error = service
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_NOT_READ]"));
    assert_eq!(fs::read_to_string(path).unwrap(), "original\n");
}

#[tokio::test]
async fn a_partial_read_authorizes_a_later_writer_service_instance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "first\nsecond\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);

    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs {
            path: "note.txt".into(),
            offset: Some(1),
            limit: Some(1),
        })
        .await
        .unwrap();
    writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "replacement\n");
}

#[cfg(unix)]
#[tokio::test]
async fn write_does_not_replace_a_symlink_removed_after_it_was_read() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.txt");
    let link = directory.path().join("link.txt");
    fs::write(&target, "original\n").unwrap();
    symlink(&target, &link).unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("link.txt"))
        .await
        .unwrap();
    fs::remove_file(&link).unwrap();

    let error = writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "link.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert!(!link.exists());
    assert_eq!(fs::read_to_string(target).unwrap(), "original\n");
}

#[cfg(unix)]
#[tokio::test]
async fn write_rejects_a_symlink_whose_target_was_deleted_after_reading() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.txt");
    let link = directory.path().join("link.txt");
    fs::write(&target, "original\n").unwrap();
    symlink(&target, &link).unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("link.txt"))
        .await
        .unwrap();
    fs::remove_file(&target).unwrap();

    let error = writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "link.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert!(!target.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn overwrite_preserves_existing_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "original\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap();

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[tokio::test]
async fn a_successful_write_refreshes_the_observed_checksum() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "original\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    let writer = writes.for_cwd(directory.path().to_owned());
    writer
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "first replacement\n".into(),
        })
        .await
        .unwrap();

    writer
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "second replacement\n".into(),
        })
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "second replacement\n");
}

#[tokio::test]
async fn durable_anchor_state_does_not_authorize_a_new_conversation_to_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    let store = directory.path().join("anchors.sqlite");
    fs::write(&path, "original\n").unwrap();
    let first_reads = ReadServiceFactory::new(ReadConfig::at(&store));
    first_reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    drop(first_reads);
    let restarted_reads = ReadServiceFactory::new(ReadConfig::at(store));
    let restarted_writes = WriteServiceFactory::sharing_reads(&restarted_reads);

    let error = restarted_writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_NOT_READ]"));
    assert_eq!(fs::read_to_string(path).unwrap(), "original\n");
}

#[tokio::test]
async fn write_rejects_a_file_changed_after_it_was_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "original\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    fs::write(&path, "external change\n").unwrap();

    let error = writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "agent replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert_eq!(fs::read_to_string(path).unwrap(), "external change\n");
}

#[tokio::test]
async fn write_arguments_are_strict() {
    assert!(
        WriteArgs {
            path: String::new(),
            content: "contents".into(),
        }
        .validate()
        .is_err()
    );
    assert!(
        serde_json::from_value::<WriteArgs>(json!({
            "path": "note.txt",
            "content": "contents",
            "unexpected": true
        }))
        .is_err()
    );

    let directory = tempfile::tempdir().unwrap();
    let error = unread_writer(directory.path())
        .write(WriteArgs {
            path: String::new(),
            content: "contents".into(),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("[E_INVALID_ARGUMENT]"));
}

#[tokio::test]
async fn write_does_not_recreate_a_file_deleted_after_it_was_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "original\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    fs::remove_file(&path).unwrap();

    let error = writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert!(!path.exists());
}

#[tokio::test]
async fn write_does_not_recreate_deleted_parent_directories_of_an_observed_file() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("nested");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("note.txt");
    fs::write(&path, "original\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let writes = WriteServiceFactory::sharing_reads(&reads);
    reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("nested/note.txt"))
        .await
        .unwrap();
    fs::remove_file(&path).unwrap();
    fs::remove_dir(&parent).unwrap();

    let error = writes
        .for_cwd(directory.path().to_owned())
        .write(WriteArgs {
            path: "nested/note.txt".into(),
            content: "replacement\n".into(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert!(!parent.exists());
}
