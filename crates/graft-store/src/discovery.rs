//! Workspace discovery for the P8 user-level workspace model.
//!
//! Resolution order:
//!
//! 1. `$GRAFT_WORKSPACE` env (workspace id in registry, or explicit root)
//! 2. cwd parent chain containing `graft.toml` + `.graft/`
//! 3. `$GRAFT_HOME/registry.toml [[routes]]`
//! 4. fail loud; use `graft attach` to route an unattached cwd

use std::path::{Path, PathBuf};

use crate::{
    RegistryStore, Result, StoreError, WorkspaceKind, graft_home_from_env, normalize_workspace_path,
};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

pub const DEFAULT_WORKSPACE_ID: &str = "ws:default";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceLocation {
    Local {
        id: Option<String>,
        root: PathBuf,
        source: WorkspaceSource,
    },
    System {
        id: String,
        root: PathBuf,
        source: WorkspaceSource,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSource {
    Env,
    Parent,
    Route,
}

#[derive(Clone, Debug)]
pub struct WorkspaceDiscovery {
    registry: RegistryStore,
}

impl WorkspaceLocation {
    pub fn root(&self) -> &Path {
        match self {
            Self::Local { root, .. } | Self::System { root, .. } => root,
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Local { id, .. } => id.as_deref(),
            Self::System { id, .. } => Some(id.as_str()),
        }
    }

    pub fn source(&self) -> WorkspaceSource {
        match self {
            Self::Local { source, .. } | Self::System { source, .. } => *source,
        }
    }

    pub fn is_system(&self) -> bool {
        matches!(self, Self::System { .. })
    }
}

impl WorkspaceDiscovery {
    pub fn from_env() -> Self {
        Self::new(RegistryStore::from_env())
    }

    pub fn new(registry: RegistryStore) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &RegistryStore {
        &self.registry
    }

    pub fn discover(&self, cwd: impl AsRef<Path>) -> Result<WorkspaceLocation> {
        let cwd = normalize_workspace_path(cwd.as_ref());
        if let Some(location) = self.discover_from_env()? {
            return Ok(location);
        }
        if let Some(root) = find_parent_workspace(&cwd) {
            self.registry.ensure_workspace(
                local_workspace_id_for_root(&root),
                WorkspaceKind::Local,
                &root,
            )?;
            return Ok(WorkspaceLocation::Local {
                id: Some(local_workspace_id_for_root(&root)),
                root,
                source: WorkspaceSource::Parent,
            });
        }
        if let Some(route) = self.registry.lookup_route_for_cwd(&cwd)? {
            let Some(workspace) = self.registry.get_workspace(&route.workspace)? else {
                return Err(StoreError::InvalidWorkspace(format!(
                    "route for {} points to unknown workspace {}",
                    route.cwd.display(),
                    route.workspace
                )));
            };
            return Ok(location_from_registry(
                workspace.id,
                workspace.kind,
                workspace.root,
                WorkspaceSource::Route,
            ));
        }
        Err(StoreError::NoWorkspace { cwd })
    }

    fn discover_from_env(&self) -> Result<Option<WorkspaceLocation>> {
        let Some(value) = std::env::var_os("GRAFT_WORKSPACE") else {
            return Ok(None);
        };
        let value = PathBuf::from(value);
        if let Some(id) = value.to_str()
            && let Some(workspace) = self.registry.get_workspace(id)?
        {
            return Ok(Some(location_from_registry(
                workspace.id,
                workspace.kind,
                workspace.root,
                WorkspaceSource::Env,
            )));
        }
        let root = normalize_workspace_path(&value);
        if is_workspace_root(&root) {
            let id = self
                .registry
                .list_workspaces()?
                .into_iter()
                .find(|workspace| workspace.root == root)
                .map(|workspace| workspace.id)
                .unwrap_or_else(|| local_workspace_id_for_root(&root));
            self.registry
                .ensure_workspace(id.clone(), WorkspaceKind::Local, &root)?;
            return Ok(Some(WorkspaceLocation::Local {
                id: Some(id),
                root,
                source: WorkspaceSource::Env,
            }));
        }
        Err(StoreError::InvalidWorkspace(env_workspace_error(&value)))
    }
}

pub fn default_workspace_root() -> PathBuf {
    default_workspace_root_for_home(&graft_home_from_env())
}

fn default_workspace_root_for_home(home: &Path) -> PathBuf {
    home.join("workspaces").join("default")
}

fn location_from_registry(
    id: String,
    kind: WorkspaceKind,
    root: PathBuf,
    source: WorkspaceSource,
) -> WorkspaceLocation {
    match kind {
        WorkspaceKind::System => WorkspaceLocation::System { id, root, source },
        WorkspaceKind::Local => WorkspaceLocation::Local {
            id: Some(id),
            root,
            source,
        },
    }
}

fn find_parent_workspace(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        if is_workspace_root(ancestor) {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn is_workspace_root(path: &Path) -> bool {
    path.join("graft.toml").is_file() && path.join(".graft").is_dir()
}

fn env_workspace_error(value: &Path) -> String {
    match value.to_str() {
        Some(id) => {
            format!(
                "GRAFT_WORKSPACE={id} is neither a registered workspace id nor a workspace root"
            )
        }
        None => format!(
            "GRAFT_WORKSPACE={} is not a workspace root; non-UTF-8 values cannot name registered workspace ids",
            value.display()
        ),
    }
}

pub fn local_workspace_id_for_root(root: &Path) -> String {
    let digest = blake3::hash(&path_identity_bytes(root));
    format!("ws:local-{}", &digest.to_hex()[..12])
}

#[cfg(unix)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = b"graft-local-workspace-root-v1\0unix\0".to_vec();
    bytes.extend_from_slice(path.as_os_str().as_bytes());
    bytes
}

#[cfg(windows)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = b"graft-local-workspace-root-v1\0windows\0".to_vec();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraftStore, RegistryStore};
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: these tests run quickly and do not spawn threads that
            // concurrently inspect these variables.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: these tests run quickly and do not spawn threads that
            // concurrently inspect these variables.
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: test-only environment restoration.
            unsafe {
                if let Some(old) = &self.old {
                    std::env::set_var(self.key, old);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn temp_home(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "graft-discovery-{label}-{}-{}",
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
    fn env_workspace_id_has_highest_priority() {
        let _lock = env_lock();
        let home = temp_home("env-id");
        let registry = RegistryStore::new(&home);
        let root = home.join("env-root");
        GraftStore::open(&root).init().unwrap();
        registry
            .ensure_workspace("ws:env", WorkspaceKind::System, &root)
            .unwrap();
        let _guard = EnvGuard::set("GRAFT_WORKSPACE", "ws:env");
        let discovery = WorkspaceDiscovery::new(registry);
        let location = discovery.discover(home.join("elsewhere")).unwrap();
        assert_eq!(location.id(), Some("ws:env"));
        assert_eq!(location.source(), WorkspaceSource::Env);
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn env_workspace_root_registers_and_returns_workspace_id() {
        let _lock = env_lock();
        let home = temp_home("env-root");
        let registry = RegistryStore::new(&home);
        let root = home.join("explicit-root");
        GraftStore::open(&root).init().unwrap();
        let expected_id = local_workspace_id_for_root(&root.canonicalize().unwrap());
        let _guard = EnvGuard::set("GRAFT_WORKSPACE", &root);
        let discovery = WorkspaceDiscovery::new(registry.clone());

        let location = discovery.discover(home.join("elsewhere")).unwrap();

        assert_eq!(location.id(), Some(expected_id.as_str()));
        assert_eq!(location.source(), WorkspaceSource::Env);
        assert_eq!(
            registry.get_workspace(&expected_id).unwrap().unwrap().root,
            root.canonicalize().unwrap()
        );
        fs::remove_dir_all(home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_env_workspace_value_is_not_lossy_matched_as_registry_id() {
        let _lock = env_lock();
        let home = temp_home("env-non-utf8-id");
        let registry = RegistryStore::new(&home);
        let root = home.join("registered-root");
        GraftStore::open(&root).init().unwrap();
        registry
            .ensure_workspace("ws:\u{fffd}", WorkspaceKind::System, &root)
            .unwrap();
        let _guard = EnvGuard::set("GRAFT_WORKSPACE", OsString::from_vec(b"ws:\xFF".to_vec()));
        let discovery = WorkspaceDiscovery::new(registry);

        let error = discovery.discover(home.join("elsewhere")).unwrap_err();

        assert!(
            matches!(error, StoreError::InvalidWorkspace(_)),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("non-UTF-8 values cannot name registered workspace ids"),
            "{message}"
        );
        fs::remove_dir_all(home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn local_workspace_id_uses_raw_path_bytes_not_lossy_display() {
        let left = PathBuf::from(OsString::from_vec(b"/tmp/graft-workspace-\xFF".to_vec()));
        let right = PathBuf::from(OsString::from_vec(b"/tmp/graft-workspace-\xFE".to_vec()));
        assert_eq!(left.to_string_lossy(), right.to_string_lossy());

        assert_ne!(
            local_workspace_id_for_root(&left),
            local_workspace_id_for_root(&right)
        );
    }

    #[test]
    fn parent_workspace_beats_registry_route() {
        let _lock = env_lock();
        let home = temp_home("parent");
        let _guard = EnvGuard::remove("GRAFT_WORKSPACE");
        let registry = RegistryStore::new(&home);
        let parent = home.join("workspace");
        let child = parent.join("child");
        GraftStore::open(&parent).init().unwrap();
        fs::create_dir_all(&child).unwrap();
        registry
            .ensure_workspace("ws:route", WorkspaceKind::System, home.join("route-root"))
            .unwrap();
        registry.upsert_route(&home, "ws:route").unwrap();
        let discovery = WorkspaceDiscovery::new(registry);
        let location = discovery.discover(&child).unwrap();
        assert_eq!(location.root(), parent.canonicalize().unwrap());
        assert_eq!(location.source(), WorkspaceSource::Parent);
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn registry_route_resolves_to_registered_workspace() {
        let _lock = env_lock();
        let home = temp_home("route");
        let _guard = EnvGuard::remove("GRAFT_WORKSPACE");
        let registry = RegistryStore::new(&home);
        let root = home.join("root");
        let cwd = home.join("checkout");
        GraftStore::open(&root).init().unwrap();
        fs::create_dir_all(&cwd).unwrap();
        registry
            .ensure_workspace("ws:routed", WorkspaceKind::System, &root)
            .unwrap();
        registry.upsert_route(&cwd, "ws:routed").unwrap();
        let discovery = WorkspaceDiscovery::new(registry);
        let location = discovery.discover(cwd.join("nested")).unwrap();
        assert_eq!(location.id(), Some("ws:routed"));
        assert_eq!(location.source(), WorkspaceSource::Route);
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn unattached_cwd_requires_explicit_workspace_route() {
        let _lock = env_lock();
        let home = temp_home("unattached");
        let _guard = EnvGuard::remove("GRAFT_WORKSPACE");
        let registry = RegistryStore::new(&home);
        let cwd = home.join("unattached");
        fs::create_dir_all(&cwd).unwrap();
        let discovery = WorkspaceDiscovery::new(registry.clone());
        let error = discovery.discover(&cwd).unwrap_err();
        assert!(matches!(error, StoreError::NoWorkspace { .. }));
        assert!(registry.lookup_workspace_for_cwd(&cwd).unwrap().is_none());
        assert!(!default_workspace_root_for_home(&home).exists());
        fs::remove_dir_all(home).ok();
    }
}
