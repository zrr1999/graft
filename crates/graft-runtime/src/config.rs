use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use graft_core::{Constraint, PropertyRef, PropertySpec};

use crate::roto_properties::load_roto_property_specs;
use graft_store::GraftStore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GraftConfig {
    #[serde(default = "default_config_schema")]
    pub(crate) schema: u32,
    #[serde(default)]
    pub(crate) admission: AdmissionConfig,
    #[serde(default)]
    pub(crate) promotion: PromotionConfig,
    #[serde(default)]
    pub(crate) sync: SyncConfig,
    #[serde(default)]
    pub(crate) promote_targets: BTreeMap<String, PromoteTargetConfig>,
    #[serde(default)]
    pub(crate) repos: BTreeMap<String, RepoConfig>,
    #[serde(skip)]
    pub(crate) properties: BTreeMap<String, PropertySpec>,
}

impl GraftConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != default_config_schema() {
            bail!(
                "[E_UNSUPPORTED_CONFIG_SCHEMA] graft.toml schema {} is not supported",
                self.schema
            );
        }
        self.validate_repos()?;
        self.validate_property_names()
    }

    fn validate_repos(&self) -> Result<()> {
        for (target_id, target) in &self.promote_targets {
            validate_repo_id(target_id)?;
            validate_config_path(&format!("promote_targets.{target_id}.path"), &target.path)?;
            validate_promote_target_branch(target_id, target.branch.as_deref())?;
        }
        for (repo_id, repo) in &self.repos {
            validate_repo_id(repo_id)?;
            if repo.url.trim().is_empty() {
                bail!("repo {repo_id} must set a non-empty url");
            }
            validate_repo_default_branch(repo_id, repo.default_branch.as_deref())?;
        }
        Ok(())
    }

    fn validate_property_names(&self) -> Result<()> {
        validate_required_property_names(
            "admission.required_properties",
            &self.admission.required_properties,
        )?;
        validate_required_property_names(
            "promotion.required_properties",
            &self.promotion.required_properties,
        )?;
        for (target_id, target) in &self.promote_targets {
            validate_required_property_names(
                &format!("promote_targets.{target_id}.required_properties"),
                &target.required_properties,
            )?;
        }
        Ok(())
    }

    pub(crate) fn repos_root_path(&self, workspace: &Path) -> Result<PathBuf> {
        resolve_config_path(workspace, Path::new(".graft/repos"))
    }

    pub(crate) fn promote_target_path(&self, workspace: &Path, target_id: &str) -> Result<PathBuf> {
        let target = self
            .promote_targets
            .get(target_id)
            .with_context(|| format!("unknown promote target {target_id}"))?;
        resolve_config_path(workspace, &target.path)
    }

    pub(crate) fn repo_path(&self, workspace: &Path, repo_id: &str) -> Result<PathBuf> {
        self.repos
            .get(repo_id)
            .with_context(|| format!("unknown repo id {repo_id}"))?;
        Ok(self.repos_root_path(workspace)?.join(repo_id))
    }
}

impl Default for GraftConfig {
    fn default() -> Self {
        Self {
            schema: default_config_schema(),
            admission: AdmissionConfig::default(),
            promotion: PromotionConfig::default(),
            sync: SyncConfig::default(),
            promote_targets: BTreeMap::new(),
            repos: BTreeMap::new(),
            properties: BTreeMap::new(),
        }
    }
}

fn default_config_schema() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoConfig {
    pub(crate) url: String,
    pub(crate) default_branch: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionConfig {
    #[serde(default)]
    pub(crate) required_properties: RequiredPropertiesConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotionConfig {
    #[serde(default)]
    pub(crate) required_properties: RequiredPropertiesConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RequiredPropertiesConfig {
    Expr(ConstraintConfig),
    Names(Vec<String>),
}

impl Default for RequiredPropertiesConfig {
    fn default() -> Self {
        Self::Names(Vec::new())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ConstraintTermConfig {
    Name(String),
    Expr(Box<ConstraintConfig>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConstraintConfig {
    pub(crate) primitive: Option<String>,
    pub(crate) all_of: Option<Vec<ConstraintTermConfig>>,
    pub(crate) any_of: Option<Vec<ConstraintTermConfig>>,
    pub(crate) both: Option<Vec<ConstraintTermConfig>>,
    pub(crate) either: Option<Vec<ConstraintTermConfig>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncConfig {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteTargetConfig {
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) required_properties: RequiredPropertiesConfig,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequiresVisitState {
    Visiting,
    Done,
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
    let mut config = load_graft_config_metadata(store)?;
    config.properties = load_property_defs(store)?;
    require_property_lock_current(store, &config.properties)?;
    Ok(config)
}

pub(crate) fn load_graft_config_metadata(store: &GraftStore) -> Result<GraftConfig> {
    let path = store.paths().config();
    if !path.exists() {
        bail!("{} does not exist; run graft init first", path.display());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let config: GraftConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    config.validate()?;
    let workspace = store.paths().workspace();
    for repo_id in config.repos.keys() {
        let _ = config.repo_path(workspace, repo_id)?;
    }
    Ok(config)
}

pub(crate) fn load_property_defs(store: &GraftStore) -> Result<BTreeMap<String, PropertySpec>> {
    let legacy_path = store.paths().properties_config();
    if legacy_path.exists() {
        bail!(
            "[E_LEGACY_PROPERTIES_UNSUPPORTED] {} is legacy property configuration; use only the v2 properties.roto file",
            legacy_path.display()
        );
    }
    let roto_path = store.paths().properties_roto_config();
    if !roto_path.exists() {
        bail!(
            "{} does not exist; run graft init first",
            roto_path.display()
        );
    }
    let properties = load_roto_property_specs(&roto_path)?;
    validate_property_requires_graph(&properties)?;
    Ok(properties)
}

pub(crate) fn validate_property_requires_graph(
    defs: &BTreeMap<String, PropertySpec>,
) -> Result<()> {
    for (name, spec) in defs {
        if spec.name.as_str() != name {
            bail!(
                "[E_PROPERTY_NAME_MISMATCH] property map key `{name}` contains spec named `{}`",
                spec.name.as_str()
            );
        }
        let mut seen_requires = std::collections::BTreeSet::new();
        for required in &spec.plan.requires {
            let required = required.as_str();
            if !seen_requires.insert(required) {
                bail!(
                    "[E_PROPERTY_REQUIRES_DUPLICATE] property `{name}` lists required property `{required}` more than once"
                );
            }
            if !defs.contains_key(required) {
                bail!(
                    "[E_PROPERTY_REQUIRES_UNKNOWN] property `{name}` requires unknown property `{required}`"
                );
            }
        }
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    for name in defs.keys() {
        visit_requires_graph(name, defs, &mut states, &mut stack)?;
    }
    Ok(())
}

fn visit_requires_graph(
    name: &str,
    defs: &BTreeMap<String, PropertySpec>,
    states: &mut BTreeMap<String, RequiresVisitState>,
    stack: &mut Vec<String>,
) -> Result<()> {
    match states.get(name).copied() {
        Some(RequiresVisitState::Done) => return Ok(()),
        Some(RequiresVisitState::Visiting) => {
            let start = stack
                .iter()
                .position(|entry| entry == name)
                .unwrap_or_default();
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            bail!(
                "[E_PROPERTY_REQUIRES_CYCLE] property requires cycle: {}",
                cycle.join(" -> ")
            );
        }
        None => {}
    }

    states.insert(name.to_string(), RequiresVisitState::Visiting);
    stack.push(name.to_string());
    let spec = defs
        .get(name)
        .with_context(|| format!("missing property `{name}` while walking requires graph"))?;
    for required in &spec.plan.requires {
        visit_requires_graph(required.as_str(), defs, states, stack)?;
    }
    stack.pop();
    states.insert(name.to_string(), RequiresVisitState::Done);
    Ok(())
}

pub(crate) fn property_lock_path(store: &GraftStore) -> PathBuf {
    store.paths().properties_lock()
}

pub(crate) fn current_property_lock(defs: &BTreeMap<String, PropertySpec>) -> Result<PropertyLock> {
    validate_property_requires_graph(defs)?;
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
    defs: &BTreeMap<String, PropertySpec>,
) -> Result<PropertyLock> {
    for spec in defs.values() {
        store.write_property_spec(spec)?;
    }
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
    url: &str,
    treeish: &str,
    resolved_oid: &str,
) -> Result<PropertyLock> {
    let Some(mut lock) = read_property_lock(store)? else {
        bail!(
            "[E_PROPERTY_LOCK_MISSING] graft.lock is missing; run `graft property lock` before locking repository refs"
        );
    };
    lock.version = graft_lock_version();
    lock.repos.insert(
        repo_id.to_string(),
        RepoLockEntry {
            url: Some(url.to_string()),
            treeish: treeish.to_string(),
            resolved_oid: resolved_oid.to_string(),
            resolved_at: time::OffsetDateTime::now_utc().to_string(),
        },
    );
    write_graft_lock(store, &lock)?;
    Ok(lock)
}

pub(crate) fn require_repo_lock_current(
    store: &GraftStore,
    repo_id: &str,
    repo_config: &RepoConfig,
    requested_treeish: &str,
) -> Result<RepoLockEntry> {
    let Some(lock) = read_property_lock(store)? else {
        bail!(
            "[E_PROPERTY_LOCK_MISSING] graft.lock is missing; run `graft property lock` before resolving repository bases"
        );
    };
    let Some(entry) = lock.repos.get(repo_id) else {
        bail!(
            "[E_REPO_LOCK_DRIFT] repo {repo_id} is missing from graft.lock; run `graft repo lock {repo_id}`"
        );
    };
    let expected_treeish = repo_config.default_branch.as_deref().unwrap_or("HEAD");
    if entry.url.as_deref() != Some(repo_config.url.as_str()) {
        bail!(
            "[E_REPO_LOCK_DRIFT] repo {repo_id} url in graft.lock does not match graft.toml; run `graft repo update {repo_id}`"
        );
    }
    if entry.treeish != expected_treeish {
        bail!(
            "[E_REPO_LOCK_DRIFT] repo {repo_id} treeish in graft.lock is `{}` but graft.toml resolves to `{expected_treeish}`; run `graft repo update {repo_id}`",
            entry.treeish
        );
    }
    if entry.treeish != requested_treeish {
        bail!(
            "[E_REPO_LOCK_DRIFT] repo base `{repo_id}@{requested_treeish}` is not locked; graft.lock has `{repo_id}@{}`; use `repo:{repo_id}@{}` or run `graft repo update {repo_id}`",
            entry.treeish,
            entry.treeish
        );
    }
    if entry.resolved_oid.trim().is_empty() {
        bail!(
            "[E_REPO_LOCK_DRIFT] repo {repo_id} lock entry has an empty resolved_oid; run `graft repo update {repo_id}`"
        );
    }
    Ok(entry.clone())
}

pub(crate) fn property_lock_drift(
    defs: &BTreeMap<String, PropertySpec>,
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

pub(crate) fn require_property_lock_current(
    store: &GraftStore,
    defs: &BTreeMap<String, PropertySpec>,
) -> Result<PropertyLock> {
    let Some(lock) = read_property_lock(store)? else {
        bail!(
            "[E_PROPERTY_LOCK_MISSING] graft.lock is missing; run `graft property lock` to derive property ids from properties.roto"
        );
    };
    let drift = property_lock_drift(defs, &lock)?;
    if !drift.is_clean() {
        bail!(
            "[E_PROPERTY_LOCK_DRIFT] graft.lock is stale ({}); run `graft property lock` to refresh it",
            drift.summary()
        );
    }
    Ok(lock)
}

pub(crate) fn resolve_property(config: &GraftConfig, name: &str) -> Result<PropertyRef> {
    let spec = config.properties.get(name).with_context(|| {
        format!("[E_UNKNOWN_PROPERTY] property {name} is not configured in properties.roto")
    })?;
    spec.property_ref().map_err(Into::into)
}

fn validate_repo_id(repo_id: &str) -> Result<()> {
    if repo_id.is_empty() {
        bail!("repo id must not be empty");
    }
    if repo_id == "workspace" {
        bail!("repo id `workspace` is reserved for the workspace scope name");
    }
    if !repo_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid repo id {repo_id}; use ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

pub(crate) fn required_properties_constraint(
    config: &GraftConfig,
    required: &RequiredPropertiesConfig,
) -> Result<Constraint> {
    match required {
        RequiredPropertiesConfig::Names(names) => names
            .iter()
            .map(|name| constraint_primitive(config, name))
            .collect::<Result<Vec<_>>>()
            .map(Constraint::all_of),
        RequiredPropertiesConfig::Expr(expr) => constraint_expr(config, expr),
    }
}

fn constraint_expr(config: &GraftConfig, expr: &ConstraintConfig) -> Result<Constraint> {
    let present = [
        expr.primitive.is_some(),
        expr.all_of.is_some(),
        expr.any_of.is_some(),
        expr.both.is_some(),
        expr.either.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if present == 0 {
        return Ok(Constraint::Top);
    }
    if present != 1 {
        bail!(
            "[E_INVALID_CONSTRAINT] constraint expression must set exactly one of primitive/all_of/any_of/both/either"
        );
    }
    if let Some(primitive) = &expr.primitive {
        return constraint_primitive(config, primitive);
    }
    if let Some(items) = &expr.all_of {
        return items
            .iter()
            .map(|item| constraint_term(config, item))
            .collect::<Result<Vec<_>>>()
            .map(Constraint::all_of);
    }
    if let Some(items) = &expr.any_of {
        return items
            .iter()
            .map(|item| constraint_term(config, item))
            .collect::<Result<Vec<_>>>()
            .map(Constraint::any_of);
    }
    if let Some(items) = &expr.both {
        if items.len() != 2 {
            bail!("[E_INVALID_CONSTRAINT] both expects exactly two terms");
        }
        return Ok(Constraint::all_of(vec![
            constraint_term(config, &items[0])?,
            constraint_term(config, &items[1])?,
        ]));
    }
    if let Some(items) = &expr.either {
        if items.len() != 2 {
            bail!("[E_INVALID_CONSTRAINT] either expects exactly two terms");
        }
        return Ok(Constraint::any_of(vec![
            constraint_term(config, &items[0])?,
            constraint_term(config, &items[1])?,
        ]));
    }
    unreachable!()
}

fn constraint_term(config: &GraftConfig, term: &ConstraintTermConfig) -> Result<Constraint> {
    match term {
        ConstraintTermConfig::Name(name) => constraint_primitive(config, name),
        ConstraintTermConfig::Expr(expr) => constraint_expr(config, expr),
    }
}

fn constraint_primitive(config: &GraftConfig, value: &str) -> Result<Constraint> {
    validate_property_name("property requirement", value)?;
    Ok(Constraint::Primitive {
        property: resolve_property(config, value)?,
    })
}

fn validate_required_property_names(
    label: &str,
    properties: &RequiredPropertiesConfig,
) -> Result<()> {
    match properties {
        RequiredPropertiesConfig::Names(names) => {
            for name in names {
                validate_property_name(label, name)?;
            }
        }
        RequiredPropertiesConfig::Expr(expr) => {
            validate_constraint_expr_property_names(label, expr)?
        }
    }
    Ok(())
}

fn validate_constraint_expr_property_names(label: &str, expr: &ConstraintConfig) -> Result<()> {
    for item in expr
        .all_of
        .iter()
        .chain(expr.any_of.iter())
        .chain(expr.both.iter())
        .chain(expr.either.iter())
        .flatten()
    {
        validate_constraint_term_property_names(label, item)?;
    }
    if let Some(primitive) = &expr.primitive {
        validate_property_name(label, primitive)?;
    }
    Ok(())
}

fn validate_constraint_term_property_names(label: &str, term: &ConstraintTermConfig) -> Result<()> {
    match term {
        ConstraintTermConfig::Name(name) => validate_property_name(label, name)?,
        ConstraintTermConfig::Expr(expr) => validate_constraint_expr_property_names(label, expr)?,
    }
    Ok(())
}

fn validate_property_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("[E_INVALID_PROPERTY] {label} contains an empty property name");
    }
    if value.contains(':') {
        bail!(
            "[E_SCOPED_PROPERTY_UNSUPPORTED] {label} uses `{value}`, but property requirements must be bare names; properties are whole-workspace by definition"
        );
    }
    Ok(())
}

pub(crate) fn validate_repo_default_branch(
    repo_id: &str,
    default_branch: Option<&str>,
) -> Result<()> {
    if default_branch.is_some_and(|value| value.trim().is_empty()) {
        bail!("repo {repo_id} default_branch must not be empty");
    }
    Ok(())
}

fn validate_promote_target_branch(target_id: &str, branch: Option<&str>) -> Result<()> {
    if branch.is_some_and(|value| value.trim().is_empty()) {
        bail!("promote target {target_id} branch must not be empty");
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

fn resolve_config_path(workspace: &Path, path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else if workspace.is_absolute() {
        Ok(workspace.join(path))
    } else {
        Ok(std::env::current_dir()
            .context("[E_CWD_UNAVAILABLE] cannot resolve relative config path")?
            .join(workspace)
            .join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{CheckPlan, PropertyName, PropertyPlan, Severity};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse_config(text: &str) -> GraftConfig {
        let config: GraftConfig = toml::from_str(text).unwrap();
        config.validate().unwrap();
        config
    }

    fn test_workspace(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "graft-runtime-config-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn test_property_spec(name: &str, requires: &[&str]) -> PropertySpec {
        PropertySpec {
            name: PropertyName::new(name),
            plan: PropertyPlan {
                checks: vec![CheckPlan::Unavailable {
                    reason: format!("{name} check"),
                }],
                requires: requires
                    .iter()
                    .map(|name| PropertyName::new(*name))
                    .collect(),
            },
            description: format!("{name} property"),
            severity: Severity::Blocking,
            source_ref: None,
        }
    }

    fn config_with_properties(text: &str, names: &[&str]) -> GraftConfig {
        let mut config = parse_config(text);
        for name in names {
            config
                .properties
                .insert((*name).to_string(), test_property_spec(name, &[]));
        }
        config
    }

    #[test]
    fn required_properties_flat_list_lowers_to_all_of_constraint() {
        let config = config_with_properties(
            r#"
[admission]
required_properties = ["fmt_clean", "tests_pass"]
"#,
            &["fmt_clean", "tests_pass"],
        );

        let constraint =
            required_properties_constraint(&config, &config.admission.required_properties).unwrap();

        assert_eq!(
            constraint,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "fmt_clean").unwrap()),
                Constraint::primitive(resolve_property(&config, "tests_pass").unwrap()),
            ])
        );
    }

    #[test]
    fn required_properties_tagged_all_of_lowers_to_both_constraint() {
        let config = config_with_properties(
            r#"
[admission.required_properties]
all_of = ["fmt_clean", "tests_pass"]
"#,
            &["fmt_clean", "tests_pass"],
        );

        let constraint =
            required_properties_constraint(&config, &config.admission.required_properties).unwrap();

        assert_eq!(
            constraint,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "fmt_clean").unwrap(),),
                Constraint::primitive(resolve_property(&config, "tests_pass").unwrap(),),
            ])
        );
    }

    #[test]
    fn required_properties_tagged_any_of_lowers_to_either_constraint() {
        let config = config_with_properties(
            r#"
[admission.required_properties]
any_of = ["fast_check", "slow_check"]
"#,
            &["fast_check", "slow_check"],
        );

        let constraint =
            required_properties_constraint(&config, &config.admission.required_properties).unwrap();

        assert_eq!(
            constraint,
            Constraint::any_of(vec![
                Constraint::primitive(resolve_property(&config, "fast_check").unwrap()),
                Constraint::primitive(resolve_property(&config, "slow_check").unwrap()),
            ])
        );
    }

    #[test]
    fn config_schema_defaults_to_v1_and_rejects_unknown_versions() {
        assert_eq!(parse_config("").schema, 1);
        assert_eq!(parse_config("schema = 1").schema, 1);

        let config: GraftConfig = toml::from_str("schema = 2").unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("[E_UNSUPPORTED_CONFIG_SCHEMA]"), "{error}");
    }

    #[test]
    fn repos_config_defaults_to_graft_managed_repo_path() {
        let config = parse_config(
            r#"
[repos.graft]
url = "https://example.test/graft.git"
default_branch = "main"
"#,
        );
        let repo = config.repos.get("graft").unwrap();
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        assert_eq!(
            config
                .repo_path(Path::new("/workspace/project"), "graft")
                .unwrap(),
            PathBuf::from("/workspace/project/.graft/repos/graft")
        );
    }

    #[test]
    fn repos_config_rejects_empty_default_branch() {
        for (label, value) in [("empty", ""), ("blank", " \t")] {
            let config: GraftConfig = toml::from_str(&format!(
                r#"
[repos.demo]
url = "https://example.test/demo.git"
default_branch = "{value}"
"#
            ))
            .unwrap();

            let message = config.validate().unwrap_err().to_string();

            assert!(
                message.contains("repo demo default_branch must not be empty"),
                "{label}: {message}"
            );
        }
    }

    #[test]
    fn promote_target_path_resolves_relative_to_workspace_root() {
        let config = parse_config(
            r#"
[promote_targets.docs]
path = "targets/docs"
"#,
        );

        assert_eq!(
            config
                .promote_target_path(Path::new("/workspace/project"), "docs")
                .unwrap(),
            PathBuf::from("/workspace/project/targets/docs")
        );
    }

    #[test]
    fn promote_targets_reject_empty_branch() {
        for (label, value) in [("empty", ""), ("blank", " \t")] {
            let config: GraftConfig = toml::from_str(&format!(
                r#"
[promote_targets.release]
path = "targets/release"
branch = "{value}"
"#
            ))
            .unwrap();

            let message = config.validate().unwrap_err().to_string();

            assert!(
                message.contains("promote target release branch must not be empty"),
                "{label}: {message}"
            );
        }
    }

    #[test]
    fn sync_config_is_enabled_unless_explicitly_disabled() {
        assert!(parse_config("").sync.enabled);

        let config = parse_config(
            r#"
[sync]
enabled = true
"#,
        );
        assert!(config.sync.enabled);

        let config = parse_config(
            r#"
[sync]
enabled = false
"#,
        );
        assert!(!config.sync.enabled);
    }

    #[test]
    fn load_graft_config_requires_existing_property_lock() {
        let dir = test_workspace("missing-lock-load");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        assert!(!dir.join("graft.lock").exists());

        let error = load_graft_config(&store).unwrap_err().to_string();

        assert!(error.contains("[E_PROPERTY_LOCK_MISSING]"), "{error}");
        assert!(
            !dir.join("graft.lock").exists(),
            "read-only config load must not recreate graft.lock"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_property_defs_discovers_v2_top_level_property_functions() {
        let dir = test_workspace("v2-roto-load");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        std::fs::write(
            dir.join("properties.roto"),
            r#"
fn helper(app: Application) -> Check {
    unavailable("helper only")
}

fn no_generated_artifacts(app: Application) -> Property {
    property(
        [app.changed_paths().any_match(["target/**", "*.tmp"]).failure()],
        "no generated artifacts",
        Severity.Blocking,
        [],
    )
}

fn cargo_tests_pass(app: Application) -> Property {
    property(
        [call(["cargo", "test", "--all-targets"], app.target()).exit_code_is(0).success()],
        "cargo tests pass",
        Severity.Warning,
        ["no_generated_artifacts"],
    )
}
"#,
        )
        .unwrap();

        let properties = load_property_defs(&store).unwrap();

        assert_eq!(
            properties.keys().cloned().collect::<Vec<_>>(),
            vec!["cargo_tests_pass", "no_generated_artifacts"]
        );
        let cargo = properties.get("cargo_tests_pass").unwrap();
        assert_eq!(cargo.name.as_str(), "cargo_tests_pass");
        assert_eq!(
            cargo.plan.requires,
            vec![PropertyName::new("no_generated_artifacts")]
        );
        assert_eq!(cargo.severity, graft_core::Severity::Warning);
        assert!(
            cargo
                .property_id()
                .unwrap()
                .as_str()
                .starts_with("property:")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn property_requires_graph_rejects_missing_dependencies() {
        let defs = BTreeMap::from([(
            "cargo_tests_pass".to_string(),
            test_property_spec("cargo_tests_pass", &["no_generated_artifacts"]),
        )]);

        let error = validate_property_requires_graph(&defs)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_PROPERTY_REQUIRES_UNKNOWN]"), "{error}");
        assert!(error.contains("cargo_tests_pass"), "{error}");
        assert!(error.contains("no_generated_artifacts"), "{error}");
    }

    #[test]
    fn property_requires_graph_rejects_cycles() {
        let defs = BTreeMap::from([
            ("a".to_string(), test_property_spec("a", &["b"])),
            ("b".to_string(), test_property_spec("b", &["c"])),
            ("c".to_string(), test_property_spec("c", &["a"])),
        ]);

        let error = validate_property_requires_graph(&defs)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_PROPERTY_REQUIRES_CYCLE]"), "{error}");
        assert!(error.contains("a -> b -> c -> a"), "{error}");
    }

    #[test]
    fn property_lock_for_v2_roto_uses_static_plan_identity() {
        let dir = test_workspace("v2-roto-lock");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let first = r#"
fn cargo_tests_pass(app: Application) -> Property {
    property(
        [call(["cargo", "test"], app.target()).exit_code_is(0).success()],
        "first description",
        Severity.Blocking,
        [],
    )
}
"#;
        let second = r#"
// comments and metadata do not affect v2 property identity
fn cargo_tests_pass(app: Application) -> Property {
    let check = call(["cargo", "test"], app.target()).exit_code_is(0).success();
    property(
        [check],
        "second description",
        Severity.Info,
        [],
    )
}
"#;

        std::fs::write(dir.join("properties.roto"), first).unwrap();
        let first_defs = load_property_defs(&store).unwrap();
        let first_lock = current_property_lock(&first_defs).unwrap();
        write_property_lock(&store, &first_defs).unwrap();

        std::fs::write(dir.join("properties.roto"), second).unwrap();
        let second_defs = load_property_defs(&store).unwrap();
        let second_lock = current_property_lock(&second_defs).unwrap();

        assert_eq!(first_lock.properties, second_lock.properties);
        assert!(store.paths().object_properties().exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn repo_lock_entry_requires_existing_property_lock_and_preserves_properties() {
        let dir = test_workspace("repo-lock-existing");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        assert!(!dir.join("graft.lock").exists());

        let missing = write_repo_lock_entry(
            &store,
            "demo",
            "https://example.invalid/demo",
            "main",
            "abc123",
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("[E_PROPERTY_LOCK_MISSING]"), "{missing}");

        let defs = BTreeMap::new();
        write_property_lock(&store, &defs).unwrap();
        let lock = write_repo_lock_entry(
            &store,
            "demo",
            "https://example.invalid/demo",
            "main",
            "abc123",
        )
        .unwrap();

        assert!(lock.properties.is_empty());
        assert_eq!(
            lock.repos
                .get("demo")
                .and_then(|entry| entry.url.as_deref()),
            Some("https://example.invalid/demo")
        );
        assert_eq!(
            lock.repos
                .get("demo")
                .map(|entry| entry.resolved_oid.as_str()),
            Some("abc123")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn repos_config_rejects_local_path_controls() {
        for (field, line) in [
            ("path", "path = \"repos/demo\""),
            ("auto_clone", "auto_clone = false"),
        ] {
            let text = format!(
                r#"
[repos.demo_repo]
url = "file:///tmp/demo.git"
{line}
"#
            );
            let err = toml::from_str::<GraftConfig>(&text).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("unknown field"), "{message}");
            assert!(message.contains(field), "{message}");
        }
    }

    #[test]
    fn repos_config_rejects_bad_repo_id() {
        let config: GraftConfig = toml::from_str(
            r#"
[repos."bad.repo"]
url = "https://example.test/bad.git"
"#,
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("invalid repo id")
        );
    }

    #[test]
    fn promote_targets_reject_path_traversal() {
        let config: GraftConfig = toml::from_str(
            r#"
[promote_targets.release]
path = "../release"
"#,
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("must not contain '..'")
        );
    }

    #[test]
    fn load_property_defs_rejects_legacy_properties_directory() {
        let dir = test_workspace("legacy-properties-dir");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        std::fs::remove_file(dir.join("properties.roto")).unwrap();
        std::fs::create_dir(dir.join("properties")).unwrap();
        std::fs::write(dir.join("properties").join("Old.toml"), "name = \"Old\"\n").unwrap();

        let err = load_property_defs(&store).unwrap_err().to_string();

        assert!(err.contains("[E_LEGACY_PROPERTIES_UNSUPPORTED]"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }
}
