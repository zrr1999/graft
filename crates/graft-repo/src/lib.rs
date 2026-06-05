use std::ffi::{OsStr, OsString};
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
    #[error(
        "[E_REPO_CLONE_PATH_PARENT_REQUIRED] repo clone destination must include an explicit parent directory: {0}"
    )]
    CloneDestinationMissingParent(PathBuf),
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
    pub fn ensure_repo(
        &self,
        url: impl AsRef<OsStr>,
        path: impl AsRef<Path>,
    ) -> Result<EnsuredRepo> {
        let expected_origin = clone_source_arg(url.as_ref())?;
        let path = path.as_ref();
        if path.exists() {
            gix::discover(path).map_err(|err| {
                GitError::Git(format!(
                    "{} exists but is not a discoverable git repository: {err}",
                    path.display()
                ))
            })?;
            ensure_origin_matches(path, &expected_origin)?;
            return Ok(EnsuredRepo {
                path: path.to_path_buf(),
                cloned: false,
            });
        }
        let plan = clone_command_plan_from_source(expected_origin, path)?;
        std::fs::create_dir_all(&plan.current_dir)?;
        command_output(&plan.current_dir, "git", &plan.args, None, &[])?;
        Ok(EnsuredRepo {
            path: path.to_path_buf(),
            cloned: true,
        })
    }

    pub fn sync_repo(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        git_output(repo_path.as_ref(), &["fetch", "--all", "--prune"], None)?;
        Ok(())
    }

    pub fn remote_default_branch(&self, repo_path: impl AsRef<Path>) -> Result<Option<String>> {
        let output = match git_output(
            repo_path.as_ref(),
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
            None,
        ) {
            Ok(output) => output,
            Err(GitError::Git(message))
                if message.is_empty() || message.contains("not a symbolic ref") =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let branch = output.trim();
        if branch.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            branch.strip_prefix("origin/").unwrap_or(branch).to_string(),
        ))
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
            match git_output(repo_path, &["rev-parse", "--verify", &remote_ref], None) {
                Ok(tree_id) => return Ok(tree_id.trim().to_string()),
                Err(GitError::Git(message)) if is_missing_revision_error(&message) => {}
                Err(error) => return Err(error),
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

fn is_missing_revision_error(message: &str) -> bool {
    message.contains("Needed a single revision") && !message.contains("warning:")
}

fn validate_git_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains('\n')
        || path.contains('\t')
        || Path::new(path).is_absolute()
    {
        return Err(GitError::UnsupportedPath(path.to_string()));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(GitError::UnsupportedPath(path.to_string()));
        }
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

fn git_args(repo_path: &Path, args: &[&str]) -> Vec<OsString> {
    let mut git_args = Vec::with_capacity(args.len() + 2);
    git_args.push(OsString::from("-C"));
    git_args.push(repo_path.as_os_str().to_os_string());
    git_args.extend(args.iter().map(OsString::from));
    git_args
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CloneCommandPlan {
    current_dir: PathBuf,
    args: Vec<OsString>,
}

#[cfg(test)]
fn clone_command_plan(source: &OsStr, destination: &Path) -> Result<CloneCommandPlan> {
    let source = clone_source_arg(source)?;
    clone_command_plan_from_source(source, destination)
}

fn clone_command_plan_from_source(
    source: OsString,
    destination: &Path,
) -> Result<CloneCommandPlan> {
    let (current_dir, destination_name) = clone_destination(destination)?;
    Ok(CloneCommandPlan {
        current_dir: current_dir.to_path_buf(),
        args: clone_args(source.as_os_str(), destination_name),
    })
}

fn clone_destination(destination: &Path) -> Result<(&Path, &OsStr)> {
    let Some(destination_name) = destination.file_name() else {
        return Err(GitError::CloneDestinationMissingParent(
            destination.to_path_buf(),
        ));
    };
    let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(GitError::CloneDestinationMissingParent(
            destination.to_path_buf(),
        ));
    };
    Ok((parent, destination_name))
}

fn clone_source_arg(source: &OsStr) -> Result<OsString> {
    let source_path = Path::new(source);
    if source_path.is_absolute() || source_should_pass_through(source) {
        return Ok(source.to_os_string());
    }
    Ok(std::env::current_dir()?.join(source_path).into_os_string())
}

fn source_should_pass_through(source: &OsStr) -> bool {
    let Some(source) = source.to_str() else {
        return false;
    };
    if source.contains("://") || source.starts_with("file:") || source.starts_with('~') {
        return true;
    }
    let first_colon = source.find(':');
    let first_slash = source.find(['/', '\\']);
    matches!(first_colon, Some(colon) if first_slash.is_none_or(|slash| colon < slash))
}

fn clone_args(source: &OsStr, destination_name: &OsStr) -> Vec<OsString> {
    vec![
        OsString::from("clone"),
        source.to_os_string(),
        destination_name.to_os_string(),
    ]
}

fn ensure_origin_matches(repo_path: &Path, expected_origin: &OsStr) -> Result<()> {
    let expected = os_str_bytes(expected_origin);
    let actual = git_output_bytes(repo_path, &["remote", "get-url", "origin"], None).map_err(
        |err| match err {
            GitError::Git(message)
                if message.contains("No such remote")
                    || message.contains("No such remote 'origin'") =>
            {
                GitError::Git(format!(
                    "[E_REPO_CACHE_ORIGIN_MISSING] repo cache {} has no origin remote; remove the cache or recreate it with `graft repo sync`",
                    repo_path.display(),
                ))
            }
            other => other,
        },
    )?;
    let actual = trim_git_line_ending(&actual);
    if actual != expected {
        return Err(GitError::Git(format!(
            "[E_REPO_CACHE_URL_DRIFT] repo cache {} has origin {} but graft.toml url resolves to {}; remove the cache or fix the repo URL",
            repo_path.display(),
            display_origin(actual),
            display_origin(&expected),
        )));
    }
    Ok(())
}

fn trim_git_line_ending(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &bytes[..end]
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn display_origin(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(value) => format!("`{value}`"),
        Err(_) => format!("{bytes:?}"),
    }
}

fn command_output<A>(
    current_dir: &Path,
    program: &str,
    args: &[A],
    input: Option<&[u8]>,
    envs: &[(&str, &str)],
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
    envs: &[(&str, &str)],
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

    #[cfg(unix)]
    #[test]
    fn git_args_preserve_non_utf8_repo_path() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(b"/tmp/graft-repo-\xFF".to_vec());
        let path = PathBuf::from(raw);
        let args = git_args(&path, &["status"]);

        assert_eq!(args[0], OsString::from("-C"));
        assert_eq!(args[1].as_os_str(), path.as_os_str());
        assert_ne!(args[1], OsString::from("."));
    }

    #[cfg(unix)]
    #[test]
    fn clone_args_preserve_non_utf8_source_path() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(b"/tmp/graft-source-\xFF".to_vec());
        let destination_name = OsStr::new("graft-clone");
        let args = clone_args(raw.as_os_str(), destination_name);

        assert_eq!(args[0], OsString::from("clone"));
        assert_eq!(args[1].as_os_str(), raw.as_os_str());
        assert_eq!(args[2].as_os_str(), destination_name);
    }

    #[test]
    fn clone_command_plan_runs_under_destination_parent() {
        let plan = clone_command_plan(
            OsStr::new("https://example.invalid/repo.git"),
            Path::new("/tmp/graft-cache/demo"),
        )
        .unwrap();

        assert_eq!(plan.current_dir, PathBuf::from("/tmp/graft-cache"));
        assert_eq!(plan.args[0], OsString::from("clone"));
        assert_eq!(
            plan.args[1],
            OsString::from("https://example.invalid/repo.git")
        );
        assert_eq!(plan.args[2], OsString::from("demo"));
    }

    #[test]
    fn clone_command_plan_requires_explicit_destination_parent() {
        let error = clone_command_plan(
            OsStr::new("https://example.invalid/repo.git"),
            Path::new("demo"),
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("[E_REPO_CLONE_PATH_PARENT_REQUIRED]"),
            "{error}"
        );
        assert!(error.contains("demo"), "{error}");
    }

    #[test]
    fn clone_command_plan_absolutizes_relative_local_source() {
        let current_dir = std::env::current_dir().unwrap();
        let plan =
            clone_command_plan(OsStr::new("../source"), Path::new("/tmp/cache/demo")).unwrap();

        assert_eq!(plan.current_dir, PathBuf::from("/tmp/cache"));
        assert_eq!(plan.args[1], current_dir.join("../source").into_os_string());
        assert_eq!(plan.args[2], OsString::from("demo"));
    }

    #[test]
    fn clone_command_plan_preserves_scp_like_remote_source() {
        let plan = clone_command_plan(
            OsStr::new("git@example.invalid:owner/repo.git"),
            Path::new("/tmp/cache/demo"),
        )
        .unwrap();

        assert_eq!(
            plan.args[1],
            OsString::from("git@example.invalid:owner/repo.git")
        );
        assert_eq!(plan.args[2], OsString::from("demo"));
    }

    #[test]
    fn clone_command_plan_preserves_tilde_source_for_git() {
        let plan =
            clone_command_plan(OsStr::new("~/repo.git"), Path::new("/tmp/cache/demo")).unwrap();

        assert_eq!(plan.args[1], OsString::from("~/repo.git"));
        assert_eq!(plan.args[2], OsString::from("demo"));
    }

    #[test]
    fn git_tree_paths_must_be_relative_normal_components() {
        validate_git_path("README.md").unwrap();
        validate_git_path("src/lib.rs").unwrap();

        for path in [
            "",
            "/absolute",
            ".",
            "./file.txt",
            "dir//file.txt",
            "dir/",
            "../escape",
            "dir/../escape",
            "dir/./file.txt",
            "line\nbreak",
            "tab\tpath",
        ] {
            assert!(
                matches!(validate_git_path(path), Err(GitError::UnsupportedPath(_))),
                "path should be rejected: {path:?}"
            );
        }
    }

    #[test]
    fn origin_output_trimming_preserves_url_bytes() {
        assert_eq!(trim_git_line_ending(b"/tmp/source \n"), b"/tmp/source ");
        assert_eq!(trim_git_line_ending(b"/tmp/source \r\n"), b"/tmp/source ");
        assert_eq!(trim_git_line_ending(b"/tmp/source \t\n"), b"/tmp/source \t");
        assert_eq!(trim_git_line_ending(b"/tmp/source"), b"/tmp/source");
        assert_eq!(
            trim_git_line_ending(b"/tmp/source-\xFF\n"),
            b"/tmp/source-\xFF"
        );
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
        let ensured = backend.ensure_repo(source.as_os_str(), &clone).unwrap();
        assert!(ensured.cloned);
        assert!(clone.join(".git").exists());
        let existing = backend.ensure_repo(source.as_os_str(), &clone).unwrap();
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
    fn ensure_repo_accepts_existing_cache_with_origin_trailing_space() {
        let root = std::env::temp_dir().join(format!(
            "graft-repo-origin-trailing-space-test-{}",
            std::process::id()
        ));
        let source = root.join("source ");
        let clone = root.join("clone");
        let _ = std::fs::remove_dir_all(&root);
        init_source_repo(&source, "a\n");

        let backend = GixBackend;
        backend.ensure_repo(source.as_os_str(), &clone).unwrap();
        let existing = backend.ensure_repo(source.as_os_str(), &clone).unwrap();

        assert!(!existing.cloned);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_repo_rejects_existing_cache_with_different_origin() {
        let root = std::env::temp_dir().join(format!(
            "graft-repo-origin-drift-test-{}",
            std::process::id()
        ));
        let source_a = root.join("source-a");
        let source_b = root.join("source-b");
        let clone = root.join("clone");
        let _ = std::fs::remove_dir_all(&root);
        init_source_repo(&source_a, "a\n");
        init_source_repo(&source_b, "b\n");

        let backend = GixBackend;
        backend.ensure_repo(source_a.as_os_str(), &clone).unwrap();
        let error = backend
            .ensure_repo(source_b.as_os_str(), &clone)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_REPO_CACHE_URL_DRIFT]"), "{error}");
        assert!(error.contains("source-a"), "{error}");
        assert!(error.contains("source-b"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_origin_head_default_branch_after_clone() {
        let root = std::env::temp_dir().join(format!(
            "graft-repo-default-branch-test-{}",
            std::process::id()
        ));
        let source = root.join("source");
        let clone = root.join("clone");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&source).unwrap();
        git_output(&source, &["init", "-b", "trunk"], None).unwrap();
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
        backend.ensure_repo(source.as_os_str(), &clone).unwrap();

        assert_eq!(
            backend.remote_default_branch(&clone).unwrap().as_deref(),
            Some("trunk")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repo_tree_state_falls_back_to_local_branch_only_when_remote_ref_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "graft-repo-missing-remote-ref-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_output(&dir, &["init", "-b", "main"], None).unwrap();
        std::fs::write(dir.join("README.md"), b"demo\n").unwrap();
        git_output(&dir, &["add", "README.md"], None).unwrap();
        git_output(
            &dir,
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

        let state = GixBackend.repo_tree_state("demo", &dir, "main").unwrap();

        let StateId::RepoTree(repo) = state else {
            panic!("expected repo tree state");
        };
        assert_eq!(repo.treeish, "main");
        assert!(!repo.resolved_tree_oid.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_tree_state_does_not_fallback_when_remote_ref_is_invalid() {
        let dir = std::env::temp_dir().join(format!(
            "graft-repo-invalid-remote-ref-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_output(&dir, &["init", "-b", "main"], None).unwrap();
        std::fs::write(dir.join("README.md"), b"demo\n").unwrap();
        git_output(&dir, &["add", "README.md"], None).unwrap();
        git_output(
            &dir,
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
        let remote_ref = dir.join(".git/refs/remotes/origin/main");
        std::fs::create_dir_all(remote_ref.parent().unwrap()).unwrap();
        std::fs::write(&remote_ref, "notasha\n").unwrap();

        let error = GixBackend
            .repo_tree_state("demo", &dir, "main")
            .unwrap_err()
            .to_string();

        assert!(error.contains("git operation failed"), "{error}");
        assert!(error.contains("ignoring broken ref"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn init_source_repo(path: &Path, readme: &str) {
        std::fs::create_dir_all(path).unwrap();
        git_output(path, &["init", "-b", "main"], None).unwrap();
        std::fs::write(path.join("README.md"), readme).unwrap();
        git_output(path, &["add", "README.md"], None).unwrap();
        git_output(
            path,
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
