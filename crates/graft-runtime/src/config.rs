use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use graft_core::{Evaluator, Judge, PropertyDef, Query};
use graft_store::GraftStore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GraftConfig {
    #[serde(default)]
    pub(crate) create: CreateConfig,
    #[serde(default)]
    pub(crate) admission: AdmissionConfig,
    #[serde(default)]
    pub(crate) promotion: PromotionConfig,
    #[serde(default)]
    pub(crate) promote_targets: BTreeMap<String, PromoteTargetConfig>,
    #[serde(default)]
    pub(crate) repos_root: Option<PathBuf>,
    #[serde(default)]
    pub(crate) repos: BTreeMap<String, RepoConfig>,
    #[serde(skip)]
    pub(crate) properties: BTreeMap<String, PropertyDef>,
}

impl GraftConfig {
    pub(crate) fn validate_repos(&self) -> Result<()> {
        if let Some(root) = &self.repos_root {
            validate_config_path("repos_root", root)?;
        }
        for (target_id, target) in &self.promote_targets {
            validate_repo_id(target_id)?;
            validate_config_path(&format!("promote_targets.{target_id}.path"), &target.path)?;
            if target.branch.as_deref().is_some_and(str::is_empty) {
                bail!("promote target {target_id} branch must not be empty");
            }
        }
        for (repo_id, repo) in &self.repos {
            validate_repo_id(repo_id)?;
            if repo.url.trim().is_empty() {
                bail!("repo {repo_id} must set a non-empty url");
            }
            if let Some(path) = &repo.path {
                validate_config_path(&format!("repos.{repo_id}.path"), path)?;
            }
        }
        Ok(())
    }

    pub(crate) fn repos_root_path(&self, workspace: &Path) -> PathBuf {
        resolve_config_path(
            workspace,
            self.repos_root
                .as_deref()
                .unwrap_or_else(|| Path::new(".graft/repos")),
        )
    }

    pub(crate) fn promote_target_path(&self, workspace: &Path, target_id: &str) -> Result<PathBuf> {
        let target = self
            .promote_targets
            .get(target_id)
            .with_context(|| format!("unknown promote target {target_id}"))?;
        Ok(resolve_config_path(workspace, &target.path))
    }

    pub(crate) fn repo_path(&self, workspace: &Path, repo_id: &str) -> Result<PathBuf> {
        let repo = self
            .repos
            .get(repo_id)
            .with_context(|| format!("unknown repo id {repo_id}"))?;
        Ok(match repo.path.as_deref() {
            Some(path) => resolve_config_path(workspace, path),
            None => self.repos_root_path(workspace).join(repo_id),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoConfig {
    pub(crate) url: String,
    pub(crate) path: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub(crate) auto_clone: bool,
    pub(crate) default_branch: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateConfig {
    pub(crate) default_base: Option<String>,
    pub(crate) default_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionConfig {
    #[serde(default)]
    pub(crate) base_properties: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotionConfig {
    #[serde(default)]
    pub(crate) required_properties: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteTargetConfig {
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) required_properties: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PropertyConfig {
    #[serde(default)]
    properties: Vec<RawPropertyDef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPropertyDef {
    name: Option<String>,
    query: Option<Query>,
    evaluator: Option<Evaluator>,
    judge: Option<Judge>,
}

impl PropertyConfig {
    fn into_property_map(self, path: &Path) -> Result<BTreeMap<String, PropertyDef>> {
        let mut seen = BTreeSet::new();
        let mut out = BTreeMap::new();
        for (index, raw) in self.properties.into_iter().enumerate() {
            let label = format!("{} properties[{index}]", path.display());
            let Some(name) = raw.name else {
                bail!("[E_INCOMPLETE_PROPERTY] {label} must set name");
            };
            if name.trim().is_empty() {
                bail!("[E_INCOMPLETE_PROPERTY] property name must not be empty");
            }
            if !seen.insert(name.clone()) {
                bail!("[E_DUPLICATE_PROPERTY] duplicate property name {name}");
            }
            let Some(query) = raw.query else {
                bail!("[E_INCOMPLETE_PROPERTY] property {name} must set query");
            };
            let Some(evaluator) = raw.evaluator else {
                bail!("[E_INCOMPLETE_PROPERTY] property {name} must set evaluator");
            };
            let Some(judge) = raw.judge else {
                bail!("[E_INCOMPLETE_PROPERTY] property {name} must set judge");
            };
            out.insert(
                name.clone(),
                PropertyDef {
                    name,
                    query,
                    evaluator,
                    judge,
                },
            );
        }
        Ok(out)
    }
}

fn raw_property_to_def(name: String, raw: RawPropertyDef, label: &str) -> Result<PropertyDef> {
    if name.trim().is_empty() {
        bail!("[E_INCOMPLETE_PROPERTY] property name must not be empty");
    }
    let Some(query) = raw.query else {
        bail!("[E_INCOMPLETE_PROPERTY] property {name} in {label} must set query");
    };
    let Some(evaluator) = raw.evaluator else {
        bail!("[E_INCOMPLETE_PROPERTY] property {name} in {label} must set evaluator");
    };
    let Some(judge) = raw.judge else {
        bail!("[E_INCOMPLETE_PROPERTY] property {name} in {label} must set judge");
    };
    Ok(PropertyDef {
        name,
        query,
        evaluator,
        judge,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PropertyLock {
    #[serde(default = "graft_lock_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) properties: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) repos: BTreeMap<String, RepoLockEntry>,
}

impl Default for PropertyLock {
    fn default() -> Self {
        Self {
            version: graft_lock_version(),
            properties: BTreeMap::new(),
            repos: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoLockEntry {
    pub(crate) treeish: String,
    pub(crate) resolved_oid: String,
    pub(crate) resolved_at: String,
}

fn graft_lock_version() -> u32 {
    1
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PropertyLockDrift {
    pub(crate) missing: Vec<String>,
    pub(crate) changed: Vec<PropertyLockChange>,
    pub(crate) extra: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PropertyLockChange {
    pub(crate) name: String,
    pub(crate) locked: String,
    pub(crate) current: String,
}

impl PropertyLockDrift {
    pub(crate) fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.extra.is_empty()
    }

    pub(crate) fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!("missing: {}", self.missing.join(", ")));
        }
        if !self.changed.is_empty() {
            parts.push(format!(
                "changed: {}",
                self.changed
                    .iter()
                    .map(|change| change.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.extra.is_empty() {
            parts.push(format!("extra: {}", self.extra.join(", ")));
        }
        parts.join("; ")
    }
}

pub(crate) fn load_graft_config(store: &GraftStore) -> Result<GraftConfig> {
    let path = store.paths().config();
    if !path.exists() {
        bail!("{} does not exist; run graft init first", path.display());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut config: GraftConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    config.validate_repos()?;
    let workspace = store.paths().workspace();
    let _ = config.repos_root_path(workspace);
    for repo_id in config.repos.keys() {
        let _ = config.repo_path(workspace, repo_id)?;
    }
    config.properties = load_property_defs(store)?;
    ensure_property_lock_current(store, &config.properties)?;
    Ok(config)
}

pub(crate) fn load_optional_graft_config(store: &GraftStore) -> Result<GraftConfig> {
    if store.paths().config().exists() {
        load_graft_config(store)
    } else {
        Ok(GraftConfig::default())
    }
}

pub(crate) fn load_property_defs(store: &GraftStore) -> Result<BTreeMap<String, PropertyDef>> {
    let path = store.paths().properties_config();
    if !path.exists() {
        bail!("{} does not exist; run graft init first", path.display());
    }
    if path.is_dir() {
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry?;
            let file = entry.path();
            if file.extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let name = file
                .file_stem()
                .and_then(|value| value.to_str())
                .with_context(|| format!("invalid property filename {}", file.display()))?
                .to_string();
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let raw: RawPropertyDef =
                toml::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
            if out.contains_key(&name) {
                bail!("[E_DUPLICATE_PROPERTY] duplicate property name {name}");
            }
            out.insert(
                name.clone(),
                raw_property_to_def(name, raw, &file.display().to_string())?,
            );
        }
        return Ok(out);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: PropertyConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    config.into_property_map(&path)
}

pub(crate) fn property_lock_path(store: &GraftStore) -> PathBuf {
    store.paths().properties_lock()
}

pub(crate) fn current_property_lock(defs: &BTreeMap<String, PropertyDef>) -> Result<PropertyLock> {
    let mut properties = BTreeMap::new();
    for (name, def) in defs {
        properties.insert(name.clone(), def.property_id()?.to_string());
    }
    Ok(PropertyLock {
        version: graft_lock_version(),
        properties,
        repos: BTreeMap::new(),
    })
}

pub(crate) fn read_property_lock(store: &GraftStore) -> Result<Option<PropertyLock>> {
    let path = property_lock_path(store);
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let lock: PropertyLock =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(lock))
}

fn write_graft_lock(store: &GraftStore, lock: &PropertyLock) -> Result<()> {
    let path = property_lock_path(store);
    let text = toml::to_string_pretty(lock).context("serialize graft.lock")?;
    std::fs::write(
        &path,
        format!("# @generated by graft property lock; do not edit by hand\n{text}"),
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub(crate) fn write_property_lock(
    store: &GraftStore,
    defs: &BTreeMap<String, PropertyDef>,
) -> Result<PropertyLock> {
    let mut lock = current_property_lock(defs)?;
    if let Some(existing) = read_property_lock(store)? {
        lock.repos = existing.repos;
    }
    write_graft_lock(store, &lock)?;
    Ok(lock)
}

pub(crate) fn write_repo_lock_entry(
    store: &GraftStore,
    repo_id: &str,
    treeish: &str,
    resolved_oid: &str,
) -> Result<PropertyLock> {
    let mut lock = read_property_lock(store)?.unwrap_or_default();
    lock.version = graft_lock_version();
    lock.repos.insert(
        repo_id.to_string(),
        RepoLockEntry {
            treeish: treeish.to_string(),
            resolved_oid: resolved_oid.to_string(),
            resolved_at: time::OffsetDateTime::now_utc().to_string(),
        },
    );
    write_graft_lock(store, &lock)?;
    Ok(lock)
}

pub(crate) fn property_lock_drift(
    defs: &BTreeMap<String, PropertyDef>,
    lock: &PropertyLock,
) -> Result<PropertyLockDrift> {
    let current = current_property_lock(defs)?;
    let mut drift = PropertyLockDrift::default();
    for (name, current_id) in &current.properties {
        match lock.properties.get(name) {
            None => drift.missing.push(name.clone()),
            Some(locked_id) if locked_id != current_id => drift.changed.push(PropertyLockChange {
                name: name.clone(),
                locked: locked_id.clone(),
                current: current_id.clone(),
            }),
            Some(_) => {}
        }
    }
    for name in lock.properties.keys() {
        if !current.properties.contains_key(name) {
            drift.extra.push(name.clone());
        }
    }
    Ok(drift)
}

pub(crate) fn ensure_property_lock_current(
    store: &GraftStore,
    defs: &BTreeMap<String, PropertyDef>,
) -> Result<PropertyLock> {
    let Some(lock) = read_property_lock(store)? else {
        return write_property_lock(store, defs);
    };
    let drift = property_lock_drift(defs, &lock)?;
    if !drift.is_clean() {
        return write_property_lock(store, defs);
    }
    Ok(lock)
}

pub(crate) fn resolve_property(
    config: &GraftConfig,
    name: &str,
) -> Result<graft_core::PropertyRef> {
    let def = config.properties.get(name).with_context(|| {
        format!("[E_UNKNOWN_PROPERTY] property {name} is not configured in properties/*.toml")
    })?;
    def.property_ref().map_err(Into::into)
}

fn validate_repo_id(repo_id: &str) -> Result<()> {
    if repo_id.is_empty() {
        bail!("repo id must not be empty");
    }
    if !repo_id.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
    }) {
        bail!("invalid repo id {repo_id}; use lowercase letters, digits, '-' or '_'");
    }
    Ok(())
}

fn validate_config_path(label: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{label} must not be empty");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{label} must not contain '..'");
    }
    Ok(())
}

fn resolve_config_path(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if workspace.is_absolute() {
        workspace.join(path)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(workspace)
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(text: &str) -> GraftConfig {
        let config: GraftConfig = toml::from_str(text).unwrap();
        config.validate_repos().unwrap();
        config
    }

    #[test]
    fn repos_config_defaults_auto_clone_and_repo_path() {
        let config = parse_config(
            r#"
repos_root = "vendor/repos"

[repos.graft]
url = "https://example.test/graft.git"
default_branch = "main"
"#,
        );
        let repo = config.repos.get("graft").unwrap();
        assert!(repo.auto_clone);
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        assert_eq!(
            config
                .repo_path(Path::new("/workspace/project"), "graft")
                .unwrap(),
            PathBuf::from("/workspace/project/vendor/repos/graft")
        );
    }

    #[test]
    fn repos_config_accepts_explicit_path_and_auto_clone_override() {
        let config = parse_config(
            r#"
[repos.demo_repo]
url = "file:///tmp/demo.git"
path = "repos/demo"
auto_clone = false
"#,
        );
        let repo = config.repos.get("demo_repo").unwrap();
        assert!(!repo.auto_clone);
        assert_eq!(
            config
                .repo_path(Path::new("/workspace/project"), "demo_repo")
                .unwrap(),
            PathBuf::from("/workspace/project/repos/demo")
        );
    }

    #[test]
    fn repos_config_rejects_bad_repo_id() {
        let config: GraftConfig = toml::from_str(
            r#"
[repos.Bad]
url = "https://example.test/bad.git"
"#,
        )
        .unwrap();
        assert!(
            config
                .validate_repos()
                .unwrap_err()
                .to_string()
                .contains("invalid repo id")
        );
    }

    #[test]
    fn repos_config_rejects_path_traversal() {
        let config: GraftConfig = toml::from_str(
            r#"
[repos.demo]
url = "https://example.test/demo.git"
path = "../demo"
"#,
        )
        .unwrap();
        assert!(
            config
                .validate_repos()
                .unwrap_err()
                .to_string()
                .contains("must not contain '..'")
        );
    }

    #[test]
    fn property_config_rejects_incomplete_property() {
        let config: PropertyConfig = toml::from_str(
            r#"
[[properties]]
name = "Broken"
"#,
        )
        .unwrap();
        let err = config
            .into_property_map(Path::new("properties/*.toml"))
            .unwrap_err();
        assert!(err.to_string().contains("E_INCOMPLETE_PROPERTY"));
    }

    #[test]
    fn property_config_rejects_duplicate_name() {
        let config: PropertyConfig = toml::from_str(
            r#"
[[properties]]
name = "ValidPatch"
[properties.query]
kind = "change"
[properties.evaluator]
kind = "builtin"
name = "valid_patch"
[properties.evaluator.options]
[properties.judge]
kind = "exit_code_zero"

[[properties]]
name = "ValidPatch"
[properties.query]
kind = "change"
[properties.evaluator]
kind = "builtin"
name = "has_change"
[properties.evaluator.options]
[properties.judge]
kind = "exit_code_zero"
"#,
        )
        .unwrap();
        let err = config
            .into_property_map(Path::new("properties/*.toml"))
            .unwrap_err();
        assert!(err.to_string().contains("E_DUPLICATE_PROPERTY"));
    }
}
