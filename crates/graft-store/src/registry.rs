//! Machine-local `$GRAFT_HOME/registry.toml` support.
//!
//! The registry is not a workspace file and is never represented as a patch.
//! It is the machine-local index for:
//!
//! - known workspaces (`[[workspaces]]`)
//! - cwd routes (`[[routes]]`)
//! - local clone paths by RepoId (`[[repo_paths]]`)
//!
//! Writes are protected by an OS file lock on `$GRAFT_HOME/.registry.lock`,
//! keep a `.bak` of the last good registry for diagnostics, and publish with
//! atomic rename. Reads fail loud when the primary registry is corrupt; callers
//! must not silently route commands through stale backup data.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{Result, StoreError, normalize_workspace_path};

const REGISTRY_SCHEMA: u32 = 1;

#[derive(Clone, Debug)]
pub struct RegistryStore {
    home: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceRecord>,
    #[serde(default)]
    pub routes: Vec<RouteRecord>,
    #[serde(default)]
    pub repo_paths: Vec<RepoPathsRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRecord {
    pub id: String,
    pub kind: WorkspaceKind,
    pub root: PathBuf,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceKind {
    System,
    Local,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRecord {
    pub cwd: PathBuf,
    pub workspace: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoPathsRecord {
    pub repo_id: String,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    pub last_seen_at: String,
}

#[derive(Debug)]
struct RegistryLock {
    path: PathBuf,
    file: File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExistingRegistryPolicy {
    RequireReadable,
    PreserveCorrupt,
}

impl RegistryStore {
    pub fn from_env() -> Self {
        Self::new(graft_home_from_env())
    }

    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            home: home.as_ref().to_path_buf(),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn registry_path(&self) -> PathBuf {
        self.home.join("registry.toml")
    }

    pub fn backup_path(&self) -> PathBuf {
        self.home.join("registry.toml.bak")
    }

    pub fn corrupt_path(&self) -> PathBuf {
        self.home.join("registry.toml.corrupt")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.home.join(".registry.lock")
    }

    pub fn load(&self) -> Result<Registry> {
        read_registry_or_default(&self.registry_path())
    }

    pub fn save(&self, registry: &Registry) -> Result<()> {
        let _lock = self.lock()?;
        self.save_locked(registry, ExistingRegistryPolicy::RequireReadable)
    }

    pub fn replace(&self, registry: &Registry) -> Result<()> {
        let _lock = self.lock()?;
        self.save_locked(registry, ExistingRegistryPolicy::PreserveCorrupt)
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let _lock = self.lock()?;
        let mut registry = read_registry_or_default(&self.registry_path())?;
        let output = f(&mut registry)?;
        self.save_locked(&registry, ExistingRegistryPolicy::RequireReadable)?;
        Ok(output)
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        Ok(self.load()?.workspaces)
    }

    pub fn get_workspace(&self, id: &str) -> Result<Option<WorkspaceRecord>> {
        Ok(self
            .load()?
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == id))
    }

    pub fn ensure_workspace(
        &self,
        id: impl Into<String>,
        kind: WorkspaceKind,
        root: impl AsRef<Path>,
    ) -> Result<WorkspaceRecord> {
        let id = id.into();
        let root = normalize_workspace_path(root.as_ref());
        self.with_mut(|registry| {
            if let Some(existing) = registry
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
            {
                existing.kind = kind;
                existing.root = root.clone();
                return Ok(existing.clone());
            }
            let record = WorkspaceRecord {
                id,
                kind,
                root,
                created_at: now_rfc3339()?,
            };
            registry.workspaces.push(record.clone());
            registry.workspaces.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(record)
        })
    }

    pub fn upsert_route(
        &self,
        cwd: impl AsRef<Path>,
        workspace: impl Into<String>,
    ) -> Result<RouteRecord> {
        let cwd = normalize_workspace_path(cwd.as_ref());
        let workspace = workspace.into();
        self.with_mut(|registry| {
            if let Some(existing) = registry.routes.iter_mut().find(|route| route.cwd == cwd) {
                existing.workspace = workspace.clone();
                return Ok(existing.clone());
            }
            let record = RouteRecord {
                cwd,
                workspace,
                created_at: now_rfc3339()?,
            };
            registry.routes.push(record.clone());
            registry.routes.sort_by(|a, b| a.cwd.cmp(&b.cwd));
            Ok(record)
        })
    }

    pub fn remove_route(&self, cwd: impl AsRef<Path>) -> Result<bool> {
        let cwd = normalize_workspace_path(cwd.as_ref());
        self.with_mut(|registry| {
            let before = registry.routes.len();
            registry.routes.retain(|route| route.cwd != cwd);
            Ok(registry.routes.len() != before)
        })
    }

    /// Return the longest route whose cwd is an ancestor of `cwd` (or exactly
    /// equal to it).
    pub fn lookup_route_for_cwd(&self, cwd: impl AsRef<Path>) -> Result<Option<RouteRecord>> {
        let cwd = normalize_workspace_path(cwd.as_ref());
        let registry = self.load()?;
        Ok(registry
            .routes
            .iter()
            .filter(|route| cwd == route.cwd || cwd.starts_with(&route.cwd))
            .max_by_key(|route| route.cwd.components().count())
            .cloned())
    }

    /// Return the workspace id for the longest matching cwd route.
    pub fn lookup_workspace_for_cwd(&self, cwd: impl AsRef<Path>) -> Result<Option<String>> {
        Ok(self.lookup_route_for_cwd(cwd)?.map(|route| route.workspace))
    }

    pub fn upsert_repo_path(
        &self,
        repo_id: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<RepoPathsRecord> {
        let repo_id = repo_id.into();
        let path = normalize_workspace_path(path.as_ref());
        self.with_mut(|registry| {
            if let Some(existing) = registry
                .repo_paths
                .iter_mut()
                .find(|record| record.repo_id == repo_id)
            {
                if !existing.paths.iter().any(|existing| existing == &path) {
                    existing.paths.push(path);
                    existing.paths.sort();
                }
                existing.last_seen_at = now_rfc3339()?;
                return Ok(existing.clone());
            }
            let record = RepoPathsRecord {
                repo_id,
                paths: vec![path],
                last_seen_at: now_rfc3339()?,
            };
            registry.repo_paths.push(record.clone());
            registry
                .repo_paths
                .sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
            Ok(record)
        })
    }

    pub fn lookup_paths_for_repo(&self, repo_id: &str) -> Result<Vec<PathBuf>> {
        Ok(self
            .load()?
            .repo_paths
            .into_iter()
            .find(|record| record.repo_id == repo_id)
            .map(|record| record.paths)
            .unwrap_or_default())
    }

    fn lock(&self) -> Result<RegistryLock> {
        RegistryLock::acquire(&self.home)
    }

    fn save_locked(&self, registry: &Registry, policy: ExistingRegistryPolicy) -> Result<()> {
        if let Some(parent) = self.registry_path().parent() {
            fs::create_dir_all(parent)?;
        }
        let registry_path = self.registry_path();
        let backup_path = self.backup_path();
        if registry_path.exists() {
            match read_registry_file(&registry_path) {
                Ok(_) => {
                    fs::copy(&registry_path, &backup_path)?;
                }
                Err(_error) if policy == ExistingRegistryPolicy::PreserveCorrupt => {
                    fs::copy(&registry_path, self.corrupt_path())?;
                }
                Err(error) => return Err(error),
            }
        }

        let mut normalized = registry.clone();
        normalized.schema = REGISTRY_SCHEMA;
        normalized.workspaces.sort_by(|a, b| a.id.cmp(&b.id));
        normalized.routes.sort_by(|a, b| a.cwd.cmp(&b.cwd));
        normalized
            .repo_paths
            .sort_by(|a, b| a.repo_id.cmp(&b.repo_id));

        let body = toml::to_string_pretty(&normalized)?;
        let tmp_path = self.home.join("registry.toml.tmp");
        {
            let mut tmp = File::create(&tmp_path)?;
            tmp.write_all(body.as_bytes())?;
            tmp.sync_all()?;
        }
        fs::rename(tmp_path, registry_path)?;
        Ok(())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            workspaces: Vec::new(),
            routes: Vec::new(),
            repo_paths: Vec::new(),
        }
    }
}

impl RegistryLock {
    fn acquire(home: &Path) -> Result<Self> {
        fs::create_dir_all(home)?;
        let path = home.join(".registry.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if let Err(err) = file.try_lock_exclusive() {
            return Err(if err.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::Locked { path }
            } else {
                StoreError::Io(err)
            });
        }
        Ok(Self { path, file })
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        // Touch the path so the field is considered read outside Debug; this
        // also makes debugging dropped locks easier under breakpoints.
        let _ = &self.path;
        // Best-effort unlock; on success the OS also releases on close/process exit.
        let _ = self.file.unlock();
    }
}

fn read_registry_or_default(registry_path: &Path) -> Result<Registry> {
    match read_registry_file(registry_path) {
        Ok(registry) => Ok(registry),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Registry::default())
        }
        Err(error) => Err(error),
    }
}

fn read_registry_file(path: &Path) -> Result<Registry> {
    let body = fs::read_to_string(path)?;
    let registry: Registry = toml::from_str(&body)?;
    if registry.schema != REGISTRY_SCHEMA {
        return Err(StoreError::InvalidRegistrySchema {
            found: registry.schema,
        });
    }
    Ok(registry)
}

pub fn graft_home_from_env() -> PathBuf {
    if let Some(home) = std::env::var_os("GRAFT_HOME") {
        return PathBuf::from(home);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(".graft");
    };
    PathBuf::from(home).join(".graft")
}

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn default_schema() -> u32 {
    REGISTRY_SCHEMA
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "graft-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn registry_crud_round_trips_three_tables() {
        let home = temp_home("crud");
        let store = RegistryStore::new(&home);

        let workspace = store
            .ensure_workspace("ws:default", WorkspaceKind::System, home.join("default"))
            .unwrap();
        assert_eq!(workspace.id, "ws:default");

        let cwd = home.join("checkout");
        fs::create_dir_all(&cwd).unwrap();
        let route = store.upsert_route(&cwd, "ws:default").unwrap();
        assert_eq!(route.workspace, "ws:default");
        assert_eq!(
            store.lookup_workspace_for_cwd(cwd.join("nested")).unwrap(),
            Some("ws:default".to_string())
        );

        let repo_path = store.upsert_repo_path("repo:abc", &cwd).unwrap();
        assert_eq!(repo_path.paths, vec![cwd.canonicalize().unwrap()]);
        assert_eq!(
            store.lookup_paths_for_repo("repo:abc").unwrap(),
            vec![cwd.canonicalize().unwrap()]
        );

        let loaded = store.load().unwrap();
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(loaded.routes.len(), 1);
        assert_eq!(loaded.repo_paths.len(), 1);
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn route_lookup_prefers_longest_prefix() {
        let home = temp_home("longest");
        let store = RegistryStore::new(&home);
        let root = home.join("root");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        store.upsert_route(&root, "ws:root").unwrap();
        store.upsert_route(&nested, "ws:nested").unwrap();
        assert_eq!(
            store
                .lookup_workspace_for_cwd(nested.join("child"))
                .unwrap(),
            Some("ws:nested".to_string())
        );
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn corrupt_primary_registry_fails_loud_even_when_backup_exists() {
        let home = temp_home("backup");
        let store = RegistryStore::new(&home);
        store
            .ensure_workspace("ws:default", WorkspaceKind::System, home.join("default"))
            .unwrap();
        store
            .ensure_workspace("ws:other", WorkspaceKind::System, home.join("other"))
            .unwrap();
        fs::write(store.registry_path(), "not = [valid").unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains("toml deserialize error"), "{error}");
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn save_refuses_to_overwrite_corrupt_primary_registry() {
        let home = temp_home("corrupt-save");
        let store = RegistryStore::new(&home);
        store
            .ensure_workspace("ws:default", WorkspaceKind::System, home.join("default"))
            .unwrap();
        fs::write(store.registry_path(), "not = [valid").unwrap();

        let error = store.save(&Registry::default()).unwrap_err().to_string();

        assert!(error.contains("toml deserialize error"), "{error}");
        assert_eq!(
            fs::read_to_string(store.registry_path()).unwrap(),
            "not = [valid"
        );
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn replace_preserves_corrupt_primary_registry_for_rebuilds() {
        let home = temp_home("corrupt-replace");
        let store = RegistryStore::new(&home);
        store
            .ensure_workspace("ws:default", WorkspaceKind::System, home.join("default"))
            .unwrap();
        fs::write(store.registry_path(), "not = [valid").unwrap();

        store.replace(&Registry::default()).unwrap();

        assert_eq!(
            fs::read_to_string(store.corrupt_path()).unwrap(),
            "not = [valid"
        );
        let registry = store.load().unwrap();
        assert!(registry.workspaces.is_empty());
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn registry_rejects_unknown_fields_and_schema_versions() {
        let home = temp_home("strict-schema");
        let store = RegistryStore::new(&home);
        fs::write(
            store.registry_path(),
            r#"
schema = 1
surprise = "ignored before"
"#,
        )
        .unwrap();

        let unknown = store.load().unwrap_err().to_string();
        assert!(unknown.contains("unknown field"), "{unknown}");

        fs::write(store.registry_path(), "schema = 2\n").unwrap();
        let schema = store.load().unwrap_err().to_string();
        assert!(schema.contains("[E_REGISTRY_SCHEMA]"), "{schema}");
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn registry_lock_is_exclusive() {
        let home = temp_home("lock");
        let first = RegistryLock::acquire(&home).unwrap();
        let second = RegistryLock::acquire(&home);
        assert!(matches!(second, Err(StoreError::Locked { .. })));
        drop(first);
        let _third = RegistryLock::acquire(&home).unwrap();
        fs::remove_dir_all(home).ok();
    }
}
