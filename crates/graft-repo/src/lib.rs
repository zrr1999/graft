use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use graft_core::{RepoBaseState, StateId, TreeEntry, TreeSnapshot};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git operation failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git output was not utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("cannot materialize unsupported path {0:?}")]
    UnsupportedPath(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Clone, Debug, Default)]
pub struct GixBackend;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnsuredRepo {
    pub path: PathBuf,
    pub cloned: bool,
}

impl GixBackend {
    pub fn ensure_repo(&self, url: &str, path: impl AsRef<Path>) -> Result<EnsuredRepo> {
        let path = path.as_ref();
        if path.exists() {
            gix::discover(path).map_err(|err| {
                GitError::Git(format!(
                    "{} exists but is not a discoverable git repository: {err}",
                    path.display()
                ))
            })?;
            return Ok(EnsuredRepo {
                path: path.to_path_buf(),
                cloned: false,
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        command_output(
            Path::new("."),
            "git",
            &["clone", url, path.to_string_lossy().as_ref()],
            None,
            &[],
        )?;
        Ok(EnsuredRepo {
            path: path.to_path_buf(),
            cloned: true,
        })
    }

    pub fn sync_repo(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        git_output(repo_path.as_ref(), &["fetch", "--all", "--prune"], None)?;
        Ok(())
    }

    pub fn repo_tree_state(
        &self,
        repo_id: &str,
        repo_path: impl AsRef<Path>,
        treeish: &str,
    ) -> Result<StateId> {
        let repo_path = repo_path.as_ref();
        let resolved_tree_oid = self.repo_tree_oid(repo_path, treeish)?;
        Ok(StateId::RepoTree(RepoBaseState::new(
            repo_id,
            treeish,
            resolved_tree_oid,
        )))
    }

    fn repo_tree_oid(&self, repo_path: &Path, treeish: &str) -> Result<String> {
        if is_plain_ref_name(treeish) {
            let remote_ref = format!("refs/remotes/origin/{treeish}^{{tree}}");
            if let Ok(tree_id) =
                git_output(repo_path, &["rev-parse", "--verify", &remote_ref], None)
            {
                return Ok(tree_id.trim().to_string());
            }
        }
        let StateId::GitTree(resolved_tree_oid) = self.tree_state(repo_path, treeish)? else {
            unreachable!("tree_state always returns StateId::GitTree")
        };
        Ok(resolved_tree_oid)
    }

    pub fn head_tree_state(&self, path: impl AsRef<Path>) -> Result<StateId> {
        let repo = gix::discover(path).map_err(|err| GitError::Git(err.to_string()))?;
        let tree_id = repo
            .head_tree_id()
            .map_err(|err| GitError::Git(err.to_string()))?;
        Ok(StateId::GitTree(tree_id.to_string()))
    }

    pub fn tree_state(&self, path: impl AsRef<Path>, treeish: &str) -> Result<StateId> {
        let spec = format!("{treeish}^{{tree}}");
        let tree_id = git_output(path.as_ref(), &["rev-parse", "--verify", &spec], None)?
            .trim()
            .to_string();
        Ok(StateId::GitTree(tree_id))
    }

    pub fn tree_snapshot(
        &self,
        repo_path: impl AsRef<Path>,
        treeish: &str,
        blob_root: Option<impl AsRef<Path>>,
    ) -> Result<TreeSnapshot> {
        let repo_path = repo_path.as_ref();
        let names = git_output_bytes(repo_path, &["ls-tree", "-rz", "--name-only", treeish], None)?;
        let mut entries = Vec::new();
        for raw_path in names
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = String::from_utf8(raw_path.to_vec())
                .map_err(|err| GitError::Git(err.to_string()))?;
            validate_git_path(&path)?;
            let spec = format!("{treeish}:{path}");
            let bytes = git_output_bytes(repo_path, &["show", spec.as_str()], None)?;
            let hash = blake3::hash(&bytes).to_hex().to_string();
            if let Some(blob_root) = blob_root.as_ref() {
                let blob_root = blob_root.as_ref();
                std::fs::create_dir_all(blob_root)?;
                let blob_path = blob_root.join(&hash);
                if !blob_path.exists() {
                    std::fs::write(blob_path, &bytes)?;
                }
            }
            entries.push(TreeEntry {
                path,
                hash,
                size: bytes.len() as u64,
            });
        }
        Ok(TreeSnapshot::new(entries))
    }
}

fn is_plain_ref_name(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(':')
        && !value.starts_with("refs/")
        && !value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn validate_git_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\n') || path.contains('\t') || path.starts_with('/') {
        return Err(GitError::UnsupportedPath(path.to_string()));
    }
    Ok(())
}

fn git_output(repo_path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    git_output_with_env(repo_path, args, input, &[])
}

fn git_output_bytes(repo_path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    command_output_bytes(repo_path, "git", &git_args(repo_path, args), input, &[])
}

fn git_output_with_env(
    repo_path: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    envs: &[(&str, &str)],
) -> Result<String> {
    Ok(String::from_utf8(command_output_bytes(
        repo_path,
        "git",
        &git_args(repo_path, args),
        input,
        envs,
    )?)?)
}

fn git_args<'a>(repo_path: &'a Path, args: &'a [&str]) -> Vec<&'a str> {
    let mut git_args = vec!["-C", repo_path.to_str().unwrap_or(".")];
    git_args.extend_from_slice(args);
    git_args
}

fn command_output(
    current_dir: &Path,
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    envs: &[(&str, &str)],
) -> Result<String> {
    Ok(String::from_utf8(command_output_bytes(
        current_dir,
        program,
        args,
        input,
        envs,
    )?)?)
}

fn command_output_bytes(
    current_dir: &Path,
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    envs: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .current_dir(current_dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Graft")
        .env("GIT_AUTHOR_EMAIL", "graft@example.invalid")
        .env("GIT_COMMITTER_NAME", "Graft")
        .env("GIT_COMMITTER_EMAIL", "graft@example.invalid")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        let Some(mut stdin) = child.stdin.take() else {
            return Err(GitError::Git("failed to open git stdin".to_string()));
        };
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(GitError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_is_constructible() {
        let backend = GixBackend;
        let _ = format!("{backend:?}");
    }

    #[test]
    fn ensures_and_syncs_local_repo_clone() {
        let root =
            std::env::temp_dir().join(format!("graft-repo-clone-test-{}", std::process::id()));
        let source = root.join("source");
        let clone = root.join("clone");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();
        git_output(&source, &["init", "-b", "main"], None).unwrap();
        std::fs::write(source.join("README.md"), b"demo\n").unwrap();
        git_output(&source, &["add", "README.md"], None).unwrap();
        git_output(
            &source,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Graft Test",
                "-c",
                "user.email=graft@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
            None,
        )
        .unwrap();

        let backend = GixBackend;
        let ensured = backend
            .ensure_repo(source.to_string_lossy().as_ref(), &clone)
            .unwrap();
        assert!(ensured.cloned);
        assert!(clone.join(".git").exists());
        let existing = backend
            .ensure_repo(source.to_string_lossy().as_ref(), &clone)
            .unwrap();
        assert!(!existing.cloned);
        backend.sync_repo(&clone).unwrap();
        let state = backend.repo_tree_state("demo", &clone, "main").unwrap();
        let StateId::RepoTree(repo) = state else {
            panic!("expected repo tree state");
        };
        assert_eq!(repo.repo_id, "demo");
        assert_eq!(repo.treeish, "main");
        assert!(!repo.resolved_tree_oid.is_empty());
        let initial_tree = repo.resolved_tree_oid.clone();

        std::fs::write(source.join("README.md"), b"updated\n").unwrap();
        git_output(&source, &["add", "README.md"], None).unwrap();
        git_output(
            &source,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Graft Test",
                "-c",
                "user.email=graft@example.invalid",
                "commit",
                "-m",
                "update",
            ],
            None,
        )
        .unwrap();
        backend.sync_repo(&clone).unwrap();
        let updated = backend.repo_tree_state("demo", &clone, "main").unwrap();
        let StateId::RepoTree(updated) = updated else {
            panic!("expected repo tree state");
        };
        assert_ne!(updated.resolved_tree_oid, initial_tree);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn captures_git_tree_snapshot_with_blake3_blobs() {
        let dir =
            std::env::temp_dir().join(format!("graft-repo-snapshot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_output(&dir, &["init", "-b", "main"], None).unwrap();
        std::fs::write(dir.join("README.md"), b"demo\n").unwrap();
        git_output(&dir, &["add", "README.md"], None).unwrap();
        git_output(
            &dir,
            &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
            None,
        )
        .unwrap();

        let snapshot = GixBackend
            .tree_snapshot(&dir, "HEAD", Some(dir.join("graft-blobs")))
            .unwrap();
        let state = GixBackend.tree_state(&dir, "main").unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].path, "README.md");
        assert!(matches!(state, StateId::GitTree(_)));
        assert!(
            dir.join("graft-blobs")
                .join(&snapshot.entries[0].hash)
                .exists()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
