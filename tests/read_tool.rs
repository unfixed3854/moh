use std::{
    fs,
    path::{Path, PathBuf},
};

use garde::Validate;
use moh::tools::read::{ReadArgs, ReadConfig, ReadService, ReadServiceFactory};
use schemars::schema_for;
use serde_json::json;

fn write_fixture(directory: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    path
}

fn tool(directory: &Path) -> ReadService {
    ReadServiceFactory::new(ReadConfig::at(directory.join("hash-store.sqlite")))
        .for_cwd(directory.to_path_buf())
}

fn anchors(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once('│').map(|(hash, _)| hash.to_owned()))
        .collect()
}

const ONE_PIXEL_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0xf0, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

const ONE_PIXEL_WEBP: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, 0x24, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20,
    0x18, 0x00, 0x00, 0x00, 0x30, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00,
    0x34, 0x25, 0xa4, 0x00, 0x03, 0x70, 0x00, 0xfe, 0xfb, 0x94, 0x00, 0x00,
];

const ONE_PIXEL_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x02, 0x02, 0x02, 0x03,
    0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x06, 0x04, 0x04, 0x04, 0x04, 0x04, 0x08, 0x06,
    0x06, 0x05, 0x06, 0x09, 0x08, 0x0a, 0x0a, 0x09, 0x08, 0x09, 0x09, 0x0a, 0x0c, 0x0f, 0x0c, 0x0a,
    0x0b, 0x0e, 0x0b, 0x09, 0x09, 0x0d, 0x11, 0x0d, 0x0e, 0x0f, 0x10, 0x10, 0x11, 0x10, 0x0a, 0x0c,
    0x12, 0x13, 0x12, 0x10, 0x13, 0x0f, 0x10, 0x10, 0x10, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0xff, 0xc4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x54, 0xdf, 0xff, 0xd9,
];

const ONE_PIXEL_BMP: &[u8] = &[
    0x42, 0x4d, 0x3a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00, 0x28, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x18, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x13, 0x0b, 0x00, 0x00, 0x13, 0x0b, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00,
];

#[tokio::test]
async fn read_pages_hashline_output() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(directory.path(), "notes.txt", "one\ntwo\nthree\n");
    let output = tool(directory.path())
        .read(ReadArgs {
            path: fixture.display().to_string(),
            offset: Some(2),
            limit: Some(1),
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    let hash = text.split_once('│').unwrap().0;
    assert_eq!(hash.len(), 3);
    assert!(
        hash.chars()
            .all(|character| character.is_ascii_alphanumeric())
    );
    assert!(text.contains("│two"));
    assert!(text.ends_with("[Showing lines 2-2 of 3. Use offset=3 to continue.]"));
}

#[test]
fn read_args_reject_fractional_and_unknown_json_fields_and_validate_positive_values() {
    for arguments in [
        json!({"path": "file.txt", "offset": -1}),
        json!({"path": "file.txt", "offset": 1.5}),
        json!({"path": "file.txt", "limit": -1}),
        json!({"path": "file.txt", "limit": 1.5}),
        json!({"path": "file.txt", "unexpected": true}),
    ] {
        assert!(serde_json::from_value::<ReadArgs>(arguments).is_err());
    }
    for args in [
        ReadArgs {
            path: "file.txt".into(),
            offset: Some(0),
            limit: None,
        },
        ReadArgs {
            path: "file.txt".into(),
            offset: None,
            limit: Some(0),
        },
    ] {
        assert!(args.validate().is_err());
    }
    assert!(serde_json::from_value::<ReadArgs>(json!({"file_path": "old.txt"})).is_err());
    assert!(
        ReadArgs {
            path: String::new(),
            offset: None,
            limit: None,
        }
        .validate()
        .is_err()
    );
}

#[test]
fn read_schema_requires_a_path_and_positive_integers() {
    let schema = serde_json::to_value(schema_for!(ReadArgs)).unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(["path"]));
    assert_eq!(schema["properties"]["offset"]["minimum"], 1);
    assert_eq!(schema["properties"]["limit"]["minimum"], 1);
    assert!(schema["properties"].get("file_path").is_none());
}

#[tokio::test]
async fn read_lists_directory_entries_in_name_order() {
    let directory = tempfile::tempdir().unwrap();
    let listed = directory.path().join("project");
    fs::create_dir(&listed).unwrap();
    fs::create_dir(listed.join("nested")).unwrap();
    write_fixture(&listed, "zeta.txt", "zeta");
    write_fixture(&listed, "alpha.txt", "alpha");

    let output = tool(directory.path())
        .read(ReadArgs {
            path: listed.display().to_string(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();

    assert_eq!(output.as_text().unwrap(), "alpha.txt\nnested/\nzeta.txt");
}

#[tokio::test]
async fn read_pages_directory_entries() {
    let directory = tempfile::tempdir().unwrap();
    let listed = directory.path().join("project");
    fs::create_dir(&listed).unwrap();
    fs::create_dir(listed.join("nested")).unwrap();
    write_fixture(&listed, "zeta.txt", "zeta");
    write_fixture(&listed, "alpha.txt", "alpha");

    let output = tool(directory.path())
        .read(ReadArgs {
            path: listed.display().to_string(),
            offset: Some(2),
            limit: Some(1),
        })
        .await
        .unwrap();

    assert_eq!(
        output.as_text().unwrap(),
        "nested/\n[Showing entries 2-2 of 3. Use offset=3 to continue.]"
    );
}

#[tokio::test]
async fn relative_paths_are_resolved_from_the_run_context_cwd() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "context local\n").unwrap();
    let store = workspace.path().join("anchors.sqlite");
    let service =
        ReadServiceFactory::new(ReadConfig::at(store)).for_cwd(workspace.path().to_path_buf());

    let output = service.read(ReadArgs::path("note.txt")).await.unwrap();
    let text = output.as_text().unwrap();

    assert!(text.contains("context local"));
}

#[tokio::test]
async fn read_classifies_text_and_display_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let read = |path: &Path| {
        let path = path.to_path_buf();
        let store = store.clone();
        async move {
            tool(&store)
                .read(ReadArgs {
                    path: path.display().to_string(),
                    offset: None,
                    limit: None,
                })
                .await
        }
    };

    assert!(
        read(&directory.path().join("missing.txt"))
            .await
            .unwrap_err()
            .to_string()
            .starts_with("[E_NOT_FOUND]")
    );
    assert!(
        read(&write_fixture(directory.path(), "nul.bin", b"hello\0world"))
            .await
            .unwrap_err()
            .to_string()
            .starts_with("[E_NOT_TEXT]")
    );
    assert!(
        read(&write_fixture(
            directory.path(),
            "utf16.txt",
            [0xff, 0xfe, b'a', 0]
        ))
        .await
        .unwrap_err()
        .to_string()
        .starts_with("[E_NOT_TEXT]")
    );
    assert!(
        read(&write_fixture(
            directory.path(),
            "image.png",
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
        ))
        .await
        .unwrap_err()
        .to_string()
        .starts_with("[E_NOT_TEXT]")
    );

    let empty = write_fixture(directory.path(), "empty.txt", []);
    assert!(
        read(&empty)
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("[File is empty. Use replace to insert content.]")
    );

    let utf8_bom = write_fixture(
        directory.path(),
        "bom.txt",
        b"\xef\xbb\xbfvisible\r\nnext\rfinal",
    );
    let bom_text = read(&utf8_bom).await.unwrap().as_text().unwrap().to_owned();
    assert!(bom_text.contains("│visible\n"));
    assert!(bom_text.contains("│next\n"));
    assert!(bom_text.contains("│final"));
    assert!(!bom_text.contains('\r'));

    let malformed = write_fixture(directory.path(), "malformed.txt", [b'a', 0xff, b'b']);
    assert!(
        read(&malformed)
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("[Non-UTF-8 bytes shown as U+FFFD; editing rewrites the file as UTF-8.]")
    );

    let bm_text = write_fixture(directory.path(), "not-image.txt", b"BM plain UTF-8 text");
    assert!(
        read(&bm_text)
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .contains("│BM plain UTF-8 text")
    );
}

#[tokio::test]
async fn read_accepts_signature_looking_utf8_text() {
    let directory = tempfile::tempdir().unwrap();

    for (name, contents, expected) in [
        (
            "signature.gif.txt",
            b"GIF89aASCII-ONLY-SIGNATURE".as_slice(),
            "GIF89aASCII-ONLY-SIGNATURE",
        ),
        (
            "signature.webp.txt",
            b"RIFFsizeWEBPASCII-ONLY-SIGNATURE".as_slice(),
            "RIFFsizeWEBPASCII-ONLY-SIGNATURE",
        ),
    ] {
        let fixture = write_fixture(directory.path(), name, contents);
        let output = tool(directory.path())
            .read(ReadArgs {
                path: fixture.display().to_string(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(
            output.as_text().unwrap().split_once('│').unwrap().1,
            expected
        );
    }
}

#[tokio::test]
async fn read_rejects_actual_supported_images() {
    let directory = tempfile::tempdir().unwrap();

    for (name, contents) in [
        ("pixel.gif", ONE_PIXEL_GIF),
        ("pixel.webp", ONE_PIXEL_WEBP),
        ("pixel.jpg", ONE_PIXEL_JPEG),
        ("pixel.bmp", ONE_PIXEL_BMP),
    ] {
        let fixture = write_fixture(directory.path(), name, contents);
        let error = tool(directory.path())
            .read(ReadArgs {
                path: fixture.display().to_string(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("[E_NOT_TEXT]"));
    }
}

#[tokio::test]
async fn read_rejects_utf32_text() {
    let directory = tempfile::tempdir().unwrap();

    for (name, contents) in [
        ("utf32-le.txt", [0xff, 0xfe, 0x00, 0x00, b'a', 0, 0, 0]),
        ("utf32-be.txt", [0x00, 0x00, 0xfe, 0xff, 0, 0, 0, b'a']),
    ] {
        let fixture = write_fixture(directory.path(), name, contents);
        let error = tool(directory.path())
            .read(ReadArgs {
                path: fixture.display().to_string(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("[E_NOT_TEXT]"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn read_rejects_a_non_regular_unix_socket() {
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("not-a-file.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();

    let error = tool(directory.path())
        .read(ReadArgs {
            path: socket_path.display().to_string(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("[E_NOT_TEXT]"));
}

#[cfg(unix)]
#[tokio::test]
async fn read_reports_access_denied_when_the_os_enforces_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(directory.path(), "private.txt", "private");
    let original_permissions = fs::metadata(&fixture).unwrap().permissions();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o000)).unwrap();

    let probe = fs::File::open(&fixture);
    if probe.is_ok() {
        fs::set_permissions(&fixture, original_permissions).unwrap();
        eprintln!("skipping access-denied assertion: this process bypasses Unix mode bits");
        return;
    }
    assert_eq!(
        probe.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );

    let result = tool(directory.path())
        .read(ReadArgs {
            path: fixture.display().to_string(),
            offset: None,
            limit: None,
        })
        .await;
    fs::set_permissions(&fixture, original_permissions).unwrap();

    assert!(result.unwrap_err().to_string().starts_with("[E_ACCESS]"));
}

#[tokio::test]
async fn read_enforces_size_line_and_rendered_line_limits() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let read = |path: &Path| {
        let path = path.to_path_buf();
        let store = store.clone();
        async move {
            tool(&store)
                .read(ReadArgs {
                    path: path.display().to_string(),
                    offset: None,
                    limit: None,
                })
                .await
        }
    };

    let too_many_lines = directory.path().join("too-many-lines.txt");
    let mut lines = String::with_capacity(238_329 * 2);
    for _ in 0..238_329 {
        lines.push_str("x\n");
    }
    fs::write(&too_many_lines, lines).unwrap();
    assert!(
        read(&too_many_lines)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("[E_FILE_TOO_LARGE]")
    );

    let too_large = directory.path().join("too-large.txt");
    let large = fs::File::create(&too_large).unwrap();
    large.set_len(100 * 1024 * 1024 + 1).unwrap();
    assert!(
        read(&too_large)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("[E_FILE_TOO_LARGE]")
    );

    let long = write_fixture(directory.path(), "long.txt", "x".repeat(204_801));
    let output = read(&long).await.unwrap().as_text().unwrap().to_owned();
    assert!(!output.contains(&format!("│{}", "x".repeat(204_801))));
    assert!(output.contains("sed -n '1p'"));
    assert!(output.contains("head -c 204800"));
}

#[tokio::test]
async fn read_returns_explanatory_text_beyond_end_of_file() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(directory.path(), "notes.txt", "one\n");
    let output = tool(directory.path())
        .read(ReadArgs {
            path: fixture.display().to_string(),
            offset: Some(2),
            limit: None,
        })
        .await
        .unwrap();
    assert!(output.as_text().unwrap().contains("beyond end"));
}

#[tokio::test]
async fn read_reuses_durable_anchors_across_external_edits() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let fixture = write_fixture(directory.path(), "notes.txt", "alpha\n}\n}\nbeta\ngamma\n");
    let args = |path: &Path| ReadArgs {
        path: path.display().to_string(),
        offset: None,
        limit: None,
    };

    let initial = tool(&store).read(args(&fixture)).await.unwrap();
    let initial_anchors = anchors(initial.as_text().unwrap());
    assert_eq!(initial_anchors.len(), 5);
    assert_ne!(initial_anchors[1], initial_anchors[2]);

    let reopened = tool(&store).read(args(&fixture)).await.unwrap();
    assert_eq!(initial_anchors, anchors(reopened.as_text().unwrap()));

    fs::write(&fixture, "leading\nalpha\n}\n}\nbeta\ngamma\n").unwrap();
    let inserted = anchors(
        tool(&store)
            .read(args(&fixture))
            .await
            .unwrap()
            .as_text()
            .unwrap(),
    );
    assert_eq!(inserted[1..], initial_anchors);

    fs::write(&fixture, "alpha\n}\n}\nbeta\ngamma\n").unwrap();
    let removed = anchors(
        tool(&store)
            .read(args(&fixture))
            .await
            .unwrap()
            .as_text()
            .unwrap(),
    );
    assert_eq!(removed[0], initial_anchors[0]);
    assert_eq!(removed[1], inserted[3]);
    assert_eq!(removed[2], inserted[2]);
    assert_eq!(removed[3..], initial_anchors[3..]);

    fs::write(&fixture, "alpha\n}\n}\nchanged\ngamma\n").unwrap();
    let changed = anchors(
        tool(&store)
            .read(args(&fixture))
            .await
            .unwrap()
            .as_text()
            .unwrap(),
    );
    assert_eq!(changed[0], removed[0]);
    assert_eq!(changed[1], removed[1]);
    assert_eq!(changed[2], removed[2]);
    assert_ne!(changed[3], removed[3]);
    assert_eq!(changed[4], removed[4]);
    assert!(changed.iter().all(|hash| {
        hash.len() == 3
            && hash
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
    }));
}

#[tokio::test]
async fn read_probes_hash_collisions_deterministically() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = write_fixture(
        directory.path(),
        "collisions.txt",
        "collision-101\ncollision-242\n",
    );
    let args = || ReadArgs {
        path: fixture.display().to_string(),
        offset: None,
        limit: None,
    };

    let first = anchors(
        tool(&directory.path().join("first"))
            .read(args())
            .await
            .unwrap()
            .as_text()
            .unwrap(),
    );
    let second = anchors(
        tool(&directory.path().join("second"))
            .read(args())
            .await
            .unwrap()
            .as_text()
            .unwrap(),
    );

    assert_eq!(first, vec!["PXF", "QYG"]);
    assert_eq!(second, first);
}

#[tokio::test]
async fn read_keeps_anchors_when_only_trailing_whitespace_changes() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let fixture = write_fixture(directory.path(), "whitespace.txt", "stable   \nother\n");
    let args = || ReadArgs {
        path: fixture.display().to_string(),
        offset: None,
        limit: None,
    };

    let before = anchors(tool(&store).read(args()).await.unwrap().as_text().unwrap());
    fs::write(&fixture, "stable\t \nother\n").unwrap();
    let after = anchors(tool(&store).read(args()).await.unwrap().as_text().unwrap());

    assert_eq!(after, before);
}

#[tokio::test]
async fn read_matches_sparse_repeated_lines_in_upstream_source_order() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let fixture = directory.path().join("widely-separated.txt");
    let filler = (0..100)
        .map(|index| format!("filler-{index}"))
        .collect::<Vec<_>>();
    let initial_lines = [
        vec!["prefix".to_owned(), "duplicate".to_owned()],
        filler.clone(),
        vec!["duplicate".to_owned(), "suffix".to_owned()],
    ]
    .concat();
    fs::write(&fixture, format!("{}\n", initial_lines.join("\n"))).unwrap();
    let args = || ReadArgs {
        path: fixture.display().to_string(),
        offset: None,
        limit: None,
    };

    let initial = anchors(tool(&store).read(args()).await.unwrap().as_text().unwrap());
    let first_duplicate = initial[1].clone();

    let deleted_lines = [
        vec!["prefix".to_owned()],
        filler.clone(),
        vec!["duplicate".to_owned(), "suffix".to_owned()],
    ]
    .concat();
    fs::write(&fixture, format!("{}\n", deleted_lines.join("\n"))).unwrap();
    let after_delete = anchors(tool(&store).read(args()).await.unwrap().as_text().unwrap());
    assert_eq!(after_delete[101], first_duplicate);

    fs::write(&fixture, format!("{}\n", initial_lines.join("\n"))).unwrap();
    let after_insert = anchors(tool(&store).read(args()).await.unwrap().as_text().unwrap());
    assert_eq!(after_insert[102], first_duplicate);
    assert_ne!(after_insert[1], first_duplicate);
}

#[tokio::test]
async fn read_matches_a_large_repeated_snapshot_without_quadratic_allocation() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("hash-store.sqlite");
    let fixture = directory.path().join("many-duplicates.txt");
    let repeated_lines = 20_000;
    fs::write(&fixture, "}\n".repeat(repeated_lines)).unwrap();
    let args = || ReadArgs {
        path: fixture.display().to_string(),
        offset: None,
        limit: Some(repeated_lines as u64),
    };

    tool(&store).read(args()).await.unwrap();
    fs::write(&fixture, "}\n".repeat(repeated_lines - 1)).unwrap();
    let output = tool(&store).read(args()).await.unwrap();
    assert_eq!(anchors(output.as_text().unwrap()).len(), repeated_lines - 1);
}
