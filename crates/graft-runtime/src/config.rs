use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use graft_core::{Constraint, ConstraintDef, Plan, PlanId};

use crate::roto_constraints::{LoadedConstraints, load_roto_constraint_defs};
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
    pub(crate) constraints: BTreeMap<String, ConstraintDef>,
    #[serde(skip)]
    pub(crate) plans: BTreeMap<PlanId, Plan>,
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
        self.validate_constraint_names()
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

    fn validate_constraint_names(&self) -> Result<()> {
        validate_required_constraint_names("admission.required", &self.admission.required)?;
        validate_required_constraint_names("promotion.required", &self.promotion.required)?;
        for (target_id, target) in &self.promote_targets {
            validate_required_constraint_names(
                &format!("promote_targets.{target_id}.required"),
                &target.required,
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
            constraints: BTreeMap::new(),
            plans: BTreeMap::new(),
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
    pub(crate) required: RequiredConstraintsConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotionConfig {
    #[serde(default)]
    pub(crate) required: RequiredConstraintsConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RequiredConstraintsConfig {
    Expr(ConstraintConfig),
    Names(Vec<String>),
}

impl Default for RequiredConstraintsConfig {
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
    pub(crate) required: RequiredConstraintsConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConstraintLock {
    #[serde(default = "graft_lock_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) constraints: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) repos: BTreeMap<String, RepoLockEntry>,
}

impl Default for ConstraintLock {
    fn default() -> Self {
        Self {
            version: graft_lock_version(),
            constraints: BTreeMap::new(),
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
pub(crate) struct ConstraintLockDrift {
    pub(crate) missing: Vec<String>,
    pub(crate) changed: Vec<ConstraintLockChange>,
    pub(crate) extra: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConstraintLockChange {
    pub(crate) name: String,
    pub(crate) locked: String,
    pub(crate) current: String,
}

impl ConstraintLockDrift {
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
    let loaded = load_constraint_catalog(store)?;
    config.constraints = loaded.defs;
    config.plans = loaded.plans;
    require_constraint_lock_current(store, &config.constraints)?;
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

pub(crate) fn load_constraint_catalog(store: &GraftStore) -> Result<LoadedConstraints> {
    let legacy_path = store.paths().legacy_properties_config();
    if legacy_path.exists() {
        bail!(
            "[E_LEGACY_CONSTRAINTS_UNSUPPORTED] {} is legacy constraint configuration; use only the v2 constraints.roto file",
            legacy_path.display()
        );
    }
    let roto_path = store.paths().constraints_roto_config();
    if !roto_path.exists() {
        bail!(
            "{} does not exist; run graft init first",
            roto_path.display()
        );
    }
    let loaded = load_roto_constraint_defs(&roto_path)?;
    validate_constraint_name_graph(&loaded.defs)?;
    Ok(loaded)
}

pub(crate) fn load_constraint_defs(store: &GraftStore) -> Result<BTreeMap<String, ConstraintDef>> {
    Ok(load_constraint_catalog(store)?.defs)
}

pub(crate) fn validate_constraint_name_graph(defs: &BTreeMap<String, ConstraintDef>) -> Result<()> {
    for (name, def) in defs {
        if def.name != *name {
            bail!(
                "[E_CONSTRAINT_NAME_MISMATCH] constraint map key `{name}` contains constraint named `{}`",
                def.name
            );
        }
    }
    Ok(())
}

pub(crate) fn constraint_lock_path(store: &GraftStore) -> PathBuf {
    store.paths().constraints_lock()
}

pub(crate) fn current_constraint_lock(
    defs: &BTreeMap<String, ConstraintDef>,
) -> Result<ConstraintLock> {
    validate_constraint_name_graph(defs)?;
    let mut constraints = BTreeMap::new();
    for (name, def) in defs {
        constraints.insert(name.clone(), def.body_id()?);
    }
    Ok(ConstraintLock {
        version: graft_lock_version(),
        constraints,
        repos: BTreeMap::new(),
    })
}

pub(crate) fn read_constraint_lock(store: &GraftStore) -> Result<Option<ConstraintLock>> {
    let path = constraint_lock_path(store);
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let lock: ConstraintLock =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(lock))
}

fn write_graft_lock(store: &GraftStore, lock: &ConstraintLock) -> Result<()> {
    let path = constraint_lock_path(store);
    let text = toml::to_string_pretty(lock).context("serialize graft.lock")?;
    std::fs::write(
        &path,
        format!("# @generated by graft constraint lock; do not edit by hand\n{text}"),
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub(crate) fn write_constraint_lock(
    store: &GraftStore,
    defs: &BTreeMap<String, ConstraintDef>,
) -> Result<ConstraintLock> {
    write_constraint_lock_with_plans(store, defs, &BTreeMap::new())
}

pub(crate) fn write_constraint_lock_with_plans(
    store: &GraftStore,
    defs: &BTreeMap<String, ConstraintDef>,
    plans: &BTreeMap<PlanId, Plan>,
) -> Result<ConstraintLock> {
    for plan in plans.values() {
        store.write_plan(plan)?;
    }
    for def in defs.values() {
        store.write_constraint_def(def)?;
    }
    let mut lock = current_constraint_lock(defs)?;
    if let Some(existing) = read_constraint_lock(store)? {
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
) -> Result<ConstraintLock> {
    let Some(mut lock) = read_constraint_lock(store)? else {
        bail!(
            "[E_CONSTRAINT_LOCK_MISSING] graft.lock is missing; run `graft constraint lock` before locking repository refs"
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
    let Some(lock) = read_constraint_lock(store)? else {
        bail!(
            "[E_CONSTRAINT_LOCK_MISSING] graft.lock is missing; run `graft constraint lock` before resolving repository bases"
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

pub(crate) fn constraint_lock_drift(
    defs: &BTreeMap<String, ConstraintDef>,
    lock: &ConstraintLock,
) -> Result<ConstraintLockDrift> {
    let current = current_constraint_lock(defs)?;
    let mut drift = ConstraintLockDrift::default();
    for (name, current_id) in &current.constraints {
        match lock.constraints.get(name) {
            None => drift.missing.push(name.clone()),
            Some(locked_id) if locked_id != current_id => {
                drift.changed.push(ConstraintLockChange {
                    name: name.clone(),
                    locked: locked_id.clone(),
                    current: current_id.clone(),
                })
            }
            Some(_) => {}
        }
    }
    for name in lock.constraints.keys() {
        if !current.constraints.contains_key(name) {
            drift.extra.push(name.clone());
        }
    }
    Ok(drift)
}

pub(crate) fn require_constraint_lock_current(
    store: &GraftStore,
    defs: &BTreeMap<String, ConstraintDef>,
) -> Result<ConstraintLock> {
    let Some(lock) = read_constraint_lock(store)? else {
        bail!(
            "[E_CONSTRAINT_LOCK_MISSING] graft.lock is missing; run `graft constraint lock` to derive constraint ids from constraints.roto"
        );
    };
    let drift = constraint_lock_drift(defs, &lock)?;
    if !drift.is_clean() {
        bail!(
            "[E_CONSTRAINT_LOCK_DRIFT] graft.lock is stale ({}); run `graft constraint lock` to refresh it",
            drift.summary()
        );
    }
    Ok(lock)
}

pub(crate) fn resolve_constraint(config: &GraftConfig, name: &str) -> Result<Constraint> {
    let def = config.constraints.get(name).with_context(|| {
        format!("[E_UNKNOWN_CONSTRAINT] constraint {name} is not configured in constraints.roto")
    })?;
    Ok(def.body.clone())
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

pub(crate) fn required_constraints_constraint(
    config: &GraftConfig,
    required: &RequiredConstraintsConfig,
) -> Result<Constraint> {
    match required {
        RequiredConstraintsConfig::Names(names) => names
            .iter()
            .map(|name| constraint_primitive(config, name))
            .collect::<Result<Vec<_>>>()
            .map(Constraint::all_of),
        RequiredConstraintsConfig::Expr(expr) => constraint_expr(config, expr),
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
    validate_constraint_name("constraint requirement", value)?;
    resolve_constraint(config, value)
}

fn validate_required_constraint_names(
    label: &str,
    constraints: &RequiredConstraintsConfig,
) -> Result<()> {
    match constraints {
        RequiredConstraintsConfig::Names(names) => {
            for name in names {
                validate_constraint_name(label, name)?;
            }
        }
        RequiredConstraintsConfig::Expr(expr) => validate_constraint_expr_names(label, expr)?,
    }
    Ok(())
}

fn validate_constraint_expr_names(label: &str, expr: &ConstraintConfig) -> Result<()> {
    for item in expr
        .all_of
        .iter()
        .chain(expr.any_of.iter())
        .chain(expr.both.iter())
        .chain(expr.either.iter())
        .flatten()
    {
        validate_constraint_term_names(label, item)?;
    }
    if let Some(primitive) = &expr.primitive {
        validate_constraint_name(label, primitive)?;
    }
    Ok(())
}

fn validate_constraint_term_names(label: &str, term: &ConstraintTermConfig) -> Result<()> {
    match term {
        ConstraintTermConfig::Name(name) => validate_constraint_name(label, name)?,
        ConstraintTermConfig::Expr(expr) => validate_constraint_expr_names(label, expr)?,
    }
    Ok(())
}

fn validate_constraint_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("[E_INVALID_CONSTRAINT] {label} contains an empty constraint name");
    }
    if value.contains(':') {
        bail!(
            "[E_SCOPED_CONSTRAINT_UNSUPPORTED] {label} uses `{value}`, but constraint requirements must be bare names; constraints are whole-workspace by definition"
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
    use graft_core::{Assertion, Observation};
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

    fn plan_id(name: &str) -> PlanId {
        PlanId::new(format!("plan:{name}"))
    }

    fn constraint_def(name: &str) -> ConstraintDef {
        ConstraintDef {
            name: name.to_string(),
            description: format!("{name} constraint"),
            body: Constraint::primitive(plan_id(name)),
        }
    }

    fn config_with_constraints(text: &str, names: &[&str]) -> GraftConfig {
        let mut config = parse_config(text);
        for name in names {
            config
                .constraints
                .insert((*name).to_string(), constraint_def(name));
        }
        config
    }

    #[test]
    fn required_flat_list_lowers_to_all_of_constraint_bodies() {
        let config = config_with_constraints(
            r#"
[admission]
required = ["fmt_clean", "tests_pass"]
"#,
            &["fmt_clean", "tests_pass"],
        );

        let constraint =
            required_constraints_constraint(&config, &config.admission.required).unwrap();

        assert_eq!(
            constraint,
            Constraint::all_of(vec![
                resolve_constraint(&config, "fmt_clean").unwrap(),
                resolve_constraint(&config, "tests_pass").unwrap(),
            ])
        );
    }

    #[test]
    fn required_tagged_any_of_lowers_to_either_constraint() {
        let config = config_with_constraints(
            r#"
[admission.required]
any_of = ["fast_check", "slow_check"]
"#,
            &["fast_check", "slow_check"],
        );

        let constraint =
            required_constraints_constraint(&config, &config.admission.required).unwrap();

        assert_eq!(
            constraint,
            Constraint::any_of(vec![
                resolve_constraint(&config, "fast_check").unwrap(),
                resolve_constraint(&config, "slow_check").unwrap(),
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
        assert!(parse_config("[sync]\nenabled = true\n").sync.enabled);
        assert!(!parse_config("[sync]\nenabled = false\n").sync.enabled);
    }

    #[test]
    fn load_graft_config_requires_existing_constraint_lock() {
        let dir = test_workspace("missing-lock-load");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        assert!(!dir.join("graft.lock").exists());

        let error = load_graft_config(&store).unwrap_err().to_string();

        assert!(error.contains("[E_CONSTRAINT_LOCK_MISSING]"), "{error}");
        assert!(
            !dir.join("graft.lock").exists(),
            "read-only config load must not recreate graft.lock"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_constraint_defs_discovers_three_layer_constraint_functions() {
        let dir = test_workspace("roto-load");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        std::fs::write(
            dir.join("constraints.roto"),
            r#"
fn no_generated_artifacts(app: Application) -> Constraint {
    primitive(app.changed_paths(["target/**", "*.tmp"]), no_match, "no generated artifacts")
}

fn cargo_tests_pass(app: Application) -> Constraint {
    primitive(app.run(["cargo", "test", "--all-targets"]), exit_zero, "cargo tests pass")
}
"#,
        )
        .unwrap();

        let catalog = load_constraint_catalog(&store).unwrap();

        assert_eq!(
            catalog.defs.keys().cloned().collect::<Vec<_>>(),
            vec!["cargo_tests_pass", "no_generated_artifacts"]
        );
        assert_eq!(catalog.defs["cargo_tests_pass"].name, "cargo_tests_pass");
        assert_eq!(catalog.plans.len(), 2);
        assert!(
            catalog
                .plans
                .values()
                .any(|plan| matches!(plan.assertion, Assertion::ExitCodeIs { code: 0 }))
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn validate_constraint_name_graph_rejects_name_mismatch() {
        let defs = BTreeMap::from([(
            "cargo_tests_pass".to_string(),
            ConstraintDef {
                name: "different".to_string(),
                description: "mismatch".to_string(),
                body: Constraint::primitive(plan_id("cargo_tests_pass")),
            },
        )]);

        let error = validate_constraint_name_graph(&defs)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_CONSTRAINT_NAME_MISMATCH]"), "{error}");
    }

    #[test]
    fn constraint_lock_uses_constraint_body_identity_not_description() {
        let first = BTreeMap::from([(
            "cargo_tests_pass".to_string(),
            ConstraintDef {
                name: "cargo_tests_pass".to_string(),
                description: "first description".to_string(),
                body: Constraint::primitive(plan_id("cargo_tests_pass")),
            },
        )]);
        let second = BTreeMap::from([(
            "cargo_tests_pass".to_string(),
            ConstraintDef {
                name: "cargo_tests_pass".to_string(),
                description: "second description".to_string(),
                body: Constraint::primitive(plan_id("cargo_tests_pass")),
            },
        )]);

        assert_eq!(
            current_constraint_lock(&first).unwrap().constraints,
            current_constraint_lock(&second).unwrap().constraints
        );
    }

    #[test]
    fn write_constraint_lock_with_plans_materializes_constraint_defs_and_plans() {
        let dir = test_workspace("lock-with-plans");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let plan = Plan {
            observation: Observation::ChangedPaths {
                patterns: vec!["src/**".to_string()],
            },
            assertion: Assertion::PathsAnyMatch,
        };
        let plan_id = plan.plan_id().unwrap();
        let defs = BTreeMap::from([(
            "source_changed".to_string(),
            ConstraintDef {
                name: "source_changed".to_string(),
                description: "source changes".to_string(),
                body: Constraint::primitive(plan_id.clone()),
            },
        )]);
        let plans = BTreeMap::from([(plan_id.clone(), plan)]);

        let lock = write_constraint_lock_with_plans(&store, &defs, &plans).unwrap();

        assert_eq!(lock.constraints.len(), 1);
        assert!(store.paths().object_constraints().exists());
        assert!(
            store
                .paths()
                .object_plans()
                .join(format!("{plan_id}.json"))
                .exists()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn repo_lock_entry_requires_existing_constraint_lock_and_preserves_constraints() {
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
        assert!(missing.contains("[E_CONSTRAINT_LOCK_MISSING]"), "{missing}");

        let defs = BTreeMap::new();
        write_constraint_lock(&store, &defs).unwrap();
        let lock = write_repo_lock_entry(
            &store,
            "demo",
            "https://example.invalid/demo",
            "main",
            "abc123",
        )
        .unwrap();

        assert!(lock.constraints.is_empty());
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
    fn load_constraint_defs_rejects_legacy_constraints_directory() {
        let dir = test_workspace("legacy-properties-dir");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        std::fs::remove_file(dir.join("constraints.roto")).unwrap();
        std::fs::create_dir(dir.join("properties")).unwrap();
        std::fs::write(dir.join("properties").join("Old.toml"), "name = \"Old\"\n").unwrap();

        let err = load_constraint_defs(&store).unwrap_err().to_string();

        assert!(err.contains("[E_LEGACY_CONSTRAINTS_UNSUPPORTED]"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }
}
