use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProjectRootLocator;

#[derive(Clone, Copy, Debug)]
enum ProjectRootMarker {
    Git,
}

impl ProjectRootLocator {
    pub(crate) fn locate(&self, cwd: &Path) -> PathBuf {
        cwd.ancestors()
            .find(|directory| ProjectRootMarker::Git.matches(directory))
            .map(Path::to_owned)
            .unwrap_or_else(|| cwd.to_owned())
    }
}

impl ProjectRootMarker {
    fn matches(self, directory: &Path) -> bool {
        match self {
            Self::Git => directory.join(".git").exists(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::project_root::ProjectRootLocator;
    use tempfile::tempdir;

    #[test]
    fn locates_the_nearest_git_directory() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("repository");
        let nested = root.join("crates").join("cli");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(ProjectRootLocator.locate(&nested), root);
    }

    #[test]
    fn accepts_a_git_worktree_marker_file() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("worktree");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /elsewhere/worktrees/moh").unwrap();

        assert_eq!(ProjectRootLocator.locate(&nested), root);
    }

    #[test]
    fn uses_the_working_directory_when_no_marker_exists() {
        let directory = tempdir().unwrap();
        let cwd = directory.path().join("plain").join("nested");
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(ProjectRootLocator.locate(&cwd), cwd);
    }
}
