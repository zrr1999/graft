use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("failed to canonicalize record for stable id: {0}")]
    Canonicalize(#[from] serde_json::Error),
    #[error("[E_CHANGE_INTEGRITY] {0}")]
    ApplicationIntegrity(#[from] ApplicationIntegrityError),
}

pub type Result<T> = std::result::Result<T, CoreError>;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(CandidateId);
id_type!(PatchId);
id_type!(EvidenceId);
id_type!(ChangeId);
id_type!(ActionId);
id_type!(ApplicationId);

mod application_model;

pub use application_model::{
    Action, ApplicabilityProof, ApplicabilityStep, ApplicationIntegrityError, ApplicationRecord,
    ApplicationRef, Change, ChangeOp, FileMode, MaterializedApplication, action_id,
    application_from_change, application_id, materialize_application,
    validate_application_integrity,
};
id_type!(PropertyId);
id_type!(RelationId);
id_type!(PromotionId);
id_type!(ScratchId);
id_type!(FileViewHash);
id_type!(TreeId);
id_type!(FileRef);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoBaseState {
    pub repo_id: String,
    pub treeish: String,
    pub resolved_tree_oid: String,
}

impl RepoBaseState {
    pub fn new(
        repo_id: impl Into<String>,
        treeish: impl Into<String>,
        resolved_tree_oid: impl Into<String>,
    ) -> Self {
        Self {
            repo_id: repo_id.into(),
            treeish: treeish.into(),
            resolved_tree_oid: resolved_tree_oid.into(),
        }
    }

    pub fn display_ref(&self) -> String {
        let short_oid = self
            .resolved_tree_oid
            .get(..12)
            .unwrap_or(self.resolved_tree_oid.as_str());
        format!("repo:{}@{}#{short_oid}", self.repo_id, self.treeish)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StateId {
    GitTree(String),
    RepoTree(RepoBaseState),
    GraftTree(String),
}

/// User-facing parsed form of a base reference.
///
/// Runtime base-like CLI arguments and `graftd` scratch `base` params share one
/// parser here. Scratch accepts the immediately materializable subset
/// (`graft:empty`, `tree:`, `candidate:`, `patch:`); higher-level runtime paths
/// may also resolve Git and configured repo refs. Each variant
/// only carries the data the parser is sure about; resolving it to a concrete
/// [`StateId`] (which may require a Git repo, a clone, or a registry lookup)
/// is the consumer's responsibility.
///
/// Supported forms:
///
/// - `HEAD`, any other Git treeish (e.g. `main`, `abc1234`, `refs/heads/x`)
///   parses to [`BaseRefSpec::GitTreeish`].
/// - `repo:<id>@<treeish>` parses to [`BaseRefSpec::Repo`]; the `<id>` must be
///   declared in `[repos.<id>]`.
/// - `tree:<digest>` parses to [`BaseRefSpec::GraftTree`].
/// - `candidate:<digest>` parses to [`BaseRefSpec::Candidate`].
/// - `patch:<digest>` parses to [`BaseRefSpec::Patch`].
/// - `graft:empty` parses to [`BaseRefSpec::Empty`], an explicit "start from
///   nothing" sentinel for environments without a Git base.
///
/// Legacy display IDs such as `gt_<digest>`, `grc_<digest>`, and `gr_<digest>`
/// are rejected with [`BaseRefParseError::LegacyId`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BaseRefSpec {
    GitTreeish(String),
    Repo { repo_id: String, treeish: String },
    GraftTree(String),
    Candidate(CandidateId),
    Patch(PatchId),
    Empty,
}

#[derive(Debug, thiserror::Error)]
pub enum BaseRefParseError {
    #[error("empty base ref")]
    Empty,
    #[error("invalid repo base ref `{0}`; expected `repo:<id>@<treeish>`")]
    InvalidRepo(String),
    #[error("invalid prefixed base ref `{0}`; value after `{1}:` must not be empty")]
    EmptyPrefix(String, &'static str),
    #[error("[E_LEGACY_ID] legacy id `{0}` uses `{1}`; use `{2}:<digest>`")]
    LegacyId(String, &'static str, &'static str),
}

fn legacy_id_prefix(value: &str) -> Option<(&'static str, &'static str)> {
    [
        ("graft-tree:", "tree"),
        ("gt_", "tree"),
        ("ch_", "change"),
        ("grc_", "candidate"),
        ("gr_", "patch"),
        ("ev_", "evidence"),
        ("cf_", "conflict"),
        ("rel_", "relation"),
        ("prm_", "promotion"),
        ("scr_", "scratch"),
        ("fv_", "file_view"),
    ]
    .into_iter()
    .find(|(prefix, _)| value.starts_with(prefix))
}

impl BaseRefSpec {
    /// Parses a single `--from`-style string. See [`BaseRefSpec`] for the
    /// supported forms. Unknown free-form input is treated as a Git treeish so
    /// existing flows like `--from main` keep working.
    pub fn parse(value: &str) -> std::result::Result<Self, BaseRefParseError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(BaseRefParseError::Empty);
        }
        if trimmed == "graft:empty" {
            return Ok(Self::Empty);
        }
        if let Some(rest) = trimmed.strip_prefix("repo:") {
            let (repo_id, treeish) = rest
                .split_once('@')
                .ok_or_else(|| BaseRefParseError::InvalidRepo(trimmed.to_string()))?;
            if repo_id.is_empty() || treeish.is_empty() {
                return Err(BaseRefParseError::InvalidRepo(trimmed.to_string()));
            }
            return Ok(Self::Repo {
                repo_id: repo_id.to_string(),
                treeish: treeish.to_string(),
            });
        }
        if let Some((legacy_prefix, typed_kind)) = legacy_id_prefix(trimmed) {
            return Err(BaseRefParseError::LegacyId(
                trimmed.to_string(),
                legacy_prefix,
                typed_kind,
            ));
        }
        if let Some(rest) = trimmed.strip_prefix("tree:") {
            if rest.is_empty() {
                return Err(BaseRefParseError::EmptyPrefix(trimmed.to_string(), "tree"));
            }
            return Ok(Self::GraftTree(trimmed.to_string()));
        }
        if let Some(rest) = trimmed.strip_prefix("candidate:") {
            if rest.is_empty() {
                return Err(BaseRefParseError::EmptyPrefix(
                    trimmed.to_string(),
                    "candidate",
                ));
            }
            return Ok(Self::Candidate(CandidateId::new(trimmed)));
        }
        if let Some(rest) = trimmed.strip_prefix("patch:") {
            if rest.is_empty() {
                return Err(BaseRefParseError::EmptyPrefix(trimmed.to_string(), "patch"));
            }
            return Ok(Self::Patch(PatchId::new(trimmed)));
        }
        Ok(Self::GitTreeish(trimmed.to_string()))
    }

    /// Pretty form suitable for logs and error messages.
    pub fn display(&self) -> String {
        match self {
            Self::GitTreeish(value) => value.clone(),
            Self::Repo { repo_id, treeish } => format!("repo:{repo_id}@{treeish}"),
            Self::GraftTree(id) => id.clone(),
            Self::Candidate(id) => id.as_str().to_string(),
            Self::Patch(id) => id.as_str().to_string(),
            Self::Empty => "graft:empty".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSnapshot {
    pub entries: Vec<TreeEntry>,
}

impl TreeSnapshot {
    pub fn new(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Self { entries }
    }

    pub fn id(&self) -> Result<String> {
        stable_typed_id("tree", self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchNode {
    pub version: u32,
    pub base_state: StateId,
    pub parent: Option<ScratchId>,
    pub op: Option<CanonicalScratchOp>,
    pub result_tree_digest: String,
}

impl ScratchNode {
    pub const VERSION: u32 = 1;

    pub fn root(base_state: StateId, result_tree_digest: impl Into<String>) -> Self {
        Self {
            version: Self::VERSION,
            base_state,
            parent: None,
            op: None,
            result_tree_digest: result_tree_digest.into(),
        }
    }

    pub fn child(
        parent: ScratchId,
        base_state: StateId,
        op: CanonicalScratchOp,
        result_tree_digest: impl Into<String>,
    ) -> Self {
        Self {
            version: Self::VERSION,
            base_state,
            parent: Some(parent),
            op: Some(op),
            result_tree_digest: result_tree_digest.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalScratchOp {
    Edit {
        path: String,
        edits: Vec<HashlineEdit>,
    },
    Write {
        path: String,
        content_hash: String,
        size: u64,
    },
    Delete {
        path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HashlineEdit {
    ReplaceLine {
        line: u64,
        hash: String,
        old: String,
        new: String,
    },
    ReplaceRange {
        start_line: u64,
        start_hash: String,
        end_line: u64,
        end_hash: String,
        new_lines: Vec<String>,
    },
    InsertAfter {
        line: u64,
        hash: String,
        new_lines: Vec<String>,
    },
    InsertBefore {
        line: u64,
        hash: String,
        new_lines: Vec<String>,
    },
    ReplaceText {
        old_text: String,
        new_text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileViewHashSeed<'a> {
    pub scratch: &'a ScratchId,
    pub path: &'a str,
    pub bytes_hash: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Unchanged,
    Captured,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub base_hash: Option<String>,
    pub target_hash: Option<String>,
    pub base_size: Option<u64>,
    pub target_size: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSummary {
    pub files: usize,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub captured: usize,
    pub target_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyRef {
    pub id: PropertyId,
    pub name: String,
}

impl PropertyRef {
    pub fn new(id: PropertyId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Constraint {
    Top,
    Bottom,
    Primitive {
        property: PropertyRef,
    },
    Both {
        left: Box<Constraint>,
        right: Box<Constraint>,
    },
    Either {
        left: Box<Constraint>,
        right: Box<Constraint>,
    },
}

impl Constraint {
    pub fn top() -> Self {
        Self::Top
    }

    pub fn bottom() -> Self {
        Self::Bottom
    }

    pub fn primitive(property: PropertyRef) -> Self {
        Self::Primitive { property }
    }

    pub fn all_of(items: impl IntoIterator<Item = Constraint>) -> Self {
        fold_right(items, Self::Top, |left, right| Self::Both {
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    pub fn any_of(items: impl IntoIterator<Item = Constraint>) -> Self {
        fold_right(items, Self::Bottom, |left, right| Self::Either {
            left: Box::new(left),
            right: Box::new(right),
        })
    }
}

fn fold_right(
    items: impl IntoIterator<Item = Constraint>,
    empty: Constraint,
    combine: impl Fn(Constraint, Constraint) -> Constraint,
) -> Constraint {
    let mut items = items.into_iter().collect::<Vec<_>>();
    match items.pop() {
        None => empty,
        Some(last) => items
            .into_iter()
            .rev()
            .fold(last, |right, left| combine(left, right)),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    ChangeMeta,
    TargetSnapshot,
    BaseAndTarget,
    Change,
    Files {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    Command {
        command: String,
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evaluator {
    Builtin {
        name: String,
        options: BTreeMap<String, String>,
    },
    Command {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        setup: Vec<String>,
        pre: Vec<String>,
        teardown: Vec<String>,
        timeout_secs: Option<u64>,
    },
    Pair {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        setup: Vec<String>,
        pre: Vec<String>,
        teardown: Vec<String>,
        timeout_secs: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Judge {
    ExitOk,
    BoolTrue,
    BoolFalse,
    Pairwise,
    Command {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_secs: Option<u64>,
    },
    ExitCodeZero,
    StdoutContains {
        text: String,
    },
    JsonEquals {
        pointer: String,
        value: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyDef {
    pub name: String,
    pub query: Query,
    pub evaluator: Evaluator,
    pub judge: Judge,
}

impl PropertyDef {
    pub fn property_id(&self) -> Result<PropertyId> {
        let seed = PropertyDefSeed {
            query: &self.query,
            evaluator: &self.evaluator,
            judge: &self.judge,
        };
        Ok(PropertyId::new(stable_typed_id("property", &seed)?))
    }

    pub fn property_ref(&self) -> Result<PropertyRef> {
        Ok(PropertyRef::new(self.property_id()?, self.name.clone()))
    }
}

#[derive(Serialize)]
struct PropertyDefSeed<'a> {
    query: &'a Query,
    evaluator: &'a Evaluator,
    judge: &'a Judge,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PropertyName(String);

impl PropertyName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PropertyName {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PropertyName {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for PropertyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Severity {
    Blocking,
    Warning,
    Info,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertySourceRef {
    pub path: String,
    pub function: PropertyName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertySpec {
    pub name: PropertyName,
    pub plan: PropertyPlan,
    pub description: String,
    pub severity: Severity,
    pub source_ref: Option<PropertySourceRef>,
}

impl PropertySpec {
    pub fn property_id(&self) -> Result<PropertyId> {
        let seed = PropertySpecSeed {
            name: &self.name,
            checks: &self.plan.checks,
            requires: &self.plan.requires,
        };
        Ok(PropertyId::new(stable_typed_id("property", &seed)?))
    }

    pub fn property_ref(&self) -> Result<PropertyRef> {
        Ok(PropertyRef::new(self.property_id()?, self.name.as_str()))
    }
}

#[derive(Serialize)]
struct PropertySpecSeed<'a> {
    name: &'a PropertyName,
    checks: &'a [CheckPlan],
    requires: &'a [PropertyName],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyPlan {
    pub checks: Vec<CheckPlan>,
    pub requires: Vec<PropertyName>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckPlan {
    Expect {
        probe: ProbePlan,
        polarity: ProbePolarity,
    },
    AllOf {
        checks: Vec<CheckPlan>,
    },
    AnyOf {
        checks: Vec<CheckPlan>,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbePolarity {
    Success,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeResult {
    Success,
    Failure,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbePlan {
    PathMatch {
        paths: PathSetPlan,
        patterns: Vec<String>,
    },
    PathAllMatch {
        paths: PathSetPlan,
        patterns: Vec<String>,
    },
    RunExitCodeIs {
        run: RunPlan,
        code: i32,
    },
    SameOutput {
        left: RunPlan,
        right: RunPlan,
        selectors: Vec<RunSelectorPlan>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PathSetPlan {
    ChangedPaths,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    pub argv: Vec<String>,
    pub tree: TreePlan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TreePlan {
    Application {
        application: ApplicationPlan,
        endpoint: ApplicationEndpoint,
    },
    WithOverlay {
        base: Box<TreePlan>,
        overlays: Vec<OverlayPlan>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationPlan {
    Current,
    PreviousFailure { selector: HistorySelector },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationEndpoint {
    Base,
    Target,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistorySelector {
    First,
    Last,
    Get { index: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayPlan {
    ReplaceFile { path: String, file: FileRefPlan },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileRefPlan {
    TreeFile { tree: Box<TreePlan>, path: String },
    Resolved { file: FileRef },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunSelectorPlan {
    Stdout,
    Stderr,
    PostFile { path: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceResult {
    Passed,
    Failed { reason: String },
    Unknown { reason: String },
    Skipped { reason: String },
}

impl EvidenceResult {
    pub fn satisfies_requirement(&self) -> bool {
        matches!(self, Self::Passed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub producer: String,
    pub message: Option<String>,
    pub created_at: String,
}

impl Provenance {
    pub fn now(producer: impl Into<String>, message: Option<String>) -> Self {
        Self {
            producer: producer.into(),
            message,
            created_at: OffsetDateTime::now_utc().to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraftCandidate {
    pub id: CandidateId,
    pub application: ApplicationRef,
    pub constraint: Constraint,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSummary {
    pub constraint: Constraint,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRecord {
    pub id: PatchId,
    pub application: ApplicationRef,
    pub constraint: Constraint,
    pub provenance: Provenance,
    pub admission: AdmissionSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub id: EvidenceId,
    pub subject: String,
    pub property: PropertyId,
    pub verifier: String,
    pub result: EvidenceResult,
    pub created_at: String,
}

impl EvidenceRecord {
    pub fn new(
        subject: impl Into<String>,
        property: PropertyId,
        verifier: impl Into<String>,
        result: EvidenceResult,
    ) -> Result<Self> {
        let mut record = Self {
            id: EvidenceId::new("evidence:pending"),
            subject: subject.into(),
            property,
            verifier: verifier.into(),
            result,
            created_at: OffsetDateTime::now_utc().to_string(),
        };
        record.id = evidence_id(&record)?;
        Ok(record)
    }

    pub fn passed(
        subject: impl Into<String>,
        property: PropertyId,
        verifier: impl Into<String>,
    ) -> Result<Self> {
        Self::new(subject, property, verifier, EvidenceResult::Passed)
    }

    pub fn failed(
        subject: impl Into<String>,
        property: PropertyId,
        verifier: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            subject,
            property,
            verifier,
            EvidenceResult::Failed {
                reason: reason.into(),
            },
        )
    }

    pub fn unknown(
        subject: impl Into<String>,
        property: PropertyId,
        verifier: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            subject,
            property,
            verifier,
            EvidenceResult::Unknown {
                reason: reason.into(),
            },
        )
    }

    pub fn skipped(
        subject: impl Into<String>,
        property: PropertyId,
        verifier: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            subject,
            property,
            verifier,
            EvidenceResult::Skipped {
                reason: reason.into(),
            },
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionRecord {
    pub id: PromotionId,
    pub patch_id: PatchId,
    pub target: String,
    pub dry_run: bool,
    pub status: String,
    pub promoted_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchRelationKind {
    Composes,
    Migrates,
    Reverts,
    Materializes,
    Promotes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRelation {
    pub id: RelationId,
    pub kind: PatchRelationKind,
    pub subject: String,
    pub sources: Vec<String>,
    pub created_at: String,
}

#[derive(Serialize)]
struct EvidenceSeed {
    subject: String,
    property: PropertyId,
    verifier: String,
    result: EvidenceResult,
}

pub fn stable_typed_id(kind: &str, value: &impl Serialize) -> Result<String> {
    let digest = stable_digest(value)?;
    Ok(format!("{kind}:{digest}"))
}

pub fn stable_digest(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(blake3_hex_digest(&bytes)[..12].to_string())
}

pub fn blake3_hex_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn scratch_id(node: &ScratchNode) -> Result<ScratchId> {
    Ok(ScratchId::new(stable_typed_id("scratch", node)?))
}

pub fn file_view_hash(seed: &FileViewHashSeed<'_>) -> Result<FileViewHash> {
    Ok(FileViewHash::new(stable_typed_id("file_view", seed)?))
}

pub fn candidate_id(candidate: &GraftCandidate) -> Result<CandidateId> {
    let seed = CandidateSeed {
        application: &candidate.application,
        constraint: &candidate.constraint,
        provenance: &candidate.provenance,
    };
    Ok(CandidateId::new(stable_typed_id("candidate", &seed)?))
}

pub fn patch_id(patch: &PatchRecord) -> Result<PatchId> {
    let seed = PatchSeed {
        application: &patch.application,
        constraint: &patch.constraint,
        producer: &patch.provenance.producer,
        message: patch.provenance.message.as_deref(),
        admission: &patch.admission,
    };
    Ok(PatchId::new(stable_typed_id("patch", &seed)?))
}

pub fn evidence_id(evidence: &EvidenceRecord) -> Result<EvidenceId> {
    let seed = EvidenceSeed {
        subject: evidence.subject.clone(),
        property: evidence.property.clone(),
        verifier: evidence.verifier.clone(),
        result: evidence.result.clone(),
    };
    Ok(EvidenceId::new(stable_typed_id("evidence", &seed)?))
}

pub fn relation_id(relation: &PatchRelation) -> Result<RelationId> {
    let seed = RelationSeed {
        kind: &relation.kind,
        subject: &relation.subject,
        sources: &relation.sources,
    };
    Ok(RelationId::new(stable_typed_id("relation", &seed)?))
}

pub fn promotion_id(promotion: &PromotionRecord) -> Result<PromotionId> {
    let seed = PromotionSeed {
        patch_id: &promotion.patch_id,
        target: &promotion.target,
        dry_run: promotion.dry_run,
        status: &promotion.status,
    };
    Ok(PromotionId::new(stable_typed_id("promotion", &seed)?))
}

#[derive(Serialize)]
struct CandidateSeed<'a> {
    application: &'a ApplicationRef,
    constraint: &'a Constraint,
    provenance: &'a Provenance,
}

#[derive(Serialize)]
struct PatchSeed<'a> {
    application: &'a ApplicationRef,
    constraint: &'a Constraint,
    producer: &'a str,
    message: Option<&'a str>,
    admission: &'a AdmissionSummary,
}

#[derive(Serialize)]
struct RelationSeed<'a> {
    kind: &'a PatchRelationKind,
    subject: &'a str,
    sources: &'a [String],
}

#[derive(Serialize)]
struct PromotionSeed<'a> {
    patch_id: &'a PatchId,
    target: &'a str,
    dry_run: bool,
    status: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_property_def(name: &str, command: &str) -> PropertyDef {
        PropertyDef {
            name: name.to_string(),
            query: Query::ChangeMeta,
            evaluator: Evaluator::Command {
                command: command.to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                setup: Vec::new(),
                pre: Vec::new(),
                teardown: Vec::new(),
                timeout_secs: Some(60),
            },
            judge: Judge::ExitOk,
        }
    }

    fn test_property_ref(name: &str) -> PropertyRef {
        test_property_def(name, "cargo test")
            .property_ref()
            .unwrap()
    }

    #[test]
    fn stable_ids_use_typed_display_form() {
        let property = test_property_def("TestsPass", "cargo test")
            .property_id()
            .unwrap();
        let evidence = EvidenceRecord::passed("candidate:demo", property, "test-verifier").unwrap();
        assert!(evidence.id.as_str().starts_with("evidence:"));
    }

    #[test]
    fn property_id_ignores_name_and_tracks_verifier_content() {
        let original = test_property_def("TestsPass", "cargo test");
        let renamed = test_property_def("CargoTests", "cargo test");
        let changed_verifier = test_property_def("TestsPass", "cargo test --all");

        assert_eq!(
            original.property_id().unwrap(),
            renamed.property_id().unwrap()
        );
        assert_ne!(
            original.property_id().unwrap(),
            changed_verifier.property_id().unwrap()
        );
        assert!(
            original
                .property_id()
                .unwrap()
                .as_str()
                .starts_with("property:")
        );
    }

    fn current_target_tree() -> TreePlan {
        TreePlan::Application {
            application: ApplicationPlan::Current,
            endpoint: ApplicationEndpoint::Target,
        }
    }

    fn test_property_spec() -> PropertySpec {
        let run = RunPlan {
            argv: vec!["cargo".into(), "test".into(), "--all-targets".into()],
            tree: current_target_tree(),
        };
        PropertySpec {
            name: PropertyName::new("cargo_tests_pass"),
            plan: PropertyPlan {
                checks: vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs { run, code: 0 },
                    polarity: ProbePolarity::Success,
                }],
                requires: vec![PropertyName::new("no_generated_artifacts")],
            },
            description: "cargo tests pass".to_string(),
            severity: Severity::Blocking,
            source_ref: Some(PropertySourceRef {
                path: "properties.roto".to_string(),
                function: PropertyName::new("cargo_tests_pass"),
            }),
        }
    }

    #[test]
    fn v2_property_id_ignores_display_metadata() {
        let original = test_property_spec();
        let mut changed_metadata = original.clone();
        changed_metadata.description = "renamed display text".to_string();
        changed_metadata.severity = Severity::Warning;
        changed_metadata.source_ref = None;

        assert_eq!(
            original.property_id().unwrap(),
            changed_metadata.property_id().unwrap()
        );
    }

    #[test]
    fn v2_property_id_tracks_name_checks_and_requires() {
        let original = test_property_spec();

        let mut renamed = original.clone();
        renamed.name = PropertyName::new("tests_pass");

        let mut changed_requires = original.clone();
        changed_requires.plan.requires = vec![PropertyName::new("cargo_fmt_clean")];

        let mut changed_check = original.clone();
        changed_check.plan.checks = vec![CheckPlan::Expect {
            probe: ProbePlan::RunExitCodeIs {
                run: RunPlan {
                    argv: vec!["cargo".into(), "test".into(), "--doc".into()],
                    tree: current_target_tree(),
                },
                code: 0,
            },
            polarity: ProbePolarity::Success,
        }];

        assert_ne!(
            original.property_id().unwrap(),
            renamed.property_id().unwrap()
        );
        assert_ne!(
            original.property_id().unwrap(),
            changed_requires.property_id().unwrap()
        );
        assert_ne!(
            original.property_id().unwrap(),
            changed_check.property_id().unwrap()
        );
    }

    #[test]
    fn v2_plan_represents_history_overlay_and_same_output() {
        let target = current_target_tree();
        let previous_target = TreePlan::Application {
            application: ApplicationPlan::PreviousFailure {
                selector: HistorySelector::First,
            },
            endpoint: ApplicationEndpoint::Target,
        };
        let checker = FileRefPlan::TreeFile {
            tree: Box::new(target.clone()),
            path: "./check_diff.sh".to_string(),
        };
        let bad_tree = TreePlan::WithOverlay {
            base: Box::new(previous_target),
            overlays: vec![OverlayPlan::ReplaceFile {
                path: "./check_diff.sh".to_string(),
                file: checker,
            }],
        };
        let bad_run = RunPlan {
            argv: vec!["bash".into(), "./check_diff.sh".into()],
            tree: bad_tree,
        };
        let base_run = RunPlan {
            argv: vec!["bash".into(), "./run.sh".into()],
            tree: TreePlan::Application {
                application: ApplicationPlan::Current,
                endpoint: ApplicationEndpoint::Base,
            },
        };
        let target_run = RunPlan {
            argv: vec!["bash".into(), "./run.sh".into()],
            tree: target,
        };

        let checks = vec![
            CheckPlan::Expect {
                probe: ProbePlan::RunExitCodeIs {
                    run: bad_run,
                    code: 0,
                },
                polarity: ProbePolarity::Failure,
            },
            CheckPlan::Expect {
                probe: ProbePlan::SameOutput {
                    left: base_run,
                    right: target_run,
                    selectors: vec![
                        RunSelectorPlan::PostFile {
                            path: "./alignment/expected.json".to_string(),
                        },
                        RunSelectorPlan::Stdout,
                        RunSelectorPlan::Stderr,
                    ],
                },
                polarity: ProbePolarity::Success,
            },
        ];

        let json = serde_json::to_string(&checks).unwrap();
        assert!(json.contains("previous_failure"));
        assert!(json.contains("replace_file"));
        assert!(json.contains("same_output"));
        assert!(json.contains("post_file"));
    }

    #[test]
    fn base_ref_parses_each_supported_form() {
        assert_eq!(
            BaseRefSpec::parse("HEAD").unwrap(),
            BaseRefSpec::GitTreeish("HEAD".to_string())
        );
        assert_eq!(
            BaseRefSpec::parse("main").unwrap(),
            BaseRefSpec::GitTreeish("main".to_string())
        );
        assert_eq!(
            BaseRefSpec::parse("  abc1234  ").unwrap(),
            BaseRefSpec::GitTreeish("abc1234".to_string())
        );
        assert_eq!(
            BaseRefSpec::parse("repo:graft@main").unwrap(),
            BaseRefSpec::Repo {
                repo_id: "graft".to_string(),
                treeish: "main".to_string(),
            }
        );
        assert_eq!(
            BaseRefSpec::parse("tree:abc").unwrap(),
            BaseRefSpec::GraftTree("tree:abc".to_string())
        );
        assert_eq!(
            BaseRefSpec::parse("candidate:foo").unwrap(),
            BaseRefSpec::Candidate(CandidateId::new("candidate:foo"))
        );
        assert_eq!(
            BaseRefSpec::parse("patch:foo").unwrap(),
            BaseRefSpec::Patch(PatchId::new("patch:foo"))
        );
        assert_eq!(
            BaseRefSpec::parse("graft:empty").unwrap(),
            BaseRefSpec::Empty
        );
    }

    #[test]
    fn base_ref_parses_rejects_malformed_input() {
        assert!(matches!(
            BaseRefSpec::parse(""),
            Err(BaseRefParseError::Empty)
        ));
        assert!(matches!(
            BaseRefSpec::parse("   "),
            Err(BaseRefParseError::Empty)
        ));
        assert!(matches!(
            BaseRefSpec::parse("repo:graft"),
            Err(BaseRefParseError::InvalidRepo(_))
        ));
        assert!(matches!(
            BaseRefSpec::parse("repo:@main"),
            Err(BaseRefParseError::InvalidRepo(_))
        ));
        assert!(matches!(
            BaseRefSpec::parse("tree:"),
            Err(BaseRefParseError::EmptyPrefix(_, "tree"))
        ));
        assert!(matches!(
            BaseRefSpec::parse("gt_abc"),
            Err(BaseRefParseError::LegacyId(_, "gt_", "tree"))
        ));
        assert!(matches!(
            BaseRefSpec::parse("grc_abc"),
            Err(BaseRefParseError::LegacyId(_, "grc_", "candidate"))
        ));
        assert!(matches!(
            BaseRefSpec::parse("graft-tree:abc"),
            Err(BaseRefParseError::LegacyId(_, "graft-tree:", "tree"))
        ));
    }

    #[test]
    fn repo_base_state_serializes_with_resolved_tree_oid() {
        let state = StateId::RepoTree(RepoBaseState::new(
            "graft",
            "main",
            "0123456789abcdef0123456789abcdef01234567",
        ));
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("repo_tree"));
        assert!(json.contains("resolved_tree_oid"));
        assert!(json.contains("0123456789abcdef0123456789abcdef01234567"));
        assert_eq!(state, serde_json::from_str::<StateId>(&json).unwrap());
        let StateId::RepoTree(repo) = state else {
            panic!("expected repo tree state");
        };
        assert_eq!(repo.display_ref(), "repo:graft@main#0123456789ab");
    }

    #[test]
    fn state_id_json_accepts_typed_ids() {
        let git = r#"{"kind":"git_tree","value":"abc123"}"#;
        let graft = r#"{"kind":"graft_tree","value":"tree:abc"}"#;
        assert_eq!(
            serde_json::from_str::<StateId>(git).unwrap(),
            StateId::GitTree("abc123".to_string())
        );
        assert_eq!(
            serde_json::from_str::<StateId>(graft).unwrap(),
            StateId::GraftTree("tree:abc".to_string())
        );
    }

    #[test]
    fn state_id_json_rejects_unknown_fields() {
        let error = serde_json::from_str::<StateId>(
            r#"{"kind":"git_tree","value":"abc123","surprise":true}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("expected \"kind\" or \"value\""), "{error}");
        assert!(error.contains("surprise"), "{error}");
    }

    #[test]
    fn change_summarizes_snapshot_capture() {
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "abc".to_string(),
            size: 7,
        }]);
        let target = StateId::GraftTree(snapshot.id().unwrap());
        let change = Change::from_snapshots(
            StateId::GitTree("unknown".to_string()),
            None,
            target,
            &snapshot,
        );
        assert_eq!(change.summary().captured, 1);
        assert!(change.id().unwrap().as_str().starts_with("change:"));
    }

    #[test]
    fn change_summarizes_snapshot_diff() {
        let base = TreeSnapshot::new(vec![
            TreeEntry {
                path: "a.txt".to_string(),
                hash: "old-a".to_string(),
                size: 1,
            },
            TreeEntry {
                path: "delete.txt".to_string(),
                hash: "old-delete".to_string(),
                size: 2,
            },
            TreeEntry {
                path: "same.txt".to_string(),
                hash: "same".to_string(),
                size: 5,
            },
        ]);
        let target = TreeSnapshot::new(vec![
            TreeEntry {
                path: "a.txt".to_string(),
                hash: "new-a".to_string(),
                size: 3,
            },
            TreeEntry {
                path: "new.txt".to_string(),
                hash: "new-file".to_string(),
                size: 4,
            },
            TreeEntry {
                path: "same.txt".to_string(),
                hash: "same".to_string(),
                size: 5,
            },
        ]);
        let change = Change::from_snapshots(
            StateId::GraftTree(base.id().unwrap()),
            Some(&base),
            StateId::GraftTree(target.id().unwrap()),
            &target,
        );
        let summary = change.summary();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.unchanged, 0);
        assert_eq!(summary.files, 3);
        assert!(
            !change
                .endpoint_diff()
                .iter()
                .any(|file| file.path == "same.txt")
        );
    }

    #[test]
    fn changes_compose_and_reverse() {
        let first = Change {
            base_state: StateId::GraftTree("a".to_string()),
            target_state: StateId::GraftTree("b".to_string()),
            ops: vec![ChangeOp::CreateFile {
                path: "src/lib.rs".to_string(),
                blob: "b1".to_string(),
                mode: FileMode::Regular,
            }],
            capture: false,
        };
        let second = Change {
            base_state: StateId::GraftTree("b".to_string()),
            target_state: StateId::GraftTree("c".to_string()),
            ops: vec![ChangeOp::ReplaceFile {
                path: "src/lib.rs".to_string(),
                before: "b1".to_string(),
                after: "c1".to_string(),
                mode_before: FileMode::Regular,
                mode_after: FileMode::Regular,
            }],
            capture: false,
        };
        let composed = Change::compose(&first, &second);
        assert_eq!(composed.base_state, StateId::GraftTree("a".to_string()));
        assert_eq!(composed.target_state, StateId::GraftTree("c".to_string()));
        assert_eq!(composed.endpoint_diff()[0].kind, FileChangeKind::Added);
        assert_eq!(
            composed.endpoint_diff()[0].target_hash.as_deref(),
            Some("c1")
        );

        let reversed = composed.reversed();
        assert_eq!(reversed.base_state, StateId::GraftTree("c".to_string()));
        assert_eq!(reversed.target_state, StateId::GraftTree("a".to_string()));
        assert_eq!(reversed.endpoint_diff()[0].kind, FileChangeKind::Deleted);
    }

    #[test]
    fn scratch_root_ids_are_stable() {
        let root = ScratchNode::root(StateId::GitTree("tree-a".to_string()), "tree:a");
        let same = ScratchNode::root(StateId::GitTree("tree-a".to_string()), "tree:a");
        let other_base = ScratchNode::root(StateId::GitTree("tree-b".to_string()), "tree:b");

        assert_eq!(scratch_id(&root).unwrap(), scratch_id(&same).unwrap());
        assert_ne!(scratch_id(&root).unwrap(), scratch_id(&other_base).unwrap());
        assert!(scratch_id(&root).unwrap().as_str().starts_with("scratch:"));
    }

    #[test]
    fn scratch_edit_sequence_ids_are_stable() {
        let base = StateId::GitTree("tree-a".to_string());
        let root_id = scratch_id(&ScratchNode::root(base.clone(), "tree:a")).unwrap();
        let edit = CanonicalScratchOp::Edit {
            path: "src/lib.rs".to_string(),
            edits: vec![HashlineEdit::ReplaceLine {
                line: 7,
                hash: "MQ".to_string(),
                old: "x".to_string(),
                new: "y".to_string(),
            }],
        };

        let left = ScratchNode::child(root_id.clone(), base.clone(), edit.clone(), "tree:b");
        let right = ScratchNode::child(root_id, base, edit, "tree:b");

        assert_eq!(scratch_id(&left).unwrap(), scratch_id(&right).unwrap());
    }

    #[test]
    fn scratch_op_order_is_identity_sensitive() {
        let base = StateId::GitTree("tree-a".to_string());
        let root_id = scratch_id(&ScratchNode::root(base.clone(), "tree:a")).unwrap();
        let edit_one = CanonicalScratchOp::Write {
            path: "a.txt".to_string(),
            content_hash: "hash-a".to_string(),
            size: 1,
        };
        let edit_two = CanonicalScratchOp::Write {
            path: "b.txt".to_string(),
            content_hash: "hash-b".to_string(),
            size: 1,
        };

        let left_first = ScratchNode::child(
            root_id.clone(),
            base.clone(),
            edit_one.clone(),
            "tree:mid-1",
        );
        let left_first_id = scratch_id(&left_first).unwrap();
        let left = ScratchNode::child(left_first_id, base.clone(), edit_two.clone(), "tree:final");

        let right_first = ScratchNode::child(root_id, base.clone(), edit_two, "tree:mid-2");
        let right_first_id = scratch_id(&right_first).unwrap();
        let right = ScratchNode::child(right_first_id, base, edit_one, "tree:final");

        assert_eq!(left.result_tree_digest, right.result_tree_digest);
        assert_ne!(scratch_id(&left).unwrap(), scratch_id(&right).unwrap());
    }

    #[test]
    fn scratch_op_json_rejects_unknown_fields() {
        let error = serde_json::from_str::<CanonicalScratchOp>(
            r#"{"kind":"write","path":"a.txt","content_hash":"hash-a","size":1,"surprise":true}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("surprise"), "{error}");
    }

    #[test]
    fn hashline_edit_json_rejects_unknown_fields() {
        let error = serde_json::from_str::<Vec<HashlineEdit>>(
            r#"[{"kind":"replace_line","line":1,"hash":"MQ","old":"a","new":"b","surprise":true}]"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("surprise"), "{error}");
    }

    #[test]
    fn evidence_result_json_rejects_unknown_variant_fields() {
        let error = serde_json::from_str::<EvidenceResult>(
            r#"{"failed":{"reason":"tests failed","surprise":true}}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("surprise"), "{error}");
    }

    #[test]
    fn file_view_hashes_bind_scratch_path_and_content() {
        let scratch = ScratchId::new("scratch:demo");
        let seed = FileViewHashSeed {
            scratch: &scratch,
            path: "src/lib.rs",
            bytes_hash: "blob-a",
        };
        let same = FileViewHashSeed {
            scratch: &scratch,
            path: "src/lib.rs",
            bytes_hash: "blob-a",
        };
        let different_path = FileViewHashSeed {
            scratch: &scratch,
            path: "src/main.rs",
            bytes_hash: "blob-a",
        };

        assert_eq!(
            file_view_hash(&seed).unwrap(),
            file_view_hash(&same).unwrap()
        );
        assert_ne!(
            file_view_hash(&seed).unwrap(),
            file_view_hash(&different_path).unwrap()
        );
        assert!(
            file_view_hash(&seed)
                .unwrap()
                .as_str()
                .starts_with("file_view:")
        );
    }

    /// Property-style coverage for ScratchId derivation.
    ///
    /// We sweep N pseudo-random `(base, ordered ops)` tuples and check that:
    ///
    /// 1. The same input deterministically produces the same `ScratchId`
    ///    (replay equivalence; `scratch_id` is a pure function).
    /// 2. Reordering two distinct leading ops in the same chain changes the
    ///    chain `ScratchId` even though the final tree digest may stay the
    ///    same (history is part of the identity).
    ///
    /// Inputs are derived from a deterministic counter so the test is fully
    /// reproducible; we still cover enough variation in path / size / op
    /// kind to exercise the canonical serialization, not just the trivial
    /// path.
    #[test]
    fn scratch_id_is_replay_equivalent_and_order_sensitive() {
        const CASES: usize = 50;
        for case in 0..CASES {
            let base_label = format!("tree-{case}");
            let base = StateId::GitTree(base_label.clone());
            let root = ScratchNode::root(base.clone(), format!("tree:root-{case}"));
            let root_id_a = scratch_id(&root).unwrap();
            let root_id_b = scratch_id(&root).unwrap();
            assert_eq!(root_id_a, root_id_b, "root replay #{case}");

            let ops: Vec<CanonicalScratchOp> =
                (0..4).map(|step| pseudo_random_op(case, step)).collect();

            let chain_a = build_chain(root_id_a.clone(), &base, &ops, case);
            let chain_b = build_chain(root_id_b.clone(), &base, &ops, case);
            assert_eq!(chain_a, chain_b, "chain replay #{case}");

            if ops.len() >= 2 && ops[0] != ops[1] {
                let mut swapped = ops.clone();
                swapped.swap(0, 1);
                let chain_swapped = build_chain(root_id_a, &base, &swapped, case);
                assert_ne!(
                    chain_a, chain_swapped,
                    "swap-sensitivity #{case}: chains must diverge when leading ops swap"
                );
            }
        }
    }

    fn build_chain(
        root_id: ScratchId,
        base: &StateId,
        ops: &[CanonicalScratchOp],
        case: usize,
    ) -> ScratchId {
        let mut parent = root_id;
        for (step, op) in ops.iter().enumerate() {
            let node = ScratchNode::child(
                parent,
                base.clone(),
                op.clone(),
                format!("tree:{case}-{step}"),
            );
            parent = scratch_id(&node).unwrap();
        }
        parent
    }

    fn pseudo_random_op(case: usize, step: usize) -> CanonicalScratchOp {
        let kind = (case + step) % 3;
        let path = format!("src/case-{case}/step-{step}.rs");
        match kind {
            0 => CanonicalScratchOp::Write {
                path,
                content_hash: format!("hash-{case}-{step}"),
                size: ((case * 7 + step * 13) % 4096) as u64,
            },
            1 => CanonicalScratchOp::Edit {
                path: path.clone(),
                edits: vec![HashlineEdit::ReplaceLine {
                    line: ((case * 3 + step) % 32 + 1) as u64,
                    hash: "MQ".to_string(),
                    old: format!("old-{case}-{step}"),
                    new: format!("new-{case}-{step}"),
                }],
            },
            _ => CanonicalScratchOp::Delete { path },
        }
    }

    #[test]
    fn constraint_smart_constructors_fold_empty_and_singleton() {
        let primitive = Constraint::primitive(test_property_ref("TestsPass"));

        assert_eq!(Constraint::all_of(Vec::new()), Constraint::Top);
        assert_eq!(Constraint::any_of(Vec::new()), Constraint::Bottom);
        assert_eq!(
            Constraint::all_of(vec![primitive.clone()]),
            primitive.clone()
        );
        assert_eq!(Constraint::any_of(vec![primitive.clone()]), primitive);
    }

    #[test]
    fn constraint_smart_constructors_fold_right_associative() {
        let first = Constraint::primitive(test_property_ref("First"));
        let second = Constraint::primitive(test_property_ref("Second"));
        let third = Constraint::primitive(test_property_ref("Third"));

        let expected = Constraint::Both {
            left: Box::new(first.clone()),
            right: Box::new(Constraint::Both {
                left: Box::new(second.clone()),
                right: Box::new(third.clone()),
            }),
        };

        assert_eq!(Constraint::all_of(vec![first, second, third]), expected);
    }

    #[test]
    fn constraint_serdes_every_lattice_node() {
        let primitive = Constraint::primitive(test_property_ref("TestsPass"));
        let constraint = Constraint::Either {
            left: Box::new(Constraint::Both {
                left: Box::new(Constraint::Top),
                right: Box::new(primitive),
            }),
            right: Box::new(Constraint::Bottom),
        };

        let json = serde_json::to_string(&constraint).unwrap();
        let roundtrip = serde_json::from_str::<Constraint>(&json).unwrap();

        assert_eq!(roundtrip, constraint);
    }

    #[test]
    fn patch_ids_include_admission_summary_but_ignore_provenance_time() {
        let constraint = Constraint::primitive(test_property_ref("TestsPass"));
        let patch = PatchRecord {
            id: PatchId::new("patch:pending"),
            application: ApplicationRef::Stored(ApplicationId::new("application:demo")),
            constraint: constraint.clone(),
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "time-a".to_string(),
            },
            admission: AdmissionSummary { constraint },
        };
        let mut later = patch.clone();
        later.provenance.created_at = "time-b".to_string();
        assert_eq!(patch_id(&patch).unwrap(), patch_id(&later).unwrap());

        later.admission = AdmissionSummary {
            constraint: Constraint::Top,
        };
        assert_ne!(patch_id(&patch).unwrap(), patch_id(&later).unwrap());
    }
}
