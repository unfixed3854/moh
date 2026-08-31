use std::fs;

use garde::Validate;
use moh::tools::{
    EditArgs, EditService, EditServiceFactory, ReadArgs, ReadConfig, ReadServiceFactory,
};
use serde_json::json;

fn hash_at(read_output: &str, line: usize) -> String {
    read_output
        .lines()
        .nth(line)
        .unwrap()
        .split_once('│')
        .unwrap()
        .0
        .to_owned()
}

#[tokio::test]
async fn edit_replaces_an_inclusive_anchored_range_and_returns_fresh_anchors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let reader = reads.for_cwd(directory.path().to_owned());
    let before = reader.read(ReadArgs::path("note.txt")).await.unwrap();
    let before = before.as_text().unwrap();
    let beta = hash_at(before, 1);
    let gamma = hash_at(before, 2);
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    let output = editor
        .edit(EditArgs {
            path: "note.txt".into(),
            remove_from: beta,
            remove_to: gamma,
            replacement_lines: vec!["delta".into(), "epsilon".into()],
        })
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "alpha\ndelta\nepsilon\n");
    let output = output.as_text().unwrap();
    assert!(output.contains("│alpha"));
    assert!(output.contains("│delta"));
    assert!(output.contains("│epsilon"));
}

#[tokio::test]
async fn edit_rejects_a_file_changed_after_it_was_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "alpha\nbeta\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let before = reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    let alpha = hash_at(before.as_text().unwrap(), 0);
    fs::write(&path, "external\nbeta\n").unwrap();
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    let error = editor
        .edit(EditArgs {
            path: "note.txt".into(),
            remove_from: alpha.clone(),
            remove_to: alpha,
            replacement_lines: vec!["agent".into()],
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert_eq!(fs::read_to_string(path).unwrap(), "external\nbeta\n");
}

#[tokio::test]
async fn edit_does_not_recreate_a_file_deleted_after_it_was_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "alpha\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let before = reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    let alpha = hash_at(before.as_text().unwrap(), 0);
    fs::remove_file(&path).unwrap();
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    let error = editor
        .edit(EditArgs {
            path: "note.txt".into(),
            remove_from: alpha.clone(),
            remove_to: alpha,
            replacement_lines: vec!["replacement".into()],
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_STALE_READ]"));
    assert!(!path.exists());
}

#[tokio::test]
async fn edit_arguments_are_strict() {
    assert!(
        serde_json::from_value::<EditArgs>(json!({
            "path": "note.txt",
            "remove_from": "Ab1",
            "remove_to": "Cd2",
            "replacement_lines": ["replacement"],
            "unexpected": true
        }))
        .is_err()
    );
    assert!(EditService::description().contains("inclusive"));
    assert!(
        EditArgs {
            path: "note.txt".into(),
            remove_from: "ab!".into(),
            remove_to: "abc".into(),
            replacement_lines: vec![],
        }
        .validate()
        .is_err()
    );
    assert!(
        EditArgs {
            path: "note.txt".into(),
            remove_from: "abc".into(),
            remove_to: "def".into(),
            replacement_lines: vec!["line\nnext".into()],
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn edit_rejects_malformed_anchors_and_embedded_line_breaks_before_file_access() {
    let directory = tempfile::tempdir().unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    for args in [
        EditArgs {
            path: "missing.txt".into(),
            remove_from: "too-long".into(),
            remove_to: "Ab1".into(),
            replacement_lines: vec!["replacement".into()],
        },
        EditArgs {
            path: "missing.txt".into(),
            remove_from: "Ab1".into(),
            remove_to: "Cd2".into(),
            replacement_lines: vec!["two\nlines".into()],
        },
    ] {
        let error = editor.edit(args).await.unwrap_err();
        assert!(error.to_string().starts_with("[E_INVALID_ARGUMENT]"));
    }
}

#[tokio::test]
async fn edit_rejects_a_reversed_anchor_range_without_changing_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "alpha\nbeta\n").unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let before = reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    let before = before.as_text().unwrap();
    let alpha = hash_at(before, 0);
    let beta = hash_at(before, 1);
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    let error = editor
        .edit(EditArgs {
            path: "note.txt".into(),
            remove_from: beta,
            remove_to: alpha,
            replacement_lines: vec!["replacement".into()],
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_INVALID_ARGUMENT]"));
    assert_eq!(fs::read_to_string(path).unwrap(), "alpha\nbeta\n");
}

#[cfg(unix)]
#[tokio::test]
async fn edit_preserves_existing_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("note.txt");
    fs::write(&path, "alpha\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let reads = ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite")));
    let before = reads
        .for_cwd(directory.path().to_owned())
        .read(ReadArgs::path("note.txt"))
        .await
        .unwrap();
    let alpha = hash_at(before.as_text().unwrap(), 0);
    let editor = EditServiceFactory::sharing_reads(&reads).for_cwd(directory.path().to_owned());

    editor
        .edit(EditArgs {
            path: "note.txt".into(),
            remove_from: alpha.clone(),
            remove_to: alpha,
            replacement_lines: vec!["beta".into()],
        })
        .await
        .unwrap();

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}
