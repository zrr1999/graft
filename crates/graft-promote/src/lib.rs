use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use graft_core::TreeSnapshot;

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
pub struct MaterializedCommit {
    pub tree_id: String,
    pub commit_id: String,
    pub ref_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub url: String,
}

impl GixBackend {
    pub fn materialize_commit(
        &self,
        repo_path: impl AsRef<Path>,
        snapshot: &TreeSnapshot,
        blob_root: impl AsRef<Path>,
        message: &str,
        ref_name: Option<&str>,
    ) -> Result<MaterializedCommit> {
        let repo_path = repo_path.as_ref();
        let tree_id = self.write_tree(repo_path, snapshot, blob_root)?;
        let parent = current_head_commit(repo_path).ok();
        let mut args = vec!["commit-tree", tree_id.as_str(), "-m", message];
        if let Some(parent) = parent.as_deref() {
            args.push("-p");
            args.push(parent);
        }
        let commit_id = git_output(repo_path, &args, None)?.trim().to_string();
        if let Some(ref_name) = ref_name {
            self.update_ref(repo_path, ref_name, &commit_id)?;
        }
        Ok(MaterializedCommit {
            tree_id,
            commit_id,
            ref_name: ref_name.map(ToString::to_string),
        })
    }

    pub fn write_tree(
        &self,
        repo_path: impl AsRef<Path>,
        snapshot: &TreeSnapshot,
        blob_root: impl AsRef<Path>,
    ) -> Result<String> {
        let repo_path = repo_path.as_ref();
        let blob_root = blob_root.as_ref();
        let index_path = git_output(repo_path, &["rev-parse", "--git-path", "graft-index"], None)?
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&index_path);
        for entry in &snapshot.entries {
            validate_git_path(&entry.path)?;
            let bytes = std::fs::read(blob_root.join(&entry.hash))?;
            let git_blob = git_output(repo_path, &["hash-object", "-w", "--stdin"], Some(&bytes))?;
            git_output_with_env(
                repo_path,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "100644",
                    git_blob.trim(),
                    entry.path.as_str(),
                ],
                None,
                &[("GIT_INDEX_FILE", index_path.as_str())],
            )?;
        }
        let tree_id = git_output_with_env(
            repo_path,
            &["write-tree"],
            None,
            &[("GIT_INDEX_FILE", index_path.as_str())],
        )?
        .trim()
        .to_string();
        let _ = std::fs::remove_file(index_path);
        Ok(tree_id)
    }

    pub fn update_ref(
        &self,
        repo_path: impl AsRef<Path>,
        ref_name: &str,
        target: &str,
    ) -> Result<()> {
        git_output(repo_path.as_ref(), &["update-ref", ref_name, target], None)?;
        Ok(())
    }

    pub fn resolve_ref(&self, repo_path: impl AsRef<Path>, ref_name: &str) -> Result<String> {
        Ok(git_output(
            repo_path.as_ref(),
            &["rev-parse", "--verify", ref_name],
            None,
        )?
        .trim()
        .to_string())
    }

    pub fn promote_branch(
        &self,
        repo_path: impl AsRef<Path>,
        branch: &str,
        commit_id: &str,
    ) -> Result<String> {
        let ref_name = if branch.starts_with("refs/") {
            branch.to_string()
        } else {
            format!("refs/heads/{branch}")
        };
        self.update_ref(repo_path, &ref_name, commit_id)?;
        Ok(ref_name)
    }

    pub fn promote_release(
        &self,
        repo_path: impl AsRef<Path>,
        tag: &str,
        commit_id: &str,
    ) -> Result<String> {
        let ref_name = if tag.starts_with("refs/") {
            tag.to_string()
        } else {
            format!("refs/tags/{tag}")
        };
        self.update_ref(repo_path, &ref_name, commit_id)?;
        Ok(ref_name)
    }

    pub fn create_pull_request(
        &self,
        repo_path: impl AsRef<Path>,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        let output = command_output(
            repo_path.as_ref(),
            "gh",
            &[
                "pr", "create", "--head", head, "--base", base, "--title", title, "--body", body,
            ],
            None,
            &[],
        )?;
        Ok(PullRequest {
            url: output.trim().to_string(),
        })
    }
}

fn current_head_commit(repo_path: &Path) -> Result<String> {
    Ok(
        git_output(repo_path, &["rev-parse", "--verify", "HEAD^{commit}"], None)?
            .trim()
            .to_string(),
    )
}

fn validate_git_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(char::from(10))
        || path.contains(char::from(9))
        || path.starts_with("/")
    {
        return Err(GitError::UnsupportedPath(path.to_string()));
    }
    Ok(())
}

fn git_output(repo_path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    git_output_with_env(repo_path, args, input, &[])
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
    fn materializes_snapshot_as_git_commit_and_ref() {
        let dir = std::env::temp_dir().join(format!("graft-promote-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        git_output(&dir, &["init"], None).unwrap();
        let bytes = b"pub fn demo() {}\n";
        let hash = blake3::hash(bytes).to_hex().to_string();
        std::fs::write(dir.join("blobs").join(&hash), bytes).unwrap();
        let snapshot = TreeSnapshot::new(vec![graft_core::TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: bytes.len() as u64,
        }]);

        let materialized = GixBackend
            .materialize_commit(
                &dir,
                &snapshot,
                dir.join("blobs"),
                "materialize test",
                Some("refs/graft/patches/test"),
            )
            .unwrap();
        assert!(!materialized.commit_id.is_empty());
        let resolved = git_output(&dir, &["rev-parse", "refs/graft/patches/test"], None).unwrap();
        assert_eq!(resolved.trim(), materialized.commit_id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
