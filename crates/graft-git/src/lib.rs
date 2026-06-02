use std::path::Path;

use graft_core::{StateId, TreeSnapshot};

pub use graft_promote::{MaterializedCommit, PullRequest};
pub use graft_repo::EnsuredRepo;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Repo(#[from] graft_repo::GitError),
    #[error(transparent)]
    Promote(#[from] graft_promote::GitError),
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Clone, Debug, Default)]
pub struct GixBackend;

impl GixBackend {
    pub fn ensure_repo(&self, url: &str, path: impl AsRef<Path>) -> Result<EnsuredRepo> {
        Ok(graft_repo::GixBackend.ensure_repo(url, path)?)
    }

    pub fn sync_repo(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        Ok(graft_repo::GixBackend.sync_repo(repo_path)?)
    }

    pub fn repo_tree_state(
        &self,
        repo_id: &str,
        repo_path: impl AsRef<Path>,
        treeish: &str,
    ) -> Result<StateId> {
        Ok(graft_repo::GixBackend.repo_tree_state(repo_id, repo_path, treeish)?)
    }

    pub fn head_tree_state(&self, path: impl AsRef<Path>) -> Result<StateId> {
        Ok(graft_repo::GixBackend.head_tree_state(path)?)
    }

    pub fn tree_state(&self, path: impl AsRef<Path>, treeish: &str) -> Result<StateId> {
        Ok(graft_repo::GixBackend.tree_state(path, treeish)?)
    }

    pub fn tree_snapshot(
        &self,
        repo_path: impl AsRef<Path>,
        treeish: &str,
        blob_root: Option<impl AsRef<Path>>,
    ) -> Result<TreeSnapshot> {
        Ok(graft_repo::GixBackend.tree_snapshot(repo_path, treeish, blob_root)?)
    }

    pub fn materialize_commit(
        &self,
        repo_path: impl AsRef<Path>,
        snapshot: &TreeSnapshot,
        blob_root: impl AsRef<Path>,
        message: &str,
        ref_name: Option<&str>,
    ) -> Result<MaterializedCommit> {
        Ok(graft_promote::GixBackend
            .materialize_commit(repo_path, snapshot, blob_root, message, ref_name)?)
    }

    pub fn write_tree(
        &self,
        repo_path: impl AsRef<Path>,
        snapshot: &TreeSnapshot,
        blob_root: impl AsRef<Path>,
    ) -> Result<String> {
        Ok(graft_promote::GixBackend.write_tree(repo_path, snapshot, blob_root)?)
    }

    pub fn update_ref(
        &self,
        repo_path: impl AsRef<Path>,
        ref_name: &str,
        target: &str,
    ) -> Result<()> {
        Ok(graft_promote::GixBackend.update_ref(repo_path, ref_name, target)?)
    }

    pub fn resolve_ref(&self, repo_path: impl AsRef<Path>, ref_name: &str) -> Result<String> {
        Ok(graft_promote::GixBackend.resolve_ref(repo_path, ref_name)?)
    }

    pub fn promote_branch(
        &self,
        repo_path: impl AsRef<Path>,
        branch: &str,
        commit_id: &str,
    ) -> Result<String> {
        Ok(graft_promote::GixBackend.promote_branch(repo_path, branch, commit_id)?)
    }

    pub fn promote_release(
        &self,
        repo_path: impl AsRef<Path>,
        tag: &str,
        commit_id: &str,
    ) -> Result<String> {
        Ok(graft_promote::GixBackend.promote_release(repo_path, tag, commit_id)?)
    }

    pub fn create_pull_request(
        &self,
        repo_path: impl AsRef<Path>,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        Ok(graft_promote::GixBackend.create_pull_request(repo_path, head, base, title, body)?)
    }
}
