use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::manifest::{
    self, MANIFESTS_REF, ManifestRecord, PublicPartition, write_manifest_sidecar,
};
use crate::{Result, SyncError, SyncOptions, SyncReport, sync_public_store_local};

pub(super) fn sync_public_store_git_url(
    workspace_root: &Path,
    remote: &Path,
    options: SyncOptions,
) -> Result<SyncReport> {
    let remote_url = remote
        .as_os_str()
        .to_str()
        .ok_or_else(|| SyncError::InvalidRemote {
            path: remote.to_path_buf(),
            message: "URL remotes must be valid UTF-8".to_string(),
        })?;
    let temp = TempSyncRepo::new(workspace_root)?;
    git_output_at(
        temp.root(),
        &[
            OsString::from("init"),
            OsString::from("--bare"),
            temp.repo().as_os_str().to_os_string(),
        ],
        None,
        &[],
    )?;
    fetch_graft_refs(temp.repo(), remote_url)?;
    let remote_public = temp.repo().join("graft-public");
    materialize_remote_public_from_refs(temp.repo(), &remote_public)?;

    let report = sync_public_store_local(workspace_root, temp.repo(), remote, options)?;
    if report.manifest_id.is_some() {
        push_graft_refs(temp.repo(), remote_url)?;
    }
    Ok(report)
}

pub(super) fn is_url_like_remote(remote: &Path) -> bool {
    remote
        .as_os_str()
        .to_str()
        .is_some_and(|value| url_scheme_prefix(value).is_some() || is_scp_like_remote(value))
}

fn url_scheme_prefix(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once("://")?;
    if rest.is_empty() || !is_url_scheme(scheme) {
        return None;
    }
    Some(scheme)
}

fn is_url_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn is_scp_like_remote(value: &str) -> bool {
    if value.starts_with('/') || value.starts_with("./") || value.starts_with("../") {
        return false;
    }
    let Some((host, path)) = value.split_once(':') else {
        return false;
    };
    !host.is_empty()
        && !path.is_empty()
        && !host.contains('/')
        && (host.contains('@') || host.contains('.'))
}

struct TempSyncRepo {
    root: PathBuf,
    repo: PathBuf,
}

impl TempSyncRepo {
    fn new(workspace_root: &Path) -> Result<Self> {
        let base = std::env::temp_dir();
        let label = format!(
            "graft-sync-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        let root = base.join(label);
        fs::create_dir_all(&root)?;
        let repo = root.join("remote.git");
        let _ = workspace_root;
        Ok(Self { root, repo })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn repo(&self) -> &Path {
        &self.repo
    }
}

impl Drop for TempSyncRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fetch_graft_refs(repo: &Path, remote_url: &str) -> Result<()> {
    let ls_remote = git_output_at(
        repo.parent().unwrap_or_else(|| Path::new(".")),
        &[
            OsString::from("ls-remote"),
            OsString::from(remote_url),
            OsString::from("refs/graft/*"),
        ],
        None,
        &[],
    )?;
    if ls_remote.trim().is_empty() {
        return Ok(());
    }
    git_bare_output(
        repo,
        &[
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from(remote_url),
            OsString::from("+refs/graft/*:refs/graft/*"),
        ],
        None,
        &[],
    )?;
    Ok(())
}

fn push_graft_refs(repo: &Path, remote_url: &str) -> Result<()> {
    git_bare_output(
        repo,
        &[
            OsString::from("push"),
            OsString::from(remote_url),
            OsString::from("refs/graft/facts:refs/graft/facts"),
            OsString::from("refs/graft/blobs:refs/graft/blobs"),
            OsString::from("refs/graft/manifests:refs/graft/manifests"),
        ],
        None,
        &[],
    )?;
    Ok(())
}

fn materialize_remote_public_from_refs(repo: &Path, remote_public: &Path) -> Result<()> {
    if git_ref_exists(repo, PublicPartition::Facts.ref_name())? {
        materialize_treeish(repo, PublicPartition::Facts.ref_name(), remote_public)?;
    }
    if git_ref_exists(repo, PublicPartition::Blobs.ref_name())? {
        materialize_treeish(
            repo,
            PublicPartition::Blobs.ref_name(),
            &remote_public.join("blob"),
        )?;
    }
    if git_ref_exists(repo, MANIFESTS_REF)? {
        match git_object_type(repo, MANIFESTS_REF)?.as_str() {
            "blob" => materialize_legacy_manifest_blob(repo, remote_public)?,
            "commit" | "tree" => {
                materialize_treeish(repo, MANIFESTS_REF, &remote_public.join("manifest"))?
            }
            other => {
                return Err(SyncError::InvalidRemote {
                    path: repo.to_path_buf(),
                    message: format!(
                        "{MANIFESTS_REF} points to unsupported Git object type `{other}`"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn materialize_legacy_manifest_blob(repo: &Path, remote_public: &Path) -> Result<()> {
    let bytes = git_bare_output_bytes(
        repo,
        &[OsString::from("show"), OsString::from(MANIFESTS_REF)],
        None,
        &[],
    )?;
    let manifest: ManifestRecord =
        serde_json::from_slice(&bytes).map_err(|error| SyncError::InvalidManifest {
            path: remote_public.join("manifest").join("legacy-ref.json"),
            message: error.to_string(),
        })?;
    write_manifest_sidecar(remote_public, &manifest)
}

fn materialize_treeish(repo: &Path, treeish: &str, dest: &Path) -> Result<()> {
    let object_type = git_object_type(repo, treeish)?;
    let tree_spec = match object_type.as_str() {
        "commit" => format!("{treeish}^{{tree}}"),
        "tree" => treeish.to_string(),
        "blob" => {
            return Err(SyncError::InvalidRemote {
                path: repo.to_path_buf(),
                message: format!(
                    "{treeish} points to a legacy digest blob; URL sync requires tree or commit refs"
                ),
            });
        }
        other => {
            return Err(SyncError::InvalidRemote {
                path: repo.to_path_buf(),
                message: format!("{treeish} points to unsupported Git object type `{other}`"),
            });
        }
    };
    fs::create_dir_all(dest)?;
    let names = git_bare_output_bytes(
        repo,
        &[
            OsString::from("ls-tree"),
            OsString::from("-rz"),
            OsString::from("-r"),
            OsString::from("--name-only"),
            OsString::from(&tree_spec),
        ],
        None,
        &[],
    )?;
    for raw in names.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        let relative = std::str::from_utf8(raw).map_err(|error| SyncError::InvalidStorePath {
            path: dest.to_path_buf(),
            message: format!("Git tree path is not valid UTF-8: {error}"),
        })?;
        validate_relative_git_path(dest, relative)?;
        let spec = format!("{tree_spec}:{relative}");
        let bytes = git_bare_output_bytes(
            repo,
            &[OsString::from("show"), OsString::from(spec)],
            None,
            &[],
        )?;
        let path = dest.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(())
}

pub(crate) fn write_tree_commit_ref<F>(
    repo: &gix::Repository,
    root: &Path,
    include: F,
    ref_name: &str,
    message: &str,
) -> Result<String>
where
    F: Copy + Fn(&Path) -> bool,
{
    let git_dir = repo.git_dir();
    let index_path = git_dir.join(format!(
        "graft-sync-index-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let index_env = [("GIT_INDEX_FILE", index_path.as_os_str())];
    git_bare_output(
        git_dir,
        &[OsString::from("read-tree"), OsString::from("--empty")],
        None,
        &index_env,
    )?;
    add_tree_files_to_index(git_dir, root, root, include, &index_env)?;
    let tree = git_bare_output(git_dir, &[OsString::from("write-tree")], None, &index_env)?
        .trim()
        .to_string();
    let mut commit_args = vec![OsString::from("commit-tree"), OsString::from(&tree)];
    if git_ref_exists(git_dir, ref_name)? && git_object_type(git_dir, ref_name)? == "commit" {
        let parent = git_bare_output(
            git_dir,
            &[OsString::from("rev-parse"), OsString::from(ref_name)],
            None,
            &[],
        )?
        .trim()
        .to_string();
        commit_args.push(OsString::from("-p"));
        commit_args.push(OsString::from(parent));
    }
    commit_args.push(OsString::from("-m"));
    commit_args.push(OsString::from(message));
    let commit = git_bare_output(git_dir, &commit_args, None, &[])?
        .trim()
        .to_string();
    git_bare_output(
        git_dir,
        &[
            OsString::from("update-ref"),
            OsString::from(ref_name),
            OsString::from(&commit),
        ],
        None,
        &[],
    )?;
    let _ = fs::remove_file(index_path);
    Ok(commit)
}

fn add_tree_files_to_index<F>(
    repo: &Path,
    root: &Path,
    path: &Path,
    include: F,
    envs: &[(&str, &OsStr)],
) -> Result<()>
where
    F: Copy + Fn(&Path) -> bool,
{
    if !path.exists() {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SyncError::InvalidStorePath {
                path: path.clone(),
                message: format!("path is not under tree root {}", root.display()),
            })?;
        if !include(relative) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            add_tree_files_to_index(repo, root, &path, include, envs)?;
        } else if entry.file_type()?.is_file() {
            let relative = manifest::digest_relative_path(root, &path)?;
            let bytes = fs::read(&path)?;
            let blob = git_bare_output(
                repo,
                &[
                    OsString::from("hash-object"),
                    OsString::from("-w"),
                    OsString::from("--stdin"),
                ],
                Some(&bytes),
                &[],
            )?
            .trim()
            .to_string();
            git_bare_output(
                repo,
                &[
                    OsString::from("update-index"),
                    OsString::from("--add"),
                    OsString::from("--cacheinfo"),
                    OsString::from("100644"),
                    OsString::from(blob),
                    OsString::from(relative),
                ],
                None,
                envs,
            )?;
        }
    }
    Ok(())
}

fn git_ref_exists(repo: &Path, ref_name: &str) -> Result<bool> {
    git_bare_status(
        repo,
        &[
            OsString::from("show-ref"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from(ref_name),
        ],
        &[],
    )
}

fn git_object_type(repo: &Path, spec: &str) -> Result<String> {
    Ok(git_bare_output(
        repo,
        &[
            OsString::from("cat-file"),
            OsString::from("-t"),
            OsString::from(spec),
        ],
        None,
        &[],
    )?
    .trim()
    .to_string())
}

fn validate_relative_git_path(root: &Path, relative: &str) -> Result<()> {
    let mut saw_component = false;
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(value) => {
                saw_component = true;
                if value.to_str().is_none() {
                    return Err(SyncError::InvalidStorePath {
                        path: root.join(relative),
                        message: "Git tree path is not valid UTF-8".to_string(),
                    });
                }
            }
            Component::CurDir => {}
            other => {
                return Err(SyncError::InvalidStorePath {
                    path: root.join(relative),
                    message: format!(
                        "unexpected Git tree path component {}",
                        other.as_os_str().to_string_lossy()
                    ),
                });
            }
        }
    }
    if !saw_component {
        return Err(SyncError::InvalidStorePath {
            path: root.to_path_buf(),
            message: "empty Git tree path".to_string(),
        });
    }
    Ok(())
}

fn git_bare_output(
    repo: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<String> {
    String::from_utf8(git_bare_output_bytes(repo, args, input, envs)?)
        .map_err(|error| SyncError::Git(error.to_string()))
}

fn git_bare_output_bytes(
    repo: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<Vec<u8>> {
    let mut git_args = Vec::with_capacity(args.len() + 2);
    git_args.push(OsString::from("--git-dir"));
    git_args.push(repo.as_os_str().to_os_string());
    git_args.extend(args.iter().cloned());
    git_output_bytes_at(
        repo.parent().unwrap_or_else(|| Path::new(".")),
        &git_args,
        input,
        envs,
    )
}

fn git_bare_status(repo: &Path, args: &[OsString], envs: &[(&str, &OsStr)]) -> Result<bool> {
    let mut git_args = Vec::with_capacity(args.len() + 2);
    git_args.push(OsString::from("--git-dir"));
    git_args.push(repo.as_os_str().to_os_string());
    git_args.extend(args.iter().cloned());
    git_status_at(
        repo.parent().unwrap_or_else(|| Path::new(".")),
        &git_args,
        envs,
    )
}

fn git_output_at(
    current_dir: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<String> {
    String::from_utf8(git_output_bytes_at(current_dir, args, input, envs)?)
        .map_err(|error| SyncError::Git(error.to_string()))
}

fn git_output_bytes_at(
    current_dir: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    envs: &[(&str, &OsStr)],
) -> Result<Vec<u8>> {
    let mut command = git_command(current_dir, args, envs);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        let Some(mut stdin) = child.stdin.take() else {
            return Err(SyncError::Git("failed to open git stdin".to_string()));
        };
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(SyncError::Git(format!(
            "git {} failed: {}",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn git_status_at(current_dir: &Path, args: &[OsString], envs: &[(&str, &OsStr)]) -> Result<bool> {
    let status = git_command(current_dir, args, envs).status()?;
    Ok(status.success())
}

fn git_command(current_dir: &Path, args: &[OsString], envs: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(current_dir)
        .env("GIT_AUTHOR_NAME", "Graft")
        .env("GIT_AUTHOR_EMAIL", "graft@example.invalid")
        .env("GIT_COMMITTER_NAME", "Graft")
        .env("GIT_COMMITTER_EMAIL", "graft@example.invalid")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in args {
        command.arg(arg);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}
