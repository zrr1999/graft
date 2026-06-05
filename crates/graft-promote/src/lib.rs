use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use graft_core::TreeSnapshot;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git operation failed: {0}")]
    Git(String),
    #[error("invalid git ref `{ref_name}`: {reason}")]
    InvalidRef {
        ref_name: String,
        reason: &'static str,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git output was not utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("cannot materialize unsupported path {0:?}")]
    UnsupportedPath(String),
    #[error("cannot remove temporary git index {path}: {source}")]
    IndexCleanup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
        let parent = current_head_commit(repo_path)?;
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
        let index_path = graft_index_path(repo_path)?;
        remove_git_index_if_exists(&index_path)?;
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
                &[("GIT_INDEX_FILE", index_path.as_os_str())],
            )?;
        }
        let tree_id = git_output_with_env(
            repo_path,
            &["write-tree"],
            None,
            &[("GIT_INDEX_FILE", index_path.as_os_str())],
        )?
        .trim()
        .to_string();
        remove_git_index_if_exists(&index_path)?;
        Ok(tree_id)
    }

    pub fn update_ref(
        &self,
        repo_path: impl AsRef<Path>,
        ref_name: &str,
        target: &str,
    ) -> Result<()> {
        validate_writable_ref_name(ref_name)?;
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

    pub fn try_resolve_ref(
        &self,
        repo_path: impl AsRef<Path>,
        ref_name: &str,
    ) -> Result<Option<String>> {
        match self.resolve_ref(repo_path, ref_name) {
            Ok(commit_id) => Ok(Some(commit_id)),
            Err(GitError::Git(message)) if is_missing_revision_error(&message) => Ok(None),
            Err(error) => Err(error),
        }
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

fn current_head_commit(repo_path: &Path) -> Result<Option<String>> {
    match git_output(repo_path, &["rev-parse", "--verify", "HEAD^{commit}"], None) {
        Ok(commit) => Ok(Some(commit.trim().to_string())),
        Err(GitError::Git(message)) if is_missing_revision_error(&message) => {
            if head_ref_is_unborn(repo_path)? {
                Ok(None)
            } else {
                Err(GitError::Git(format!(
                    "HEAD could not be resolved: {message}"
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn is_missing_revision_error(message: &str) -> bool {
    message.contains("Needed a single revision") && !message.contains("warning:")
}

fn head_ref_is_unborn(repo_path: &Path) -> Result<bool> {
    match git_output(repo_path, &["symbolic-ref", "-q", "HEAD"], None) {
        Ok(ref_name) => Ok(!git_path(repo_path, ref_name.trim())?.exists()),
        Err(GitError::Git(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

fn graft_index_path(repo_path: &Path) -> Result<PathBuf> {
    git_path(repo_path, "graft-index")
}

fn git_path(repo_path: &Path, path: &str) -> Result<PathBuf> {
    let raw = git_output(repo_path, &["rev-parse", "--git-path", path], None)?;
    let path = PathBuf::from(raw.trim());
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_path.join(path))
    }
}

fn remove_git_index_if_exists(index_path: &Path) -> Result<()> {
    match std::fs::remove_file(index_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(GitError::IndexCleanup {
            path: index_path.to_path_buf(),
            source: error,
        }),
    }
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

fn validate_writable_ref_name(ref_name: &str) -> Result<()> {
    if ref_name.is_empty() {
        return invalid_ref(ref_name, "must not be empty");
    }
    if ref_name != ref_name.trim() {
        return invalid_ref(ref_name, "must not contain leading or trailing whitespace");
    }
    if !ref_name.starts_with("refs/") {
        return invalid_ref(ref_name, "must start with refs/");
    }
    if ref_name.ends_with('/') {
        return invalid_ref(ref_name, "must not end with /");
    }
    if ref_name.contains("//") {
        return invalid_ref(ref_name, "must not contain consecutive slashes");
    }
    if ref_name.contains("..") {
        return invalid_ref(ref_name, "must not contain ..");
    }
    if ref_name.contains("@{") {
        return invalid_ref(ref_name, "must not contain @{");
    }
    if ref_name.ends_with('.') {
        return invalid_ref(ref_name, "must not end with .");
    }
    if ref_name.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return invalid_ref(
            ref_name,
            "must not contain whitespace or control characters",
        );
    }
    if ref_name
        .bytes()
        .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
    {
        return invalid_ref(ref_name, "must not contain ~ ^ : ? * [ or \\");
    }
    for component in ref_name.split('/') {
        if component.is_empty() {
            return invalid_ref(ref_name, "must not contain empty path components");
        }
        if component.starts_with('.') {
            return invalid_ref(ref_name, "path components must not start with .");
        }
        if component.ends_with(".lock") {
            return invalid_ref(ref_name, "path components must not end with .lock");
        }
    }
    Ok(())
}

fn invalid_ref<T>(ref_name: &str, reason: &'static str) -> Result<T> {
    Err(GitError::InvalidRef {
        ref_name: ref_name.to_string(),
        reason,
    })
}

fn git_output(repo_path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    git_output_with_env(repo_path, args, input, &[])
}

fn git_output_with_env(
    repo_path: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<String> {
    Ok(String::from_utf8(command_output_bytes(
        repo_path,
        "git",
        &git_args(repo_path, args),
        input,
        envs,
    )?)?)
}

fn git_args(repo_path: &Path, args: &[&str]) -> Vec<OsString> {
    let mut git_args = Vec::with_capacity(args.len() + 2);
    git_args.push(OsString::from("-C"));
    git_args.push(repo_path.as_os_str().to_os_string());
    git_args.extend(args.iter().map(OsString::from));
    git_args
}

fn command_output<A>(
    current_dir: &Path,
    program: &str,
    args: &[A],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<String>
where
    A: AsRef<OsStr>,
{
    Ok(String::from_utf8(command_output_bytes(
        current_dir,
        program,
        args,
        input,
        envs,
    )?)?)
}

fn command_output_bytes<A>(
    current_dir: &Path,
    program: &str,
    args: &[A],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<Vec<u8>>
where
    A: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .current_dir(current_dir)
        .env("GIT_AUTHOR_NAME", "Graft")
        .env("GIT_AUTHOR_EMAIL", "graft@example.invalid")
        .env("GIT_COMMITTER_NAME", "Graft")
        .env("GIT_COMMITTER_EMAIL", "graft@example.invalid")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in args {
        command.arg(arg.as_ref());
    }
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
    fn writable_ref_validation_accepts_standard_and_graft_refs() {
        for ref_name in [
            "refs/heads/main",
            "refs/tags/v1.0.0",
            "refs/graft/patches/abc123",
            "refs/graft/custom/nested",
        ] {
            validate_writable_ref_name(ref_name).unwrap();
        }
    }

    #[test]
    fn writable_ref_validation_rejects_invalid_git_refs() {
        for (ref_name, reason) in [
            ("", "must not be empty"),
            (" main", "leading or trailing whitespace"),
            ("main", "must start with refs/"),
            ("refs/graft/patches/patch:abc123", "~ ^ : ? * [ or \\"),
            ("refs/graft//patches/abc123", "consecutive slashes"),
            ("refs/graft/patches/abc..123", "must not contain .."),
            ("refs/graft/patches/@{abc123", "must not contain @{"),
            ("refs/graft/patches/abc123.", "must not end with ."),
            ("refs/graft/patches/.abc123", "must not start with ."),
            ("refs/graft/patches/abc123.lock", "must not end with .lock"),
            (
                "refs/graft/patches/abc 123",
                "whitespace or control characters",
            ),
        ] {
            let message = validate_writable_ref_name(ref_name)
                .unwrap_err()
                .to_string();
            assert!(message.contains(ref_name), "{ref_name}: {message}");
            assert!(message.contains(reason), "{ref_name}: {message}");
        }
    }

    #[test]
    fn update_ref_rejects_invalid_ref_before_invoking_git() {
        let error = GixBackend
            .update_ref(
                Path::new("/definitely/missing-graft-promote-repo"),
                "refs/graft/patches/patch:abc123",
                "deadbeef",
            )
            .unwrap_err();

        assert!(matches!(error, GitError::InvalidRef { .. }));
        let message = error.to_string();
        assert!(message.contains("refs/graft/patches/patch:abc123"));
        assert!(!message.contains("not a git repository"), "{message}");
    }

    #[test]
    fn try_resolve_ref_returns_none_only_for_missing_refs() {
        let dir = std::env::temp_dir().join(format!(
            "graft-promote-try-resolve-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_output(&dir, &["init"], None).unwrap();

        assert_eq!(
            GixBackend
                .try_resolve_ref(&dir, "refs/graft/patches/missing")
                .unwrap(),
            None
        );
        let broken_ref = dir.join(".git/refs/graft/patches/broken");
        std::fs::create_dir_all(broken_ref.parent().unwrap()).unwrap();
        std::fs::write(&broken_ref, "notasha\n").unwrap();

        let broken = GixBackend
            .try_resolve_ref(&dir, "refs/graft/patches/broken")
            .unwrap_err()
            .to_string();

        assert!(broken.contains("git operation failed"), "{broken}");
        assert!(broken.contains("ignoring broken ref"), "{broken}");

        let not_git = std::env::temp_dir().join(format!(
            "graft-promote-try-resolve-not-git-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&not_git);
        std::fs::create_dir_all(&not_git).unwrap();
        let error = GixBackend
            .try_resolve_ref(&not_git, "refs/graft/patches/missing")
            .unwrap_err()
            .to_string();

        assert!(error.contains("not a git repository"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&not_git);
    }

    #[cfg(unix)]
    #[test]
    fn git_args_preserve_non_utf8_repo_path() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(b"/tmp/graft-promote-\xFF".to_vec());
        let path = PathBuf::from(raw);
        let args = git_args(&path, &["status"]);

        assert_eq!(args[0], OsString::from("-C"));
        assert_eq!(args[1].as_os_str(), path.as_os_str());
        assert_ne!(args[1], OsString::from("."));
    }

    #[cfg(unix)]
    #[test]
    fn command_output_accepts_non_utf8_env_values() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(b"/tmp/graft-index-\xFF".to_vec());
        let output = command_output_bytes(
            Path::new("."),
            "sh",
            &["-c", "printf ok"],
            None,
            &[("GIT_INDEX_FILE", raw.as_os_str())],
        )
        .unwrap();

        assert_eq!(output, b"ok");
    }

    #[test]
    fn graft_index_path_resolves_under_repo() {
        let dir = std::env::temp_dir().join(format!(
            "graft-promote-index-path-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_output(&dir, &["init"], None).unwrap();

        let index_path = graft_index_path(&dir).unwrap();

        assert_eq!(index_path, dir.join(".git").join("graft-index"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_tree_reports_stale_index_cleanup_failure() {
        let dir = std::env::temp_dir().join(format!(
            "graft-promote-stale-index-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        git_output(&dir, &["init"], None).unwrap();
        let index_path = graft_index_path(&dir).unwrap();
        std::fs::create_dir_all(&index_path).unwrap();
        let snapshot = TreeSnapshot::new(Vec::new());

        let error = GixBackend
            .write_tree(&dir, &snapshot, dir.join("blobs"))
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("cannot remove temporary git index"),
            "{error}"
        );
        assert!(
            index_path.exists(),
            "write_tree must not remove a directory as if it were a temporary index file"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn materialize_commit_uses_existing_head_as_parent() {
        let dir =
            std::env::temp_dir().join(format!("graft-promote-parent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        git_output(&dir, &["init"], None).unwrap();
        git_output(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"], None).unwrap();

        let initial_bytes = b"initial\n";
        let initial_hash = blake3::hash(initial_bytes).to_hex().to_string();
        std::fs::write(dir.join("blobs").join(&initial_hash), initial_bytes).unwrap();
        let initial_snapshot = TreeSnapshot::new(vec![graft_core::TreeEntry {
            path: "README.md".to_string(),
            hash: initial_hash,
            size: initial_bytes.len() as u64,
        }]);
        let initial = GixBackend
            .materialize_commit(
                &dir,
                &initial_snapshot,
                dir.join("blobs"),
                "initial",
                Some("refs/heads/main"),
            )
            .unwrap();

        let next_bytes = b"next\n";
        let next_hash = blake3::hash(next_bytes).to_hex().to_string();
        std::fs::write(dir.join("blobs").join(&next_hash), next_bytes).unwrap();
        let next_snapshot = TreeSnapshot::new(vec![graft_core::TreeEntry {
            path: "README.md".to_string(),
            hash: next_hash,
            size: next_bytes.len() as u64,
        }]);
        let next = GixBackend
            .materialize_commit(
                &dir,
                &next_snapshot,
                dir.join("blobs"),
                "next",
                Some("refs/heads/main"),
            )
            .unwrap();

        let commit = git_output(&dir, &["cat-file", "-p", &next.commit_id], None).unwrap();
        assert!(
            commit.contains(&format!("parent {}", initial.commit_id)),
            "{commit}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_commit_rejects_broken_head_ref() {
        let dir = std::env::temp_dir().join(format!(
            "graft-promote-broken-head-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        git_output(&dir, &["init"], None).unwrap();
        git_output(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"], None).unwrap();
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), "notasha\n").unwrap();

        let bytes = b"next\n";
        let hash = blake3::hash(bytes).to_hex().to_string();
        std::fs::write(dir.join("blobs").join(&hash), bytes).unwrap();
        let snapshot = TreeSnapshot::new(vec![graft_core::TreeEntry {
            path: "README.md".to_string(),
            hash,
            size: bytes.len() as u64,
        }]);

        let error = GixBackend
            .materialize_commit(&dir, &snapshot, dir.join("blobs"), "next", None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("git operation failed"), "{error}");
        assert!(error.contains("HEAD could not be resolved"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
