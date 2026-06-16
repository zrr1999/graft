use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use graft_core::{
    ApplicationEndpoint, ApplicationPlan, ApplicationRef, Assertion, Change, EvidenceRecord,
    EvidenceResult, FileChange, FileChangeKind, FileRefPlan, GraftCandidate, HistorySelector,
    ObservationPlan, OverlayPlan, PatchRecord, Plan, PlanId, RunPlan, RunSelectorPlan, StateId,
    TreeEntry, TreePlan, TreeSnapshot, stable_typed_id,
};
use graft_store::GraftStore;
use graft_validate::ValidationSubject;
use serde::Serialize;

use crate::config::{GraftConfig, load_graft_config};
use crate::repo::materialized_snapshot_for_state;
use crate::requirements::{constraint_primitives, validation_constraint_with_base};

#[derive(Clone)]
struct ValidationTarget {
    subject: ValidationSubject,
    base_snapshot: Option<TreeSnapshot>,
    target_snapshot: Option<TreeSnapshot>,
    integrity: EvidenceResult,
}

#[derive(Default)]
struct ValidationMemo {
    evidence_by_plan: BTreeMap<String, Vec<EvidenceRecord>>,
    run_results: BTreeMap<String, std::result::Result<RunExecution, String>>,
}

pub(crate) fn validate_candidate(
    store: &GraftStore,
    candidate: &GraftCandidate,
    constraint_primitives_to_validate: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target = validation_target_for_application(
        store,
        &config,
        candidate.id.as_str(),
        &candidate.application,
    )?;
    ensure_integrity_passed(&target.integrity)?;
    let constraint = validation_constraint_with_base(
        &config,
        constraint_primitives_to_validate,
        &candidate.constraint,
    )?;
    let primitives = constraint_primitives(&constraint);
    let mut records = Vec::new();
    let mut memo = ValidationMemo::default();
    for constraint in primitives {
        let mut constraint_records =
            validate_plan_ref(store, &config, &target, &constraint, &mut memo)?;
        for evidence in &constraint_records {
            store.write_evidence(evidence)?;
            store.append_candidate_evidence_index(candidate.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut constraint_records);
    }
    Ok(records)
}

pub(crate) fn validate_patch(
    store: &GraftStore,
    patch: &PatchRecord,
    constraint_primitives_to_validate: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target =
        validation_target_for_application(store, &config, patch.id.as_str(), &patch.application)?;
    ensure_integrity_passed(&target.integrity)?;
    let constraint = validation_constraint_with_base(
        &config,
        constraint_primitives_to_validate,
        &patch.constraint,
    )?;
    let primitives = constraint_primitives(&constraint);
    let mut records = Vec::new();
    let mut memo = ValidationMemo::default();
    for constraint in primitives {
        let mut constraint_records =
            validate_plan_ref(store, &config, &target, &constraint, &mut memo)?;
        for evidence in &constraint_records {
            store.write_evidence(evidence)?;
            store.append_patch_evidence_index(patch.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut constraint_records);
    }
    Ok(records)
}

fn validate_plan_ref(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    memo: &mut ValidationMemo,
) -> Result<Vec<EvidenceRecord>> {
    let memo_key = plan_id.to_string();
    if let Some(records) = memo.evidence_by_plan.get(&memo_key) {
        return Ok(records.clone());
    }
    let plan = config.plans.get(plan_id).with_context(|| {
        format!(
            "[E_UNKNOWN_PLAN] plan {} is not configured in constraints.roto",
            plan_id
        )
    })?;
    if plan.plan_id()? != *plan_id {
        bail!(
            "[E_CONSTRAINT_DRIFT] plan {} no longer matches its configured observation/assertion",
            plan_id
        );
    }
    let record = verify_plan(store, config, target, plan_id, plan, memo)?;
    let records = vec![record];
    memo.evidence_by_plan.insert(memo_key, records.clone());
    Ok(records)
}

#[cfg(test)]
fn validate_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    memo: &mut ValidationMemo,
) -> Result<Vec<EvidenceRecord>> {
    validate_plan_ref(store, config, target, plan_id, memo)
}

fn verify_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    plan: &Plan,
    memo: &mut ValidationMemo,
) -> Result<EvidenceRecord> {
    let result = evaluate_plan(store, config, target, plan_id, plan, memo);
    Ok(graft_validate::evidence_for_plan_id(
        &target.subject,
        plan_id.clone(),
        result,
    )?)
}

fn evaluate_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    plan: &Plan,
    memo: &mut ValidationMemo,
) -> EvidenceResult {
    match (&plan.observation, &plan.assertion) {
        (ObservationPlan::ChangedPaths { patterns }, Assertion::PathsAnyMatch) => {
            if changed_paths_match_any(&target.subject.changed_paths, patterns) {
                EvidenceResult::Passed
            } else {
                EvidenceResult::Failed {
                    reason: "no changed path matched any pattern".to_string(),
                }
            }
        }
        (ObservationPlan::ChangedPaths { patterns }, Assertion::PathsAllMatch) => {
            if changed_paths_all_match(&target.subject.changed_paths, patterns) {
                EvidenceResult::Passed
            } else {
                EvidenceResult::Failed {
                    reason: "at least one changed path did not match any pattern".to_string(),
                }
            }
        }
        (ObservationPlan::ChangedPaths { patterns }, Assertion::PathsNoMatch) => {
            if changed_paths_match_any(&target.subject.changed_paths, patterns) {
                EvidenceResult::Failed {
                    reason: "at least one changed path matched a forbidden pattern".to_string(),
                }
            } else {
                EvidenceResult::Passed
            }
        }
        (ObservationPlan::ChangedPaths { patterns }, Assertion::PathsNotAllMatch) => {
            if changed_paths_all_match(&target.subject.changed_paths, patterns) {
                EvidenceResult::Failed {
                    reason: "all changed paths matched the patterns".to_string(),
                }
            } else {
                EvidenceResult::Passed
            }
        }
        (ObservationPlan::Run { run }, Assertion::ExitCodeIs { code }) => {
            match execute_run_plan(store, config, target, plan_id, run, memo) {
                Ok(run) if run.output.status.code() == Some(*code) => EvidenceResult::Passed,
                Ok(run) => EvidenceResult::Failed {
                    reason: format!(
                        "run exit code was {:?}, expected {code}",
                        run.output.status.code()
                    ),
                },
                Err(reason) => EvidenceResult::Unknown { reason },
            }
        }
        (ObservationPlan::Run { run }, Assertion::ExitCodeIsNot { code }) => {
            match execute_run_plan(store, config, target, plan_id, run, memo) {
                Ok(run) if run.output.status.code() != Some(*code) => EvidenceResult::Passed,
                Ok(run) => EvidenceResult::Failed {
                    reason: format!(
                        "run exit code was {:?}, expected not {code}",
                        run.output.status.code()
                    ),
                },
                Err(reason) => EvidenceResult::Unknown { reason },
            }
        }
        (
            ObservationPlan::SameOutput {
                left,
                right,
                selectors,
            },
            Assertion::OutputsSame,
        ) => {
            match evaluate_same_output(
                store,
                config,
                target,
                plan_id,
                SameOutputPlan {
                    left,
                    right,
                    selectors,
                },
                memo,
            ) {
                SameOutputEvaluation::Same => EvidenceResult::Passed,
                SameOutputEvaluation::Different => EvidenceResult::Failed {
                    reason: "selected outputs differed".to_string(),
                },
                SameOutputEvaluation::Error(reason) => EvidenceResult::Unknown { reason },
            }
        }
        (
            ObservationPlan::SameOutput {
                left,
                right,
                selectors,
            },
            Assertion::OutputsDiffer,
        ) => {
            match evaluate_same_output(
                store,
                config,
                target,
                plan_id,
                SameOutputPlan {
                    left,
                    right,
                    selectors,
                },
                memo,
            ) {
                SameOutputEvaluation::Different => EvidenceResult::Passed,
                SameOutputEvaluation::Same => EvidenceResult::Failed {
                    reason: "selected outputs were the same".to_string(),
                },
                SameOutputEvaluation::Error(reason) => EvidenceResult::Unknown { reason },
            }
        }
        (ObservationPlan::Unavailable { reason }, Assertion::Unavailable) => {
            EvidenceResult::Unknown {
                reason: reason.clone(),
            }
        }
        _ => EvidenceResult::Unknown {
            reason: format!(
                "observation {:?} cannot satisfy assertion {:?}",
                plan.observation, plan.assertion
            ),
        },
    }
}

fn changed_paths_match_any(paths: &[String], patterns: &[String]) -> bool {
    paths
        .iter()
        .any(|path| patterns.iter().any(|pattern| path_matches(pattern, path)))
}

fn changed_paths_all_match(paths: &[String], patterns: &[String]) -> bool {
    paths
        .iter()
        .all(|path| patterns.iter().any(|pattern| path_matches(pattern, path)))
}

#[derive(Clone, Debug)]
struct RunExecution {
    output: std::process::Output,
    cwd: PathBuf,
}

#[derive(Serialize)]
struct RunResultKeySeed<'a> {
    argv: &'a [String],
    tree: &'a str,
}

fn execute_run_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    run: &RunPlan,
    memo: &mut ValidationMemo,
) -> std::result::Result<RunExecution, String> {
    let Some(program) = run.argv.first() else {
        return Err("run argv must not be empty".to_string());
    };
    let cwd = materialize_run_tree(store, config, target, plan_id, &run.tree)?;
    let run_result_id = stable_typed_id(
        "run_result",
        &RunResultKeySeed {
            argv: &run.argv,
            tree: cwd
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    format!("materialized run tree has invalid id: {}", cwd.display())
                })?,
        },
    )
    .map_err(|error| format!("run result id could not be computed: {error}"))?;
    if let Some(cached) = memo.run_results.get(&run_result_id) {
        return cached.clone();
    }
    let result = ProcessCommand::new(program)
        .args(&run.argv[1..])
        .current_dir(&cwd)
        .output()
        .map(|output| RunExecution {
            output,
            cwd: cwd.clone(),
        })
        .map_err(|error| {
            format!(
                "failed to execute `{}` in {}: {error}",
                program,
                cwd.display()
            )
        });
    memo.run_results.insert(run_result_id, result.clone());
    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SameOutputEvaluation {
    Same,
    Different,
    Error(String),
}

struct SameOutputPlan<'a> {
    left: &'a RunPlan,
    right: &'a RunPlan,
    selectors: &'a [RunSelectorPlan],
}

fn evaluate_same_output(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    plan: SameOutputPlan<'_>,
    memo: &mut ValidationMemo,
) -> SameOutputEvaluation {
    let SameOutputPlan {
        left,
        right,
        selectors,
    } = plan;
    if selectors.is_empty() {
        return SameOutputEvaluation::Error(
            "same_output requires at least one selector".to_string(),
        );
    }
    let left = match execute_run_plan(store, config, target, plan_id, left, memo) {
        Ok(run) => run,
        Err(reason) => return SameOutputEvaluation::Error(format!("left run failed: {reason}")),
    };
    let left_values = match capture_run_selectors(&left, selectors) {
        Ok(values) => values,
        Err(reason) => {
            return SameOutputEvaluation::Error(format!("left selector failed: {reason}"));
        }
    };
    let right = match execute_run_plan(store, config, target, plan_id, right, memo) {
        Ok(run) => run,
        Err(reason) => return SameOutputEvaluation::Error(format!("right run failed: {reason}")),
    };
    let right_values = match capture_run_selectors(&right, selectors) {
        Ok(values) => values,
        Err(reason) => {
            return SameOutputEvaluation::Error(format!("right selector failed: {reason}"));
        }
    };
    if left_values == right_values {
        SameOutputEvaluation::Same
    } else {
        SameOutputEvaluation::Different
    }
}

fn capture_run_selectors(
    run: &RunExecution,
    selectors: &[RunSelectorPlan],
) -> std::result::Result<Vec<Vec<u8>>, String> {
    selectors
        .iter()
        .map(|selector| capture_run_selector(run, selector))
        .collect()
}

fn capture_run_selector(
    run: &RunExecution,
    selector: &RunSelectorPlan,
) -> std::result::Result<Vec<u8>, String> {
    match selector {
        RunSelectorPlan::Stdout => Ok(run.output.stdout.clone()),
        RunSelectorPlan::Stderr => Ok(run.output.stderr.clone()),
        RunSelectorPlan::PostFile { path } => {
            let path = normalize_plan_path(path)?;
            let full_path = run.cwd.join(&path);
            fs::read(&full_path).map_err(|error| {
                format!("read post_file `{path}` in {}: {error}", run.cwd.display())
            })
        }
    }
}

fn materialize_run_tree(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    tree: &TreePlan,
) -> std::result::Result<PathBuf, String> {
    let snapshot = snapshot_for_tree_plan(store, config, target, plan_id, tree)?;
    let tree_id = snapshot
        .id()
        .map_err(|error| format!("tree id could not be computed: {error}"))?;
    let destination = store.paths().derived_worktrees().join(&tree_id);
    store
        .materialize_tree_snapshot(&snapshot, &destination)
        .map_err(|error| format!("materialize {tree_id} for command run: {error}"))?;
    Ok(destination)
}

fn snapshot_for_tree_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    tree: &TreePlan,
) -> std::result::Result<TreeSnapshot, String> {
    match tree {
        TreePlan::Application {
            application: ApplicationPlan::Current,
            endpoint,
        } => match endpoint {
            ApplicationEndpoint::Base => target
                .base_snapshot
                .clone()
                .ok_or_else(|| "current application base tree is unavailable".to_string()),
            ApplicationEndpoint::Target => target
                .target_snapshot
                .clone()
                .ok_or_else(|| "current application target tree is unavailable".to_string()),
        },
        TreePlan::Application {
            application: ApplicationPlan::PreviousFailure { selector },
            endpoint,
        } => previous_failure_snapshot(store, config, plan_id, selector, endpoint),
        TreePlan::WithOverlay { base, overlays } => {
            let base = snapshot_for_tree_plan(store, config, target, plan_id, base)?;
            apply_overlays(store, config, target, plan_id, base, overlays)
        }
    }
}

#[derive(Clone, Debug)]
struct HistoricalFailure {
    subject: String,
    created_at: String,
    application: ApplicationRef,
}

fn previous_failure_snapshot(
    store: &GraftStore,
    config: &GraftConfig,
    plan_id: &PlanId,
    selector: &HistorySelector,
    endpoint: &ApplicationEndpoint,
) -> std::result::Result<TreeSnapshot, String> {
    let failure = select_previous_failure(store, plan_id, selector)?;
    let resolved = store
        .resolve_application(&failure.application)
        .map_err(|error| {
            format!(
                "read previous failed application `{}`: {error}",
                failure.subject
            )
        })?;
    let change = resolved.change;
    let state = match endpoint {
        ApplicationEndpoint::Base => &change.base_state,
        ApplicationEndpoint::Target => &change.target_state,
    };
    materialized_snapshot_for_state(store, config, state).map_err(|error| {
        format!(
            "materialize {endpoint:?} tree for previous failed application `{}`: {error}",
            failure.subject
        )
    })
}

fn select_previous_failure(
    store: &GraftStore,
    plan_id: &PlanId,
    selector: &HistorySelector,
) -> std::result::Result<HistoricalFailure, String> {
    let mut failures = BTreeMap::<String, HistoricalFailure>::new();
    for candidate in store
        .list_candidates()
        .map_err(|error| format!("list candidates for previous_failure: {error}"))?
    {
        let subject = candidate.id.as_str();
        let evidence = store
            .candidate_evidence_records(subject)
            .map_err(|error| format!("read candidate evidence for `{subject}`: {error}"))?;
        record_failed_application(
            &mut failures,
            subject,
            &candidate.application,
            plan_id,
            evidence,
        );
    }
    for patch in store
        .list_patches()
        .map_err(|error| format!("list patches for previous_failure: {error}"))?
    {
        let subject = patch.id.as_str();
        let evidence = store
            .registry_evidence_for_subject(subject)
            .map_err(|error| format!("read patch evidence for `{subject}`: {error}"))?;
        record_failed_application(
            &mut failures,
            subject,
            &patch.application,
            plan_id,
            evidence,
        );
    }
    let mut failures = failures.into_values().collect::<Vec<_>>();
    failures.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.subject.cmp(&right.subject))
    });
    if failures.is_empty() {
        return Err(format!(
            "no previous failed application for plan `{}`",
            plan_id.as_str()
        ));
    }
    let index = match selector {
        HistorySelector::First => 0,
        HistorySelector::Last => failures.len() - 1,
        HistorySelector::Get { index } => usize::try_from(*index)
            .map_err(|_| format!("previous_failure index {index} is too large"))?,
    };
    failures.get(index).cloned().ok_or_else(|| {
        format!(
            "previous_failure selector {} was out of range for {} failed application(s)",
            history_selector_label(selector),
            failures.len()
        )
    })
}

fn record_failed_application(
    failures: &mut BTreeMap<String, HistoricalFailure>,
    subject: &str,
    application: &ApplicationRef,
    plan_id: &PlanId,
    evidence: Vec<EvidenceRecord>,
) {
    for record in evidence {
        if record.plan == *plan_id && matches!(record.result, EvidenceResult::Failed { .. }) {
            let failure = HistoricalFailure {
                subject: subject.to_string(),
                created_at: record.created_at,
                application: application.clone(),
            };
            failures
                .entry(subject.to_string())
                .and_modify(|current| {
                    if (failure.created_at.as_str(), failure.subject.as_str())
                        < (current.created_at.as_str(), current.subject.as_str())
                    {
                        *current = failure.clone();
                    }
                })
                .or_insert(failure);
        }
    }
}

fn history_selector_label(selector: &HistorySelector) -> String {
    match selector {
        HistorySelector::First => "History.First".to_string(),
        HistorySelector::Last => "History.Last".to_string(),
        HistorySelector::Get { index } => format!("History.Get({index})"),
    }
}

fn apply_overlays(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    base: TreeSnapshot,
    overlays: &[OverlayPlan],
) -> std::result::Result<TreeSnapshot, String> {
    let mut entries = base
        .entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    for overlay in overlays {
        match overlay {
            OverlayPlan::ReplaceFile { path, file } => {
                let path = normalize_plan_path(path)?;
                let file = resolve_file_ref(store, config, target, plan_id, file)?;
                entries.insert(
                    path.clone(),
                    TreeEntry {
                        path,
                        hash: file.hash,
                        size: file.size,
                    },
                );
            }
        }
    }
    Ok(TreeSnapshot::new(entries.into_values().collect()))
}

fn resolve_file_ref(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    plan_id: &PlanId,
    file: &FileRefPlan,
) -> std::result::Result<TreeEntry, String> {
    match file {
        FileRefPlan::TreeFile { tree, path } => {
            let path = normalize_plan_path(path)?;
            let snapshot = snapshot_for_tree_plan(store, config, target, plan_id, tree)?;
            snapshot
                .entries
                .into_iter()
                .find(|entry| entry.path == path)
                .ok_or_else(|| format!("file `{path}` was not found in referenced tree"))
        }
        FileRefPlan::Resolved { file } => Err(format!(
            "resolved file ref `{}` cannot be consumed until file-ref object storage lands",
            file.as_str()
        )),
    }
}

fn normalize_plan_path(path: &str) -> std::result::Result<String, String> {
    let mut value = path;
    while let Some(stripped) = value.strip_prefix("./") {
        value = stripped;
    }
    if value.is_empty() {
        return Err("plan file path must not be empty".to_string());
    }
    if value.starts_with('/') {
        return Err(format!("plan file path `{path}` must be relative"));
    }
    if value.contains('\\') || value.contains('\n') || value.contains('\t') {
        return Err(format!(
            "plan file path `{path}` contains an invalid character"
        ));
    }
    if value.contains("//") {
        return Err(format!(
            "plan file path `{path}` must not contain empty components"
        ));
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "plan file path `{path}` contains invalid component `{component}`"
            ));
        }
    }
    Ok(value.to_string())
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == path;
    }
    let mut rest = path;
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }
    if !starts_with_wildcard
        && let Some(first) = parts.first()
        && !rest.starts_with(first)
    {
        return false;
    }
    for part in &parts {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    if !ends_with_wildcard
        && let Some(last) = parts.last()
        && !path.ends_with(last)
    {
        return false;
    }
    true
}

pub(crate) fn evidence_for_current_verifiers(
    _config: &GraftConfig,
    required: &[PlanId],
    evidence: &[EvidenceRecord],
    subject: &str,
) -> Result<Vec<EvidenceRecord>> {
    let required = required.iter().collect::<BTreeSet<_>>();
    Ok(evidence
        .iter()
        .filter(|record| {
            required.contains(&record.plan)
                && record.subject == subject
                && record.verifier == graft_validate::verifier_id_for_plan(&record.plan)
        })
        .cloned()
        .collect())
}

fn validation_target_for_application(
    store: &GraftStore,
    config: &GraftConfig,
    id: &str,
    application: &ApplicationRef,
) -> Result<ValidationTarget> {
    let resolved = store.resolve_application(application)?;
    let change = resolved.change;
    let changed_paths = change
        .endpoint_diff()
        .into_iter()
        .filter(|file| !matches!(file.kind, FileChangeKind::Unchanged))
        .map(|file| file.path)
        .collect();
    let base_snapshot = materialized_snapshot_for_state(store, config, &change.base_state)?;
    let target_snapshot = materialized_snapshot_for_state(store, config, &change.target_state)?;
    let integrity = change_integrity_for_snapshots(&change, &base_snapshot, &target_snapshot);
    Ok(ValidationTarget {
        subject: ValidationSubject::with_change(id.to_string(), changed_paths),
        base_snapshot: Some(base_snapshot),
        target_snapshot: Some(target_snapshot),
        integrity,
    })
}

pub(crate) fn ensure_change_integrity(
    store: &GraftStore,
    config: &GraftConfig,
    application: &ApplicationRef,
) -> Result<()> {
    let result = change_integrity_for_application(store, config, application)?;
    ensure_integrity_passed(&result)
}

fn change_integrity_for_application(
    store: &GraftStore,
    config: &GraftConfig,
    application: &ApplicationRef,
) -> Result<EvidenceResult> {
    let resolved = store.resolve_application(application)?;
    let change = resolved.change;
    let target_snapshot = match materialized_snapshot_for_state(store, config, &change.target_state)
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::m003_target_not_materializable(
                    &format!("{:?}", change.target_state),
                    &error.to_string(),
                )
                .format_reason(),
            });
        }
    };
    Ok(change_integrity_for_change(
        store,
        config,
        &change,
        &target_snapshot,
    ))
}

fn ensure_integrity_passed(result: &EvidenceResult) -> Result<()> {
    match result {
        EvidenceResult::Passed => Ok(()),
        EvidenceResult::Failed { reason }
        | EvidenceResult::Unknown { reason }
        | EvidenceResult::Skipped { reason } => {
            bail!("[E_CHANGE_INTEGRITY] patch core invariant failed: {reason}")
        }
    }
}

fn change_integrity_for_change(
    store: &GraftStore,
    config: &GraftConfig,
    change: &Change,
    target_snapshot: &TreeSnapshot,
) -> EvidenceResult {
    let base_snapshot = match materialized_snapshot_for_state(store, config, &change.base_state) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::v003_base_unmaterializable(&error.to_string())
                    .format_reason(),
            };
        }
    };
    change_integrity_for_snapshots(change, &base_snapshot, target_snapshot)
}

fn change_integrity_for_snapshots(
    change: &Change,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> EvidenceResult {
    match validate_change_replays_to_target(change, base_snapshot, target_snapshot) {
        Ok(()) => EvidenceResult::Passed,
        Err(reason) => EvidenceResult::Failed { reason },
    }
}

fn validate_change_replays_to_target(
    change: &Change,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    ensure_snapshot_matches_state("base", &change.base_state, base_snapshot)?;
    ensure_snapshot_matches_state("target", &change.target_state, target_snapshot)?;

    let mut applied = snapshot_entries(base_snapshot);
    let mut seen = BTreeSet::new();
    for file in change.endpoint_diff() {
        if !seen.insert(file.path.clone()) {
            return Err(format!("duplicate change entry for path {}", file.path));
        }
        match file.kind {
            FileChangeKind::Added | FileChangeKind::Captured => {
                if applied.contains_key(&file.path) {
                    return Err(format!("path {} already exists in base", file.path));
                }
                let entry = target_entry_from_change(&file, target_snapshot)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Modified => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(&file, base_entry, base_snapshot)?;
                let entry = target_entry_from_change(&file, target_snapshot)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Deleted => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(&file, base_entry, base_snapshot)?;
                if file.target_hash.is_some() || file.target_size.is_some() {
                    return Err(format!("deleted path {} has target content", file.path));
                }
                applied.remove(&file.path);
            }
            FileChangeKind::Unchanged => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("unchanged path {} is missing from base", file.path));
                };
                ensure_change_matches_base(&file, base_entry, base_snapshot)?;
                let entry = target_entry_from_change(&file, target_snapshot)?;
                if entry != *base_entry {
                    return Err(format!(
                        "unchanged path {} changes content in target",
                        file.path
                    ));
                }
            }
        }
    }

    let expected = snapshot_entries(target_snapshot);
    if applied == expected {
        Ok(())
    } else {
        Err(snapshot_mismatch_reason(&applied, &expected))
    }
}

fn ensure_snapshot_matches_state(
    label: &str,
    state: &StateId,
    snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    if let StateId::GraftTree(expected) = state {
        let actual = snapshot
            .id()
            .map_err(|error| format!("{label} snapshot id could not be computed: {error}"))?;
        if actual != *expected {
            return Err(format!(
                "{label} snapshot id mismatch: state declares {expected}, snapshot is {actual}"
            ));
        }
    }
    Ok(())
}

fn snapshot_entries(snapshot: &TreeSnapshot) -> BTreeMap<String, TreeEntry> {
    snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn target_entry_from_change(
    file: &FileChange,
    target_snapshot: &TreeSnapshot,
) -> std::result::Result<TreeEntry, String> {
    if let (Some(hash), Some(size)) = (&file.target_hash, file.target_size) {
        return Ok(TreeEntry {
            path: file.path.clone(),
            hash: hash.clone(),
            size,
        });
    }
    snapshot_entries(target_snapshot)
        .get(&file.path)
        .cloned()
        .ok_or_else(|| format!("path {} is missing from target snapshot", file.path))
}

fn ensure_change_matches_base(
    file: &FileChange,
    base_entry: &TreeEntry,
    base_snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    let base_entries = snapshot_entries(base_snapshot);
    let declared_hash = file
        .base_hash
        .clone()
        .or_else(|| base_entries.get(&file.path).map(|entry| entry.hash.clone()));
    let declared_size = file
        .base_size
        .or_else(|| base_entries.get(&file.path).map(|entry| entry.size));
    if declared_hash.as_deref() != Some(base_entry.hash.as_str())
        || declared_size != Some(base_entry.size)
    {
        return Err(format!(
            "path {} does not match declared base content",
            file.path
        ));
    }
    Ok(())
}

fn snapshot_mismatch_reason(
    applied: &BTreeMap<String, TreeEntry>,
    expected: &BTreeMap<String, TreeEntry>,
) -> String {
    let paths = applied
        .keys()
        .chain(expected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        match (applied.get(&path), expected.get(&path)) {
            (None, Some(_)) => return format!("target contains path {path} absent after replay"),
            (Some(_), None) => return format!("replay produced path {path} absent from target"),
            (Some(left), Some(right)) if left != right => {
                return format!("replay produced different content for path {path}");
            }
            _ => {}
        }
    }
    "replayed patch did not reconstruct target".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{Assertion, ObservationPlan};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(name: &str) -> (PathBuf, GraftStore) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "graft-runtime-validation-{name}-{}-{nanos}",
            std::process::id()
        ));
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        (dir, store)
    }

    fn target_with_paths(paths: &[&str]) -> ValidationTarget {
        ValidationTarget {
            subject: ValidationSubject::with_change(
                "subject:demo".to_string(),
                paths.iter().map(|path| path.to_string()).collect(),
            ),
            base_snapshot: Some(TreeSnapshot::new(Vec::new())),
            target_snapshot: Some(TreeSnapshot::new(Vec::new())),
            integrity: EvidenceResult::Passed,
        }
    }

    fn config_with_plan(plan: Plan) -> (GraftConfig, PlanId) {
        let plan_id = plan.plan_id().unwrap();
        let mut config = GraftConfig::default();
        config.plans.insert(plan_id.clone(), plan);
        (config, plan_id)
    }

    #[test]
    fn changed_paths_plan_honors_positive_and_negative_assertions() {
        let (dir, store) = temp_store("changed-paths");
        let target = target_with_paths(&["src/lib.rs", "README.md"]);
        let mut memo = ValidationMemo::default();

        let (config, plan_id) = config_with_plan(Plan {
            observation: ObservationPlan::ChangedPaths {
                patterns: vec!["src/**".to_string()],
            },
            assertion: Assertion::PathsAnyMatch,
        });
        let records = validate_plan(&store, &config, &target, &plan_id, &mut memo).unwrap();
        assert_eq!(records[0].plan, plan_id);
        assert_eq!(records[0].result, EvidenceResult::Passed);

        let (config, plan_id) = config_with_plan(Plan {
            observation: ObservationPlan::ChangedPaths {
                patterns: vec!["target/**".to_string()],
            },
            assertion: Assertion::PathsNoMatch,
        });
        let records = validate_plan(
            &store,
            &config,
            &target,
            &plan_id,
            &mut ValidationMemo::default(),
        )
        .unwrap();
        assert_eq!(records[0].result, EvidenceResult::Passed);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn run_plan_records_exit_code_result_against_plan_id() {
        let (dir, store) = temp_store("run-plan");
        let target = target_with_paths(&[]);
        let (config, plan_id) = config_with_plan(Plan {
            observation: ObservationPlan::Run {
                run: RunPlan {
                    argv: vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        "exit 0".to_string(),
                    ],
                    tree: TreePlan::Application {
                        application: ApplicationPlan::Current,
                        endpoint: ApplicationEndpoint::Target,
                    },
                },
            },
            assertion: Assertion::ExitCodeIs { code: 0 },
        });

        let records = validate_plan(
            &store,
            &config,
            &target,
            &plan_id,
            &mut ValidationMemo::default(),
        )
        .unwrap();

        assert_eq!(records[0].plan, plan_id);
        assert_eq!(records[0].verifier, format!("plan@{}", records[0].plan));
        assert_eq!(records[0].result, EvidenceResult::Passed);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn run_result_memo_reuses_same_observation_for_multiple_assertions() {
        let (dir, store) = temp_store("run-result-memo");
        let target = target_with_paths(&[]);
        let counter = dir.join("counter.txt");
        let script = format!(
            "n=0; if test -f {0}; then n=$(cat {0}); fi; n=$((n + 1)); printf '%s' \"$n\" > {0}; exit 0",
            counter.display()
        );
        let run = RunPlan {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), script],
            tree: TreePlan::Application {
                application: ApplicationPlan::Current,
                endpoint: ApplicationEndpoint::Target,
            },
        };
        let first = Plan {
            observation: ObservationPlan::Run { run: run.clone() },
            assertion: Assertion::ExitCodeIs { code: 0 },
        };
        let second = Plan {
            observation: ObservationPlan::Run { run },
            assertion: Assertion::ExitCodeIsNot { code: 7 },
        };
        let first_id = first.plan_id().unwrap();
        let second_id = second.plan_id().unwrap();
        let mut config = GraftConfig::default();
        config.plans.insert(first_id.clone(), first);
        config.plans.insert(second_id.clone(), second);
        let mut memo = ValidationMemo::default();

        let first_records = validate_plan(&store, &config, &target, &first_id, &mut memo).unwrap();
        let second_records =
            validate_plan(&store, &config, &target, &second_id, &mut memo).unwrap();

        assert_eq!(first_records[0].result, EvidenceResult::Passed);
        assert_eq!(second_records[0].result, EvidenceResult::Passed);
        assert_eq!(memo.run_results.len(), 1);
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn evidence_for_current_verifiers_filters_subject_plan_and_verifier() {
        let plan = PlanId::new("plan:demo");
        let matching =
            EvidenceRecord::passed("subject:demo", plan.clone(), "plan@plan:demo").unwrap();
        let wrong_subject =
            EvidenceRecord::passed("subject:other", plan.clone(), "plan@plan:demo").unwrap();
        let wrong_verifier =
            EvidenceRecord::passed("subject:demo", plan.clone(), "legacy").unwrap();

        let filtered = evidence_for_current_verifiers(
            &GraftConfig::default(),
            std::slice::from_ref(&plan),
            &[matching.clone(), wrong_subject, wrong_verifier],
            "subject:demo",
        )
        .unwrap();

        assert_eq!(filtered, vec![matching]);
    }
}
