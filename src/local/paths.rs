use std::{
    env, fs,
    io::ErrorKind,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Component, Path, PathBuf},
};

use directories::ProjectDirs;
use nix::{
    errno::Errno,
    fcntl::{OFlag, open, openat},
    sys::stat::{FchmodatFlags, Mode, SFlag, fchmod, fchmodat, fstat, mkdirat},
    unistd::{Uid, mkdir},
};
use thiserror::Error;

/// Inputs used to resolve Moh's per-user filesystem locations.
#[derive(Clone, Debug)]
pub struct PathRoots {
    /// Optional Moh-specific runtime directory rooted under `XDG_RUNTIME_DIR`.
    pub runtime_dir: Option<PathBuf>,
    /// System temporary directory used when no runtime directory is available.
    pub temp_dir: PathBuf,
    /// Moh's platform configuration directory.
    pub config_dir: PathBuf,
    /// Moh's platform state directory.
    pub state_dir: PathBuf,
    /// Effective Unix user ID that owns all trusted runtime paths.
    pub effective_uid: u32,
}

/// Errors raised while resolving or validating Moh's local runtime paths.
#[derive(Debug, Error)]
pub enum LocalPathError {
    /// The platform did not provide Moh-specific configuration directories.
    #[error("could not resolve Moh's platform directories")]
    PlatformDirectoriesUnavailable,
    /// A local path could not be inspected safely.
    #[error("could not inspect local path {path}: {source}")]
    Inspect {
        /// Path whose metadata could not be read.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The runtime path could not be created.
    #[error("could not create runtime directory {path}: {source}")]
    CreateRuntimeDirectory {
        /// Directory Moh attempted to create.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The runtime directory could not be opened without following symlinks.
    #[error("could not open secure runtime directory {path}: {source}")]
    OpenRuntimeDirectory {
        /// Runtime directory path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The trusted runtime path was not a directory.
    #[error("runtime directory {path} has an unexpected type; expected an owned directory")]
    RuntimeDirectoryType {
        /// Runtime path with the unexpected type.
        path: PathBuf,
    },
    /// The trusted runtime directory had a different owner.
    #[error("runtime directory {path} has unexpected owner uid {found}; expected uid {expected}")]
    RuntimeDirectoryOwner {
        /// Runtime directory path.
        path: PathBuf,
        /// Owner reported by the filesystem.
        found: u32,
        /// Expected effective user ID.
        expected: u32,
    },
    /// Permissions could not be restricted through the validated directory handle.
    #[error("could not restrict runtime directory {path} to owner-only access: {source}")]
    RestrictRuntimeDirectory {
        /// Runtime directory path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The state path could not be created as one exact directory entry.
    #[error("could not create state directory {path}: {source}")]
    CreateStateDirectory {
        /// Directory Moh attempted to create.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The state directory could not be opened without following symlinks.
    #[error("could not open secure state directory {path}: {source}")]
    OpenStateDirectory {
        /// State directory path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The trusted state path was not a directory.
    #[error("state directory {path} has an unexpected type; expected an owned directory")]
    StateDirectoryType {
        /// State path with the unexpected type.
        path: PathBuf,
    },
    /// The trusted state directory had a different owner.
    #[error("state directory {path} has unexpected owner uid {found}; expected uid {expected}")]
    StateDirectoryOwner {
        /// State directory path.
        path: PathBuf,
        /// Owner reported by the filesystem.
        found: u32,
        /// Expected effective user ID.
        expected: u32,
    },
    /// State-directory permissions could not be restricted through its validated handle.
    #[error("could not restrict state directory {path} to owner-only access: {source}")]
    RestrictStateDirectory {
        /// State directory path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The configured state path cannot be traversed without escaping its opened root.
    #[error("state directory {path} cannot be prepared securely: {reason}")]
    UnsafeStateDirectory {
        /// Rejected state directory path.
        path: PathBuf,
        /// Stable explanation of the unsafe path shape.
        reason: &'static str,
    },
    /// The socket candidate was not a Unix socket.
    #[error("socket candidate {path} has an unexpected type; expected an owned Unix socket")]
    SocketType {
        /// Socket candidate path.
        path: PathBuf,
    },
    /// The socket candidate had a different owner.
    #[error("socket candidate {path} has unexpected owner uid {found}; expected uid {expected}")]
    SocketOwner {
        /// Socket candidate path.
        path: PathBuf,
        /// Owner reported by the filesystem.
        found: u32,
        /// Expected effective user ID.
        expected: u32,
    },
}

/// Resolved filesystem paths shared by the local client and backend.
#[derive(Clone, Debug)]
pub struct LocalPaths {
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    spawn_lock_path: PathBuf,
    config_path: PathBuf,
    state_dir: PathBuf,
    server_log_path: PathBuf,
    effective_uid: u32,
}

/// Filesystem identity of one validated endpoint candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SocketCandidateIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) changed_at_seconds: i64,
    pub(crate) changed_at_nanoseconds: i64,
}

impl LocalPaths {
    /// Resolves paths from the current Unix environment and platform directories.
    pub fn platform_default() -> Result<Self, LocalPathError> {
        let directories = ProjectDirs::from("", "", "moh")
            .ok_or(LocalPathError::PlatformDirectoriesUnavailable)?;
        let effective_uid = Uid::effective().as_raw();
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|directory| directory.join("moh"));
        let roots = PathRoots {
            runtime_dir,
            temp_dir: env::temp_dir(),
            config_dir: directories.config_dir().to_path_buf(),
            state_dir: directories
                .state_dir()
                .unwrap_or_else(|| directories.data_local_dir())
                .to_path_buf(),
            effective_uid,
        };
        Ok(Self::from_roots(roots))
    }

    /// Resolves paths from explicit roots, primarily for isolated tests.
    pub fn from_roots(roots: PathRoots) -> Self {
        let runtime_dir = roots
            .runtime_dir
            .unwrap_or_else(|| roots.temp_dir.join(format!("moh-{}", roots.effective_uid)));
        let socket_path = runtime_dir.join("backend.sock");
        let spawn_lock_path = runtime_dir.join("backend.lock");
        let config_path = roots.config_dir.join("config.toml");
        let server_log_path = roots.state_dir.join("server.log");
        Self {
            runtime_dir,
            socket_path,
            spawn_lock_path,
            config_path,
            state_dir: roots.state_dir,
            server_log_path,
            effective_uid: roots.effective_uid,
        }
    }

    /// Returns the trusted runtime directory for the local backend endpoint.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Returns the local Unix socket endpoint.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Returns the exact startup-lock path shared by concurrent clients.
    pub fn spawn_lock_path(&self) -> &Path {
        &self.spawn_lock_path
    }

    /// Returns the backend configuration file path.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Returns the platform state directory.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the detached backend diagnostic log path.
    pub fn server_log_path(&self) -> &Path {
        &self.server_log_path
    }

    /// Creates or validates the runtime directory, then restricts it to mode `0700`.
    pub fn prepare_runtime_dir(&self) -> Result<(), LocalPathError> {
        match mkdir(&self.runtime_dir, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(source) => {
                return Err(LocalPathError::CreateRuntimeDirectory {
                    path: self.runtime_dir.clone(),
                    source: source.into(),
                });
            }
        }

        let descriptor = open(
            &self.runtime_dir,
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| LocalPathError::OpenRuntimeDirectory {
            path: self.runtime_dir.clone(),
            source: source.into(),
        })?;
        let metadata = fstat(&descriptor).map_err(|source| LocalPathError::Inspect {
            path: self.runtime_dir.clone(),
            source: source.into(),
        })?;
        self.validate_runtime_directory(&metadata)?;
        fchmod(&descriptor, Mode::from_bits_truncate(0o700)).map_err(|source| {
            LocalPathError::RestrictRuntimeDirectory {
                path: self.runtime_dir.clone(),
                source: source.into(),
            }
        })
    }

    /// Securely creates missing state ancestors and the final state directory without following
    /// component symlinks, then restricts every newly created component and the final directory
    /// to mode `0700` through validated directory handles. Absolute paths start from an opened
    /// filesystem root, while relative injected paths start from an opened current directory;
    /// current-directory components are ignored and parent-directory components are rejected.
    pub fn prepare_state_dir(&self) -> Result<(), LocalPathError> {
        let mut components = Vec::new();
        for component in self.state_dir.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(component) => components.push(component),
                Component::ParentDir => {
                    return Err(LocalPathError::UnsafeStateDirectory {
                        path: self.state_dir.clone(),
                        reason: "parent-directory components are not allowed",
                    });
                }
                Component::Prefix(_) => {
                    return Err(LocalPathError::UnsafeStateDirectory {
                        path: self.state_dir.clone(),
                        reason: "platform path prefixes are not supported",
                    });
                }
            }
        }
        if components.is_empty() {
            return Err(LocalPathError::UnsafeStateDirectory {
                path: self.state_dir.clone(),
                reason: "the filesystem root or current directory is not a valid state path",
            });
        }

        let starting_path = if self.state_dir.is_absolute() {
            "/"
        } else {
            "."
        };
        let mut descriptor = open(
            starting_path,
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| LocalPathError::OpenStateDirectory {
            path: self.state_dir.clone(),
            source: source.into(),
        })?;

        let final_index = components.len() - 1;
        for (index, component) in components.into_iter().enumerate() {
            let created = match mkdirat(&descriptor, component, Mode::from_bits_truncate(0o700)) {
                Ok(()) => true,
                Err(Errno::EEXIST) => false,
                Err(source) => {
                    return Err(LocalPathError::CreateStateDirectory {
                        path: self.state_dir.clone(),
                        source: source.into(),
                    });
                }
            };
            if created {
                fchmodat(
                    &descriptor,
                    component,
                    Mode::from_bits_truncate(0o700),
                    FchmodatFlags::NoFollowSymlink,
                )
                .map_err(|source| LocalPathError::RestrictStateDirectory {
                    path: self.state_dir.clone(),
                    source: source.into(),
                })?;
            }
            let child = openat(
                &descriptor,
                component,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|source| LocalPathError::OpenStateDirectory {
                path: self.state_dir.clone(),
                source: source.into(),
            })?;

            let is_final = index == final_index;
            if created || is_final {
                let metadata = fstat(&child).map_err(|source| LocalPathError::Inspect {
                    path: self.state_dir.clone(),
                    source: source.into(),
                })?;
                if metadata.st_uid != self.effective_uid {
                    return Err(LocalPathError::StateDirectoryOwner {
                        path: self.state_dir.clone(),
                        found: metadata.st_uid,
                        expected: self.effective_uid,
                    });
                }
                if !SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFDIR) {
                    return Err(LocalPathError::StateDirectoryType {
                        path: self.state_dir.clone(),
                    });
                }
                fchmod(&child, Mode::from_bits_truncate(0o700)).map_err(|source| {
                    LocalPathError::RestrictStateDirectory {
                        path: self.state_dir.clone(),
                        source: source.into(),
                    }
                })?;
            }
            descriptor = child;
        }
        Ok(())
    }

    /// Validates that an existing endpoint is an owner-matching Unix socket.
    ///
    /// A missing endpoint is valid because it is the normal pre-bind state.
    pub fn validate_socket_candidate(&self) -> Result<(), LocalPathError> {
        self.socket_candidate_identity().map(drop)
    }

    /// Validates and identifies the endpoint using the same non-following metadata snapshot.
    pub(crate) fn socket_candidate_identity(
        &self,
    ) -> Result<Option<SocketCandidateIdentity>, LocalPathError> {
        match fs::symlink_metadata(&self.socket_path) {
            Ok(metadata) => {
                if metadata.uid() != self.effective_uid {
                    return Err(LocalPathError::SocketOwner {
                        path: self.socket_path.clone(),
                        found: metadata.uid(),
                        expected: self.effective_uid,
                    });
                }
                if !metadata.file_type().is_socket() {
                    return Err(LocalPathError::SocketType {
                        path: self.socket_path.clone(),
                    });
                }
                Ok(Some(SocketCandidateIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    changed_at_seconds: metadata.ctime(),
                    changed_at_nanoseconds: metadata.ctime_nsec(),
                }))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LocalPathError::Inspect {
                path: self.socket_path.clone(),
                source,
            }),
        }
    }

    fn validate_runtime_directory(
        &self,
        metadata: &nix::sys::stat::FileStat,
    ) -> Result<(), LocalPathError> {
        if metadata.st_uid != self.effective_uid {
            return Err(LocalPathError::RuntimeDirectoryOwner {
                path: self.runtime_dir.clone(),
                found: metadata.st_uid,
                expected: self.effective_uid,
            });
        }
        if !SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFDIR) {
            return Err(LocalPathError::RuntimeDirectoryType {
                path: self.runtime_dir.clone(),
            });
        }
        Ok(())
    }
}
