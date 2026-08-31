mod tools {
    pub mod anchor_store {
        use std::path::Path;

        use moh::tools::anchor_store::{AnchorSnapshot, AnchorStore};
        use moh::tools::moh_state_dir;

        mod tests {
            use super::*;

            // This catches an accidentally non-durable store implementation: a separately
            // opened connection must recover the snapshot written by the first connection.
            #[tokio::test]
            async fn saves_and_reopens_a_snapshot() {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("hash-store.sqlite");
                let store = AnchorStore::open_at(&path).await.unwrap();
                let snapshot = AnchorSnapshot {
                    checksum: "checksum".into(),
                    line_count: 2,
                    hashes: vec!["Ab1".into(), "Cd2".into()],
                    lines: vec!["first".into(), "second".into()],
                };
                store
                    .save(Path::new("/canonical/file.txt"), &snapshot)
                    .await
                    .unwrap();

                let reopened = AnchorStore::open_at(&path).await.unwrap();
                assert_eq!(
                    reopened
                        .load(Path::new("/canonical/file.txt"))
                        .await
                        .unwrap(),
                    Some(snapshot)
                );
            }

            #[tokio::test]
            async fn migrates_legacy_snapshots_without_line_identities() {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("hash-store.sqlite");
                let connection = rusqlite::Connection::open(&path).unwrap();
                connection
                    .execute_batch(
                        "CREATE TABLE snapshots (\
                            path TEXT PRIMARY KEY, \
                            checksum TEXT NOT NULL, \
                            line_count INTEGER NOT NULL, \
                            hashes TEXT NOT NULL\
                        ); \
                        INSERT INTO snapshots (path, checksum, line_count, hashes) \
                        VALUES ('/canonical/legacy.txt', 'legacy', 1, '[\"Ab1\"]');",
                    )
                    .unwrap();
                drop(connection);

                let store = AnchorStore::open_at(&path).await.unwrap();
                assert_eq!(
                    store
                        .load(Path::new("/canonical/legacy.txt"))
                        .await
                        .unwrap(),
                    Some(AnchorSnapshot {
                        checksum: "legacy".into(),
                        line_count: 1,
                        hashes: vec!["Ab1".into()],
                        lines: vec![],
                    })
                );
            }

            #[tokio::test]
            async fn quarantines_a_corrupt_store_and_reopens_the_rebuilt_database() {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("hash-store.sqlite");
                let corrupt_bytes = b"not a sqlite database";
                std::fs::write(&path, corrupt_bytes).unwrap();

                let snapshot = AnchorSnapshot {
                    checksum: "rebuilt".into(),
                    line_count: 1,
                    hashes: vec!["Ab1".into()],
                    lines: vec!["line".into()],
                };
                let canonical_path = Path::new("/canonical/rebuilt.txt");
                let store = AnchorStore::open_at(&path).await.unwrap();
                store.save(canonical_path, &snapshot).await.unwrap();
                drop(store);

                let quarantine = std::fs::read_dir(directory.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|candidate| {
                        candidate
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("hash-store.sqlite.corrupt-")
                    })
                    .expect("the corrupt database should be moved aside");
                assert_eq!(std::fs::read(quarantine).unwrap(), corrupt_bytes);

                let reopened = AnchorStore::open_at(&path).await.unwrap();
                assert_eq!(reopened.load(canonical_path).await.unwrap(), Some(snapshot));
            }

            #[test]
            fn moh_state_dir_uses_the_platform_state_or_local_data_directory() {
                let path = moh_state_dir().unwrap();
                assert!(path.ends_with("moh"));
            }
        }
    }
}
