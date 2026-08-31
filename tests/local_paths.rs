#![cfg(unix)]

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
    path::PathBuf,
    sync::{Arc, Barrier, Mutex},
    thread,
};

use moh::local::{LocalPaths, PathRoots};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvironmentRestore {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentRestore {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let previous = env::var_os(key);
        // SAFETY: this test target serializes every mutation of this process environment variable
        // through `ENV_LOCK`, and the guard restores the prior value before releasing that lock.
        unsafe { env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        // SAFETY: see `EnvironmentRestore::set`; the corresponding `ENV_LOCK` guard still lives.
        unsafe {
            if let Some(value) = &self.previous {
                env::set_var(self.key, value);
            } else {
                env::remove_var(self.key);
            }
        }
    }
}

fn paths_in(root: &std::path::Path) -> LocalPaths {
    LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    })
}

#[test]
fn prepares_owner_only_runtime_paths() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let paths = paths_in(root);

    paths.prepare_runtime_dir().unwrap();

    assert_eq!(
        fs::metadata(paths.runtime_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        paths.socket_path(),
        paths.runtime_dir().join("backend.sock")
    );
    assert_eq!(
        paths.spawn_lock_path(),
        paths.runtime_dir().join("backend.lock")
    );
    assert_eq!(paths.server_log_path(), root.join("state/server.log"));
}

#[test]
fn prepares_an_absent_owner_only_state_directory() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let paths = paths_in(temporary_directory.path());

    paths.prepare_state_dir().unwrap();

    assert!(paths.state_dir().is_dir());
    assert_eq!(
        fs::metadata(paths.state_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn securely_creates_every_missing_state_ancestor_owner_only() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let state_dir = root.join("missing-root/level/moh");
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: state_dir.clone(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    paths.prepare_state_dir().unwrap();

    for path in [
        root.join("missing-root"),
        root.join("missing-root/level"),
        state_dir,
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} must be owner-only",
            path.display()
        );
    }
}

#[test]
fn relative_state_paths_walk_from_an_opened_current_directory() {
    let current_directory = env::current_dir().unwrap();
    let temporary_directory = tempfile::Builder::new()
        .prefix(".moh-relative-state-")
        .tempdir_in(&current_directory)
        .unwrap();
    let relative_root = PathBuf::from(temporary_directory.path().file_name().unwrap());
    let state_dir = relative_root.join("./missing/moh");
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(relative_root.join("runtime")),
        temp_dir: relative_root.join("tmp"),
        config_dir: relative_root.join("config"),
        state_dir: state_dir.clone(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    paths.prepare_state_dir().unwrap();

    assert_eq!(
        fs::metadata(current_directory.join(state_dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn rejects_a_current_directory_only_state_path() {
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(PathBuf::from("runtime")),
        temp_dir: PathBuf::from("tmp"),
        config_dir: PathBuf::from("config"),
        state_dir: PathBuf::from("."),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    let error = paths.prepare_state_dir().unwrap_err();

    assert!(matches!(
        error,
        moh::local::LocalPathError::UnsafeStateDirectory { .. }
    ));
}

#[test]
fn rejects_parent_components_before_creating_any_prefix() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("uncreated/../escaped"),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    let error = paths.prepare_state_dir().unwrap_err();

    assert!(matches!(
        error,
        moh::local::LocalPathError::UnsafeStateDirectory { .. }
    ));
    assert!(!root.join("uncreated").exists());
    assert!(!root.join("escaped").exists());
}

#[test]
fn rejects_an_intermediate_state_symlink_without_touching_its_target() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let target = root.join("target");
    fs::create_dir(&target).unwrap();
    fs::create_dir(target.join("nested")).unwrap();
    fs::write(target.join("sentinel"), b"unchanged").unwrap();
    symlink(&target, root.join("state-link")).unwrap();
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("state-link/nested/moh"),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    let error = paths.prepare_state_dir().unwrap_err();

    assert!(matches!(
        &error,
        moh::local::LocalPathError::OpenStateDirectory { .. }
    ));
    assert!(
        error
            .to_string()
            .contains(&paths.state_dir().display().to_string())
    );
    assert_eq!(fs::read_link(root.join("state-link")).unwrap(), target);
    assert_eq!(
        fs::read(root.join("target/sentinel")).unwrap(),
        b"unchanged"
    );
    assert!(!root.join("target/nested/moh").exists());
}

#[test]
fn leaves_existing_permissive_state_ancestors_unchanged() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let existing = root.join("shared-state-root");
    fs::create_dir(&existing).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
    let state_dir = existing.join("missing/moh");
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: state_dir.clone(),
        effective_uid: nix::unistd::Uid::effective().as_raw(),
    });

    paths.prepare_state_dir().unwrap();

    assert_eq!(
        fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(existing.join("missing"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(state_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn platform_default_prepares_a_missing_xdg_state_home() {
    let _environment_lock = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temporary_directory = tempfile::tempdir().unwrap();
    let state_home = temporary_directory.path().join("missing-root");
    let _state_home = EnvironmentRestore::set("XDG_STATE_HOME", state_home.as_os_str());

    let paths = LocalPaths::platform_default().unwrap();
    assert_eq!(paths.state_dir(), state_home.join("moh"));
    paths.prepare_state_dir().unwrap();

    for path in [state_home, paths.state_dir().to_path_buf()] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} must be owner-only",
            path.display()
        );
    }
}

#[test]
fn restricts_an_existing_permissive_state_directory() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let paths = paths_in(temporary_directory.path());
    fs::create_dir(paths.state_dir()).unwrap();
    fs::set_permissions(paths.state_dir(), fs::Permissions::from_mode(0o755)).unwrap();

    paths.prepare_state_dir().unwrap();

    assert_eq!(
        fs::metadata(paths.state_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn rejects_a_symlink_state_directory_without_restricting_its_target() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let target = root.join("state-target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&target, root.join("state")).unwrap();

    let error = paths_in(root).prepare_state_dir().unwrap_err();

    assert!(matches!(
        error,
        moh::local::LocalPathError::OpenStateDirectory { .. }
    ));
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn rejects_a_non_directory_state_path() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let paths = paths_in(temporary_directory.path());
    fs::write(paths.state_dir(), b"not a directory").unwrap();

    let error = paths.prepare_state_dir().unwrap_err();

    assert!(matches!(
        error,
        moh::local::LocalPathError::OpenStateDirectory { .. }
            | moh::local::LocalPathError::StateDirectoryType { .. }
    ));
    assert_eq!(fs::read(paths.state_dir()).unwrap(), b"not a directory");
}

#[test]
fn rejects_a_state_directory_with_a_different_expected_owner() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let current_uid = nix::unistd::Uid::effective().as_raw();
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        effective_uid: current_uid.saturating_add(1),
    });
    fs::create_dir(paths.state_dir()).unwrap();

    let error = paths.prepare_state_dir().unwrap_err();

    assert!(matches!(
        error,
        moh::local::LocalPathError::StateDirectoryOwner { .. }
    ));
}

#[test]
fn concurrent_runtime_preparation_accepts_the_racing_creator() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let paths = Arc::new(paths_in(temporary_directory.path()));
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();

    for _ in 0..8 {
        let paths = Arc::clone(&paths);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            paths.prepare_runtime_dir()
        }));
    }

    for worker in workers {
        worker.join().unwrap().unwrap();
    }
}

#[test]
fn rejects_a_symlink_runtime_directory() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let target = root.join("target");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.join("runtime")).unwrap();

    let error = paths_in(root).prepare_runtime_dir().unwrap_err();

    assert!(error.to_string().contains("runtime"));
    assert!(error.to_string().contains("directory"));
}

#[test]
fn rejects_a_non_socket_endpoint_without_removing_it() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let paths = paths_in(temporary_directory.path());
    paths.prepare_runtime_dir().unwrap();
    fs::write(paths.socket_path(), "not a socket").unwrap();

    let error = paths.validate_socket_candidate().unwrap_err();

    assert!(error.to_string().contains("socket"));
    assert!(paths.socket_path().is_file());
}

#[test]
fn rejects_a_socket_owned_by_a_different_effective_uid() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let current_uid = nix::unistd::Uid::effective().as_raw();
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        effective_uid: current_uid.saturating_add(1),
    });
    fs::create_dir(paths.runtime_dir()).unwrap();
    UnixListener::bind(paths.socket_path()).unwrap();

    let error = paths.validate_socket_candidate().unwrap_err();

    assert!(error.to_string().contains("owner"));
    assert!(paths.socket_path().exists());
}
